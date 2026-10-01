pub(crate) mod platform_env;

use crate::Env;
use crate::Fs;
use crate::env::EnvImpl;
use crate::fs::FsImpl;
use platform_env::PlatformEnv;

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
    type E: Env;
    type F: Fs + platform_env::CurrentDirSetter;

    /// Returns a reference to the environment interface.
    fn env(&self) -> &impl Env;

    /// Returns a mutable reference to the environment interface.
    fn env_mut(&mut self) -> PlatformEnv<'_, Self::E, Self::F>;

    /// Returns a reference to the file system interface.
    fn fs(&self) -> &impl Fs;

    /// Returns a mutable reference to the file system interface.
    fn fs_mut(&mut self) -> &mut impl Fs;
}

pub(crate) struct PlatformImpl {
    pub(crate) env: EnvImpl,
    pub(crate) fs: FsImpl,
}

impl Platform for PlatformImpl {
    type E = EnvImpl;
    type F = FsImpl;

    /// Returns an opaque reference to the environment interface.
    fn env(&self) -> &impl Env {
        &self.env
    }

    /// Returns an opaque mutable reference to the environment interface.
    fn env_mut(&mut self) -> PlatformEnv<'_, Self::E, Self::F> {
        PlatformEnv {
            env: &mut self.env,
            fs: &mut self.fs,
        }
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
