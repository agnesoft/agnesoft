use std::collections::HashMap;
use std::collections::hash_map;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Seek;
use std::io::Write;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::SystemTime;

use crate::Error;
use crate::Result;
use crate::platform::platform_env::CurrentDirSetter;

macro_rules! fs_error {
    ($($arg:tt)*) => {
        $crate::Error::fs(format!($($arg)*))
    };
}

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub(crate) use unix::UnixAttributes;
#[cfg(unix)]
use unix::{native_attributes, native_metadata, open_for_metadata, set_native_attributes};
#[cfg(windows)]
use windows::{native_attributes, native_metadata, open_for_metadata, set_native_attributes};

/// A filesystem interface with runtime borrow checking.
///
/// Enforces one-writer-or-many-readers per file: a file can be opened for
/// reading any number of times, but opening it for writing requires that no
/// other read or write handles exist for that file. Violations return an
/// error rather than panicking. Handles are RAII — dropping a handle
/// releases the borrow automatically.
///
/// The two provided implementations are `FsImpl` (delegates to
/// [`std::fs`]) and [`TestFs`](crate::test_platform::test_fs::TestFs)
/// (purely in-memory, behind the `testing` feature).
///
/// # Examples
///
/// ```rust
/// use agplatform::{Fs, Platform};
///
/// let platform = agplatform::platform();
/// let fs = platform.fs();
/// # let fs = agplatform::test_platform::test_fs::TestFs::new();
/// fs.write("/hello.txt", "world").unwrap();
/// let content = fs.read_to_string("/hello.txt").unwrap();
/// assert_eq!(content, "world");
/// ```
pub trait Fs {
    /// A read-only file handle.
    type ReadHandle: Read + Seek;

    /// A read-write file handle.
    type WriteHandle: Read + Write + Seek;

    /// An iterator over the entries of a directory.
    type Dir: Iterator<Item = DirEntry>;

    /// Opens a file for reading.
    ///
    /// Multiple read handles to the same file may coexist. The returned
    /// handle implements [`Read`] and [`Seek`].
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The file does not exist.
    /// - The file is currently opened for writing.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform};
    /// use std::io::{Read, Seek, SeekFrom};
    ///
    /// let platform = agplatform::platform();
    /// let fs = platform.fs();
    /// # let fs = agplatform::test_platform::test_fs::TestFs::new();
    /// fs.write("/data.bin", &[0xCA, 0xFE]).unwrap();
    ///
    /// let mut handle = fs.open_read("/data.bin").unwrap();
    /// handle.seek(SeekFrom::Start(0)).unwrap();
    /// let mut buf = [0u8; 2];
    /// handle.read_exact(&mut buf).unwrap();
    /// assert_eq!(buf, [0xCA, 0xFE]);
    /// ```
    fn open_read<P: AsRef<Path>>(&self, path: P) -> Result<Self::ReadHandle>;

    /// Opens a file for writing (and reading), creating it if it does not exist.
    ///
    /// Equivalent to calling [`open_write_options`](Fs::open_write_options)
    /// with `OpenWriteOptions::new().create()`.
    /// The returned handle implements [`Read`], [`Write`], and [`Seek`].
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The file is currently opened for reading or writing.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform};
    /// use std::io::{Read, Write, Seek, SeekFrom};
    ///
    /// let platform = agplatform::platform();
    /// let fs = platform.fs();
    /// # let fs = agplatform::test_platform::test_fs::TestFs::new();
    /// let mut handle = fs.open_write("/log.txt").unwrap();
    /// handle.write_all(b"hello").unwrap();
    /// handle.seek(SeekFrom::Start(0)).unwrap();
    /// let mut buf = String::new();
    /// handle.read_to_string(&mut buf).unwrap();
    /// assert_eq!(buf, "hello");
    /// ```
    fn open_write<P: AsRef<Path>>(&self, path: P) -> Result<Self::WriteHandle> {
        self.open_write_options(path, OpenWriteOptions::new().create())
    }

    /// Opens a file for writing with the given `OpenWriteOptions`.
    ///
    /// The returned handle implements [`Read`], [`Write`], and [`Seek`].
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The file is currently opened for reading or writing.
    /// - `create_new` is set and the file already exists.
    /// - `create` is not set and the file does not exist.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Fs, OpenWriteOptions, Platform};
    /// use std::io::Write;
    ///
    /// let platform = agplatform::platform();
    /// let fs = platform.fs();
    /// # let fs = agplatform::test_platform::test_fs::TestFs::new();
    /// fs.write("/data.txt", "old content").unwrap();
    ///
    /// let mut handle = fs.open_write_options(
    ///     "/data.txt",
    ///     OpenWriteOptions::new().truncate(),
    /// ).unwrap();
    /// handle.write_all(b"new").unwrap();
    /// drop(handle);
    ///
    /// assert_eq!(fs.read_to_string("/data.txt").unwrap(), "new");
    /// ```
    fn open_write_options<P: AsRef<Path>>(
        &self,
        path: P,
        options: OpenWriteOptions,
    ) -> Result<Self::WriteHandle>;

    /// Reads the entire contents of a file as a byte vector.
    ///
    /// This is a convenience method that opens a read handle, seeks to
    /// determine the file length, reads the contents, and closes the handle.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The file cannot be opened for reading.
    /// - An I/O error occurs during the read.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform};
    ///
    /// let platform = agplatform::platform();
    /// let fs = platform.fs();
    /// # let fs = agplatform::test_platform::test_fs::TestFs::new();
    /// fs.write("/data.bin", &[1, 2, 3]).unwrap();
    /// let bytes = fs.read("/data.bin").unwrap();
    /// assert_eq!(bytes, [1, 2, 3]);
    /// ```
    fn read<P: AsRef<Path>>(&self, path: P) -> Result<Vec<u8>> {
        let error_path = path.as_ref().to_path_buf();

        self.open_read(path).and_then(|mut file| {
            let mut buffer = Vec::new();
            file.read_to_end(&mut buffer)
                .map(|_| buffer)
                .map_err(fs_error_at(&error_path))
        })
    }

    /// Reads the entire contents of a file as a UTF-8 string.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The file cannot be opened for reading.
    /// - An I/O error occurs during the read.
    /// - The file contents are not valid UTF-8.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform};
    ///
    /// let platform = agplatform::platform();
    /// let fs = platform.fs();
    /// # let fs = agplatform::test_platform::test_fs::TestFs::new();
    /// fs.write("/greeting.txt", "hello").unwrap();
    /// let text = fs.read_to_string("/greeting.txt").unwrap();
    /// assert_eq!(text, "hello");
    /// ```
    fn read_to_string<P: AsRef<Path>>(&self, path: P) -> Result<String> {
        let error_path = path.as_ref().to_path_buf();
        self.read(path)
            .and_then(|data| String::from_utf8(data).map_err(fs_error_at(&error_path)))
    }

    /// Writes the given bytes to a file, creating it if it does
    /// not exist, and truncating it if it does.
    ///
    /// This is a convenience method that opens a write handle (with
    /// `create` enabled), writes all bytes, and closes the handle.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The file cannot be opened for writing.
    /// - An I/O error occurs during the write.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform};
    ///
    /// let platform = agplatform::platform();
    /// let fs = platform.fs();
    /// # let fs = agplatform::test_platform::test_fs::TestFs::new();
    /// fs.write("/note.txt", "hello").unwrap();
    /// assert_eq!(fs.read_to_string("/note.txt").unwrap(), "hello");
    /// ```
    fn write<P: AsRef<Path>, C: AsRef<[u8]>>(&self, path: P, data: C) -> Result<()> {
        let error_path = path.as_ref().to_path_buf();

        self.open_write_options(path, OpenWriteOptions::new().create().truncate())
            .and_then(|mut file| {
                file.write_all(data.as_ref())
                    .map_err(fs_error_at(&error_path))
            })
    }

    /// Returns an iterator over the entries in a directory.
    ///
    /// The directory is read eagerly when this is called, so iterating
    /// cannot fail. Only regular files and directories are listed:
    /// symlinks (which are not followed), other special files, and
    /// entries whose names are not valid UTF-8 are skipped. The `.` and
    /// `..` entries are not included. The order of entries is not
    /// guaranteed.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The path does not exist.
    /// - The path is not a directory.
    /// - An I/O error occurs while reading the directory.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform};
    ///
    /// let platform = agplatform::platform();
    /// let fs = platform.fs();
    /// # let fs = agplatform::test_platform::test_fs::TestFs::new();
    /// # fs.create_dir_all("/mydir").unwrap();
    /// # fs.write("/mydir/a.txt", "").unwrap();
    /// let entries: Vec<_> = fs.read_dir("/mydir")
    ///     .unwrap()
    ///     .collect();
    /// assert!(!entries.is_empty());
    /// ```
    fn read_dir<P: AsRef<Path>>(&self, path: P) -> Result<Self::Dir>;

    /// Creates a directory and all of its missing parent directories,
    /// like [`std::fs::create_dir_all`].
    ///
    /// Succeeds if the directory already exists. Note that the file
    /// writing methods do not create missing parent directories.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - A non-directory file already exists at the path.
    /// - An I/O error occurs during creation.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform};
    ///
    /// let platform = agplatform::platform();
    /// let fs = platform.fs();
    /// # let fs = agplatform::test_platform::test_fs::TestFs::new();
    /// fs.create_dir_all("/a/b/c").unwrap();
    /// assert!(fs.read_dir("/a/b/c").unwrap().collect::<Vec<_>>().is_empty());
    /// ```
    fn create_dir_all<P: AsRef<Path>>(&self, path: P) -> Result;

    /// Returns `Ok(true)` if the path is verified to exist
    /// pointing to an existing file or directory. Symlinks
    /// are followed and the existence of the target is checked.
    ///
    /// NOTE: This method does not prevent time-of-check to
    /// time-of-use (TOCTOU) race condition.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Path is invalid (e.g. contains \0 character)
    /// - Permission denied.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform};
    ///
    /// let platform = agplatform::platform();
    /// let fs = platform.fs();
    /// # let fs = agplatform::test_platform::test_fs::TestFs::new();
    /// # fs.create_dir_all("/my/dir").unwrap();
    /// # fs.write("/my/dir/file.txt", "").unwrap();
    /// assert!(fs.exists("/my/dir/").unwrap());
    /// assert!(fs.exists("/my/dir/file.txt").unwrap());
    /// ```
    fn exists<P: AsRef<Path>>(&self, path: P) -> Result<bool>;

    /// Returns the [`Metadata`] of a file or directory. Symlinks are
    /// followed.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The path does not exist.
    /// - The path is neither a regular file nor a directory.
    /// - Permission denied.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform};
    ///
    /// let platform = agplatform::platform();
    /// let fs = platform.fs();
    /// # let fs = agplatform::test_platform::test_fs::TestFs::new();
    /// fs.write("/data.txt", "hello").unwrap();
    /// let metadata = fs.metadata("/data.txt").unwrap();
    /// assert!(metadata.file_type.is_file());
    /// assert_eq!(metadata.len, 5);
    /// ```
    fn metadata<P: AsRef<Path>>(&self, path: P) -> Result<Metadata>;

    /// Sets or clears the [`readonly`](Metadata::readonly) attribute of
    /// a file or directory. Symlinks are followed.
    ///
    /// Does nothing if the attribute already has the requested value. No
    /// permission check is made in that case so the call succeeds even if
    /// the caller would not be allowed to change the attribute.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The path does not exist.
    /// - The path is neither a regular file nor a directory.
    /// - Permission denied.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform};
    ///
    /// let platform = agplatform::platform();
    /// let fs = platform.fs();
    /// # let fs = agplatform::test_platform::test_fs::TestFs::new();
    /// fs.write("/data.txt", "hello").unwrap();
    /// fs.set_readonly("/data.txt", true).unwrap();
    /// assert!(fs.metadata("/data.txt").unwrap().readonly);
    /// ```
    fn set_readonly<P: AsRef<Path>>(&self, path: P, readonly: bool) -> Result;

    /// Sets or clears the [`executable`](Metadata::executable) attribute
    /// of a file. Symlinks are followed.
    ///
    /// Unix only: on Windows executability is derived from the file
    /// extension and cannot be set.
    ///
    /// Does nothing if the attribute already has the requested value. No
    /// permission check is made in that case so the call succeeds even if
    /// the caller would not be allowed to change the attribute.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The path does not exist.
    /// - The path is a directory, whose execute bits grant traversal.
    /// - The path is neither a regular file nor a directory.
    /// - Permission denied.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform};
    ///
    /// let platform = agplatform::platform();
    /// let fs = platform.fs();
    /// # let fs = agplatform::test_platform::test_fs::TestFs::new();
    /// fs.write("/run.sh", "echo hello").unwrap();
    /// fs.set_executable("/run.sh", true).unwrap();
    /// ```
    #[cfg(unix)]
    fn set_executable<P: AsRef<Path>>(&self, path: P, executable: bool) -> Result;

    /// Sets or clears the [`hidden`](Metadata::hidden) attribute of a
    /// file or directory. Symlinks are followed.
    ///
    /// Windows only: on Unix a file is hidden when its name starts with
    /// `.`, which cannot be set without renaming it.
    ///
    /// Does nothing if the attribute already has the requested value. No
    /// permission check is made in that case so the call succeeds even if
    /// the caller would not be allowed to change the attribute.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The path does not exist.
    /// - The path is neither a regular file nor a directory.
    /// - Permission denied.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform};
    ///
    /// let platform = agplatform::platform();
    /// let fs = platform.fs();
    /// # let fs = agplatform::test_platform::test_fs::TestFs::new();
    /// fs.write("/cache.db", "").unwrap();
    /// fs.set_hidden("/cache.db", true).unwrap();
    /// ```
    #[cfg(windows)]
    fn set_hidden<P: AsRef<Path>>(&self, path: P, hidden: bool) -> Result;

    /// Sets the [`modified`](Metadata::modified) time of a file or
    /// directory. Symlinks are followed.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The path does not exist.
    /// - The path is neither a regular file nor a directory.
    /// - Permission denied.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform};
    /// use std::time::SystemTime;
    ///
    /// let platform = agplatform::platform();
    /// let fs = platform.fs();
    /// # let fs = agplatform::test_platform::test_fs::TestFs::new();
    /// fs.write("/data.txt", "hello").unwrap();
    /// fs.set_modified("/data.txt", SystemTime::UNIX_EPOCH).unwrap();
    /// assert_eq!(fs.metadata("/data.txt").unwrap().modified, SystemTime::UNIX_EPOCH);
    /// ```
    fn set_modified<P: AsRef<Path>>(&self, path: P, modified: SystemTime) -> Result;

    /// Sets the [`mode`](Metadata::mode) of a file or directory: the
    /// permission bits including the set-user-ID, set-group-ID and sticky
    /// bits. Bits above `0o7777` are ignored. Symlinks are followed.
    ///
    /// Unix only: Windows has no mode, std supports only the
    /// [`readonly`](Metadata::readonly) attribute there.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The path does not exist.
    /// - The path is neither a regular file nor a directory.
    /// - Permission denied, e.g. the caller does not own the entry.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform};
    ///
    /// let platform = agplatform::platform();
    /// let fs = platform.fs();
    /// # let fs = agplatform::test_platform::test_fs::TestFs::new();
    /// fs.write("/secret.txt", "hello").unwrap();
    /// fs.set_mode("/secret.txt", 0o600).unwrap();
    /// assert_eq!(fs.metadata("/secret.txt").unwrap().mode, 0o600);
    /// ```
    #[cfg(unix)]
    fn set_mode<P: AsRef<Path>>(&self, path: P, mode: u32) -> Result;
}

/// Options for opening a file in write mode.
///
/// Constructed via a builder pattern. The underlying file descriptor is
/// always opened with both read and write access; these options control
/// creation, truncation, and append behavior.
///
/// # Examples
///
/// ```rust
/// use agplatform::OpenWriteOptions;
///
/// let options = OpenWriteOptions::new()
///     .create()
///     .truncate();
/// ```
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OpenWriteOptions {
    /// Writes are appended to the end of the file regardless of the
    /// current seek position.
    pub append: bool,
    /// Truncate the file to zero length when opened.
    pub truncate: bool,
    /// Create the file if it does not exist.
    pub create: bool,
    /// Create a new file, failing if it already exists.
    pub create_new: bool,
    /// The mode a new file is created with, see
    /// [`mode`](Self::mode) (Unix only).
    #[cfg(unix)]
    pub mode: Option<u32>,
}

impl OpenWriteOptions {
    /// Returns a new set of options with all flags disabled.
    pub const fn new() -> Self {
        OpenWriteOptions {
            append: false,
            truncate: false,
            create: false,
            create_new: false,
            #[cfg(unix)]
            mode: None,
        }
    }

    /// Sets append mode.
    ///
    /// Every write will seek to the end of the file before writing,
    /// regardless of the current cursor position. This is useful for
    /// log files or append-only data structures.
    pub const fn append(mut self) -> Self {
        self.append = true;
        self
    }

    /// Creates the file if it does not already exist.
    pub const fn create(mut self) -> Self {
        self.create = true;
        self
    }

    /// Creates a new file, returning an error if it already exists.
    ///
    /// This is useful for ensuring atomic file creation without
    /// accidentally overwriting existing data.
    pub const fn create_new(mut self) -> Self {
        self.create_new = true;
        self
    }

    /// Truncates the file to zero length when opened.
    ///
    /// Cannot be combined with [`append`](Self::append).
    pub const fn truncate(mut self) -> Self {
        self.truncate = true;
        self
    }

    /// Sets the [`mode`](Metadata::mode) a new file is created with, as
    /// restricted by the umask of the process. Without it a file is created
    /// with `0o666` restricted by the umask. An existing file keeps its mode.
    ///
    /// Unix only: Windows has no mode, see [`Fs::set_mode`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::OpenWriteOptions;
    ///
    /// let options = OpenWriteOptions::new().create_new().mode(0o600);
    /// ```
    #[cfg(unix)]
    pub const fn mode(mut self, mode: u32) -> Self {
        self.mode = Some(mode);
        self
    }
}

/// Distinguishes files from directories in a [`DirEntry`].
///
/// # Examples
///
/// ```rust
/// use agplatform::FileType;
///
/// let ft = FileType::Dir;
/// assert!(ft.is_dir());
/// assert!(!ft.is_file());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    /// A regular file.
    File,
    /// A directory.
    Dir,
}

impl FileType {
    /// Returns `true` if this entry is a directory.
    pub fn is_dir(&self) -> bool {
        matches!(self, FileType::Dir)
    }

    /// Returns `true` if this entry is a regular file.
    pub fn is_file(&self) -> bool {
        matches!(self, FileType::File)
    }
}

/// A single entry inside a directory, returned by iterating the result
/// of [`Fs::read_dir`].
///
/// Provides the full path, the file name component, and the entry type.
///
/// # Examples
///
/// ```rust
/// use agplatform::{Fs, Platform};
///
/// let platform = agplatform::platform();
/// let fs = platform.fs();
/// # let fs = agplatform::test_platform::test_fs::TestFs::new();
/// # fs.create_dir_all("/mydir").unwrap();
/// # fs.write("/mydir/a.txt", "").unwrap();
/// for entry in fs.read_dir("/mydir").unwrap() {
///     println!("{}: {:?}", entry.path().display(), entry.file_type());
/// }
/// ```
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub(crate) path: PathBuf,
    pub(crate) file_type: FileType,
}

impl DirEntry {
    /// [internal]
    ///
    /// Builds an entry for [`Fs::read_dir`] implementations. Returns `None`
    /// for names that are not valid UTF-8 (same as in `Env`), which keeps
    /// [`file_name`](Self::file_name) panic-free.
    pub(crate) fn new(path: PathBuf, file_type: FileType) -> Option<Self> {
        path.file_name().and_then(|name| name.to_str())?;
        Some(Self { path, file_type })
    }

    /// Returns the full path to this entry.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the bare file name of this entry (no leading path).
    ///
    /// # Panics
    ///
    /// Panics if the path has no file name component (e.g. a root
    /// path) or the file name is not valid UTF-8. This cannot happen
    /// for entries produced by [`Fs::read_dir`], which skips entries
    /// with non-UTF-8 names.
    pub fn file_name(&self) -> &str {
        self.path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("DirEntry path has no UTF-8 file name component")
    }

    /// Returns the type of this entry (file or directory).
    pub fn file_type(&self) -> FileType {
        self.file_type
    }
}

/// [internal]
///
/// The flags of [`Metadata`]: natively stored on Windows, derived from the
/// mode on Unix (see `UnixAttributes`), or with the platform semantics
/// applied by [`platform_attributes`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Attributes {
    pub(crate) readonly: bool,
    pub(crate) executable: bool,
    pub(crate) hidden: bool,
}

impl Attributes {
    pub(crate) fn set(&mut self, attribute: Attribute, value: bool) {
        match attribute {
            Attribute::Readonly => self.readonly = value,
            #[cfg(unix)]
            Attribute::Executable => self.executable = value,
            #[cfg(windows)]
            Attribute::Hidden => self.hidden = value,
        }
    }

    /// Returns the natively stored flags, which on Windows are the
    /// attributes themselves, see `UnixAttributes::attributes`.
    #[cfg(windows)]
    pub(crate) fn attributes(&self) -> Attributes {
        *self
    }
}

/// [internal]
///
/// The attributes of an entry as natively stored by the platform, shared by
/// [`FsImpl`] and [`TestFs`](crate::test_platform::test_fs::TestFs).
#[cfg(unix)]
pub(crate) type NativeAttributes = UnixAttributes;
/// [internal]
///
/// The attributes of an entry as natively stored by the platform, shared by
/// [`FsImpl`] and [`TestFs`](crate::test_platform::test_fs::TestFs).
#[cfg(windows)]
pub(crate) type NativeAttributes = Attributes;

/// Metadata of a file or directory, returned by [`Fs::metadata`].
///
/// The `readonly`, `executable` and `hidden` flags map to the native
/// concept of the current platform. Where there is none, the flag is
/// derived from the path and its setter is not available on that
/// platform. On Unix the `mode` and owner are reported as well, Windows has
/// no equivalent in std:
///
/// | Flag         | Unix                                   | Windows                                             |
/// |--------------|----------------------------------------|-----------------------------------------------------|
/// | `readonly`   | no write permission bits               | `FILE_ATTRIBUTE_READONLY`                           |
/// | `executable` | any execute permission bit, files only | derived: `.exe`, `.com`, `.bat` or `.cmd` extension |
/// | `hidden`     | derived: name starts with `.`          | `FILE_ATTRIBUTE_HIDDEN`                             |
///
/// # Examples
///
/// ```rust
/// use agplatform::{Fs, Platform};
///
/// let platform = agplatform::platform();
/// let fs = platform.fs();
/// # let fs = agplatform::test_platform::test_fs::TestFs::new();
/// fs.write("/data.txt", "hello").unwrap();
/// let metadata = fs.metadata("/data.txt").unwrap();
/// assert!(!metadata.readonly);
/// assert!(metadata.modified <= std::time::SystemTime::now());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    /// The type of the entry (file or directory).
    pub file_type: FileType,
    /// The size of a file in bytes. The value for directories is platform
    /// specific.
    pub len: u64,
    /// The last modification time, or [`SystemTime::UNIX_EPOCH`] on
    /// platforms that do not record it.
    pub modified: SystemTime,
    /// The creation time, or [`SystemTime::UNIX_EPOCH`] if the platform or
    /// filesystem does not record it (e.g. Linux before 4.11 or
    /// filesystems without `statx` birth time support).
    pub created: SystemTime,
    /// The entry cannot be written to, see [`Fs::set_readonly`].
    pub readonly: bool,
    /// The file can be executed, see `Fs::set_executable` (Unix only).
    pub executable: bool,
    /// The entry is hidden from regular directory listings, see
    /// `Fs::set_hidden` (Windows only).
    pub hidden: bool,
    /// The permission bits including the set-user-ID, set-group-ID and
    /// sticky bits (`0o7777`), see `Fs::set_mode` (Unix only).
    #[cfg(unix)]
    pub mode: u32,
    /// The user ID of the owner (Unix only).
    #[cfg(unix)]
    pub uid: u32,
    /// The group ID of the owner (Unix only).
    #[cfg(unix)]
    pub gid: u32,
}

impl Metadata {
    pub(crate) fn new(
        path: &Path,
        file_type: FileType,
        len: u64,
        modified: SystemTime,
        created: SystemTime,
        native: NativeAttributes,
    ) -> Self {
        let attributes = platform_attributes(path, file_type, native.attributes());

        Self {
            file_type,
            len,
            modified,
            created,
            readonly: attributes.readonly,
            executable: attributes.executable,
            hidden: attributes.hidden,
            #[cfg(unix)]
            mode: native.mode,
            #[cfg(unix)]
            uid: native.uid,
            #[cfg(unix)]
            gid: native.gid,
        }
    }
}

/// [internal]
///
/// Selects a single flag of [`Attributes`], letting the `Fs::set_*`
/// methods share one non-generic implementation.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Attribute {
    Readonly,
    #[cfg(unix)]
    Executable,
    #[cfg(windows)]
    Hidden,
}

/// [internal]
///
/// Applies the platform semantics described in [`Metadata`] to the natively
/// `stored` flags.
fn platform_attributes(path: &Path, file_type: FileType, stored: Attributes) -> Attributes {
    #[cfg(unix)]
    let attributes = Attributes {
        readonly: stored.readonly,
        executable: file_type.is_file() && stored.executable,
        hidden: path
            .file_name()
            .is_some_and(|name| name.as_encoded_bytes().starts_with(b".")),
    };
    #[cfg(windows)]
    let attributes = Attributes {
        readonly: stored.readonly,
        executable: file_type.is_file()
            && path.extension().is_some_and(|extension| {
                ["exe", "com", "bat", "cmd"]
                    .iter()
                    .any(|executable| extension.eq_ignore_ascii_case(executable))
            }),
        hidden: stored.hidden,
    };

    attributes
}

/// Tracks whether a file is currently borrowed for reading or writing.
///
/// Used internally by [`FsImpl`] and
/// [`TestFs`](crate::test_platform::test_fs::TestFs) to enforce the
/// one-writer-or-many-readers invariant at runtime.
#[derive(Debug, Clone, Copy)]
pub enum BorrowState {
    /// The file is borrowed for reading by the given number of handles.
    Read(u64),
    /// The file is borrowed for writing by exactly one handle.
    Write,
}

/// The shared state behind every [`FileReadHandle`] and [`FileWriteHandle`].
///
/// Holds the OS file descriptor and a reference to the borrow-tracking
/// table. When dropped, the entry in the table is decremented or removed
/// so the file can be opened again.
pub struct FileHandle {
    handles: Arc<Mutex<HashMap<PathBuf, BorrowState>>>,
    path: PathBuf,
    file: File,
}

/// Locks the given mutex and returns the guard. If the mutex is poisoned,
/// it clears the poison and returns the inner value. Use only when you are
/// sure it is safe to ignore the poison.
pub(crate) fn lock_handles<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| {
        mutex.clear_poison();
        e.into_inner()
    })
}

impl Drop for FileHandle {
    fn drop(&mut self) {
        let mut handles = lock_handles(&self.handles);

        if let Some(state) = handles.get_mut(&self.path) {
            match state {
                BorrowState::Read(count) => {
                    *count = count.saturating_sub(1);

                    if *count == 0 {
                        handles.remove(&self.path);
                    }
                }
                BorrowState::Write => {
                    handles.remove(&self.path);
                }
            }
        }
    }
}

/// A read-only file handle backed by [`std::fs::File`].
///
/// Implements [`Read`] and [`Seek`]. Dropping the handle releases the
/// read borrow, allowing the file to be opened for writing.
pub struct FileReadHandle(FileHandle);

impl FileReadHandle {
    fn acquire(
        handles: Arc<Mutex<HashMap<PathBuf, BorrowState>>>,
        path: PathBuf,
        file: File,
    ) -> Result<Self> {
        {
            let mut handles = lock_handles(&handles);
            let entry = handles.entry(path.clone());

            match entry {
                hash_map::Entry::Occupied(mut open) => match open.get() {
                    BorrowState::Read(count) => {
                        open.insert(BorrowState::Read(count + 1));
                    }
                    BorrowState::Write => {
                        return Err(fs_error!(
                            "File could not be opened, it is already borrowed for writing: {}",
                            path.display()
                        ));
                    }
                },
                hash_map::Entry::Vacant(_) => {
                    entry.insert_entry(BorrowState::Read(1));
                }
            }
        }

        Ok(Self(FileHandle {
            handles,
            path,
            file,
        }))
    }
}

impl Read for FileReadHandle {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.file.read(buf)
    }
}

impl Seek for FileReadHandle {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.file.seek(pos)
    }
}

/// A read-write file handle backed by [`std::fs::File`].
///
/// Implements [`Read`], [`Write`], and [`Seek`]. Dropping the handle
/// releases the write borrow, allowing the file to be opened again for
/// reading or writing.
pub struct FileWriteHandle(FileHandle);

impl FileWriteHandle {
    fn acquire(
        handles: Arc<Mutex<HashMap<PathBuf, BorrowState>>>,
        path: PathBuf,
        options: &mut OpenOptions,
    ) -> Result<Self> {
        let file;

        {
            let mut handles = lock_handles(&handles);
            let entry = handles.entry(path.clone());

            match entry {
                hash_map::Entry::Occupied(open) => match open.get() {
                    BorrowState::Read(count) => {
                        return Err(fs_error!(
                            "File could not be opened, it is already borrowed for reading {count} times: {}",
                            path.display()
                        ));
                    }
                    BorrowState::Write => {
                        return Err(fs_error!(
                            "File could not be opened, it is already borrowed for writing: {}",
                            path.display()
                        ));
                    }
                },
                hash_map::Entry::Vacant(_) => {
                    file = options.open(&path).map_err(fs_error_at(&path))?;
                    entry.insert_entry(BorrowState::Write);
                }
            }
        }

        Ok(Self(FileHandle {
            handles,
            path,
            file,
        }))
    }
}

impl Read for FileWriteHandle {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.file.read(buf)
    }
}

impl Seek for FileWriteHandle {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.0.file.seek(pos)
    }
}

impl Write for FileWriteHandle {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.file.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.file.flush()
    }
}

/// The real filesystem implementation backed by [`std::fs`].
///
/// Relative paths are resolved against the configured current directory.
/// Borrow tracking is enforced at runtime: opening a file for writing
/// while it is already open (for reading or writing) returns an error.
///
/// Normally obtained via [`platform()`](crate::platform) rather than
/// constructed directly.
pub struct FsImpl {
    pub(crate) current_dir: PathBuf,
    handles: Arc<Mutex<HashMap<PathBuf, BorrowState>>>,
}

impl FsImpl {
    /// Creates a new `FsImpl` rooted at the given directory.
    ///
    /// Relative paths passed to any [`Fs`] method will be resolved
    /// against `current_dir`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform};
    ///
    /// let platform = agplatform::platform();
    /// let content = platform.fs().read("Cargo.toml").unwrap();
    /// assert!(!content.is_empty());
    /// ```
    pub fn new(current_dir: PathBuf) -> Self {
        FsImpl {
            current_dir,
            handles: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl CurrentDirSetter for FsImpl {
    fn set_current_dir(&mut self, path: PathBuf) {
        self.current_dir = path;
    }
}

impl Fs for FsImpl {
    type ReadHandle = FileReadHandle;
    type WriteHandle = FileWriteHandle;
    type Dir = std::vec::IntoIter<DirEntry>;

    fn open_read<P: AsRef<Path>>(&self, path: P) -> Result<Self::ReadHandle> {
        let normalized_path = normalize_path(&self.current_dir, path);

        if std::fs::metadata(&normalized_path).is_ok_and(|metadata| metadata.is_dir()) {
            return Err(fs_error!(
                "File could not be opened, path is a directory: {}",
                normalized_path.display()
            ));
        }

        let file = File::open(&normalized_path).map_err(fs_error_at(&normalized_path))?;

        FileReadHandle::acquire(self.handles.clone(), normalized_path, file)
    }

    fn open_write_options<P: AsRef<Path>>(
        &self,
        path: P,
        options: OpenWriteOptions,
    ) -> Result<Self::WriteHandle> {
        let normalized_path = normalize_path(&self.current_dir, path);
        let mut open_options = File::options();
        open_options
            .read(true)
            .write(true)
            .create(options.create)
            .create_new(options.create_new)
            .append(options.append)
            .truncate(options.truncate);

        #[cfg(unix)]
        if let Some(mode) = options.mode {
            std::os::unix::fs::OpenOptionsExt::mode(&mut open_options, mode);
        }

        FileWriteHandle::acquire(self.handles.clone(), normalized_path, &mut open_options)
    }

    fn read_dir<P: AsRef<Path>>(&self, path: P) -> Result<Self::Dir> {
        let normalized_path = normalize_path(&self.current_dir, path);
        let to_entry = |path: PathBuf, file_type: std::fs::FileType| {
            file_type
                .is_dir()
                .then_some(FileType::Dir)
                .or(file_type.is_file().then_some(FileType::File))
                .and_then(|file_type| DirEntry::new(path, file_type))
        };

        let entries: Vec<DirEntry> = std::fs::read_dir(&normalized_path)
            .and_then(|dir| {
                dir.filter_map(|result| {
                    result
                        .and_then(|entry| entry.file_type().map(|t| to_entry(entry.path(), t)))
                        .transpose()
                })
                .collect()
            })
            .map_err(fs_error_at(&normalized_path))?;

        Ok(entries.into_iter())
    }

    fn create_dir_all<P: AsRef<Path>>(&self, path: P) -> Result {
        let normalized_path = normalize_path(&self.current_dir, path);
        std::fs::create_dir_all(&normalized_path).map_err(fs_error_at(&normalized_path))
    }

    fn exists<P: AsRef<Path>>(&self, path: P) -> Result<bool> {
        let normalized_path = normalize_path(&self.current_dir, path);
        std::fs::exists(&normalized_path).map_err(fs_error_at(&normalized_path))
    }

    fn metadata<P: AsRef<Path>>(&self, path: P) -> Result<Metadata> {
        let normalized_path = normalize_path(&self.current_dir, path);
        let (metadata, file_type) = native_metadata(&normalized_path)?;

        Ok(Metadata::new(
            &normalized_path,
            file_type,
            metadata.len(),
            metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
            metadata.created().unwrap_or(SystemTime::UNIX_EPOCH),
            native_attributes(&metadata),
        ))
    }

    fn set_readonly<P: AsRef<Path>>(&self, path: P, readonly: bool) -> Result {
        self.set_attribute(path.as_ref(), Attribute::Readonly, readonly)
    }

    #[cfg(unix)]
    fn set_executable<P: AsRef<Path>>(&self, path: P, executable: bool) -> Result {
        self.set_attribute(path.as_ref(), Attribute::Executable, executable)
    }

    #[cfg(windows)]
    fn set_hidden<P: AsRef<Path>>(&self, path: P, hidden: bool) -> Result {
        self.set_attribute(path.as_ref(), Attribute::Hidden, hidden)
    }

    fn set_modified<P: AsRef<Path>>(&self, path: P, modified: SystemTime) -> Result {
        let normalized_path = normalize_path(&self.current_dir, path);
        native_metadata(&normalized_path)?;

        open_for_metadata(&normalized_path)
            .and_then(|file| file.set_modified(modified))
            .map_err(fs_error_at(&normalized_path))
    }

    #[cfg(unix)]
    fn set_mode<P: AsRef<Path>>(&self, path: P, mode: u32) -> Result {
        use std::os::unix::fs::PermissionsExt;

        let normalized_path = normalize_path(&self.current_dir, path);
        native_metadata(&normalized_path)?;

        std::fs::set_permissions(
            &normalized_path,
            std::fs::Permissions::from_mode(mode & 0o7777),
        )
        .map_err(fs_error_at(&normalized_path))
    }
}

impl FsImpl {
    fn set_attribute(&self, path: &Path, attribute: Attribute, value: bool) -> Result {
        let normalized_path = normalize_path(&self.current_dir, path);
        let (metadata, _) = native_metadata(&normalized_path)?;

        #[cfg(unix)]
        if matches!(attribute, Attribute::Executable) && metadata.is_dir() {
            return Err(fs_error!(
                "Executable attribute cannot be set, path is a directory: {}",
                normalized_path.display()
            ));
        }

        let mut native = native_attributes(&metadata);
        native.set(attribute, value);

        set_native_attributes(&normalized_path, &metadata, native)
            .map_err(fs_error_at(&normalized_path))
    }
}

/// [internal]
///
/// Returns a mapping into an [`Error::fs`] whose description is the mapped
/// error followed by the `path` it occurred on and whose source is the
/// mapped error, for use with [`Result::map_err`].
pub(crate) fn fs_error_at<T: std::error::Error + Send + Sync + 'static>(
    path: &Path,
) -> impl FnOnce(T) -> Error + '_ {
    move |error| fs_error!("{error}: {}", path.display()).with_source(error)
}

/// [internal]
///
/// Normalizes and resolves the given path relative to the current directory
/// resolving any intermediate `.` and `..` components. Operates on `std::path::Path`
/// so it resolves the path components correctly according to the current file system semantics.
///
/// Examples:
///
/// - `Cargo.toml` -> `<current_dir>/Cargo.toml`
/// - `./src/../Cargo.toml` -> `<current_dir>/Cargo.toml`
/// - `/etc/hosts` -> `/etc/hosts`
pub(crate) fn normalize_path<C: AsRef<Path>, P: AsRef<Path>>(current_dir: C, path: P) -> PathBuf {
    cannonicalize_path(current_dir.as_ref(), path.as_ref())
}

fn cannonicalize_path(current_dir: &Path, path: &Path) -> PathBuf {
    let mut base = if path.is_relative() {
        current_dir.to_path_buf()
    } else {
        PathBuf::new()
    };

    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                base.pop();
            }
            Component::RootDir | Component::Prefix(_) => {
                base.push(component);
            }
            Component::Normal(part) => {
                base.push(part);
            }
        }
    }

    base
}

#[cfg(test)]
mod tests {
    use std::io::SeekFrom;

    use super::*;

    struct TestFile {
        path: PathBuf,
    }

    impl TestFile {
        pub fn new(path: PathBuf) -> Self {
            let _ = std::fs::remove_file(&path);
            Self { path }
        }
    }

    impl Drop for TestFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    struct TestDir {
        path: PathBuf,
    }

    impl TestDir {
        pub fn new(path: PathBuf) -> Self {
            let _ = std::fs::remove_dir_all(&path);
            Self { path }
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn assert_os_error(error: Error, path: &Path) {
        let suffix = format!(": {}", path.display());
        let os_message = error.description().strip_suffix(&suffix);
        assert!(
            os_message.is_some_and(|message| !message.is_empty()),
            "error should be the OS message followed by the path, got: {error}"
        );
    }

    mod fs_impl {
        use super::*;

        #[test]
        fn fs_impl_should_enforce_one_writer_or_many_readers() {
            let fs = FsImpl::new(std::env::current_dir().unwrap());
            let normalized_path = normalize_path(&fs.current_dir, "Cargo.toml");

            // GIVEN: two read handles are open
            let read1 = fs.open_read("Cargo.toml").unwrap();
            let read2 = fs.open_read("Cargo.toml").unwrap();

            // THEN: write fails because file is read-borrowed twice
            let err = fs.open_write("Cargo.toml").err().unwrap().to_string();
            assert_eq!(
                err,
                fs_error!(
                    "File could not be opened, it is already borrowed for reading 2 times: {}",
                    normalized_path.display()
                )
                .to_string()
            );

            // WHEN: both reads are dropped
            drop(read1);
            drop(read2);

            // THEN: open write handle succeeds
            let write_handle = fs.open_write("Cargo.toml").unwrap();

            // AND: read now fails while write is active
            let err = fs.open_read("Cargo.toml").err().unwrap().to_string();
            assert_eq!(
                err,
                fs_error!(
                    "File could not be opened, it is already borrowed for writing: {}",
                    normalized_path.display()
                )
                .to_string()
            );

            // AND: convenience methods also fail while write-borrowed
            assert!(fs.read("Cargo.toml").is_err());
            assert!(fs.write("Cargo.toml", "data").is_err());

            // AND: second write also fails
            let err = fs.open_write("Cargo.toml").err().unwrap().to_string();
            assert_eq!(
                err,
                fs_error!(
                    "File could not be opened, it is already borrowed for writing: {}",
                    normalized_path.display()
                )
                .to_string()
            );

            // WHEN: write is dropped
            drop(write_handle);

            // THEN: read succeeds again
            let _read = fs.open_read("Cargo.toml").unwrap();
        }

        mod current_dir_setter {
            use super::*;

            #[test]
            fn set_current_dir_should_update_current_directory() {
                let mut fs = FsImpl::new(PathBuf::new());
                let new_dir = PathBuf::from("/tmp");
                fs.set_current_dir(new_dir.clone());
                assert_eq!(fs.current_dir, new_dir);
            }
        }

        mod fs {
            use super::*;

            mod open_read {
                use super::*;

                #[test]
                fn open_read_should_return_readable_handle() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    let mut file = fs.open_read("Cargo.toml").unwrap();
                    let len = file.seek(SeekFrom::End(0)).unwrap() as usize;
                    assert!(len > 0);
                    file.seek(SeekFrom::Start(0)).unwrap();
                    let mut content = vec![0_u8; len];
                    let read = file.read(&mut content).unwrap();
                    assert_eq!(read, len);
                }

                #[test]
                fn open_read_should_fail_for_nonexistent_file() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    assert_os_error(
                        fs.open_read("nonexistent_file.txt").err().unwrap(),
                        &normalize_path(&fs.current_dir, "nonexistent_file.txt"),
                    );
                }

                #[test]
                fn open_read_dir_should_fail() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());

                    assert_eq!(
                        fs.open_read("src").err().unwrap().to_string(),
                        fs_error!(
                            "File could not be opened, path is a directory: {}",
                            fs.current_dir.join("src").display()
                        )
                        .to_string()
                    );
                }

                #[test]
                fn open_read_should_fail_when_file_is_write_borrowed() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    let _write_handle = fs.open_write("Cargo.toml").unwrap();
                    assert!(fs.open_read("Cargo.toml").is_err());
                }
            }

            mod open_write {
                use super::*;

                #[test]
                fn open_write_should_return_writable_handle_to_new_or_existent_file() {
                    let _test_file = TestFile::new(
                        std::env::current_dir()
                            .unwrap()
                            .join("fs_test_open_write.tmp"),
                    );
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    let content = b"content";
                    let mut file = fs.open_write("fs_test_open_write.tmp").unwrap();
                    let written = file.write(content).unwrap();
                    assert_eq!(written, content.len());

                    drop(file);

                    let mut file = fs.open_write("fs_test_open_write.tmp").unwrap();
                    let len = file.seek(SeekFrom::End(0)).unwrap() as usize;
                    assert_eq!(len, content.len());

                    file.seek(SeekFrom::Start(0)).unwrap();
                    let mut read_content = vec![0_u8; content.len()];
                    let read = file.read(&mut read_content).unwrap();
                    assert_eq!(read, len);
                    assert_eq!(read_content, content);
                }
            }

            mod open_write_options {
                use super::*;

                #[test]
                fn open_write_options_append_should_only_append_to_existing_content() {
                    let _test_file =
                        TestFile::new(std::env::current_dir().unwrap().join("fs_test_append.tmp"));
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    let content = b"content";
                    fs.write("fs_test_append.tmp", content).unwrap();

                    let mut file = fs
                        .open_write_options("fs_test_append.tmp", OpenWriteOptions::new().append())
                        .unwrap();

                    // WHY: append mode always writes at the end regardless of the seek position
                    file.seek(SeekFrom::Start(0)).unwrap();
                    let written = file.write(content).unwrap();
                    assert_eq!(written, content.len());
                    let current = file.stream_position().unwrap() as usize;
                    assert_eq!(current, 2 * written);

                    file.flush().unwrap();
                }

                #[test]
                fn open_write_options_truncate_should_clear_existing_content() {
                    let _test_file = TestFile::new(
                        std::env::current_dir()
                            .unwrap()
                            .join("fs_test_truncate.tmp"),
                    );
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    fs.write("fs_test_truncate.tmp", "content").unwrap();

                    let mut file = fs
                        .open_write_options(
                            "fs_test_truncate.tmp",
                            OpenWriteOptions::new().truncate(),
                        )
                        .unwrap();
                    let len = file.seek(SeekFrom::End(0)).unwrap() as usize;
                    assert_eq!(len, 0);
                }

                #[test]
                fn open_write_options_create_new_should_fail_on_existing_file() {
                    let _test_file = TestFile::new(
                        std::env::current_dir()
                            .unwrap()
                            .join("fs_test_create_new.tmp"),
                    );
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    fs.write("fs_test_create_new.tmp", "content").unwrap();

                    assert_os_error(
                        fs.open_write_options(
                            "fs_test_create_new.tmp",
                            OpenWriteOptions::new().create_new(),
                        )
                        .err()
                        .unwrap(),
                        &normalize_path(&fs.current_dir, "fs_test_create_new.tmp"),
                    );
                }

                #[cfg(unix)]
                #[test]
                fn open_write_options_mode_should_apply_to_new_file_only() {
                    let path = std::env::current_dir()
                        .unwrap()
                        .join("fs_test_open_write_mode.tmp");
                    let _test_file = TestFile::new(path.clone());
                    let fs = FsImpl::new(std::env::current_dir().unwrap());

                    // WHY: the umask can only remove bits, 0o600 survives any
                    // common one.
                    drop(
                        fs.open_write_options(
                            "fs_test_open_write_mode.tmp",
                            OpenWriteOptions::new().create_new().mode(0o600),
                        )
                        .unwrap(),
                    );
                    assert_eq!(mode(&path), 0o600);

                    drop(
                        fs.open_write_options(
                            "fs_test_open_write_mode.tmp",
                            OpenWriteOptions::new().create().mode(0o644),
                        )
                        .unwrap(),
                    );
                    assert_eq!(mode(&path), 0o600);
                }

                #[test]
                fn open_write_options_append_with_truncate_should_be_rejected() {
                    let _test_file = TestFile::new(
                        std::env::current_dir()
                            .unwrap()
                            .join("fs_test_reject_append_truncate.tmp"),
                    );
                    let fs = FsImpl::new(std::env::current_dir().unwrap());

                    assert!(
                        fs.open_write_options(
                            "fs_test_reject_append_truncate.tmp",
                            OpenWriteOptions::new().append().truncate()
                        )
                        .is_err()
                    );
                    assert!(
                        fs.open_write_options(
                            "fs_test_reject_append_truncate.tmp",
                            OpenWriteOptions::new().append().truncate().create()
                        )
                        .is_err()
                    );
                    assert!(
                        fs.open_write_options(
                            "fs_test_reject_append_truncate.tmp",
                            OpenWriteOptions::new().append().truncate().create_new()
                        )
                        .is_ok()
                    );
                    assert!(
                        fs.open_write_options(
                            "fs_test_reject_append_truncate.tmp",
                            OpenWriteOptions::new().append().truncate().create_new()
                        )
                        .is_err()
                    );
                }
            }

            mod read {
                use super::*;

                #[test]
                fn read_should_return_file_contents_as_bytes() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    let path = fs
                        .current_dir
                        .join("Cargo.toml")
                        .to_string_lossy()
                        .to_string();
                    let result = fs.read(path.as_str()).unwrap();
                    assert!(!result.is_empty());
                }
            }

            mod read_to_string {
                use super::*;

                #[test]
                fn read_to_string_should_return_file_contents_as_string() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    let result = fs.read_to_string("./src/../Cargo.toml").unwrap();
                    assert!(!result.is_empty());
                }

                #[test]
                fn read_to_string_should_fail_for_nonexistent_file() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    assert!(fs.read_to_string("Cargo.toml2").is_err());
                }
            }

            mod write {
                use super::*;

                #[test]
                fn write_should_create_file_and_write_content() {
                    let _test_file = TestFile::new(
                        std::env::current_dir()
                            .unwrap()
                            .join("fs_test_write_create.tmp"),
                    );
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    fs.write("fs_test_write_create.tmp", "hello").unwrap();
                    assert_eq!(
                        fs.read_to_string("fs_test_write_create.tmp").unwrap(),
                        "hello"
                    );
                }

                #[test]
                fn write_should_overwrite_existing_content() {
                    let _test_file = TestFile::new(
                        std::env::current_dir()
                            .unwrap()
                            .join("fs_test_write_overwrite.tmp"),
                    );
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    fs.write("fs_test_write_overwrite.tmp", "hello").unwrap();
                    fs.write("fs_test_write_overwrite.tmp", "a").unwrap();
                    assert_eq!(
                        fs.read_to_string("fs_test_write_overwrite.tmp").unwrap(),
                        "a"
                    );
                }

                #[test]
                fn write_should_fail_when_file_is_write_borrowed() {
                    let _test_file = TestFile::new(
                        std::env::current_dir()
                            .unwrap()
                            .join("fs_test_write_while_write_borrowed.tmp"),
                    );
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    fs.write("fs_test_write_while_write_borrowed.tmp", "hello")
                        .unwrap();
                    let _write_handle = fs
                        .open_write("fs_test_write_while_write_borrowed.tmp")
                        .unwrap();
                    assert!(
                        fs.write("fs_test_write_while_write_borrowed.tmp", "world")
                            .is_err()
                    );
                }

                #[test]
                fn write_should_fail_when_file_is_read_borrowed() {
                    let _test_file = TestFile::new(
                        std::env::current_dir()
                            .unwrap()
                            .join("fs_test_write_while_read_borrowed.tmp"),
                    );
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    fs.write("fs_test_write_while_read_borrowed.tmp", "hello")
                        .unwrap();
                    let _read_handle = fs
                        .open_read("fs_test_write_while_read_borrowed.tmp")
                        .unwrap();
                    fs.write("fs_test_write_while_read_borrowed.tmp", "world")
                        .unwrap_err();
                    assert_eq!(
                        fs.read_to_string("fs_test_write_while_read_borrowed.tmp")
                            .unwrap(),
                        "hello"
                    );
                }
            }

            mod read_dir {
                use super::*;

                #[test]
                fn read_dir_should_list_directory_contents() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    let entries: Vec<DirEntry> = fs.read_dir("src").unwrap().collect();
                    let names: Vec<String> =
                        entries.iter().map(|e| e.file_name().to_string()).collect();
                    assert!(names.contains(&"fs.rs".to_string()));
                    assert!(names.contains(&"lib.rs".to_string()));
                }

                #[test]
                fn read_dir_should_distinguish_files_and_directories() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    let entries: Vec<DirEntry> = fs.read_dir("src").unwrap().collect();
                    let fs_entry = entries.iter().find(|e| e.file_name() == "fs.rs").unwrap();
                    assert!(
                        fs_entry.file_type().is_file(),
                        "expected fs.rs to be a file"
                    );

                    let platform_entry = entries
                        .iter()
                        .find(|e| e.file_name() == "platform")
                        .unwrap();
                    assert!(
                        platform_entry.file_type().is_dir(),
                        "expected 'platform' to be a directory"
                    );
                }

                #[test]
                fn read_dir_should_fail_for_nonexistent_path() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    assert_os_error(
                        fs.read_dir("nonexistent_dir").err().unwrap(),
                        &normalize_path(&fs.current_dir, "nonexistent_dir"),
                    );
                }

                #[cfg(unix)]
                #[test]
                fn read_dir_should_skip_symlinks() {
                    let dir = std::env::current_dir()
                        .unwrap()
                        .join("fs_test_read_dir_symlink.tmp");
                    let _test_dir = TestDir::new(dir.clone());
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    fs.create_dir_all("fs_test_read_dir_symlink.tmp").unwrap();
                    fs.write("fs_test_read_dir_symlink.tmp/a.txt", "aaa")
                        .unwrap();
                    std::os::unix::fs::symlink(dir.join("a.txt"), dir.join("link")).unwrap();

                    let names: Vec<String> = fs
                        .read_dir("fs_test_read_dir_symlink.tmp")
                        .unwrap()
                        .map(|e| e.file_name().to_string())
                        .collect();
                    assert_eq!(names, vec!["a.txt"]);
                }

                #[cfg(unix)]
                #[test]
                fn read_dir_should_skip_non_utf8_names() {
                    use std::os::unix::ffi::OsStrExt;

                    let dir = std::env::current_dir()
                        .unwrap()
                        .join("fs_test_read_dir_non_utf8.tmp");
                    let _test_dir = TestDir::new(dir.clone());
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    fs.create_dir_all("fs_test_read_dir_non_utf8.tmp").unwrap();
                    fs.write("fs_test_read_dir_non_utf8.tmp/a.txt", "aaa")
                        .unwrap();
                    std::fs::write(dir.join(std::ffi::OsStr::from_bytes(b"\xff.txt")), "bbb")
                        .unwrap();

                    let names: Vec<String> = fs
                        .read_dir("fs_test_read_dir_non_utf8.tmp")
                        .unwrap()
                        .map(|e| e.file_name().to_string())
                        .collect();
                    assert_eq!(names, vec!["a.txt"]);
                }
            }

            mod create_dir_all {
                use super::*;

                #[test]
                fn create_dir_all_should_create_directory_and_parents() {
                    let _test_dir = TestDir::new(
                        std::env::current_dir()
                            .unwrap()
                            .join("fs_test_create_dir_all.tmp"),
                    );
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    fs.create_dir_all("fs_test_create_dir_all.tmp/a/b").unwrap();

                    assert!(
                        std::env::current_dir()
                            .unwrap()
                            .join("fs_test_create_dir_all.tmp/a/b")
                            .is_dir()
                    );
                }

                #[test]
                fn create_dir_all_should_succeed_if_directory_already_exists() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    fs.create_dir_all("src").unwrap();
                    assert!(
                        fs.current_dir.join("src").is_dir(),
                        "src should still be a directory"
                    );
                }

                #[test]
                fn create_dir_all_should_fail_if_file_exists_at_path() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    assert_os_error(
                        fs.create_dir_all("Cargo.toml").err().unwrap(),
                        &normalize_path(&fs.current_dir, "Cargo.toml"),
                    );
                }
            }

            mod exists {
                use super::*;

                #[test]
                fn exists_should_return_true_for_existing_file() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    assert!(fs.exists("Cargo.toml").unwrap());
                }

                #[test]
                fn exists_should_return_true_for_existing_directory() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    assert!(fs.exists("src").unwrap());
                }

                #[test]
                fn exists_should_return_false_for_non_existing_file() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    assert!(!fs.exists("non_existing_file.tmp").unwrap());
                }

                #[test]
                fn exists_should_return_false_for_non_existing_directory() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    assert!(!fs.exists("non_existing_directory").unwrap());
                }

                #[test]
                fn exists_should_return_error_for_invalid_path() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    let result = fs.exists("\0");
                    assert!(result.is_err());
                }
            }

            mod metadata {
                use super::*;

                #[test]
                fn metadata_should_return_file_metadata() {
                    let _test_file = TestFile::new(
                        std::env::current_dir()
                            .unwrap()
                            .join("fs_test_metadata_file.tmp"),
                    );
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    fs.write("fs_test_metadata_file.tmp", "abc").unwrap();

                    let metadata = fs.metadata("fs_test_metadata_file.tmp").unwrap();

                    assert_eq!(metadata.file_type, FileType::File);
                    assert_eq!(metadata.len, 3);
                    assert!(metadata.modified <= SystemTime::now());
                    assert!(!metadata.readonly && !metadata.executable && !metadata.hidden);
                }

                #[test]
                fn metadata_should_return_dir_metadata() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    let metadata = fs.metadata("src").unwrap();
                    assert_eq!(metadata.file_type, FileType::Dir);
                    assert!(!metadata.executable);
                }

                #[cfg(unix)]
                #[test]
                fn metadata_should_return_mode_and_owner() {
                    use std::os::unix::fs::MetadataExt;

                    let path = std::env::current_dir()
                        .unwrap()
                        .join("fs_test_metadata_mode.tmp");
                    let _test_file = TestFile::new(path.clone());
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    fs.write("fs_test_metadata_mode.tmp", "").unwrap();
                    set_mode(&path, 0o4640);
                    let native = std::fs::metadata(&path).unwrap();

                    let metadata = fs.metadata("fs_test_metadata_mode.tmp").unwrap();

                    assert_eq!(metadata.mode, 0o4640);
                    assert_eq!((metadata.uid, metadata.gid), (native.uid(), native.gid()));
                }

                #[cfg(unix)]
                #[test]
                fn metadata_dot_name_should_be_hidden() {
                    let _test_file = TestFile::new(
                        std::env::current_dir()
                            .unwrap()
                            .join(".fs_test_metadata_hidden.tmp"),
                    );
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    fs.write(".fs_test_metadata_hidden.tmp", "").unwrap();

                    assert!(fs.metadata(".fs_test_metadata_hidden.tmp").unwrap().hidden);
                }

                #[cfg(windows)]
                #[test]
                fn metadata_executable_extension_should_be_executable() {
                    let _test_file = TestFile::new(
                        std::env::current_dir()
                            .unwrap()
                            .join("fs_test_metadata_executable.bat"),
                    );
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    fs.write("fs_test_metadata_executable.bat", "").unwrap();

                    assert!(
                        fs.metadata("fs_test_metadata_executable.bat")
                            .unwrap()
                            .executable
                    );
                }

                #[test]
                fn metadata_should_fail_for_nonexistent_path() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    assert_os_error(
                        fs.metadata("nonexistent.tmp").err().unwrap(),
                        &normalize_path(&fs.current_dir, "nonexistent.tmp"),
                    );
                }

                #[cfg(unix)]
                #[test]
                fn metadata_should_fail_for_unsupported_file_type() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    assert_eq!(
                        fs.metadata("/dev/null").err().unwrap().to_string(),
                        fs_error!(
                            "Unsupported file type, path is neither a file nor a directory: /dev/null"
                        )
                        .to_string()
                    );
                }
            }

            mod set_readonly {
                use super::*;

                #[cfg(unix)]
                #[test]
                fn set_readonly_should_map_to_write_bits() {
                    let path = std::env::current_dir()
                        .unwrap()
                        .join("fs_test_set_readonly.tmp");
                    let _test_file = TestFile::new(path.clone());
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    fs.write("fs_test_set_readonly.tmp", "").unwrap();

                    for (initial, readonly, expected) in [
                        (0o644, true, 0o444),
                        (0o755, true, 0o555),
                        (0o444, false, 0o644),
                        (0o664, false, 0o664),
                    ] {
                        set_mode(&path, initial);
                        fs.set_readonly("fs_test_set_readonly.tmp", readonly)
                            .unwrap();
                        assert_eq!(
                            mode(&path),
                            expected,
                            "initial: {initial:o}, readonly: {readonly}"
                        );
                    }
                }

                #[cfg(windows)]
                #[test]
                fn set_readonly_unchanged_should_succeed() {
                    let _test_file = TestFile::new(
                        std::env::current_dir()
                            .unwrap()
                            .join("fs_test_set_readonly_unchanged.tmp"),
                    );
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    fs.write("fs_test_set_readonly_unchanged.tmp", "").unwrap();

                    fs.set_readonly("fs_test_set_readonly_unchanged.tmp", false)
                        .unwrap();

                    assert!(
                        !fs.metadata("fs_test_set_readonly_unchanged.tmp")
                            .unwrap()
                            .readonly
                    );
                }

                #[cfg(windows)]
                #[test]
                fn set_readonly_false_without_other_attributes_should_set_normal() {
                    use super::windows::FILE_ATTRIBUTE_NORMAL;
                    use super::windows::FILE_ATTRIBUTE_READONLY;
                    use std::os::windows::ffi::OsStrExt;
                    use std::os::windows::fs::MetadataExt;

                    #[link(name = "kernel32")]
                    unsafe extern "system" {
                        fn SetFileAttributesW(file_name: *const u16, file_attributes: u32) -> i32;
                    }

                    let path = std::env::current_dir()
                        .unwrap()
                        .join("fs_test_set_readonly_normal.tmp");
                    let _test_file = TestFile::new(path.clone());
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    fs.write("fs_test_set_readonly_normal.tmp", "").unwrap();
                    let file_name: Vec<u16> = path
                        .as_os_str()
                        .encode_wide()
                        .chain(std::iter::once(0))
                        .collect();

                    // WHY: new files get FILE_ATTRIBUTE_ARCHIVE, it must be cleared
                    // so that no other attribute remains once readonly is cleared.
                    // SAFETY: `file_name` is a NUL-terminated UTF-16 string alive for
                    // the duration of the call.
                    assert_ne!(
                        unsafe { SetFileAttributesW(file_name.as_ptr(), FILE_ATTRIBUTE_READONLY) },
                        0
                    );

                    fs.set_readonly("fs_test_set_readonly_normal.tmp", false)
                        .unwrap();

                    assert_eq!(
                        std::fs::metadata(&path).unwrap().file_attributes(),
                        FILE_ATTRIBUTE_NORMAL
                    );
                }

                #[cfg(windows)]
                #[test]
                fn set_readonly_should_set_readonly_attribute() {
                    let _test_file = TestFile::new(
                        std::env::current_dir()
                            .unwrap()
                            .join("fs_test_set_readonly.tmp"),
                    );
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    fs.write("fs_test_set_readonly.tmp", "").unwrap();

                    for readonly in [true, false] {
                        fs.set_readonly("fs_test_set_readonly.tmp", readonly)
                            .unwrap();
                        assert_eq!(
                            fs.metadata("fs_test_set_readonly.tmp").unwrap().readonly,
                            readonly
                        );
                    }
                }
            }

            #[cfg(unix)]
            mod set_executable {
                use super::*;

                #[test]
                fn set_executable_file_should_map_to_execute_bits() {
                    let path = std::env::current_dir()
                        .unwrap()
                        .join("fs_test_set_executable_file.tmp");
                    let _test_file = TestFile::new(path.clone());
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    fs.write("fs_test_set_executable_file.tmp", "").unwrap();

                    for (initial, executable, expected) in [
                        (0o644, true, 0o755),
                        (0o600, true, 0o700),
                        (0o000, true, 0o100),
                        (0o744, true, 0o744),
                        (0o755, false, 0o644),
                    ] {
                        set_mode(&path, initial);
                        fs.set_executable("fs_test_set_executable_file.tmp", executable)
                            .unwrap();
                        assert_eq!(
                            mode(&path),
                            expected,
                            "initial: {initial:o}, executable: {executable}"
                        );
                    }
                }

                #[test]
                fn set_executable_dir_should_fail() {
                    let _test_dir = TestDir::new(
                        std::env::current_dir()
                            .unwrap()
                            .join("fs_test_set_executable_dir.tmp"),
                    );
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    fs.create_dir_all("fs_test_set_executable_dir.tmp").unwrap();

                    assert_eq!(
                        fs.set_executable("fs_test_set_executable_dir.tmp", true)
                            .err()
                            .unwrap()
                            .to_string(),
                        fs_error!(
                            "Executable attribute cannot be set, path is a directory: {}",
                            normalize_path(&fs.current_dir, "fs_test_set_executable_dir.tmp")
                                .display()
                        )
                        .to_string()
                    );
                }
            }

            #[cfg(windows)]
            mod set_hidden {
                use super::*;

                #[test]
                fn set_hidden_should_set_hidden_attribute() {
                    let _test_file = TestFile::new(
                        std::env::current_dir()
                            .unwrap()
                            .join("fs_test_set_hidden.tmp"),
                    );
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    fs.write("fs_test_set_hidden.tmp", "").unwrap();

                    for hidden in [true, false] {
                        fs.set_hidden("fs_test_set_hidden.tmp", hidden).unwrap();
                        assert_eq!(
                            fs.metadata("fs_test_set_hidden.tmp").unwrap().hidden,
                            hidden
                        );
                    }
                }
            }
            mod set_modified {
                use super::*;

                #[test]
                fn set_modified_should_set_modified_time() {
                    let _test_dir = TestDir::new(
                        std::env::current_dir()
                            .unwrap()
                            .join("fs_test_set_modified.tmp"),
                    );
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    fs.create_dir_all("fs_test_set_modified.tmp").unwrap();
                    fs.write("fs_test_set_modified.tmp/a.txt", "").unwrap();
                    let modified = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000);

                    for path in ["fs_test_set_modified.tmp/a.txt", "fs_test_set_modified.tmp"] {
                        fs.set_modified(path, modified).unwrap();
                        assert_eq!(
                            fs.metadata(path).unwrap().modified,
                            modified,
                            "path: {path}"
                        );
                    }
                }

                #[test]
                fn set_modified_should_fail_for_nonexistent_path() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    assert_os_error(
                        fs.set_modified("nonexistent.tmp", SystemTime::UNIX_EPOCH)
                            .err()
                            .unwrap(),
                        &normalize_path(&fs.current_dir, "nonexistent.tmp"),
                    );
                }

                #[cfg(unix)]
                #[test]
                fn set_modified_should_fail_for_unsupported_file_type() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    assert_eq!(
                        fs.set_modified("/dev/null", SystemTime::UNIX_EPOCH)
                            .err()
                            .unwrap()
                            .to_string(),
                        fs_error!(
                            "Unsupported file type, path is neither a file nor a directory: /dev/null"
                        )
                        .to_string()
                    );
                }
            }

            #[cfg(unix)]
            mod set_mode {
                use super::*;

                #[test]
                fn set_mode_should_set_permission_bits() {
                    let path = std::env::current_dir()
                        .unwrap()
                        .join("fs_test_set_mode.tmp");
                    let _test_file = TestFile::new(path.clone());
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    fs.write("fs_test_set_mode.tmp", "").unwrap();

                    for (requested, expected) in
                        [(0o600, 0o600), (0o4750, 0o4750), (0o100644, 0o644)]
                    {
                        fs.set_mode("fs_test_set_mode.tmp", requested).unwrap();
                        assert_eq!(mode(&path), expected, "requested: {requested:o}");
                    }
                }

                #[test]
                fn set_mode_should_fail_for_nonexistent_path() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    assert_os_error(
                        fs.set_mode("nonexistent.tmp", 0o600).err().unwrap(),
                        &normalize_path(&fs.current_dir, "nonexistent.tmp"),
                    );
                }

                #[test]
                fn set_mode_should_fail_for_unsupported_file_type() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    assert_eq!(
                        fs.set_mode("/dev/null", 0o600)
                            .err()
                            .unwrap()
                            .to_string(),
                        fs_error!(
                            "Unsupported file type, path is neither a file nor a directory: /dev/null"
                        )
                        .to_string()
                    );
                }
            }
        }

        mod set_attribute {
            use super::*;

            #[test]
            fn set_attribute_should_fail_for_nonexistent_path() {
                let fs = FsImpl::new(std::env::current_dir().unwrap());
                assert_os_error(
                    fs.set_attribute(Path::new("nonexistent.tmp"), Attribute::Readonly, true)
                        .err()
                        .unwrap(),
                    &normalize_path(&fs.current_dir, "nonexistent.tmp"),
                );
            }

            #[cfg(unix)]
            #[test]
            fn set_attribute_should_fail_for_unsupported_file_type() {
                let fs = FsImpl::new(std::env::current_dir().unwrap());
                assert_eq!(
                    fs.set_attribute(Path::new("/dev/null"), Attribute::Readonly, true)
                        .err()
                        .unwrap()
                        .to_string(),
                    fs_error!(
                        "Unsupported file type, path is neither a file nor a directory: /dev/null"
                    )
                    .to_string()
                );
            }
        }
    }

    mod file_handle {
        use super::*;

        mod drop {
            use super::*;

            #[test]
            fn drop_should_not_panic_when_path_not_in_handles() {
                let handle = FileHandle {
                    handles: Arc::new(Mutex::new(HashMap::new())),
                    path: PathBuf::from("Cargo.toml"),
                    file: std::fs::File::open("Cargo.toml").unwrap(),
                };
                drop(handle);
            }
        }
    }

    mod file_read_handle {
        use super::*;

        #[test]
        fn file_read_handle_should_be_usable_as_struct_field() {
            let fs = FsImpl::new(std::env::current_dir().unwrap());

            struct S<Handle: Read + Seek> {
                file: Handle,
            }

            let mut s = S {
                file: fs.open_read("Cargo.toml").unwrap(),
            };

            let mut buffer = vec![0_u8; 1];
            let read = s.file.read(&mut buffer).unwrap();
            assert_eq!(read, buffer.len());
        }
    }

    mod lock_handles {
        use super::*;

        #[test]
        fn lock_handles_should_recover_from_poison_and_return_guard() {
            let fs = FsImpl::new(std::env::current_dir().unwrap());
            let poisoned_handles = fs.handles.clone();
            let _ = std::panic::catch_unwind(|| {
                std::thread::spawn(move || {
                    let _guard = poisoned_handles.lock().unwrap();
                    panic!("Poison the mutex");
                })
                .join()
                .ok()
            });
            fs.open_read("Cargo.toml").unwrap();
            fs.open_write("Cargo.toml").unwrap();
        }
    }

    mod fs_error {
        use crate::ErrorKind;

        #[test]
        fn fs_error_should_format_arguments_into_fs_error() {
            let error = fs_error!("{} {}", "some", "error");
            assert_eq!(error.kind(), ErrorKind::Fs);
            assert_eq!(error.description(), "some error");
        }
    }

    mod fs_error_at {
        use super::*;

        #[test]
        fn fs_error_at_should_map_into_fs_error_for_path_with_source() {
            use std::error::Error as _;

            let path = Path::new("/some/file.txt");
            let result: std::result::Result<(), std::io::Error> = Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "fs error",
            ));

            let error = result.map_err(fs_error_at(path)).unwrap_err();

            assert_eq!(
                error.to_string(),
                fs_error!("fs error: {}", path.display()).to_string()
            );
            assert_eq!(
                error
                    .source()
                    .and_then(|source| source.downcast_ref::<std::io::Error>())
                    .map(std::io::Error::kind),
                Some(std::io::ErrorKind::NotFound)
            );
        }
    }

    mod file_type {
        use super::*;

        mod is_dir {
            use super::*;

            #[test]
            fn is_dir_should_return_true_for_dir() {
                assert!(FileType::Dir.is_dir());
                assert!(!FileType::File.is_dir());
            }
        }

        mod is_file {
            use super::*;

            #[test]
            fn is_file_should_return_true_for_file() {
                assert!(FileType::File.is_file());
                assert!(!FileType::Dir.is_file());
            }
        }
    }

    mod dir_entry {
        use super::*;

        #[test]
        fn dir_entry_should_expose_path_file_name_and_file_type() {
            let entry = DirEntry {
                path: PathBuf::from("/some/dir/file.txt"),
                file_type: FileType::File,
            };
            assert_eq!(entry.path(), Path::new("/some/dir/file.txt"));
            assert_eq!(entry.file_name(), "file.txt");
            assert_eq!(entry.file_type(), FileType::File);
        }
    }

    mod attributes {
        use super::*;

        mod set {
            use super::*;

            #[test]
            fn set_should_change_only_selected_flag() {
                for (attribute, expected) in [
                    (
                        Attribute::Readonly,
                        Attributes {
                            readonly: true,
                            ..Attributes::default()
                        },
                    ),
                    #[cfg(unix)]
                    (
                        Attribute::Executable,
                        Attributes {
                            executable: true,
                            ..Attributes::default()
                        },
                    ),
                    #[cfg(windows)]
                    (
                        Attribute::Hidden,
                        Attributes {
                            hidden: true,
                            ..Attributes::default()
                        },
                    ),
                ] {
                    let mut attributes = Attributes::default();
                    attributes.set(attribute, true);
                    assert_eq!(attributes, expected, "attribute: {attribute:?}");
                }
            }
        }
    }

    mod platform_attributes {
        use super::*;

        #[cfg(unix)]
        #[test]
        fn platform_attributes_should_derive_hidden_and_restrict_executable_to_files() {
            let all = Attributes {
                readonly: true,
                executable: true,
                hidden: true,
            };

            for (path, file_type, stored, expected) in [
                (
                    "/a/file",
                    FileType::File,
                    all,
                    Attributes {
                        readonly: true,
                        executable: true,
                        ..Attributes::default()
                    },
                ),
                (
                    "/a/.file",
                    FileType::File,
                    Attributes::default(),
                    Attributes {
                        hidden: true,
                        ..Attributes::default()
                    },
                ),
                (
                    "/a/dir",
                    FileType::Dir,
                    all,
                    Attributes {
                        readonly: true,
                        ..Attributes::default()
                    },
                ),
                (
                    "/",
                    FileType::Dir,
                    Attributes::default(),
                    Attributes::default(),
                ),
            ] {
                assert_eq!(
                    platform_attributes(Path::new(path), file_type, stored),
                    expected,
                    "path: {path}, stored: {stored:?}"
                );
            }
        }

        #[cfg(windows)]
        #[test]
        fn platform_attributes_should_derive_executable_from_file_extension() {
            let all = Attributes {
                readonly: true,
                executable: true,
                hidden: true,
            };

            for (path, file_type, stored, expected) in [
                (
                    "C:\\a\\file",
                    FileType::File,
                    all,
                    Attributes {
                        readonly: true,
                        hidden: true,
                        ..Attributes::default()
                    },
                ),
                (
                    "C:\\a\\.file",
                    FileType::File,
                    Attributes::default(),
                    Attributes::default(),
                ),
                (
                    "C:\\a\\run.exe",
                    FileType::File,
                    Attributes::default(),
                    Attributes {
                        executable: true,
                        ..Attributes::default()
                    },
                ),
                (
                    "C:\\a\\run.CMD",
                    FileType::File,
                    Attributes::default(),
                    Attributes {
                        executable: true,
                        ..Attributes::default()
                    },
                ),
                (
                    "C:\\a\\run.com",
                    FileType::File,
                    Attributes::default(),
                    Attributes {
                        executable: true,
                        ..Attributes::default()
                    },
                ),
                (
                    "C:\\a\\run.bat",
                    FileType::File,
                    Attributes::default(),
                    Attributes {
                        executable: true,
                        ..Attributes::default()
                    },
                ),
                (
                    "C:\\a\\dir.exe",
                    FileType::Dir,
                    Attributes::default(),
                    Attributes::default(),
                ),
            ] {
                assert_eq!(
                    platform_attributes(Path::new(path), file_type, stored),
                    expected,
                    "path: {path}, stored: {stored:?}"
                );
            }
        }
    }
}
