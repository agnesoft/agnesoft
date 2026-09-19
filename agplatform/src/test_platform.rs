pub mod test_env;
pub mod test_fs;

use crate::Env;
use crate::Fs;
use crate::Platform;
use crate::TestEnv;
use crate::TestFs;
use crate::platform::platform_env::PlatformEnv;

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
    type E = TestEnv;
    type F = TestFs;

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
    fn env_mut(&mut self) -> PlatformEnv<'_, Self::E, Self::F> {
        PlatformEnv {
            env: &mut self.env,
            fs: &mut self.fs,
        }
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
    fn test_env() {
        let test_env = TestEnv::new()
            .with_args(vec!["arg1", "arg2"])
            .with_current_dir("/home")
            .with_current_exe("/tmp/project/app")
            .with_home_dir("/home/user")
            .with_tmp_dir("/tmp")
            .with_vars(vec![("KEY1", "value1"), ("KEY2", "value2")]);
        let mut p = test_platform().with_env(test_env);
        let mut e = p.env_mut();

        assert_eq!(e.args().collect::<Vec<_>>(), &["arg1", "arg2"]);
        assert_eq!(e.current_dir(), std::path::Path::new("/home"));
        assert_eq!(e.set_current_dir("/tmp"), std::path::Path::new("/home"));
        assert_eq!(e.current_dir(), std::path::Path::new("/tmp"));
        assert_eq!(e.current_exe(), std::path::Path::new("/tmp/project/app"));
        assert_eq!(e.home_dir(), std::path::Path::new("/home/user"));
        assert_eq!(e.tmp_dir(), std::path::Path::new("/tmp"));
        assert_eq!(e.var("KEY1"), Some("value1"));
        assert_eq!(e.var("KEY2"), Some("value2"));
        assert_eq!(e.var("NON_EXISTENT_KEY"), None);
        assert_eq!(e.set_var("NON_EXISTENT_KEY", "value"), None);
        assert_eq!(e.remove_var("NON_EXISTENT_KEY"), Some("value".to_string()));
        assert_eq!(e.set_var("KEY1", "new_value"), Some("value1".to_string()));
        assert_eq!(
            e.vars().collect::<Vec<_>>(),
            vec![
                &("KEY1".to_string(), "new_value".to_string()),
                &("KEY2".to_string(), "value2".to_string())
            ]
        );
    }

    #[test]
    fn test_fs() {
        let test_fs = TestFs::new();
        let mut test_platform = test_platform().with_fs(test_fs);
        test_platform.env_mut().set_current_dir("/tmp");
        test_platform
            .fs
            .set("/tmp/file.txt", vec![b'h', b'e', b'l', b'l', b'o']);

        assert_eq!(
            test_platform.fs().read("/tmp/file.txt").unwrap(),
            vec![b'h', b'e', b'l', b'l', b'o'],
        );
        assert_eq!(
            test_platform.fs().read_to_string("/tmp/file.txt").unwrap(),
            "hello",
        );
        test_platform.fs.remove("/tmp/file.txt");
        assert!(
            test_platform
                .fs()
                .read("/tmp/file.txt")
                .unwrap_err()
                .description()
                .starts_with("File not found: "),
            "Expected 'File not found' error"
        );
    }
}
