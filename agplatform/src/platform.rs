pub(crate) mod platform_env;

use crate::Env;
use crate::Fs;
use crate::Process;
use crate::Request;
use crate::env::EnvImpl;
use crate::fs::FsImpl;
use crate::process::ProcessImpl;
use crate::request::RequestImpl;
use platform_env::PlatformEnv;

/// The trait represents a zero-cost abstract interface to a platform
/// implementing env, fs, process, and request functionality. For a
/// production implementation use [`platform()`](crate::platform) to get
/// an opaque default implementation. For testing use `test_platform()`,
/// which provides memory-only but fully functional mock
/// implementations (enabled via the `testing` feature).
///
/// # Examples
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
    type P: Process + platform_env::CurrentDirSetter + platform_env::EnvVarSetter;

    /// Returns a reference to the environment interface.
    fn env(&self) -> &(impl Env + use<Self>);

    /// Returns a mutable reference to the environment interface.
    fn env_mut(&mut self) -> PlatformEnv<'_, Self::E, Self::F, Self::P>;

    /// Returns a reference to the file system interface.
    fn fs(&self) -> &(impl Fs + use<Self>);

    /// Returns a mutable reference to the file system interface.
    fn fs_mut(&mut self) -> &mut (impl Fs + use<Self>);

    /// Returns a reference to the process interface.
    fn process(&self) -> &(impl Process + use<Self>);

    /// Returns a mutable reference to the process interface.
    fn process_mut(&mut self) -> &mut (impl Process + use<Self>);

    /// Returns a reference to the network request interface.
    fn request(&self) -> &(impl Request + use<Self>);
}

pub(crate) struct PlatformImpl {
    pub(crate) env: EnvImpl,
    pub(crate) fs: FsImpl,
    pub(crate) process: ProcessImpl,
    pub(crate) request: RequestImpl,
}

impl Platform for PlatformImpl {
    type E = EnvImpl;
    type F = FsImpl;
    type P = ProcessImpl;

    /// Returns an opaque reference to the environment interface.
    fn env(&self) -> &(impl Env + use<>) {
        &self.env
    }

    /// Returns an opaque mutable reference to the environment interface.
    fn env_mut(&mut self) -> PlatformEnv<'_, Self::E, Self::F, Self::P> {
        PlatformEnv {
            env: &mut self.env,
            fs: &mut self.fs,
            process: &mut self.process,
        }
    }

    /// Returns an opaque reference to the file system interface.
    fn fs(&self) -> &(impl Fs + use<>) {
        &self.fs
    }

    /// Returns an opaque mutable reference to the file system interface.
    fn fs_mut(&mut self) -> &mut (impl Fs + use<>) {
        &mut self.fs
    }

    /// Returns an opaque reference to the process interface.
    fn process(&self) -> &(impl Process + use<>) {
        &self.process
    }

    /// Returns an opaque mutable reference to the process interface.
    fn process_mut(&mut self) -> &mut (impl Process + use<>) {
        &mut self.process
    }

    /// Returns an opaque reference to the network request interface.
    fn request(&self) -> &(impl Request + use<>) {
        &self.request
    }
}
