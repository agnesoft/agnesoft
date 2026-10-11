use std::collections::BTreeMap;
use std::io::Cursor;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;

use crate::Error;
use crate::Fs;
use crate::Result;
use crate::fs::BorrowState;
use crate::fs::OpenWriteOptions;
use crate::fs::lock_handles;
use crate::fs::normalize_path;
use crate::platform::platform_env::CurrentDirSetter;

#[derive(Debug, Clone)]
pub struct TestFsFile {
    content: Arc<RwLock<Cursor<Vec<u8>>>>,
    state: BorrowState,
}

pub struct TestFsDir {}

pub enum TestFsEntry {
    File(TestFsFile),
    Dir(TestFsDir),
}

#[derive(Debug, Clone, Copy)]
pub enum FailMode {
    Flush,
    Read,
    Seek(SeekFrom),
    Write,
}

/// The test file system is a memory-only but fully functional mock
/// implementation of a file system interface. It is used in tests to be
/// passed where `impl Fs` is expected.
pub struct TestFs {
    current_dir: PathBuf,
    fs: Arc<Mutex<BTreeMap<PathBuf, TestFsEntry>>>,
    fail_mode: Option<FailMode>,
}

impl TestFs {
    /// Creates a new instance of the test file system.
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::test_platform::test_fs::TestFs;
    ///
    /// let test_fs = TestFs::new();
    /// ```
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        TestFs {
            current_dir: PathBuf::new(),
            fs: Arc::new(Mutex::new(BTreeMap::new())),
            fail_mode: None,
        }
    }

    pub fn set_fail_mode(&mut self, fail_mode: Option<FailMode>) {
        self.fail_mode = fail_mode;
    }
}

impl CurrentDirSetter for TestFs {
    fn set_current_dir(&mut self, path: PathBuf) {
        self.current_dir = path;
    }
}

struct TestFileHandle {
    fs: Arc<Mutex<BTreeMap<PathBuf, TestFsEntry>>>,
    path: PathBuf,
    content: Arc<RwLock<Cursor<Vec<u8>>>>,
    fail_mode: Option<FailMode>,
}

impl Drop for TestFileHandle {
    fn drop(&mut self) {
        let mut fs = lock_handles(&self.fs);

        if let Some(entry) = fs.get_mut(&self.path)
            && let TestFsEntry::File(f) = entry
        {
            match &mut f.state {
                BorrowState::Read(count) => {
                    *count = count.saturating_sub(1);
                }
                BorrowState::Write => {
                    f.state = BorrowState::Read(0);
                }
            }
        }
    }
}

fn lock_content(
    content: &Arc<RwLock<Cursor<Vec<u8>>>>,
) -> std::sync::RwLockWriteGuard<'_, Cursor<Vec<u8>>> {
    content.clear_poison();
    content.write().expect("Failed to lock content for writing")
}

pub struct TestFileRead(TestFileHandle);

impl Read for TestFileRead {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if matches!(self.0.fail_mode, Some(FailMode::Read)) {
            return Err(std::io::Error::other("simulated read failure"));
        }

        let mut cursor = lock_content(&self.0.content);
        cursor.read(buf)
    }
}

impl Seek for TestFileRead {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        if let Some(FailMode::Seek(seek_direction)) = self.0.fail_mode
            && std::mem::discriminant(&pos) == std::mem::discriminant(&seek_direction)
        {
            return Err(std::io::Error::other("simulated seek failure"));
        }

        let mut cursor = lock_content(&self.0.content);
        cursor.seek(pos)
    }
}

pub struct TestFileWrite {
    handle: TestFileHandle,
    append: bool,
}

impl Read for TestFileWrite {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if matches!(self.handle.fail_mode, Some(FailMode::Read)) {
            return Err(std::io::Error::other("simulated read failure"));
        }

        let mut cursor = lock_content(&self.handle.content);
        cursor.read(buf)
    }
}

impl Seek for TestFileWrite {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        if let Some(FailMode::Seek(seek_direction)) = self.handle.fail_mode
            && std::mem::discriminant(&pos) == std::mem::discriminant(&seek_direction)
        {
            return Err(std::io::Error::other("simulated seek failure"));
        }

        let mut cursor = lock_content(&self.handle.content);
        cursor.seek(pos)
    }
}

impl Write for TestFileWrite {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if matches!(self.handle.fail_mode, Some(FailMode::Write)) {
            return Err(std::io::Error::other("simulated write failure"));
        }

        let mut cursor = lock_content(&self.handle.content);

        if self.append {
            cursor
                .seek(std::io::SeekFrom::End(0))
                .expect("Cursor seek in memory should not fail");
        }

        cursor.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if matches!(self.handle.fail_mode, Some(FailMode::Flush)) {
            return Err(std::io::Error::other("simulated flush failure"));
        }

        let mut cursor = lock_content(&self.handle.content);
        cursor.flush()
    }
}

impl Fs for TestFs {
    type ReadHandle = TestFileRead;
    type WriteHandle = TestFileWrite;

    fn open_read<P: AsRef<Path>>(&self, path: P) -> Result<Self::ReadHandle> {
        let normalized_path = normalize_path(&self.current_dir, path);
        let mut fs = lock_handles(&self.fs);
        let file = fs.iter_mut().find_map(|(entry_path, entry)| {
            if let TestFsEntry::File(f) = entry
                && **entry_path == normalized_path
            {
                Some(f)
            } else {
                None
            }
        });

        if let Some(file) = file {
            match &mut file.state {
                BorrowState::Read(count) => {
                    *count += 1;
                }
                BorrowState::Write => {
                    return Err(Error::fs(format!(
                        "File could not be opened, it is already borrowed for writing: {}",
                        normalized_path.display()
                    )));
                }
            }

            return Ok(TestFileRead(TestFileHandle {
                fs: self.fs.clone(),
                path: normalized_path,
                content: file.content.clone(),
                fail_mode: self.fail_mode,
            }));
        }

        Err(Error::fs(format!(
            "File could not be opened, file not found: {}",
            normalized_path.display()
        )))
    }

    fn open_write_options<P: AsRef<Path>>(
        &self,
        path: P,
        options: OpenWriteOptions,
    ) -> Result<Self::WriteHandle> {
        let normalized_path = normalize_path(&self.current_dir, path);
        let mut fs = lock_handles(&self.fs);
        let file = fs.iter_mut().find_map(|(entry_path, entry)| {
            if let TestFsEntry::File(f) = entry
                && **entry_path == normalized_path
            {
                Some(f)
            } else {
                None
            }
        });

        if let Some(file) = file {
            if options.create_new {
                return Err(Error::fs(format!(
                    "File could not be opened with 'create_new' option, file already exists: {}",
                    normalized_path.display()
                )));
            }

            match file.state {
                BorrowState::Read(count) => {
                    if count == 0 {
                        file.state = BorrowState::Write;
                    } else {
                        return Err(Error::fs(format!(
                            "File could not be opened, it is already borrowed for reading {} times: {}",
                            count,
                            normalized_path.display()
                        )));
                    }
                }
                BorrowState::Write => {
                    return Err(Error::fs(format!(
                        "File could not be opened, it is already borrowed for writing: {}",
                        normalized_path.display()
                    )));
                }
            }

            if options.truncate {
                let mut write_guard = file.content.write().unwrap();
                write_guard.set_position(0);
                write_guard.get_mut().clear();
            }

            return Ok(TestFileWrite {
                handle: TestFileHandle {
                    fs: self.fs.clone(),
                    path: normalized_path,
                    content: file.content.clone(),
                    fail_mode: self.fail_mode,
                },
                append: options.append,
            });
        }

        if !options.create {
            return Err(Error::fs(format!(
                "File could not be opened with 'create' option, file does not exist: {}",
                normalized_path.display()
            )));
        }

        let new_file_entry = TestFsFile {
            content: Arc::new(RwLock::new(Cursor::new(vec![]))),
            state: BorrowState::Read(1),
        };
        let file = TestFileWrite {
            handle: TestFileHandle {
                fs: self.fs.clone(),
                path: normalized_path.clone(),
                content: new_file_entry.content.clone(),
                fail_mode: self.fail_mode,
            },
            append: false,
        };

        fs.insert(normalized_path, TestFsEntry::File(new_file_entry));

        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::SeekFrom;

    #[test]
    fn file_borrow_checker() {
        let mut fs = TestFs::new();
        fs.set_current_dir("/tmp".into());
        fs.write("Cargo.toml", "project = \"agplatform\"").unwrap();

        let normalized_path = normalize_path(&fs.current_dir, "Cargo.toml");
        let file = fs.open_read("Cargo.toml").unwrap();
        let same_file_read = fs.open_read("Cargo.toml").unwrap();

        let file_write_err = fs.open_write("Cargo.toml").err().unwrap().to_string();
        assert_eq!(
            file_write_err,
            Error::fs(format!(
                "File could not be opened, it is already borrowed for reading 2 times: {}",
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
                "File could not be opened, it is already borrowed for writing: {}",
                normalized_path.display()
            ))
            .to_string()
        );

        let same_file_write_err = fs.open_write("Cargo.toml").err().unwrap().to_string();
        assert_eq!(
            same_file_write_err,
            Error::fs(format!(
                "File could not be opened, it is already borrowed for writing: {}",
                normalized_path.display()
            ))
            .to_string()
        );

        drop(file_write_ok);

        let _file_read_ok = fs.open_read("Cargo.toml").unwrap();
    }

    #[test]
    fn read_via_read_handle() {
        let mut fs = TestFs::new();
        fs.set_current_dir("/tmp".into());
        fs.write("Cargo.toml", "project = \"agplatform\"").unwrap();

        let mut file = fs.open_read("Cargo.toml").unwrap();
        let len = file.seek(SeekFrom::End(0)).unwrap() as usize;
        assert!(len > 0);
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut content = vec![0_u8; len];
        let read = file.read(&mut content).unwrap();
        assert_eq!(read, len);

        assert!(
            fs.open_read("Cargo3.toml").is_err(),
            "Opening 'Cargo3.toml' for reading should fail because it does not exist"
        );
    }

    #[test]
    fn read_write_via_write_handle() {
        let mut fs = TestFs::new();
        fs.set_current_dir("/tmp".into());
        fs.write("Cargo.toml", "project = \"agplatform\"").unwrap();

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
        file.flush().unwrap();

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

        assert!(
            fs.open_write_options("Cargo4.toml", OpenWriteOptions::new().append())
                .is_err(),
            "Opening 'Cargo4.toml' with just 'Append' should fail because it does not exist"
        );
    }

    #[test]
    fn invalid_file_handle() {
        let handle = TestFileHandle {
            fs: Arc::new(Mutex::new(BTreeMap::new())),
            path: PathBuf::from("Cargo.toml"),
            content: Arc::new(RwLock::new(Cursor::new(Vec::new()))),
            fail_mode: None,
        };
        drop(handle);
    }

    #[test]
    fn simulate_failed_io_operations() {
        let mut fs = TestFs::new();
        const CONTENT: &str = "content";
        fs.write("/tmp/test.txt", CONTENT).unwrap();

        let write_file = fs.open_write("/tmp/test.txt").unwrap();
        assert!(fs.read("/tmp/test.txt").is_err());
        assert!(fs.write("/tmp/test.txt", CONTENT).is_err());
        drop(write_file);

        fs.set_fail_mode(Some(FailMode::Read));
        let mut buffer = vec![0_u8; CONTENT.len()];
        assert!(
            fs.open_read("/tmp/test.txt")
                .unwrap()
                .read(&mut buffer)
                .is_err()
        );
        fs.set_fail_mode(Some(FailMode::Seek(SeekFrom::Start(0))));
        assert!(
            fs.open_read("/tmp/test.txt")
                .unwrap()
                .seek(SeekFrom::Start(0))
                .is_err()
        );
        fs.set_fail_mode(Some(FailMode::Seek(SeekFrom::End(0))));
        assert!(
            fs.open_read("/tmp/test.txt")
                .unwrap()
                .seek(SeekFrom::End(0))
                .is_err()
        );

        fs.set_fail_mode(Some(FailMode::Read));
        let mut buffer = vec![0_u8; CONTENT.len()];
        assert!(
            fs.open_write("/tmp/test.txt")
                .unwrap()
                .read(&mut buffer)
                .is_err()
        );
        fs.set_fail_mode(Some(FailMode::Seek(SeekFrom::Start(0))));
        assert!(
            fs.open_write("/tmp/test.txt")
                .unwrap()
                .seek(SeekFrom::Start(0))
                .is_err()
        );
        fs.set_fail_mode(Some(FailMode::Seek(SeekFrom::End(0))));
        assert!(
            fs.open_write("/tmp/test.txt")
                .unwrap()
                .seek(SeekFrom::End(0))
                .is_err()
        );

        fs.set_fail_mode(Some(FailMode::Seek(SeekFrom::Start(0))));
        assert!(fs.read("/tmp/test.txt").is_err());

        fs.set_fail_mode(Some(FailMode::Seek(SeekFrom::End(0))));
        assert!(fs.read("/tmp/test.txt").is_err());

        fs.set_fail_mode(Some(FailMode::Read));
        assert!(fs.read("/tmp/test.txt").is_err());

        fs.set_fail_mode(Some(FailMode::Write));
        assert!(fs.write("/tmp/test.txt", "content").is_err());

        fs.set_fail_mode(None);
        assert_eq!(fs.read("/tmp/test.txt").unwrap(), CONTENT.as_bytes());
    }
}
