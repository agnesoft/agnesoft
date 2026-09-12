pub mod test_env;

use crate::Env;
use crate::Platform;
use crate::TestEnv;

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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_with_env() {
        let test_env = TestEnv::new()
            .with_args(vec!["arg1", "arg2"])
            .with_current_dir("/home")
            .with_vars(vec![("KEY1", "value1"), ("KEY2", "value2")]);
        let mut test_platform = test_platform().with_env(test_env);
        assert_eq!(
            test_platform.env().args().collect::<Vec<_>>(),
            &["arg1", "arg2"]
        );
        assert_eq!(
            test_platform.env().current_dir(),
            std::path::Path::new("/home")
        );
        assert_eq!(
            test_platform.env_mut().set_current_dir("/tmp"),
            std::path::Path::new("/home")
        );
        assert_eq!(test_platform.env().var("KEY1"), Some("value1"));
        assert_eq!(test_platform.env().var("KEY2"), Some("value2"));
        assert_eq!(test_platform.env().var("NON_EXISTENT_KEY"), None);
        assert_eq!(
            test_platform.env_mut().set_var("NON_EXISTENT_KEY", "value"),
            None
        );
        assert_eq!(
            test_platform.env_mut().remove_var("NON_EXISTENT_KEY"),
            Some("value".to_string())
        );
        assert_eq!(
            test_platform.env_mut().set_var("KEY1", "new_value"),
            Some("value1".to_string())
        );
        assert_eq!(
            test_platform.env().vars().collect::<Vec<_>>(),
            vec![
                &("KEY1".to_string(), "new_value".to_string()),
                &("KEY2".to_string(), "value2".to_string())
            ]
        );
    }
}
