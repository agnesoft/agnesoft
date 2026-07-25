use crate::Platform;

/// Gated by the `testing` feature flag.
///
/// The test platform is a memory-only but fully functional
/// mock implementation of the `Platform` trait. It is used
/// in tests to be passed where `impl Platform` is expected.
///
/// Example:
///
/// ```rust
/// use agplatform::Platform;
/// use agplatform::test_platform::TestPlatform;
///
/// fn do_something(platform: &impl Platform) {}
///
/// let test_platform = TestPlatform;
/// do_something(&test_platform);
/// ```
pub struct TestPlatform;

impl Platform for TestPlatform {}
