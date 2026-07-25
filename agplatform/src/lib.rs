/// The trait represents an abstract interface to a platform
/// implementations providing env, fs, exec and request
/// functionality. For production implementation use the
/// `platform()` function to get an opaque default implementation.
/// For testing use `test_platform()` instead that provides
/// memory-only but fully functional mock implementations.
pub trait Platform {}

struct PlatformImpl;

impl Platform for PlatformImpl {}

/// Retruns an opaque default platform implementation
/// that is internally just a wrapper around the
/// std / tokio implementations.
pub fn platform() -> impl Platform {
    PlatformImpl
}
