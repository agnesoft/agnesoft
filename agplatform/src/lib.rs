mod env;
mod error;
#[cfg(feature = "testing")]
pub mod test_platform;

pub use env::Env;
pub use error::Error;
pub use error::ErrorKind;

pub type Result<T = ()> = std::result::Result<T, Error>;

/// The trait represents a zero-cost abstract interface to
/// a platform implementing env, fs, exec and request
/// functionality. For production implementation use the
/// `platform()` function to get an opaque default implementation.
/// For testing use `test_platform::test_platform()` instead that provides
/// memory-only but fully functional mock implementations
/// (enabled via the feature `testing`).
///
/// Example:
///
/// ```rust
/// use agplatform::Platform;
///
/// fn do_something(platform: &impl Platform) {}
///
/// let platform = agplatform::platform();
/// do_something(&platform);
/// ```
pub trait Platform {
    fn env(&self) -> &impl Env;
    fn env_mut(&mut self) -> &mut impl Env;
}

struct PlatformImpl {
    env: env::EnvImpl,
}

impl Platform for PlatformImpl {
    fn env(&self) -> &impl Env {
        &self.env
    }

    fn env_mut(&mut self) -> &mut impl Env {
        &mut self.env
    }
}

/// Returns an opaque default platform implementation
/// that is internally just a wrapper around the
/// std / tokio implementations.
pub fn platform() -> impl Platform {
    PlatformImpl { env: env::EnvImpl }
}
