use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::ffi::OsString;
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
use std::sync::PoisonError;
use std::sync::RwLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::SystemTime;

use crate::Fs;
use crate::ReadFile;
use crate::Result;
use crate::WriteFile;
use crate::env::SharedEnv;
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
pub use crate::test_platform::ROOT;

/// An in-memory file holding its content, borrow state and metadata.
///
/// Shared by all of its hard links and open handles, see
/// [`TestFsEntry::File`].
#[derive(Debug)]
pub struct TestFsFile {
    data: Vec<u8>,
    /// At least `data.len()`, the bytes beyond `data` are zeros not held in
    /// memory like in a sparse file.
    len: u64,
    modified: SystemTime,
    state: BorrowState,
    created: SystemTime,
    attributes: NativeAttributes,
}

impl TestFsFile {
    fn new(now: SystemTime, attributes: NativeAttributes) -> Self {
        Self {
            data: Vec::new(),
            len: 0,
            modified: now,
            state: BorrowState::Read(0),
            created: now,
            attributes,
        }
    }

    fn set_len(&mut self, len: u64) {
        if len < self.data.len() as u64 {
            self.data.truncate(len as usize);
        }

        self.len = len;
        self.modified = SystemTime::now();
    }
}

/// An in-memory directory entry holding its metadata and the names of its
/// children.
pub struct TestFsDir {
    created: SystemTime,
    modified: SystemTime,
    attributes: NativeAttributes,
    /// The names of the children, each with its [`name_hash`], in the order
    /// [`Fs::read_dir`] lists them.
    children: BTreeSet<(u64, OsString)>,
}

impl TestFsDir {
    fn new(created: SystemTime) -> Self {
        Self {
            created,
            modified: created,
            attributes: new_dir_attributes(),
            children: BTreeSet::new(),
        }
    }
}

/// Returns the 64-bit FNV-1a hash of the file `name`, which orders the
/// entries of a directory deterministically but not alphabetically, like
/// the hashed directories of ext4. Unlike [`std::hash::DefaultHasher`] it is
/// the same across Rust versions.
fn name_hash(name: &OsStr) -> u64 {
    name.as_encoded_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        })
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
    env: SharedEnv,
    fs: Arc<Mutex<BTreeMap<PathBuf, TestFsEntry>>>,
    fail_mode: Mutex<Option<FailMode>>,
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
    /// [`Fs::read_dir`] lists a directory lazily, in an order that is
    /// unspecified as with [`std::fs::read_dir`] but deterministic: by a
    /// fixed hash of the names rather than alphabetically. Files can be
    /// written up to [`MAX_FILE_LEN`] bytes, while lengths set by
    /// [`WriteFile::set_len`] or [`set_len`](TestFs::set_len) extend them
    /// by zeros not held in memory, like sparse files.
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
            env: SharedEnv::new(Vec::new(), PathBuf::from(ROOT)),
            fs: Arc::new(Mutex::new(BTreeMap::from([(PathBuf::from(ROOT), root)]))),
            fail_mode: Mutex::new(None),
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
                    missing_kind(&fs, &resolved_path) =>
                    "Creation time could not be set, path not found: {}",
                    normalized_path.display()
                ));
            }
        }

        Ok(())
    }

    /// Sets the length of a file like [`WriteFile::set_len`] without
    /// opening it, i.e. regardless of its borrows and permissions. Its
    /// content is cut at `len` or extended by zeros that are not held in
    /// memory, so a file can be made arbitrarily large. Symlinks are
    /// followed.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The path does not exist or is a directory.
    /// - `len` exceeds `i64::MAX`, like [`WriteFile::set_len`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::Fs;
    /// use agplatform::test_platform::test_fs::TestFs;
    ///
    /// let fs = TestFs::new();
    /// fs.write("/image.iso", "").unwrap();
    /// fs.set_len("/image.iso", 10 << 30).unwrap();
    /// assert_eq!(fs.metadata("/image.iso").unwrap().len, 10 << 30);
    /// ```
    pub fn set_len<P: AsRef<Path>>(&self, path: P, len: u64) -> Result {
        let (mut fs, normalized_path, resolved_path) = self.lock_resolve(path.as_ref(), true)?;

        match fs.get_mut(&resolved_path) {
            Some(TestFsEntry::File(_)) if i64::try_from(len).is_err() => {
                Err(invalid_len_error(&normalized_path))
            }
            Some(TestFsEntry::File(file)) => {
                lock_file_write(file).set_len(len);
                Ok(())
            }
            Some(TestFsEntry::Dir(_)) => Err(fs_error!(
                IsADirectory;
                "File length could not be set, it is a directory: {}",
                normalized_path.display()
            )),
            Some(TestFsEntry::Symlink(_)) | None => Err(fs_error!(
                missing_kind(&fs, &resolved_path) =>
                "File length could not be set, path not found: {}",
                normalized_path.display()
            )),
        }
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
                    missing_kind(&fs, &resolved_path) =>
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
    pub fn set_fail_mode(&self, fail_mode: Option<FailMode>) {
        *self
            .fail_mode
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = fail_mode;
    }

    /// [internal]
    ///
    /// Resolves relative paths against the current directory of `env`
    /// instead, e.g. of a [`TestPlatform`](crate::test_platform::TestPlatform).
    pub(crate) fn set_env(&mut self, env: SharedEnv) {
        self.env = env;
    }

    /// [internal]
    ///
    /// Returns the current directory relative paths are resolved against.
    #[cfg(test)]
    pub(crate) fn current_dir(&self) -> PathBuf {
        self.env.current_dir().to_path_buf()
    }

    fn fail_mode(&self) -> Option<FailMode> {
        *self
            .fail_mode
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Locks the filesystem and returns it with the normalized `path`, for
    /// messages, and the `path` with its symlinks resolved, the last
    /// component only if `follow_last` is set, see
    /// [`resolve_dir`](Self::resolve_dir).
    fn lock_resolve(
        &self,
        path: &Path,
        follow_last: bool,
    ) -> Result<(TestFsGuard<'_>, PathBuf, PathBuf)> {
        let (fs, normalized_path) = self.lock_normalize(path)?;
        let resolved_path = self.resolve_dir(&fs, &normalized_path, follow_last)?;

        Ok((fs, normalized_path, resolved_path))
    }

    /// Locks the filesystem and returns it with the normalized `path`.
    fn lock_normalize(&self, path: &Path) -> Result<(TestFsGuard<'_>, PathBuf)> {
        self.lock_normalize_at(&self.env.current_dir(), path)
    }

    /// Locks the filesystem and returns it with the `path` normalized
    /// against `current_dir`, see [`normalize_at`].
    fn lock_normalize_at(
        &self,
        current_dir: &Path,
        path: &Path,
    ) -> Result<(TestFsGuard<'_>, PathBuf)> {
        let normalized_path = normalize_at(current_dir, path)?;
        Ok((lock_handles(&self.fs), normalized_path))
    }

    /// Resolves the symlinks of a `path` already normalized by
    /// [`normalize_path`], see [`resolve_symlinks`]. While the filesystem
    /// has no symlinks the `path` is returned without walking its
    /// components. A trailing separator is dropped, see
    /// [`resolve_dir`](Self::resolve_dir) to honour it.
    fn resolve(
        &self,
        fs: &BTreeMap<PathBuf, TestFsEntry>,
        path: &Path,
        follow_last: bool,
    ) -> Result<PathBuf> {
        // WHY: relaxed suffices, the count only changes while `fs` is
        // locked, which the caller holds.
        if self.symlinks.load(Ordering::Relaxed) == 0 {
            return Ok(if ends_with_separator(path) {
                path.components().collect()
            } else {
                path.to_path_buf()
            });
        }

        resolve_symlinks(fs, path, follow_last)
    }

    /// Resolves the symlinks of a normalized `path` like
    /// [`resolve`](Self::resolve) unless it [`requires_dir`]: then its last
    /// component is followed and fails if it is not a directory.
    fn resolve_dir(
        &self,
        fs: &BTreeMap<PathBuf, TestFsEntry>,
        path: &Path,
        follow_last: bool,
    ) -> Result<PathBuf> {
        let dir_required = requires_dir(path);
        let resolved = self.resolve(fs, path, follow_last || dir_required)?;

        if dir_required && matches!(fs.get(&resolved), Some(TestFsEntry::File(_))) {
            return Err(fs_error!(
                NotADirectory;
                "Not a directory: {}",
                path.display()
            ));
        }

        Ok(resolved)
    }
}

/// Returns `true` if the normalized `path` ends with a separator other
/// than a root, so that it must be a directory, see [`normalize_path`].
fn requires_dir(path: &Path) -> bool {
    ends_with_separator(path) && path.parent().is_some()
}

/// Returns the kind of error for the missing resolved `path`: not a
/// directory if its closest existing ancestor is not one, as reported by
/// the OS, not found otherwise.
fn missing_kind(fs: &BTreeMap<PathBuf, TestFsEntry>, path: &Path) -> std::io::ErrorKind {
    match path
        .ancestors()
        .skip(1)
        .find_map(|ancestor| fs.get(ancestor))
    {
        Some(TestFsEntry::Dir(_)) | None => std::io::ErrorKind::NotFound,
        Some(_) => std::io::ErrorKind::NotADirectory,
    }
}

/// Returns the error for resolving a path through more than
/// [`MAX_SYMLINKS`] symlinks, the OS error `ELOOP` which std reports as a
/// filesystem loop.
fn symlink_loop_error() -> std::io::Error {
    #[cfg(target_os = "linux")]
    const ELOOP: i32 = 40;
    #[cfg(all(unix, not(target_os = "linux")))]
    const ELOOP: i32 = 62;
    // WHY: `ERROR_CANT_RESOLVE_FILENAME`, which std maps to the same kind.
    #[cfg(windows)]
    const ELOOP: i32 = 1921;

    std::io::Error::from_raw_os_error(ELOOP)
}

type TestFsGuard<'a> = std::sync::MutexGuard<'a, BTreeMap<PathBuf, TestFsEntry>>;

/// The number of symlinks followed when resolving a path before it is
/// considered a loop, `MAXSYMLINKS` on Linux.
const MAX_SYMLINKS: usize = 40;

/// Returns the `path` normalized against `current_dir`, see
/// [`normalize_path`].
///
/// WHY: the two paths of an operation are normalized against the same
/// current directory, even when it is changed in between.
fn normalize_at(current_dir: &Path, path: &Path) -> Result<PathBuf> {
    if path.to_string_lossy().contains('\0') {
        return Err(fs_error!(
            InvalidInput;
            "Path contains a NUL byte: {}",
            path.display()
        ));
    }

    Ok(normalize_path(current_dir, path).into_owned())
}

/// Resolves the symlinks of a normalized `path`: in the parent components
/// always, in the last component only if `follow_last` is set. A relative
/// target continues from the directory containing the symlink and a `..`
/// in a target leaves the directory the path has resolved into so far,
/// which must exist and be a directory, as the OS requires.
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
                if !is_dir(fs, &resolved) {
                    let kind = match fs.get(&resolved) {
                        Some(_) => std::io::ErrorKind::NotADirectory,
                        None => missing_kind(fs, &resolved),
                    };

                    return Err(fs_error!(
                        kind =>
                        "Path could not be resolved, not a directory: {}",
                        resolved.display()
                    ));
                }

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
                return Err(
                    fs_error!("Too many levels of symbolic links: {}", path.display())
                        .with_source(symlink_loop_error()),
                );
            }

            resolved.pop();

            for target_component in symlink.target.components().rev() {
                components.push_front(target_component);
            }
        }
    }

    Ok(resolved)
}

struct TestFileHandle {
    pos: u64,
    file: Arc<RwLock<TestFsFile>>,
    fail_mode: Option<FailMode>,
    path: PathBuf,
}

impl TestFileHandle {
    fn len(&self) -> u64 {
        lock_file_read(&self.file).len
    }

    fn metadata(&self) -> Metadata {
        let file = lock_file_read(&self.file);

        Metadata::new(
            &self.path,
            FileType::File,
            file.len,
            file.modified,
            file.created,
            file.attributes,
        )
    }

    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        if let Some(FailMode::Seek(seek_direction)) = self.fail_mode
            && std::mem::discriminant(&pos) == std::mem::discriminant(&seek_direction)
        {
            return Err(std::io::Error::other("simulated seek failure"));
        }

        // WHY: like `lseek`, a position before the start or beyond
        // `i64::MAX` is invalid.
        let new_pos = match pos {
            SeekFrom::Start(offset) => i64::try_from(offset).ok(),
            SeekFrom::End(offset) => i64::try_from(self.len())
                .ok()
                .and_then(|len| len.checked_add(offset)),
            SeekFrom::Current(offset) => i64::try_from(self.pos)
                .ok()
                .and_then(|pos| pos.checked_add(offset)),
        };

        self.pos = new_pos
            .and_then(|pos| u64::try_from(pos).ok())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "invalid seek to a negative or overflowing position",
                )
            })?;

        Ok(self.pos)
    }
}

/// The largest position up to which a [`TestFs`] file can be written,
/// beyond which writing fails as if the file were too large for the
/// filesystem rather than allocating it in memory. Lengths set by
/// [`WriteFile::set_len`] or [`TestFs::set_len`] are not limited, the bytes
/// beyond the written ones are zeros not held in memory.
pub const MAX_FILE_LEN: u64 = 1 << 30;

/// Returns the error for writing a file beyond [`MAX_FILE_LEN`].
fn file_too_large_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::FileTooLarge,
        format!("file would exceed the maximum length of {MAX_FILE_LEN} bytes"),
    )
}

/// Returns the error for a length `ftruncate` rejects, which takes a
/// signed length.
fn invalid_len_error(path: &Path) -> crate::Error {
    fs_error!(
        InvalidInput;
        "File length could not be set, length is invalid: {}",
        path.display()
    )
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

impl TestFileRead {
    /// Reads up to `max` bytes from the position, passing `read` the bytes
    /// held of them and the number of the zero bytes past those of a sparse
    /// file, and advances the position by their total.
    fn read_with(
        &mut self,
        max: u64,
        read: impl FnOnce(&[u8], usize) -> std::io::Result<()>,
    ) -> std::io::Result<usize> {
        if matches!(self.0.fail_mode, Some(FailMode::Read)) {
            return Err(std::io::Error::other("simulated read failure"));
        }

        let file = lock_file_read(&self.0.file);
        let read_size = file
            .len
            .saturating_sub(self.0.pos)
            .min(max)
            .min(usize::MAX as u64) as usize;
        let start = std::cmp::min(self.0.pos, file.data.len() as u64) as usize;
        let held = std::cmp::min(file.data.len() - start, read_size);
        read(&file.data[start..start + held], read_size - held)?;
        drop(file);
        self.0.pos += read_size as u64;
        Ok(read_size)
    }
}

impl Read for TestFileRead {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.read_with(buf.len() as u64, |held, zeroes| {
            buf[..held.len()].copy_from_slice(held);
            buf[held.len()..held.len() + zeroes].fill(0);
            Ok(())
        })
    }

    // WHY: read at once rather than in growing chunks, each zeroed first.
    fn read_to_end(&mut self, buf: &mut Vec<u8>) -> std::io::Result<usize> {
        self.read_with(u64::MAX, |held, zeroes| {
            buf.try_reserve(held.len() + zeroes)
                .map_err(|_| std::io::Error::from(std::io::ErrorKind::OutOfMemory))?;
            buf.extend_from_slice(held);
            buf.resize(buf.len() + zeroes, 0);
            Ok(())
        })
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
    write_only: bool,
}

impl Read for TestFileWrite {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.check_readable()?;
        self.handle.read(buf)
    }

    fn read_to_end(&mut self, buf: &mut Vec<u8>) -> std::io::Result<usize> {
        self.check_readable()?;
        self.handle.read_to_end(buf)
    }
}

impl TestFileWrite {
    fn check_readable(&self) -> std::io::Result<()> {
        if self.write_only {
            return Err(std::io::Error::other("file is opened for writing only"));
        }

        Ok(())
    }
}

// WHY: within one `TestFs` the borrow rules exclude handles with
// conflicting locks, a write handle is the only one of its file, so every
// lock is acquired right away.
impl ReadFile for TestFileRead {
    fn metadata(&self) -> Result<Metadata> {
        Ok(self.0.metadata())
    }

    fn lock_shared(&self) -> Result {
        Ok(())
    }

    fn try_lock_shared(&self) -> Result<bool> {
        Ok(true)
    }

    fn unlock(&self) -> Result {
        Ok(())
    }
}

impl ReadFile for TestFileWrite {
    fn metadata(&self) -> Result<Metadata> {
        Ok(self.handle.0.metadata())
    }

    fn lock_shared(&self) -> Result {
        Ok(())
    }

    fn try_lock_shared(&self) -> Result<bool> {
        Ok(true)
    }

    fn unlock(&self) -> Result {
        Ok(())
    }
}

impl WriteFile for TestFileWrite {
    fn set_len(&self, len: u64) -> Result {
        if i64::try_from(len).is_err() {
            return Err(invalid_len_error(&self.handle.0.path));
        }

        lock_file_write(&self.handle.0.file).set_len(len);
        Ok(())
    }

    fn sync_all(&self) -> Result {
        Ok(())
    }

    fn sync_data(&self) -> Result {
        Ok(())
    }

    fn lock(&self) -> Result {
        Ok(())
    }

    fn try_lock(&self) -> Result<bool> {
        Ok(true)
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
            self.handle.0.pos = self.handle.0.len();
        }

        let end_pos = self
            .handle
            .0
            .pos
            .checked_add(buf.len() as u64)
            .filter(|&end_pos| end_pos <= MAX_FILE_LEN)
            .ok_or_else(file_too_large_error)?;

        {
            let mut file = lock_file_write(&self.handle.0.file);

            if end_pos > file.data.len() as u64 {
                file.data.resize(end_pos as usize, 0);
            }

            file.data[self.handle.0.pos as usize..end_pos as usize].copy_from_slice(buf);
            file.len = file.len.max(end_pos);
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

/// A lazy iterator over the entries of a directory of [`TestFs`] in the
/// order of the FNV-1a hashes of their names.
///
/// Like [`std::fs::ReadDir`] it does not hold the filesystem: each entry is
/// looked up when it is reached, continuing after the last one returned.
/// An entry added or removed during the iteration may or may not be
/// returned, but no entry is returned twice. The iteration ends if the
/// directory is removed.
pub struct TestReadDir {
    fs: Arc<Mutex<BTreeMap<PathBuf, TestFsEntry>>>,
    dir: PathBuf,
    path: PathBuf,
    cursor: Option<(u64, OsString)>,
}

impl Iterator for TestReadDir {
    type Item = Result<DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        let fs = lock_handles(&self.fs);
        let Some(TestFsEntry::Dir(dir)) = fs.get(&self.dir) else {
            return None;
        };
        let after = self
            .cursor
            .as_ref()
            .map_or(Bound::Unbounded, Bound::Excluded);

        // WHY: names that are not valid Unicode are skipped, see
        // `DirEntry::new`.
        let (child, entry) = dir
            .children
            .range::<(u64, OsString), _>((after, Bound::Unbounded))
            .find_map(|child| {
                fs.get(&self.dir.join(&child.1))
                    .and_then(|entry| {
                        DirEntry::new(self.path.join(&child.1), entry_file_type(entry))
                    })
                    .map(|entry| (child, entry))
            })?;
        self.cursor = Some(child.clone());

        Some(Ok(entry))
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
                file.len,
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
                missing_kind(fs, path) =>
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

/// Copies the `source` file to `target_path`, resolved, overwriting a file
/// there, see [`Fs::copy`]. The normalized paths are used in errors.
fn copy_file(
    fs: &mut BTreeMap<PathBuf, TestFsEntry>,
    source: &Arc<RwLock<TestFsFile>>,
    normalized_source: &Path,
    target_path: PathBuf,
    normalized_target: &Path,
) -> Result {
    let source_file = lock_file_read(source);
    let now = SystemTime::now();

    let reads = match source_file.state {
        BorrowState::Read(count) => count,
        BorrowState::Write => {
            return Err(fs_error!(
                ResourceBusy;
                "File could not be opened, it is already borrowed for writing: {}",
                normalized_source.display()
            ));
        }
    };

    match fs.get(&target_path) {
        Some(TestFsEntry::Dir(_)) => Err(fs_error!(
            IsADirectory;
            "File could not be opened, path is a directory: {}",
            normalized_target.display()
        )),
        // WHY: the source is open for reading while it is copied, so as the
        // target it is borrowed once more.
        Some(TestFsEntry::File(target)) if Arc::ptr_eq(target, source) => Err(fs_error!(
            ResourceBusy;
            "File could not be opened, it is already borrowed for reading {} times: {}",
            reads + 1,
            normalized_target.display()
        )),
        Some(TestFsEntry::File(target)) => {
            let mut target_file = lock_file_write(target);

            match target_file.state {
                BorrowState::Read(0) => {}
                BorrowState::Read(count) => {
                    return Err(fs_error!(
                        ResourceBusy;
                        "File could not be opened, it is already borrowed for reading {count} times: {}",
                        normalized_target.display()
                    ));
                }
                BorrowState::Write => {
                    return Err(fs_error!(
                        ResourceBusy;
                        "File could not be opened, it is already borrowed for writing: {}",
                        normalized_target.display()
                    ));
                }
            }

            if target_file.attributes.attributes().readonly {
                return Err(fs_error!(
                    PermissionDenied;
                    "File could not be opened, it is read-only: {}",
                    normalized_target.display()
                ));
            }

            target_file.data = source_file.data.clone();
            target_file.len = source_file.len;
            target_file.modified = now;
            target_file.attributes =
                overwritten_attributes(target_file.attributes, source_file.attributes);
            Ok(())
        }
        Some(TestFsEntry::Symlink(_)) | None => {
            check_copy_parent(fs, &target_path, normalized_target)?;
            let mut copy = TestFsFile::new(now, copied_attributes(source_file.attributes));
            copy.data = source_file.data.clone();
            copy.len = source_file.len;
            drop(source_file);
            touch_parent(fs, &target_path, now);
            insert_entry(
                fs,
                target_path,
                TestFsEntry::File(Arc::new(RwLock::new(copy))),
            );
            Ok(())
        }
    }
}

/// Fails if the parent directory of the resolved `path` does not exist, the
/// `normalized_path` used in the error after `what` could not be done.
fn check_parent(
    fs: &BTreeMap<PathBuf, TestFsEntry>,
    path: &Path,
    normalized_path: &Path,
    what: &str,
) -> Result {
    if path.parent().is_some_and(|parent| is_dir(fs, parent)) {
        return Ok(());
    }

    Err(fs_error!(
        missing_kind(fs, path) =>
        "{what}, parent directory does not exist: {}",
        normalized_path.display()
    ))
}

/// Fails if the parent directory of the `target_path` of [`Fs::copy`]
/// does not exist, its `normalized_target` used in the error.
fn check_copy_parent(
    fs: &BTreeMap<PathBuf, TestFsEntry>,
    target_path: &Path,
    normalized_target: &Path,
) -> Result {
    if target_path
        .parent()
        .is_some_and(|parent| is_dir(fs, parent))
    {
        Ok(())
    } else {
        Err(fs_error!(
            NotFound;
            "Path could not be copied, parent directory does not exist: {}",
            normalized_target.display()
        ))
    }
}

/// Returns the `attributes` of an existing file overwritten by a copy of a
/// file with the `source` attributes: its permissions are copied, its
/// owner is kept.
fn overwritten_attributes(
    attributes: NativeAttributes,
    source: NativeAttributes,
) -> NativeAttributes {
    #[cfg(unix)]
    let overwritten = NativeAttributes {
        mode: source.mode,
        ..attributes
    };
    #[cfg(windows)]
    let overwritten = NativeAttributes {
        readonly: source.readonly,
        ..attributes
    };

    overwritten
}

fn has_children(fs: &BTreeMap<PathBuf, TestFsEntry>, path: &Path) -> bool {
    matches!(fs.get(path), Some(TestFsEntry::Dir(dir)) if !dir.children.is_empty())
}

/// Inserts the `entry` at `path` and adds it to the children of its parent
/// directory.
fn insert_entry(fs: &mut BTreeMap<PathBuf, TestFsEntry>, path: PathBuf, entry: TestFsEntry) {
    parent_children(fs, &path)
        .map(|(children, name)| children.insert((name_hash(name), name.to_os_string())));
    fs.insert(path, entry);
}

/// Removes the entry at `path` and from the children of its parent
/// directory.
fn remove_entry(fs: &mut BTreeMap<PathBuf, TestFsEntry>, path: &Path) -> Option<TestFsEntry> {
    parent_children(fs, path)
        .map(|(children, name)| children.remove(&(name_hash(name), name.to_os_string())));
    fs.remove(path)
}

/// Returns the children of the parent directory of `path` and its name,
/// if the parent is a directory.
fn parent_children<'a>(
    fs: &'a mut BTreeMap<PathBuf, TestFsEntry>,
    path: &'a Path,
) -> Option<(&'a mut BTreeSet<(u64, OsString)>, &'a OsStr)> {
    match path.parent().and_then(|parent| fs.get_mut(parent)) {
        Some(TestFsEntry::Dir(dir)) => path.file_name().map(|name| (&mut dir.children, name)),
        _ => None,
    }
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
    type Dir = TestReadDir;

    fn open_read<P: AsRef<Path>>(&self, path: P) -> Result<Self::ReadHandle> {
        let (fs, normalized_path, resolved_path) = self.lock_resolve(path.as_ref(), true)?;

        if is_dir(&fs, &resolved_path) {
            return Err(fs_error!(
                IsADirectory;
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
                        ResourceBusy;
                        "File could not be opened, it is already borrowed for writing: {}",
                        normalized_path.display()
                    ));
                }
            }

            return Ok(TestFileRead(TestFileHandle {
                pos: 0,
                file: file.clone(),
                fail_mode: self.fail_mode(),
                path: normalized_path,
            }));
        }

        Err(fs_error!(
            missing_kind(&fs, &resolved_path) =>
            "File could not be opened, file not found: {}",
            normalized_path.display()
        ))
    }

    fn open_write_options<P: AsRef<Path>>(
        &self,
        path: P,
        options: OpenWriteOptions,
    ) -> Result<Self::WriteHandle> {
        let (mut fs, normalized_path) = self.lock_normalize(path.as_ref())?;

        // WHY: like `O_CREAT`, a path that must be a directory cannot be
        // created as a file, whether it exists or not.
        if (options.create || options.create_new) && requires_dir(&normalized_path) {
            let link_path = self.resolve(&fs, &normalized_path, false)?;
            check_parent(
                &fs,
                &link_path,
                &normalized_path,
                "File could not be created",
            )?;

            return Err(fs_error!(
                IsADirectory;
                "File could not be opened, path is a directory: {}",
                normalized_path.display()
            ));
        }

        let link_path = self.resolve_dir(&fs, &normalized_path, false)?;

        // WHY: like `O_EXCL`, `create_new` does not follow a symlink, even
        // a dangling one.
        if options.create_new && fs.contains_key(&link_path) {
            return Err(fs_error!(
                AlreadyExists;
                "File could not be opened with 'create_new' option, file already exists: {}",
                normalized_path.display()
            ));
        }

        let resolved_path = self.resolve(&fs, &link_path, true)?;

        if is_dir(&fs, &resolved_path) {
            return Err(fs_error!(
                IsADirectory;
                "File could not be opened, path is a directory: {}",
                normalized_path.display()
            ));
        }

        if let Some(TestFsEntry::File(file)) = fs.get(&resolved_path) {
            if options.append && options.truncate {
                return Err(fs_error!(
                    InvalidInput;
                    "Existing file could not be opened with both 'append' and 'truncate' options: {}",
                    normalized_path.display()
                ));
            }

            let mut inode = lock_file_write(file);

            if inode.attributes.attributes().readonly {
                return Err(fs_error!(
                    PermissionDenied;
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
                            ResourceBusy;
                            "File could not be opened, it is already borrowed for reading {count} times: {}",
                            normalized_path.display()
                        ));
                    }
                }
                BorrowState::Write => {
                    return Err(fs_error!(
                        ResourceBusy;
                        "File could not be opened, it is already borrowed for writing: {}",
                        normalized_path.display()
                    ));
                }
            }

            if options.truncate {
                inode.set_len(0);
            }

            return Ok(TestFileWrite {
                handle: TestFileRead(TestFileHandle {
                    pos: 0,
                    file: file.clone(),
                    fail_mode: self.fail_mode(),
                    path: normalized_path,
                }),
                append: options.append,
                write_only: options.write_only,
            });
        }

        if !options.create && !options.create_new {
            return Err(fs_error!(
                missing_kind(&fs, &resolved_path) =>
                "File could not be opened with 'create' or 'create_new' option, file does not exist: {}",
                normalized_path.display()
            ));
        }

        if (options.append && options.truncate) && !options.create_new {
            return Err(fs_error!(
                InvalidInput;
                "File could not be opened with both 'append' and 'truncate' options but without 'create_new' option: {}",
                normalized_path.display()
            ));
        }

        check_parent(
            &fs,
            &resolved_path,
            &normalized_path,
            "File could not be created",
        )?;

        let now = SystemTime::now();
        let mut new_file = TestFsFile::new(now, new_file_attributes(&options));
        new_file.state = BorrowState::Write;
        let new_file = Arc::new(RwLock::new(new_file));
        let file = TestFileWrite {
            handle: TestFileRead(TestFileHandle {
                pos: 0,
                file: new_file.clone(),
                fail_mode: self.fail_mode(),
                path: normalized_path,
            }),
            append: options.append,
            write_only: options.write_only,
        };

        touch_parent(&mut fs, &resolved_path, now);
        insert_entry(&mut fs, resolved_path, TestFsEntry::File(new_file));

        Ok(file)
    }

    fn read_dir<P: AsRef<Path>>(&self, path: P) -> Result<Self::Dir> {
        let (fs, normalized_path, resolved_path) = self.lock_resolve(path.as_ref(), true)?;

        match fs.get(&resolved_path) {
            Some(TestFsEntry::File(_)) => {
                return Err(fs_error!(
                    NotADirectory;
                    "Not a directory: {}",
                    normalized_path.display()
                ));
            }
            Some(TestFsEntry::Symlink(_)) | None => {
                return Err(fs_error!(
                    missing_kind(&fs, &resolved_path) =>
                    "Directory not found: {}",
                    normalized_path.display()
                ));
            }
            Some(TestFsEntry::Dir(_)) => {}
        }

        Ok(TestReadDir {
            fs: self.fs.clone(),
            dir: resolved_path,
            path: normalized_path,
            cursor: None,
        })
    }

    fn create_dir<P: AsRef<Path>>(&self, path: P) -> Result {
        // WHY: like `mkdir`, a trailing separator neither follows a symlink
        // nor fails for an existing entry other than a directory.
        let (mut fs, normalized_path) = self.lock_normalize(path.as_ref())?;
        let link_path = self.resolve(&fs, &normalized_path, false)?;

        if fs.contains_key(&link_path) {
            return Err(fs_error!(
                AlreadyExists;
                "Directory could not be created, path already exists: {}",
                normalized_path.display()
            ));
        }

        check_parent(
            &fs,
            &link_path,
            &normalized_path,
            "Directory could not be created",
        )?;

        let now = SystemTime::now();
        touch_parent(&mut fs, &link_path, now);
        insert_entry(&mut fs, link_path, TestFsEntry::Dir(TestFsDir::new(now)));

        Ok(())
    }

    fn create_dir_all<P: AsRef<Path>>(&self, path: P) -> Result {
        let (mut fs, normalized_path) = self.lock_normalize(path.as_ref())?;
        let now = SystemTime::now();
        // WHY: an empty path is not a directory to create, as with
        // `std::fs::create_dir_all`.
        let ancestors: Vec<&Path> = normalized_path
            .ancestors()
            .filter(|ancestor| !ancestor.as_os_str().is_empty())
            .collect();
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
                // WHY: as with `mkdir`, a file fails as an existing path,
                // a file in a parent component as not a directory.
                Some(TestFsEntry::File(_)) => {
                    let kind = if ancestor == normalized_path {
                        std::io::ErrorKind::AlreadyExists
                    } else {
                        std::io::ErrorKind::NotADirectory
                    };

                    return Err(fs_error!(
                        kind =>
                        "Cannot create directory, a file already exists at: {}",
                        link.display()
                    ));
                }
                Some(TestFsEntry::Symlink(_)) | None => {
                    if matches!(fs.get(&link), Some(TestFsEntry::Symlink(_))) {
                        return Err(fs_error!(
                            AlreadyExists;
                            "Cannot create directory, a dangling symlink exists at: {}",
                            link.display()
                        ));
                    }

                    touch_parent(&mut fs, &dir, now);
                    insert_entry(&mut fs, dir.clone(), TestFsEntry::Dir(TestFsDir::new(now)));
                }
            }

            parent = dir;
        }

        Ok(())
    }

    fn exists<P: AsRef<Path>>(&self, path: P) -> Result<bool> {
        let (fs, normalized_path, resolved_path) = self.lock_resolve(path.as_ref(), true)?;

        if !fs.contains_key(&resolved_path)
            && missing_kind(&fs, &resolved_path) == std::io::ErrorKind::NotADirectory
        {
            return Err(fs_error!(
                NotADirectory;
                "Not a directory: {}",
                normalized_path.display()
            ));
        }

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
        let (mut fs, normalized_path) = self.lock_normalize(path.as_ref())?;
        let link_path = self.resolve(&fs, &normalized_path, false)?;
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
                InvalidInput;
                "Symlink could not be read, path is not a symlink: {}",
                normalized_path.display()
            )),
            None => Err(fs_error!(
                missing_kind(&fs, &link_path) =>
                "Symlink could not be read, path not found: {}",
                normalized_path.display()
            )),
        }
    }

    fn canonicalize<P: AsRef<Path>>(&self, path: P) -> Result<PathBuf> {
        let (fs, normalized_path, resolved_path) = self.lock_resolve(path.as_ref(), true)?;

        if fs.contains_key(&resolved_path) {
            Ok(resolved_path)
        } else {
            Err(fs_error!(
                missing_kind(&fs, &resolved_path) =>
                "Path could not be canonicalized, path not found: {}",
                normalized_path.display()
            ))
        }
    }

    fn create_hard_link<P: AsRef<Path>, T: AsRef<Path>>(&self, path: P, target: T) -> Result {
        let current_dir = self.env.current_dir();
        let (mut fs, normalized_path) = self.lock_normalize_at(&current_dir, path.as_ref())?;
        let link_path = self.resolve(&fs, &normalized_path, false)?;
        let normalized_target = normalize_at(&current_dir, target.as_ref())?;

        if fs.contains_key(&link_path) {
            return Err(fs_error!(
                AlreadyExists;
                "Hard link could not be created, path already exists: {}",
                normalized_path.display()
            ));
        }

        check_parent(
            &fs,
            &link_path,
            &normalized_path,
            "Hard link could not be created",
        )?;

        // WHY: like `link`, which cannot create a directory.
        if requires_dir(&normalized_path) {
            return Err(fs_error!(
                NotFound;
                "Hard link could not be created, path ends with a separator: {}",
                normalized_path.display()
            ));
        }

        let target_path = self.resolve_dir(&fs, &normalized_target, true)?;

        let file = match fs.get(&target_path) {
            Some(TestFsEntry::File(file)) => file.clone(),
            Some(TestFsEntry::Dir(_)) => {
                return Err(fs_error!(
                    PermissionDenied;
                    "Hard link could not be created, target is a directory: {}",
                    normalized_target.display()
                ));
            }
            Some(TestFsEntry::Symlink(_)) | None => {
                return Err(fs_error!(
                    missing_kind(&fs, &target_path) =>
                    "Hard link could not be created, target not found: {}",
                    normalized_target.display()
                ));
            }
        };

        touch_parent(&mut fs, &link_path, SystemTime::now());
        insert_entry(&mut fs, link_path, TestFsEntry::File(file));

        Ok(())
    }

    fn copy<S: AsRef<Path>, T: AsRef<Path>>(&self, source: S, target: T) -> Result {
        let current_dir = self.env.current_dir();
        let normalized_source = normalize_at(&current_dir, source.as_ref())?;
        let (mut fs, normalized_target) = self.lock_normalize_at(&current_dir, target.as_ref())?;
        let target_path = self.resolve(&fs, &normalized_target, false)?;
        let source_path = self.resolve_dir(&fs, &normalized_source, false)?;

        let Some(source_entry) = fs.get(&source_path) else {
            return Err(fs_error!(
                missing_kind(&fs, &source_path) =>
                "Path could not be copied, source not found: {}",
                normalized_source.display()
            ));
        };

        // WHY: like `std::fs::copy`, a symlink at `target` is followed and
        // a file there is overwritten, a directory or a symlink is copied
        // only to a new path.
        let copy = match source_entry {
            TestFsEntry::File(file) => {
                // WHY: like `O_CREAT`, a path that must be a directory cannot
                // be created as a file, whether it exists or not.
                if requires_dir(&normalized_target) {
                    check_copy_parent(&fs, &target_path, &normalized_target)?;

                    return Err(fs_error!(
                        IsADirectory;
                        "File could not be opened, path is a directory: {}",
                        normalized_target.display()
                    ));
                }

                let file = file.clone();
                let target_path = self.resolve(&fs, &target_path, true)?;
                return copy_file(
                    &mut fs,
                    &file,
                    &normalized_source,
                    target_path,
                    &normalized_target,
                );
            }
            TestFsEntry::Dir(dir) => Ok(dir.attributes),
            TestFsEntry::Symlink(symlink) => Err(symlink.target.clone()),
        };

        if fs.contains_key(&target_path) {
            return Err(fs_error!(
                AlreadyExists;
                "Path could not be copied, target already exists: {}",
                normalized_target.display()
            ));
        }

        check_copy_parent(&fs, &target_path, &normalized_target)?;
        let now = SystemTime::now();
        let entry = match copy {
            Ok(attributes) => {
                if has_children(&fs, &source_path) {
                    return Err(fs_error!(
                        DirectoryNotEmpty;
                        "Directory could not be copied, it is not empty: {}",
                        normalized_source.display()
                    ));
                }

                let mut copy = TestFsDir::new(now);
                copy.attributes = copied_attributes(attributes);
                TestFsEntry::Dir(copy)
            }
            Err(link) => {
                return self.add_symlink(&mut fs, &normalized_target, target_path, link);
            }
        };
        touch_parent(&mut fs, &target_path, now);
        insert_entry(&mut fs, target_path, entry);

        Ok(())
    }

    fn copy_dir_all<S: AsRef<Path>, T: AsRef<Path>>(&self, source: S, target: T) -> Result {
        let current_dir = self.env.current_dir();
        copy_tree(
            self,
            &normalize_at(&current_dir, source.as_ref())?,
            &normalize_at(&current_dir, target.as_ref())?,
        )
    }

    fn remove<P: AsRef<Path>>(&self, path: P) -> Result {
        let (mut fs, normalized_path, link_path) = self.lock_resolve(path.as_ref(), false)?;
        self.check_not_dir_symlink(&fs, &normalized_path, &link_path)?;

        match fs.get(&link_path) {
            Some(TestFsEntry::File(file)) => match lock_file_read(file).state {
                BorrowState::Read(0) => {}
                BorrowState::Read(count) => {
                    return Err(fs_error!(
                        ResourceBusy;
                        "File could not be removed, it is already borrowed for reading {count} times: {}",
                        normalized_path.display()
                    ));
                }
                BorrowState::Write => {
                    return Err(fs_error!(
                        ResourceBusy;
                        "File could not be removed, it is already borrowed for writing: {}",
                        normalized_path.display()
                    ));
                }
            },
            Some(TestFsEntry::Dir(_)) => {
                if link_path.parent().is_none() {
                    return Err(fs_error!(
                        ResourceBusy;
                        "Directory could not be removed, path is a root: {}",
                        normalized_path.display()
                    ));
                }

                if has_children(&fs, &link_path) {
                    return Err(fs_error!(
                        DirectoryNotEmpty;
                        "Directory could not be removed, it is not empty: {}",
                        normalized_path.display()
                    ));
                }
            }
            Some(TestFsEntry::Symlink(_)) => {}
            None => {
                return Err(fs_error!(
                    missing_kind(&fs, &link_path) =>
                    "Path could not be removed, path not found: {}",
                    normalized_path.display()
                ));
            }
        }

        if let Some(TestFsEntry::Symlink(_)) = remove_entry(&mut fs, &link_path) {
            self.symlinks.fetch_sub(1, Ordering::Relaxed);
        }

        touch_parent(&mut fs, &link_path, SystemTime::now());

        Ok(())
    }

    fn remove_dir_all<P: AsRef<Path>>(&self, path: P) -> Result {
        let (mut fs, normalized_path, link_path) = self.lock_resolve(path.as_ref(), false)?;
        check_not_root(&link_path)?;
        self.check_not_dir_symlink(&fs, &normalized_path, &link_path)?;

        match fs.get(&link_path) {
            Some(TestFsEntry::Dir(_)) | Some(TestFsEntry::Symlink(_)) => {}
            Some(TestFsEntry::File(_)) => {
                return Err(fs_error!(
                    NotADirectory;
                    "Directory could not be removed, path is not a directory: {}",
                    normalized_path.display()
                ));
            }
            None => {
                return Err(fs_error!(
                    missing_kind(&fs, &link_path) =>
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
                            ResourceBusy;
                            "File could not be removed, it is already borrowed for reading {count} times: {}",
                            display_path()
                        ));
                    }
                    BorrowState::Write => {
                        return Err(fs_error!(
                            ResourceBusy;
                            "File could not be removed, it is already borrowed for writing: {}",
                            display_path()
                        ));
                    }
                }
            }
        }

        for path in &tree {
            if let Some(TestFsEntry::Symlink(_)) = remove_entry(&mut fs, path) {
                self.symlinks.fetch_sub(1, Ordering::Relaxed);
            }
        }

        touch_parent(&mut fs, &link_path, SystemTime::now());

        Ok(())
    }

    fn rename<S: AsRef<Path>, T: AsRef<Path>>(&self, source: S, target: T) -> Result {
        let current_dir = self.env.current_dir();
        let normalized_source = normalize_at(&current_dir, source.as_ref())?;
        let (mut fs, normalized_target) = self.lock_normalize_at(&current_dir, target.as_ref())?;
        let target_path = self.resolve(&fs, &normalized_target, false)?;
        let source_path = self.resolve(&fs, &normalized_source, false)?;

        let Some(source_entry) = fs.get(&source_path) else {
            return Err(fs_error!(
                missing_kind(&fs, &source_path) =>
                "Path could not be renamed, source not found: {}",
                normalized_source.display()
            ));
        };

        // WHY: like `rename`, a trailing separator does not follow a
        // symlink but requires a directory.
        if requires_dir(&normalized_source) && !matches!(source_entry, TestFsEntry::Dir(_)) {
            return Err(fs_error!(
                NotADirectory;
                "Path could not be renamed, source is not a directory: {}",
                normalized_source.display()
            ));
        }

        if source_path == target_path {
            return self.check_rename_target_dir(
                &fs,
                source_entry,
                &normalized_target,
                &target_path,
            );
        }

        if source_path.parent().is_none() {
            return Err(fs_error!(
                ResourceBusy;
                "Path could not be renamed, source is a root: {}",
                normalized_source.display()
            ));
        }

        check_parent(
            &fs,
            &target_path,
            &normalized_target,
            "Path could not be renamed",
        )?;
        self.check_rename_target_dir(&fs, source_entry, &normalized_target, &target_path)?;

        if target_path.starts_with(&source_path) {
            return Err(fs_error!(
                InvalidInput;
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
                remove_entry(&mut fs, &source_path);
                touch_parent(&mut fs, &source_path, SystemTime::now());
                return Ok(());
            }
            (TestFsEntry::Dir(_), Some(TestFsEntry::Dir(_))) => {
                if has_children(&fs, &target_path) {
                    return Err(fs_error!(
                        DirectoryNotEmpty;
                        "Directory could not be renamed, target is not empty: {}",
                        normalized_target.display()
                    ));
                }
            }
            (TestFsEntry::Dir(_), Some(_)) => {
                return Err(fs_error!(
                    NotADirectory;
                    "Directory could not be renamed, target is not a directory: {}",
                    normalized_target.display()
                ));
            }
            (_, Some(TestFsEntry::Dir(_))) => {
                return Err(fs_error!(
                    IsADirectory;
                    "Path could not be renamed, target is a directory: {}",
                    normalized_target.display()
                ));
            }
            (_, Some(TestFsEntry::File(target_file))) => match lock_file_read(target_file).state {
                BorrowState::Read(0) => {}
                BorrowState::Read(count) => {
                    return Err(fs_error!(
                        ResourceBusy;
                        "File could not be replaced, it is already borrowed for reading {count} times: {}",
                        normalized_target.display()
                    ));
                }
                BorrowState::Write => {
                    return Err(fs_error!(
                        ResourceBusy;
                        "File could not be replaced, it is already borrowed for writing: {}",
                        normalized_target.display()
                    ));
                }
            },
            (_, Some(TestFsEntry::Symlink(_))) => {}
        }

        if let Some(TestFsEntry::Symlink(_)) = remove_entry(&mut fs, &target_path) {
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
            .filter_map(|path| remove_entry(&mut fs, &path).map(|entry| (path, entry)))
            .collect();

        for (path, entry) in entries {
            let moved = target_path.join(path.strip_prefix(&source_path).unwrap_or(&path));
            insert_entry(&mut fs, moved, entry);
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
                    missing_kind(&fs, &resolved_path) =>
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
                AlreadyExists;
                "Symlink could not be created, path already exists: {}",
                normalized_path.display()
            ));
        }

        let Some(parent) = link_path.parent().filter(|parent| is_dir(fs, parent)) else {
            return Err(fs_error!(
                NotFound;
                "Symlink could not be created, parent directory does not exist: {}",
                normalized_path.display()
            ));
        };

        // WHY: always resolved in full, the joined target is not normalized
        // and may contain `..`.
        let resolved_target = resolve_symlinks(fs, &parent.join(&target), true)?;

        // WHY: `Path::components` ignores an empty target, which the OS
        // rejects.
        if target.as_os_str().is_empty() {
            return Err(fs_error!(
                NotFound;
                "Symlink could not be created, target not found: {}",
                target.display()
            ));
        }

        if !matches!(
            fs.get(&resolved_target),
            Some(TestFsEntry::File(_)) | Some(TestFsEntry::Dir(_))
        ) {
            return Err(fs_error!(
                missing_kind(fs, &resolved_target) =>
                "Symlink could not be created, target not found: {}",
                target.display()
            ));
        }

        if ends_with_separator(&target) && !is_dir(fs, &resolved_target) {
            return Err(fs_error!(
                NotADirectory;
                "Symlink could not be created, target is not a directory: {}",
                target.display()
            ));
        }

        // WHY: like `symlink`, which cannot create a directory.
        if requires_dir(normalized_path) {
            return Err(fs_error!(
                NotFound;
                "Symlink could not be created, path ends with a separator: {}",
                normalized_path.display()
            ));
        }

        let now = SystemTime::now();
        touch_parent(fs, &link_path, now);
        insert_entry(
            fs,
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
                    IsADirectory;
                    "Executable attribute cannot be set, path is a directory: {}",
                    normalized_path.display()
                ));
            }
            Some(TestFsEntry::File(file)) => lock_file_write(file).attributes.set(attribute, value),
            Some(TestFsEntry::Dir(dir)) => dir.attributes.set(attribute, value),
            Some(TestFsEntry::Symlink(_)) | None => {
                return Err(fs_error!(
                    missing_kind(&fs, &resolved_path) =>
                    "Attributes could not be set, path not found: {}",
                    normalized_path.display()
                ));
            }
        }

        Ok(())
    }

    /// Fails if the normalized `path` [`requires_dir`] but is a symlink,
    /// to the directory at the resolved `dir`, which [`Fs::remove`] and
    /// [`Fs::remove_dir_all`] would follow, the OS removes neither.
    fn check_not_dir_symlink(
        &self,
        fs: &BTreeMap<PathBuf, TestFsEntry>,
        path: &Path,
        dir: &Path,
    ) -> Result {
        if requires_dir(path)
            && fs.contains_key(dir)
            && self
                .resolve(fs, path, false)
                .is_ok_and(|link| matches!(fs.get(&link), Some(TestFsEntry::Symlink(_))))
        {
            return Err(fs_error!(
                NotADirectory;
                "Path could not be removed, path is a symlink: {}",
                path.display()
            ));
        }

        Ok(())
    }

    /// Fails if the normalized `target` of [`Fs::rename`], at the
    /// unresolved `target_path`, [`requires_dir`] but the `source` cannot
    /// replace it as a directory, like `rename` which does not follow it.
    fn check_rename_target_dir(
        &self,
        fs: &BTreeMap<PathBuf, TestFsEntry>,
        source: &TestFsEntry,
        target: &Path,
        target_path: &Path,
    ) -> Result {
        if !requires_dir(target) {
            return Ok(());
        }

        match (source, fs.get(target_path)) {
            (TestFsEntry::Dir(_), None | Some(TestFsEntry::Dir(_))) => Ok(()),
            (TestFsEntry::Dir(_), Some(_)) => Err(fs_error!(
                NotADirectory;
                "Directory could not be renamed, target is not a directory: {}",
                target.display()
            )),
            // WHY: `FsImpl` checks the types with the symlink followed, as
            // the OS does for a path ending with a separator, see
            // `check_rename_types`.
            _ if is_dir(fs, &self.resolve(fs, target_path, true)?) => Err(fs_error!(
                IsADirectory;
                "Path could not be renamed, target is a directory: {}",
                target.display()
            )),
            _ => Err(fs_error!(
                NotADirectory;
                "Path could not be renamed, target is not a directory: {}",
                target.display()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A change of the current directory of the env the file system
    /// shares, like by its platform.
    impl TestFs {
        fn set_current_dir(&self, path: PathBuf) {
            self.env
                .replace_current_dir(crate::utils::resolve_path(&self.env.current_dir(), &path));
        }
    }
    use std::io::SeekFrom;

    /// Inserts a symlink without checking its target, which
    /// [`Fs::create_symlink`] requires to exist.
    fn insert_symlink(fs: &TestFs, path: &str, target: &str) {
        lock_handles(&fs.fs).insert(
            normalize_path(fs.current_dir(), path).into_owned(),
            TestFsEntry::Symlink(TestFsSymlink::new(target.into(), SystemTime::now())),
        );
        fs.symlinks.fetch_add(1, Ordering::Relaxed);
    }

    /// Returns the kind of the [`std::io::Error`] that caused the `error`.
    fn io_kind(error: &crate::Error) -> Option<std::io::ErrorKind> {
        error
            .source_as::<std::io::Error>()
            .map(std::io::Error::kind)
    }

    /// Returns the `error` displayed with the kind of its cause.
    fn described(error: &crate::Error) -> (String, Option<std::io::ErrorKind>) {
        (error.to_string(), io_kind(error))
    }

    /// Asserts that `error` has the description and the cause of the
    /// `expected` error.
    #[track_caller]
    fn assert_error(error: &crate::Error, expected: crate::Error) {
        assert_eq!(described(error), described(&expected));
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
            assert_error(
                &fs.open_write("/b.txt").err().unwrap(),
                fs_error!(
                    ResourceBusy;
                    "File could not be opened, it is already borrowed for reading 1 times: {}",
                    normalize_path(fs.current_dir(), "/b.txt").display()
                ),
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
            assert_error(
                &fs.open_read("/a.txt").err().unwrap(),
                fs_error!(
                    ResourceBusy;
                    "File could not be opened, it is already borrowed for writing: {}",
                    normalize_path(fs.current_dir(), "/a.txt").display()
                ),
            );
        }

        #[test]
        fn test_fs_should_enforce_one_writer_or_many_readers() {
            let fs = TestFs::new();
            fs.set_current_dir("/tmp".into());
            fs.create_dir_all("/tmp").unwrap();
            fs.write("Cargo.toml", "project = \"agplatform\"").unwrap();
            let normalized_path = normalize_path(fs.current_dir(), "Cargo.toml");

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
                assert_error(
                    &fs.set_created("/a.txt", SystemTime::UNIX_EPOCH)
                        .err()
                        .unwrap(),
                    fs_error!(
                        NotFound;
                        "Creation time could not be set, path not found: {}",
                        normalize_path(fs.current_dir(), "/a.txt").display()
                    ),
                );
            }

            #[test]
            fn set_created_invalid_path_should_fail() {
                let fs = TestFs::new();
                assert!(fs.set_created("\0", SystemTime::UNIX_EPOCH).is_err());
            }
        }

        mod set_len {
            use super::*;

            #[test]
            fn set_len_should_extend_with_zeros_not_held_in_memory() {
                // GIVEN a file with content
                let fs = TestFs::new();
                fs.write("/a.txt", "content").unwrap();

                // WHEN making it larger than can be written
                fs.set_len("/a.txt", MAX_FILE_LEN * 10).unwrap();

                // THEN it reports the length, reads the content followed by
                // zeros and holds only the content
                assert_eq!(fs.metadata("/a.txt").unwrap().len, MAX_FILE_LEN * 10);
                let mut file = fs.open_read("/a.txt").unwrap();
                let mut buf = [1; 9];
                file.read_exact(&mut buf).unwrap();
                assert_eq!(&buf, b"content\0\0");
                file.seek(SeekFrom::End(-2)).unwrap();
                assert_eq!(file.read(&mut buf).unwrap(), 2);
                assert_eq!(&buf[..2], b"\0\0");
                assert_eq!(lock_file_read(&file.0.file).data.len(), 7);
            }

            #[test]
            fn set_len_should_truncate_ignoring_borrows_and_permissions() {
                let fs = TestFs::new();
                fs.write("/a.txt", "content").unwrap();
                fs.set_readonly("/a.txt", true).unwrap();
                let file = fs.open_read("/a.txt").unwrap();

                fs.set_len("/a.txt", 2).unwrap();

                drop(file);
                assert_eq!(fs.read("/a.txt").unwrap(), b"co");
            }

            #[test]
            fn set_len_should_follow_symlinks() {
                let fs = TestFs::new();
                fs.write("/a.txt", "content").unwrap();
                fs.create_symlink("/link", "/a.txt").unwrap();

                fs.set_len("/link", 2).unwrap();

                assert_eq!(fs.read("/a.txt").unwrap(), b"co");
            }

            #[test]
            fn set_len_should_fail_for_directory_missing_path_and_invalid_length() {
                let fs = TestFs::new();
                fs.write("/a.txt", "content").unwrap();
                fs.create_dir_all("/dir").unwrap();
                let path = |path| normalize_path(fs.current_dir(), path);

                assert_error(
                    &fs.set_len("/dir", 0).err().unwrap(),
                    fs_error!(
                        IsADirectory;
                        "File length could not be set, it is a directory: {}",
                        path("/dir").display()
                    ),
                );
                assert_error(
                    &fs.set_len("/b.txt", 0).err().unwrap(),
                    fs_error!(
                        NotFound;
                        "File length could not be set, path not found: {}",
                        path("/b.txt").display()
                    ),
                );
                assert_error(
                    &fs.set_len("/a.txt", u64::MAX).err().unwrap(),
                    invalid_len_error(&path("/a.txt")),
                );
                assert!(fs.set_len("\0", 0).is_err());
                assert_eq!(fs.read("/a.txt").unwrap(), b"content");
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
                assert_error(
                    &fs.set_owner("/a.txt", 0, 0).err().unwrap(),
                    fs_error!(
                        NotFound;
                        "Owner could not be set, path not found: {}",
                        normalize_path(fs.current_dir(), "/a.txt").display()
                    ),
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
                let fs = TestFs::new();
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
                let fs = TestFs::new();
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
                let fs = TestFs::new();
                fs.write("/test.txt", "content").unwrap();

                fs.set_fail_mode(Some(FailMode::Write));
                assert!(fs.write("/test.txt", "content").is_err());

                fs.set_fail_mode(None);
                assert!(fs.read("/test.txt").unwrap().is_empty());
            }

            #[test]
            fn set_fail_mode_flush_should_cause_flush_failure() {
                let fs = TestFs::new();
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
                let path = normalize_path(fs.current_dir(), "/link/a.txt");

                assert_eq!(
                    fs.resolve(&lock_handles(&fs.fs), &path, true).unwrap(),
                    path
                );

                fs.create_symlink("/link", "dir").unwrap();

                assert_eq!(
                    fs.resolve(&lock_handles(&fs.fs), &path, true).unwrap(),
                    normalize_path(fs.current_dir(), "/dir/a.txt")
                );
            }
        }

        mod set_env {
            use super::*;
            use crate::Env;

            #[test]
            fn set_env_relative_should_resolve_against_changed_current_dir_of_env() {
                // GIVEN: a file above the current directory `/a/b` of an env
                let mut fs = TestFs::new();
                fs.create_dir_all("/a/b").unwrap();
                fs.write("/top.txt", "").unwrap();
                let env = crate::TestEnv::new().with_current_dir(normalize_path(ROOT, "/a/b"));
                fs.set_env(env.shared().clone());

                // WHEN
                env.set_current_dir("..");

                // THEN
                assert_eq!(fs.current_dir(), normalize_path(ROOT, "/a"));
                assert!(fs.exists("../top.txt").unwrap());
            }
        }

        mod fs {
            use super::*;

            mod open_read {
                use super::*;

                #[test]
                fn open_read_with_trailing_separator_should_fail_for_file() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();

                    assert_error(
                        &fs.open_read("/a.txt/").err().unwrap(),
                        fs_error!(
                            NotADirectory;
                            "Not a directory: {}",
                            normalize_path(fs.current_dir(), "/a.txt/").display()
                        ),
                    );
                }

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

                    assert_error(
                        &fs.open_read("/mydir").err().unwrap(),
                        fs_error!(
                            IsADirectory;
                            "File could not be opened, path is a directory: {}",
                            normalize_path(fs.current_dir(), "/mydir").display()
                        ),
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
                            normalize_path(fs.current_dir(), "/a").display()
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
                fn open_write_options_create_dir_path_should_fail_without_creating() {
                    let fs = TestFs::new();

                    assert_error(
                        &fs.open_write_options("/a.txt/", OpenWriteOptions::new().create())
                            .err()
                            .unwrap(),
                        fs_error!(
                            IsADirectory;
                            "File could not be opened, path is a directory: {}",
                            normalize_path(fs.current_dir(), "/a.txt/").display()
                        ),
                    );
                    assert!(!fs.exists("/a.txt").unwrap());
                }

                #[test]
                fn open_write_options_create_dir_path_without_parent_dir_should_fail() {
                    let fs = TestFs::new();

                    assert_error(
                        &fs.open_write_options("/missing/a.txt/", OpenWriteOptions::new().create())
                            .err()
                            .unwrap(),
                        fs_error!(
                            NotFound;
                            "File could not be created, parent directory does not exist: {}",
                            normalize_path(fs.current_dir(), "/missing/a.txt/").display()
                        ),
                    );
                }

                #[test]
                fn open_write_options_write_only_should_write_but_not_read() {
                    let fs = TestFs::new();
                    let mut file = fs
                        .open_write_options("/a.txt", OpenWriteOptions::new().create().write_only())
                        .unwrap();

                    file.write_all(b"aaa").unwrap();

                    file.rewind().unwrap();
                    assert!(file.read(&mut [0_u8; 3]).is_err());
                    drop(file);
                    assert_eq!(fs.read_to_string("/a.txt").unwrap(), "aaa");
                }

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

                    assert_error(
                        &fs.open_write("/test.txt").err().unwrap(),
                        fs_error!(
                            PermissionDenied;
                            "File could not be opened, it is read-only: {}",
                            normalize_path(fs.current_dir(), "/test.txt").display()
                        ),
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

                    assert_error(
                        &fs.open_write_options("/mydir", OpenWriteOptions::new().create())
                            .err()
                            .unwrap(),
                        fs_error!(
                            IsADirectory;
                            "File could not be opened, path is a directory: {}",
                            normalize_path(fs.current_dir(), "/mydir").display()
                        ),
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

                    for (path, kind) in [
                        ("/missing/a.txt", std::io::ErrorKind::NotFound),
                        ("/file/a.txt", std::io::ErrorKind::NotADirectory),
                    ] {
                        assert_eq!(
                            described(
                                &fs.open_write_options(path, OpenWriteOptions::new().create())
                                    .err()
                                    .unwrap()
                            ),
                            described(&fs_error!(
                                kind =>
                                "File could not be created, parent directory does not exist: {}",
                                normalize_path(fs.current_dir(), path).display()
                            )),
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
                    insert_symlink(&fs, "/loop", "loop");

                    for path in ["\0", "/loop/a.txt", "/loop/a.txt/"] {
                        assert!(
                            fs.open_write_options(path, OpenWriteOptions::new().create())
                                .is_err(),
                            "path: {path}"
                        );
                    }
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

                    assert_error(
                        &fs.open_write_options("/link", OpenWriteOptions::new().create_new())
                            .err()
                            .unwrap(),
                        fs_error!(
                            AlreadyExists;
                            "File could not be opened with 'create_new' option, file already exists: {}",
                            normalize_path(fs.current_dir(), "/link").display()
                        ),
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
                fn read_dir_should_list_entries_in_hash_order() {
                    let fs = TestFs::new();
                    fs.create_dir("/dir").unwrap();

                    for name in ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"] {
                        fs.write(format!("/dir/{name}").as_str(), "").unwrap();
                    }

                    let names: Vec<String> = fs
                        .read_dir("/dir")
                        .unwrap()
                        .map(|e| e.unwrap().file_name().to_string())
                        .collect();
                    assert_eq!(names, ["b.txt", "c.txt", "d.txt", "e.txt", "a.txt"]);
                }

                #[test]
                fn read_dir_should_continue_after_changes_during_iteration() {
                    // GIVEN: a listing that has returned `b.txt` and `c.txt`
                    let fs = TestFs::new();
                    fs.create_dir("/dir").unwrap();

                    for name in ["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"] {
                        fs.write(format!("/dir/{name}").as_str(), "").unwrap();
                    }

                    let mut entries = fs.read_dir("/dir").unwrap();
                    entries.next().unwrap().unwrap();
                    entries.next().unwrap().unwrap();

                    // WHEN: a returned entry is recreated, one not returned yet is
                    // removed and one is added
                    fs.remove("/dir/b.txt").unwrap();
                    fs.write("/dir/b.txt", "").unwrap();
                    fs.remove("/dir/d.txt").unwrap();
                    fs.write("/dir/f.txt", "").unwrap();

                    // THEN: the listing continues with the remaining entries only
                    let names: Vec<String> = entries
                        .map(|e| e.unwrap().file_name().to_string())
                        .collect();
                    assert_eq!(names, ["e.txt", "a.txt", "f.txt"]);
                }

                #[test]
                fn read_dir_removed_during_iteration_should_end() {
                    let fs = TestFs::new();
                    fs.create_dir("/dir").unwrap();
                    fs.write("/dir/a.txt", "").unwrap();
                    fs.write("/dir/b.txt", "").unwrap();
                    let mut entries = fs.read_dir("/dir").unwrap();
                    entries.next().unwrap().unwrap();

                    fs.remove_dir_all("/dir").unwrap();

                    assert!(entries.next().is_none());
                }

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
                        normalize_path(fs.current_dir(), "/mydir/file.txt")
                    );

                    let dir = entries.iter().find(|e| e.file_name() == "subdir").unwrap();
                    assert!(
                        dir.file_type().is_dir(),
                        "expected subdir to be a directory"
                    );
                    assert_eq!(
                        dir.path(),
                        normalize_path(fs.current_dir(), "/mydir/subdir")
                    );
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
                        normalize_path(fs.current_dir(), "/").join(name),
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

                    assert_error(
                        &fs.read_dir("/a.txt").err().unwrap(),
                        fs_error!(
                            NotADirectory;
                            "Not a directory: {}",
                            normalize_path(fs.current_dir(), "/a.txt").display()
                        ),
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
                    assert_eq!(paths, vec![normalize_path(fs.current_dir(), "/link/a.txt")]);
                }

                #[test]
                fn read_dir_dangling_symlink_should_fail() {
                    let fs = TestFs::new();
                    insert_symlink(&fs, "/link", "dir");

                    assert_error(
                        &fs.read_dir("/link").err().unwrap(),
                        fs_error!(
                            NotFound;
                            "Directory not found: {}",
                            normalize_path(fs.current_dir(), "/link").display()
                        ),
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
                            described(&fs.create_dir(path).err().unwrap()),
                            described(&fs_error!(
                                AlreadyExists;
                                "Directory could not be created, path already exists: {}",
                                normalize_path(fs.current_dir(), path).display()
                            )),
                            "path: {path}"
                        );
                    }
                    assert!(!fs.exists("/missing").unwrap());
                }

                #[test]
                fn create_dir_without_parent_dir_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();

                    for (path, kind) in [
                        ("/missing/dir", std::io::ErrorKind::NotFound),
                        ("/a.txt/dir", std::io::ErrorKind::NotADirectory),
                    ] {
                        assert_eq!(
                            described(&fs.create_dir(path).err().unwrap()),
                            described(&fs_error!(
                                kind =>
                                "Directory could not be created, parent directory does not exist: {}",
                                normalize_path(fs.current_dir(), path).display()
                            )),
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
                fn create_dir_all_empty_path_should_create_nothing() {
                    let fs = TestFs::new();

                    fs.create_dir_all("").unwrap();

                    assert!(!fs.exists("").unwrap());
                }

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

                    assert_error(
                        &fs.create_dir_all("/a/b").err().unwrap(),
                        fs_error!(
                            NotADirectory;
                            "Cannot create directory, a file already exists at: {}",
                            normalize_path(fs.current_dir(), "/a").display()
                        ),
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

                    assert_error(
                        &fs.create_dir_all("/link/b").err().unwrap(),
                        fs_error!(
                            NotADirectory;
                            "Cannot create directory, a file already exists at: {}",
                            normalize_path(fs.current_dir(), "/link").display()
                        ),
                    );
                }

                #[test]
                fn create_dir_all_dangling_symlink_should_fail() {
                    let fs = TestFs::new();
                    insert_symlink(&fs, "/link", "dir");

                    assert_error(
                        &fs.create_dir_all("/link/b").err().unwrap(),
                        fs_error!(
                            AlreadyExists;
                            "Cannot create directory, a dangling symlink exists at: {}",
                            normalize_path(fs.current_dir(), "/link").display()
                        ),
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
                fn exists_below_file_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();

                    for path in ["/a.txt/b.txt", "/a.txt/"] {
                        assert_eq!(
                            described(&fs.exists(path).err().unwrap()),
                            described(&fs_error!(
                                NotADirectory;
                                "Not a directory: {}",
                                normalize_path(fs.current_dir(), path).display()
                            )),
                            "path: {path}"
                        );
                    }
                }

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

                #[cfg(windows)]
                #[test]
                fn metadata_other_root_should_fail_until_created() {
                    let other_root = "D:\\";

                    let fs = TestFs::new();
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
                    assert_error(
                        &fs.metadata("/a.txt").err().unwrap(),
                        fs_error!(
                            NotFound;
                            "Metadata could not be read, path not found: {}",
                            normalize_path(fs.current_dir(), "/a.txt").display()
                        ),
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
                fn symlink_metadata_with_trailing_separator_should_follow_symlink() {
                    let fs = TestFs::new();
                    fs.create_dir("/dir").unwrap();
                    fs.create_symlink("/link", "dir").unwrap();

                    assert_eq!(
                        fs.symlink_metadata("/link/").unwrap().file_type,
                        FileType::Dir
                    );
                    assert_eq!(
                        fs.symlink_metadata("/link").unwrap().file_type,
                        FileType::Symlink
                    );
                }

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
                fn create_symlink_with_trailing_separator_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();

                    assert_error(
                        &fs.create_symlink("/link/", "a.txt").err().unwrap(),
                        fs_error!(
                            NotFound;
                            "Symlink could not be created, path ends with a separator: {}",
                            normalize_path(fs.current_dir(), "/link/").display()
                        ),
                    );
                    assert!(!fs.exists("/link").unwrap());
                }

                #[test]
                fn create_symlink_parent_dir_of_non_directory_in_target_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();

                    for (target, resolved, kind) in [
                        ("missing/../a.txt", "/missing", std::io::ErrorKind::NotFound),
                        (
                            "a.txt/../a.txt",
                            "/a.txt",
                            std::io::ErrorKind::NotADirectory,
                        ),
                    ] {
                        assert_eq!(
                            described(&fs.create_symlink("/link", target).err().unwrap()),
                            described(&fs_error!(
                                kind =>
                                "Path could not be resolved, not a directory: {}",
                                normalize_path(fs.current_dir(), resolved).display()
                            )),
                            "target: {target}"
                        );
                    }
                }

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
                            described(&fs.create_symlink(path, target).err().unwrap()),
                            described(&fs_error!(
                                AlreadyExists;
                                "Symlink could not be created, path already exists: {}",
                                normalize_path(fs.current_dir(), path).display()
                            )),
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
                    assert_error(
                        &fs.create_symlink("/file_link", "a.txt/").err().unwrap(),
                        fs_error!(
                            NotADirectory;
                            "Symlink could not be created, target is not a directory: {}",
                            native_symlink_target(Path::new("a.txt/")).display()
                        ),
                    );
                }

                #[test]
                fn create_symlink_without_parent_dir_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();

                    for path in ["/missing/link", "/a.txt/link"] {
                        assert_eq!(
                            described(&fs.create_symlink(path, "/a.txt").err().unwrap()),
                            described(&fs_error!(
                                NotFound;
                                "Symlink could not be created, parent directory does not exist: {}",
                                normalize_path(fs.current_dir(), path).display()
                            )),
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
                            described(&fs.create_symlink("/link", target).err().unwrap()),
                            described(
                                &fs_error!(NotFound; "Symlink could not be created, target not found: {target}")
                            ),
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
                    insert_symlink(&fs, "/loop", "loop");

                    assert!(fs.create_symlink("\0", "a.txt").is_err());
                    assert!(fs.create_symlink("/loop/link", "a.txt").is_err());
                }
            }

            mod canonicalize {
                use super::*;

                #[test]
                fn canonicalize_should_resolve_symlinks_and_dots() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/dir/a.txt", "aaa").unwrap();
                    fs.create_symlink("/link", "/dir").unwrap();

                    assert_eq!(
                        fs.canonicalize("/link/./a.txt").unwrap(),
                        Path::new(ROOT).join("dir").join("a.txt")
                    );
                }

                #[test]
                fn canonicalize_path_with_nul_should_fail() {
                    let fs = TestFs::new();

                    assert_eq!(
                        fs.canonicalize("/a\0b").err().unwrap().to_string(),
                        fs_error!("Path contains a NUL byte: /a\0b").to_string()
                    );
                }

                #[test]
                fn canonicalize_nonexistent_path_should_fail() {
                    let fs = TestFs::new();

                    assert_error(
                        &fs.canonicalize("/missing").err().unwrap(),
                        fs_error!(
                            NotFound;
                            "Path could not be canonicalized, path not found: {}",
                            Path::new(ROOT).join("missing").display()
                        ),
                    );
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
                            described(&fs.read_link(path).err().unwrap()),
                            described(&fs_error!(
                                InvalidInput;
                                "Symlink could not be read, path is not a symlink: {}",
                                normalize_path(fs.current_dir(), path).display()
                            )),
                            "path: {path}"
                        );
                    }
                }

                #[test]
                fn read_link_should_fail_for_nonexistent_path() {
                    let fs = TestFs::new();
                    assert_error(
                        &fs.read_link("/link").err().unwrap(),
                        fs_error!(
                            NotFound;
                            "Symlink could not be read, path not found: {}",
                            normalize_path(fs.current_dir(), "/link").display()
                        ),
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
                fn create_hard_link_with_trailing_separator_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();

                    assert_error(
                        &fs.create_hard_link("/b.txt/", "/a.txt").err().unwrap(),
                        fs_error!(
                            NotFound;
                            "Hard link could not be created, path ends with a separator: {}",
                            normalize_path(fs.current_dir(), "/b.txt/").display()
                        ),
                    );
                    assert!(!fs.exists("/b.txt").unwrap());
                }

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
                    let fs = TestFs::new();
                    let dir = normalize_path(fs.current_dir(), "/dir").into_owned();
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
                            described(&fs.create_hard_link(path, target).err().unwrap()),
                            described(&fs_error!(
                                AlreadyExists;
                                "Hard link could not be created, path already exists: {}",
                                normalize_path(fs.current_dir(), path).display()
                            )),
                            "path: {path}, target: {target}"
                        );
                    }
                }

                #[test]
                fn create_hard_link_without_parent_dir_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();

                    for (path, kind) in [
                        ("/missing/link", std::io::ErrorKind::NotFound),
                        ("/a.txt/link", std::io::ErrorKind::NotADirectory),
                    ] {
                        assert_eq!(
                            described(&fs.create_hard_link(path, "/a.txt").err().unwrap()),
                            described(&fs_error!(
                                kind =>
                                "Hard link could not be created, parent directory does not exist: {}",
                                normalize_path(fs.current_dir(), path).display()
                            )),
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
                            described(&fs.create_hard_link("/link", target).err().unwrap()),
                            described(&fs_error!(
                                NotFound;
                                "Hard link could not be created, target not found: {}",
                                normalize_path(fs.current_dir(), target).display()
                            )),
                            "target: {target}"
                        );
                    }
                }

                #[test]
                fn create_hard_link_dir_target_should_fail() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();

                    assert_error(
                        &fs.create_hard_link("/link", "/dir").err().unwrap(),
                        fs_error!(
                            PermissionDenied;
                            "Hard link could not be created, target is a directory: {}",
                            normalize_path(fs.current_dir(), "/dir").display()
                        ),
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

                    insert_symlink(&fs, "/loop", "loop");

                    assert!(fs.create_hard_link("\0", "/a.txt").is_err());
                    assert!(fs.create_hard_link("/link", "\0").is_err());
                    assert!(fs.create_hard_link("/loop/link", "/a.txt").is_err());
                }
            }

            mod copy {
                use super::*;

                #[test]
                fn copy_file_to_trailing_separator_should_fail_without_creating() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();

                    assert_error(
                        &fs.copy("/a.txt", "/b.txt/").err().unwrap(),
                        fs_error!(
                            IsADirectory;
                            "File could not be opened, path is a directory: {}",
                            normalize_path(fs.current_dir(), "/b.txt/").display()
                        ),
                    );
                    assert!(!fs.exists("/b.txt").unwrap());
                }

                #[test]
                fn copy_should_keep_length_beyond_written_content() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "content").unwrap();
                    fs.set_len("/a.txt", MAX_FILE_LEN * 10).unwrap();
                    fs.write("/c.txt", "c").unwrap();

                    for target in ["/b.txt", "/c.txt"] {
                        fs.copy("/a.txt", target).unwrap();

                        assert_eq!(
                            fs.metadata(target).unwrap().len,
                            MAX_FILE_LEN * 10,
                            "target: {target}"
                        );
                    }
                }

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

                    assert_error(
                        &fs.copy("/dir/link", "/link").err().unwrap(),
                        fs_error!(NotFound; "Symlink could not be created, target not found: a.txt"),
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

                    assert_error(
                        &fs.copy("/a.txt", "/b.txt").err().unwrap(),
                        fs_error!(
                            ResourceBusy;
                            "File could not be opened, it is already borrowed for writing: {}",
                            normalize_path(fs.current_dir(), "/a.txt").display()
                        ),
                    );
                    assert!(!fs.exists("/b.txt").unwrap());
                }

                #[test]
                fn copy_file_should_overwrite_existing_file() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.write("/b.txt", "longer content").unwrap();

                    fs.copy("/a.txt", "/b.txt").unwrap();

                    assert_eq!(fs.read_to_string("/b.txt").unwrap(), "aaa");
                }

                #[cfg(unix)]
                #[test]
                fn copy_file_over_existing_file_should_copy_mode_and_keep_owner() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.set_mode("/a.txt", 0o640).unwrap();
                    fs.write("/b.txt", "bbb").unwrap();
                    fs.set_owner("/b.txt", 1, 2).unwrap();

                    fs.copy("/a.txt", "/b.txt").unwrap();

                    let metadata = fs.metadata("/b.txt").unwrap();
                    assert_eq!((metadata.mode, metadata.uid, metadata.gid), (0o640, 1, 2));
                }

                #[test]
                fn copy_file_to_symlink_should_write_to_its_target() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.write("/b.txt", "bbb").unwrap();
                    fs.create_symlink("/link", "/b.txt").unwrap();
                    insert_symlink(&fs, "/dangling", "c.txt");

                    fs.copy("/a.txt", "/link").unwrap();
                    fs.copy("/a.txt", "/dangling").unwrap();

                    assert_eq!(fs.read_to_string("/b.txt").unwrap(), "aaa");
                    assert_eq!(fs.read_to_string("/c.txt").unwrap(), "aaa");
                    assert!(fs.symlink_metadata("/link").unwrap().file_type.is_symlink());
                }

                #[test]
                fn copy_file_to_dir_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.create_dir("/dir").unwrap();

                    assert_error(
                        &fs.copy("/a.txt", "/dir").err().unwrap(),
                        fs_error!(
                            IsADirectory;
                            "File could not be opened, path is a directory: {}",
                            normalize_path(fs.current_dir(), "/dir").display()
                        ),
                    );
                }

                #[test]
                fn copy_file_to_other_name_of_itself_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.create_hard_link("/b.txt", "/a.txt").unwrap();

                    assert_error(
                        &fs.copy("/a.txt", "/b.txt").err().unwrap(),
                        fs_error!(
                            ResourceBusy;
                            "File could not be opened, it is already borrowed for reading 1 times: {}",
                            normalize_path(fs.current_dir(), "/b.txt").display()
                        ),
                    );
                    assert_eq!(fs.read_to_string("/a.txt").unwrap(), "aaa");
                }

                #[test]
                fn copy_file_to_borrowed_target_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.write("/b.txt", "bbb").unwrap();
                    let target = normalize_path(fs.current_dir(), "/b.txt");

                    {
                        let _read = fs.open_read("/b.txt").unwrap();
                        assert_error(
                            &fs.copy("/a.txt", "/b.txt").err().unwrap(),
                            fs_error!(
                                ResourceBusy;
                                "File could not be opened, it is already borrowed for reading 1 times: {}",
                                target.display()
                            ),
                        );
                    }

                    let _write = fs.open_write("/b.txt").unwrap();
                    assert_error(
                        &fs.copy("/a.txt", "/b.txt").err().unwrap(),
                        fs_error!(
                            ResourceBusy;
                            "File could not be opened, it is already borrowed for writing: {}",
                            target.display()
                        ),
                    );
                }

                #[test]
                fn copy_file_to_readonly_target_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.write("/b.txt", "bbb").unwrap();
                    fs.set_readonly("/b.txt", true).unwrap();

                    assert_error(
                        &fs.copy("/a.txt", "/b.txt").err().unwrap(),
                        fs_error!(
                            PermissionDenied;
                            "File could not be opened, it is read-only: {}",
                            normalize_path(fs.current_dir(), "/b.txt").display()
                        ),
                    );
                    assert_eq!(fs.read_to_string("/b.txt").unwrap(), "bbb");
                }

                #[test]
                fn copy_dir_or_symlink_to_existing_target_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.write("/b.txt", "bbb").unwrap();
                    fs.create_dir("/dir").unwrap();
                    fs.create_symlink("/link", "/a.txt").unwrap();

                    for source in ["/dir", "/link"] {
                        assert_eq!(
                            described(&fs.copy(source, "/b.txt").err().unwrap()),
                            described(&fs_error!(
                                AlreadyExists;
                                "Path could not be copied, target already exists: {}",
                                normalize_path(fs.current_dir(), "/b.txt").display()
                            )),
                            "source: {source}"
                        );
                    }

                    assert_eq!(fs.read_to_string("/b.txt").unwrap(), "bbb");
                }

                #[test]
                fn copy_without_target_parent_dir_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.create_dir("/dir").unwrap();

                    for (source, target) in [
                        ("/a.txt", "/missing/b.txt"),
                        ("/a.txt", "/a.txt/b.txt"),
                        ("/a.txt", "/missing/b.txt/"),
                        ("/dir", "/missing/b.txt"),
                    ] {
                        assert_eq!(
                            described(&fs.copy(source, target).err().unwrap()),
                            described(&fs_error!(
                                NotFound;
                                "Path could not be copied, parent directory does not exist: {}",
                                normalize_path(fs.current_dir(), target).display()
                            )),
                            "source: {source}, target: {target}"
                        );
                    }
                }

                #[test]
                fn copy_nonexistent_source_should_fail() {
                    let fs = TestFs::new();

                    assert_error(
                        &fs.copy("/missing.txt", "/b.txt").err().unwrap(),
                        fs_error!(
                            NotFound;
                            "Path could not be copied, source not found: {}",
                            normalize_path(fs.current_dir(), "/missing.txt").display()
                        ),
                    );
                }

                #[test]
                fn copy_non_empty_dir_should_fail() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir/sub").unwrap();

                    assert_error(
                        &fs.copy("/dir", "/copy").err().unwrap(),
                        fs_error!(
                            DirectoryNotEmpty;
                            "Directory could not be copied, it is not empty: {}",
                            normalize_path(fs.current_dir(), "/dir").display()
                        ),
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
                fn copy_file_to_symlink_loop_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    insert_symlink(&fs, "/loop", "loop");

                    assert!(fs.copy("/a.txt", "/loop").is_err());
                }

                #[test]
                fn copy_invalid_path_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();

                    insert_symlink(&fs, "/loop", "loop");

                    assert!(fs.copy("/a.txt", "\0").is_err());
                    assert!(fs.copy("\0", "/b.txt").is_err());
                    assert!(fs.copy("/a.txt", "/loop/b.txt").is_err());
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
                fn copy_dir_all_invalid_path_should_fail() {
                    let fs = TestFs::new();
                    fs.create_dir("/dir").unwrap();

                    assert!(fs.copy_dir_all("/dir", "\0").is_err());
                    assert!(fs.copy_dir_all("\0", "/copy").is_err());
                }

                #[test]
                fn copy_dir_all_should_check_parent_of_normalized_target() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/dir/a.txt", "aaa").unwrap();

                    fs.copy_dir_all("/dir", "/copy/missing/..").unwrap();

                    assert_eq!(fs.read_to_string("/copy/a.txt").unwrap(), "aaa");
                }

                #[test]
                fn copy_dir_all_file_source_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();

                    assert_error(
                        &fs.copy_dir_all("/a.txt", "/copy").err().unwrap(),
                        fs_error!(
                            NotADirectory;
                            "Directory could not be copied, source is not a directory: {}",
                            normalize_path(fs.current_dir(), "/a.txt").display()
                        ),
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
                            described(&fs.copy_dir_all("/dir", target).err().unwrap()),
                            described(&fs_error!(
                                AlreadyExists;
                                "Directory could not be copied, target already exists: {}",
                                normalize_path(fs.current_dir(), target).display()
                            )),
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
                            described(&fs.copy_dir_all("/dir", target).err().unwrap()),
                            described(&fs_error!(
                                NotFound;
                                "Directory could not be copied, parent directory does not exist: {}", normalize_path(fs.current_dir(), target).display()
                            )),
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

                    assert_error(
                        &fs.copy_dir_all("/dir", "/other/copy").err().unwrap(),
                        fs_error!(
                            NotFound;
                            "Symlink could not be created, target not found: {}",
                            native_symlink_target(Path::new("../a.txt")).display()
                        ),
                    );
                    assert!(fs.exists("/other/copy").unwrap());
                }

                #[test]
                fn copy_dir_all_write_borrowed_file_should_fail() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();
                    fs.write("/dir/a.txt", "aaa").unwrap();
                    let _write = fs.open_write("/dir/a.txt").unwrap();

                    assert_error(
                        &fs.copy_dir_all("/dir", "/copy").err().unwrap(),
                        fs_error!(
                            ResourceBusy;
                            "File could not be opened, it is already borrowed for writing: {}",
                            normalize_path(fs.current_dir(), "/dir/a.txt").display()
                        ),
                    );
                }
            }

            mod remove {
                use super::*;

                #[test]
                fn remove_with_trailing_separator_should_keep_symlink_to_dir() {
                    let fs = TestFs::new();
                    fs.create_dir("/dir").unwrap();
                    fs.create_symlink("/link", "dir").unwrap();

                    assert_error(
                        &fs.remove("/link/").err().unwrap(),
                        fs_error!(
                            NotADirectory;
                            "Path could not be removed, path is a symlink: {}",
                            normalize_path(fs.current_dir(), "/link/").display()
                        ),
                    );
                    assert!(fs.exists("/dir").unwrap());
                    assert_eq!(
                        fs.symlink_metadata("/link").unwrap().file_type,
                        FileType::Symlink
                    );
                }

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
                    let path = normalize_path(fs.current_dir(), "/b.txt");

                    let read = fs.open_read("/a.txt").unwrap();
                    assert_error(
                        &fs.remove("/b.txt").err().unwrap(),
                        fs_error!(
                            ResourceBusy;
                            "File could not be removed, it is already borrowed for reading 1 times: {}",
                            path.display()
                        ),
                    );
                    drop(read);

                    let _write = fs.open_write("/a.txt").unwrap();
                    assert_error(
                        &fs.remove("/b.txt").err().unwrap(),
                        fs_error!(
                            ResourceBusy;
                            "File could not be removed, it is already borrowed for writing: {}",
                            path.display()
                        ),
                    );
                    assert!(fs.exists("/b.txt").unwrap());
                }

                #[test]
                fn remove_non_empty_dir_should_fail() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir/sub").unwrap();

                    assert_error(
                        &fs.remove("/dir").err().unwrap(),
                        fs_error!(
                            DirectoryNotEmpty;
                            "Directory could not be removed, it is not empty: {}",
                            normalize_path(fs.current_dir(), "/dir").display()
                        ),
                    );
                }

                #[test]
                fn remove_root_should_fail() {
                    let fs = TestFs::new();

                    assert_error(
                        &fs.remove(ROOT).err().unwrap(),
                        fs_error!(
                            ResourceBusy;
                            "Directory could not be removed, path is a root: {}",
                            normalize_path(fs.current_dir(), ROOT).display()
                        ),
                    );
                    assert!(fs.exists(ROOT).unwrap());
                }

                #[test]
                fn remove_should_fail_for_nonexistent_path() {
                    let fs = TestFs::new();
                    assert_error(
                        &fs.remove("/a.txt").err().unwrap(),
                        fs_error!(
                            NotFound;
                            "Path could not be removed, path not found: {}",
                            normalize_path(fs.current_dir(), "/a.txt").display()
                        ),
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
                fn remove_dir_all_with_trailing_separator_should_keep_symlink_and_target() {
                    let fs = TestFs::new();
                    fs.create_dir("/dir").unwrap();
                    fs.write("/dir/a.txt", "aaa").unwrap();
                    fs.create_symlink("/link", "dir").unwrap();

                    assert_error(
                        &fs.remove_dir_all("/link/").err().unwrap(),
                        fs_error!(
                            NotADirectory;
                            "Path could not be removed, path is a symlink: {}",
                            normalize_path(fs.current_dir(), "/link/").display()
                        ),
                    );
                    assert!(fs.exists("/dir/a.txt").unwrap());
                    assert!(fs.exists("/link").unwrap());
                }

                #[test]
                fn remove_dir_all_empty_path_should_fail_and_keep_tree() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    fs.create_dir_all("").unwrap();

                    assert!(fs.remove_dir_all("").is_err());
                    assert_eq!(fs.read_to_string("/a.txt").unwrap(), "aaa");
                }

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

                    assert_error(
                        &fs.remove_dir_all("/a.txt").err().unwrap(),
                        fs_error!(
                            NotADirectory;
                            "Directory could not be removed, path is not a directory: {}",
                            normalize_path(fs.current_dir(), "/a.txt").display()
                        ),
                    );
                    assert!(fs.exists("/a.txt").unwrap());
                }

                #[test]
                fn remove_dir_all_root_should_fail_before_removing_anything() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir").unwrap();

                    for path in [ROOT, "/dir/.."] {
                        assert_eq!(
                            described(&fs.remove_dir_all(path).err().unwrap()),
                            described(&fs_error!(
                                ResourceBusy;
                                "Directory could not be removed, path is a root: {}",
                                normalize_path(fs.current_dir(), ROOT).display()
                            )),
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

                    assert_error(
                        &fs.remove_dir_all("/dir").err().unwrap(),
                        fs_error!(
                            ResourceBusy;
                            "File could not be removed, it is already borrowed for reading 1 times: {}",
                            normalize_path(fs.current_dir(), "/dir/a.txt").display()
                        ),
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

                    assert_error(
                        &fs.remove_dir_all("/dir").err().unwrap(),
                        fs_error!(
                            ResourceBusy;
                            "File could not be removed, it is already borrowed for writing: {}",
                            normalize_path(fs.current_dir(), "/dir/sub/link").display()
                        ),
                    );
                    assert!(fs.exists("/dir/a.txt").unwrap());
                }

                #[test]
                fn remove_dir_all_should_fail_for_nonexistent_path() {
                    let fs = TestFs::new();
                    assert_error(
                        &fs.remove_dir_all("/dir").err().unwrap(),
                        fs_error!(
                            NotFound;
                            "Directory could not be removed, path not found: {}",
                            normalize_path(fs.current_dir(), "/dir").display()
                        ),
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
                fn rename_source_with_trailing_separator_should_require_dir() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();

                    assert_error(
                        &fs.rename("/a.txt/", "/b.txt").err().unwrap(),
                        fs_error!(
                            NotADirectory;
                            "Path could not be renamed, source is not a directory: {}",
                            normalize_path(fs.current_dir(), "/a.txt/").display()
                        ),
                    );
                    assert!(fs.exists("/a.txt").unwrap());
                }

                #[test]
                fn rename_target_with_trailing_separator_should_require_dir() {
                    fn check(
                        source: &str,
                        target: &str,
                        expected: Option<(std::io::ErrorKind, &str)>,
                    ) {
                        let fs = TestFs::new();
                        fs.write("/a.txt", "aaa").unwrap();
                        fs.create_dir("/dir").unwrap();
                        fs.create_dir("/empty").unwrap();
                        fs.create_symlink("/link", "empty").unwrap();

                        let result = fs.rename(source, target);

                        assert_eq!(
                            result.as_ref().err().map(described),
                            expected.map(|(kind, message)| described(&fs_error!(
                                kind =>
                                "{message}: {}",
                                normalize_path(fs.current_dir(), target).display()
                            ))),
                            "source: {source}, target: {target}"
                        );
                    }

                    let not_dir = "Path could not be renamed, target is not a directory";
                    let dir_not_dir = "Directory could not be renamed, target is not a directory";
                    check(
                        "/a.txt",
                        "/a.txt/",
                        Some((std::io::ErrorKind::NotADirectory, not_dir)),
                    );
                    check(
                        "/a.txt",
                        "/b.txt/",
                        Some((std::io::ErrorKind::NotADirectory, not_dir)),
                    );
                    check(
                        "/a.txt",
                        "/link/",
                        Some((
                            std::io::ErrorKind::IsADirectory,
                            "Path could not be renamed, target is a directory",
                        )),
                    );
                    check(
                        "/dir",
                        "/a.txt/",
                        Some((std::io::ErrorKind::NotADirectory, dir_not_dir)),
                    );
                    check("/dir", "/new/", None);
                    check("/dir", "/empty/", None);
                }

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
                                normalize_path(fs.current_dir(), target).display()
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
                    let path = normalize_path(fs.current_dir(), "/b.txt");

                    let read = fs.open_read("/b.txt").unwrap();
                    assert_error(
                        &fs.rename("/a.txt", "/b.txt").err().unwrap(),
                        fs_error!(
                            ResourceBusy;
                            "File could not be replaced, it is already borrowed for reading 1 times: {}",
                            path.display()
                        ),
                    );
                    drop(read);

                    let _write = fs.open_write("/b.txt").unwrap();
                    assert_error(
                        &fs.rename("/a.txt", "/b.txt").err().unwrap(),
                        fs_error!(
                            ResourceBusy;
                            "File could not be replaced, it is already borrowed for writing: {}",
                            path.display()
                        ),
                    );
                    assert!(fs.exists("/a.txt").unwrap());
                }

                #[test]
                fn rename_without_target_parent_dir_should_fail() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();

                    for (target, kind) in [
                        ("/missing/b.txt", std::io::ErrorKind::NotFound),
                        ("/a.txt/b.txt", std::io::ErrorKind::NotADirectory),
                    ] {
                        assert_eq!(
                            described(&fs.rename("/a.txt", target).err().unwrap()),
                            described(&fs_error!(
                                kind =>
                                "Path could not be renamed, parent directory does not exist: {}",
                                normalize_path(fs.current_dir(), target).display()
                            )),
                            "target: {target}"
                        );
                    }
                }

                #[test]
                fn rename_dir_into_itself_should_fail() {
                    let fs = TestFs::new();
                    fs.create_dir_all("/dir/sub").unwrap();

                    assert_error(
                        &fs.rename("/dir", "/dir/sub/moved").err().unwrap(),
                        fs_error!(
                            InvalidInput;
                            "Directory could not be renamed, target is inside source: {}",
                            normalize_path(fs.current_dir(), "/dir/sub/moved").display()
                        ),
                    );
                    assert!(fs.exists("/dir/sub").unwrap());
                }

                #[test]
                fn rename_nonexistent_source_should_fail() {
                    let fs = TestFs::new();
                    assert_error(
                        &fs.rename("/a.txt", "/b.txt").err().unwrap(),
                        fs_error!(
                            NotFound;
                            "Path could not be renamed, source not found: {}",
                            normalize_path(fs.current_dir(), "/a.txt").display()
                        ),
                    );
                }

                #[test]
                fn rename_root_should_fail() {
                    let fs = TestFs::new();
                    assert_error(
                        &fs.rename(ROOT, "/moved").err().unwrap(),
                        fs_error!(
                            ResourceBusy;
                            "Path could not be renamed, source is a root: {}",
                            normalize_path(fs.current_dir(), ROOT).display()
                        ),
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
                    assert!(fs.rename("/a.txt", "/loop/b.txt").is_err());
                    assert!(fs.rename("/a.txt", "/loop/").is_err());
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

                    assert_error(
                        &fs.set_executable("/dir", true).err().unwrap(),
                        fs_error!(
                            IsADirectory;
                            "Executable attribute cannot be set, path is a directory: {}",
                            normalize_path(fs.current_dir(), "/dir").display()
                        ),
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
                    assert_error(
                        &fs.set_modified("/a.txt", SystemTime::UNIX_EPOCH)
                            .err()
                            .unwrap(),
                        fs_error!(
                            NotFound;
                            "Modified time could not be set, path not found: {}",
                            normalize_path(fs.current_dir(), "/a.txt").display()
                        ),
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
                    assert_error(
                        &fs.set_mode("/a.txt", 0o600).err().unwrap(),
                        fs_error!(
                            NotFound;
                            "Mode could not be set, path not found: {}",
                            normalize_path(fs.current_dir(), "/a.txt").display()
                        ),
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
                assert_error(
                    &fs.set_attribute(Path::new("/a.txt"), Attribute::Readonly, true)
                        .err()
                        .unwrap(),
                    fs_error!(
                        NotFound;
                        "Attributes could not be set, path not found: {}",
                        normalize_path(fs.current_dir(), "/a.txt").display()
                    ),
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

    mod test_file_read {
        use super::*;

        mod read_file {
            use super::*;

            mod metadata {
                use super::*;

                #[test]
                fn metadata_should_return_metadata_of_open_file() {
                    let fs = TestFs::new();
                    fs.write("/.a.txt", "content").unwrap();

                    let metadata = fs.open_read("/.a.txt").unwrap().metadata().unwrap();

                    assert_eq!(metadata, fs.metadata("/.a.txt").unwrap());
                    assert_eq!(metadata.len, 7);
                }
            }

            mod lock_shared {
                use super::*;

                #[test]
                fn lock_shared_should_succeed_for_every_read_handle() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    let file = fs.open_read("/a.txt").unwrap();
                    let other = fs.open_read("/a.txt").unwrap();

                    file.lock_shared().unwrap();
                    other.lock_shared().unwrap();
                }
            }

            mod try_lock_shared {
                use super::*;

                #[test]
                fn try_lock_shared_should_lock() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();

                    assert!(fs.open_read("/a.txt").unwrap().try_lock_shared().unwrap());
                }
            }

            mod unlock {
                use super::*;

                #[test]
                fn unlock_should_succeed() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "aaa").unwrap();
                    let file = fs.open_read("/a.txt").unwrap();
                    file.lock_shared().unwrap();

                    file.unlock().unwrap();
                }
            }
        }

        mod read {
            use super::*;

            mod read_to_end {
                use super::*;

                #[test]
                fn read_to_end_should_append_rest_of_sparse_file() {
                    // GIVEN a file extended with zeros and read in part
                    let fs = TestFs::new();
                    fs.write("/a.txt", "content").unwrap();
                    fs.set_len("/a.txt", 9).unwrap();
                    let mut file = fs.open_read("/a.txt").unwrap();
                    file.read_exact(&mut [0; 4]).unwrap();
                    let mut buf = b"x".to_vec();

                    // WHEN reading the rest of it
                    let read = file.read_to_end(&mut buf).unwrap();

                    // THEN the rest is appended including the zeros
                    assert_eq!(read, 5);
                    assert_eq!(buf, b"xent\0\0");
                    assert_eq!(file.read_to_end(&mut buf).unwrap(), 0);
                }

                #[test]
                fn read_to_end_too_large_file_should_fail_with_out_of_memory() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "").unwrap();
                    fs.set_len("/a.txt", i64::MAX as u64).unwrap();

                    let error = fs
                        .open_read("/a.txt")
                        .unwrap()
                        .read_to_end(&mut Vec::new())
                        .unwrap_err();

                    assert_eq!(error.kind(), std::io::ErrorKind::OutOfMemory);
                }

                #[test]
                fn read_to_end_should_fail_in_read_fail_mode() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "content").unwrap();
                    fs.set_fail_mode(Some(FailMode::Read));

                    assert!(
                        fs.open_read("/a.txt")
                            .unwrap()
                            .read_to_end(&mut Vec::new())
                            .is_err()
                    );
                }
            }
        }
    }

    mod test_file_write {
        use super::*;

        mod read {
            use super::*;

            mod read_to_end {
                use super::*;

                #[test]
                fn read_to_end_should_read_from_position() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "content").unwrap();
                    let mut file = fs.open_write("/a.txt").unwrap();
                    file.seek(SeekFrom::Start(3)).unwrap();
                    let mut buf = Vec::new();

                    file.read_to_end(&mut buf).unwrap();

                    assert_eq!(buf, b"tent");
                }

                #[test]
                fn read_to_end_write_only_should_fail() {
                    let fs = TestFs::new();

                    let error = fs
                        .open_write_options("/a.txt", OpenWriteOptions::new().create().write_only())
                        .unwrap()
                        .read_to_end(&mut Vec::new())
                        .unwrap_err();

                    assert_eq!(error.to_string(), "file is opened for writing only");
                }
            }
        }

        mod read_file {
            use super::*;

            mod metadata {
                use super::*;

                #[test]
                fn metadata_should_return_metadata_of_open_file() {
                    let fs = TestFs::new();
                    let mut file = fs.open_write("/a.txt").unwrap();
                    file.write_all(b"content").unwrap();

                    let metadata = file.metadata().unwrap();

                    assert_eq!(metadata.len, 7);
                    assert!(metadata.file_type.is_file());
                }
            }

            mod lock_shared {
                use super::*;

                #[test]
                fn lock_shared_should_succeed() {
                    let fs = TestFs::new();

                    fs.open_write("/a.txt").unwrap().lock_shared().unwrap();
                }
            }

            mod try_lock_shared {
                use super::*;

                #[test]
                fn try_lock_shared_should_lock() {
                    let fs = TestFs::new();

                    assert!(fs.open_write("/a.txt").unwrap().try_lock_shared().unwrap());
                }
            }

            mod unlock {
                use super::*;

                #[test]
                fn unlock_should_succeed() {
                    let fs = TestFs::new();
                    let file = fs.open_write("/a.txt").unwrap();
                    file.lock().unwrap();

                    file.unlock().unwrap();
                }
            }
        }

        mod write_file {
            use super::*;

            mod lock {
                use super::*;

                #[test]
                fn lock_should_succeed() {
                    let fs = TestFs::new();

                    fs.open_write("/a.txt").unwrap().lock().unwrap();
                }
            }

            mod try_lock {
                use super::*;

                #[test]
                fn try_lock_should_lock() {
                    let fs = TestFs::new();

                    assert!(fs.open_write("/a.txt").unwrap().try_lock().unwrap());
                }
            }

            mod set_len {
                use super::*;

                #[test]
                fn set_len_invalid_length_should_fail_and_keep_content() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "content").unwrap();
                    let file = fs
                        .open_write_options("/a.txt", OpenWriteOptions::new())
                        .unwrap();
                    let path = normalize_path(fs.current_dir(), "/a.txt");

                    assert_error(
                        &file.set_len(u64::MAX).err().unwrap(),
                        fs_error!(
                            InvalidInput;
                            "File length could not be set, length is invalid: {}",
                            path.display()
                        ),
                    );
                    drop(file);
                    assert_eq!(fs.read("/a.txt").unwrap(), b"content");
                }

                #[test]
                fn set_len_beyond_max_file_len_should_extend_and_allow_writing_written_part() {
                    // GIVEN a file opened for writing
                    let fs = TestFs::new();
                    let mut file = fs.open_write("/a.txt").unwrap();

                    // WHEN making it larger than can be written and writing
                    // at its start
                    file.set_len(MAX_FILE_LEN + 1).unwrap();
                    file.write_all(b"ab").unwrap();

                    // THEN it keeps the length set with the written bytes
                    // held in memory only
                    assert_eq!(file.metadata().unwrap().len, MAX_FILE_LEN + 1);
                    assert_eq!(lock_file_read(&file.handle.0.file).data, b"ab");
                }

                #[test]
                fn set_len_should_truncate_or_extend_with_zeros_keeping_cursor() {
                    let fs = TestFs::new();
                    fs.write("/a.txt", "content").unwrap();
                    let mut file = fs.open_write("/a.txt").unwrap();
                    file.seek(SeekFrom::Start(3)).unwrap();

                    file.set_len(2).unwrap();
                    file.set_len(4).unwrap();

                    assert_eq!(file.stream_position().unwrap(), 3);
                    drop(file);
                    assert_eq!(fs.read("/a.txt").unwrap(), b"co\0\0");
                }
            }

            mod sync_all {
                use super::*;

                #[test]
                fn sync_all_should_succeed() {
                    let fs = TestFs::new();
                    let file = fs.open_write("/a.txt").unwrap();

                    file.sync_all().unwrap();
                }
            }

            mod sync_data {
                use super::*;

                #[test]
                fn sync_data_should_succeed() {
                    let fs = TestFs::new();
                    let file = fs.open_write("/a.txt").unwrap();

                    file.sync_data().unwrap();
                }
            }
        }

        mod write {
            use super::*;

            #[allow(clippy::module_inception)]
            mod write {
                use super::*;

                #[test]
                fn write_beyond_max_file_len_should_fail() {
                    let fs = TestFs::new();
                    let mut file = fs.open_write("/a.txt").unwrap();
                    file.seek(SeekFrom::Start(MAX_FILE_LEN)).unwrap();

                    assert_eq!(
                        file.write(b"a").err().unwrap().kind(),
                        std::io::ErrorKind::FileTooLarge
                    );
                    drop(file);
                    assert!(fs.read("/a.txt").unwrap().is_empty());
                }

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

    mod name_hash {
        use super::*;

        #[test]
        fn name_hash_should_be_fnv1a() {
            assert_eq!(name_hash(OsStr::new("")), 0xcbf2_9ce4_8422_2325);
            assert_eq!(name_hash(OsStr::new("a")), 0xaf63_dc4c_8601_ec8c);
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
            let path = normalize_path(fs.current_dir(), "/dir_link/file_link");

            assert_eq!(
                resolve_symlinks(&lock_handles(&fs.fs), &path, true).unwrap(),
                normalize_path(fs.current_dir(), "/a/b/c.txt")
            );
        }

        #[test]
        fn resolve_symlinks_without_follow_last_should_keep_last_symlink() {
            let fs = TestFs::new();
            fs.create_dir_all("/a/b").unwrap();
            fs.create_symlink("/dir_link", "a").unwrap();
            fs.create_symlink("/a/link", "b").unwrap();
            let path = normalize_path(fs.current_dir(), "/dir_link/link");

            assert_eq!(
                resolve_symlinks(&lock_handles(&fs.fs), &path, false).unwrap(),
                normalize_path(fs.current_dir(), "/a/link")
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
            let path = normalize_path(fs.current_dir(), "/up");

            // WHEN
            let resolved = resolve_symlinks(&lock_handles(&fs.fs), &path, true).unwrap();

            // THEN
            assert_eq!(resolved, normalize_path(fs.current_dir(), "/a"));
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
            let within = normalize_path(fs.current_dir(), &format!("/link{}", MAX_SYMLINKS - 1))
                .into_owned();
            let beyond =
                normalize_path(fs.current_dir(), &format!("/link{MAX_SYMLINKS}")).into_owned();
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
                file: Arc::new(RwLock::new(file)),
                fail_mode: None,
                path: PathBuf::from(ROOT),
            }
        }

        mod seek {
            use super::*;

            #[test]
            fn seek_overflowing_position_should_fail_with_invalid_input() {
                let mut handle = new_handle(BorrowState::Read(1));
                lock_file_write(&handle.file).set_len(1);
                handle.seek(SeekFrom::Start(1)).unwrap();

                for pos in [
                    SeekFrom::Start(u64::MAX),
                    SeekFrom::End(i64::MAX),
                    SeekFrom::Current(i64::MAX),
                ] {
                    assert_eq!(
                        handle.seek(pos).err().unwrap().kind(),
                        std::io::ErrorKind::InvalidInput,
                        "pos: {pos:?}"
                    );
                }

                assert_eq!(handle.pos, 1);
            }

            #[test]
            fn seek_should_fail_for_negative_offset_from_end() {
                let mut handle = new_handle(BorrowState::Read(1));
                assert_eq!(
                    handle.seek(SeekFrom::End(-10)).err().unwrap().kind(),
                    std::io::ErrorKind::InvalidInput
                );
            }

            #[test]
            fn seek_should_fail_for_negative_offset_from_current() {
                let mut handle = new_handle(BorrowState::Read(1));
                assert_eq!(
                    handle.seek(SeekFrom::Current(-10)).err().unwrap().kind(),
                    std::io::ErrorKind::InvalidInput
                );
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
