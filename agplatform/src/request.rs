/// Abstract network request interface.
///
/// Sends network requests and receives their responses.
///
/// The two provided implementations are `RequestImpl` and
/// [`TestRequest`](crate::test_platform::test_request::TestRequest)
/// (purely in-memory, behind the `testing` feature).
pub trait Request {}

/// The production [`Request`] implementation.
///
/// Normally obtained via [`platform()`](crate::platform) rather than
/// constructed directly.
pub struct RequestImpl;

impl Request for RequestImpl {}
