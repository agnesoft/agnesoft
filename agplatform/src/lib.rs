#[macro_use]
mod error;
mod env;
mod fs;
#[cfg(feature = "testing")]
pub mod test_platform;
mod utils;

pub use env::Env;
pub use env::EnvArgs;
pub use env::EnvVars;
pub use error::Error;
pub use error::ErrorKind;
pub use fs::Fs;
#[cfg(feature = "testing")]
pub use test_platform::{test_env::TestEnv, test_fs::TestFs};

pub type Result<T = ()> = std::result::Result<T, Error>;

/// The trait represents a zero-cost abstract interface to a platform
/// implementing env, fs, exec, and request functionality. For a
/// production implementation use [`platform`] to get an opaque default
/// implementation. For testing use [`test_platform::test_platform`],
/// which provides memory-only but fully functional mock
/// implementations (enabled via the `testing` feature).
///
/// Example:
///
/// ```rust
/// use agplatform::{Env, Platform};
///
/// fn do_something(platform: &impl Platform) {
///     let current_dir = platform.env().current_dir();
/// }
///
/// let platform = agplatform::platform();
/// do_something(&platform)
/// ```
pub trait Platform {
    /// Returns a reference to the environment interface.
    fn env(&self) -> &impl Env;

    /// Returns a mutable reference to the environment interface.
    fn env_mut(&mut self) -> &mut impl Env;

    /// Returns a reference to the file system interface.
    fn fs(&self) -> &impl Fs;

    /// Returns a mutable reference to the file system interface.
    fn fs_mut(&mut self) -> &mut impl Fs;
}

struct PlatformImpl {
    env: env::EnvImpl,
    fs: fs::FsImpl,
}

impl Platform for PlatformImpl {
    /// Returns an opaque reference to the environment interface.
    fn env(&self) -> &impl Env {
        &self.env
    }

    /// Returns an opaque mutable reference to the environment interface.
    fn env_mut(&mut self) -> &mut impl Env {
        &mut self.env
    }

    /// Returns an opaque reference to the file system interface.
    fn fs(&self) -> &impl Fs {
        &self.fs
    }

    /// Returns an opaque mutable reference to the file system interface.
    fn fs_mut(&mut self) -> &mut impl Fs {
        &mut self.fs
    }
}

/// Returns an opaque default platform implementation.
pub fn platform() -> impl Platform {
    PlatformImpl {
        env: env::EnvImpl::new_from_std(),
        fs: fs::FsImpl::new(),
    }
}
