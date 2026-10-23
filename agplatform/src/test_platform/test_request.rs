use crate::Request;

/// Enabled by the `testing` feature flag.
///
/// A memory-only [`crate::Request`] implementation for tests. No network
/// is ever accessed.
///
/// # Examples
///
/// ```rust
/// use agplatform::TestRequest;
///
/// let request = TestRequest::new();
/// ```
pub struct TestRequest;

impl TestRequest {
    /// Creates a new test request interface.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::TestRequest;
    ///
    /// let request = TestRequest::new();
    /// ```
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self
    }
}

impl Request for TestRequest {}
