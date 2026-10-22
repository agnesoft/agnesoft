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
pub(crate) use unix::native_symlink_target;
#[cfg(unix)]
use unix::{
    handle_file_id, metadata_file_id, native_attributes, native_handle_metadata, native_metadata,
    native_remove_dir_all, native_remove_symlink, native_symlink, native_symlink_metadata,
    open_read_error, opened_file, opened_id, opened_len, path_file_id, set_native_attributes,
};
#[cfg(windows)]
pub(crate) use windows::native_symlink_target;
#[cfg(windows)]
use windows::{
    handle_file_id, metadata_file_id, native_attributes, native_handle_metadata, native_metadata,
    native_remove_dir_all, native_remove_symlink, native_symlink, native_symlink_metadata,
    open_read_error, opened_file, opened_id, opened_len, path_file_id, set_native_attributes,
};

/// A filesystem interface with runtime borrow checking.
///
/// Enforces one-writer-or-many-readers per file: a file can be opened for
/// reading any number of times, but opening it for writing requires that no
/// other read or write handles exist for that file. Violations return an
/// error rather than panicking. Handles are RAII — dropping a handle
/// releases the borrow automatically. Borrows are tracked per file, not per
/// path: a file is borrowed once whether it is opened through a symlink, by
/// another of its hard links or on a different mount. The symlink itself is
/// not borrowed.
///
/// Symlinks are followed unless documented otherwise. Paths passed to the
/// methods are normalized lexically first: a `..` component removes the
/// preceding component even if it is a symlink. A `..` in a symlink target
/// refers to the parent of the directory the link resolves into. An empty
/// path does not exist, as with [`std::fs`].
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
    type ReadHandle: ReadFile;

    /// A read-write file handle.
    type WriteHandle: WriteFile;

    /// An iterator over the entries of a directory.
    type Dir: Iterator<Item = Result<DirEntry>>;

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
    /// - The file cannot be opened for both reading and writing, the handle
    ///   allows both. Use [`write`](Fs::write) for a file that cannot be
    ///   read.
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
    /// The directory is read lazily as the iterator advances, so each
    /// entry is a [`Result`]. Regular files, directories and symlinks are
    /// listed, symlinks are not followed and are reported as
    /// [`FileType::Symlink`]. Other special files and entries whose names
    /// are not valid UTF-8 are skipped. The `.` and `..` entries are not
    /// included. The order of entries is not guaranteed. The `path` itself
    /// is followed if it is a symlink.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The path does not exist.
    /// - The path is not a directory.
    /// - An I/O error occurs while opening the directory.
    ///
    /// The iterator yields an error if an I/O error occurs while reading
    /// an entry.
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
    ///     .collect::<agplatform::Result<_>>()
    ///     .unwrap();
    /// assert!(!entries.is_empty());
    /// ```
    fn read_dir<P: AsRef<Path>>(&self, path: P) -> Result<Self::Dir>;

    /// Creates a directory, like [`std::fs::create_dir`]. Symlinks in the
    /// parent components are followed.
    ///
    /// The creation is atomic: if the directory is created by several
    /// callers at once, exactly one of them succeeds. This makes it usable
    /// e.g. as a lock between processes.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Anything, including a dangling symlink, already exists at `path`.
    /// - The parent directory of `path` does not exist.
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
    /// fs.create_dir("/lock").unwrap();
    /// assert!(fs.create_dir("/lock").is_err());
    /// ```
    fn create_dir<P: AsRef<Path>>(&self, path: P) -> Result;

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
    /// assert!(fs.read_dir("/a/b/c").unwrap().next().is_none());
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

    /// Returns the [`Metadata`] of a file, directory or symlink. Unlike
    /// [`metadata`](Fs::metadata) a symlink is not followed: its own
    /// metadata is returned with [`FileType::Symlink`]. Symlinks in the
    /// parent components are followed.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The path does not exist.
    /// - The path is neither a regular file, a directory nor a symlink.
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
    /// fs.create_symlink("/link", "/data.txt").unwrap();
    /// assert!(fs.symlink_metadata("/link").unwrap().file_type.is_symlink());
    /// ```
    fn symlink_metadata<P: AsRef<Path>>(&self, path: P) -> Result<Metadata>;

    /// Creates a symlink at `path` pointing to `target`.
    ///
    /// The `target` is stored as given, except that on Windows `/` is stored
    /// as `\`, which Windows requires to resolve a relative symlink. A
    /// relative `target` is resolved against the parent directory of `path`
    /// when the symlink is followed. The `target` must exist: on Windows its
    /// type selects a file or a directory symlink, which is checked on every
    /// platform for consistency.
    ///
    /// On Windows creating symlinks requires Developer Mode or
    /// administrator privileges.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Anything, including a dangling symlink, already exists at `path`.
    /// - The parent directory of `path` does not exist.
    /// - The `target` is empty, does not exist or is neither a regular file
    ///   nor a directory.
    /// - The `target` ends with a separator but is not a directory.
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
    /// fs.create_dir_all("/dir").unwrap();
    /// fs.write("/dir/data.txt", "hello").unwrap();
    /// fs.create_symlink("/dir/link", "data.txt").unwrap();
    /// assert_eq!(fs.read_to_string("/dir/link").unwrap(), "hello");
    /// ```
    fn create_symlink<P: AsRef<Path>, T: AsRef<Path>>(&self, path: P, target: T) -> Result;

    /// Returns the target of a symlink as stored, without resolving it: on
    /// Unix as given to [`create_symlink`](Fs::create_symlink). On Windows
    /// `/` is returned as `\` and an absolute target in its normalized form,
    /// e.g. `C:/x` as `C:\x`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The path does not exist.
    /// - The path is not a symlink.
    /// - Permission denied.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform};
    /// use std::path::Path;
    ///
    /// let platform = agplatform::platform();
    /// let fs = platform.fs();
    /// # let fs = agplatform::test_platform::test_fs::TestFs::new();
    /// fs.write("/data.txt", "hello").unwrap();
    /// fs.create_symlink("/link", "data.txt").unwrap();
    /// assert_eq!(fs.read_link("/link").unwrap(), Path::new("data.txt"));
    /// ```
    fn read_link<P: AsRef<Path>>(&self, path: P) -> Result<PathBuf>;

    /// Returns the absolute path of `path` with all symlinks resolved, like
    /// [`std::fs::canonicalize`]. On Windows `FsImpl` returns a verbatim
    /// (`\\?\`) path like std does.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The path or a symlink in it does not resolve to an existing entry.
    /// - A component of the path is not a directory.
    /// - Permission denied.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform};
    /// use std::path::Path;
    ///
    /// let platform = agplatform::platform();
    /// let fs = platform.fs();
    /// # let fs = agplatform::test_platform::test_fs::TestFs::new();
    /// fs.create_dir_all("/dir").unwrap();
    /// fs.write("/dir/data.txt", "hello").unwrap();
    /// fs.create_symlink("/link", "/dir").unwrap();
    /// let path = fs.canonicalize("/link/data.txt").unwrap();
    /// assert_eq!(fs.read_to_string(path).unwrap(), "hello");
    /// ```
    fn canonicalize<P: AsRef<Path>>(&self, path: P) -> Result<PathBuf>;

    /// Creates a hard link at `path` to the file `target`: another name for
    /// the same file, sharing its content, metadata and borrows.
    ///
    /// Unlike in [`create_symlink`](Fs::create_symlink) a relative `target`
    /// is resolved against the current directory like any other path, and
    /// symlinks in `target` are followed, so the link is always to the file
    /// itself. Hard links can only be created on the filesystem (volume) of
    /// the `target`.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Anything, including a dangling symlink, already exists at `path`.
    /// - The parent directory of `path` does not exist.
    /// - The `target` does not exist or is a directory.
    /// - The `target` is on a different filesystem than `path`.
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
    /// fs.create_hard_link("/other.txt", "/data.txt").unwrap();
    /// fs.write("/other.txt", "world").unwrap();
    /// assert_eq!(fs.read_to_string("/data.txt").unwrap(), "world");
    /// ```
    fn create_hard_link<P: AsRef<Path>, T: AsRef<Path>>(&self, path: P, target: T) -> Result;

    /// Copies a file, an empty directory or a symlink `source` to the new
    /// path `target`. A symlink is not followed: a symlink pointing where
    /// `source` points is created at `target` as by
    /// [`create_symlink`](Fs::create_symlink), so a relative symlink is
    /// resolved against the parent directory of `target`. Symlinks in the
    /// parent components are followed.
    ///
    /// A file is copied with its content like by [`std::fs::copy`]: a file
    /// at `target` is overwritten, a symlink at `target` is followed. The
    /// copy is independent also if the `source` has other hard links. A
    /// directory or a symlink is copied only to a path that does not exist
    /// yet. A file or a directory is copied with its permissions: the
    /// [`mode`](Metadata::mode) on Unix, the
    /// [`readonly`](Metadata::readonly) attribute on Windows. Other
    /// metadata, e.g. the modification time or the owner, is not copied.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The `source` does not exist.
    /// - The `source` is a directory that is not empty.
    /// - The `source` is a file currently opened for writing.
    /// - The `source` is a file and `target` is a directory or a file
    ///   currently opened for reading or writing, e.g. the `source` itself.
    /// - The `source` is a symlink that would not resolve at `target`.
    /// - The `source` is neither a regular file, a directory nor a symlink.
    /// - The `source` is a directory or a symlink and anything, including a
    ///   dangling symlink, already exists at `target`.
    /// - The parent directory of `target` does not exist.
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
    /// fs.copy("/data.txt", "/copy.txt").unwrap();
    /// fs.write("/copy.txt", "world").unwrap();
    /// assert_eq!(fs.read_to_string("/data.txt").unwrap(), "hello");
    /// ```
    fn copy<S: AsRef<Path>, T: AsRef<Path>>(&self, source: S, target: T) -> Result;

    /// Copies the directory `source` with all of its content to the new
    /// path `target`. The `source` itself is followed if it is a symlink,
    /// the entries in it are copied as by [`copy`](Fs::copy): symlinks are
    /// not followed but copied as symlinks. Entries skipped by
    /// [`read_dir`](Fs::read_dir) are not copied.
    ///
    /// The whole tree is listed before anything is copied, so `target` may
    /// be inside `source`. Directories get their permissions once their
    /// content is copied, so read-only directories are copied too. If an
    /// error occurs, what was copied until then is not removed.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The `source` does not exist or is not a directory.
    /// - Anything, including a dangling symlink, already exists at `target`.
    /// - The parent directory of `target` does not exist.
    /// - An entry cannot be copied, see [`copy`](Fs::copy), e.g. a file is
    ///   currently opened for writing or a symlink would not resolve in
    ///   the copy.
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
    /// fs.create_dir_all("/dir/sub").unwrap();
    /// fs.write("/dir/sub/data.txt", "hello").unwrap();
    /// fs.copy_dir_all("/dir", "/copy").unwrap();
    /// assert_eq!(fs.read_to_string("/copy/sub/data.txt").unwrap(), "hello");
    /// ```
    fn copy_dir_all<S: AsRef<Path>, T: AsRef<Path>>(&self, source: S, target: T) -> Result;

    /// Removes a file, a symlink or an empty directory. A symlink is not
    /// followed: the symlink itself is removed, not its target. Symlinks in
    /// the parent components are followed. A file with other hard links
    /// remains under its other names.
    ///
    /// A read-only file is removed as any other, on Windows too.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The path does not exist.
    /// - The path is a directory that is not empty or a root.
    /// - The path is a file currently opened for reading or writing, by any
    ///   of its names.
    /// - The path is neither a regular file, a directory nor a symlink.
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
    /// fs.remove("/data.txt").unwrap();
    /// assert!(!fs.exists("/data.txt").unwrap());
    /// ```
    fn remove<P: AsRef<Path>>(&self, path: P) -> Result;

    /// Removes a directory with all of its content, like
    /// [`std::fs::remove_dir_all`]. Symlinks are not followed: a symlink at
    /// `path` is removed itself, as are the symlinks in the directory,
    /// their targets are kept. Symlinks in the parent components are
    /// followed. All entries are removed, also those skipped by
    /// [`read_dir`](Fs::read_dir).
    ///
    /// Nothing is removed if a file in the directory is currently opened
    /// for reading or writing, by any of its names. Otherwise `FsImpl`
    /// removes the directory with [`std::fs::remove_dir_all`], which
    /// cannot be redirected outside of it by a symlink swapped in during
    /// the removal. If an error occurs, what was removed until then is not
    /// restored.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The path does not exist.
    /// - The path is a file or a root.
    /// - A file in the directory is currently opened for reading or
    ///   writing.
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
    /// fs.create_dir_all("/dir/sub").unwrap();
    /// fs.write("/dir/sub/data.txt", "hello").unwrap();
    /// fs.remove_dir_all("/dir").unwrap();
    /// assert!(!fs.exists("/dir").unwrap());
    /// ```
    fn remove_dir_all<P: AsRef<Path>>(&self, path: P) -> Result;

    /// Renames (moves) a file, a directory or a symlink `source` to
    /// `target`, like [`std::fs::rename`]. A symlink is not followed: the
    /// symlink itself is renamed, its relative target then resolves
    /// against its new parent directory. Symlinks in the parent components
    /// are followed. Renaming a path to itself does nothing. Renaming a hard
    /// link onto another one of the same file removes `source`, on Unix
    /// too where [`std::fs::rename`] does nothing.
    ///
    /// An existing `target` is replaced atomically as by
    /// [`std::fs::rename`]: a directory `source` replaces only an empty
    /// directory, anything else replaces anything but a directory. A
    /// symlink at `target` is replaced, not followed. On Windows this
    /// requires Windows 10 1607 or later and a filesystem supporting
    /// `FileRenameInfoEx`, see [`std::fs::rename`].
    ///
    /// A borrowed `source` remains borrowed under its new name.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The `source` does not exist.
    /// - The `source` is a directory and `target` is inside it.
    /// - The `source` is a directory and `target` is not an empty directory.
    /// - The `source` is not a directory and `target` is a directory.
    /// - The `target` is a file currently opened for reading or writing,
    ///   which would be replaced.
    /// - The parent directory of `target` does not exist.
    /// - The `source` and `target` are on different filesystems.
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
    /// fs.write("/data.txt.tmp", "hello").unwrap();
    /// fs.rename("/data.txt.tmp", "/data.txt").unwrap();
    /// assert_eq!(fs.read_to_string("/data.txt").unwrap(), "hello");
    /// ```
    fn rename<S: AsRef<Path>, T: AsRef<Path>>(&self, source: S, target: T) -> Result;

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

/// An open file that can be read, see [`Fs::open_read`].
pub trait ReadFile: Read + Seek {
    /// Returns the metadata of the open file, like
    /// [`std::fs::File::metadata`].
    ///
    /// # Errors
    ///
    /// Returns an error if the metadata cannot be queried.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform, ReadFile};
    ///
    /// let platform = agplatform::platform();
    /// let fs = platform.fs();
    /// # let fs = agplatform::test_platform::test_fs::TestFs::new();
    /// fs.write("/data.txt", "hello").unwrap();
    /// let file = fs.open_read("/data.txt").unwrap();
    /// assert_eq!(file.metadata().unwrap().len, 5);
    /// ```
    fn metadata(&self) -> Result<Metadata>;

    /// Locks the file for reading, waiting until no other handle holds an
    /// exclusive lock on it, like [`std::fs::File::lock_shared`]. Any
    /// number of handles may hold a shared lock at once. The lock is
    /// released by [`unlock`](Self::unlock) or when the handle is dropped.
    ///
    /// Within one [`Fs`] the borrow rules already exclude conflicting
    /// handles, so a lock only waits for or excludes other processes and
    /// other `Fs` instances. On Unix the lock is advisory: it does not stop
    /// reads and writes of those not locking the file.
    ///
    /// # Errors
    ///
    /// Returns an error if locking is not supported, e.g. on some network
    /// filesystems.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform, ReadFile};
    ///
    /// let platform = agplatform::platform();
    /// let fs = platform.fs();
    /// # let fs = agplatform::test_platform::test_fs::TestFs::new();
    /// fs.write("/data.txt", "hello").unwrap();
    /// let file = fs.open_read("/data.txt").unwrap();
    /// file.lock_shared().unwrap();
    /// ```
    fn lock_shared(&self) -> Result;

    /// Locks the file for reading like [`lock_shared`](Self::lock_shared)
    /// if possible right away, like [`std::fs::File::try_lock_shared`].
    /// Returns `false` if another handle holds an exclusive lock on it.
    ///
    /// # Errors
    ///
    /// Returns an error if locking is not supported, e.g. on some network
    /// filesystems.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform, ReadFile};
    ///
    /// let platform = agplatform::platform();
    /// let fs = platform.fs();
    /// # let fs = agplatform::test_platform::test_fs::TestFs::new();
    /// fs.write("/data.txt", "hello").unwrap();
    /// let file = fs.open_read("/data.txt").unwrap();
    /// assert!(file.try_lock_shared().unwrap());
    /// ```
    fn try_lock_shared(&self) -> Result<bool>;

    /// Releases the lock held by this handle, if any, like
    /// [`std::fs::File::unlock`].
    ///
    /// # Errors
    ///
    /// Returns an error if the lock cannot be released.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform, ReadFile};
    ///
    /// let platform = agplatform::platform();
    /// let fs = platform.fs();
    /// # let fs = agplatform::test_platform::test_fs::TestFs::new();
    /// fs.write("/data.txt", "hello").unwrap();
    /// let file = fs.open_read("/data.txt").unwrap();
    /// file.lock_shared().unwrap();
    /// file.unlock().unwrap();
    /// ```
    fn unlock(&self) -> Result;
}

/// An open file that can be written, see [`Fs::open_write`].
pub trait WriteFile: ReadFile + Write {
    /// Truncates or extends the open file to `len` bytes, like
    /// [`std::fs::File::set_len`]. An extension is filled with zeros. The
    /// cursor is not moved.
    ///
    /// # Errors
    ///
    /// Returns an error if the length cannot be set, e.g. the file is in
    /// append mode on Windows.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform, ReadFile, WriteFile};
    ///
    /// let platform = agplatform::platform();
    /// let fs = platform.fs();
    /// # let fs = agplatform::test_platform::test_fs::TestFs::new();
    /// fs.write("/data.txt", "hello").unwrap();
    /// let file = fs.open_write("/data.txt").unwrap();
    /// file.set_len(2).unwrap();
    /// assert_eq!(file.metadata().unwrap().len, 2);
    /// ```
    fn set_len(&self, len: u64) -> Result;

    /// Writes the content and the metadata of the open file to the disk,
    /// like [`std::fs::File::sync_all`].
    ///
    /// # Errors
    ///
    /// Returns an error if the data cannot be written to the disk.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform, WriteFile};
    /// use std::io::Write;
    ///
    /// let platform = agplatform::platform();
    /// let fs = platform.fs();
    /// # let fs = agplatform::test_platform::test_fs::TestFs::new();
    /// let mut file = fs.open_write("/data.txt").unwrap();
    /// file.write_all(b"hello").unwrap();
    /// file.sync_all().unwrap();
    /// ```
    fn sync_all(&self) -> Result;

    /// Writes the content of the open file to the disk, like
    /// [`std::fs::File::sync_data`]. Metadata is written only if needed to
    /// read the content back, e.g. a changed length.
    ///
    /// # Errors
    ///
    /// Returns an error if the data cannot be written to the disk.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform, WriteFile};
    /// use std::io::Write;
    ///
    /// let platform = agplatform::platform();
    /// let fs = platform.fs();
    /// # let fs = agplatform::test_platform::test_fs::TestFs::new();
    /// let mut file = fs.open_write("/data.txt").unwrap();
    /// file.write_all(b"hello").unwrap();
    /// file.sync_data().unwrap();
    /// ```
    fn sync_data(&self) -> Result;

    /// Locks the file exclusively, waiting until no other handle holds a
    /// lock on it, like [`std::fs::File::lock`]. The lock is released by
    /// [`unlock`](ReadFile::unlock) or when the handle is dropped.
    ///
    /// Within one [`Fs`] the borrow rules already exclude conflicting
    /// handles, so a lock only waits for or excludes other processes and
    /// other `Fs` instances. On Unix the lock is advisory: it does not stop
    /// reads and writes of those not locking the file. On
    /// Windows other handles cannot read or write the locked file.
    ///
    /// # Errors
    ///
    /// Returns an error if locking is not supported, e.g. on some network
    /// filesystems, or on Windows if the file is opened for appending only.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform, ReadFile, WriteFile};
    ///
    /// let platform = agplatform::platform();
    /// let fs = platform.fs();
    /// # let fs = agplatform::test_platform::test_fs::TestFs::new();
    /// let file = fs.open_write("/data.txt").unwrap();
    /// file.lock().unwrap();
    /// file.unlock().unwrap();
    /// ```
    fn lock(&self) -> Result;

    /// Locks the file exclusively like [`lock`](Self::lock) if possible
    /// right away, like [`std::fs::File::try_lock`]. Returns `false` if
    /// another handle holds a lock on it.
    ///
    /// # Errors
    ///
    /// Returns an error if locking is not supported, e.g. on some network
    /// filesystems, or on Windows if the file is opened for appending only.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform, WriteFile};
    ///
    /// let platform = agplatform::platform();
    /// let fs = platform.fs();
    /// # let fs = agplatform::test_platform::test_fs::TestFs::new();
    /// let file = fs.open_write("/data.txt").unwrap();
    /// assert!(file.try_lock().unwrap());
    /// ```
    fn try_lock(&self) -> Result<bool>;
}

/// Options for opening a file in write mode.
///
/// Constructed via a builder pattern. The file is opened with both read
/// and write access unless [`write_only`](Self::write_only); these options
/// also control creation, truncation, and append behavior.
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
    /// Open the file for writing only, see [`write_only`](Self::write_only).
    pub write_only: bool,
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
            write_only: false,
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

    /// Opens the file for writing only, like [`std::fs::write`], so that
    /// a file that cannot be read can be written. Reading from the handle
    /// fails.
    pub const fn write_only(mut self) -> Self {
        self.write_only = true;
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

/// Distinguishes files, directories and symlinks in a [`DirEntry`] or
/// [`Metadata`].
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
    /// A symlink, reported only where symlinks are not followed:
    /// [`Fs::read_dir`] and [`Fs::symlink_metadata`].
    Symlink,
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

    /// Returns `true` if this entry is a symlink.
    pub fn is_symlink(&self) -> bool {
        matches!(self, FileType::Symlink)
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
///     let entry = entry.unwrap();
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

    /// Returns the type of this entry (file, directory or symlink).
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
    /// The type of the entry (file or directory), a symlink only when
    /// returned by [`Fs::symlink_metadata`].
    pub file_type: FileType,
    /// The size of a file in bytes. The value for directories is platform
    /// specific. For a symlink it is the length of its target on Unix and
    /// `0` on Windows.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorrowState {
    /// The file is borrowed for reading by the given number of handles.
    Read(u64),
    /// The file is borrowed for writing by exactly one handle.
    Write,
}

/// The shared state behind every [`FileReadHandle`] and [`FileWriteHandle`].
///
/// Holds the OS file and its registration in the borrow-tracking table of
/// its [`FsImpl`]. When dropped, the registration is removed so the file
/// can be opened again.
pub struct FileHandle {
    borrows: Arc<Mutex<Borrows>>,
    key: u64,
    file: Arc<File>,
    path: PathBuf,
}

/// [internal]
///
/// Identifies a file regardless of the path it is opened by, so that a file
/// is borrowed once whether it is opened through symlinks, hard links or
/// different mounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct FileId {
    volume: u64,
    index: u128,
}

/// [internal]
///
/// The details of an opened file queried together, see `opened_file`.
pub(crate) struct OpenedFile {
    #[cfg(unix)]
    id: FileId,
    is_dir: bool,
    #[cfg(unix)]
    len: u64,
}

/// [internal]
///
/// The open handles of an [`FsImpl`], each borrowing its file for reading
/// or writing.
///
/// WHY: a handle is identified by its [`FileId`] only once another one is
/// opened or a path is checked against it. Its identity cannot change while
/// it is open, so until then nothing can conflict with it and identifying
/// it would only add a query to every open.
///
/// Kept in a list: only a few handles are open at a time, so scanning
/// them is cheaper than hashing their keys.
#[derive(Default)]
pub(crate) struct Borrows {
    next_key: u64,
    handles: Vec<(u64, Borrow)>,
}

/// [internal]
///
/// An open handle registered in [`Borrows`].
struct Borrow {
    file: Arc<File>,
    id: Option<FileId>,
    write: bool,
}

impl Borrows {
    fn is_empty(&self) -> bool {
        self.handles.is_empty()
    }

    /// Registers the open `file` borrowed for writing if `write` (reading
    /// otherwise) with its `id` if known and returns its key.
    fn insert(&mut self, file: Arc<File>, id: Option<FileId>, write: bool) -> u64 {
        let key = self.next_key;
        self.next_key += 1;
        self.handles.push((key, Borrow { file, id, write }));
        key
    }

    fn remove(&mut self, key: u64) {
        if let Some(index) = self.handles.iter().position(|(k, _)| *k == key) {
            self.handles.swap_remove(index);
        }
    }

    /// Returns how the file `id` is borrowed, if at all, identifying the
    /// handles not identified yet. A handle that cannot be identified is
    /// skipped.
    fn state(&mut self, id: FileId) -> Option<BorrowState> {
        let mut reads = 0;

        for (_, borrow) in &mut self.handles {
            if borrow.id.is_none() {
                borrow.id = handle_file_id(&borrow.file).ok();
            }

            if borrow.id == Some(id) {
                if borrow.write {
                    return Some(BorrowState::Write);
                }

                reads += 1;
            }
        }

        (reads > 0).then_some(BorrowState::Read(reads))
    }
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
        lock_handles(&self.borrows).remove(self.key);
    }
}

/// A read-only file handle backed by [`std::fs::File`].
///
/// Implements [`Read`] and [`Seek`]. Dropping the handle releases the
/// read borrow, allowing the file to be opened for writing.
pub struct FileReadHandle(FileHandle);

impl FileReadHandle {
    /// Registers the `file` opened at `path` with its `opened` details
    /// unless it is borrowed for writing.
    fn acquire(
        borrows: Arc<Mutex<Borrows>>,
        path: PathBuf,
        file: File,
        opened: &OpenedFile,
    ) -> Result<Self> {
        let key = {
            let mut handles = lock_handles(&borrows);
            // WHY: with no other handle open the file cannot be borrowed and
            // it is left to be identified only once needed.
            let id = opened_id(&file, opened, !handles.is_empty());

            if let Some(BorrowState::Write) = id.and_then(|id| handles.state(id)) {
                return Err(fs_error!(
                    "File could not be opened, it is already borrowed for writing: {}",
                    path.display()
                ));
            }

            let file = Arc::new(file);
            let key = handles.insert(file.clone(), id, false);
            (key, file)
        };

        Ok(Self(FileHandle {
            borrows,
            key: key.0,
            file: key.1,
            path,
        }))
    }
}

impl Read for FileReadHandle {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        (&*self.0.file).read(buf)
    }

    fn read_to_end(&mut self, buf: &mut Vec<u8>) -> std::io::Result<usize> {
        (&*self.0.file).read_to_end(buf)
    }

    fn read_to_string(&mut self, buf: &mut String) -> std::io::Result<usize> {
        (&*self.0.file).read_to_string(buf)
    }
}

impl Seek for FileReadHandle {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        (&*self.0.file).seek(pos)
    }
}

/// A read-write file handle backed by [`std::fs::File`].
///
/// Implements [`Read`], [`Write`], and [`Seek`]. Dropping the handle
/// releases the write borrow, allowing the file to be opened again for
/// reading or writing.
pub struct FileWriteHandle(FileHandle);

impl FileWriteHandle {
    /// Opens the file at `path` with `options` unless it is borrowed, which
    /// is checked only if it `may_exist`.
    fn acquire(
        borrows: Arc<Mutex<Borrows>>,
        path: PathBuf,
        options: &OpenOptions,
        may_exist: bool,
    ) -> Result<Self> {
        let (key, file) = {
            let mut handles = lock_handles(&borrows);

            // WHY: checked before opening because opening may truncate the
            // file. A file that cannot be identified does not exist yet, so
            // it cannot be borrowed, nor can any file if none is.
            if let Some(state) = (may_exist && !handles.is_empty())
                .then(|| path_file_id(&path).ok())
                .flatten()
                .and_then(|id| handles.state(id))
            {
                return Err(borrowed_error(&path, state, "opened"));
            }

            let file = Arc::new(options.open(&path).map_err(fs_error_at(&path))?);
            (handles.insert(file.clone(), None, true), file)
        };

        Ok(Self(FileHandle {
            borrows,
            key,
            file,
            path,
        }))
    }
}

impl Read for FileWriteHandle {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        (&*self.0.file).read(buf)
    }

    fn read_to_end(&mut self, buf: &mut Vec<u8>) -> std::io::Result<usize> {
        (&*self.0.file).read_to_end(buf)
    }

    fn read_to_string(&mut self, buf: &mut String) -> std::io::Result<usize> {
        (&*self.0.file).read_to_string(buf)
    }
}

impl Seek for FileWriteHandle {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        (&*self.0.file).seek(pos)
    }
}

impl Write for FileWriteHandle {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        (&*self.0.file).write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        (&*self.0.file).flush()
    }
}

impl FileHandle {
    fn metadata(&self) -> Result<Metadata> {
        native_handle_metadata(&self.path, &self.file).map(|native| to_metadata(&self.path, native))
    }

    fn lock_shared(&self) -> Result {
        self.file.lock_shared().map_err(fs_error_at(&self.path))
    }

    fn try_lock_shared(&self) -> Result<bool> {
        self.locked(self.file.try_lock_shared())
    }

    fn unlock(&self) -> Result {
        self.file.unlock().map_err(fs_error_at(&self.path))
    }

    /// Returns whether a `try` locking `result` acquired the lock, `false`
    /// if another handle holds it.
    fn locked(&self, result: std::result::Result<(), std::fs::TryLockError>) -> Result<bool> {
        result
            .map(|()| true)
            .or_else(|error| {
                let error = std::io::Error::from(error);
                (error.kind() == std::io::ErrorKind::WouldBlock)
                    .then_some(false)
                    .ok_or(error)
            })
            .map_err(fs_error_at(&self.path))
    }
}

impl ReadFile for FileReadHandle {
    fn metadata(&self) -> Result<Metadata> {
        self.0.metadata()
    }

    fn lock_shared(&self) -> Result {
        self.0.lock_shared()
    }

    fn try_lock_shared(&self) -> Result<bool> {
        self.0.try_lock_shared()
    }

    fn unlock(&self) -> Result {
        self.0.unlock()
    }
}

impl ReadFile for FileWriteHandle {
    fn metadata(&self) -> Result<Metadata> {
        self.0.metadata()
    }

    fn lock_shared(&self) -> Result {
        self.0.lock_shared()
    }

    fn try_lock_shared(&self) -> Result<bool> {
        self.0.try_lock_shared()
    }

    fn unlock(&self) -> Result {
        self.0.unlock()
    }
}

impl WriteFile for FileWriteHandle {
    fn set_len(&self, len: u64) -> Result {
        self.0.file.set_len(len).map_err(fs_error_at(&self.0.path))
    }

    fn sync_all(&self) -> Result {
        self.0.file.sync_all().map_err(fs_error_at(&self.0.path))
    }

    fn sync_data(&self) -> Result {
        self.0.file.sync_data().map_err(fs_error_at(&self.0.path))
    }

    fn lock(&self) -> Result {
        self.0.file.lock().map_err(fs_error_at(&self.0.path))
    }

    fn try_lock(&self) -> Result<bool> {
        self.0.locked(self.0.file.try_lock())
    }
}

/// [internal]
///
/// Returns the [`Metadata`] of the entry at `path` from its `native`
/// metadata and file type.
fn to_metadata(path: &Path, (metadata, file_type): (std::fs::Metadata, FileType)) -> Metadata {
    Metadata::new(
        path,
        file_type,
        metadata.len(),
        metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        metadata.created().unwrap_or(SystemTime::UNIX_EPOCH),
        native_attributes(&metadata),
    )
}

/// A lazy iterator over the entries of a directory of [`FsImpl`], backed by
/// [`std::fs::ReadDir`].
pub struct ReadDir {
    path: PathBuf,
    dir: std::fs::ReadDir,
}

impl Iterator for ReadDir {
    type Item = Result<DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        let to_entry = |path: PathBuf, file_type: std::fs::FileType| {
            file_type
                .is_dir()
                .then_some(FileType::Dir)
                .or(file_type.is_file().then_some(FileType::File))
                .or(file_type.is_symlink().then_some(FileType::Symlink))
                .and_then(|file_type| DirEntry::new(path, file_type))
        };

        self.dir.find_map(|result| {
            result
                .and_then(|entry| entry.file_type().map(|t| to_entry(entry.path(), t)))
                .map_err(fs_error_at(&self.path))
                .transpose()
        })
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
    handles: Arc<Mutex<Borrows>>,
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
            handles: Arc::new(Mutex::new(Borrows::default())),
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
    type Dir = ReadDir;

    fn open_read<P: AsRef<Path>>(&self, path: P) -> Result<Self::ReadHandle> {
        self.open_read_file(normalize_path(&self.current_dir, path))
            .map(|(handle, _)| handle)
    }

    fn write<P: AsRef<Path>, C: AsRef<[u8]>>(&self, path: P, data: C) -> Result<()> {
        let normalized_path = normalize_path(&self.current_dir, path);

        // WHY: opened for writing only, as by `std::fs::write`, so that a
        // file that cannot be read can be written.
        FileWriteHandle::acquire(
            self.handles.clone(),
            normalized_path,
            File::options().write(true).create(true).truncate(true),
            true,
        )
        .and_then(|handle| {
            (&*handle.0.file)
                .write_all(data.as_ref())
                .map_err(fs_error_at(&handle.0.path))
        })
    }

    fn read<P: AsRef<Path>>(&self, path: P) -> Result<Vec<u8>> {
        let (handle, opened) = self.open_read_file(normalize_path(&self.current_dir, path))?;

        opened_len(&handle.0.file, &opened)
            .and_then(|len| {
                let mut buffer = Vec::with_capacity(usize::try_from(len).unwrap_or_default());
                // WHY: through `Take` rather than `File::read_to_end`, which
                // would query the length again.
                Read::take(&*handle.0.file, u64::MAX)
                    .read_to_end(&mut buffer)
                    .map(|_| buffer)
            })
            .map_err(fs_error_at(&handle.0.path))
    }

    fn open_write_options<P: AsRef<Path>>(
        &self,
        path: P,
        options: OpenWriteOptions,
    ) -> Result<Self::WriteHandle> {
        let normalized_path = normalize_path(&self.current_dir, path);
        let mut open_options = File::options();
        open_options
            .read(!options.write_only)
            .write(true)
            .create(options.create)
            .create_new(options.create_new)
            .append(options.append)
            .truncate(options.truncate);

        #[cfg(unix)]
        if let Some(mode) = options.mode {
            std::os::unix::fs::OpenOptionsExt::mode(&mut open_options, mode);
        }

        FileWriteHandle::acquire(
            self.handles.clone(),
            normalized_path,
            &open_options,
            !options.create_new,
        )
    }

    fn read_dir<P: AsRef<Path>>(&self, path: P) -> Result<Self::Dir> {
        let normalized_path = normalize_path(&self.current_dir, path);
        let dir = std::fs::read_dir(&normalized_path).map_err(fs_error_at(&normalized_path))?;

        Ok(ReadDir {
            path: normalized_path,
            dir,
        })
    }

    fn create_dir<P: AsRef<Path>>(&self, path: P) -> Result {
        let normalized_path = normalize_path(&self.current_dir, path);
        std::fs::create_dir(&normalized_path).map_err(fs_error_at(&normalized_path))
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
        native_metadata(&normalized_path).map(|native| to_metadata(&normalized_path, native))
    }

    fn symlink_metadata<P: AsRef<Path>>(&self, path: P) -> Result<Metadata> {
        let normalized_path = normalize_path(&self.current_dir, path);
        native_symlink_metadata(&normalized_path)
            .map(|native| to_metadata(&normalized_path, native))
    }

    fn create_symlink<P: AsRef<Path>, T: AsRef<Path>>(&self, path: P, target: T) -> Result {
        let normalized_path = normalize_path(&self.current_dir, path);
        let target = native_symlink_target(target.as_ref());

        if target.as_os_str().is_empty() {
            return Err(fs_error!(
                "Symlink could not be created, target not found: {}",
                target.display()
            ));
        }

        // WHY: joined rather than normalized so that the OS resolves `..`
        // in the target the same way it will when following the symlink.
        // A trailing separator is left out so that it is checked below the
        // same way on every platform. An existing path is left to the OS
        // to report, a missing parent directory only explains a target
        // that cannot be found.
        let parent = normalized_path.parent().unwrap_or(&normalized_path);
        let (_, file_type) =
            native_metadata(&parent.join(target.components().as_path())).map_err(|error| {
                if parent_dir(&normalized_path).is_none() {
                    fs_error!(
                        "Symlink could not be created, parent directory does not exist: {}",
                        normalized_path.display()
                    )
                } else {
                    error
                }
            })?;

        if ends_with_separator(&target) && !file_type.is_dir() {
            return Err(fs_error!(
                "Symlink could not be created, target is not a directory: {}",
                target.display()
            ));
        }

        native_symlink(&normalized_path, &target, file_type).map_err(fs_error_at(&normalized_path))
    }

    fn read_link<P: AsRef<Path>>(&self, path: P) -> Result<PathBuf> {
        let normalized_path = normalize_path(&self.current_dir, path);
        std::fs::read_link(&normalized_path).map_err(fs_error_at(&normalized_path))
    }

    fn canonicalize<P: AsRef<Path>>(&self, path: P) -> Result<PathBuf> {
        let normalized_path = normalize_path(&self.current_dir, path);
        std::fs::canonicalize(&normalized_path).map_err(fs_error_at(&normalized_path))
    }

    fn create_hard_link<P: AsRef<Path>, T: AsRef<Path>>(&self, path: P, target: T) -> Result {
        let normalized_path = normalize_path(&self.current_dir, path);
        let normalized_target = normalize_path(&self.current_dir, target);

        // WHY: an existing path and a missing parent directory are left to
        // the OS to report.
        if native_metadata(&normalized_target)?.1.is_dir() {
            return Err(fs_error!(
                "Hard link could not be created, target is a directory: {}",
                normalized_target.display()
            ));
        }

        // WHY: `std::fs::hard_link` does not follow a symlink `target` on
        // every platform, the canonical path never is a symlink.
        std::fs::canonicalize(&normalized_target)
            .and_then(|target| std::fs::hard_link(target, &normalized_path))
            .map_err(fs_error_at(&normalized_path))
    }

    fn copy<S: AsRef<Path>, T: AsRef<Path>>(&self, source: S, target: T) -> Result {
        let normalized_source = normalize_path(&self.current_dir, source);
        let normalized_target = normalize_path(&self.current_dir, target);
        let (metadata, file_type) = native_symlink_metadata(&normalized_source)?;

        // WHY: a file is created with `create_new`, which fails for both
        // atomically, so they are only checked to explain its failure.
        if !matches!(file_type, FileType::File)
            && let Some(error) = copy_target_error(&normalized_target)
        {
            return Err(error);
        }

        match file_type {
            FileType::File => self.copy_file(
                &normalized_source,
                &normalized_target,
                metadata.permissions(),
            ),
            FileType::Dir => copy_dir(
                &normalized_source,
                &normalized_target,
                metadata.permissions(),
            ),
            FileType::Symlink => std::fs::read_link(&normalized_source)
                .map_err(fs_error_at(&normalized_source))
                .and_then(|link| self.create_symlink(&normalized_target, link)),
        }
    }

    fn copy_dir_all<S: AsRef<Path>, T: AsRef<Path>>(&self, source: S, target: T) -> Result {
        copy_tree(
            self,
            &normalize_path(&self.current_dir, source),
            &normalize_path(&self.current_dir, target),
        )
    }

    fn remove<P: AsRef<Path>>(&self, path: P) -> Result {
        let normalized_path = normalize_path(&self.current_dir, path);
        let (metadata, file_type) = native_symlink_metadata(&normalized_path)?;
        // WHY: held while removing so that the file cannot be opened in
        // between.
        let mut borrows = lock_handles(&self.handles);

        let result = match file_type {
            FileType::Dir => std::fs::remove_dir(&normalized_path),
            FileType::Symlink => native_remove_symlink(&normalized_path, &metadata),
            FileType::File => {
                check_not_borrowed(&normalized_path, &metadata, &mut borrows, "removed")?;
                std::fs::remove_file(&normalized_path)
            }
        };

        result.map_err(fs_error_at(&normalized_path))
    }

    fn remove_dir_all<P: AsRef<Path>>(&self, path: P) -> Result {
        let normalized_path = normalize_path(&self.current_dir, path);

        check_not_root(&normalized_path).and_then(|()| {
            // WHY: held while removing so that no file in the tree can be
            // opened in between.
            let mut borrows = lock_handles(&self.handles);

            // WHY: std removes relative to opened directories, which a
            // symlink swapped in during the removal cannot redirect. The
            // tree is walked only to find borrowed files, if any, and not
            // through a symlink at `path`, which is removed itself.
            if !borrows.is_empty()
                && std::fs::symlink_metadata(&normalized_path)
                    .is_ok_and(|metadata| metadata.is_dir())
            {
                check_tree_not_borrowed(&normalized_path, &mut borrows)?;
            }

            native_remove_dir_all(&normalized_path).map_err(fs_error_at(&normalized_path))
        })
    }

    fn rename<S: AsRef<Path>, T: AsRef<Path>>(&self, source: S, target: T) -> Result {
        let normalized_source = normalize_path(&self.current_dir, source);
        let normalized_target = normalize_path(&self.current_dir, target);
        // WHY: held while renaming so that a replaced file cannot be opened
        // in between.
        let mut borrows = lock_handles(&self.handles);
        let source = Entry::new(&normalized_source);
        let target = Entry::new(&normalized_target);
        check_rename_types(&source, &target)?;

        #[cfg(unix)]
        if other_link_of_same_file(&source, &target) {
            return std::fs::remove_file(&normalized_source)
                .map_err(fs_error_at(&normalized_source));
        }

        if let Some(metadata) = replaced_other_file(&source, &target, &borrows) {
            check_not_borrowed(&normalized_target, metadata, &mut borrows, "replaced")?;
        }

        std::fs::rename(&normalized_source, &normalized_target).map_err(|error| {
            fs_error!(
                "{error}: {} -> {}",
                normalized_source.display(),
                normalized_target.display()
            )
            .with_source(error)
        })
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

        std::fs::set_times(
            &normalized_path,
            std::fs::FileTimes::new().set_modified(modified),
        )
        .map_err(fs_error_at(&normalized_path))
    }

    #[cfg(unix)]
    fn set_mode<P: AsRef<Path>>(&self, path: P, mode: u32) -> Result {
        use std::os::unix::fs::PermissionsExt;

        let normalized_path = normalize_path(&self.current_dir, path);

        std::fs::set_permissions(
            &normalized_path,
            std::fs::Permissions::from_mode(mode & 0o7777),
        )
        .map_err(fs_error_at(&normalized_path))
    }
}

impl FsImpl {
    /// Opens the file at the normalized `path` for reading and returns it
    /// with its details.
    fn open_read_file(&self, path: PathBuf) -> Result<(FileReadHandle, OpenedFile)> {
        let (file, opened) = File::open(&path)
            .map_err(|error| open_read_error(&path, error))
            .and_then(|file| {
                opened_file(&file)
                    .map(|opened| (file, opened))
                    .map_err(fs_error_at(&path))
            })?;

        if opened.is_dir {
            return Err(directory_error(&path));
        }

        FileReadHandle::acquire(self.handles.clone(), path, file, &opened)
            .map(|handle| (handle, opened))
    }

    fn copy_file(&self, source: &Path, target: &Path, permissions: std::fs::Permissions) -> Result {
        let mut handles = lock_handles(&self.handles);
        let (source_file, target_file) = File::open(source)
            .map(Arc::new)
            .map_err(fs_error_at(source))
            .and_then(|source_file| {
                // WHY: with no other handle open the source cannot be
                // borrowed.
                let source_id = (!handles.is_empty())
                    .then(|| handle_file_id(&source_file).ok())
                    .flatten();

                if let Some(BorrowState::Write) = source_id.and_then(|id| handles.state(id)) {
                    return Err(fs_error!(
                        "File could not be opened, it is already borrowed for writing: {}",
                        source.display()
                    ));
                }

                let source_key = handles.insert(source_file.clone(), source_id, false);

                // WHY: registered by hand under the lock, which a dropped
                // handle would take again.
                open_copy_target(target, &mut handles)
                    .map(|file| {
                        let file = Arc::new(file);
                        let target_key = handles.insert(file.clone(), None, true);
                        ((source_key, source_file), (target_key, file))
                    })
                    .inspect_err(|_| handles.remove(source_key))
            })?;
        drop(handles);
        let source = FileReadHandle(FileHandle {
            borrows: self.handles.clone(),
            key: source_file.0,
            file: source_file.1,
            path: source.to_path_buf(),
        });
        let target_handle = FileWriteHandle(FileHandle {
            borrows: self.handles.clone(),
            key: target_file.0,
            file: target_file.1,
            path: target.to_path_buf(),
        });

        std::io::copy(&mut &*source.0.file, &mut &*target_handle.0.file)
            .and_then(|_| target_handle.0.file.set_permissions(permissions))
            .map_err(fs_error_at(target))
    }

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
/// The entries of a directory tree listed by [`list_tree`], each as its
/// `source` and `target` path. The `dirs` are in pre-order, the root
/// first.
#[derive(Default)]
struct Tree {
    dirs: Vec<(PathBuf, PathBuf)>,
    files: Vec<(PathBuf, PathBuf)>,
    symlinks: Vec<(PathBuf, PathBuf)>,
}

/// [internal]
///
/// Implements [`Fs::copy_dir_all`] on top of the other methods of `fs`.
pub(crate) fn copy_tree<F: Fs>(fs: &F, source: &Path, target: &Path) -> Result {
    fs.metadata(source)
        .and_then(|metadata| {
            if !metadata.file_type.is_dir() {
                return Err(fs_error!(
                    "Directory could not be copied, source is not a directory: {}",
                    source.display()
                ));
            }

            if fs.symlink_metadata(target).is_ok() {
                return Err(fs_error!(
                    "Directory could not be copied, target already exists: {}",
                    target.display()
                ));
            }

            if !target.parent().is_some_and(|parent| {
                fs.metadata(parent)
                    .is_ok_and(|metadata| metadata.file_type.is_dir())
            }) {
                return Err(fs_error!(
                    "Directory could not be copied, parent directory does not exist: {}",
                    target.display()
                ));
            }

            let mut tree = Tree::default();
            tree.dirs.push((source.to_path_buf(), target.to_path_buf()));
            list_tree(fs, source, target, &mut tree).map(|()| tree)
        })
        .and_then(|tree| {
            tree.dirs
                .iter()
                .try_for_each(|(_, target)| fs.create_dir_all(target))
                .and_then(|()| {
                    tree.files
                        .iter()
                        .try_for_each(|(source, target)| fs.copy(source, target))
                })
                .and_then(|()| copy_symlinks(fs, tree.symlinks))
                .and_then(|()| {
                    tree.dirs
                        .iter()
                        .rev()
                        .try_for_each(|(source, target)| copy_permissions(fs, source, target))
                })
        })
}

/// [internal]
///
/// Adds the entries of the directory `source` and of its subdirectories to
/// the `tree`, each with its path under `target`.
fn list_tree<F: Fs>(fs: &F, source: &Path, target: &Path, tree: &mut Tree) -> Result {
    fs.read_dir(source).and_then(|mut entries| {
        entries.try_for_each(|entry| {
            entry.and_then(|entry| {
                let paths = (entry.path().to_path_buf(), target.join(entry.file_name()));

                match entry.file_type() {
                    FileType::Dir => {
                        tree.dirs.push(paths.clone());
                        list_tree(fs, &paths.0, &paths.1, tree)
                    }
                    FileType::File => {
                        tree.files.push(paths);
                        Ok(())
                    }
                    FileType::Symlink => {
                        tree.symlinks.push(paths);
                        Ok(())
                    }
                }
            })
        })
    })
}

/// [internal]
///
/// Copies the `pending` symlinks in passes: a symlink can be copied only
/// once what it points to exists, which may be another symlink of them.
/// Fails with the last error once a pass copies none.
fn copy_symlinks<F: Fs>(fs: &F, mut pending: Vec<(PathBuf, PathBuf)>) -> Result {
    while !pending.is_empty() {
        let count = pending.len();
        let mut error = None;
        pending.retain(|(source, target)| {
            fs.copy(source, target)
                .map_err(|e| error = Some(e))
                .is_err()
        });

        if let Some(error) = error.filter(|_| pending.len() == count) {
            return Err(error);
        }
    }

    Ok(())
}

/// [internal]
///
/// Sets the permissions of the directory `target` to those of `source`,
/// see [`Fs::copy`].
fn copy_permissions<F: Fs>(fs: &F, source: &Path, target: &Path) -> Result {
    fs.metadata(source).and_then(|metadata| {
        #[cfg(unix)]
        let result = fs.set_mode(target, metadata.mode);
        #[cfg(windows)]
        let result = fs.set_readonly(target, metadata.readonly);

        result
    })
}

/// [internal]
///
/// Returns the error for opening the directory at `path` for reading.
pub(crate) fn directory_error(path: &Path) -> Error {
    fs_error!(
        "File could not be opened, path is a directory: {}",
        path.display()
    )
}

/// [internal]
///
/// Opens the `target` of [`Fs::copy`] of a file for writing, overwriting
/// it if it exists and is not borrowed in the locked `handles`, which
/// include the source.
///
/// WHY: created as a new file first, which can be neither the source nor
/// borrowed, so that only an existing one is identified to be checked.
fn open_copy_target(target: &Path, handles: &mut Borrows) -> Result<File> {
    let opened = match File::options().write(true).create_new(true).open(target) {
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if let Some(state) = path_file_id(target).ok().and_then(|id| handles.state(id)) {
                return Err(borrowed_error(target, state, "opened"));
            }

            File::options()
                .write(true)
                .create(true)
                .truncate(true)
                .open(target)
        }
        result => result,
    };

    opened
        .map_err(|error| missing_parent_error(target).unwrap_or_else(|| fs_error_at(target)(error)))
}

/// [internal]
///
/// Returns the error for a `target` of [`Fs::copy`] that already exists or
/// whose parent directory does not, if it does.
fn copy_target_error(target: &Path) -> Option<Error> {
    if std::fs::symlink_metadata(target).is_ok() {
        Some(fs_error!(
            "Path could not be copied, target already exists: {}",
            target.display()
        ))
    } else {
        missing_parent_error(target)
    }
}

/// [internal]
///
/// Returns the error for a `target` of [`Fs::copy`] whose parent directory
/// does not exist, if it does not.
fn missing_parent_error(target: &Path) -> Option<Error> {
    parent_dir(target).is_none().then(|| {
        fs_error!(
            "Path could not be copied, parent directory does not exist: {}",
            target.display()
        )
    })
}

/// [internal]
///
/// Creates the directory `target` if the directory `source` is empty.
fn copy_dir(source: &Path, target: &Path, permissions: std::fs::Permissions) -> Result {
    std::fs::read_dir(source)
        .map(|mut entries| entries.next().is_none())
        .map_err(fs_error_at(source))
        .and_then(|empty| {
            if !empty {
                return Err(fs_error!(
                    "Directory could not be copied, it is not empty: {}",
                    source.display()
                ));
            }

            std::fs::create_dir(target)
                .and_then(|()| std::fs::set_permissions(target, permissions))
                .map_err(fs_error_at(target))
        })
}

/// [internal]
///
/// Fails if the normalized `path` is a root, which [`Fs::remove_dir_all`]
/// refuses before anything is removed.
pub(crate) fn check_not_root(path: &Path) -> Result {
    if path.parent().is_none() && !path.as_os_str().is_empty() {
        return Err(fs_error!(
            "Directory could not be removed, path is a root: {}",
            path.display()
        ));
    }

    Ok(())
}

/// [internal]
///
/// A path of [`Fs::rename`] with its metadata, if it exists, queried once
/// without following a symlink.
struct Entry<'a> {
    path: &'a Path,
    metadata: Option<std::fs::Metadata>,
}

impl<'a> Entry<'a> {
    fn new(path: &'a Path) -> Self {
        Self {
            path,
            metadata: std::fs::symlink_metadata(path).ok(),
        }
    }

    fn is_dir(&self) -> Option<bool> {
        self.metadata.as_ref().map(std::fs::Metadata::is_dir)
    }

    /// Returns the metadata if the entry is a file.
    fn file(&self) -> Option<&std::fs::Metadata> {
        self.metadata.as_ref().filter(|metadata| metadata.is_file())
    }

    /// Returns the [`FileId`] if the entry is a file.
    fn file_id(&self) -> Option<FileId> {
        self.file()
            .and_then(|metadata| metadata_file_id(self.path, metadata).ok())
    }
}

/// [internal]
///
/// Fails if `source` is a directory and an existing `target` is not or
/// the other way around. The OS enforces it on Unix, but on Windows
/// `std::fs::rename` may fall back to `MoveFileExW` which lets a directory
/// replace a file.
fn check_rename_types(source: &Entry, target: &Entry) -> Result {
    match (source.is_dir(), target.is_dir()) {
        (Some(true), Some(false)) => Err(fs_error!(
            "Directory could not be renamed, target is not a directory: {}",
            target.path.display()
        )),
        (Some(false), Some(true)) => Err(fs_error!(
            "Path could not be renamed, target is a directory: {}",
            target.path.display()
        )),
        _ => Ok(()),
    }
}

/// [internal]
///
/// Returns `true` if `source` and `target` are different directory
/// entries, i.e. hard links, of the same file. POSIX `rename` does nothing
/// for them while Windows removes `source`. Entries in the same directory
/// whose names differ may still be one entry on a case-insensitive
/// filesystem, so they must both be listed.
#[cfg(unix)]
fn other_link_of_same_file(source: &Entry, target: &Entry) -> bool {
    fn entry(path: &Path) -> Option<(&Path, &std::ffi::OsStr)> {
        path.parent().zip(path.file_name())
    }

    source.file_id().is_some()
        && source.file_id() == target.file_id()
        && entry(source.path).zip(entry(target.path)).is_some_and(
            |((source_dir, source_name), (target_dir, target_name))| {
                path_file_id(source_dir).ok() != path_file_id(target_dir).ok()
                    || (source_name != target_name
                        && std::fs::read_dir(source_dir).is_ok_and(|entries| {
                            let names: Vec<_> = entries
                                .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
                                .collect();
                            names.iter().any(|name| name == source_name)
                                && names.iter().any(|name| name == target_name)
                        }))
            },
        )
}

/// [internal]
///
/// Returns the metadata of `target` if renaming `source` to it replaces a
/// file other than `source` and any file is borrowed, so that it must be
/// checked.
fn replaced_other_file<'a>(
    source: &Entry,
    target: &'a Entry,
    borrows: &Borrows,
) -> Option<&'a std::fs::Metadata> {
    target.file().filter(|_| {
        !borrows.is_empty() && !(source.file().is_some() && source.file_id() == target.file_id())
    })
}

/// [internal]
///
/// Fails if the file at `path`, whose `metadata` was queried, is borrowed,
/// so that it cannot be removed or replaced, the `action` named in the
/// error.
fn check_not_borrowed(
    path: &Path,
    metadata: &std::fs::Metadata,
    borrows: &mut Borrows,
    action: &str,
) -> Result {
    if borrows.is_empty() {
        return Ok(());
    }

    match metadata_file_id(path, metadata)
        .ok()
        .and_then(|id| borrows.state(id))
    {
        Some(state) => Err(borrowed_error(path, state, action)),
        None => Ok(()),
    }
}

/// [internal]
///
/// Returns the error for the file at `path` that could not be acted on,
/// the `action` named in it, because it is borrowed as `state`.
fn borrowed_error(path: &Path, state: BorrowState, action: &str) -> Error {
    match state {
        BorrowState::Read(count) => fs_error!(
            "File could not be {action}, it is already borrowed for reading {count} times: {}",
            path.display()
        ),
        BorrowState::Write => fs_error!(
            "File could not be {action}, it is already borrowed for writing: {}",
            path.display()
        ),
    }
}

/// [internal]
///
/// Fails if any file in the directory `dir` or in its subdirectories is
/// borrowed. Symlinks are not followed.
fn check_tree_not_borrowed(dir: &Path, borrows: &mut Borrows) -> Result {
    std::fs::read_dir(dir)
        .map_err(fs_error_at(dir))
        .and_then(|mut entries| {
            entries.try_for_each(|entry| {
                entry
                    .and_then(|entry| entry.metadata().map(|metadata| (entry.path(), metadata)))
                    .map_err(fs_error_at(dir))
                    .and_then(|(path, metadata)| {
                        if metadata.is_dir() {
                            check_tree_not_borrowed(&path, borrows)
                        } else if metadata.is_file() {
                            check_not_borrowed(&path, &metadata, borrows, "removed")
                        } else {
                            Ok(())
                        }
                    })
            })
        })
}

/// [internal]
///
/// Returns the parent of `path` if it is an existing directory.
fn parent_dir(path: &Path) -> Option<&Path> {
    path.parent()
        .filter(|parent| std::fs::metadata(parent).is_ok_and(|metadata| metadata.is_dir()))
}

/// [internal]
///
/// Returns `true` if `path` ends with a separator, which [`Path::components`]
/// drops but which requires the path to be a directory.
pub(crate) fn ends_with_separator(path: &Path) -> bool {
    path.as_os_str()
        .as_encoded_bytes()
        .last()
        .is_some_and(|&byte| std::path::is_separator(byte.into()))
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

/// [internal]
///
/// Returns `true` if the non-empty `path` has no `.`, `..` or empty
/// components, i.e. no repeated or trailing `/`, so that joining it gives
/// what normalizing it component by component would.
#[cfg(unix)]
fn is_normal(path: &Path) -> bool {
    let bytes = path.as_os_str().as_encoded_bytes();

    !bytes.is_empty()
        && bytes
            .split(|&byte| byte == b'/')
            .enumerate()
            .all(|(index, part)| match part {
                b"" => index == 0,
                b"." | b".." => false,
                _ => true,
            })
}

fn cannonicalize_path(current_dir: &Path, path: &Path) -> PathBuf {
    // WHY: left empty for the OS to reject, rather than resolved to the
    // current directory.
    if path.as_os_str().is_empty() {
        return PathBuf::new();
    }

    #[cfg(unix)]
    if is_normal(path) {
        return current_dir.join(path);
    }

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
    use crate::TestDir;

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

    /// Returns two `FsImpl`s, whose borrows are independent, in a test
    /// directory of the caller with the file `a.txt`.
    #[track_caller]
    fn two_fs_with_file() -> (TestDir, FsImpl, FsImpl) {
        let test_dir = TestDir::new();
        let fs = FsImpl::new(test_dir.path().to_path_buf());
        fs.write("a.txt", "aaa").unwrap();
        let other = FsImpl::new(test_dir.path().to_path_buf());
        (test_dir, fs, other)
    }

    fn assert_os_error(error: Error, path: &Path) {
        let suffix = format!(": {}", path.display());
        let os_message = error.description().strip_suffix(&suffix);
        assert!(
            os_message.is_some_and(|message| !message.is_empty()),
            "error should be the OS message followed by the path, got: {error}"
        );
    }

    fn assert_rename_os_error(error: Error, source: &Path, target: &Path) {
        assert_os_error(
            error,
            Path::new(&format!("{} -> {}", source.display(), target.display())),
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

        #[test]
        fn fs_impl_should_borrow_symlinked_file_under_its_target() {
            // GIVEN: a file and a symlink to it
            let test_dir = TestDir::new();
            let fs = FsImpl::new(test_dir.path().to_path_buf());
            fs.write("a.txt", "aaa").unwrap();
            fs.create_symlink("link", "a.txt").unwrap();

            // WHEN: the file is opened for writing through the symlink
            let _write = fs.open_write("link").unwrap();

            // THEN: opening the file itself fails
            assert_eq!(
                fs.open_read("a.txt").err().unwrap().to_string(),
                fs_error!(
                    "File could not be opened, it is already borrowed for writing: {}",
                    normalize_path(&fs.current_dir, "a.txt").display()
                )
                .to_string()
            );
        }

        #[test]
        fn fs_impl_should_borrow_hard_linked_file_once() {
            // GIVEN: a file with a second name
            let test_dir = TestDir::new();
            let fs = FsImpl::new(test_dir.path().to_path_buf());
            fs.write("a.txt", "aaa").unwrap();
            fs.create_hard_link("b.txt", "a.txt").unwrap();

            // WHEN: the file is opened for reading by one name
            let _read = fs.open_read("a.txt").unwrap();

            // THEN: opening it for writing by the other name fails
            assert_eq!(
                fs.open_write("b.txt").err().unwrap().to_string(),
                fs_error!(
                    "File could not be opened, it is already borrowed for reading 1 times: {}",
                    normalize_path(&fs.current_dir, "b.txt").display()
                )
                .to_string()
            );
            assert_eq!(fs.read_to_string("b.txt").unwrap(), "aaa");
        }

        #[test]
        fn fs_impl_should_borrow_file_created_through_dangling_symlink_under_its_target() {
            // GIVEN: a dangling symlink
            let test_dir = TestDir::new();
            let dir = test_dir.path();
            let fs = FsImpl::new(test_dir.path().to_path_buf());
            fs.write("a.txt", "").unwrap();
            fs.create_symlink("link", "a.txt").unwrap();
            std::fs::remove_file(dir.join("a.txt")).unwrap();

            // WHEN: the target is created by opening the symlink for writing
            let _write = fs.open_write("link").unwrap();

            // THEN: opening the created target fails
            assert_eq!(
                fs.open_read("a.txt").err().unwrap().to_string(),
                fs_error!(
                    "File could not be opened, it is already borrowed for writing: {}",
                    normalize_path(&fs.current_dir, "a.txt").display()
                )
                .to_string()
            );
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
                fn open_read_parent_dir_in_symlink_target_should_leave_resolved_dir() {
                    // GIVEN: `up` reached through `dir_link` points to `../b/c.txt`, which
                    // exists relative to `a/b` where `up` is but not relative to `dir_link`
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("a/b").unwrap();
                    fs.write("a/b/c.txt", "ccc").unwrap();
                    fs.create_symlink("a/b/up", "../b/c.txt").unwrap();
                    fs.create_symlink("dir_link", "a/b").unwrap();

                    // WHEN
                    let content = fs.read_to_string("dir_link/up");

                    // THEN
                    assert_eq!(content.unwrap(), "ccc");
                }

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
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
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
                fn open_write_options_write_only_should_write_but_not_read() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    let mut file = fs
                        .open_write_options("a.txt", OpenWriteOptions::new().create().write_only())
                        .unwrap();

                    file.write_all(b"aaa").unwrap();

                    file.rewind().unwrap();
                    assert!(file.read(&mut [0_u8; 3]).is_err());
                    drop(file);
                    assert_eq!(fs.read_to_string("a.txt").unwrap(), "aaa");
                }

                #[test]
                fn open_write_options_append_should_only_append_to_existing_content() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
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
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
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
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
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
                    let test_dir = TestDir::new();
                    let path = test_dir.path().join("fs_test_open_write_mode.tmp");
                    let fs = FsImpl::new(test_dir.path().to_path_buf());

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
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());

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

                #[cfg(unix)]
                #[test]
                fn write_should_write_file_that_cannot_be_read() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();
                    fs.set_mode("a.txt", 0o200).unwrap();

                    fs.write("a.txt", "bbb").unwrap();

                    fs.set_mode("a.txt", 0o600).unwrap();
                    assert_eq!(fs.read_to_string("a.txt").unwrap(), "bbb");
                }

                #[test]
                fn write_should_create_file_and_write_content() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("fs_test_write_create.tmp", "hello").unwrap();
                    assert_eq!(
                        fs.read_to_string("fs_test_write_create.tmp").unwrap(),
                        "hello"
                    );
                }

                #[test]
                fn write_should_overwrite_existing_content() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("fs_test_write_overwrite.tmp", "hello").unwrap();
                    fs.write("fs_test_write_overwrite.tmp", "a").unwrap();
                    assert_eq!(
                        fs.read_to_string("fs_test_write_overwrite.tmp").unwrap(),
                        "a"
                    );
                }

                #[test]
                fn write_should_fail_when_file_is_write_borrowed() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
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
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
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
                    let entries: Vec<DirEntry> =
                        fs.read_dir("src").unwrap().collect::<Result<_>>().unwrap();
                    let names: Vec<String> =
                        entries.iter().map(|e| e.file_name().to_string()).collect();
                    assert!(names.contains(&"fs.rs".to_string()));
                    assert!(names.contains(&"lib.rs".to_string()));
                }

                #[test]
                fn read_dir_should_distinguish_files_and_directories() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    let entries: Vec<DirEntry> =
                        fs.read_dir("src").unwrap().collect::<Result<_>>().unwrap();
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

                #[test]
                fn read_dir_should_list_symlinks_without_following() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();
                    fs.create_symlink("link", "a.txt").unwrap();

                    let mut entries: Vec<(String, FileType)> = fs
                        .read_dir(".")
                        .unwrap()
                        .map(|e| e.unwrap())
                        .map(|e| (e.file_name().to_string(), e.file_type()))
                        .collect();
                    entries.sort_by(|a, b| a.0.cmp(&b.0));
                    assert_eq!(
                        entries,
                        vec![
                            ("a.txt".to_string(), FileType::File),
                            ("link".to_string(), FileType::Symlink)
                        ]
                    );
                }

                #[cfg(unix)]
                #[test]
                fn read_dir_should_skip_non_utf8_names() {
                    use std::os::unix::ffi::OsStrExt;

                    let test_dir = TestDir::new();
                    let dir = test_dir.path();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();
                    std::fs::write(dir.join(std::ffi::OsStr::from_bytes(b"\xff.txt")), "bbb")
                        .unwrap();

                    let names: Vec<String> = fs
                        .read_dir(".")
                        .unwrap()
                        .map(|e| e.unwrap().file_name().to_string())
                        .collect();
                    assert_eq!(names, vec!["a.txt"]);
                }
            }

            mod create_dir {
                use super::*;

                #[test]
                fn create_dir_should_create_empty_dir() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());

                    fs.create_dir("dir").unwrap();

                    assert!(test_dir.path().join("dir").is_dir());
                    assert!(fs.read_dir("dir").unwrap().next().is_none());
                }

                #[test]
                fn create_dir_concurrently_should_succeed_once() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());

                    let created = std::thread::scope(|scope| {
                        let threads: Vec<_> = (0..8)
                            .map(|_| scope.spawn(|| fs.create_dir("lock").is_ok()))
                            .collect();
                        threads
                            .into_iter()
                            .map(|thread| thread.join().unwrap())
                            .filter(|&created| created)
                            .count()
                    });

                    assert_eq!(created, 1);
                }

                #[test]
                fn create_dir_existing_path_should_fail() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir").unwrap();
                    fs.write("a.txt", "aaa").unwrap();
                    fs.write("b.txt", "").unwrap();
                    fs.create_symlink("dangling", "b.txt").unwrap();
                    fs.remove("b.txt").unwrap();

                    for path in ["dir", "a.txt", "dangling"] {
                        assert_os_error(
                            fs.create_dir(path).err().unwrap(),
                            &normalize_path(&fs.current_dir, path),
                        );
                    }
                    assert!(!fs.exists("b.txt").unwrap());
                }

                #[test]
                fn create_dir_without_parent_dir_should_fail() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();

                    for path in ["missing/dir", "a.txt/dir"] {
                        assert_os_error(
                            fs.create_dir(path).err().unwrap(),
                            &normalize_path(&fs.current_dir, path),
                        );
                    }
                }
            }

            mod create_dir_all {
                use super::*;

                #[test]
                fn create_dir_all_empty_path_should_create_nothing() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());

                    fs.create_dir_all("").unwrap();

                    assert_eq!(std::fs::read_dir(test_dir.path()).unwrap().count(), 0);
                }

                #[test]
                fn create_dir_all_should_create_directory_and_parents() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("a/b").unwrap();

                    assert!(test_dir.path().join("a/b").is_dir());
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
                fn exists_empty_path_should_return_false() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());

                    assert!(!fs.exists("").unwrap());
                }

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
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
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
                    let test_dir = TestDir::new();

                    let path = test_dir.path().join("fs_test_metadata_mode.tmp");
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
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
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write(".fs_test_metadata_hidden.tmp", "").unwrap();

                    assert!(fs.metadata(".fs_test_metadata_hidden.tmp").unwrap().hidden);
                }

                #[cfg(windows)]
                #[test]
                fn metadata_executable_extension_should_be_executable() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
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
                            "Unsupported file type, path is neither a file, a directory nor a symlink: /dev/null"
                        )
                        .to_string()
                    );
                }

                #[test]
                fn metadata_should_follow_symlinks() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "abc").unwrap();
                    fs.create_symlink("link", "a.txt").unwrap();

                    let metadata = fs.metadata("link").unwrap();

                    assert_eq!((metadata.file_type, metadata.len), (FileType::File, 3));
                }
            }

            mod symlink_metadata {
                use super::*;

                #[test]
                fn symlink_metadata_should_not_follow_symlink() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "abcdef").unwrap();
                    fs.create_symlink("link", "a.txt").unwrap();

                    let metadata = fs.symlink_metadata("link").unwrap();

                    assert_eq!(metadata.file_type, FileType::Symlink);
                    #[cfg(unix)]
                    assert_eq!((metadata.len, metadata.mode), (5, 0o777));
                }

                #[test]
                fn symlink_metadata_file_should_return_file_metadata() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    assert_eq!(
                        fs.symlink_metadata("Cargo.toml").unwrap().file_type,
                        FileType::File
                    );
                }

                #[test]
                fn symlink_metadata_should_fail_for_nonexistent_path() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    assert_os_error(
                        fs.symlink_metadata("nonexistent.tmp").err().unwrap(),
                        &normalize_path(&fs.current_dir, "nonexistent.tmp"),
                    );
                }
            }

            mod create_symlink {
                use super::*;

                #[test]
                fn create_symlink_should_link_file_and_dir() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir").unwrap();
                    fs.write("a.txt", "aaa").unwrap();
                    fs.write("dir/b.txt", "bbb").unwrap();

                    fs.create_symlink("file_link", "a.txt").unwrap();
                    fs.create_symlink("dir_link", "dir").unwrap();

                    assert_eq!(fs.read_to_string("file_link").unwrap(), "aaa");
                    assert_eq!(fs.read_to_string("dir_link/b.txt").unwrap(), "bbb");
                }

                #[test]
                fn create_symlink_existing_path_should_fail() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();

                    assert_os_error(
                        fs.create_symlink("a.txt", "a.txt").err().unwrap(),
                        &normalize_path(&fs.current_dir, "a.txt"),
                    );
                }

                #[test]
                fn create_symlink_nested_relative_target_should_resolve_against_symlink_parent() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir").unwrap();
                    fs.write("dir/a.txt", "aaa").unwrap();

                    fs.create_symlink("link", "dir/a.txt").unwrap();

                    assert_eq!(fs.read_to_string("link").unwrap(), "aaa");
                }

                #[test]
                fn create_symlink_empty_target_should_fail() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());

                    assert_eq!(
                        fs.create_symlink("fs_test_create_symlink_empty.tmp", "")
                            .err()
                            .unwrap()
                            .to_string(),
                        fs_error!("Symlink could not be created, target not found: ").to_string()
                    );
                    assert!(
                        fs.symlink_metadata("fs_test_create_symlink_empty.tmp")
                            .is_err()
                    );
                }

                #[test]
                fn create_symlink_target_with_trailing_separator_should_require_dir() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir").unwrap();
                    fs.write("a.txt", "aaa").unwrap();

                    fs.create_symlink("dir_link", "dir/").unwrap();

                    assert!(fs.metadata("dir_link").unwrap().file_type.is_dir());
                    assert_eq!(
                        fs.create_symlink("file_link", "a.txt/")
                            .err()
                            .unwrap()
                            .to_string(),
                        fs_error!(
                            "Symlink could not be created, target is not a directory: {}",
                            native_symlink_target(Path::new("a.txt/")).display()
                        )
                        .to_string()
                    );
                }

                #[test]
                fn create_symlink_nonexistent_target_should_fail() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    assert_os_error(
                        fs.create_symlink("fs_test_create_symlink_dangling.tmp", "missing.tmp")
                            .err()
                            .unwrap(),
                        &normalize_path(&fs.current_dir, "missing.tmp"),
                    );
                    assert!(
                        fs.symlink_metadata("fs_test_create_symlink_dangling.tmp")
                            .is_err()
                    );
                }

                #[test]
                fn create_symlink_without_parent_dir_should_fail() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());

                    for (path, target) in [
                        ("fs_test_create_symlink_missing.tmp/link", "/"),
                        ("Cargo.toml/link", "/"),
                    ] {
                        assert_os_error(
                            fs.create_symlink(path, target).err().unwrap(),
                            &normalize_path(&fs.current_dir, path),
                        );
                    }

                    // WHY: a relative target cannot be resolved either, the
                    // missing parent directory is reported instead.
                    let path = "fs_test_create_symlink_missing.tmp/link";
                    assert_eq!(
                        fs.create_symlink(path, "missing.tmp")
                            .err()
                            .unwrap()
                            .to_string(),
                        fs_error!(
                            "Symlink could not be created, parent directory does not exist: {}",
                            normalize_path(&fs.current_dir, path).display()
                        )
                        .to_string()
                    );
                }
            }

            mod canonicalize {
                use super::*;

                #[test]
                fn canonicalize_should_resolve_symlinks_and_dots() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir").unwrap();
                    fs.write("dir/a.txt", "aaa").unwrap();
                    fs.create_symlink("link", "dir").unwrap();

                    assert_eq!(
                        fs.canonicalize("link/./a.txt").unwrap(),
                        std::fs::canonicalize(test_dir.path().join("dir").join("a.txt")).unwrap()
                    );
                }

                #[test]
                fn canonicalize_nonexistent_path_should_fail() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());

                    assert_os_error(
                        fs.canonicalize("missing").err().unwrap(),
                        &normalize_path(&fs.current_dir, "missing"),
                    );
                }
            }

            mod read_link {
                use super::*;

                #[test]
                fn read_link_should_return_target_as_given() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();
                    fs.create_symlink("link", "a.txt").unwrap();

                    assert_eq!(fs.read_link("link").unwrap(), Path::new("a.txt"));
                }

                #[test]
                fn read_link_file_should_fail() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    assert_os_error(
                        fs.read_link("Cargo.toml").err().unwrap(),
                        &normalize_path(&fs.current_dir, "Cargo.toml"),
                    );
                }
            }

            mod create_hard_link {
                use super::*;

                #[test]
                fn create_hard_link_should_share_file() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();

                    fs.create_hard_link("b.txt", "a.txt").unwrap();
                    fs.write("b.txt", "bbb").unwrap();
                    fs.set_readonly("b.txt", true).unwrap();

                    let metadata = fs.metadata("a.txt").unwrap();
                    assert_eq!(fs.read_to_string("a.txt").unwrap(), "bbb");
                    assert!(metadata.readonly);
                    fs.set_readonly("a.txt", false).unwrap();
                }

                #[test]
                fn create_hard_link_symlink_target_should_link_file() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();
                    fs.create_symlink("link", "a.txt").unwrap();

                    fs.create_hard_link("b.txt", "link").unwrap();

                    assert_eq!(
                        fs.symlink_metadata("b.txt").unwrap().file_type,
                        FileType::File
                    );
                }

                #[test]
                fn create_hard_link_existing_path_should_fail() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();

                    assert_os_error(
                        fs.create_hard_link("a.txt", "a.txt").err().unwrap(),
                        &normalize_path(&fs.current_dir, "a.txt"),
                    );
                }

                #[test]
                fn create_hard_link_without_parent_dir_should_fail() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());

                    for path in [
                        "fs_test_create_hard_link_missing.tmp/link",
                        "Cargo.toml/link",
                    ] {
                        assert_os_error(
                            fs.create_hard_link(path, "Cargo.toml").err().unwrap(),
                            &normalize_path(&fs.current_dir, path),
                        );
                    }
                }

                #[test]
                fn create_hard_link_nonexistent_target_should_fail() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    assert_os_error(
                        fs.create_hard_link("fs_test_create_hard_link_dangling.tmp", "missing.tmp")
                            .err()
                            .unwrap(),
                        &normalize_path(&fs.current_dir, "missing.tmp"),
                    );
                }

                #[test]
                fn create_hard_link_dir_target_should_fail() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    assert_eq!(
                        fs.create_hard_link("fs_test_create_hard_link_dir.tmp", "src")
                            .err()
                            .unwrap()
                            .to_string(),
                        fs_error!(
                            "Hard link could not be created, target is a directory: {}",
                            normalize_path(&fs.current_dir, "src").display()
                        )
                        .to_string()
                    );
                }
            }

            mod copy {
                use super::*;

                #[test]
                fn copy_hard_linked_file_should_create_independent_file() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();
                    fs.create_hard_link("b.txt", "a.txt").unwrap();

                    fs.copy("b.txt", "c.txt").unwrap();
                    fs.write("c.txt", "ccc").unwrap();

                    assert_eq!(fs.read_to_string("a.txt").unwrap(), "aaa");
                    assert_eq!(fs.read_to_string("c.txt").unwrap(), "ccc");
                }

                #[test]
                fn copy_empty_dir_should_create_empty_dir() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir").unwrap();

                    fs.copy("dir", "copy").unwrap();

                    assert!(fs.metadata("copy").unwrap().file_type.is_dir());
                    assert!(fs.read_dir("copy").unwrap().next().is_none());
                }

                #[cfg(unix)]
                #[test]
                fn copy_should_copy_mode() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();
                    fs.create_dir_all("dir").unwrap();
                    fs.set_mode("a.txt", 0o4750).unwrap();
                    fs.set_mode("dir", 0o700).unwrap();

                    fs.copy("a.txt", "b.txt").unwrap();
                    fs.copy("dir", "copy").unwrap();

                    assert_eq!(fs.metadata("b.txt").unwrap().mode, 0o4750);
                    assert_eq!(fs.metadata("copy").unwrap().mode, 0o700);
                }

                #[cfg(windows)]
                #[test]
                fn copy_should_copy_readonly_attribute() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();
                    fs.set_readonly("a.txt", true).unwrap();

                    fs.copy("a.txt", "b.txt").unwrap();

                    assert!(fs.metadata("b.txt").unwrap().readonly);
                    fs.set_readonly("a.txt", false).unwrap();
                    fs.set_readonly("b.txt", false).unwrap();
                }

                #[test]
                fn copy_symlink_should_create_symlink_with_same_target() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir").unwrap();
                    fs.write("a.txt", "aaa").unwrap();
                    fs.create_symlink("file_link", "a.txt").unwrap();
                    fs.create_symlink("dir_link", "dir").unwrap();

                    for (source, target) in [("file_link", "file_copy"), ("dir_link", "dir_copy")] {
                        fs.copy(source, target).unwrap();
                        assert_eq!(
                            fs.read_link(target).unwrap(),
                            fs.read_link(source).unwrap(),
                            "source: {source}"
                        );
                    }
                    assert_eq!(fs.read_to_string("file_copy").unwrap(), "aaa");
                    assert!(fs.metadata("dir_copy").unwrap().file_type.is_dir());
                }

                #[test]
                fn copy_symlink_should_resolve_against_target_parent() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir").unwrap();
                    fs.write("dir/a.txt", "aaa").unwrap();
                    fs.create_symlink("dir/link", "a.txt").unwrap();

                    assert_os_error(
                        fs.copy("dir/link", "link").err().unwrap(),
                        &normalize_path(&fs.current_dir, "a.txt"),
                    );
                }

                #[test]
                fn copy_should_allow_read_borrowed_source() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();
                    let _read = fs.open_read("a.txt").unwrap();

                    fs.copy("a.txt", "b.txt").unwrap();

                    assert_eq!(fs.read_to_string("b.txt").unwrap(), "aaa");
                }

                #[test]
                fn copy_write_borrowed_source_should_fail() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();
                    let _write = fs.open_write("a.txt").unwrap();

                    assert_eq!(
                        fs.copy("a.txt", "b.txt").err().unwrap().to_string(),
                        fs_error!(
                            "File could not be opened, it is already borrowed for writing: {}",
                            normalize_path(&fs.current_dir, "a.txt").display()
                        )
                        .to_string()
                    );
                    assert!(!fs.exists("b.txt").unwrap());
                }

                #[test]
                fn copy_file_should_overwrite_existing_file() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();
                    fs.write("b.txt", "longer content").unwrap();

                    fs.copy("a.txt", "b.txt").unwrap();

                    assert_eq!(fs.read_to_string("b.txt").unwrap(), "aaa");
                }

                #[test]
                fn copy_file_to_symlink_should_overwrite_its_target() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();
                    fs.write("b.txt", "bbb").unwrap();
                    fs.create_symlink("link", "b.txt").unwrap();

                    fs.write("c.txt", "").unwrap();
                    fs.create_symlink("dangling", "c.txt").unwrap();
                    fs.remove("c.txt").unwrap();

                    fs.copy("a.txt", "link").unwrap();
                    fs.copy("a.txt", "dangling").unwrap();

                    assert_eq!(fs.read_to_string("b.txt").unwrap(), "aaa");
                    assert_eq!(fs.read_to_string("c.txt").unwrap(), "aaa");
                    assert!(fs.symlink_metadata("link").unwrap().file_type.is_symlink());
                }

                #[test]
                fn copy_file_to_dir_should_fail() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();
                    fs.create_dir("dir").unwrap();

                    assert_os_error(
                        fs.copy("a.txt", "dir").err().unwrap(),
                        &normalize_path(&fs.current_dir, "dir"),
                    );
                }

                #[test]
                fn copy_file_to_other_name_of_itself_should_fail() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();
                    fs.create_hard_link("b.txt", "a.txt").unwrap();

                    assert_eq!(
                        fs.copy("a.txt", "b.txt").err().unwrap().to_string(),
                        fs_error!(
                            "File could not be opened, it is already borrowed for reading 1 times: {}",
                            normalize_path(&fs.current_dir, "b.txt").display()
                        )
                        .to_string()
                    );
                    assert_eq!(fs.read_to_string("a.txt").unwrap(), "aaa");
                }

                #[test]
                fn copy_dir_or_symlink_to_existing_target_should_fail() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();
                    fs.write("b.txt", "bbb").unwrap();
                    fs.create_dir("dir").unwrap();
                    fs.create_symlink("link", "a.txt").unwrap();

                    for source in ["dir", "link"] {
                        assert_eq!(
                            fs.copy(source, "b.txt").err().unwrap().to_string(),
                            fs_error!(
                                "Path could not be copied, target already exists: {}",
                                normalize_path(&fs.current_dir, "b.txt").display()
                            )
                            .to_string(),
                            "source: {source}"
                        );
                    }

                    assert_eq!(fs.read_to_string("b.txt").unwrap(), "bbb");
                }

                #[test]
                fn copy_without_target_parent_dir_should_fail() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();

                    for target in ["missing/b.txt", "a.txt/b.txt"] {
                        assert_eq!(
                            fs.copy("a.txt", target).err().unwrap().to_string(),
                            fs_error!(
                                "Path could not be copied, parent directory does not exist: {}",
                                normalize_path(&fs.current_dir, target).display()
                            )
                            .to_string(),
                            "target: {target}"
                        );
                    }
                }

                #[test]
                fn copy_nonexistent_source_should_fail() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());

                    assert_os_error(
                        fs.copy("missing.txt", "b.txt").err().unwrap(),
                        &normalize_path(&fs.current_dir, "missing.txt"),
                    );
                }

                #[test]
                fn copy_non_empty_dir_should_fail() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir/sub").unwrap();

                    assert_eq!(
                        fs.copy("dir", "copy").err().unwrap().to_string(),
                        fs_error!(
                            "Directory could not be copied, it is not empty: {}",
                            normalize_path(&fs.current_dir, "dir").display()
                        )
                        .to_string()
                    );
                    assert!(!fs.exists("copy").unwrap());
                }

                #[cfg(unix)]
                #[test]
                fn copy_should_fail_for_unsupported_file_type() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    let path = normalize_path(&fs.current_dir, "socket");
                    let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();

                    assert_eq!(
                        fs.copy("socket", "copy").err().unwrap().to_string(),
                        fs_error!(
                            "Unsupported file type, path is neither a file, a directory nor a symlink: {}",
                            path.display()
                        )
                        .to_string()
                    );
                }
            }

            mod copy_dir_all {
                use super::*;

                #[test]
                fn copy_dir_all_should_copy_tree() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir/sub").unwrap();
                    fs.create_dir_all("dir/empty").unwrap();
                    fs.write("dir/a.txt", "aaa").unwrap();
                    fs.write("dir/sub/b.txt", "bbb").unwrap();
                    fs.create_symlink("dir/link", "sub/b.txt").unwrap();
                    fs.create_symlink("dir/sub/up", "../empty").unwrap();

                    fs.copy_dir_all("dir", "copy").unwrap();
                    fs.write("copy/sub/b.txt", "ccc").unwrap();

                    assert_eq!(fs.read_to_string("copy/a.txt").unwrap(), "aaa");
                    assert_eq!(fs.read_to_string("copy/link").unwrap(), "ccc");
                    assert_eq!(fs.read_to_string("dir/sub/b.txt").unwrap(), "bbb");
                    assert_eq!(
                        fs.read_link("copy/sub/up").unwrap(),
                        native_symlink_target(Path::new("../empty"))
                    );
                    assert!(fs.read_dir("copy/empty").unwrap().next().is_none());
                }

                #[test]
                fn copy_dir_all_symlink_to_symlink_should_be_copied_after_it() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir").unwrap();
                    fs.write("dir/z.txt", "zzz").unwrap();
                    fs.create_symlink("dir/b_link", "z.txt").unwrap();
                    fs.create_symlink("dir/a_link", "b_link").unwrap();
                    fs.create_symlink("dir/c_link", "a_link").unwrap();

                    fs.copy_dir_all("dir", "copy").unwrap();

                    assert_eq!(fs.read_to_string("copy/c_link").unwrap(), "zzz");
                    assert_eq!(fs.read_link("copy/c_link").unwrap(), Path::new("a_link"));
                }

                #[cfg(unix)]
                #[test]
                fn copy_dir_all_should_copy_dir_mode_after_content() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir/sub").unwrap();
                    fs.write("dir/sub/a.txt", "aaa").unwrap();
                    fs.set_mode("dir/sub", 0o555).unwrap();
                    fs.set_mode("dir", 0o750).unwrap();

                    let result = fs.copy_dir_all("dir", "copy");
                    let modes = (
                        fs.metadata("copy").map(|metadata| metadata.mode),
                        fs.metadata("copy/sub").map(|metadata| metadata.mode),
                    );
                    for path in ["dir/sub", "copy/sub"] {
                        let _ = fs.set_mode(path, 0o755);
                    }

                    result.unwrap();
                    assert_eq!((modes.0.unwrap(), modes.1.unwrap()), (0o750, 0o555));
                    assert_eq!(fs.read_to_string("copy/sub/a.txt").unwrap(), "aaa");
                }

                #[cfg(windows)]
                #[test]
                fn copy_dir_all_should_copy_dir_readonly_attribute() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir/sub").unwrap();
                    fs.write("dir/sub/a.txt", "aaa").unwrap();
                    fs.set_readonly("dir/sub", true).unwrap();

                    let result = fs.copy_dir_all("dir", "copy");
                    let readonly = fs.metadata("copy/sub").map(|metadata| metadata.readonly);
                    for path in ["dir/sub", "copy/sub"] {
                        let _ = fs.set_readonly(path, false);
                    }

                    result.unwrap();
                    assert!(readonly.unwrap());
                }

                #[test]
                fn copy_dir_all_target_inside_source_should_copy_source_as_listed() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir").unwrap();
                    fs.write("dir/a.txt", "aaa").unwrap();

                    fs.copy_dir_all("dir", "dir/copy").unwrap();

                    assert_eq!(fs.read_to_string("dir/copy/a.txt").unwrap(), "aaa");
                    assert!(!fs.exists("dir/copy/copy").unwrap());
                }

                #[test]
                fn copy_dir_all_should_check_parent_of_normalized_target() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir").unwrap();
                    fs.write("dir/a.txt", "aaa").unwrap();

                    fs.copy_dir_all("dir", "copy/missing/..").unwrap();

                    assert_eq!(fs.read_to_string("copy/a.txt").unwrap(), "aaa");
                }

                #[test]
                fn copy_dir_all_file_source_should_fail() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();

                    assert_eq!(
                        fs.copy_dir_all("a.txt", "copy").err().unwrap().to_string(),
                        fs_error!(
                            "Directory could not be copied, source is not a directory: {}",
                            normalize_path(&fs.current_dir, "a.txt").display()
                        )
                        .to_string()
                    );
                }

                #[test]
                fn copy_dir_all_nonexistent_source_should_fail() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());

                    assert_os_error(
                        fs.copy_dir_all("missing", "copy").err().unwrap(),
                        &normalize_path(&fs.current_dir, "missing"),
                    );
                }

                #[test]
                fn copy_dir_all_existing_target_should_fail() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir").unwrap();
                    fs.write("a.txt", "aaa").unwrap();

                    for target in ["a.txt", "dir"] {
                        assert_eq!(
                            fs.copy_dir_all("dir", target).err().unwrap().to_string(),
                            fs_error!(
                                "Directory could not be copied, target already exists: {}",
                                normalize_path(&fs.current_dir, target).display()
                            )
                            .to_string(),
                            "target: {target}"
                        );
                    }
                }

                #[test]
                fn copy_dir_all_without_target_parent_dir_should_fail() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir").unwrap();
                    fs.write("a.txt", "aaa").unwrap();

                    for target in ["missing/copy", "a.txt/copy"] {
                        assert_eq!(
                            fs.copy_dir_all("dir", target).err().unwrap().to_string(),
                            fs_error!(
                                "Directory could not be copied, parent directory does not exist: {}",
                                normalize_path(&fs.current_dir, target).display()
                            )
                            .to_string(),
                            "target: {target}"
                        );
                    }
                }

                #[test]
                fn copy_dir_all_unresolvable_symlink_should_fail() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir").unwrap();
                    fs.create_dir_all("other").unwrap();
                    fs.write("a.txt", "aaa").unwrap();
                    fs.create_symlink("dir/link", "../a.txt").unwrap();

                    assert_os_error(
                        fs.copy_dir_all("dir", "other/copy").err().unwrap(),
                        &normalize_path(&fs.current_dir, "other/copy")
                            .join(native_symlink_target(Path::new("../a.txt"))),
                    );
                    assert!(fs.exists("other/copy").unwrap());
                }
            }

            mod remove {
                use super::*;

                #[test]
                fn remove_should_remove_file_and_empty_dir() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir").unwrap();
                    fs.write("a.txt", "aaa").unwrap();
                    fs.set_readonly("a.txt", true).unwrap();

                    for path in ["a.txt", "dir"] {
                        fs.remove(path).unwrap();
                        assert!(fs.symlink_metadata(path).is_err(), "path: {path}");
                    }
                }

                #[test]
                fn remove_symlink_should_keep_target() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir").unwrap();
                    fs.write("dir/a.txt", "aaa").unwrap();
                    fs.create_symlink("file_link", "dir/a.txt").unwrap();
                    fs.create_symlink("dir_link", "dir").unwrap();

                    for path in ["file_link", "dir_link"] {
                        fs.remove(path).unwrap();
                        assert!(fs.symlink_metadata(path).is_err(), "path: {path}");
                    }
                    assert_eq!(fs.read_to_string("dir/a.txt").unwrap(), "aaa");
                }

                #[test]
                fn remove_hard_link_should_keep_other_name() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();
                    fs.create_hard_link("b.txt", "a.txt").unwrap();

                    fs.remove("a.txt").unwrap();

                    assert!(!fs.exists("a.txt").unwrap());
                    assert_eq!(fs.read_to_string("b.txt").unwrap(), "aaa");
                }

                #[test]
                fn remove_borrowed_file_should_fail() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();
                    fs.create_hard_link("b.txt", "a.txt").unwrap();
                    let path = normalize_path(&fs.current_dir, "b.txt");

                    let read = fs.open_read("a.txt").unwrap();
                    assert_eq!(
                        fs.remove("b.txt")
                            .err()
                            .unwrap()
                            .to_string(),
                        fs_error!(
                            "File could not be removed, it is already borrowed for reading 1 times: {}",
                            path.display()
                        )
                        .to_string()
                    );
                    drop(read);

                    let _write = fs.open_write("a.txt").unwrap();
                    assert_eq!(
                        fs.remove("b.txt").err().unwrap().to_string(),
                        fs_error!(
                            "File could not be removed, it is already borrowed for writing: {}",
                            path.display()
                        )
                        .to_string()
                    );
                    assert!(fs.exists("b.txt").unwrap());
                }

                #[test]
                fn remove_non_empty_dir_should_fail() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();

                    assert_os_error(
                        fs.remove(".").err().unwrap(),
                        &normalize_path(&fs.current_dir, "."),
                    );
                }

                #[test]
                fn remove_should_fail_for_nonexistent_path() {
                    let fs = FsImpl::new(std::env::current_dir().unwrap());
                    assert_os_error(
                        fs.remove("nonexistent.tmp").err().unwrap(),
                        &normalize_path(&fs.current_dir, "nonexistent.tmp"),
                    );
                }

                #[cfg(unix)]
                #[test]
                fn remove_should_fail_for_unsupported_file_type() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    let path = normalize_path(&fs.current_dir, "socket");
                    let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();

                    assert_eq!(
                        fs.remove("socket")
                            .err()
                            .unwrap()
                            .to_string(),
                        fs_error!(
                            "Unsupported file type, path is neither a file, a directory nor a symlink: {}",
                            path.display()
                        )
                        .to_string()
                    );
                }
            }

            mod remove_dir_all {
                use super::*;

                #[test]
                fn remove_dir_all_empty_path_should_fail_and_keep_current_dir() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();

                    assert!(fs.remove_dir_all("").is_err());
                    assert_eq!(fs.read_to_string("a.txt").unwrap(), "aaa");
                }

                #[test]
                fn remove_dir_all_should_remove_tree_but_keep_symlink_targets() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir/sub/empty").unwrap();
                    fs.create_dir_all("outside").unwrap();
                    fs.write("dir/a.txt", "aaa").unwrap();
                    fs.write("dir/sub/b.txt", "bbb").unwrap();
                    fs.write("outside/c.txt", "ccc").unwrap();
                    fs.set_readonly("dir/a.txt", true).unwrap();
                    fs.create_symlink("dir/file_link", "../outside/c.txt")
                        .unwrap();
                    fs.create_symlink("dir/sub/dir_link", "../../outside")
                        .unwrap();

                    fs.remove_dir_all("dir").unwrap();

                    assert!(fs.symlink_metadata("dir").is_err());
                    assert_eq!(fs.read_to_string("outside/c.txt").unwrap(), "ccc");
                }

                #[test]
                fn remove_dir_all_symlink_should_remove_only_symlink() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir").unwrap();
                    fs.write("dir/a.txt", "aaa").unwrap();
                    fs.create_symlink("link", "dir").unwrap();

                    fs.remove_dir_all("link").unwrap();

                    assert!(fs.symlink_metadata("link").is_err());
                    assert_eq!(fs.read_to_string("dir/a.txt").unwrap(), "aaa");
                }

                #[test]
                fn remove_dir_all_file_should_fail() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();

                    assert_os_error(
                        fs.remove_dir_all("a.txt").err().unwrap(),
                        &normalize_path(&fs.current_dir, "a.txt"),
                    );
                    assert!(fs.exists("a.txt").unwrap());
                }

                #[test]
                fn remove_dir_all_borrowed_file_should_fail() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir").unwrap();
                    fs.write("dir/a.txt", "aaa").unwrap();
                    let _write = fs.open_write("dir/a.txt").unwrap();

                    assert_eq!(
                        fs.remove_dir_all("dir").err().unwrap().to_string(),
                        fs_error!(
                            "File could not be removed, it is already borrowed for writing: {}",
                            normalize_path(&fs.current_dir, "dir")
                                .join("a.txt")
                                .display()
                        )
                        .to_string()
                    );
                    assert!(fs.exists("dir/a.txt").unwrap());
                }

                #[test]
                fn remove_dir_all_should_fail_for_nonexistent_path() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    assert_os_error(
                        fs.remove_dir_all("dir").err().unwrap(),
                        &normalize_path(&fs.current_dir, "dir"),
                    );
                }

                #[test]
                fn remove_dir_all_should_ignore_files_borrowed_outside_tree() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir/sub").unwrap();
                    fs.write("dir/sub/a.txt", "aaa").unwrap();
                    fs.write("b.txt", "bbb").unwrap();
                    fs.create_symlink("dir/link", "../b.txt").unwrap();
                    let _read = fs.open_read("b.txt").unwrap();

                    fs.remove_dir_all("dir").unwrap();

                    assert!(!fs.exists("dir").unwrap());
                }

                #[test]
                fn remove_dir_all_hard_link_to_borrowed_file_should_fail() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir/sub").unwrap();
                    fs.write("dir/a.txt", "aaa").unwrap();
                    fs.write("b.txt", "bbb").unwrap();
                    fs.create_hard_link("dir/sub/link", "b.txt").unwrap();
                    let _read = fs.open_read("b.txt").unwrap();

                    assert_eq!(
                        fs.remove_dir_all("dir").err().unwrap().to_string(),
                        fs_error!(
                            "File could not be removed, it is already borrowed for reading 1 times: {}",
                            normalize_path(&fs.current_dir, "dir")
                                .join("sub")
                                .join("link")
                                .display()
                        )
                        .to_string()
                    );
                    assert!(fs.exists("dir/a.txt").unwrap());
                }

                #[cfg(unix)]
                #[test]
                fn remove_dir_all_should_remove_unsupported_entries() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir").unwrap();
                    let _listener = std::os::unix::net::UnixListener::bind(normalize_path(
                        &fs.current_dir,
                        "dir/socket",
                    ))
                    .unwrap();

                    fs.remove_dir_all("dir").unwrap();

                    assert!(!fs.exists("dir").unwrap());
                }
            }

            mod rename {
                use super::*;

                #[test]
                fn rename_borrowed_file_should_keep_borrow_under_new_name() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();
                    let mut read = fs.open_read("a.txt").unwrap();

                    fs.rename("a.txt", "b.txt").unwrap();

                    assert!(!fs.exists("a.txt").unwrap());
                    assert!(fs.open_write("b.txt").is_err());
                    let mut content = String::new();
                    read.read_to_string(&mut content).unwrap();
                    assert_eq!(content, "aaa");
                }

                #[test]
                fn rename_dir_should_move_tree() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir/sub").unwrap();
                    fs.create_dir_all("other").unwrap();
                    fs.write("dir/sub/a.txt", "aaa").unwrap();
                    fs.create_symlink("dir/link", "sub/a.txt").unwrap();

                    fs.rename("dir", "other/moved").unwrap();

                    assert!(!fs.exists("dir").unwrap());
                    assert_eq!(fs.read_to_string("other/moved/link").unwrap(), "aaa");
                }

                #[test]
                fn rename_symlink_should_move_only_symlink() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir").unwrap();
                    fs.write("a.txt", "aaa").unwrap();
                    fs.create_symlink("link", "a.txt").unwrap();

                    fs.rename("link", "dir/link").unwrap();

                    assert_eq!(fs.read_link("dir/link").unwrap(), Path::new("a.txt"));
                    assert_eq!(fs.read_to_string("a.txt").unwrap(), "aaa");
                }

                #[test]
                fn rename_file_should_replace_file() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();
                    fs.write("b.txt", "bbb").unwrap();

                    fs.rename("a.txt", "b.txt").unwrap();

                    assert!(!fs.exists("a.txt").unwrap());
                    assert_eq!(fs.read_to_string("b.txt").unwrap(), "aaa");
                }

                #[test]
                fn rename_hard_link_onto_same_file_should_remove_source() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir").unwrap();
                    fs.write("a.txt", "aaa").unwrap();
                    fs.create_hard_link("b.txt", "a.txt").unwrap();
                    fs.create_hard_link("dir/c.txt", "a.txt").unwrap();
                    let mut read = fs.open_read("a.txt").unwrap();

                    for (source, target) in [("a.txt", "b.txt"), ("b.txt", "dir/c.txt")] {
                        fs.rename(source, target).unwrap();
                        assert!(!fs.exists(source).unwrap(), "source: {source}");
                    }

                    assert_eq!(fs.read_to_string("dir/c.txt").unwrap(), "aaa");
                    let mut content = String::new();
                    read.read_to_string(&mut content).unwrap();
                    assert_eq!(content, "aaa");
                }

                #[test]
                fn rename_to_itself_should_do_nothing() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir").unwrap();
                    fs.write("dir/a.txt", "aaa").unwrap();
                    fs.create_symlink("link", "dir").unwrap();
                    let _read = fs.open_read("dir/a.txt").unwrap();

                    for (source, target) in [
                        ("dir/a.txt", "dir/a.txt"),
                        ("dir/a.txt", "dir/../dir/a.txt"),
                        ("link/a.txt", "dir/a.txt"),
                    ] {
                        fs.rename(source, target).unwrap();
                        assert_eq!(
                            fs.read_to_string("dir/a.txt").unwrap(),
                            "aaa",
                            "source: {source}, target: {target}"
                        );
                    }
                }

                #[test]
                fn rename_should_replace_target_as_std() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir/sub").unwrap();
                    fs.create_dir_all("empty").unwrap();
                    fs.write("a.txt", "aaa").unwrap();
                    fs.write("b.txt", "bbb").unwrap();
                    fs.write("c.txt", "").unwrap();
                    fs.create_symlink("link", "b.txt").unwrap();
                    fs.create_symlink("dangling", "c.txt").unwrap();
                    fs.remove("c.txt").unwrap();

                    fs.rename("a.txt", "dangling").unwrap();
                    fs.rename("link", "b.txt").unwrap();
                    fs.rename("dir", "empty").unwrap();

                    assert_eq!(fs.read_to_string("dangling").unwrap(), "aaa");
                    assert_eq!(fs.read_link("b.txt").unwrap(), Path::new("b.txt"));
                    assert!(fs.exists("empty/sub").unwrap());
                }

                #[test]
                fn rename_incompatible_target_should_fail() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir").unwrap();
                    fs.create_dir_all("full/sub").unwrap();
                    fs.write("a.txt", "aaa").unwrap();
                    fs.write("b.txt", "").unwrap();
                    fs.create_symlink("dangling", "b.txt").unwrap();
                    fs.remove("b.txt").unwrap();

                    for (source, target, error) in [
                        (
                            "dir",
                            "a.txt",
                            "Directory could not be renamed, target is not a directory",
                        ),
                        (
                            "dir",
                            "dangling",
                            "Directory could not be renamed, target is not a directory",
                        ),
                        (
                            "a.txt",
                            "dir",
                            "Path could not be renamed, target is a directory",
                        ),
                        (
                            "dangling",
                            "dir",
                            "Path could not be renamed, target is a directory",
                        ),
                    ] {
                        assert_eq!(
                            fs.rename(source, target)
                                .expect_err(&format!("source: {source}, target: {target}"))
                                .to_string(),
                            fs_error!(
                                "{error}: {}",
                                normalize_path(&fs.current_dir, target).display()
                            )
                            .to_string(),
                            "source: {source}, target: {target}"
                        );
                    }

                    assert_rename_os_error(
                        fs.rename("dir", "full")
                            .expect_err("source: dir, target: full"),
                        &normalize_path(&fs.current_dir, "dir"),
                        &normalize_path(&fs.current_dir, "full"),
                    );
                }

                #[test]
                fn rename_borrowed_target_should_fail() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();
                    fs.write("b.txt", "bbb").unwrap();
                    let _read = fs.open_read("b.txt").unwrap();

                    assert_eq!(
                        fs.rename("a.txt", "b.txt").err().unwrap().to_string(),
                        fs_error!(
                            "File could not be replaced, it is already borrowed for reading 1 times: {}",
                            normalize_path(&fs.current_dir, "b.txt").display()
                        )
                        .to_string()
                    );
                    assert!(fs.exists("a.txt").unwrap());
                }

                #[test]
                fn rename_onto_symlink_to_borrowed_file_should_replace_symlink() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();
                    fs.write("b.txt", "bbb").unwrap();
                    fs.create_symlink("link", "b.txt").unwrap();
                    let _read = fs.open_read("b.txt").unwrap();

                    fs.rename("a.txt", "link").unwrap();

                    assert_eq!(fs.read_to_string("link").unwrap(), "aaa");
                    assert_eq!(fs.read_to_string("b.txt").unwrap(), "bbb");
                }

                #[test]
                fn rename_without_target_parent_dir_should_fail() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "aaa").unwrap();

                    for target in ["missing/b.txt", "a.txt/b.txt"] {
                        assert_rename_os_error(
                            fs.rename("a.txt", target).err().unwrap(),
                            &normalize_path(&fs.current_dir, "a.txt"),
                            &normalize_path(&fs.current_dir, target),
                        );
                    }
                }

                #[test]
                fn rename_dir_into_itself_should_fail() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.create_dir_all("dir/sub").unwrap();

                    assert_rename_os_error(
                        fs.rename("dir", "dir/sub/moved").err().unwrap(),
                        &normalize_path(&fs.current_dir, "dir"),
                        &normalize_path(&fs.current_dir, "dir/sub/moved"),
                    );
                    assert!(fs.exists("dir/sub").unwrap());
                }

                #[test]
                fn rename_nonexistent_source_should_fail() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    assert_rename_os_error(
                        fs.rename("a.txt", "b.txt").err().unwrap(),
                        &normalize_path(&fs.current_dir, "a.txt"),
                        &normalize_path(&fs.current_dir, "b.txt"),
                    );
                }
            }

            mod set_readonly {
                use super::*;

                #[cfg(unix)]
                #[test]
                fn set_readonly_should_map_to_write_bits() {
                    let test_dir = TestDir::new();
                    let path = test_dir.path().join("fs_test_set_readonly.tmp");
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
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
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
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

                    let test_dir = TestDir::new();
                    let path = test_dir.path().join("fs_test_set_readonly_normal.tmp");
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
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
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
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
                    let test_dir = TestDir::new();
                    let path = test_dir.path().join("fs_test_set_executable_file.tmp");
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
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
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());

                    assert_eq!(
                        fs.set_executable(".", true).err().unwrap().to_string(),
                        fs_error!(
                            "Executable attribute cannot be set, path is a directory: {}",
                            normalize_path(&fs.current_dir, ".").display()
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
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
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

                #[cfg(unix)]
                #[test]
                fn set_modified_should_set_time_of_file_that_cannot_be_read() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "").unwrap();
                    fs.set_mode("a.txt", 0o200).unwrap();
                    let modified = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000);

                    fs.set_modified("a.txt", modified).unwrap();

                    assert_eq!(fs.metadata("a.txt").unwrap().modified, modified);
                }

                #[test]
                fn set_modified_should_set_modified_time() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "").unwrap();
                    let modified = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000);

                    for path in ["a.txt", "."] {
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
                            "Unsupported file type, path is neither a file, a directory nor a symlink: /dev/null"
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
                    let test_dir = TestDir::new();
                    let path = test_dir.path().join("fs_test_set_mode.tmp");
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
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
                fn set_mode_should_set_mode_of_special_file() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    let path = test_dir.path().join("socket");
                    let _socket = std::os::unix::net::UnixListener::bind(&path).unwrap();

                    fs.set_mode("socket", 0o600).unwrap();

                    assert_eq!(mode(&path), 0o600);
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
                        "Unsupported file type, path is neither a file, a directory nor a symlink: /dev/null"
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
            fn drop_should_not_panic_when_file_not_in_handles() {
                let handle = FileHandle {
                    borrows: Arc::new(Mutex::new(Borrows::default())),
                    key: 0,
                    file: Arc::new(std::fs::File::open("Cargo.toml").unwrap()),
                    path: PathBuf::from("Cargo.toml"),
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

        mod read_file {
            use super::*;

            mod metadata {
                use super::*;

                #[test]
                fn metadata_should_return_metadata_of_open_file() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write(".a.txt", "content").unwrap();

                    let metadata = fs.open_read(".a.txt").unwrap().metadata().unwrap();

                    assert_eq!(metadata, fs.metadata(".a.txt").unwrap());
                    assert_eq!(metadata.len, 7);
                }
            }

            mod lock_shared {
                use super::*;

                #[test]
                fn lock_shared_should_allow_only_other_shared_locks() {
                    // GIVEN a file locked for reading through one `FsImpl`
                    let (_test_dir, fs, other) = two_fs_with_file();
                    let file = fs.open_read("a.txt").unwrap();

                    file.lock_shared().unwrap();

                    // THEN another `FsImpl` can lock it for reading only
                    assert!(other.open_read("a.txt").unwrap().try_lock_shared().unwrap());
                    assert!(!other.open_write("a.txt").unwrap().try_lock().unwrap());
                }
            }

            mod try_lock_shared {
                use super::*;

                #[test]
                fn try_lock_shared_should_fail_while_exclusively_locked() {
                    let (_test_dir, fs, other) = two_fs_with_file();
                    let file = fs.open_write("a.txt").unwrap();
                    file.lock().unwrap();
                    let other_file = other.open_read("a.txt").unwrap();

                    assert!(!other_file.try_lock_shared().unwrap());
                    file.unlock().unwrap();
                    assert!(other_file.try_lock_shared().unwrap());
                }
            }

            mod unlock {
                use super::*;

                #[test]
                fn unlock_should_release_lock() {
                    let (_test_dir, fs, other) = two_fs_with_file();
                    let file = fs.open_read("a.txt").unwrap();
                    file.lock_shared().unwrap();
                    let other_file = other.open_write("a.txt").unwrap();

                    file.unlock().unwrap();

                    assert!(other_file.try_lock().unwrap());
                }
            }
        }

        mod read {
            use super::*;

            mod read_to_end {
                use super::*;

                #[test]
                fn read_to_end_should_append_whole_file() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "content").unwrap();
                    let mut buffer = b"old ".to_vec();

                    let read = fs.open_read("a.txt").unwrap().read_to_end(&mut buffer);

                    assert_eq!(read.unwrap(), 7);
                    assert_eq!(buffer, b"old content");
                }
            }

            mod read_to_string {
                use super::*;

                #[test]
                fn read_to_string_should_append_whole_file() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "content").unwrap();
                    let mut buffer = "old ".to_string();

                    let read = fs.open_read("a.txt").unwrap().read_to_string(&mut buffer);

                    assert_eq!(read.unwrap(), 7);
                    assert_eq!(buffer, "old content");
                }
            }
        }
    }

    mod file_write_handle {
        use super::*;

        mod read_file {
            use super::*;

            mod metadata {
                use super::*;

                #[test]
                fn metadata_should_return_metadata_of_open_file() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    let mut file = fs.open_write("a.txt").unwrap();
                    file.write_all(b"content").unwrap();

                    let metadata = file.metadata().unwrap();

                    assert_eq!(metadata.len, 7);
                    assert!(metadata.file_type.is_file());
                }
            }

            mod lock_shared {
                use super::*;

                #[test]
                fn lock_shared_should_exclude_exclusive_locks() {
                    let (_test_dir, fs, other) = two_fs_with_file();
                    let file = fs.open_write("a.txt").unwrap();

                    file.lock_shared().unwrap();

                    assert!(!other.open_write("a.txt").unwrap().try_lock().unwrap());
                }
            }

            mod try_lock_shared {
                use super::*;

                #[test]
                fn try_lock_shared_should_lock_unlocked_file() {
                    let (_test_dir, fs, other) = two_fs_with_file();
                    let file = fs.open_write("a.txt").unwrap();

                    assert!(file.try_lock_shared().unwrap());
                    assert!(!other.open_write("a.txt").unwrap().try_lock().unwrap());
                }
            }

            mod unlock {
                use super::*;

                #[test]
                fn unlock_should_release_lock() {
                    let (_test_dir, fs, other) = two_fs_with_file();
                    let file = fs.open_write("a.txt").unwrap();
                    file.lock().unwrap();
                    let other_file = other.open_read("a.txt").unwrap();

                    file.unlock().unwrap();

                    assert!(other_file.try_lock_shared().unwrap());
                }
            }
        }

        mod write_file {
            use super::*;

            mod lock {
                use super::*;

                #[test]
                fn lock_should_exclude_other_locks_until_dropped() {
                    // GIVEN a file locked exclusively through one `FsImpl`
                    let (_test_dir, fs, other) = two_fs_with_file();
                    let file = fs.open_write("a.txt").unwrap();
                    file.lock().unwrap();
                    let other_file = other.open_read("a.txt").unwrap();

                    // THEN another `FsImpl` cannot lock it until it is dropped
                    assert!(!other_file.try_lock_shared().unwrap());
                    drop(file);
                    assert!(other_file.try_lock_shared().unwrap());
                }
            }

            mod try_lock {
                use super::*;

                #[test]
                fn try_lock_should_fail_while_locked_by_other_handle() {
                    let (_test_dir, fs, other) = two_fs_with_file();
                    let file = fs.open_read("a.txt").unwrap();
                    file.lock_shared().unwrap();
                    let other_file = other.open_write("a.txt").unwrap();

                    assert!(!other_file.try_lock().unwrap());
                    drop(file);
                    assert!(other_file.try_lock().unwrap());
                }
            }

            mod set_len {
                use super::*;

                #[test]
                fn set_len_should_truncate_or_extend_with_zeros_keeping_cursor() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "content").unwrap();
                    let mut file = fs.open_write("a.txt").unwrap();
                    file.seek(SeekFrom::Start(3)).unwrap();

                    file.set_len(2).unwrap();
                    file.set_len(4).unwrap();

                    assert_eq!(file.stream_position().unwrap(), 3);
                    drop(file);
                    assert_eq!(fs.read("a.txt").unwrap(), b"co\0\0");
                }
            }

            mod sync_all {
                use super::*;

                #[test]
                fn sync_all_should_succeed() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    let mut file = fs.open_write("a.txt").unwrap();
                    file.write_all(b"content").unwrap();

                    file.sync_all().unwrap();
                }
            }

            mod sync_data {
                use super::*;

                #[test]
                fn sync_data_should_succeed() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    let mut file = fs.open_write("a.txt").unwrap();
                    file.write_all(b"content").unwrap();

                    file.sync_data().unwrap();
                }
            }
        }

        mod read {
            use super::*;

            mod read_to_end {
                use super::*;

                #[test]
                fn read_to_end_should_append_rest_of_file() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "content").unwrap();
                    let mut buffer = Vec::new();

                    let read = fs.open_write("a.txt").unwrap().read_to_end(&mut buffer);

                    assert_eq!(read.unwrap(), 7);
                    assert_eq!(buffer, b"content");
                }
            }

            mod read_to_string {
                use super::*;

                #[test]
                fn read_to_string_should_append_rest_of_file() {
                    let test_dir = TestDir::new();
                    let fs = FsImpl::new(test_dir.path().to_path_buf());
                    fs.write("a.txt", "content").unwrap();
                    let mut buffer = String::new();

                    let read = fs.open_write("a.txt").unwrap().read_to_string(&mut buffer);

                    assert_eq!(read.unwrap(), 7);
                    assert_eq!(buffer, "content");
                }
            }
        }
    }

    mod borrows {
        use super::*;

        mod state {
            use super::*;

            #[test]
            fn state_should_identify_handles_regardless_of_path_once_needed() {
                // GIVEN a file opened through a symlink, not identified yet
                let test_dir = TestDir::new();
                let dir = test_dir.path();
                let fs = FsImpl::new(test_dir.path().to_path_buf());
                fs.write("a.txt", "aaa").unwrap();
                fs.write("b.txt", "bbb").unwrap();
                fs.create_symlink("symlink", "a.txt").unwrap();
                fs.create_hard_link("hard_link", "a.txt").unwrap();
                let mut borrows = Borrows::default();
                let file = Arc::new(File::open(dir.join("symlink")).unwrap());
                borrows.insert(file, None, false);
                let id = |name: &str| path_file_id(&dir.join(name)).unwrap();

                // WHEN asked for the file by its other names
                let states = [
                    borrows.state(id("a.txt")),
                    borrows.state(id("hard_link")),
                    borrows.state(id("b.txt")),
                ];

                // THEN it is identified as the same file
                assert_eq!(
                    states,
                    [Some(BorrowState::Read(1)), Some(BorrowState::Read(1)), None]
                );
            }

            #[test]
            fn state_should_count_reads_and_report_write() {
                let mut borrows = Borrows::default();
                let id = path_file_id(Path::new("Cargo.toml")).unwrap();
                let open = || Arc::new(File::open("Cargo.toml").unwrap());
                borrows.insert(open(), Some(id), false);
                let second = borrows.insert(open(), None, false);

                assert_eq!(borrows.state(id), Some(BorrowState::Read(2)));

                borrows.remove(second);
                borrows.insert(open(), Some(id), true);

                assert_eq!(borrows.state(id), Some(BorrowState::Write));
            }
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

    mod check_not_root {
        use super::*;

        #[test]
        fn check_not_root_should_fail_only_for_root() {
            #[cfg(unix)]
            let (root, dir) = ("/", "/dir");
            #[cfg(windows)]
            let (root, dir) = ("C:\\", "C:\\dir");

            assert_eq!(
                check_not_root(Path::new(root)).err().unwrap().to_string(),
                fs_error!("Directory could not be removed, path is a root: {root}").to_string()
            );
            assert!(check_not_root(Path::new(dir)).is_ok());
        }
    }

    mod ends_with_separator {
        use super::*;

        #[test]
        fn ends_with_separator_should_detect_trailing_separator() {
            for (path, expected) in [("a/", true), ("a/b", false), ("", false), ("/", true)] {
                assert_eq!(
                    ends_with_separator(Path::new(path)),
                    expected,
                    "path: {path}"
                );
            }
        }
    }

    #[cfg(unix)]
    mod is_normal {
        use super::*;

        #[test]
        fn is_normal_should_reject_dot_and_empty_components() {
            for (path, expected) in [
                ("a", true),
                ("a/b", true),
                ("/a/b", true),
                (".a/b..", true),
                ("", false),
                ("/", false),
                ("a/", false),
                ("a//b", false),
                ("./a", false),
                ("a/./b", false),
                ("a/..", false),
                ("../a", false),
            ] {
                assert_eq!(is_normal(Path::new(path)), expected, "path: {path}");
            }
        }
    }

    #[cfg(unix)]
    mod normalize_path {
        use super::*;

        #[test]
        fn normalize_path_should_resolve_against_current_dir_with_or_without_dots() {
            for (path, expected) in [
                ("a/b", "/cwd/a/b"),
                ("/a/b", "/a/b"),
                ("a/./b/../c/", "/cwd/a/c"),
                ("a//b", "/cwd/a/b"),
                ("../a", "/a"),
                ("", ""),
            ] {
                assert_eq!(
                    normalize_path("/cwd", path),
                    PathBuf::from(expected),
                    "path: {path}"
                );
            }
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

        mod is_symlink {
            use super::*;

            #[test]
            fn is_symlink_should_return_true_for_symlink() {
                assert!(FileType::Symlink.is_symlink());
                assert!(!FileType::File.is_symlink());
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
