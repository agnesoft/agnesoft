use std::collections::BTreeMap;
use std::io::Cursor;
use std::io::Read;
use std::io::Seek;
use std::io::Write;
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
    pub content: Arc<RwLock<Cursor<Vec<u8>>>>,
    pub state: BorrowState,
}

pub struct TestFsDir {}

pub enum TestFsEntry {
    File(TestFsFile),
    Dir(TestFsDir),
}

/// The test file system is a memory-only but fully functional mock
/// implementation of a file system interface. It is used in tests to be
/// passed where `impl Fs` is expected.
pub struct TestFs {
    current_dir: PathBuf,
    fs: Arc<Mutex<BTreeMap<PathBuf, TestFsEntry>>>,
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
        }
    }
}

impl CurrentDirSetter for TestFs {
    fn set_current_dir(&mut self, path: std::path::PathBuf) {
        self.current_dir = path;
    }
}

struct TestFileHandle {
    fs: Arc<Mutex<BTreeMap<PathBuf, TestFsEntry>>>,
    path: PathBuf,
    content: Arc<RwLock<Cursor<Vec<u8>>>>,
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

pub struct TestFileRead(TestFileHandle);

impl Read for TestFileRead {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut cursor = self.0.content.write().unwrap();
        cursor.read(buf)
    }
}

impl Seek for TestFileRead {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        let mut cursor = self.0.content.write().unwrap();
        cursor.seek(pos)
    }
}

pub struct TestFileWrite {
    handle: TestFileHandle,
    append: bool,
}

impl Read for TestFileWrite {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut cursor = self.handle.content.write().unwrap();
        cursor.read(buf)
    }
}

impl Seek for TestFileWrite {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        let mut cursor = self.handle.content.write().unwrap();
        cursor.seek(pos)
    }
}

impl Write for TestFileWrite {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut cursor = self.handle.content.write().unwrap();
        if self.append {
            cursor.seek(std::io::SeekFrom::End(0))?;
        }
        cursor.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut cursor = self.handle.content.write().unwrap();
        cursor.flush()
    }
}

impl Fs for TestFs {
    type ReadHandle = TestFileRead;
    type WriteHandle = TestFileWrite;

    fn open_read<P: AsRef<std::path::Path>>(&self, path: P) -> Result<Self::ReadHandle> {
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
                        "File could not be opened because it is already borrowed for writing: {}",
                        normalized_path.display()
                    )));
                }
            }

            return Ok(TestFileRead(TestFileHandle {
                fs: self.fs.clone(),
                path: normalized_path,
                content: file.content.clone(),
            }));
        }

        Err(Error::fs(format!(
            "File could not be opened, file not found: {}",
            normalized_path.display()
        )))
    }

    fn open_write_options<P: AsRef<std::path::Path>>(
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

            if let BorrowState::Write = file.state {
                return Err(Error::fs(format!(
                    "File '{}' is already borrowed for writing",
                    normalized_path.display()
                )));
            }

            if let BorrowState::Read(count) = file.state {
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
            },
            append: false,
        };

        fs.insert(normalized_path, TestFsEntry::File(new_file_entry));

        Ok(file)
    }
}
