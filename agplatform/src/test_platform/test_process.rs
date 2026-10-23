use std::path::PathBuf;

use crate::Process;
use crate::platform::platform_env::CurrentDirSetter;
use crate::test_platform::test_fs::ROOT;

/// Enabled by the `testing` feature flag.
///
/// A memory-only [`crate::Process`] implementation for tests. No real
/// process is ever spawned.
///
/// # Examples
///
/// ```rust
/// use agplatform::TestProcess;
///
/// let process = TestProcess::new();
/// ```
pub struct TestProcess {
    pub(crate) current_dir: PathBuf,
}

impl TestProcess {
    /// Creates a new test process interface whose current directory is
    /// the [`ROOT`] of [`TestFs`](crate::TestFs).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::TestProcess;
    ///
    /// let process = TestProcess::new();
    /// ```
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            current_dir: PathBuf::from(ROOT),
        }
    }
}

impl CurrentDirSetter for TestProcess {
    fn set_current_dir(&mut self, path: PathBuf) {
        self.current_dir = path;
    }
}

impl Process for TestProcess {}
