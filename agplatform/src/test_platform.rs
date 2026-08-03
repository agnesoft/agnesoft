mod test_env;

use crate::Env;
use crate::Platform;
use crate::test_platform::test_env::TestEnv;

/// Enabled by the `testing` feature flag.
///
/// The test platform is a memory-only but fully functional
/// mock implementation of the `Platform` trait. It is used
/// in tests to be passed where `impl Platform` is expected.
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

impl Platform for TestPlatform {
    fn env(&self) -> &impl Env {
        &self.env
    }

    fn env_mut(&mut self) -> &mut impl Env {
        &mut self.env
    }
}

/// Returns an instance of the `TestPlatform` struct
/// that implements the `Platform` trait via the mock
/// implementations.
pub fn test_platform() -> TestPlatform {
    TestPlatform {
        env: TestEnv::new(),
    }
}
