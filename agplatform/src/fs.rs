use std::collections::HashMap;
use std::collections::hash_map;
use std::fs::File;
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

/// The trait represents a file system interface.
pub trait Fs {
    fn open_read<P: AsRef<Path>>(&self, path: P) -> Result<impl Read + Seek>;
    fn open_write<P: AsRef<Path>>(&self, path: P) -> Result<impl Read + Write + Seek> {
        self.open_write_options(path, OpenWriteOptions::new().create())
    }
    fn open_write_options<P: AsRef<Path>>(
        &self,
        path: P,
        options: OpenWriteOptions,
    ) -> Result<impl Read + Write + Seek>;

    /// Reads the contents of a file at the given path as bytes (Vec<u8>).
    fn read<P: AsRef<Path>>(&self, path: P) -> Result<Vec<u8>>;

    /// Reads the contents of a file at the given path as UTF-8 encoded String.
    fn read_to_string<P: AsRef<Path>>(&self, path: P) -> Result<String> {
        String::from_utf8(self.read(path)?).map_err(Error::fs)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenWriteOptions {
    pub append: bool,
    pub truncate: bool,
    pub create: bool,
    pub create_new: bool,
}

impl OpenWriteOptions {
    pub const fn new() -> Self {
        OpenWriteOptions {
            append: false,
            truncate: false,
            create: false,
            create_new: false,
        }
    }

    pub const fn append(mut self) -> Self {
        self.append = true;
        self
    }

    pub const fn create(mut self) -> Self {
        self.create = true;
        self
    }

    pub const fn create_new(mut self) -> Self {
        self.create_new = true;
        self
    }

    pub const fn truncate(mut self) -> Self {
        self.truncate = true;
        self
    }
}

#[derive(Debug, Clone, Copy)]
pub enum BorrowState {
    Read(u64),
    Write,
}

pub struct FileHandle {
    handle: Arc<Mutex<HashMap<PathBuf, BorrowState>>>,
    path: PathBuf,
    file: File,
}

impl Drop for FileHandle {
    fn drop(&mut self) {
        if let Ok(mut handles) = self.handle.lock()
            && let Some(state) = handles.get_mut(&self.path)
        {
            match state {
                BorrowState::Read(count) => {
                    *count -= 1;

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

pub struct FileReadHandle(FileHandle);

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

pub struct FileWriteHandle(FileHandle);

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

/// The default implementation using the real file system via std::fs.
pub struct FsImpl {
    pub(crate) current_dir: PathBuf,
    handles: Arc<Mutex<HashMap<PathBuf, BorrowState>>>,
}

impl FsImpl {
    /// Creates a new instance of the default file system implementation.
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
    fn open_read<P: AsRef<Path>>(&self, path: P) -> Result<impl Read + Seek> {
        let normalized_path = normalize_path(&self.current_dir, path);

        {
            let mut handles = self.handles.lock().map_err(Error::fs)?;
            let entry = handles.entry(normalized_path.clone());

            match entry {
                hash_map::Entry::Occupied(mut open) => match open.get() {
                    BorrowState::Read(count) => {
                        open.insert(BorrowState::Read(count + 1));
                    }
                    BorrowState::Write => {
                        return Err(Error::fs(format!(
                            "File '{}' is already borrowed for writing",
                            normalized_path.display()
                        )));
                    }
                },
                hash_map::Entry::Vacant(_) => {
                    entry.insert_entry(BorrowState::Read(1));
                }
            }
        }

        let file = File::open(&normalized_path).map_err(Error::fs)?;
        Ok(FileReadHandle(FileHandle {
            handle: self.handles.clone(),
            path: normalized_path,
            file,
        }))
    }

    fn open_write_options<P: AsRef<Path>>(
        &self,
        path: P,
        options: OpenWriteOptions,
    ) -> Result<impl Read + Write + Seek> {
        let normalized_path = normalize_path(&self.current_dir, path);

        {
            let mut handles = self.handles.lock().map_err(Error::fs)?;
            let entry = handles.entry(normalized_path.clone());

            match entry {
                hash_map::Entry::Occupied(open) => match open.get() {
                    BorrowState::Read(count) => {
                        return Err(Error::fs(format!(
                            "File '{}' is already borrowed for reading {count} times",
                            normalized_path.display()
                        )));
                    }
                    BorrowState::Write => {
                        return Err(Error::fs(format!(
                            "File '{}' is already borrowed for writing",
                            normalized_path.display()
                        )));
                    }
                },
                hash_map::Entry::Vacant(_) => {
                    entry.insert_entry(BorrowState::Write);
                }
            }
        }

        let file = File::options()
            .read(true)
            .write(true)
            .create(options.create)
            .create_new(options.create_new)
            .append(options.append)
            .truncate(options.truncate)
            .open(&normalized_path)
            .map_err(Error::fs)?;

        Ok(FileWriteHandle(FileHandle {
            handle: self.handles.clone(),
            path: normalized_path,
            file,
        }))
    }

    /// Reads the contents of a file at the given path as bytes (Vec<u8>) or String.
    /// The path is normalized relative to the current directory or used as is
    /// if it is an absolute path.
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform};
    ///
    /// let platform = agplatform::platform();
    ///
    /// // read as bytes
    /// let content = platform.fs().read("Cargo.toml").unwrap();
    /// assert!(!content.is_empty());
    ///
    /// // read as UTF-8 string
    /// let content_utf8 = platform.fs().read_to_string("Cargo.toml").unwrap();
    /// assert!(!content_utf8.is_empty());
    /// ```
    fn read<P: AsRef<Path>>(&self, path: P) -> Result<Vec<u8>> {
        std::fs::read(normalize_path(&self.current_dir, path)).map_err(Error::fs)
    }
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

    #[test]
    fn read() {
        let fs = FsImpl::new(std::env::current_dir().unwrap());
        let path = fs
            .current_dir
            .join("Cargo.toml")
            .to_string_lossy()
            .to_string();
        let result = fs.read(&path).unwrap();
        assert!(!result.is_empty());
    }

    #[test]
    fn read_to_string() {
        let fs = FsImpl::new(std::env::current_dir().unwrap());
        let result = fs.read_to_string("./src/../Cargo.toml").unwrap();
        assert!(!result.is_empty());
    }

    #[test]
    fn read_fail() {
        let fs = FsImpl::new(std::env::current_dir().unwrap());
        let result = fs.read_to_string("Cargo.toml2");
        assert!(result.is_err());
    }

    #[test]
    fn set_current_dir() {
        let mut fs = FsImpl::new(PathBuf::new());
        let new_dir = PathBuf::from("/tmp");
        fs.set_current_dir(new_dir.clone());
        assert_eq!(fs.current_dir, new_dir);
    }

    #[test]
    fn file_borrow_checker() {
        let fs = FsImpl::new(std::env::current_dir().unwrap());
        let normalized_path = normalize_path(&fs.current_dir, "Cargo.toml");

        let file = fs.open_read("Cargo.toml").unwrap();
        let same_file_read = fs.open_read("Cargo.toml").unwrap();

        let file_write_err = fs.open_write("Cargo.toml").err().unwrap().to_string();
        assert_eq!(
            file_write_err,
            Error::fs(format!(
                "File '{}' is already borrowed for reading 2 times",
                normalized_path.display()
            ))
            .to_string()
        );

        drop(file);
        drop(same_file_read);

        let file_write_ok = fs.open_write("Cargo.toml").unwrap();
        let file_read_err = fs.open_read("Cargo.toml").err().unwrap().to_string();

        assert_eq!(
            file_read_err,
            Error::fs(format!(
                "File '{}' is already borrowed for writing",
                normalized_path.display()
            ))
            .to_string()
        );

        let same_file_write_err = fs.open_write("Cargo.toml").err().unwrap().to_string();
        assert_eq!(
            same_file_write_err,
            Error::fs(format!(
                "File '{}' is already borrowed for writing",
                normalized_path.display()
            ))
            .to_string()
        );

        drop(file_write_ok);

        let _file_read_ok = fs.open_read("Cargo.toml").unwrap();
    }

    #[test]
    fn read_via_read_handle() {
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
    fn read_write_via_write_handle() {
        let _test_file = TestFile::new(std::env::current_dir().unwrap().join("Cargo2.toml"));

        let fs = FsImpl::new(std::env::current_dir().unwrap());
        let mut file = fs.open_write("Cargo2.toml").unwrap();
        let content = b"content";

        let written = file.write(content).unwrap();
        assert_eq!(written, content.len());

        let len = file.seek(SeekFrom::End(0)).unwrap() as usize;
        assert_eq!(len, content.len());

        file.seek(SeekFrom::Start(0)).unwrap();
        let mut read_content = vec![0_u8; len];
        let read = file.read(&mut read_content).unwrap();
        assert_eq!(read, len);

        drop(file);

        let mut file = fs
            .open_write_options("Cargo2.toml", OpenWriteOptions::new().append())
            .unwrap();
        let written = file.write(content).unwrap();
        assert_eq!(written, content.len());
        let current = file.stream_position().unwrap() as usize;
        assert_eq!(current, 2 * content.len());
        file.seek(SeekFrom::Start(0)).unwrap();
        let written = file.write(content).unwrap();
        assert_eq!(written, content.len());
        let current = file.stream_position().unwrap() as usize;
        assert_eq!(current, 3 * written);

        drop(file);

        let mut file = fs
            .open_write_options("Cargo2.toml", OpenWriteOptions::new().truncate())
            .unwrap();
        let len = file.seek(SeekFrom::End(0)).unwrap() as usize;
        assert_eq!(len, 0);

        drop(file);

        assert!(
            fs.open_write_options("Cargo2.toml", OpenWriteOptions::new().create_new())
                .is_err(),
            "Opening 'Cargo2.toml' with 'CreateNew' should fail because it already exists"
        );
    }
}
