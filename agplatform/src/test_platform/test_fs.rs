use std::collections::BTreeMap;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::ops::Bound;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;

use crate::Error;
use crate::Fs;
use crate::Result;
use crate::fs::BorrowState;
use crate::fs::DirEntry;
use crate::fs::FileType;
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

/// The root (`/`, or `""` for relative paths when the current directory is
/// empty) has no entry of its own but always exists as a directory.
fn is_root(path: &Path) -> bool {
    path.parent().is_none()
}

fn is_dir(fs: &BTreeMap<PathBuf, TestFsEntry>, path: &Path) -> bool {
    is_root(path) || matches!(fs.get(path), Some(TestFsEntry::Dir(_)))
}

impl Fs for TestFs {
    type ReadHandle = TestFileRead;
    type WriteHandle = TestFileWrite;
    type Dir = std::vec::IntoIter<DirEntry>;

    fn open_read<P: AsRef<Path>>(&self, path: P) -> Result<Self::ReadHandle> {
        let normalized_path = normalize_path(&self.current_dir, path);
        let mut fs = lock_handles(&self.fs);

        if is_dir(&fs, &normalized_path) {
            return Err(Error::fs(format!(
                "File could not be opened, path is a directory: {}",
                normalized_path.display()
            )));
        }

        if let Some(TestFsEntry::File(file)) = fs.get_mut(&normalized_path) {
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

        if is_dir(&fs, &normalized_path) {
            return Err(Error::fs(format!(
                "File could not be opened, path is a directory: {}",
                normalized_path.display()
            )));
        }

        if let Some(TestFsEntry::File(file)) = fs.get_mut(&normalized_path) {
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

        if !normalized_path
            .parent()
            .is_some_and(|parent| is_dir(&fs, parent))
        {
            return Err(Error::fs(format!(
                "File could not be created, parent directory does not exist: {}",
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

    fn read_dir<P: AsRef<Path>>(&self, path: P) -> Result<Self::Dir> {
        let normalized_path = normalize_path(&self.current_dir, path);
        let fs = lock_handles(&self.fs);

        match fs.get(&normalized_path) {
            Some(TestFsEntry::File(_)) => {
                return Err(Error::fs(format!(
                    "Not a directory: {}",
                    normalized_path.display()
                )));
            }
            None if !is_root(&normalized_path) => {
                return Err(Error::fs(format!(
                    "Directory not found: {}",
                    normalized_path.display()
                )));
            }
            _ => {}
        }

        // Descendants of a path sort directly after it, so this visits only
        // the directory's subtree rather than the whole map.
        let entries: Vec<DirEntry> = fs
            .range::<Path, _>((Bound::Excluded(normalized_path.as_path()), Bound::Unbounded))
            .take_while(|(path, _)| path.starts_with(&normalized_path))
            .filter(|(path, _)| path.parent() == Some(&normalized_path))
            .filter_map(|(path, entry)| {
                DirEntry::new(
                    path.clone(),
                    match entry {
                        TestFsEntry::Dir(_) => FileType::Dir,
                        TestFsEntry::File(_) => FileType::File,
                    },
                )
            })
            .collect();

        Ok(entries.into_iter())
    }

    fn create_dir_all<P: AsRef<Path>>(&self, path: P) -> Result {
        let normalized_path = normalize_path(&self.current_dir, path);
        let mut fs = lock_handles(&self.fs);
        let dirs: Vec<&Path> = normalized_path
            .ancestors()
            .filter(|dir| !is_root(dir))
            .collect();

        if let Some(file) = dirs
            .iter()
            .find(|dir| matches!(fs.get(**dir), Some(TestFsEntry::File(_))))
        {
            return Err(Error::fs(format!(
                "Cannot create directory, a file already exists at: {}",
                file.display()
            )));
        }

        for dir in dirs {
            fs.entry(dir.to_path_buf())
                .or_insert(TestFsEntry::Dir(TestFsDir {}));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::SeekFrom;

    mod test_fs {
        use super::*;

        #[test]
        fn test_fs_should_enforce_one_writer_or_many_readers() {
            let mut fs = TestFs::new();
            fs.set_current_dir("/tmp".into());
            fs.create_dir_all("/tmp").unwrap();
            fs.write("Cargo.toml", "project = \"agplatform\"").unwrap();
            let normalized_path = normalize_path(&fs.current_dir, "Cargo.toml");

            // GIVEN: two read handles are open
            let read1 = fs.open_read("Cargo.toml").unwrap();
            let read2 = fs.open_read("Cargo.toml").unwrap();

            // THEN: write fails because file is read-borrowed twice
            let err = fs.open_write("Cargo.toml").err().unwrap().to_string();
            assert_eq!(
                err,
                Error::fs(format!(
                    "File could not be opened, it is already borrowed for reading 2 times: {}",
                    normalized_path.display()
                ))
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
                Error::fs(format!(
                    "File could not be opened, it is already borrowed for writing: {}",
                    normalized_path.display()
                ))
                .to_string()
            );

            // AND: second write also fails
            let err = fs.open_write("Cargo.toml").err().unwrap().to_string();
            assert_eq!(
                err,
                Error::fs(format!(
                    "File could not be opened, it is already borrowed for writing: {}",
                    normalized_path.display()
                ))
                .to_string()
            );

            // AND: convenience methods also fail while write-borrowed
            assert!(fs.read("Cargo.toml").is_err());
            assert!(fs.write("Cargo.toml", "data").is_err());

            // WHEN: write is dropped
            drop(write_handle);

            // THEN: read succeeds again
            let _read = fs.open_read("Cargo.toml").unwrap();
        }

        mod open_read {
            use super::*;

            #[test]
            fn open_read_should_return_readable_handle() {
                let fs = TestFs::new();
                fs.write("/test.txt", "project = \"agplatform\"").unwrap();

                let mut file = fs.open_read("/test.txt").unwrap();
                let len = file.seek(SeekFrom::End(0)).unwrap() as usize;
                assert!(len > 0);
                file.seek(SeekFrom::Start(0)).unwrap();
                let mut content = vec![0_u8; len];
                let read = file.read(&mut content).unwrap();
                assert_eq!(read, len);
                let mut zero_buffer = vec![];
                let read_zero = file.read(&mut zero_buffer).unwrap();
                assert_eq!(read_zero, 0);
            }

            #[test]
            fn open_read_should_fail_for_nonexistent_file() {
                let fs = TestFs::new();

                assert!(fs.open_read("/nonexistent.txt").is_err());
            }

            #[test]
            fn open_read_dir_should_fail() {
                let fs = TestFs::new();
                fs.create_dir_all("/mydir").unwrap();

                assert_eq!(
                    fs.open_read("/mydir").err().unwrap().to_string(),
                    Error::fs(format!(
                        "File could not be opened, path is a directory: {}",
                        normalize_path(&fs.current_dir, "/mydir").display()
                    ))
                    .to_string()
                );
            }

            #[test]
            fn open_read_should_fail_when_file_is_write_borrowed() {
                let fs = TestFs::new();
                fs.write("/test.txt", "content").unwrap();

                let _write_handle = fs.open_write("/test.txt").unwrap();

                assert!(fs.open_read("/test.txt").is_err());
            }

            #[test]
            fn open_read_should_provide_independent_cursors() {
                let fs = TestFs::new();
                fs.write("/test.txt", "content").unwrap();

                let mut cursor1 = fs.open_read("/test.txt").unwrap();
                let mut cursor2 = fs.open_read("/test.txt").unwrap();

                let mut buffer1 = vec![0_u8; 7];
                let mut buffer2 = vec![0_u8; 7];

                let len1 = cursor1.read(&mut buffer1).unwrap();
                let len2 = cursor2.read(&mut buffer2).unwrap();

                assert_eq!(len1, len2);
                assert_eq!(buffer1, buffer2);
            }
        }

        mod open_write {
            use super::*;

            #[test]
            fn open_write_should_return_writable_handle_to_new_or_existent_file() {
                let fs = TestFs::new();
                let content = b"content";

                let mut file = fs.open_write("/test.txt").unwrap();
                let written = file.write(content).unwrap();
                assert_eq!(written, content.len());

                drop(file);

                let mut file = fs.open_write("/test.txt").unwrap();
                let len = file.seek(SeekFrom::End(0)).unwrap() as usize;
                assert_eq!(len, content.len());

                file.seek(SeekFrom::Start(0)).unwrap();
                let mut read_content = vec![0_u8; len];
                let read = file.read(&mut read_content).unwrap();
                assert_eq!(read, len);
                assert_eq!(read_content, content);
            }

            #[test]
            fn open_write_should_write_at_arbitrary_offset() {
                let fs = TestFs::new();
                fs.write("/test.txt", "content").unwrap();

                let mut write_handle = fs.open_write("/test.txt").unwrap();
                write_handle.seek(SeekFrom::Start(1)).unwrap();
                let _ = write_handle.write(&[b'a'; 1]).unwrap();
                drop(write_handle);

                let content = fs.read("/test.txt").unwrap();
                assert_eq!(content, b"cantent");
            }
        }

        mod open_write_options {
            use super::*;

            #[test]
            fn open_write_options_append_should_only_append_to_existing_content() {
                let fs = TestFs::new();
                let content = b"content";
                fs.write("/test.txt", "content").unwrap();

                let mut file = fs
                    .open_write_options("/test.txt", OpenWriteOptions::new().append())
                    .unwrap();

                // Write in append mode always seeks to end regardless of previous seek position
                file.seek(SeekFrom::Start(0)).unwrap();
                let written = file.write(content).unwrap();
                assert_eq!(written, content.len());
                let current = file.stream_position().unwrap() as usize;
                assert_eq!(current, 2 * written);

                file.flush().unwrap();
            }

            #[test]
            fn open_write_options_truncate_should_clear_existing_content() {
                let fs = TestFs::new();
                fs.write("/test.txt", "content").unwrap();

                let mut file = fs
                    .open_write_options("/test.txt", OpenWriteOptions::new().truncate())
                    .unwrap();
                let len = file.seek(SeekFrom::End(0)).unwrap() as usize;
                assert_eq!(len, 0);
            }

            #[test]
            fn open_write_options_create_new_should_fail_on_existing_file() {
                let fs = TestFs::new();
                fs.write("/test.txt", "content").unwrap();

                assert!(
                    fs.open_write_options("/test.txt", OpenWriteOptions::new().create_new())
                        .is_err()
                );
            }

            #[test]
            fn open_write_options_append_should_fail_on_nonexistent_file() {
                let fs = TestFs::new();

                assert!(
                    fs.open_write_options("/test.txt", OpenWriteOptions::new().append())
                        .is_err()
                );
            }

            #[test]
            fn open_write_options_create_new_should_create_new_file() {
                let fs = TestFs::new();

                assert!(
                    fs.open_write_options("/test.txt", OpenWriteOptions::new().create_new())
                        .is_ok()
                );
            }

            #[test]
            fn open_write_options_dir_should_fail_and_keep_dir() {
                let fs = TestFs::new();
                fs.create_dir_all("/mydir").unwrap();
                fs.write("/mydir/a.txt", "aaa").unwrap();

                assert_eq!(
                    fs.open_write_options("/mydir", OpenWriteOptions::new().create())
                        .err()
                        .unwrap()
                        .to_string(),
                    Error::fs(format!(
                        "File could not be opened, path is a directory: {}",
                        normalize_path(&fs.current_dir, "/mydir").display()
                    ))
                    .to_string()
                );

                let names: Vec<String> = fs
                    .read_dir("/mydir")
                    .unwrap()
                    .map(|e| e.file_name().to_string())
                    .collect();
                assert_eq!(names, vec!["a.txt"]);
            }

            #[test]
            fn open_write_options_without_parent_dir_should_fail() {
                let fs = TestFs::new();
                fs.write("/file", "aaa").unwrap();

                for path in ["/missing/a.txt", "/file/a.txt"] {
                    assert_eq!(
                        fs.open_write_options(path, OpenWriteOptions::new().create())
                            .err()
                            .unwrap()
                            .to_string(),
                        Error::fs(format!(
                            "File could not be created, parent directory does not exist: {}",
                            normalize_path(&fs.current_dir, path).display()
                        ))
                        .to_string(),
                        "path: {path}"
                    );
                }
            }

            #[test]
            fn open_write_options_should_reject_append_with_truncate() {
                let fs = TestFs::new();

                assert!(
                    fs.open_write_options("/a.txt", OpenWriteOptions::new().append().truncate())
                        .is_err()
                );
                assert!(
                    fs.open_write_options(
                        "/b.txt",
                        OpenWriteOptions::new().append().truncate().create()
                    )
                    .is_err()
                );
                assert!(
                    fs.open_write_options(
                        "/c.txt",
                        OpenWriteOptions::new().append().truncate().create_new()
                    )
                    .is_ok()
                );
                assert!(
                    fs.open_write_options(
                        "/c.txt",
                        OpenWriteOptions::new().append().truncate().create_new()
                    )
                    .is_err()
                );
                assert!(
                    fs.open_write_options("/c.txt", OpenWriteOptions::new().append().truncate())
                        .is_err()
                );
            }
        }

        mod set_fail_mode {
            use super::*;

            #[test]
            fn set_fail_mode_read_should_cause_read_failure() {
                let mut fs = TestFs::new();
                fs.write("/test.txt", "content").unwrap();
                fs.set_fail_mode(Some(FailMode::Read));

                let mut buffer = vec![0_u8; 7];
                assert!(
                    fs.open_read("/test.txt")
                        .unwrap()
                        .read(&mut buffer)
                        .is_err()
                );

                let mut buffer = vec![0_u8; 7];
                assert!(
                    fs.open_write("/test.txt")
                        .unwrap()
                        .read(&mut buffer)
                        .is_err()
                );

                assert!(fs.read("/test.txt").is_err());
            }

            #[test]
            fn set_fail_mode_seek_should_cause_seek_failure() {
                let mut fs = TestFs::new();
                fs.write("/test.txt", "content").unwrap();

                fs.set_fail_mode(Some(FailMode::Seek(SeekFrom::Start(0))));
                assert!(
                    fs.open_read("/test.txt")
                        .unwrap()
                        .seek(SeekFrom::Start(0))
                        .is_err()
                );

                fs.set_fail_mode(Some(FailMode::Seek(SeekFrom::End(0))));
                assert!(
                    fs.open_read("/test.txt")
                        .unwrap()
                        .seek(SeekFrom::End(0))
                        .is_err()
                );

                fs.set_fail_mode(Some(FailMode::Seek(SeekFrom::Start(0))));
                assert!(
                    fs.open_write("/test.txt")
                        .unwrap()
                        .seek(SeekFrom::Start(0))
                        .is_err()
                );

                fs.set_fail_mode(Some(FailMode::Seek(SeekFrom::End(0))));
                assert!(
                    fs.open_write("/test.txt")
                        .unwrap()
                        .seek(SeekFrom::End(0))
                        .is_err()
                );
            }

            #[test]
            fn set_fail_mode_write_should_cause_write_failure() {
                let mut fs = TestFs::new();
                fs.write("/test.txt", "content").unwrap();

                fs.set_fail_mode(Some(FailMode::Write));
                assert!(fs.write("/test.txt", "content").is_err());

                fs.set_fail_mode(None);
                assert!(fs.read("/test.txt").unwrap().is_empty());
            }

            #[test]
            fn set_fail_mode_flush_should_cause_flush_failure() {
                let mut fs = TestFs::new();
                fs.write("/test.txt", "content").unwrap();

                fs.set_fail_mode(Some(FailMode::Flush));
                assert!(fs.open_write("/test.txt").unwrap().flush().is_err());
            }
        }

        mod read_dir {
            use super::*;

            #[test]
            fn read_dir_should_list_files() {
                let fs = TestFs::new();
                fs.create_dir_all("/mydir").unwrap();
                fs.write("/mydir/a.txt", "aaa").unwrap();
                fs.write("/mydir/b.txt", "bbb").unwrap();

                let mut names: Vec<String> = fs
                    .read_dir("/mydir")
                    .unwrap()
                    .map(|e| e.file_name().to_string())
                    .collect();
                names.sort();
                assert_eq!(names, vec!["a.txt", "b.txt"]);
            }

            #[test]
            fn read_dir_should_distinguish_files_and_directories() {
                let fs = TestFs::new();
                fs.create_dir_all("/mydir/subdir").unwrap();
                fs.write("/mydir/file.txt", "data").unwrap();

                let entries: Vec<DirEntry> = fs.read_dir("/mydir").unwrap().collect();

                let file = entries
                    .iter()
                    .find(|e| e.file_name() == "file.txt")
                    .unwrap();
                assert!(file.file_type().is_file(), "expected file.txt to be a file");
                assert_eq!(file.path(), Path::new("/mydir/file.txt"));

                let dir = entries.iter().find(|e| e.file_name() == "subdir").unwrap();
                assert!(
                    dir.file_type().is_dir(),
                    "expected subdir to be a directory"
                );
                assert_eq!(dir.path(), Path::new("/mydir/subdir"));
            }

            #[test]
            fn read_dir_should_not_include_nested_entries() {
                let fs = TestFs::new();
                fs.create_dir_all("/mydir/sub").unwrap();
                fs.write("/mydir/a.txt", "aaa").unwrap();
                fs.write("/mydir/sub/b.txt", "bbb").unwrap();

                let mut names: Vec<String> = fs
                    .read_dir("/mydir")
                    .unwrap()
                    .map(|e| e.file_name().to_string())
                    .collect();
                names.sort();
                assert_eq!(names, vec!["a.txt", "sub"]);
            }

            #[test]
            fn read_dir_should_return_empty_for_empty_directory() {
                let fs = TestFs::new();
                fs.create_dir_all("/emptydir").unwrap();

                let entries: Vec<DirEntry> = fs.read_dir("/emptydir").unwrap().collect();
                assert!(entries.is_empty());
            }

            #[test]
            fn read_dir_root_should_list_top_level_entries() {
                let fs = TestFs::new();
                fs.write("/a.txt", "aaa").unwrap();
                fs.create_dir_all("/sub").unwrap();

                let mut names: Vec<String> = fs
                    .read_dir("/")
                    .unwrap()
                    .map(|e| e.file_name().to_string())
                    .collect();
                names.sort();
                assert_eq!(names, vec!["a.txt", "sub"]);
            }

            #[test]
            fn read_dir_should_fail_for_nonexistent_path() {
                let fs = TestFs::new();
                assert!(fs.read_dir("/nonexistent").is_err());
            }

            #[test]
            fn read_dir_should_skip_non_utf8_names() {
                #[cfg(unix)]
                let name = {
                    use std::os::unix::ffi::OsStrExt;
                    std::ffi::OsStr::from_bytes(b"\xff.txt").to_os_string()
                };
                // A lone UTF-16 surrogate is a valid Windows name but not UTF-8.
                #[cfg(windows)]
                let name = {
                    use std::os::windows::ffi::OsStringExt;
                    std::ffi::OsString::from_wide(&[0xD800])
                };

                let fs = TestFs::new();
                fs.write("/a.txt", "aaa").unwrap();
                // Inserted directly: passing a `&Path` to `write` would add a
                // second instantiation of every generic method to cover.
                lock_handles(&fs.fs).insert(
                    normalize_path(&fs.current_dir, "/").join(name),
                    TestFsEntry::Dir(TestFsDir {}),
                );

                let names: Vec<String> = fs
                    .read_dir("/")
                    .unwrap()
                    .map(|e| e.file_name().to_string())
                    .collect();
                assert_eq!(names, vec!["a.txt"]);
            }

            #[test]
            fn read_dir_file_should_fail() {
                let fs = TestFs::new();
                fs.write("/a.txt", "aaa").unwrap();

                assert_eq!(
                    fs.read_dir("/a.txt").err().unwrap().to_string(),
                    Error::fs(format!(
                        "Not a directory: {}",
                        normalize_path(&fs.current_dir, "/a.txt").display()
                    ))
                    .to_string()
                );
            }
        }

        mod create_dir_all {
            use super::*;

            #[test]
            fn create_dir_all_should_create_directory_and_parents() {
                let fs = TestFs::new();
                fs.create_dir_all("/a/b/c").unwrap();

                let entries: Vec<DirEntry> = fs.read_dir("/a").unwrap().collect();
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].file_name(), "b");
                assert!(
                    entries[0].file_type().is_dir(),
                    "expected 'b' to be a directory"
                );

                let entries: Vec<DirEntry> = fs.read_dir("/a/b").unwrap().collect();
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].file_name(), "c");

                let entries: Vec<DirEntry> = fs.read_dir("/a/b/c").unwrap().collect();
                assert!(entries.is_empty());
            }

            #[test]
            fn create_dir_all_should_succeed_if_directory_already_exists() {
                let fs = TestFs::new();
                fs.create_dir_all("/mydir").unwrap();
                fs.create_dir_all("/mydir").unwrap();

                let entries: Vec<DirEntry> = fs.read_dir("/mydir").unwrap().collect();
                assert!(entries.is_empty(), "directory should exist and be empty");
            }

            #[test]
            fn create_dir_all_should_fail_if_file_exists_at_path() {
                let fs = TestFs::new();
                fs.write("/conflict", "data").unwrap();
                assert!(fs.create_dir_all("/conflict").is_err());
            }

            #[test]
            fn create_dir_all_file_as_parent_should_fail() {
                let fs = TestFs::new();
                fs.write("/a", "aaa").unwrap();

                assert_eq!(
                    fs.create_dir_all("/a/b").err().unwrap().to_string(),
                    Error::fs(format!(
                        "Cannot create directory, a file already exists at: {}",
                        normalize_path(&fs.current_dir, "/a").display()
                    ))
                    .to_string()
                );
            }
        }
    }

    mod test_file_handle {
        use super::*;

        mod seek {
            use super::*;

            #[test]
            fn seek_should_fail_for_negative_offset_from_end() {
                let mut handle = TestFileHandle {
                    fs: Arc::new(Mutex::new(BTreeMap::new())),
                    path: PathBuf::from("Cargo.toml"),
                    pos: 0,
                    len: 0,
                    content: Arc::new(RwLock::new(Vec::new())),
                    fail_mode: None,
                };

                assert!(handle.seek(SeekFrom::End(-10)).is_err());
            }

            #[test]
            fn seek_should_fail_for_negative_offset_from_current() {
                let mut handle = TestFileHandle {
                    fs: Arc::new(Mutex::new(BTreeMap::new())),
                    path: PathBuf::from("Cargo.toml"),
                    pos: 0,
                    len: 0,
                    content: Arc::new(RwLock::new(Vec::new())),
                    fail_mode: None,
                };

                assert!(handle.seek(SeekFrom::Current(-10)).is_err());
            }
        }

        mod drop {
            use super::*;

            #[test]
            fn drop_should_not_panic_when_path_not_in_fs() {
                let handle = TestFileHandle {
                    fs: Arc::new(Mutex::new(BTreeMap::new())),
                    path: PathBuf::from("Cargo.toml"),
                    pos: 0,
                    len: 0,
                    content: Arc::new(RwLock::new(Vec::new())),
                    fail_mode: None,
                };

                drop(handle);
            }
        }
    }
}
