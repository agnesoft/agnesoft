use std::collections::BTreeMap;
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

/// An in-memory file entry containing its content and borrow state.
#[derive(Debug, Clone)]
pub struct TestFsFile {
    content: Arc<RwLock<Vec<u8>>>,
    state: BorrowState,
}

/// A directory entry (currently a placeholder for future expansion).
pub struct TestFsDir {}

/// A single entry in the [`TestFs`] filesystem tree.
pub enum TestFsEntry {
    /// A file with in-memory content.
    File(TestFsFile),
    /// A directory.
    Dir(TestFsDir),
}

/// Simulated I/O failure mode for [`TestFs`].
///
/// When set via [`TestFs::set_fail_mode`], all handles opened afterward
/// will fail the specified operation with a synthetic I/O error. This is
/// useful for testing error-handling paths that are impossible to trigger
/// with a real filesystem.
///
/// # Examples
///
/// ```rust
/// use agplatform::Fs;
/// use agplatform::test_platform::test_fs::{TestFs, FailMode};
///
/// let mut fs = TestFs::new();
/// fs.write("/data.txt", "hello").unwrap();
///
/// fs.set_fail_mode(Some(FailMode::Read));
/// assert!(fs.read("/data.txt").is_err());
///
/// fs.set_fail_mode(None);
/// assert_eq!(fs.read_to_string("/data.txt").unwrap(), "hello");
/// ```
#[derive(Debug, Clone, Copy)]
pub enum FailMode {
    /// Fail [`Write::flush`] calls.
    Flush,
    /// Fail [`Read::read`] calls.
    Read,
    /// Fail [`Seek::seek`] calls whose [`SeekFrom`] variant matches
    /// the one stored here.
    Seek(SeekFrom),
    /// Fail [`Write::write`] calls.
    Write,
}

/// A purely in-memory filesystem for testing.
///
/// Implements [`Fs`] with the same borrow-checking semantics as
/// `FsImpl` — one writer or many readers per file —
/// but stores all data in memory. No disk I/O occurs.
///
/// Supports optional failure injection via [`set_fail_mode`](TestFs::set_fail_mode)
/// to exercise error-handling code paths.
///
/// # Examples
///
/// ```rust
/// use agplatform::Fs;
/// use agplatform::test_platform::test_fs::TestFs;
///
/// let fs = TestFs::new();
/// fs.write("/greeting.txt", "hello world").unwrap();
/// assert_eq!(fs.read_to_string("/greeting.txt").unwrap(), "hello world");
/// ```
pub struct TestFs {
    current_dir: PathBuf,
    fs: Arc<Mutex<BTreeMap<PathBuf, TestFsEntry>>>,
    fail_mode: Option<FailMode>,
}

impl TestFs {
    /// Creates an empty in-memory filesystem.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::test_platform::test_fs::TestFs;
    ///
    /// let fs = TestFs::new();
    /// ```
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        TestFs {
            current_dir: PathBuf::new(),
            fs: Arc::new(Mutex::new(BTreeMap::new())),
            fail_mode: None,
        }
    }

    /// Sets the failure injection mode.
    ///
    /// All handles opened after this call will simulate the specified
    /// I/O failure. Pass `None` to restore normal behavior.
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
    pos: u64,
    len: u64,
    content: Arc<RwLock<Vec<u8>>>,
    fail_mode: Option<FailMode>,
}

impl TestFileHandle {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        if let Some(FailMode::Seek(seek_direction)) = self.fail_mode
            && std::mem::discriminant(&pos) == std::mem::discriminant(&seek_direction)
        {
            return Err(std::io::Error::other("simulated seek failure"));
        }

        match pos {
            SeekFrom::Start(offset) => {
                self.pos = offset;
            }
            SeekFrom::End(offset) => {
                let new_pos = self.len as i64 + offset;

                if new_pos < 0 {
                    return Err(std::io::Error::other("seek before start"));
                }

                self.pos = new_pos as u64;
            }
            SeekFrom::Current(offset) => {
                let new_pos = self.pos as i64 + offset;

                if new_pos < 0 {
                    return Err(std::io::Error::other("seek before start"));
                }

                self.pos = new_pos as u64;
            }
        }

        Ok(self.pos)
    }
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

fn lock_content_read(content: &Arc<RwLock<Vec<u8>>>) -> std::sync::RwLockReadGuard<'_, Vec<u8>> {
    content.clear_poison();
    content.read().expect("Failed to lock content for reading")
}

fn lock_content_write(content: &Arc<RwLock<Vec<u8>>>) -> std::sync::RwLockWriteGuard<'_, Vec<u8>> {
    content.clear_poison();
    content.write().expect("Failed to lock content for writing")
}

/// A read-only handle to an in-memory file in [`TestFs`].
///
/// Implements [`Read`] and [`Seek`]. Dropping the handle releases the
/// read borrow.
pub struct TestFileRead(TestFileHandle);

impl Read for TestFileRead {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if matches!(self.0.fail_mode, Some(FailMode::Read)) {
            return Err(std::io::Error::other("simulated read failure"));
        }

        let max_read = self.0.len.saturating_sub(self.0.pos);
        let read_size = std::cmp::min(max_read, buf.len() as u64) as usize;

        if read_size == 0 {
            return Ok(0);
        }

        buf[..read_size].copy_from_slice(
            &lock_content_read(&self.0.content)
                [self.0.pos as usize..self.0.pos as usize + read_size],
        );
        self.0.pos += read_size as u64;
        Ok(read_size)
    }
}

impl Seek for TestFileRead {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.0.seek(pos)
    }
}

/// A read-write handle to an in-memory file in [`TestFs`].
///
/// Implements [`Read`], [`Write`], and [`Seek`]. Dropping the handle
/// releases the write borrow.
pub struct TestFileWrite {
    handle: TestFileRead,
    append: bool,
}

impl Read for TestFileWrite {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.handle.read(buf)
    }
}

impl Seek for TestFileWrite {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.handle.seek(pos)
    }
}

impl Write for TestFileWrite {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if matches!(self.handle.0.fail_mode, Some(FailMode::Write)) {
            return Err(std::io::Error::other("simulated write failure"));
        }

        if self.append {
            self.handle.0.pos = self.handle.0.len;
        }

        let end_pos = self.handle.0.pos + buf.len() as u64;

        {
            let mut content = lock_content_write(&self.handle.0.content);

            if end_pos > content.len() as u64 {
                content.resize(end_pos as usize, 0);
                self.handle.0.len = end_pos;
            }

            content[self.handle.0.pos as usize..end_pos as usize].copy_from_slice(buf);
        }

        self.handle.0.pos = end_pos;

        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if matches!(self.handle.0.fail_mode, Some(FailMode::Flush)) {
            return Err(std::io::Error::other("simulated flush failure"));
        }

        Ok(())
    }
}

impl Fs for TestFs {
    type ReadHandle = TestFileRead;
    type WriteHandle = TestFileWrite;

    fn open_read<P: AsRef<Path>>(&self, path: P) -> Result<Self::ReadHandle> {
        let normalized_path = normalize_path(&self.current_dir, path);
        let mut fs = lock_handles(&self.fs);
        let file = match fs.get_mut(&normalized_path) {
            Some(TestFsEntry::File(f)) => Some(f),
            _ => None,
        };

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
                pos: 0,
                len: lock_content_read(&file.content).len() as u64,
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
        let file = match fs.get_mut(&normalized_path) {
            Some(TestFsEntry::File(f)) => Some(f),
            _ => None,
        };

        if let Some(file) = file {
            if options.create_new {
                return Err(Error::fs(format!(
                    "File could not be opened with 'create_new' option, file already exists: {}",
                    normalized_path.display()
                )));
            }

            if options.append && options.truncate {
                return Err(Error::fs(format!(
                    "Existing file could not be opened with both 'append' and 'truncate' options: {}",
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

            let len = if options.truncate {
                *lock_content_write(&file.content) = vec![];
                0
            } else {
                lock_content_read(&file.content).len() as u64
            };

            return Ok(TestFileWrite {
                handle: TestFileRead(TestFileHandle {
                    fs: self.fs.clone(),
                    path: normalized_path,
                    pos: 0,
                    len,
                    content: file.content.clone(),
                    fail_mode: self.fail_mode,
                }),
                append: options.append,
            });
        }

        if !options.create && !options.create_new {
            return Err(Error::fs(format!(
                "File could not be opened with 'create' or 'create_new' option, file does not exist: {}",
                normalized_path.display()
            )));
        }

        if (options.append && options.truncate) && !options.create_new {
            return Err(Error::fs(format!(
                "File could not be opened with both 'append' and 'truncate' options but without 'create_new' option: {}",
                normalized_path.display()
            )));
        }

        let new_file_entry = TestFsFile {
            content: Arc::new(RwLock::new(Vec::new())),
            state: BorrowState::Write,
        };
        let file = TestFileWrite {
            handle: TestFileRead(TestFileHandle {
                fs: self.fs.clone(),
                path: normalized_path.clone(),
                content: new_file_entry.content.clone(),
                pos: 0,
                len: 0,
                fail_mode: self.fail_mode,
            }),
            append: options.append,
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
        let mut zero_buffer = vec![];
        let read_zero = file.read(&mut zero_buffer).unwrap();
        assert_eq!(read_zero, 0);

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

        let new_file = fs.open_write_options("Cargo5.toml", OpenWriteOptions::new().create_new());

        assert!(
            new_file.is_ok(),
            "Opening 'Cargo5.toml' with 'CreateNew' should succeed as the file does not exist"
        );

        let read_file = fs.open_read("Cargo5.toml");

        assert!(
            read_file.is_err(),
            "Reading 'Cargo5.toml' should fail because it is borrowed for writing"
        );
    }

    #[test]
    fn invalid_file_handle() {
        let mut handle = TestFileHandle {
            fs: Arc::new(Mutex::new(BTreeMap::new())),
            path: PathBuf::from("Cargo.toml"),
            pos: 0,
            len: 0,
            content: Arc::new(RwLock::new(Vec::new())),
            fail_mode: None,
        };
        assert!(
            handle.seek(SeekFrom::End(-10)).is_err(),
            "Seeking to a negative offset from the end should fail"
        );
        assert!(
            handle.seek(SeekFrom::Current(-10)).is_err(),
            "Seeking to a negative offset from the end should fail"
        );
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
        fs.set_fail_mode(Some(FailMode::Flush));

        assert!(fs.open_write("/tmp/test.txt").unwrap().flush().is_err());

        fs.set_fail_mode(Some(FailMode::Seek(SeekFrom::Start(0))));
        assert!(fs.read("/tmp/test.txt").is_err());

        fs.set_fail_mode(Some(FailMode::Seek(SeekFrom::End(0))));
        assert!(fs.read("/tmp/test.txt").is_err());

        fs.set_fail_mode(Some(FailMode::Read));
        assert!(fs.read("/tmp/test.txt").is_err());

        fs.set_fail_mode(Some(FailMode::Write));
        assert!(fs.write("/tmp/test.txt", "content").is_err());

        fs.set_fail_mode(None);
        assert!(
            fs.read("/tmp/test.txt").unwrap().is_empty(),
            "File should be empty after previous write truncates"
        );
    }

    #[test]
    fn independent_file_cursors() {
        let fs = TestFs::new();
        fs.write("/test.txt", "content").unwrap();

        let mut read_cursor1 = fs.open_read("/test.txt").unwrap();
        let mut read_cursor2 = fs.open_read("/test.txt").unwrap();

        let mut buffer1 = vec![0_u8; 7];
        let mut buffer2 = vec![0_u8; 7];

        let len1 = read_cursor1.read(&mut buffer1).unwrap();
        let len2 = read_cursor2.read(&mut buffer2).unwrap();

        assert_eq!(len1, len2);
        assert_eq!(buffer1, buffer2);
    }

    #[test]
    fn write_in_the_middle() {
        let fs = TestFs::new();
        fs.write("/test.txt", "content").unwrap();

        let mut write_cursor = fs.open_write("/test.txt").unwrap();
        write_cursor.seek(SeekFrom::Start(1)).unwrap();
        let _ = write_cursor.write(&[b'a'; 1]).unwrap();
        drop(write_cursor);

        let content = fs.read("/test.txt").unwrap();
        assert_eq!(content, b"cantent");
    }

    #[test]
    fn reject_append_truncate() {
        let fs = TestFs::new();

        assert!(
            fs.open_write_options("Cargo7.toml", OpenWriteOptions::new().append().truncate())
                .is_err()
        );
        assert!(
            fs.open_write_options(
                "Cargo7.toml",
                OpenWriteOptions::new().append().truncate().create()
            )
            .is_err()
        );
        assert!(
            fs.open_write_options(
                "Cargo7.toml",
                OpenWriteOptions::new().append().truncate().create_new()
            )
            .is_ok()
        );
        assert!(
            fs.open_write_options(
                "Cargo7.toml",
                OpenWriteOptions::new().append().truncate().create_new()
            )
            .is_err()
        );
        assert!(
            fs.open_write_options("Cargo7.toml", OpenWriteOptions::new().append().truncate())
                .is_err()
        );
    }
}
