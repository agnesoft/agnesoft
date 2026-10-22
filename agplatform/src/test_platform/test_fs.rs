use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::ops::Bound;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
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
use crate::fs::check_not_root;
use crate::fs::copy_tree;
use crate::fs::ends_with_separator;
use crate::fs::lock_handles;
use crate::fs::native_symlink_target;
use crate::fs::normalize_path;
use crate::platform::platform_env::CurrentDirSetter;

/// An in-memory file holding its content, borrow state and metadata.
///
/// Shared by all of its hard links and open handles, see
/// [`TestFsEntry::File`].
#[derive(Debug)]
pub struct TestFsFile {
    data: Vec<u8>,
    modified: SystemTime,
    state: BorrowState,
    created: SystemTime,
    attributes: NativeAttributes,
}

impl TestFsFile {
    fn new(now: SystemTime, attributes: NativeAttributes) -> Self {
        Self {
            data: Vec::new(),
            modified: now,
            state: BorrowState::Read(0),
            created: now,
            attributes,
        }
    }
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

/// An in-memory symlink entry holding its target as given and its
/// metadata.
pub struct TestFsSymlink {
    target: PathBuf,
    created: SystemTime,
    attributes: NativeAttributes,
}

impl TestFsSymlink {
    fn new(target: PathBuf, created: SystemTime) -> Self {
        // WHY: symlink permissions are not used on Linux, their mode is
        // always `0o777` regardless of the umask.
        #[cfg(unix)]
        let attributes = NativeAttributes {
            mode: 0o777,
            uid: OWNER_ID,
            gid: OWNER_ID,
        };
        #[cfg(windows)]
        let attributes = NativeAttributes::default();

        Self {
            target,
            created,
            attributes,
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
    /// A file with in-memory content, shared by all paths that are hard
    /// links to it.
    File(Arc<RwLock<TestFsFile>>),
    /// A directory.
    Dir(TestFsDir),
    /// A symlink.
    Symlink(TestFsSymlink),
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
    /// The number of symlinks in `fs`, changed only while `fs` is locked.
    symlinks: AtomicUsize,
}

impl TestFs {
    /// Creates an in-memory filesystem holding only the [`ROOT`] directory,
    /// which is also the current directory. Other roots, e.g. further
    /// Windows drives, can be added with [`Fs::create_dir_all`].
    ///
    /// On Unix new entries are owned by user and group `1000` and created
    /// under the umask `0o022`: files `0o644`, directories `0o755`.
    ///
    /// Permissions are stored and reported but not modelled as access
    /// checks: only opening a [`readonly`](crate::Metadata::readonly) file
    /// for writing fails. Other mode bits, the owner and the permissions of
    /// directories have no effect, there is no current user. Use
    /// [`set_fail_mode`](TestFs::set_fail_mode) to test failing
    /// operations.
    ///
    /// Symlinks are resolved like on a real filesystem, including relative
    /// targets, symlinks in parent components and loops, which fail after
    /// 40 symlinks as on Linux. The Windows privileges needed to create
    /// symlinks are not modelled. Hard links share the file itself: its
    /// content, metadata and borrows.
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
            symlinks: AtomicUsize::new(0),
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
        let (mut fs, normalized_path, resolved_path) = self.lock_resolve(path.as_ref(), true)?;

        match fs.get_mut(&resolved_path) {
            Some(TestFsEntry::File(file)) => lock_file_write(file).created = created,
            Some(TestFsEntry::Dir(dir)) => dir.created = created,
            Some(TestFsEntry::Symlink(_)) | None => {
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
        let (mut fs, normalized_path, resolved_path) = self.lock_resolve(path, true)?;

        match fs.get_mut(&resolved_path) {
            Some(TestFsEntry::File(file)) => update(&mut lock_file_write(file).attributes),
            Some(TestFsEntry::Dir(dir)) => update(&mut dir.attributes),
            Some(TestFsEntry::Symlink(_)) | None => {
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

    /// Locks the filesystem and returns it with the normalized `path`, for
    /// messages, and the `path` with its symlinks resolved, the last
    /// component only if `follow_last` is set, see [`resolve`](Self::resolve).
    fn lock_resolve(
        &self,
        path: &Path,
        follow_last: bool,
    ) -> Result<(TestFsGuard<'_>, PathBuf, PathBuf)> {
        let (fs, normalized_path) = self.lock_normalize(path)?;
        let resolved_path = self.resolve(&fs, &normalized_path, follow_last)?;

        Ok((fs, normalized_path, resolved_path))
    }

    /// Locks the filesystem and returns it with the normalized `path`.
    fn lock_normalize(&self, path: &Path) -> Result<(TestFsGuard<'_>, PathBuf)> {
        let normalized_path = self.normalize(path)?;
        Ok((lock_handles(&self.fs), normalized_path))
    }

    /// Returns the `path` normalized against the current directory, see
    /// [`normalize_path`].
    fn normalize(&self, path: &Path) -> Result<PathBuf> {
        if path.to_string_lossy().contains('\0') {
            return Err(fs_error!("Path contains a NUL byte: {}", path.display()));
        }

        Ok(normalize_path(&self.current_dir, path))
    }

    /// Resolves the symlinks of a `path` already normalized by
    /// [`normalize_path`], see [`resolve_symlinks`]. While the filesystem
    /// has no symlinks the `path` is returned as is, without walking its
    /// components.
    fn resolve(
        &self,
        fs: &BTreeMap<PathBuf, TestFsEntry>,
        path: &Path,
        follow_last: bool,
    ) -> Result<PathBuf> {
        // WHY: relaxed suffices, the count only changes while `fs` is
        // locked, which the caller holds.
        if self.symlinks.load(Ordering::Relaxed) == 0 {
            return Ok(path.to_path_buf());
        }

        resolve_symlinks(fs, path, follow_last)
    }
}

type TestFsGuard<'a> = std::sync::MutexGuard<'a, BTreeMap<PathBuf, TestFsEntry>>;

/// The number of symlinks followed when resolving a path before it is
/// considered a loop, `MAXSYMLINKS` on Linux.
const MAX_SYMLINKS: usize = 40;

/// Resolves the symlinks of a normalized `path`: in the parent components
/// always, in the last component only if `follow_last` is set. A relative
/// target continues from the directory containing the symlink and a `..`
/// in a target leaves the directory the path has resolved into so far.
fn resolve_symlinks(
    fs: &BTreeMap<PathBuf, TestFsEntry>,
    path: &Path,
    follow_last: bool,
) -> Result<PathBuf> {
    let mut components: VecDeque<Component> = path.components().collect();
    let mut resolved = PathBuf::new();
    let mut followed = 0;

    while let Some(component) = components.pop_front() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::RootDir | Component::Prefix(_) | Component::Normal(_) => {
                resolved.push(component);
            }
        }

        if let Some(TestFsEntry::Symlink(symlink)) = fs.get(&resolved)
            && (follow_last || !components.is_empty())
        {
            followed += 1;

            if followed > MAX_SYMLINKS {
                return Err(fs_error!(
                    "Too many levels of symbolic links: {}",
                    path.display()
                ));
            }

            resolved.pop();

            for target_component in symlink.target.components().rev() {
                components.push_front(target_component);
            }
        }
    }

    Ok(resolved)
}

impl CurrentDirSetter for TestFs {
    fn set_current_dir(&mut self, path: PathBuf) {
        self.current_dir = path;
    }
}

struct TestFileHandle {
    pos: u64,
    len: u64,
    file: Arc<RwLock<TestFsFile>>,
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
        let mut file = lock_file_write(&self.file);

        match &mut file.state {
            BorrowState::Read(count) => {
                *count = count.saturating_sub(1);
            }
            BorrowState::Write => {
                file.state = BorrowState::Read(0);
            }
        }
    }
}

fn lock_file_read(file: &RwLock<TestFsFile>) -> std::sync::RwLockReadGuard<'_, TestFsFile> {
    file.clear_poison();
    file.read().expect("Failed to lock file for reading")
}

fn lock_file_write(file: &RwLock<TestFsFile>) -> std::sync::RwLockWriteGuard<'_, TestFsFile> {
    file.clear_poison();
    file.write().expect("Failed to lock file for writing")
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
            &lock_file_read(&self.0.file).data
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
            let mut file = lock_file_write(&self.handle.0.file);

            if end_pos > file.data.len() as u64 {
                file.data.resize(end_pos as usize, 0);
                self.handle.0.len = end_pos;
            }

            file.data[self.handle.0.pos as usize..end_pos as usize].copy_from_slice(buf);
            file.modified = SystemTime::now();
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

fn entry_file_type(entry: &TestFsEntry) -> FileType {
    match entry {
        TestFsEntry::Dir(_) => FileType::Dir,
        TestFsEntry::File(_) => FileType::File,
        TestFsEntry::Symlink(_) => FileType::Symlink,
    }
}

/// Returns the [`Metadata`] of the entry at the resolved `path`, reported
/// for the `normalized_path` it was resolved from.
fn entry_metadata(
    fs: &BTreeMap<PathBuf, TestFsEntry>,
    normalized_path: &Path,
    path: &Path,
) -> Result<Metadata> {
    let (file_type, len, modified, created, attributes) = match fs.get(path) {
        Some(TestFsEntry::File(file)) => {
            let file = lock_file_read(file);
            (
                FileType::File,
                file.data.len() as u64,
                file.modified,
                file.created,
                file.attributes,
            )
        }
        Some(TestFsEntry::Dir(dir)) => {
            (FileType::Dir, 0, dir.modified, dir.created, dir.attributes)
        }
        Some(TestFsEntry::Symlink(symlink)) => {
            #[cfg(unix)]
            let len = symlink.target.as_os_str().len() as u64;
            #[cfg(windows)]
            let len = 0;

            (
                FileType::Symlink,
                len,
                symlink.created,
                symlink.created,
                symlink.attributes,
            )
        }
        None => {
            return Err(fs_error!(
                "Metadata could not be read, path not found: {}",
                normalized_path.display()
            ));
        }
    };

    Ok(Metadata::new(
        normalized_path,
        file_type,
        len,
        modified,
        created,
        attributes,
    ))
}

/// Descendants of a path sort directly after it, so only the entry after
/// `path` needs to be checked.
fn has_children(fs: &BTreeMap<PathBuf, TestFsEntry>, path: &Path) -> bool {
    fs.range::<Path, _>((Bound::Excluded(path), Bound::Unbounded))
        .next()
        .is_some_and(|(child, _)| child.starts_with(path))
}

/// Returns the attributes of a copy of an entry, which keeps only the
/// permissions, see [`Fs::copy`].
fn copied_attributes(attributes: NativeAttributes) -> NativeAttributes {
    #[cfg(unix)]
    let copied = NativeAttributes {
        mode: attributes.mode,
        uid: OWNER_ID,
        gid: OWNER_ID,
    };
    #[cfg(windows)]
    let copied = NativeAttributes {
        readonly: attributes.readonly,
        ..NativeAttributes::default()
    };

    copied
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
    type Dir = std::vec::IntoIter<Result<DirEntry>>;

    fn open_read<P: AsRef<Path>>(&self, path: P) -> Result<Self::ReadHandle> {
        let (fs, normalized_path, resolved_path) = self.lock_resolve(path.as_ref(), true)?;

        if is_dir(&fs, &resolved_path) {
            return Err(fs_error!(
                "File could not be opened, path is a directory: {}",
                normalized_path.display()
            ));
        }

        if let Some(TestFsEntry::File(file)) = fs.get(&resolved_path) {
            let mut inode = lock_file_write(file);

            match &mut inode.state {
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
                pos: 0,
                len: inode.data.len() as u64,
                file: file.clone(),
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
        let (mut fs, normalized_path, link_path) = self.lock_resolve(path.as_ref(), false)?;

        // WHY: like `O_EXCL`, `create_new` does not follow a symlink, even
        // a dangling one.
        if options.create_new && fs.contains_key(&link_path) {
            return Err(fs_error!(
                "File could not be opened with 'create_new' option, file already exists: {}",
                normalized_path.display()
            ));
        }

        let resolved_path = self.resolve(&fs, &link_path, true)?;

        if is_dir(&fs, &resolved_path) {
            return Err(fs_error!(
                "File could not be opened, path is a directory: {}",
                normalized_path.display()
            ));
        }

        if let Some(TestFsEntry::File(file)) = fs.get(&resolved_path) {
            if options.append && options.truncate {
                return Err(fs_error!(
                    "Existing file could not be opened with both 'append' and 'truncate' options: {}",
                    normalized_path.display()
                ));
            }

            let mut inode = lock_file_write(file);

            if inode.attributes.attributes().readonly {
                return Err(fs_error!(
                    "File could not be opened, it is read-only: {}",
                    normalized_path.display()
                ));
            }

            match inode.state {
                BorrowState::Read(count) => {
                    if count == 0 {
                        inode.state = BorrowState::Write;
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
                inode.data.clear();
                inode.modified = SystemTime::now();
                0
            } else {
                inode.data.len() as u64
            };

            return Ok(TestFileWrite {
                handle: TestFileRead(TestFileHandle {
                    pos: 0,
                    len,
                    file: file.clone(),
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

        if !resolved_path
            .parent()
            .is_some_and(|parent| is_dir(&fs, parent))
        {
            return Err(fs_error!(
                "File could not be created, parent directory does not exist: {}",
                normalized_path.display()
            ));
        }

        let now = SystemTime::now();
        let mut new_file = TestFsFile::new(now, new_file_attributes(&options));
        new_file.state = BorrowState::Write;
        let new_file = Arc::new(RwLock::new(new_file));
        let file = TestFileWrite {
            handle: TestFileRead(TestFileHandle {
                pos: 0,
                len: 0,
                file: new_file.clone(),
                fail_mode: self.fail_mode,
            }),
            append: options.append,
        };

        touch_parent(&mut fs, &resolved_path, now);
        fs.insert(resolved_path, TestFsEntry::File(new_file));

        Ok(file)
    }

    fn read_dir<P: AsRef<Path>>(&self, path: P) -> Result<Self::Dir> {
        let (fs, normalized_path, resolved_path) = self.lock_resolve(path.as_ref(), true)?;

        match fs.get(&resolved_path) {
            Some(TestFsEntry::File(_)) => {
                return Err(fs_error!("Not a directory: {}", normalized_path.display()));
            }
            Some(TestFsEntry::Symlink(_)) | None => {
                return Err(fs_error!(
                    "Directory not found: {}",
                    normalized_path.display()
                ));
            }
            Some(TestFsEntry::Dir(_)) => {}
        }

        // Descendants of a path sort directly after it, so this visits only
        // the directory's subtree rather than the whole map.
        let entries: Vec<Result<DirEntry>> = fs
            .range::<Path, _>((Bound::Excluded(resolved_path.as_path()), Bound::Unbounded))
            .take_while(|(path, _)| path.starts_with(&resolved_path))
            .filter(|(path, _)| path.parent() == Some(&resolved_path))
            .filter_map(|(path, entry)| {
                path.file_name().and_then(|name| {
                    DirEntry::new(normalized_path.join(name), entry_file_type(entry))
                })
            })
            .map(Ok)
            .collect();

        Ok(entries.into_iter())
    }

    fn create_dir<P: AsRef<Path>>(&self, path: P) -> Result {
        let (mut fs, normalized_path, link_path) = self.lock_resolve(path.as_ref(), false)?;

        if fs.contains_key(&link_path) {
            return Err(fs_error!(
                "Directory could not be created, path already exists: {}",
                normalized_path.display()
            ));
        }

        if !link_path.parent().is_some_and(|parent| is_dir(&fs, parent)) {
            return Err(fs_error!(
                "Directory could not be created, parent directory does not exist: {}",
                normalized_path.display()
            ));
        }

        let now = SystemTime::now();
        touch_parent(&mut fs, &link_path, now);
        fs.insert(link_path, TestFsEntry::Dir(TestFsDir::new(now)));

        Ok(())
    }

    fn create_dir_all<P: AsRef<Path>>(&self, path: P) -> Result {
        let (mut fs, normalized_path) = self.lock_normalize(path.as_ref())?;
        let now = SystemTime::now();
        let ancestors: Vec<&Path> = normalized_path.ancestors().collect();
        let mut parent = PathBuf::new();

        // WHY: each ancestor is created like `std::fs::create_dir_all`
        // does, which follows a symlink to a directory but does not create
        // the target of a dangling one.
        for ancestor in ancestors.into_iter().rev() {
            let link = ancestor
                .file_name()
                .map_or_else(|| ancestor.to_path_buf(), |name| parent.join(name));
            let dir = self.resolve(&fs, &link, true)?;

            match fs.get(&dir) {
                Some(TestFsEntry::Dir(_)) => {}
                Some(TestFsEntry::File(_)) => {
                    return Err(fs_error!(
                        "Cannot create directory, a file already exists at: {}",
                        link.display()
                    ));
                }
                Some(TestFsEntry::Symlink(_)) | None => {
                    if matches!(fs.get(&link), Some(TestFsEntry::Symlink(_))) {
                        return Err(fs_error!(
                            "Cannot create directory, a dangling symlink exists at: {}",
                            link.display()
                        ));
                    }

                    touch_parent(&mut fs, &dir, now);
                    fs.insert(dir.clone(), TestFsEntry::Dir(TestFsDir::new(now)));
                }
            }

            parent = dir;
        }

        Ok(())
    }

    fn exists<P: AsRef<Path>>(&self, path: P) -> Result<bool> {
        let (fs, _, resolved_path) = self.lock_resolve(path.as_ref(), true)?;
        Ok(fs.contains_key(&resolved_path))
    }

    fn metadata<P: AsRef<Path>>(&self, path: P) -> Result<Metadata> {
        let (fs, normalized_path, resolved_path) = self.lock_resolve(path.as_ref(), true)?;
        entry_metadata(&fs, &normalized_path, &resolved_path)
    }

    fn symlink_metadata<P: AsRef<Path>>(&self, path: P) -> Result<Metadata> {
        let (fs, normalized_path, link_path) = self.lock_resolve(path.as_ref(), false)?;
        entry_metadata(&fs, &normalized_path, &link_path)
    }

    fn create_symlink<P: AsRef<Path>, T: AsRef<Path>>(&self, path: P, target: T) -> Result {
        let (mut fs, normalized_path, link_path) = self.lock_resolve(path.as_ref(), false)?;
        self.add_symlink(
            &mut fs,
            &normalized_path,
            link_path,
            native_symlink_target(target.as_ref()),
        )
    }

    fn read_link<P: AsRef<Path>>(&self, path: P) -> Result<PathBuf> {
        let (fs, normalized_path, link_path) = self.lock_resolve(path.as_ref(), false)?;

        match fs.get(&link_path) {
            Some(TestFsEntry::Symlink(symlink)) => Ok(symlink.target.clone()),
            Some(TestFsEntry::File(_)) | Some(TestFsEntry::Dir(_)) => Err(fs_error!(
                "Symlink could not be read, path is not a symlink: {}",
                normalized_path.display()
            )),
            None => Err(fs_error!(
                "Symlink could not be read, path not found: {}",
                normalized_path.display()
            )),
        }
    }

    fn create_hard_link<P: AsRef<Path>, T: AsRef<Path>>(&self, path: P, target: T) -> Result {
        let (mut fs, normalized_path, link_path) = self.lock_resolve(path.as_ref(), false)?;
        let normalized_target = self.normalize(target.as_ref())?;

        if fs.contains_key(&link_path) {
            return Err(fs_error!(
                "Hard link could not be created, path already exists: {}",
                normalized_path.display()
            ));
        }

        if !link_path.parent().is_some_and(|parent| is_dir(&fs, parent)) {
            return Err(fs_error!(
                "Hard link could not be created, parent directory does not exist: {}",
                normalized_path.display()
            ));
        }

        let file = match fs.get(&self.resolve(&fs, &normalized_target, true)?) {
            Some(TestFsEntry::File(file)) => file.clone(),
            Some(TestFsEntry::Dir(_)) => {
                return Err(fs_error!(
                    "Hard link could not be created, target is a directory: {}",
                    normalized_target.display()
                ));
            }
            Some(TestFsEntry::Symlink(_)) | None => {
                return Err(fs_error!(
                    "Hard link could not be created, target not found: {}",
                    normalized_target.display()
                ));
            }
        };

        touch_parent(&mut fs, &link_path, SystemTime::now());
        fs.insert(link_path, TestFsEntry::File(file));

        Ok(())
    }

    fn copy<S: AsRef<Path>, T: AsRef<Path>>(&self, source: S, target: T) -> Result {
        let normalized_source = self.normalize(source.as_ref())?;
        let (mut fs, normalized_target, target_path) = self.lock_resolve(target.as_ref(), false)?;
        let source_path = self.resolve(&fs, &normalized_source, false)?;

        let Some(source_entry) = fs.get(&source_path) else {
            return Err(fs_error!(
                "Path could not be copied, source not found: {}",
                normalized_source.display()
            ));
        };

        if fs.contains_key(&target_path) {
            return Err(fs_error!(
                "Path could not be copied, target already exists: {}",
                normalized_target.display()
            ));
        }

        if !target_path
            .parent()
            .is_some_and(|parent| is_dir(&fs, parent))
        {
            return Err(fs_error!(
                "Path could not be copied, parent directory does not exist: {}",
                normalized_target.display()
            ));
        }

        let now = SystemTime::now();
        let entry = match source_entry {
            TestFsEntry::File(file) => {
                let file = lock_file_read(file);

                if let BorrowState::Write = file.state {
                    return Err(fs_error!(
                        "File could not be copied, it is already borrowed for writing: {}",
                        normalized_source.display()
                    ));
                }

                let mut copy = TestFsFile::new(now, copied_attributes(file.attributes));
                copy.data = file.data.clone();
                TestFsEntry::File(Arc::new(RwLock::new(copy)))
            }
            TestFsEntry::Dir(dir) => {
                if has_children(&fs, &source_path) {
                    return Err(fs_error!(
                        "Directory could not be copied, it is not empty: {}",
                        normalized_source.display()
                    ));
                }

                let mut copy = TestFsDir::new(now);
                copy.attributes = copied_attributes(dir.attributes);
                TestFsEntry::Dir(copy)
            }
            TestFsEntry::Symlink(symlink) => {
                let link = symlink.target.clone();
                return self.add_symlink(&mut fs, &normalized_target, target_path, link);
            }
        };

        touch_parent(&mut fs, &target_path, now);
        fs.insert(target_path, entry);

        Ok(())
    }

    fn copy_dir_all<S: AsRef<Path>, T: AsRef<Path>>(&self, source: S, target: T) -> Result {
        copy_tree(self, source.as_ref(), target.as_ref())
    }

    fn remove<P: AsRef<Path>>(&self, path: P) -> Result {
        let (mut fs, normalized_path, link_path) = self.lock_resolve(path.as_ref(), false)?;

        match fs.get(&link_path) {
            Some(TestFsEntry::File(file)) => match lock_file_read(file).state {
                BorrowState::Read(0) => {}
                BorrowState::Read(count) => {
                    return Err(fs_error!(
                        "File could not be removed, it is already borrowed for reading {count} times: {}",
                        normalized_path.display()
                    ));
                }
                BorrowState::Write => {
                    return Err(fs_error!(
                        "File could not be removed, it is already borrowed for writing: {}",
                        normalized_path.display()
                    ));
                }
            },
            Some(TestFsEntry::Dir(_)) => {
                if link_path.parent().is_none() {
                    return Err(fs_error!(
                        "Directory could not be removed, path is a root: {}",
                        normalized_path.display()
                    ));
                }

                if has_children(&fs, &link_path) {
                    return Err(fs_error!(
                        "Directory could not be removed, it is not empty: {}",
                        normalized_path.display()
                    ));
                }
            }
            Some(TestFsEntry::Symlink(_)) => {}
            None => {
                return Err(fs_error!(
                    "Path could not be removed, path not found: {}",
                    normalized_path.display()
                ));
            }
        }

        if let Some(TestFsEntry::Symlink(_)) = fs.remove(&link_path) {
            self.symlinks.fetch_sub(1, Ordering::Relaxed);
        }

        touch_parent(&mut fs, &link_path, SystemTime::now());

        Ok(())
    }

    fn remove_dir_all<P: AsRef<Path>>(&self, path: P) -> Result {
        let (mut fs, normalized_path, link_path) = self.lock_resolve(path.as_ref(), false)?;
        check_not_root(&link_path)?;

        match fs.get(&link_path) {
            Some(TestFsEntry::Dir(_)) | Some(TestFsEntry::Symlink(_)) => {}
            Some(TestFsEntry::File(_)) => {
                return Err(fs_error!(
                    "Directory could not be removed, path is not a directory: {}",
                    normalized_path.display()
                ));
            }
            None => {
                return Err(fs_error!(
                    "Directory could not be removed, path not found: {}",
                    normalized_path.display()
                ));
            }
        }

        // WHY: a symlink has no descendants, so only the symlink itself is
        // removed.
        let tree: Vec<PathBuf> = fs
            .range::<Path, _>((Bound::Included(link_path.as_path()), Bound::Unbounded))
            .take_while(|(path, _)| path.starts_with(&link_path))
            .map(|(path, _)| path.clone())
            .collect();

        for path in &tree {
            if let Some(TestFsEntry::File(file)) = fs.get(path) {
                let display_path = || {
                    normalized_path
                        .join(path.strip_prefix(&link_path).unwrap_or(path))
                        .display()
                        .to_string()
                };

                match lock_file_read(file).state {
                    BorrowState::Read(0) => {}
                    BorrowState::Read(count) => {
                        return Err(fs_error!(
                            "File could not be removed, it is already borrowed for reading {count} times: {}",
                            display_path()
                        ));
                    }
                    BorrowState::Write => {
                        return Err(fs_error!(
                            "File could not be removed, it is already borrowed for writing: {}",
                            display_path()
                        ));
                    }
                }
            }
        }

        for path in &tree {
            if let Some(TestFsEntry::Symlink(_)) = fs.remove(path) {
                self.symlinks.fetch_sub(1, Ordering::Relaxed);
            }
        }

        touch_parent(&mut fs, &link_path, SystemTime::now());

        Ok(())
    }

    fn rename<S: AsRef<Path>, T: AsRef<Path>>(&self, source: S, target: T) -> Result {
        let normalized_source = self.normalize(source.as_ref())?;
        let (mut fs, normalized_target, target_path) = self.lock_resolve(target.as_ref(), false)?;
        let source_path = self.resolve(&fs, &normalized_source, false)?;

        let Some(source_entry) = fs.get(&source_path) else {
            return Err(fs_error!(
                "Path could not be renamed, source not found: {}",
                normalized_source.display()
            ));
        };

        if source_path == target_path {
            return Ok(());
        }

        if source_path.parent().is_none() {
            return Err(fs_error!(
                "Path could not be renamed, source is a root: {}",
                normalized_source.display()
            ));
        }

        if !target_path
            .parent()
            .is_some_and(|parent| is_dir(&fs, parent))
        {
            return Err(fs_error!(
                "Path could not be renamed, parent directory does not exist: {}",
                normalized_target.display()
            ));
        }

        if target_path.starts_with(&source_path) {
            return Err(fs_error!(
                "Directory could not be renamed, target is inside source: {}",
                normalized_target.display()
            ));
        }

        // WHY: the rules of POSIX `rename`, which `std::fs::rename` follows
        // on every platform.
        match (source_entry, fs.get(&target_path)) {
            (_, None) => {}
            (TestFsEntry::File(source_file), Some(TestFsEntry::File(target_file)))
                if Arc::ptr_eq(source_file, target_file) =>
            {
                fs.remove(&source_path);
                touch_parent(&mut fs, &source_path, SystemTime::now());
                return Ok(());
            }
            (TestFsEntry::Dir(_), Some(TestFsEntry::Dir(_))) => {
                if has_children(&fs, &target_path) {
                    return Err(fs_error!(
                        "Directory could not be renamed, target is not empty: {}",
                        normalized_target.display()
                    ));
                }
            }
            (TestFsEntry::Dir(_), Some(_)) => {
                return Err(fs_error!(
                    "Directory could not be renamed, target is not a directory: {}",
                    normalized_target.display()
                ));
            }
            (_, Some(TestFsEntry::Dir(_))) => {
                return Err(fs_error!(
                    "Path could not be renamed, target is a directory: {}",
                    normalized_target.display()
                ));
            }
            (_, Some(TestFsEntry::File(target_file))) => match lock_file_read(target_file).state {
                BorrowState::Read(0) => {}
                BorrowState::Read(count) => {
                    return Err(fs_error!(
                        "File could not be replaced, it is already borrowed for reading {count} times: {}",
                        normalized_target.display()
                    ));
                }
                BorrowState::Write => {
                    return Err(fs_error!(
                        "File could not be replaced, it is already borrowed for writing: {}",
                        normalized_target.display()
                    ));
                }
            },
            (_, Some(TestFsEntry::Symlink(_))) => {}
        }

        if let Some(TestFsEntry::Symlink(_)) = fs.remove(&target_path) {
            self.symlinks.fetch_sub(1, Ordering::Relaxed);
        }

        let tree: Vec<PathBuf> = fs
            .range::<Path, _>((Bound::Included(source_path.as_path()), Bound::Unbounded))
            .take_while(|(path, _)| path.starts_with(&source_path))
            .map(|(path, _)| path.clone())
            .collect();
        let now = SystemTime::now();

        let entries: Vec<(PathBuf, TestFsEntry)> = tree
            .into_iter()
            .filter_map(|path| fs.remove(&path).map(|entry| (path, entry)))
            .collect();

        for (path, entry) in entries {
            let moved = target_path.join(path.strip_prefix(&source_path).unwrap_or(&path));
            fs.insert(moved, entry);
        }

        touch_parent(&mut fs, &source_path, now);
        touch_parent(&mut fs, &target_path, now);

        Ok(())
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
        let (mut fs, normalized_path, resolved_path) = self.lock_resolve(path.as_ref(), true)?;

        match fs.get_mut(&resolved_path) {
            Some(TestFsEntry::File(file)) => lock_file_write(file).modified = modified,
            Some(TestFsEntry::Dir(dir)) => dir.modified = modified,
            Some(TestFsEntry::Symlink(_)) | None => {
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
    /// Adds a symlink at `link_path` to the native `target`, see
    /// [`Fs::create_symlink`].
    fn add_symlink(
        &self,
        fs: &mut TestFsGuard<'_>,
        normalized_path: &Path,
        link_path: PathBuf,
        target: PathBuf,
    ) -> Result {
        if fs.contains_key(&link_path) {
            return Err(fs_error!(
                "Symlink could not be created, path already exists: {}",
                normalized_path.display()
            ));
        }

        let Some(parent) = link_path.parent().filter(|parent| is_dir(fs, parent)) else {
            return Err(fs_error!(
                "Symlink could not be created, parent directory does not exist: {}",
                normalized_path.display()
            ));
        };

        // WHY: always resolved in full, the joined target is not normalized
        // and may contain `..`.
        let resolved_target = resolve_symlinks(fs, &parent.join(&target), true)?;

        // WHY: `Path::components` ignores an empty target, which the OS
        // rejects.
        if target.as_os_str().is_empty()
            || !matches!(
                fs.get(&resolved_target),
                Some(TestFsEntry::File(_)) | Some(TestFsEntry::Dir(_))
            )
        {
            return Err(fs_error!(
                "Symlink could not be created, target not found: {}",
                target.display()
            ));
        }

        if ends_with_separator(&target) && !is_dir(fs, &resolved_target) {
            return Err(fs_error!(
                "Symlink could not be created, target is not a directory: {}",
                target.display()
            ));
        }

        let now = SystemTime::now();
        touch_parent(fs, &link_path, now);
        fs.insert(
            link_path,
            TestFsEntry::Symlink(TestFsSymlink::new(target, now)),
        );
        self.symlinks.fetch_add(1, Ordering::Relaxed);

        Ok(())
    }

    fn set_attribute(&self, path: &Path, attribute: Attribute, value: bool) -> Result {
        let (mut fs, normalized_path, resolved_path) = self.lock_resolve(path, true)?;

        match fs.get_mut(&resolved_path) {
            #[cfg(unix)]
            Some(TestFsEntry::Dir(_)) if matches!(attribute, Attribute::Executable) => {
                return Err(fs_error!(
                    "Executable attribute cannot be set, path is a directory: {}",
                    normalized_path.display()
                ));
            }
            Some(TestFsEntry::File(file)) => lock_file_write(file).attributes.set(attribute, value),
            Some(TestFsEntry::Dir(dir)) => dir.attributes.set(attribute, value),
            Some(TestFsEntry::Symlink(_)) | None => {
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

    /// Inserts a symlink without checking its target, which
    /// [`Fs::create_symlink`] requires to exist.
    fn insert_symlink(fs: &TestFs, path: &str, target: &str) {
        lock_handles(&fs.fs).insert(
            normalize_path(&fs.current_dir, path),
            TestFsEntry::Symlink(TestFsSymlink::new(target.into(), SystemTime::now())),
        );
        fs.symlinks.fetch_add(1, Ordering::Relaxed);
    }

    mod test_fs {
        use super::*;

        #[test]
        fn test_fs_should_borrow_hard_linked_file_once() {
            // GIVEN: a file with a second name
            let fs = TestFs::new();
            fs.write("/a.txt", "aaa").unwrap();
            fs.create_hard_link("/b.txt", "/a.txt").unwrap();

            // WHEN: the file is opened for reading by one name
            let _read = fs.open_read("/a.txt").unwrap();

            // THEN: opening it for writing by the other name fails
            assert_eq!(
                fs.open_write("/b.txt").err().unwrap().to_string(),
                fs_error!(
                    "File could not be opened, it is already borrowed for reading 1 times: {}",
                    normalize_path(&fs.current_dir, "/b.txt").display()
                )
                .to_string()
            );
        }

        #[test]
        fn test_fs_should_borrow_symlinked_file_under_its_target() {
            // GIVEN: a file and a symlink to it
            let fs = TestFs::new();
            fs.write("/a.txt", "aaa").unwrap();
            fs.create_symlink("/link", "a.txt").unwrap();

            // WHEN: the file is opened for writing through the symlink
            let _write = fs.open_write("/link").unwrap();

            // THEN: opening the file itself fails
            assert_eq!(
                fs.open_read("/a.txt").err().unwrap().to_string(),
                fs_error!(
                    "File could not be opened, it is already borrowed for writing: {}",
                    normalize_path(&fs.current_dir, "/a.txt").display()
                )
                .to_string()
            );
        }

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

        mod resolve {
            use super::*;

            #[test]
            fn resolve_should_walk_components_only_once_symlinks_exist() {
                let fs = TestFs::new();
                fs.create_dir_all("/dir").unwrap();
                let path = normalize_path(&fs.current_dir, "/link/a.txt");

                assert_eq!(
                    fs.resolve(&lock_handles(&fs.fs), &path, true).unwrap(),
                    path
                );

                fs.create_symlink("/link", "dir").unwrap();

                assert_eq!(
                    fs.resolve(&lock_handles(&fs.fs), &path, true).unwrap(),
                    normalize_path(&fs.current_dir, "/dir/a.txt")
                );
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

                #[test]
                fn open_read_should_follow_symlinks() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/a/b").unwrap();
                    fs.write("/a/b/c.txt", "ccc").unwrap();
                    fs.create_symlink("/a/b/file_link", "c.txt").unwrap();
                    fs.create_symlink("/dir_link", "/a/b").unwrap();
                    fs.create_symlink("/a/b/up_link", "../b/file_link").unwrap();
                    fs.create_symlink("/chain_link", "dir_link/up_link")
                        .unwrap();

                    for path in [
                        "/a/b/file_link",
                        "/dir_link/c.txt",
                        "/dir_link/up_link",
                        "/chain_link",
                    ] {
                        assert_eq!(fs.read_to_string(path).unwrap(), "ccc", "path: {path}");
                    }
                }

                #[test]
                fn open_read_symlink_loop_should_fail() {
                    let fs = TestFs::new();
                    insert_symlink(&fs, "/a", "b");
                    insert_symlink(&fs, "/b", "a");

                    assert_eq!(
                        fs.open_read("/a").err().unwrap().to_string(),
                        fs_error!(
                            "Too many levels of symbolic links: {}",
                            normalize_path(&fs.current_dir, "/a").display()
                        )
                        .to_string()
                    );
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
                        .map(|e| e.unwrap().file_name().to_string())
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

                #[test]
                fn open_write_options_create_dangling_symlink_should_create_target() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    insert_symlink(&fs, "/link", "dir/a.txt");

                    fs.write("/link", "aaa").unwrap();

                    assert_eq!(fs.read_to_string("/dir/a.txt").unwrap(), "aaa");
                    assert!(fs.symlink_metadata("/link").unwrap().file_type.is_symlink());
                }

                #[test]
                fn open_write_options_create_new_symlink_should_fail() {
                    let fs = TestFs::new();
                    insert_symlink(&fs, "/link", "a.txt");

                    assert_eq!(
                        fs.open_write_options("/link", OpenWriteOptions::new().create_new())
                            .err()
                            .unwrap()
                            .to_string(),
                        fs_error!(
                            "File could not be opened with 'create_new' option, file already exists: {}",
                            normalize_path(&fs.current_dir, "/link").display()
                        )
                        .to_string()
                    );
                    assert!(!fs.exists("/a.txt").unwrap());
                }

                #[test]
                fn open_write_options_symlink_loop_should_fail() {
                    let fs = TestFs::new();
                    insert_symlink(&fs, "/link", "link");

                    assert!(fs.write("/link", "aaa").is_err());
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
                        .map(|e| e.unwrap().file_name().to_string())
                        .collect();
                    names.sort();
                    assert_eq!(names, vec!["a.txt", "b.txt"]);
                }

                #[test]
                fn read_dir_should_distinguish_files_and_directories() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/mydir/subdir").unwrap();
                    fs.write("/mydir/file.txt", "data").unwrap();

                    let entries: Vec<DirEntry> = fs
                        .read_dir("/mydir")
                        .unwrap()
                        .collect::<Result<_>>()
                        .unwrap();

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
                        .map(|e| e.unwrap().file_name().to_string())
                        .collect();
                    names.sort();
                    assert_eq!(names, vec!["a.txt", "sub"]);
                }

                #[test]
                fn read_dir_should_return_empty_for_empty_directory() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/emptydir").unwrap();

                    let entries: Vec<DirEntry> = fs
                        .read_dir("/emptydir")
                        .unwrap()
                        .collect::<Result<_>>()
                        .unwrap();
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
                        .map(|e| e.unwrap().file_name().to_string())
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
                        .map(|e| e.unwrap().file_name().to_string())
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

                #[test]
                fn read_dir_should_list_symlinks_without_following() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/dir/a.txt", "aaa").unwrap();
                    fs.create_symlink("/dir/link", "a.txt").unwrap();

                    let entries: Vec<(String, FileType)> = fs
                        .read_dir("/dir")
                        .unwrap()
                        .map(|e| e.unwrap())
                        .map(|e| (e.file_name().to_string(), e.file_type()))
                        .collect();
                    assert_eq!(
                        entries,
                        vec![
                            ("a.txt".to_string(), FileType::File),
                            ("link".to_string(), FileType::Symlink)
                        ]
                    );
                }

                #[test]
                fn read_dir_symlink_should_list_target_entries_under_symlink_path() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/dir/a.txt", "aaa").unwrap();
                    fs.create_symlink("/link", "dir").unwrap();

                    let paths: Vec<PathBuf> = fs
                        .read_dir("/link")
                        .unwrap()
                        .map(|e| e.unwrap().path().to_path_buf())
                        .collect();
                    assert_eq!(paths, vec![normalize_path(&fs.current_dir, "/link/a.txt")]);
                }

                #[test]
                fn read_dir_dangling_symlink_should_fail() {
                    let fs = TestFs::new();
                    insert_symlink(&fs, "/link", "dir");

                    assert_eq!(
                        fs.read_dir("/link").err().unwrap().to_string(),
                        fs_error!(
                            "Directory not found: {}",
                            normalize_path(&fs.current_dir, "/link").display()
                        )
                        .to_string()
                    );
                }
            }

            mod create_dir {
                use super::*;

                #[test]
                fn create_dir_should_create_empty_dir_and_update_parent_modified() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.set_modified("/dir", SystemTime::UNIX_EPOCH).unwrap();

                    fs.create_dir("/dir/sub").unwrap();

                    assert!(fs.metadata("/dir/sub").unwrap().file_type.is_dir());
                    assert!(fs.read_dir("/dir/sub").unwrap().next().is_none());
                    assert!(fs.metadata("/dir").unwrap().modified > SystemTime::UNIX_EPOCH);
                }

                #[test]
                fn create_dir_concurrently_should_succeed_once() {
                    let fs = TestFs::new();

                    let created = std::thread::scope(|scope| {
                        let threads: Vec<_> = (0..8)
                            .map(|_| scope.spawn(|| fs.create_dir("/lock").is_ok()))
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
                fn create_dir_should_follow_parent_symlinks() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.create_symlink("/link", "dir").unwrap();

                    fs.create_dir("/link/sub").unwrap();

                    assert!(fs.metadata("/dir/sub").unwrap().file_type.is_dir());
                }

                #[test]
                fn create_dir_existing_path_should_fail() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.create_symlink("/link", "dir").unwrap();
                    insert_symlink(&fs, "/dangling", "missing");

                    for path in ["/dir", "/a.txt", "/link", "/dangling"] {
                        assert_eq!(
                            fs.create_dir(path).err().unwrap().to_string(),
                            fs_error!(
                                "Directory could not be created, path already exists: {}",
                                normalize_path(&fs.current_dir, path).display()
                            )
                            .to_string(),
                            "path: {path}"
                        );
                    }
                    assert!(!fs.exists("/missing").unwrap());
                }

                #[test]
                fn create_dir_without_parent_dir_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();

                    for path in ["/missing/dir", "/a.txt/dir"] {
                        assert_eq!(
                            fs.create_dir(path).err().unwrap().to_string(),
                            fs_error!(
                                "Directory could not be created, parent directory does not exist: {}",
                                normalize_path(&fs.current_dir, path).display()
                            )
                            .to_string(),
                            "path: {path}"
                        );
                    }
                }

                #[test]
                fn create_dir_invalid_path_should_fail() {
                    let fs = TestFs::new();
                    insert_symlink(&fs, "/loop", "loop");

                    assert!(fs.create_dir("\0").is_err());
                    assert!(fs.create_dir("/loop/dir").is_err());
                }
            }

            mod create_dir_all {
                use super::*;

                #[test]
                fn create_dir_all_should_create_directory_and_parents() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/a/b/c").unwrap();

                    let entries: Vec<DirEntry> =
                        fs.read_dir("/a").unwrap().collect::<Result<_>>().unwrap();
                    assert_eq!(entries.len(), 1);
                    assert_eq!(entries[0].file_name(), "b");
                    assert!(
                        entries[0].file_type().is_dir(),
                        "expected 'b' to be a directory"
                    );

                    let entries: Vec<DirEntry> =
                        fs.read_dir("/a/b").unwrap().collect::<Result<_>>().unwrap();
                    assert_eq!(entries.len(), 1);
                    assert_eq!(entries[0].file_name(), "c");

                    let entries: Vec<DirEntry> = fs
                        .read_dir("/a/b/c")
                        .unwrap()
                        .collect::<Result<_>>()
                        .unwrap();
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

                    let entries: Vec<DirEntry> = fs
                        .read_dir("/mydir")
                        .unwrap()
                        .collect::<Result<_>>()
                        .unwrap();
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

                #[test]
                fn create_dir_all_should_follow_symlinks_to_dirs() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.create_symlink("/link", "dir").unwrap();

                    fs.create_dir_all("/link").unwrap();
                    fs.create_dir_all("/link/a/b").unwrap();

                    assert!(fs.metadata("/dir/a/b").unwrap().file_type.is_dir());
                    assert!(fs.symlink_metadata("/link").unwrap().file_type.is_symlink());
                }

                #[test]
                fn create_dir_all_symlink_to_file_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.create_symlink("/link", "a.txt").unwrap();

                    assert_eq!(
                        fs.create_dir_all("/link/b").err().unwrap().to_string(),
                        fs_error!(
                            "Cannot create directory, a file already exists at: {}",
                            normalize_path(&fs.current_dir, "/link").display()
                        )
                        .to_string()
                    );
                }

                #[test]
                fn create_dir_all_dangling_symlink_should_fail() {
                    let fs = TestFs::new();
                    insert_symlink(&fs, "/link", "dir");

                    assert_eq!(
                        fs.create_dir_all("/link/b").err().unwrap().to_string(),
                        fs_error!(
                            "Cannot create directory, a dangling symlink exists at: {}",
                            normalize_path(&fs.current_dir, "/link").display()
                        )
                        .to_string()
                    );
                    assert!(!fs.exists("/dir").unwrap());
                }

                #[test]
                fn create_dir_all_symlink_loop_should_fail() {
                    let fs = TestFs::new();
                    insert_symlink(&fs, "/link", "link");

                    assert!(fs.create_dir_all("/link").is_err());
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

                #[test]
                fn exists_should_follow_symlinks() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.create_symlink("/link", "a.txt").unwrap();
                    insert_symlink(&fs, "/dangling", "missing.txt");

                    assert!(fs.exists("/link").unwrap());
                    assert!(!fs.exists("/dangling").unwrap());
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

                #[test]
                fn metadata_should_follow_symlinks() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "abc").unwrap();
                    fs.create_symlink("/link", "a.txt").unwrap();

                    let metadata = fs.metadata("/link").unwrap();

                    assert_eq!((metadata.file_type, metadata.len), (FileType::File, 3));
                }
            }

            mod symlink_metadata {
                use super::*;

                #[test]
                fn symlink_metadata_should_not_follow_symlink() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "abcdef").unwrap();
                    fs.create_symlink("/link", "a.txt").unwrap();

                    let metadata = fs.symlink_metadata("/link").unwrap();

                    assert_eq!(metadata.file_type, FileType::Symlink);
                    assert_eq!(metadata.created, metadata.modified);
                    #[cfg(unix)]
                    assert_eq!(
                        (metadata.len, metadata.mode, metadata.uid),
                        (5, 0o777, 1000)
                    );
                }

                #[test]
                fn symlink_metadata_should_follow_parent_symlinks() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/dir/a.txt", "abc").unwrap();
                    fs.create_symlink("/link", "dir").unwrap();

                    assert_eq!(
                        fs.symlink_metadata("/link/a.txt").unwrap().file_type,
                        FileType::File
                    );
                }

                #[test]
                fn symlink_metadata_invalid_path_should_fail() {
                    let fs = TestFs::new();
                    assert!(fs.symlink_metadata("\0").is_err());
                }
            }

            mod create_symlink {
                use super::*;

                #[test]
                fn create_symlink_should_link_file_and_dir() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/dir/a.txt", "aaa").unwrap();

                    fs.create_symlink("/file_link", "/dir/a.txt").unwrap();
                    fs.create_symlink("/dir_link", "/dir").unwrap();

                    assert_eq!(fs.read_to_string("/file_link").unwrap(), "aaa");
                    assert_eq!(fs.read_to_string("/dir_link/a.txt").unwrap(), "aaa");
                }

                #[test]
                fn create_symlink_relative_target_should_resolve_against_symlink_parent() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/a.txt", "aaa").unwrap();

                    fs.create_symlink("/dir/link", "../a.txt").unwrap();

                    assert_eq!(fs.read_to_string("/dir/link").unwrap(), "aaa");
                }

                #[test]
                fn create_symlink_should_update_parent_modified() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.set_modified("/", SystemTime::UNIX_EPOCH).unwrap();

                    fs.create_symlink("/link", "a.txt").unwrap();

                    assert!(fs.metadata("/").unwrap().modified > SystemTime::UNIX_EPOCH);
                }

                #[test]
                fn create_symlink_existing_path_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    insert_symlink(&fs, "/dangling", "missing.txt");

                    for (path, target) in [
                        ("/a.txt", "a.txt"),
                        ("/dangling", "a.txt"),
                        ("/a.txt", "missing.txt"),
                    ] {
                        assert_eq!(
                            fs.create_symlink(path, target).err().unwrap().to_string(),
                            fs_error!(
                                "Symlink could not be created, path already exists: {}",
                                normalize_path(&fs.current_dir, path).display()
                            )
                            .to_string(),
                            "path: {path}, target: {target}"
                        );
                    }
                }

                #[test]
                fn create_symlink_empty_target_should_fail() {
                    let fs = TestFs::new();

                    assert_eq!(
                        fs.create_symlink("/link", "").err().unwrap().to_string(),
                        fs_error!("Symlink could not be created, target not found: ").to_string()
                    );
                    assert!(!fs.exists("/link").unwrap());
                }

                #[test]
                fn create_symlink_target_with_trailing_separator_should_require_dir() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/a.txt", "aaa").unwrap();

                    fs.create_symlink("/dir_link", "dir/").unwrap();

                    assert!(fs.metadata("/dir_link").unwrap().file_type.is_dir());
                    assert_eq!(
                        fs.create_symlink("/file_link", "a.txt/")
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
                fn create_symlink_without_parent_dir_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();

                    for path in ["/missing/link", "/a.txt/link"] {
                        assert_eq!(
                            fs.create_symlink(path, "/a.txt").err().unwrap().to_string(),
                            fs_error!(
                                "Symlink could not be created, parent directory does not exist: {}",
                                normalize_path(&fs.current_dir, path).display()
                            )
                            .to_string(),
                            "path: {path}"
                        );
                    }
                }

                #[test]
                fn create_symlink_nonexistent_target_should_fail() {
                    let fs = TestFs::new();
                    insert_symlink(&fs, "/dangling", "missing.txt");

                    for target in ["missing.txt", "dangling"] {
                        assert_eq!(
                            fs.create_symlink("/link", target)
                                .err()
                                .unwrap()
                                .to_string(),
                            fs_error!("Symlink could not be created, target not found: {target}")
                                .to_string(),
                            "target: {target}"
                        );
                    }
                    assert!(!fs.exists("/link").unwrap());
                }

                #[test]
                fn create_symlink_target_loop_should_fail() {
                    let fs = TestFs::new();
                    insert_symlink(&fs, "/loop", "loop");

                    assert!(fs.create_symlink("/link", "loop").is_err());
                }

                #[test]
                fn create_symlink_invalid_path_should_fail() {
                    let fs = TestFs::new();
                    assert!(fs.create_symlink("\0", "a.txt").is_err());
                }
            }

            mod read_link {
                use super::*;

                #[test]
                fn read_link_should_return_target_as_given() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.create_symlink("/dir/link", "./../a.txt").unwrap();

                    #[cfg(unix)]
                    let expected = "./../a.txt";
                    // WHY: `/` is stored as `\` like on a real Windows filesystem.
                    #[cfg(windows)]
                    let expected = r".\..\a.txt";
                    assert_eq!(fs.read_link("/dir/link").unwrap(), Path::new(expected));
                }

                #[test]
                fn read_link_non_symlink_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();

                    for path in ["/a.txt", "/"] {
                        assert_eq!(
                            fs.read_link(path).err().unwrap().to_string(),
                            fs_error!(
                                "Symlink could not be read, path is not a symlink: {}",
                                normalize_path(&fs.current_dir, path).display()
                            )
                            .to_string(),
                            "path: {path}"
                        );
                    }
                }

                #[test]
                fn read_link_should_fail_for_nonexistent_path() {
                    let fs = TestFs::new();
                    assert_eq!(
                        fs.read_link("/link").err().unwrap().to_string(),
                        fs_error!(
                            "Symlink could not be read, path not found: {}",
                            normalize_path(&fs.current_dir, "/link").display()
                        )
                        .to_string()
                    );
                }

                #[test]
                fn read_link_invalid_path_should_fail() {
                    let fs = TestFs::new();
                    assert!(fs.read_link("\0").is_err());
                }
            }

            mod create_hard_link {
                use super::*;

                #[test]
                fn create_hard_link_should_share_file() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();

                    fs.create_hard_link("/b.txt", "/a.txt").unwrap();
                    fs.write("/b.txt", "bbb").unwrap();
                    fs.set_readonly("/b.txt", true).unwrap();
                    fs.set_modified("/b.txt", SystemTime::UNIX_EPOCH).unwrap();

                    assert_eq!(fs.read_to_string("/a.txt").unwrap(), "bbb");
                    assert_eq!(
                        fs.metadata("/a.txt").unwrap(),
                        fs.metadata("/b.txt").unwrap()
                    );
                    assert!(fs.metadata("/a.txt").unwrap().readonly);
                }

                #[test]
                fn create_hard_link_relative_target_should_resolve_against_current_dir() {
                    let mut fs = TestFs::new();
                    let dir = normalize_path(&fs.current_dir, "/dir");
                    fs.create_dir_all(&dir).unwrap();
                    fs.write("/dir/a.txt", "aaa").unwrap();
                    fs.set_current_dir(dir);

                    fs.create_hard_link("b.txt", "a.txt").unwrap();

                    assert_eq!(fs.read_to_string("b.txt").unwrap(), "aaa");
                }

                #[test]
                fn create_hard_link_symlink_target_should_link_file() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.create_symlink("/link", "a.txt").unwrap();

                    fs.create_hard_link("/b.txt", "/link").unwrap();

                    assert_eq!(
                        fs.symlink_metadata("/b.txt").unwrap().file_type,
                        FileType::File
                    );
                    assert_eq!(fs.read_to_string("/b.txt").unwrap(), "aaa");
                }

                #[test]
                fn create_hard_link_should_update_parent_modified() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.set_modified("/dir", SystemTime::UNIX_EPOCH).unwrap();

                    fs.create_hard_link("/dir/b.txt", "/a.txt").unwrap();

                    assert!(fs.metadata("/dir").unwrap().modified > SystemTime::UNIX_EPOCH);
                }

                #[test]
                fn create_hard_link_existing_path_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    insert_symlink(&fs, "/dangling", "missing.txt");

                    for (path, target) in [
                        ("/a.txt", "/a.txt"),
                        ("/dangling", "/a.txt"),
                        ("/a.txt", "/missing.txt"),
                    ] {
                        assert_eq!(
                            fs.create_hard_link(path, target).err().unwrap().to_string(),
                            fs_error!(
                                "Hard link could not be created, path already exists: {}",
                                normalize_path(&fs.current_dir, path).display()
                            )
                            .to_string(),
                            "path: {path}, target: {target}"
                        );
                    }
                }

                #[test]
                fn create_hard_link_without_parent_dir_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();

                    for path in ["/missing/link", "/a.txt/link"] {
                        assert_eq!(
                            fs.create_hard_link(path, "/a.txt")
                                .err()
                                .unwrap()
                                .to_string(),
                            fs_error!(
                                "Hard link could not be created, parent directory does not exist: {}",
                                normalize_path(&fs.current_dir, path).display()
                            )
                            .to_string(),
                            "path: {path}"
                        );
                    }
                }

                #[test]
                fn create_hard_link_nonexistent_target_should_fail() {
                    let fs = TestFs::new();
                    insert_symlink(&fs, "/dangling", "missing.txt");

                    for target in ["/missing.txt", "/dangling"] {
                        assert_eq!(
                            fs.create_hard_link("/link", target)
                                .err()
                                .unwrap()
                                .to_string(),
                            fs_error!(
                                "Hard link could not be created, target not found: {}",
                                normalize_path(&fs.current_dir, target).display()
                            )
                            .to_string(),
                            "target: {target}"
                        );
                    }
                }

                #[test]
                fn create_hard_link_dir_target_should_fail() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();

                    assert_eq!(
                        fs.create_hard_link("/link", "/dir")
                            .err()
                            .unwrap()
                            .to_string(),
                        fs_error!(
                            "Hard link could not be created, target is a directory: {}",
                            normalize_path(&fs.current_dir, "/dir").display()
                        )
                        .to_string()
                    );
                }

                #[test]
                fn create_hard_link_target_loop_should_fail() {
                    let fs = TestFs::new();
                    insert_symlink(&fs, "/loop", "loop");

                    assert!(fs.create_hard_link("/link", "/loop").is_err());
                }

                #[test]
                fn create_hard_link_invalid_path_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();

                    assert!(fs.create_hard_link("\0", "/a.txt").is_err());
                    assert!(fs.create_hard_link("/link", "\0").is_err());
                }
            }

            mod copy {
                use super::*;

                #[test]
                fn copy_hard_linked_file_should_create_independent_file() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.create_hard_link("/b.txt", "/a.txt").unwrap();

                    fs.copy("/b.txt", "/c.txt").unwrap();
                    fs.write("/c.txt", "ccc").unwrap();

                    assert_eq!(fs.read_to_string("/a.txt").unwrap(), "aaa");
                    assert_eq!(fs.read_to_string("/c.txt").unwrap(), "ccc");
                }

                #[test]
                fn copy_empty_dir_should_create_empty_dir() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();

                    fs.copy("/dir", "/copy").unwrap();

                    assert!(fs.metadata("/copy").unwrap().file_type.is_dir());
                    assert!(fs.read_dir("/copy").unwrap().next().is_none());
                }

                #[test]
                fn copy_should_copy_only_permissions() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.create_dir_all("/dir").unwrap();

                    for source in ["/a.txt", "/dir"] {
                        fs.set_readonly(source, true).unwrap();
                        fs.set_modified(source, SystemTime::UNIX_EPOCH).unwrap();
                        #[cfg(unix)]
                        fs.set_owner(source, 0, 0).unwrap();
                    }
                    #[cfg(windows)]
                    fs.set_hidden("/a.txt", true).unwrap();

                    for (source, target) in [("/a.txt", "/b.txt"), ("/dir", "/copy")] {
                        fs.copy(source, target).unwrap();
                        let metadata = fs.metadata(target).unwrap();
                        assert!(metadata.readonly, "source: {source}");
                        assert!(
                            metadata.modified > SystemTime::UNIX_EPOCH,
                            "source: {source}"
                        );
                        assert!(!metadata.hidden, "source: {source}");
                        #[cfg(unix)]
                        assert_eq!(
                            (metadata.mode, metadata.uid, metadata.gid),
                            (fs.metadata(source).unwrap().mode, OWNER_ID, OWNER_ID),
                            "source: {source}"
                        );
                    }
                }

                #[test]
                fn copy_symlink_should_create_symlink_with_same_target() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.create_symlink("/file_link", "a.txt").unwrap();
                    fs.create_symlink("/dir_link", "dir").unwrap();

                    for (source, target) in
                        [("/file_link", "/file_copy"), ("/dir_link", "/dir_copy")]
                    {
                        fs.copy(source, target).unwrap();
                        assert_eq!(
                            fs.read_link(target).unwrap(),
                            fs.read_link(source).unwrap(),
                            "source: {source}"
                        );
                    }
                    assert_eq!(fs.read_to_string("/file_copy").unwrap(), "aaa");
                    assert!(fs.metadata("/dir_copy").unwrap().file_type.is_dir());
                    assert_eq!(fs.symlinks.load(Ordering::Relaxed), 4);
                }

                #[test]
                fn copy_symlink_should_resolve_against_target_parent() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/dir/a.txt", "aaa").unwrap();
                    fs.create_symlink("/dir/link", "a.txt").unwrap();

                    assert_eq!(
                        fs.copy("/dir/link", "/link").err().unwrap().to_string(),
                        fs_error!("Symlink could not be created, target not found: a.txt")
                            .to_string()
                    );
                    assert!(fs.symlink_metadata("/link").is_err());
                }

                #[test]
                fn copy_should_follow_parent_symlinks() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/dir/a.txt", "aaa").unwrap();
                    fs.create_symlink("/link", "dir").unwrap();

                    fs.copy("/link/a.txt", "/link/b.txt").unwrap();

                    assert_eq!(fs.read_to_string("/dir/b.txt").unwrap(), "aaa");
                }

                #[test]
                fn copy_should_update_parent_modified() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.set_modified("/dir", SystemTime::UNIX_EPOCH).unwrap();

                    fs.copy("/a.txt", "/dir/b.txt").unwrap();

                    assert!(fs.metadata("/dir").unwrap().modified > SystemTime::UNIX_EPOCH);
                }

                #[test]
                fn copy_should_allow_read_borrowed_source() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    let _read = fs.open_read("/a.txt").unwrap();

                    fs.copy("/a.txt", "/b.txt").unwrap();

                    assert_eq!(fs.read_to_string("/b.txt").unwrap(), "aaa");
                }

                #[test]
                fn copy_write_borrowed_source_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    let _write = fs.open_write("/a.txt").unwrap();

                    assert_eq!(
                        fs.copy("/a.txt", "/b.txt").err().unwrap().to_string(),
                        fs_error!(
                            "File could not be copied, it is already borrowed for writing: {}",
                            normalize_path(&fs.current_dir, "/a.txt").display()
                        )
                        .to_string()
                    );
                    assert!(!fs.exists("/b.txt").unwrap());
                }

                #[test]
                fn copy_existing_target_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.write("/b.txt", "bbb").unwrap();
                    insert_symlink(&fs, "/dangling", "missing.txt");

                    for target in ["/b.txt", "/dangling"] {
                        assert_eq!(
                            fs.copy("/a.txt", target).err().unwrap().to_string(),
                            fs_error!(
                                "Path could not be copied, target already exists: {}",
                                normalize_path(&fs.current_dir, target).display()
                            )
                            .to_string(),
                            "target: {target}"
                        );
                    }
                    assert_eq!(fs.read_to_string("/b.txt").unwrap(), "bbb");
                }

                #[test]
                fn copy_without_target_parent_dir_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();

                    for target in ["/missing/b.txt", "/a.txt/b.txt"] {
                        assert_eq!(
                            fs.copy("/a.txt", target).err().unwrap().to_string(),
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
                    let fs = TestFs::new();

                    assert_eq!(
                        fs.copy("/missing.txt", "/b.txt").err().unwrap().to_string(),
                        fs_error!(
                            "Path could not be copied, source not found: {}",
                            normalize_path(&fs.current_dir, "/missing.txt").display()
                        )
                        .to_string()
                    );
                }

                #[test]
                fn copy_non_empty_dir_should_fail() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir/sub").unwrap();

                    assert_eq!(
                        fs.copy("/dir", "/copy").err().unwrap().to_string(),
                        fs_error!(
                            "Directory could not be copied, it is not empty: {}",
                            normalize_path(&fs.current_dir, "/dir").display()
                        )
                        .to_string()
                    );
                    assert!(!fs.exists("/copy").unwrap());
                }

                #[test]
                fn copy_source_loop_should_fail() {
                    let fs = TestFs::new();
                    insert_symlink(&fs, "/loop", "loop");

                    assert!(fs.copy("/loop/a.txt", "/b.txt").is_err());
                }

                #[test]
                fn copy_invalid_path_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();

                    assert!(fs.copy("/a.txt", "\0").is_err());
                    assert!(fs.copy("\0", "/b.txt").is_err());
                }
            }

            mod copy_dir_all {
                use super::*;

                #[test]
                fn copy_dir_all_should_copy_tree() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir/sub").unwrap();
                    fs.create_dir_all("/dir/empty").unwrap();
                    fs.write("/dir/a.txt", "aaa").unwrap();
                    fs.write("/dir/sub/b.txt", "bbb").unwrap();
                    fs.create_symlink("/dir/link", "sub/b.txt").unwrap();
                    fs.create_symlink("/dir/sub/up", "../empty").unwrap();

                    fs.copy_dir_all("/dir", "/copy").unwrap();
                    fs.write("/copy/sub/b.txt", "ccc").unwrap();

                    assert_eq!(fs.read_to_string("/copy/a.txt").unwrap(), "aaa");
                    assert_eq!(fs.read_to_string("/copy/link").unwrap(), "ccc");
                    assert_eq!(fs.read_to_string("/dir/sub/b.txt").unwrap(), "bbb");
                    assert_eq!(fs.read_link("/copy/sub/up").unwrap(), Path::new("../empty"));
                    assert!(fs.read_dir("/copy/empty").unwrap().next().is_none());
                }

                #[test]
                fn copy_dir_all_symlink_to_symlink_should_be_copied_after_it() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/dir/z.txt", "zzz").unwrap();
                    fs.create_symlink("/dir/b_link", "z.txt").unwrap();
                    fs.create_symlink("/dir/a_link", "b_link").unwrap();

                    fs.copy_dir_all("/dir", "/copy").unwrap();

                    assert_eq!(fs.read_link("/copy/a_link").unwrap(), Path::new("b_link"));
                    assert_eq!(fs.read_to_string("/copy/a_link").unwrap(), "zzz");
                }

                #[test]
                fn copy_dir_all_should_copy_dir_permissions() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir/sub").unwrap();
                    fs.write("/dir/sub/a.txt", "aaa").unwrap();
                    fs.set_readonly("/dir/sub", true).unwrap();
                    fs.set_readonly("/dir", true).unwrap();

                    fs.copy_dir_all("/dir", "/copy").unwrap();

                    for target in ["/copy", "/copy/sub"] {
                        assert!(fs.metadata(target).unwrap().readonly, "target: {target}");
                    }
                    #[cfg(unix)]
                    assert_eq!(
                        fs.metadata("/copy/sub").unwrap().mode,
                        fs.metadata("/dir/sub").unwrap().mode
                    );
                }

                #[test]
                fn copy_dir_all_should_follow_source_symlink() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/dir/a.txt", "aaa").unwrap();
                    fs.create_symlink("/link", "dir").unwrap();

                    fs.copy_dir_all("/link", "/copy").unwrap();

                    assert!(fs.symlink_metadata("/copy").unwrap().file_type.is_dir());
                    assert_eq!(fs.read_to_string("/copy/a.txt").unwrap(), "aaa");
                }

                #[test]
                fn copy_dir_all_target_inside_source_should_copy_source_as_listed() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/dir/a.txt", "aaa").unwrap();

                    fs.copy_dir_all("/dir", "/dir/copy").unwrap();

                    assert_eq!(fs.read_to_string("/dir/copy/a.txt").unwrap(), "aaa");
                    assert!(!fs.exists("/dir/copy/copy").unwrap());
                }

                #[test]
                fn copy_dir_all_file_source_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();

                    assert_eq!(
                        fs.copy_dir_all("/a.txt", "/copy")
                            .err()
                            .unwrap()
                            .to_string(),
                        fs_error!(
                            "Directory could not be copied, source is not a directory: /a.txt"
                        )
                        .to_string()
                    );
                }

                #[test]
                fn copy_dir_all_nonexistent_source_should_fail() {
                    let fs = TestFs::new();
                    assert!(fs.copy_dir_all("/missing", "/copy").is_err());
                    assert!(!fs.exists("/copy").unwrap());
                }

                #[test]
                fn copy_dir_all_existing_target_should_fail() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/a.txt", "aaa").unwrap();
                    insert_symlink(&fs, "/dangling", "missing.txt");

                    for target in ["/a.txt", "/dangling", "/dir"] {
                        assert_eq!(
                            fs.copy_dir_all("/dir", target).err().unwrap().to_string(),
                            fs_error!(
                                "Directory could not be copied, target already exists: {target}"
                            )
                            .to_string(),
                            "target: {target}"
                        );
                    }
                }

                #[test]
                fn copy_dir_all_without_target_parent_dir_should_fail() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/a.txt", "aaa").unwrap();

                    for target in ["/missing/copy", "/a.txt/copy"] {
                        assert_eq!(
                            fs.copy_dir_all("/dir", target).err().unwrap().to_string(),
                            fs_error!(
                                "Directory could not be copied, parent directory does not exist: {target}"
                            )
                            .to_string(),
                            "target: {target}"
                        );
                    }
                }

                #[test]
                fn copy_dir_all_unresolvable_symlink_should_fail() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.create_dir_all("/other").unwrap();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.create_symlink("/dir/link", "../a.txt").unwrap();

                    assert_eq!(
                        fs.copy_dir_all("/dir", "/other/copy")
                            .err()
                            .unwrap()
                            .to_string(),
                        fs_error!(
                            "Symlink could not be created, target not found: {}",
                            native_symlink_target(Path::new("../a.txt")).display()
                        )
                        .to_string()
                    );
                    assert!(fs.exists("/other/copy").unwrap());
                }

                #[test]
                fn copy_dir_all_write_borrowed_file_should_fail() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/dir/a.txt", "aaa").unwrap();
                    let _write = fs.open_write("/dir/a.txt").unwrap();

                    assert_eq!(
                        fs.copy_dir_all("/dir", "/copy").err().unwrap().to_string(),
                        fs_error!(
                            "File could not be copied, it is already borrowed for writing: {}",
                            normalize_path(&fs.current_dir, "/dir/a.txt").display()
                        )
                        .to_string()
                    );
                }
            }

            mod remove {
                use super::*;

                #[test]
                fn remove_should_remove_file_and_empty_dir() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.set_readonly("/a.txt", true).unwrap();

                    for path in ["/a.txt", "/dir"] {
                        fs.remove(path).unwrap();
                        assert!(fs.symlink_metadata(path).is_err(), "path: {path}");
                    }
                }

                #[test]
                fn remove_should_update_parent_modified() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/dir/a.txt", "aaa").unwrap();
                    fs.set_modified("/dir", SystemTime::UNIX_EPOCH).unwrap();

                    fs.remove("/dir/a.txt").unwrap();

                    assert!(fs.metadata("/dir").unwrap().modified > SystemTime::UNIX_EPOCH);
                }

                #[test]
                fn remove_symlink_should_keep_target() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/dir/a.txt", "aaa").unwrap();
                    fs.create_symlink("/file_link", "dir/a.txt").unwrap();
                    fs.create_symlink("/dir_link", "dir").unwrap();
                    insert_symlink(&fs, "/dangling", "missing.txt");

                    for path in ["/file_link", "/dir_link", "/dangling"] {
                        fs.remove(path).unwrap();
                        assert!(fs.symlink_metadata(path).is_err(), "path: {path}");
                    }
                    assert_eq!(fs.read_to_string("/dir/a.txt").unwrap(), "aaa");
                    assert_eq!(fs.symlinks.load(Ordering::Relaxed), 0);
                }

                #[test]
                fn remove_should_follow_parent_symlinks() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/dir/a.txt", "aaa").unwrap();
                    fs.create_symlink("/link", "dir").unwrap();

                    fs.remove("/link/a.txt").unwrap();

                    assert!(!fs.exists("/dir/a.txt").unwrap());
                }

                #[test]
                fn remove_hard_link_should_keep_other_name() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.create_hard_link("/b.txt", "/a.txt").unwrap();

                    fs.remove("/a.txt").unwrap();

                    assert!(!fs.exists("/a.txt").unwrap());
                    assert_eq!(fs.read_to_string("/b.txt").unwrap(), "aaa");
                }

                #[test]
                fn remove_borrowed_file_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.create_hard_link("/b.txt", "/a.txt").unwrap();
                    let path = normalize_path(&fs.current_dir, "/b.txt");

                    let read = fs.open_read("/a.txt").unwrap();
                    assert_eq!(
                        fs.remove("/b.txt").err().unwrap().to_string(),
                        fs_error!(
                            "File could not be removed, it is already borrowed for reading 1 times: {}",
                            path.display()
                        )
                        .to_string()
                    );
                    drop(read);

                    let _write = fs.open_write("/a.txt").unwrap();
                    assert_eq!(
                        fs.remove("/b.txt").err().unwrap().to_string(),
                        fs_error!(
                            "File could not be removed, it is already borrowed for writing: {}",
                            path.display()
                        )
                        .to_string()
                    );
                    assert!(fs.exists("/b.txt").unwrap());
                }

                #[test]
                fn remove_non_empty_dir_should_fail() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir/sub").unwrap();

                    assert_eq!(
                        fs.remove("/dir").err().unwrap().to_string(),
                        fs_error!(
                            "Directory could not be removed, it is not empty: {}",
                            normalize_path(&fs.current_dir, "/dir").display()
                        )
                        .to_string()
                    );
                }

                #[test]
                fn remove_root_should_fail() {
                    let fs = TestFs::new();

                    assert_eq!(
                        fs.remove(ROOT).err().unwrap().to_string(),
                        fs_error!(
                            "Directory could not be removed, path is a root: {}",
                            normalize_path(&fs.current_dir, ROOT).display()
                        )
                        .to_string()
                    );
                    assert!(fs.exists(ROOT).unwrap());
                }

                #[test]
                fn remove_should_fail_for_nonexistent_path() {
                    let fs = TestFs::new();
                    assert_eq!(
                        fs.remove("/a.txt").err().unwrap().to_string(),
                        fs_error!(
                            "Path could not be removed, path not found: {}",
                            normalize_path(&fs.current_dir, "/a.txt").display()
                        )
                        .to_string()
                    );
                }

                #[test]
                fn remove_invalid_path_should_fail() {
                    let fs = TestFs::new();
                    assert!(fs.remove("\0").is_err());
                }
            }

            mod remove_dir_all {
                use super::*;

                #[test]
                fn remove_dir_all_should_remove_tree_but_keep_symlink_targets() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir/sub/empty").unwrap();
                    fs.create_dir_all("/outside").unwrap();
                    fs.write("/dir/a.txt", "aaa").unwrap();
                    fs.write("/dir/sub/b.txt", "bbb").unwrap();
                    fs.write("/outside/c.txt", "ccc").unwrap();
                    fs.set_readonly("/dir/a.txt", true).unwrap();
                    fs.create_symlink("/dir/file_link", "../outside/c.txt")
                        .unwrap();
                    fs.create_symlink("/dir/sub/dir_link", "../../outside")
                        .unwrap();

                    fs.remove_dir_all("/dir").unwrap();

                    assert!(fs.symlink_metadata("/dir").is_err());
                    assert_eq!(fs.read_to_string("/outside/c.txt").unwrap(), "ccc");
                    assert_eq!(fs.symlinks.load(Ordering::Relaxed), 0);
                    assert_eq!(fs.read_dir(ROOT).unwrap().count(), 1);
                }

                #[test]
                fn remove_dir_all_symlink_should_remove_only_symlink() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/dir/a.txt", "aaa").unwrap();
                    fs.create_symlink("/link", "dir").unwrap();

                    fs.remove_dir_all("/link").unwrap();

                    assert!(fs.symlink_metadata("/link").is_err());
                    assert_eq!(fs.read_to_string("/dir/a.txt").unwrap(), "aaa");
                }

                #[test]
                fn remove_dir_all_should_follow_parent_symlinks() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir/sub").unwrap();
                    fs.write("/dir/sub/a.txt", "aaa").unwrap();
                    fs.create_symlink("/link", "dir").unwrap();

                    fs.remove_dir_all("/link/sub").unwrap();

                    assert!(!fs.exists("/dir/sub").unwrap());
                    assert!(fs.exists("/dir").unwrap());
                }

                #[test]
                fn remove_dir_all_should_update_parent_modified() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir/sub").unwrap();
                    fs.set_modified("/dir", SystemTime::UNIX_EPOCH).unwrap();

                    fs.remove_dir_all("/dir/sub").unwrap();

                    assert!(fs.metadata("/dir").unwrap().modified > SystemTime::UNIX_EPOCH);
                }

                #[test]
                fn remove_dir_all_file_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();

                    assert_eq!(
                        fs.remove_dir_all("/a.txt").err().unwrap().to_string(),
                        fs_error!(
                            "Directory could not be removed, path is not a directory: {}",
                            normalize_path(&fs.current_dir, "/a.txt").display()
                        )
                        .to_string()
                    );
                    assert!(fs.exists("/a.txt").unwrap());
                }

                #[test]
                fn remove_dir_all_root_should_fail_before_removing_anything() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();

                    for path in [ROOT, "/dir/.."] {
                        assert_eq!(
                            fs.remove_dir_all(path).err().unwrap().to_string(),
                            fs_error!(
                                "Directory could not be removed, path is a root: {}",
                                normalize_path(&fs.current_dir, ROOT).display()
                            )
                            .to_string(),
                            "path: {path}"
                        );
                    }
                    assert!(fs.exists("/dir").unwrap());
                }

                #[test]
                fn remove_dir_all_borrowed_file_should_fail() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/dir/a.txt", "aaa").unwrap();
                    let _read = fs.open_read("/dir/a.txt").unwrap();

                    assert_eq!(
                        fs.remove_dir_all("/dir").err().unwrap().to_string(),
                        fs_error!(
                            "File could not be removed, it is already borrowed for reading 1 times: {}",
                            normalize_path(&fs.current_dir, "/dir/a.txt").display()
                        )
                        .to_string()
                    );
                    assert!(fs.exists("/dir/a.txt").unwrap());
                }

                #[test]
                fn remove_dir_all_hard_link_to_borrowed_file_should_fail() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir/sub").unwrap();
                    fs.write("/dir/a.txt", "aaa").unwrap();
                    fs.write("/b.txt", "bbb").unwrap();
                    fs.create_hard_link("/dir/sub/link", "/b.txt").unwrap();
                    let _write = fs.open_write("/b.txt").unwrap();

                    assert_eq!(
                        fs.remove_dir_all("/dir").err().unwrap().to_string(),
                        fs_error!(
                            "File could not be removed, it is already borrowed for writing: {}",
                            normalize_path(&fs.current_dir, "/dir/sub/link").display()
                        )
                        .to_string()
                    );
                    assert!(fs.exists("/dir/a.txt").unwrap());
                }

                #[test]
                fn remove_dir_all_should_fail_for_nonexistent_path() {
                    let fs = TestFs::new();
                    assert_eq!(
                        fs.remove_dir_all("/dir").err().unwrap().to_string(),
                        fs_error!(
                            "Directory could not be removed, path not found: {}",
                            normalize_path(&fs.current_dir, "/dir").display()
                        )
                        .to_string()
                    );
                }

                #[test]
                fn remove_dir_all_invalid_path_should_fail() {
                    let fs = TestFs::new();
                    assert!(fs.remove_dir_all("\0").is_err());
                }
            }

            mod rename {
                use super::*;

                #[test]
                fn rename_borrowed_file_should_keep_borrow_under_new_name() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    let mut read = fs.open_read("/a.txt").unwrap();

                    fs.rename("/a.txt", "/b.txt").unwrap();

                    assert!(!fs.exists("/a.txt").unwrap());
                    assert!(fs.open_write("/b.txt").is_err());
                    let mut content = String::new();
                    read.read_to_string(&mut content).unwrap();
                    assert_eq!(content, "aaa");
                }

                #[test]
                fn rename_dir_should_move_tree() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir/sub").unwrap();
                    fs.create_dir_all("/other").unwrap();
                    fs.write("/dir/sub/a.txt", "aaa").unwrap();
                    fs.create_symlink("/dir/link", "sub/a.txt").unwrap();

                    fs.rename("/dir", "/other/moved").unwrap();

                    assert!(!fs.exists("/dir").unwrap());
                    assert_eq!(fs.read_to_string("/other/moved/link").unwrap(), "aaa");
                    assert_eq!(fs.symlinks.load(Ordering::Relaxed), 1);
                }

                #[test]
                fn rename_symlink_should_move_only_symlink() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.create_symlink("/link", "a.txt").unwrap();

                    fs.rename("/link", "/dir/link").unwrap();

                    assert_eq!(fs.read_link("/dir/link").unwrap(), Path::new("a.txt"));
                    assert!(fs.metadata("/dir/link").is_err());
                    assert_eq!(fs.read_to_string("/a.txt").unwrap(), "aaa");
                }

                #[test]
                fn rename_file_should_replace_file() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.write("/b.txt", "bbb").unwrap();

                    fs.rename("/a.txt", "/b.txt").unwrap();

                    assert!(!fs.exists("/a.txt").unwrap());
                    assert_eq!(fs.read_to_string("/b.txt").unwrap(), "aaa");
                }

                #[test]
                fn rename_hard_link_onto_same_file_should_remove_source() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.create_hard_link("/b.txt", "/a.txt").unwrap();
                    fs.create_hard_link("/dir/c.txt", "/a.txt").unwrap();
                    let mut read = fs.open_read("/a.txt").unwrap();

                    for (source, target) in [("/a.txt", "/b.txt"), ("/b.txt", "/dir/c.txt")] {
                        fs.rename(source, target).unwrap();
                        assert!(!fs.exists(source).unwrap(), "source: {source}");
                    }

                    assert_eq!(fs.read_to_string("/dir/c.txt").unwrap(), "aaa");
                    let mut content = String::new();
                    read.read_to_string(&mut content).unwrap();
                    assert_eq!(content, "aaa");
                }

                #[test]
                fn rename_to_itself_should_do_nothing() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/dir/a.txt", "aaa").unwrap();
                    fs.create_symlink("/link", "dir").unwrap();
                    let _read = fs.open_read("/dir/a.txt").unwrap();

                    for (source, target) in [
                        ("/dir/a.txt", "/dir/a.txt"),
                        ("/dir/a.txt", "/dir/../dir/a.txt"),
                        ("/link/a.txt", "/dir/a.txt"),
                    ] {
                        fs.rename(source, target).unwrap();
                        assert_eq!(
                            fs.read_to_string("/dir/a.txt").unwrap(),
                            "aaa",
                            "source: {source}, target: {target}"
                        );
                    }
                }

                #[test]
                fn rename_should_update_parents_modified() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/a").unwrap();
                    fs.create_dir_all("/b").unwrap();
                    fs.write("/a/x.txt", "xxx").unwrap();
                    fs.set_modified("/a", SystemTime::UNIX_EPOCH).unwrap();
                    fs.set_modified("/b", SystemTime::UNIX_EPOCH).unwrap();

                    fs.rename("/a/x.txt", "/b/x.txt").unwrap();

                    for dir in ["/a", "/b"] {
                        assert!(
                            fs.metadata(dir).unwrap().modified > SystemTime::UNIX_EPOCH,
                            "dir: {dir}"
                        );
                    }
                }

                #[test]
                fn rename_should_follow_parent_symlinks() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/dir/a.txt", "aaa").unwrap();
                    fs.create_symlink("/link", "dir").unwrap();

                    fs.rename("/link/a.txt", "/link/b.txt").unwrap();

                    assert_eq!(fs.read_to_string("/dir/b.txt").unwrap(), "aaa");
                }

                #[test]
                fn rename_should_replace_target_as_std() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir/sub").unwrap();
                    fs.create_dir_all("/empty").unwrap();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.write("/b.txt", "bbb").unwrap();
                    fs.create_symlink("/link", "b.txt").unwrap();
                    insert_symlink(&fs, "/dangling", "missing.txt");

                    fs.rename("/a.txt", "/dangling").unwrap();
                    fs.rename("/link", "/b.txt").unwrap();
                    fs.rename("/dir", "/empty").unwrap();

                    assert_eq!(fs.read_to_string("/dangling").unwrap(), "aaa");
                    assert_eq!(fs.read_link("/b.txt").unwrap(), Path::new("b.txt"));
                    assert!(fs.exists("/empty/sub").unwrap());
                    assert_eq!(fs.symlinks.load(Ordering::Relaxed), 1);
                }

                #[test]
                fn rename_incompatible_target_should_fail() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.create_dir_all("/full/sub").unwrap();
                    fs.write("/a.txt", "aaa").unwrap();
                    insert_symlink(&fs, "/dangling", "missing.txt");

                    for (source, target, error) in [
                        (
                            "/dir",
                            "/a.txt",
                            "Directory could not be renamed, target is not a directory",
                        ),
                        (
                            "/dir",
                            "/dangling",
                            "Directory could not be renamed, target is not a directory",
                        ),
                        (
                            "/a.txt",
                            "/dir",
                            "Path could not be renamed, target is a directory",
                        ),
                        (
                            "/dangling",
                            "/dir",
                            "Path could not be renamed, target is a directory",
                        ),
                        (
                            "/dir",
                            "/full",
                            "Directory could not be renamed, target is not empty",
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
                }

                #[test]
                fn rename_borrowed_target_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.write("/b.txt", "bbb").unwrap();
                    let path = normalize_path(&fs.current_dir, "/b.txt");

                    let read = fs.open_read("/b.txt").unwrap();
                    assert_eq!(
                        fs.rename("/a.txt", "/b.txt").err().unwrap().to_string(),
                        fs_error!(
                            "File could not be replaced, it is already borrowed for reading 1 times: {}",
                            path.display()
                        )
                        .to_string()
                    );
                    drop(read);

                    let _write = fs.open_write("/b.txt").unwrap();
                    assert_eq!(
                        fs.rename("/a.txt", "/b.txt").err().unwrap().to_string(),
                        fs_error!(
                            "File could not be replaced, it is already borrowed for writing: {}",
                            path.display()
                        )
                        .to_string()
                    );
                    assert!(fs.exists("/a.txt").unwrap());
                }

                #[test]
                fn rename_without_target_parent_dir_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();

                    for target in ["/missing/b.txt", "/a.txt/b.txt"] {
                        assert_eq!(
                            fs.rename("/a.txt", target).err().unwrap().to_string(),
                            fs_error!(
                                "Path could not be renamed, parent directory does not exist: {}",
                                normalize_path(&fs.current_dir, target).display()
                            )
                            .to_string(),
                            "target: {target}"
                        );
                    }
                }

                #[test]
                fn rename_dir_into_itself_should_fail() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir/sub").unwrap();

                    assert_eq!(
                        fs.rename("/dir", "/dir/sub/moved")
                            .err()
                            .unwrap()
                            .to_string(),
                        fs_error!(
                            "Directory could not be renamed, target is inside source: {}",
                            normalize_path(&fs.current_dir, "/dir/sub/moved").display()
                        )
                        .to_string()
                    );
                    assert!(fs.exists("/dir/sub").unwrap());
                }

                #[test]
                fn rename_nonexistent_source_should_fail() {
                    let fs = TestFs::new();
                    assert_eq!(
                        fs.rename("/a.txt", "/b.txt").err().unwrap().to_string(),
                        fs_error!(
                            "Path could not be renamed, source not found: {}",
                            normalize_path(&fs.current_dir, "/a.txt").display()
                        )
                        .to_string()
                    );
                }

                #[test]
                fn rename_root_should_fail() {
                    let fs = TestFs::new();
                    assert_eq!(
                        fs.rename(ROOT, "/moved").err().unwrap().to_string(),
                        fs_error!(
                            "Path could not be renamed, source is a root: {}",
                            normalize_path(&fs.current_dir, ROOT).display()
                        )
                        .to_string()
                    );
                }

                #[test]
                fn rename_invalid_path_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    insert_symlink(&fs, "/loop", "loop");

                    assert!(fs.rename("\0", "/b.txt").is_err());
                    assert!(fs.rename("/a.txt", "\0").is_err());
                    assert!(fs.rename("/loop/a.txt", "/b.txt").is_err());
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

                #[test]
                fn set_modified_should_follow_symlinks() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "").unwrap();
                    fs.create_symlink("/link", "a.txt").unwrap();

                    fs.set_modified("/link", SystemTime::UNIX_EPOCH).unwrap();

                    assert_eq!(
                        fs.metadata("/a.txt").unwrap().modified,
                        SystemTime::UNIX_EPOCH
                    );
                    assert!(
                        fs.symlink_metadata("/link").unwrap().modified > SystemTime::UNIX_EPOCH
                    );
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

    mod resolve_symlinks {
        use super::*;

        #[test]
        fn resolve_symlinks_should_follow_symlinks_in_all_components() {
            let fs = TestFs::new();
            fs.create_dir_all("/a/b").unwrap();
            fs.write("/a/b/c.txt", "").unwrap();
            fs.create_symlink("/dir_link", "a/b").unwrap();
            fs.create_symlink("/a/b/file_link", "/a/b/c.txt").unwrap();
            let path = normalize_path(&fs.current_dir, "/dir_link/file_link");

            assert_eq!(
                resolve_symlinks(&lock_handles(&fs.fs), &path, true).unwrap(),
                normalize_path(&fs.current_dir, "/a/b/c.txt")
            );
        }

        #[test]
        fn resolve_symlinks_without_follow_last_should_keep_last_symlink() {
            let fs = TestFs::new();
            fs.create_dir_all("/a/b").unwrap();
            fs.create_symlink("/dir_link", "a").unwrap();
            fs.create_symlink("/a/link", "b").unwrap();
            let path = normalize_path(&fs.current_dir, "/dir_link/link");

            assert_eq!(
                resolve_symlinks(&lock_handles(&fs.fs), &path, false).unwrap(),
                normalize_path(&fs.current_dir, "/a/link")
            );
        }

        #[test]
        fn resolve_symlinks_parent_dir_in_target_should_leave_resolved_dir() {
            // GIVEN: `/up` points to `..` of what `/dir_link` resolves to,
            // `/a`, not to the lexical parent of `/dir_link`, `/`
            let fs = TestFs::new();
            fs.create_dir_all("/a/b").unwrap();
            fs.create_symlink("/dir_link", "a/b").unwrap();
            fs.create_symlink("/up", "./dir_link/..").unwrap();
            let path = normalize_path(&fs.current_dir, "/up");

            // WHEN
            let resolved = resolve_symlinks(&lock_handles(&fs.fs), &path, true).unwrap();

            // THEN
            assert_eq!(resolved, normalize_path(&fs.current_dir, "/a"));
        }

        #[test]
        fn resolve_symlinks_loop_should_fail_after_max_symlinks() {
            let fs = TestFs::new();
            fs.write("/a.txt", "").unwrap();
            let mut target = "a.txt".to_string();

            for i in 0..=MAX_SYMLINKS {
                let link = format!("link{i}");
                fs.create_symlink(format!("/{link}").as_str(), target.as_str())
                    .unwrap();
                target = link;
            }

            let fs_map = lock_handles(&fs.fs);
            let within = normalize_path(&fs.current_dir, format!("/link{}", MAX_SYMLINKS - 1));
            let beyond = normalize_path(&fs.current_dir, format!("/link{MAX_SYMLINKS}"));
            assert!(resolve_symlinks(&fs_map, &within, true).is_ok());
            assert_eq!(
                resolve_symlinks(&fs_map, &beyond, true)
                    .err()
                    .unwrap()
                    .to_string(),
                fs_error!("Too many levels of symbolic links: {}", beyond.display()).to_string()
            );
        }
    }

    mod test_file_handle {
        use super::*;

        fn new_handle(state: BorrowState) -> TestFileHandle {
            let mut file = TestFsFile::new(SystemTime::now(), new_dir_attributes());
            file.state = state;

            TestFileHandle {
                pos: 0,
                len: 0,
                file: Arc::new(RwLock::new(file)),
                fail_mode: None,
            }
        }

        mod seek {
            use super::*;

            #[test]
            fn seek_should_fail_for_negative_offset_from_end() {
                let mut handle = new_handle(BorrowState::Read(1));
                assert!(handle.seek(SeekFrom::End(-10)).is_err());
            }

            #[test]
            fn seek_should_fail_for_negative_offset_from_current() {
                let mut handle = new_handle(BorrowState::Read(1));
                assert!(handle.seek(SeekFrom::Current(-10)).is_err());
            }
        }

        mod drop {
            use super::*;

            #[test]
            fn drop_should_release_borrow() {
                for (state, expected) in [(BorrowState::Read(2), 1), (BorrowState::Write, 0)] {
                    let handle = new_handle(state);
                    let file = handle.file.clone();

                    drop(handle);

                    assert!(
                        matches!(lock_file_read(&file).state, BorrowState::Read(count) if count == expected),
                        "state: {state:?}"
                    );
                }
            }
        }
    }
}
