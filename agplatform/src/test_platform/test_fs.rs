use crate::Fs;

/// The test file system is a memory-only but fully functional mock
/// implementation of a file system interface. It is used in tests to be
/// passed where `impl Fs` is expected.
pub struct TestFs;

impl TestFs {
    /// Creates a new instance of the test file system.
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::test_platform::test_fs::TestFs;
    ///
    /// let test_fs = TestFs::new();
    /// ```
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        TestFs
    }
}

impl Fs for TestFs {}
