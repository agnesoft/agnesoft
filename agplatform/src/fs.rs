use std::collections::HashMap;
use std::collections::hash_map;
use std::fmt::Display;
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

use crate::Error;
use crate::Result;
use crate::platform::platform_env::CurrentDirSetter;

macro_rules! fs_error {
    ($($arg:tt)*) => {
        $crate::Error::fs(format!($($arg)*))
    };
}

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
        let mut file = self.open_read(path)?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)
            .map_err(fs_error_at(&error_path))?;
        Ok(buffer)
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
        String::from_utf8(self.read(path)?).map_err(fs_error_at(&error_path))
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
        let mut file =
            self.open_write_options(path, OpenWriteOptions::new().create().truncate())?;
        file.write_all(data.as_ref())
            .map_err(fs_error_at(&error_path))?;
        Ok(())
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
}

impl OpenWriteOptions {
    /// Returns a new set of options with all flags disabled.
    pub const fn new() -> Self {
        OpenWriteOptions {
            append: false,
            truncate: false,
            create: false,
            create_new: false,
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

        FileWriteHandle::acquire(
            self.handles.clone(),
            normalized_path,
            File::options()
                .read(true)
                .write(true)
                .create(options.create)
                .create_new(options.create_new)
                .append(options.append)
                .truncate(options.truncate),
        )
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
}

/// [internal]
///
/// Returns a mapping into an [`Error::fs`] whose description is the mapped
/// error followed by the `path` it occurred on, for use with
/// [`Result::map_err`].
pub(crate) fn fs_error_at<T: Display>(path: &Path) -> impl FnOnce(T) -> Error + '_ {
    move |error| fs_error!("{error}: {}", path.display())
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
pub(crate) fn normalize_path<C: Into<PathBuf>, P: AsRef<Path>>(current_dir: C, path: P) -> PathBuf {
    if path.as_ref().is_relative() {
        cannonicalize_path(current_dir.into(), path)
    } else {
        cannonicalize_path(PathBuf::new(), path)
    }
}

fn cannonicalize_path<P: AsRef<Path>>(mut base: PathBuf, path: P) -> PathBuf {
    for component in path.as_ref().components() {
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
        fn fs_error_at_should_map_into_fs_error_for_path() {
            let path = Path::new("/some/file.txt");
            let result: std::result::Result<(), &str> = Err("fs error");
            assert_eq!(
                result.map_err(fs_error_at(path)).unwrap_err().to_string(),
                fs_error!("fs error: {}", path.display()).to_string()
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
}
