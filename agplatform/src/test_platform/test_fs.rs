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
use std::time::SystemTime;

use crate::Fs;
use crate::Result;
use crate::fs::Attribute;
use crate::fs::BorrowState;
use crate::fs::DirEntry;
use crate::fs::FileType;
use crate::fs::Metadata;
use crate::fs::NativeAttributes;
use crate::fs::OpenWriteOptions;
use crate::fs::lock_handles;
use crate::fs::normalize_path;
use crate::platform::platform_env::CurrentDirSetter;

/// The content of an in-memory file, shared with its open handles.
#[derive(Debug)]
pub struct TestFsContent {
    data: Vec<u8>,
    modified: SystemTime,
}

impl TestFsContent {
    fn new(modified: SystemTime) -> Self {
        Self {
            data: Vec::new(),
            modified,
        }
    }
}

/// An in-memory file entry containing its content, borrow state and
/// metadata.
#[derive(Debug, Clone)]
pub struct TestFsFile {
    content: Arc<RwLock<TestFsContent>>,
    state: BorrowState,
    created: SystemTime,
    attributes: NativeAttributes,
}

/// An in-memory directory entry holding its metadata.
pub struct TestFsDir {
    created: SystemTime,
    modified: SystemTime,
    attributes: NativeAttributes,
}

impl TestFsDir {
    fn new(created: SystemTime) -> Self {
        Self {
            created,
            modified: created,
            attributes: new_dir_attributes(),
        }
    }
}

/// The user and group ID of new entries on Unix.
#[cfg(unix)]
const OWNER_ID: u32 = 1000;

/// The umask new entries are created with on Unix.
#[cfg(unix)]
const UMASK: u32 = 0o022;

/// Returns the attributes of a new file, on Unix with the `mode` of
/// `options` (by default `0o666`), see [`unix_attributes`].
#[cfg(unix)]
fn new_file_attributes(options: &OpenWriteOptions) -> NativeAttributes {
    unix_attributes(options.mode.unwrap_or(0o666))
}

/// Returns the attributes of a new file.
#[cfg(windows)]
fn new_file_attributes(_: &OpenWriteOptions) -> NativeAttributes {
    NativeAttributes::default()
}

/// Returns the attributes of a new directory, on Unix with the mode
/// `0o777`, see [`unix_attributes`].
fn new_dir_attributes() -> NativeAttributes {
    #[cfg(unix)]
    let attributes = unix_attributes(0o777);
    #[cfg(windows)]
    let attributes = NativeAttributes::default();

    attributes
}

/// Returns the attributes of a new Unix entry created with `mode`
/// restricted by [`UMASK`] and owned by [`OWNER_ID`].
#[cfg(unix)]
fn unix_attributes(mode: u32) -> NativeAttributes {
    NativeAttributes {
        mode: mode & 0o7777 & !UMASK,
        uid: OWNER_ID,
        gid: OWNER_ID,
    }
}

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

/// The root directory of a new [`TestFs`].
#[cfg(unix)]
pub const ROOT: &str = "/";
/// The root directory of a new [`TestFs`].
#[cfg(windows)]
pub const ROOT: &str = "C:\\";

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
    /// Creates an in-memory filesystem holding only the [`ROOT`] directory,
    /// which is also the current directory. Other roots, e.g. further
    /// Windows drives, can be added with [`Fs::create_dir_all`].
    ///
    /// On Unix new entries are owned by user and group `1000` and created
    /// as under the umask `0o022`: files `0o644`, directories `0o755`.
    ///
    /// Permissions are stored and reported but not modelled as access
    /// checks: only opening a [`readonly`](crate::Metadata::readonly) file
    /// for writing fails. Other mode bits, the owner and the permissions of
    /// directories have no effect, there is no current user. Use
    /// [`set_fail_mode`](TestFs::set_fail_mode) to test failing
    /// operations.
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
        let root = TestFsEntry::Dir(TestFsDir::new(SystemTime::now()));

        TestFs {
            current_dir: PathBuf::from(ROOT),
            fs: Arc::new(Mutex::new(BTreeMap::from([(PathBuf::from(ROOT), root)]))),
            fail_mode: None,
        }
    }

    /// Sets the creation time of a file or directory as reported by
    /// [`Fs::metadata`]. Unlike [`Fs::set_modified`] this is not
    /// available on real filesystems.
    ///
    /// # Errors
    ///
    /// Returns an error if the path does not exist.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::Fs;
    /// use agplatform::test_platform::test_fs::TestFs;
    /// use std::time::SystemTime;
    ///
    /// let fs = TestFs::new();
    /// fs.write("/data.txt", "hello").unwrap();
    /// fs.set_created("/data.txt", SystemTime::UNIX_EPOCH).unwrap();
    /// assert_eq!(fs.metadata("/data.txt").unwrap().created, SystemTime::UNIX_EPOCH);
    /// ```
    pub fn set_created<P: AsRef<Path>>(&self, path: P, created: SystemTime) -> Result {
        let normalized_path = self.validate_normalize_path(path)?;
        let mut fs = lock_handles(&self.fs);

        match fs.get_mut(&normalized_path) {
            Some(TestFsEntry::File(file)) => file.created = created,
            Some(TestFsEntry::Dir(dir)) => dir.created = created,
            None => {
                return Err(fs_error!(
                    "Creation time could not be set, path not found: {}",
                    normalized_path.display()
                ));
            }
        }

        Ok(())
    }

    /// Sets the owner of a file or directory as reported by
    /// [`Fs::metadata`]. Unlike [`Fs::set_mode`] this is not available on
    /// real filesystems, changing the owner requires privileges.
    ///
    /// Unix only: std does not report owners on Windows.
    ///
    /// # Errors
    ///
    /// Returns an error if the path does not exist.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::Fs;
    /// use agplatform::test_platform::test_fs::TestFs;
    ///
    /// let fs = TestFs::new();
    /// fs.write("/data.txt", "hello").unwrap();
    /// fs.set_owner("/data.txt", 0, 0).unwrap();
    /// assert_eq!(fs.metadata("/data.txt").unwrap().uid, 0);
    /// ```
    #[cfg(unix)]
    pub fn set_owner<P: AsRef<Path>>(&self, path: P, uid: u32, gid: u32) -> Result {
        self.update_attributes(path.as_ref(), "Owner", |attributes| {
            attributes.uid = uid;
            attributes.gid = gid;
        })
    }

    #[cfg(unix)]
    fn update_attributes(
        &self,
        path: &Path,
        what: &str,
        update: impl FnOnce(&mut crate::fs::UnixAttributes),
    ) -> Result {
        let normalized_path = self.validate_normalize_path(path)?;
        let mut fs = lock_handles(&self.fs);

        match fs.get_mut(&normalized_path) {
            Some(TestFsEntry::File(file)) => update(&mut file.attributes),
            Some(TestFsEntry::Dir(dir)) => update(&mut dir.attributes),
            None => {
                return Err(fs_error!(
                    "{what} could not be set, path not found: {}",
                    normalized_path.display()
                ));
            }
        }

        Ok(())
    }

    /// Sets the failure injection mode.
    ///
    /// All handles opened after this call will simulate the specified
    /// I/O failure. Pass `None` to restore normal behavior.
    pub fn set_fail_mode(&mut self, fail_mode: Option<FailMode>) {
        self.fail_mode = fail_mode;
    }

    fn validate_normalize_path<P: AsRef<Path>>(&self, path: P) -> Result<PathBuf> {
        let path = path.as_ref();
        if path.to_string_lossy().contains('\0') {
            return Err(fs_error!("Path contains a NUL byte: {}", path.display()));
        }

        Ok(normalize_path(&self.current_dir, path))
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
    content: Arc<RwLock<TestFsContent>>,
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

fn lock_content_read(
    content: &Arc<RwLock<TestFsContent>>,
) -> std::sync::RwLockReadGuard<'_, TestFsContent> {
    content.clear_poison();
    content.read().expect("Failed to lock content for reading")
}

fn lock_content_write(
    content: &Arc<RwLock<TestFsContent>>,
) -> std::sync::RwLockWriteGuard<'_, TestFsContent> {
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
            &lock_content_read(&self.0.content).data
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

            if end_pos > content.data.len() as u64 {
                content.data.resize(end_pos as usize, 0);
                self.handle.0.len = end_pos;
            }

            content.data[self.handle.0.pos as usize..end_pos as usize].copy_from_slice(buf);
            content.modified = SystemTime::now();
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

fn is_dir(fs: &BTreeMap<PathBuf, TestFsEntry>, path: &Path) -> bool {
    matches!(fs.get(path), Some(TestFsEntry::Dir(_)))
}

/// Adding an entry to a directory updates its modification time.
fn touch_parent(fs: &mut BTreeMap<PathBuf, TestFsEntry>, path: &Path, now: SystemTime) {
    if let Some(TestFsEntry::Dir(dir)) = path.parent().and_then(|parent| fs.get_mut(parent)) {
        dir.modified = now;
    }
}

impl Fs for TestFs {
    type ReadHandle = TestFileRead;
    type WriteHandle = TestFileWrite;
    type Dir = std::vec::IntoIter<DirEntry>;

    fn open_read<P: AsRef<Path>>(&self, path: P) -> Result<Self::ReadHandle> {
        let normalized_path = self.validate_normalize_path(path)?;
        let mut fs = lock_handles(&self.fs);

        if is_dir(&fs, &normalized_path) {
            return Err(fs_error!(
                "File could not be opened, path is a directory: {}",
                normalized_path.display()
            ));
        }

        if let Some(TestFsEntry::File(file)) = fs.get_mut(&normalized_path) {
            match &mut file.state {
                BorrowState::Read(count) => {
                    *count += 1;
                }
                BorrowState::Write => {
                    return Err(fs_error!(
                        "File could not be opened, it is already borrowed for writing: {}",
                        normalized_path.display()
                    ));
                }
            }

            return Ok(TestFileRead(TestFileHandle {
                fs: self.fs.clone(),
                path: normalized_path,
                content: file.content.clone(),
                pos: 0,
                len: lock_content_read(&file.content).data.len() as u64,
                fail_mode: self.fail_mode,
            }));
        }

        Err(fs_error!(
            "File could not be opened, file not found: {}",
            normalized_path.display()
        ))
    }

    fn open_write_options<P: AsRef<Path>>(
        &self,
        path: P,
        options: OpenWriteOptions,
    ) -> Result<Self::WriteHandle> {
        let normalized_path = self.validate_normalize_path(path)?;
        let mut fs = lock_handles(&self.fs);

        if is_dir(&fs, &normalized_path) {
            return Err(fs_error!(
                "File could not be opened, path is a directory: {}",
                normalized_path.display()
            ));
        }

        if let Some(TestFsEntry::File(file)) = fs.get_mut(&normalized_path) {
            if options.create_new {
                return Err(fs_error!(
                    "File could not be opened with 'create_new' option, file already exists: {}",
                    normalized_path.display()
                ));
            }

            if options.append && options.truncate {
                return Err(fs_error!(
                    "Existing file could not be opened with both 'append' and 'truncate' options: {}",
                    normalized_path.display()
                ));
            }

            if file.attributes.attributes().readonly {
                return Err(fs_error!(
                    "File could not be opened, it is read-only: {}",
                    normalized_path.display()
                ));
            }

            match file.state {
                BorrowState::Read(count) => {
                    if count == 0 {
                        file.state = BorrowState::Write;
                    } else {
                        return Err(fs_error!(
                            "File could not be opened, it is already borrowed for reading {count} times: {}",
                            normalized_path.display()
                        ));
                    }
                }
                BorrowState::Write => {
                    return Err(fs_error!(
                        "File could not be opened, it is already borrowed for writing: {}",
                        normalized_path.display()
                    ));
                }
            }

            let len = if options.truncate {
                *lock_content_write(&file.content) = TestFsContent::new(SystemTime::now());
                0
            } else {
                lock_content_read(&file.content).data.len() as u64
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
            return Err(fs_error!(
                "File could not be opened with 'create' or 'create_new' option, file does not exist: {}",
                normalized_path.display()
            ));
        }

        if (options.append && options.truncate) && !options.create_new {
            return Err(fs_error!(
                "File could not be opened with both 'append' and 'truncate' options but without 'create_new' option: {}",
                normalized_path.display()
            ));
        }

        if !normalized_path
            .parent()
            .is_some_and(|parent| is_dir(&fs, parent))
        {
            return Err(fs_error!(
                "File could not be created, parent directory does not exist: {}",
                normalized_path.display()
            ));
        }

        let now = SystemTime::now();
        let new_file_entry = TestFsFile {
            content: Arc::new(RwLock::new(TestFsContent::new(now))),
            state: BorrowState::Write,
            created: now,
            attributes: new_file_attributes(&options),
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

        touch_parent(&mut fs, &normalized_path, now);
        fs.insert(normalized_path, TestFsEntry::File(new_file_entry));

        Ok(file)
    }

    fn read_dir<P: AsRef<Path>>(&self, path: P) -> Result<Self::Dir> {
        let normalized_path = self.validate_normalize_path(path)?;
        let fs = lock_handles(&self.fs);

        match fs.get(&normalized_path) {
            Some(TestFsEntry::File(_)) => {
                return Err(fs_error!("Not a directory: {}", normalized_path.display()));
            }
            None => {
                return Err(fs_error!(
                    "Directory not found: {}",
                    normalized_path.display()
                ));
            }
            Some(TestFsEntry::Dir(_)) => {}
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
        let normalized_path = self.validate_normalize_path(path)?;
        let mut fs = lock_handles(&self.fs);
        let dirs: Vec<&Path> = normalized_path.ancestors().collect();

        if let Some(file) = dirs
            .iter()
            .find(|dir| matches!(fs.get(**dir), Some(TestFsEntry::File(_))))
        {
            return Err(fs_error!(
                "Cannot create directory, a file already exists at: {}",
                file.display()
            ));
        }

        let now = SystemTime::now();

        for dir in dirs.into_iter().rev() {
            if !fs.contains_key(dir) {
                touch_parent(&mut fs, dir, now);
                fs.insert(dir.to_path_buf(), TestFsEntry::Dir(TestFsDir::new(now)));
            }
        }

        Ok(())
    }

    fn exists<P: AsRef<Path>>(&self, path: P) -> Result<bool> {
        let normalized_path = self.validate_normalize_path(path)?;
        let fs = lock_handles(&self.fs);

        Ok(matches!(
            fs.get(&normalized_path),
            Some(TestFsEntry::File(_)) | Some(TestFsEntry::Dir(_))
        ))
    }

    fn metadata<P: AsRef<Path>>(&self, path: P) -> Result<Metadata> {
        let normalized_path = self.validate_normalize_path(path)?;
        let fs = lock_handles(&self.fs);

        let (file_type, len, modified, created, attributes) = match fs.get(&normalized_path) {
            Some(TestFsEntry::File(file)) => {
                let content = lock_content_read(&file.content);
                (
                    FileType::File,
                    content.data.len() as u64,
                    content.modified,
                    file.created,
                    file.attributes,
                )
            }
            Some(TestFsEntry::Dir(dir)) => {
                (FileType::Dir, 0, dir.modified, dir.created, dir.attributes)
            }
            None => {
                return Err(fs_error!(
                    "Metadata could not be read, path not found: {}",
                    normalized_path.display()
                ));
            }
        };

        Ok(Metadata::new(
            &normalized_path,
            file_type,
            len,
            modified,
            created,
            attributes,
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
        let normalized_path = self.validate_normalize_path(path)?;
        let mut fs = lock_handles(&self.fs);

        match fs.get_mut(&normalized_path) {
            Some(TestFsEntry::File(file)) => lock_content_write(&file.content).modified = modified,
            Some(TestFsEntry::Dir(dir)) => dir.modified = modified,
            None => {
                return Err(fs_error!(
                    "Modified time could not be set, path not found: {}",
                    normalized_path.display()
                ));
            }
        }

        Ok(())
    }

    #[cfg(unix)]
    fn set_mode<P: AsRef<Path>>(&self, path: P, mode: u32) -> Result {
        self.update_attributes(path.as_ref(), "Mode", |attributes| {
            attributes.mode = mode & 0o7777;
        })
    }
}

impl TestFs {
    fn set_attribute(&self, path: &Path, attribute: Attribute, value: bool) -> Result {
        let normalized_path = self.validate_normalize_path(path)?;
        let mut fs = lock_handles(&self.fs);

        match fs.get_mut(&normalized_path) {
            #[cfg(unix)]
            Some(TestFsEntry::Dir(_)) if matches!(attribute, Attribute::Executable) => {
                return Err(fs_error!(
                    "Executable attribute cannot be set, path is a directory: {}",
                    normalized_path.display()
                ));
            }
            Some(TestFsEntry::File(file)) => file.attributes.set(attribute, value),
            Some(TestFsEntry::Dir(dir)) => dir.attributes.set(attribute, value),
            None => {
                return Err(fs_error!(
                    "Attributes could not be set, path not found: {}",
                    normalized_path.display()
                ));
            }
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

            // AND: convenience methods also fail while write-borrowed
            assert!(fs.read("Cargo.toml").is_err());
            assert!(fs.write("Cargo.toml", "data").is_err());

            // WHEN: write is dropped
            drop(write_handle);

            // THEN: read succeeds again
            let _read = fs.open_read("Cargo.toml").unwrap();
        }

        mod set_created {
            use super::*;

            #[test]
            fn set_created_should_set_creation_time() {
                let fs = TestFs::new();
                fs.write("/a.txt", "").unwrap();
                fs.create_dir_all("/dir").unwrap();

                for path in ["/a.txt", "/dir", "/"] {
                    fs.set_created(path, SystemTime::UNIX_EPOCH).unwrap();
                    assert_eq!(
                        fs.metadata(path).unwrap().created,
                        SystemTime::UNIX_EPOCH,
                        "path: {path}"
                    );
                }
            }

            #[test]
            fn set_created_should_fail_for_nonexistent_path() {
                let fs = TestFs::new();
                assert_eq!(
                    fs.set_created("/a.txt", SystemTime::UNIX_EPOCH)
                        .err()
                        .unwrap()
                        .to_string(),
                    fs_error!(
                        "Creation time could not be set, path not found: {}",
                        normalize_path(&fs.current_dir, "/a.txt").display()
                    )
                    .to_string()
                );
            }

            #[test]
            fn set_created_invalid_path_should_fail() {
                let fs = TestFs::new();
                assert!(fs.set_created("\0", SystemTime::UNIX_EPOCH).is_err());
            }
        }

        #[cfg(unix)]
        mod set_owner {
            use super::*;

            #[test]
            fn set_owner_should_set_user_and_group() {
                let fs = TestFs::new();
                fs.write("/a.txt", "").unwrap();
                fs.create_dir_all("/dir").unwrap();

                for path in ["/a.txt", "/dir"] {
                    fs.set_owner(path, 0, 5).unwrap();
                    let metadata = fs.metadata(path).unwrap();
                    assert_eq!((metadata.uid, metadata.gid), (0, 5), "path: {path}");
                }
            }

            #[test]
            fn set_owner_should_fail_for_nonexistent_path() {
                let fs = TestFs::new();
                assert_eq!(
                    fs.set_owner("/a.txt", 0, 0).err().unwrap().to_string(),
                    fs_error!(
                        "Owner could not be set, path not found: {}",
                        normalize_path(&fs.current_dir, "/a.txt").display()
                    )
                    .to_string()
                );
            }

            #[test]
            fn set_owner_invalid_path_should_fail() {
                let fs = TestFs::new();
                assert!(fs.set_owner("\0", 0, 0).is_err());
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

        mod fs {
            use super::*;

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
                        fs_error!(
                            "File could not be opened, path is a directory: {}",
                            normalize_path(&fs.current_dir, "/mydir").display()
                        )
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

                #[test]
                fn open_read_invalid_path_should_fail() {
                    let fs = TestFs::new();
                    let result = fs.open_read("\0");
                    assert!(result.is_err());
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
                    let fs = TestFs::new();
                    fs.write("/test.txt", "content").unwrap();

                    let mut file = fs
                        .open_write_options("/test.txt", OpenWriteOptions::new().truncate())
                        .unwrap();
                    let len = file.seek(SeekFrom::End(0)).unwrap() as usize;
                    assert_eq!(len, 0);
                }

                #[test]
                fn open_write_options_truncate_should_update_modified() {
                    let fs = TestFs::new();
                    fs.write("/test.txt", "content").unwrap();
                    fs.set_modified("/test.txt", SystemTime::UNIX_EPOCH)
                        .unwrap();

                    let _file = fs
                        .open_write_options("/test.txt", OpenWriteOptions::new().truncate())
                        .unwrap();

                    assert!(fs.metadata("/test.txt").unwrap().modified > SystemTime::UNIX_EPOCH);
                }

                #[test]
                fn open_write_options_create_should_update_parent_modified() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();

                    for (parent, file) in [("/dir", "/dir/test.txt"), ("/", "/test.txt")] {
                        fs.set_modified(parent, SystemTime::UNIX_EPOCH).unwrap();
                        fs.open_write(file).unwrap();
                        assert!(
                            fs.metadata(parent).unwrap().modified > SystemTime::UNIX_EPOCH,
                            "parent: {parent}"
                        );
                    }
                }

                #[test]
                fn open_write_options_readonly_file_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/test.txt", "content").unwrap();
                    fs.set_readonly("/test.txt", true).unwrap();

                    assert_eq!(
                        fs.open_write("/test.txt").err().unwrap().to_string(),
                        fs_error!(
                            "File could not be opened, it is read-only: {}",
                            normalize_path(&fs.current_dir, "/test.txt").display()
                        )
                        .to_string()
                    );
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

                #[cfg(unix)]
                #[test]
                fn open_write_options_mode_should_apply_umask_to_new_file_only() {
                    let fs = TestFs::new();

                    drop(
                        fs.open_write_options(
                            "/a.txt",
                            OpenWriteOptions::new().create_new().mode(0o4777),
                        )
                        .unwrap(),
                    );
                    assert_eq!(fs.metadata("/a.txt").unwrap().mode, 0o4755);

                    drop(
                        fs.open_write_options("/a.txt", OpenWriteOptions::new().mode(0o600))
                            .unwrap(),
                    );
                    assert_eq!(fs.metadata("/a.txt").unwrap().mode, 0o4755);
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
                        fs_error!(
                            "File could not be opened, path is a directory: {}",
                            normalize_path(&fs.current_dir, "/mydir").display()
                        )
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
                            fs_error!(
                                "File could not be created, parent directory does not exist: {}",
                                normalize_path(&fs.current_dir, path).display()
                            )
                            .to_string(),
                            "path: {path}"
                        );
                    }
                }

                #[test]
                fn open_write_options_should_reject_append_with_truncate() {
                    let fs = TestFs::new();

                    assert!(
                        fs.open_write_options(
                            "/a.txt",
                            OpenWriteOptions::new().append().truncate()
                        )
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
                        fs.open_write_options(
                            "/c.txt",
                            OpenWriteOptions::new().append().truncate()
                        )
                        .is_err()
                    );
                }

                #[test]
                fn open_write_options_invalid_path_should_fail() {
                    let fs = TestFs::new();
                    let result = fs.open_write_options("\0", OpenWriteOptions::new());
                    assert!(result.is_err());
                }
            }

            mod read_to_string {
                use super::*;

                #[test]
                fn read_to_string_invalid_utf8_should_fail() {
                    let fs = TestFs::new();
                    fs.open_write("/bad.txt")
                        .unwrap()
                        .write_all(&[0xFF])
                        .unwrap();

                    let error = fs.read_to_string("/bad.txt").err().unwrap();
                    assert!(
                        error
                            .description()
                            .ends_with(&format!(": {}", Path::new("/bad.txt").display())),
                        "error should name the path, got: {error}"
                    );
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
                    assert_eq!(
                        file.path(),
                        normalize_path(&fs.current_dir, "/mydir/file.txt")
                    );

                    let dir = entries.iter().find(|e| e.file_name() == "subdir").unwrap();
                    assert!(
                        dir.file_type().is_dir(),
                        "expected subdir to be a directory"
                    );
                    assert_eq!(dir.path(), normalize_path(&fs.current_dir, "/mydir/subdir"));
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
                    // WHY: a lone UTF-16 surrogate is a valid Windows name but not UTF-8.
                    #[cfg(windows)]
                    let name = {
                        use std::os::windows::ffi::OsStringExt;
                        std::ffi::OsString::from_wide(&[0xD800])
                    };

                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    // WHY: inserted directly because passing a `&Path` to `write` would add a
                    // second instantiation of every generic method to cover.
                    lock_handles(&fs.fs).insert(
                        normalize_path(&fs.current_dir, "/").join(name),
                        TestFsEntry::Dir(TestFsDir::new(SystemTime::now())),
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
                        fs_error!(
                            "Not a directory: {}",
                            normalize_path(&fs.current_dir, "/a.txt").display()
                        )
                        .to_string()
                    );
                }

                #[test]
                fn read_dir_invalid_path_should_fail() {
                    let fs = TestFs::new();
                    let result = fs.read_dir("\0");
                    assert!(result.is_err());
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
                fn create_dir_all_should_update_parent_modified() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/a").unwrap();
                    fs.set_modified("/a", SystemTime::UNIX_EPOCH).unwrap();

                    fs.create_dir_all("/a/b/c").unwrap();

                    assert!(fs.metadata("/a").unwrap().modified > SystemTime::UNIX_EPOCH);
                    assert_eq!(
                        fs.metadata("/a/b").unwrap().modified,
                        fs.metadata("/a/b/c").unwrap().modified
                    );
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
                        fs_error!(
                            "Cannot create directory, a file already exists at: {}",
                            normalize_path(&fs.current_dir, "/a").display()
                        )
                        .to_string()
                    );
                }

                #[test]
                fn create_dir_all_invalid_path_should_fail() {
                    let fs = TestFs::new();
                    let result = fs.create_dir_all("\0");
                    assert!(result.is_err());
                }
            }

            mod exists {
                use super::*;

                #[test]
                fn exists_should_return_true_for_existing_file() {
                    let fs = TestFs::new();
                    fs.write("Cargo.toml", "data").unwrap();
                    assert!(fs.exists("Cargo.toml").unwrap());
                }

                #[test]
                fn exists_root_should_return_true() {
                    let fs = TestFs::new();
                    assert!(fs.exists("/").unwrap());
                }

                #[test]
                fn exists_should_return_true_for_existing_directory() {
                    let fs = TestFs::new();
                    fs.create_dir_all("src").unwrap();
                    assert!(fs.exists("src").unwrap());
                }

                #[test]
                fn exists_should_return_false_for_non_existing_file() {
                    let fs = TestFs::new();
                    assert!(!fs.exists("non_existing_file.tmp").unwrap());
                }

                #[test]
                fn exists_should_return_false_for_non_existing_directory() {
                    let fs = TestFs::new();
                    assert!(!fs.exists("non_existing_directory").unwrap());
                }

                #[test]
                fn exists_should_return_error_for_invalid_path() {
                    let fs = TestFs::new();
                    let result = fs.exists("\0");
                    assert!(result.is_err());
                }
            }

            mod metadata {
                use super::*;

                #[test]
                fn metadata_should_return_file_metadata() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "abc").unwrap();

                    let metadata = fs.metadata("/a.txt").unwrap();

                    assert_eq!(metadata.file_type, FileType::File);
                    assert_eq!(metadata.len, 3);
                    assert!(metadata.created <= metadata.modified);
                    assert!(!metadata.readonly && !metadata.executable && !metadata.hidden);
                }

                #[test]
                fn metadata_should_return_dir_metadata() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/a").unwrap();

                    let metadata = fs.metadata("/a").unwrap();

                    assert_eq!(metadata.file_type, FileType::Dir);
                    assert_eq!(metadata.len, 0);
                    assert_eq!(metadata.created, metadata.modified);
                    assert!(!metadata.readonly && !metadata.executable && !metadata.hidden);
                }

                #[test]
                fn metadata_root_should_return_dir_created_with_fs() {
                    let before = SystemTime::now();
                    let fs = TestFs::new();

                    let metadata = fs.metadata(ROOT).unwrap();

                    assert_eq!(metadata.file_type, FileType::Dir);
                    assert!(metadata.created >= before);
                    assert_eq!(metadata.modified, metadata.created);
                }

                #[test]
                fn metadata_other_root_should_fail_until_created() {
                    #[cfg(unix)]
                    let other_root = "";
                    #[cfg(windows)]
                    let other_root = "D:\\";

                    let mut fs = TestFs::new();
                    fs.set_current_dir(PathBuf::new());
                    assert!(fs.metadata(other_root).is_err());

                    fs.create_dir_all(other_root).unwrap();

                    assert_eq!(fs.metadata(other_root).unwrap().file_type, FileType::Dir);
                }

                #[cfg(unix)]
                #[test]
                fn metadata_should_report_umask_modes_and_owner() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "").unwrap();
                    fs.create_dir_all("/dir").unwrap();

                    for (path, mode) in [("/a.txt", 0o644), ("/dir", 0o755), ("/", 0o755)] {
                        let metadata = fs.metadata(path).unwrap();
                        assert_eq!(
                            (metadata.mode, metadata.uid, metadata.gid),
                            (mode, 1000, 1000),
                            "path: {path}"
                        );
                    }
                }

                #[cfg(unix)]
                #[test]
                fn metadata_dot_name_should_be_hidden() {
                    let fs = TestFs::new();
                    fs.write("/.hidden", "").unwrap();
                    assert!(fs.metadata("/.hidden").unwrap().hidden);
                }

                #[test]
                fn metadata_should_fail_for_nonexistent_path() {
                    let fs = TestFs::new();
                    assert_eq!(
                        fs.metadata("/a.txt").err().unwrap().to_string(),
                        fs_error!(
                            "Metadata could not be read, path not found: {}",
                            normalize_path(&fs.current_dir, "/a.txt").display()
                        )
                        .to_string()
                    );
                }

                #[test]
                fn metadata_invalid_path_should_fail() {
                    let fs = TestFs::new();
                    assert!(fs.metadata("\0").is_err());
                }
            }

            mod set_readonly {
                use super::*;

                #[test]
                fn set_readonly_should_store_readonly_attribute() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "").unwrap();
                    fs.create_dir_all("/dir").unwrap();

                    for path in ["/a.txt", "/dir", "/"] {
                        fs.set_readonly(path, true).unwrap();
                        assert!(fs.metadata(path).unwrap().readonly, "path: {path}");
                    }
                }
            }

            #[cfg(unix)]
            mod set_executable {
                use super::*;

                #[test]
                fn set_executable_should_store_executable_attribute() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "").unwrap();

                    fs.set_executable("/a.txt", true).unwrap();

                    assert!(fs.metadata("/a.txt").unwrap().executable);
                }

                #[test]
                fn set_executable_dir_should_fail() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();

                    assert_eq!(
                        fs.set_executable("/dir", true).err().unwrap().to_string(),
                        fs_error!(
                            "Executable attribute cannot be set, path is a directory: {}",
                            normalize_path(&fs.current_dir, "/dir").display()
                        )
                        .to_string()
                    );
                }
            }

            #[cfg(windows)]
            mod set_hidden {
                use super::*;

                #[test]
                fn set_hidden_should_store_hidden_attribute() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "").unwrap();

                    fs.set_hidden("/a.txt", true).unwrap();

                    assert!(fs.metadata("/a.txt").unwrap().hidden);
                }
            }
            mod set_modified {
                use super::*;

                #[test]
                fn set_modified_should_set_modified_time() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "").unwrap();
                    fs.create_dir_all("/dir").unwrap();

                    for path in ["/a.txt", "/dir", "/"] {
                        fs.set_modified(path, SystemTime::UNIX_EPOCH).unwrap();
                        assert_eq!(
                            fs.metadata(path).unwrap().modified,
                            SystemTime::UNIX_EPOCH,
                            "path: {path}"
                        );
                    }
                }

                #[test]
                fn set_modified_should_fail_for_nonexistent_path() {
                    let fs = TestFs::new();
                    assert_eq!(
                        fs.set_modified("/a.txt", SystemTime::UNIX_EPOCH)
                            .err()
                            .unwrap()
                            .to_string(),
                        fs_error!(
                            "Modified time could not be set, path not found: {}",
                            normalize_path(&fs.current_dir, "/a.txt").display()
                        )
                        .to_string()
                    );
                }

                #[test]
                fn set_modified_invalid_path_should_fail() {
                    let fs = TestFs::new();
                    assert!(fs.set_modified("\0", SystemTime::UNIX_EPOCH).is_err());
                }
            }

            #[cfg(unix)]
            mod set_mode {
                use super::*;

                #[test]
                fn set_mode_should_set_permission_bits() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "").unwrap();
                    fs.create_dir_all("/dir").unwrap();

                    for (path, requested, expected) in [
                        ("/a.txt", 0o4750, 0o4750),
                        ("/a.txt", 0o100600, 0o600),
                        ("/dir", 0o1777, 0o1777),
                    ] {
                        fs.set_mode(path, requested).unwrap();
                        assert_eq!(
                            fs.metadata(path).unwrap().mode,
                            expected,
                            "path: {path}, requested: {requested:o}"
                        );
                    }
                }

                #[test]
                fn set_mode_without_write_bits_should_make_file_readonly() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "").unwrap();

                    fs.set_mode("/a.txt", 0o444).unwrap();

                    assert!(fs.metadata("/a.txt").unwrap().readonly);
                    assert!(fs.write("/a.txt", "data").is_err());
                }

                #[test]
                fn set_mode_should_fail_for_nonexistent_path() {
                    let fs = TestFs::new();
                    assert_eq!(
                        fs.set_mode("/a.txt", 0o600).err().unwrap().to_string(),
                        fs_error!(
                            "Mode could not be set, path not found: {}",
                            normalize_path(&fs.current_dir, "/a.txt").display()
                        )
                        .to_string()
                    );
                }

                #[test]
                fn set_mode_invalid_path_should_fail() {
                    let fs = TestFs::new();
                    assert!(fs.set_mode("\0", 0o600).is_err());
                }
            }
        }

        mod set_attribute {
            use super::*;

            #[test]
            fn set_attribute_should_fail_for_nonexistent_path() {
                let fs = TestFs::new();
                assert_eq!(
                    fs.set_attribute(Path::new("/a.txt"), Attribute::Readonly, true)
                        .err()
                        .unwrap()
                        .to_string(),
                    fs_error!(
                        "Attributes could not be set, path not found: {}",
                        normalize_path(&fs.current_dir, "/a.txt").display()
                    )
                    .to_string()
                );
            }

            #[test]
            fn set_attribute_invalid_path_should_fail() {
                let fs = TestFs::new();
                assert!(
                    fs.set_attribute(Path::new("\0"), Attribute::Readonly, true)
                        .is_err()
                );
            }
        }
    }

    mod test_file_write {
        use super::*;

        mod write {
            use super::*;

            #[allow(clippy::module_inception)]
            mod write {
                use super::*;

                #[test]
                fn write_should_update_modified() {
                    let fs = TestFs::new();
                    let mut file = fs.open_write("/a.txt").unwrap();
                    fs.set_modified("/a.txt", SystemTime::UNIX_EPOCH).unwrap();

                    file.write_all(b"abc").unwrap();

                    assert!(fs.metadata("/a.txt").unwrap().modified > SystemTime::UNIX_EPOCH);
                }
            }
        }
    }

    mod touch_parent {
        use super::*;

        #[test]
        fn touch_parent_should_update_parent_dir_modified() {
            let created = SystemTime::UNIX_EPOCH;
            let now = created + std::time::Duration::from_secs(1);
            let mut fs = BTreeMap::new();
            fs.insert(
                PathBuf::from("/a"),
                TestFsEntry::Dir(TestFsDir::new(created)),
            );

            touch_parent(&mut fs, Path::new("/a/b"), now);
            touch_parent(&mut fs, Path::new("/a"), now);

            assert!(matches!(
                fs.get(Path::new("/a")),
                Some(TestFsEntry::Dir(dir)) if dir.modified == now && dir.created == created
            ));
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
                    content: Arc::new(RwLock::new(TestFsContent::new(SystemTime::now()))),
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
                    content: Arc::new(RwLock::new(TestFsContent::new(SystemTime::now()))),
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
                    content: Arc::new(RwLock::new(TestFsContent::new(SystemTime::now()))),
                    fail_mode: None,
                };

                drop(handle);
            }
        }
    }
}
