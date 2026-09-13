pub mod test_env;
pub mod test_fs;

use crate::Env;
use crate::Fs;
use crate::Platform;
use crate::TestEnv;
use crate::TestFs;

/// Enabled by the `testing` feature flag.
///
/// The test platform is a memory-only but fully functional mock
/// implementation of the [`Platform`] trait. It is used in tests to be
/// passed where `impl Platform` is expected.
///
/// Example:
///
/// ```rust
/// use agplatform::Platform;
/// use agplatform::test_platform::test_platform;
///
/// fn do_something(platform: &impl Platform) {}
///
/// let test_platform = test_platform();
/// do_something(&test_platform);
/// ```
pub struct TestPlatform {
    pub env: TestEnv,
    pub fs: TestFs,
}

impl TestPlatform {
    /// Creates a test platform with the specified test environment.
    ///
    /// See [`Self::with_env`] and [`crate::TestEnv`].
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::{Platform, Env};
    /// use agplatform::test_platform::test_platform;
    ///
    /// let test_env = agplatform::TestEnv::new().with_args(vec!["arg1", "arg2"]);
    /// let test_platform = test_platform().with_env(test_env);
    /// assert_eq!(test_platform.env().args().collect::<Vec<_>>(), &["arg1", "arg2"]);
    /// ```
    pub fn with_env(mut self, env: TestEnv) -> Self {
        self.env = env;
        self
    }

    /// Creates a test platform with the specified test file system.
    ///
    /// See [`Self::with_fs`] and [`crate::TestFs`].
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::{Platform, Fs};
    /// use agplatform::test_platform::test_platform;
    ///
    /// let test_fs = agplatform::TestFs::new();
    /// let test_platform = test_platform().with_fs(test_fs);
    /// ```
    pub fn with_fs(mut self, fs: TestFs) -> Self {
        self.fs = fs;
        self
    }
}

impl Platform for TestPlatform {
    /// Returns a reference to the test environment.
    ///
    /// See [`crate::Platform::env`].
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::{Env, Platform};
    /// use agplatform::test_platform::test_platform;
    ///
    /// let platform = test_platform();
    /// let env = platform.env();
    /// ```
    fn env(&self) -> &impl Env {
        &self.env
    }

    /// Returns a mutable reference to the test environment.
    ///
    /// See [`crate::Platform::env_mut`].
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::{Env, Platform};
    /// use agplatform::test_platform::test_platform;
    ///
    /// let mut platform = test_platform();
    /// let env_mut = platform.env_mut();
    /// ```
    fn env_mut(&mut self) -> &mut impl Env {
        &mut self.env
    }

    /// Returns a reference to the test file system.
    ///
    /// See [`crate::Platform::fs`].
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform};
    /// use agplatform::test_platform::test_platform;
    ///
    /// let platform = test_platform();
    /// let fs = platform.fs();
    /// ```
    fn fs(&self) -> &impl Fs {
        &self.fs
    }

    /// Returns a mutable reference to the test file system.
    ///
    /// See [`crate::Platform::fs_mut`].
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform};
    /// use agplatform::test_platform::test_platform;
    ///
    /// let mut platform = test_platform();
    /// let fs_mut = platform.fs_mut();
    /// ```
    fn fs_mut(&mut self) -> &mut impl Fs {
        &mut self.fs
    }
}

/// Returns an instance of [`TestPlatform`] that implements the
/// [`Platform`] trait via the mock implementations.
///
/// Example:
///
/// ```rust
/// use agplatform::Platform;
/// use agplatform::test_platform::test_platform;
///
/// fn do_something(platform: &impl Platform) {}
///
/// let test_platform = test_platform();
/// do_something(&test_platform);
/// ```
pub fn test_platform() -> TestPlatform {
    TestPlatform {
        env: TestEnv::new(),
        fs: TestFs::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_with_env() {
        let test_env = TestEnv::new();
        let current_dir = test_env.current_dir().to_path_buf();
        let mut test_platform = test_platform().with_env(test_env);
        assert_eq!(test_platform.env().current_dir(), current_dir);
        test_platform.env_mut().set_current_dir("/tmp");
        assert_eq!(
            test_platform.env().current_dir(),
            std::path::Path::new("/tmp")
        );
    }

    #[test]
    fn test_platform_with_fs() {
        let test_fs = TestFs::new();
        let mut test_platform = test_platform().with_fs(test_fs);
        let _ = test_platform.fs();
        let _ = test_platform.fs_mut();
    }
}
