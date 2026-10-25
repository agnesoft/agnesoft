pub(crate) mod platform_env;

use crate::Env;
use crate::env::EnvImpl;
use platform_env::PlatformEnv;

/// Enabled by the `env` feature flag.
///
/// The trait represents a zero-cost abstract interface to a platform
/// implementing env and, with the respective `fs`, `process` and `request`
/// features, fs, process and request functionality. For a production
/// implementation use [`platform()`](crate::platform) to get an opaque
/// default implementation. For testing use `test_platform()`, which
/// provides memory-only but fully functional mock implementations (enabled
/// via the `testing` feature).
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
pub trait Platform: Send + Sync {
    type E: Env;

    /// Returns a reference to the environment interface.
    fn env(&self) -> &(impl Env + use<Self>);

    /// Returns a mutable reference to the environment interface.
    fn env_mut(&mut self) -> PlatformEnv<'_, Self::E>;

    /// Enabled by the `fs` feature flag.
    ///
    /// Returns a reference to the file system interface.
    #[cfg(feature = "fs")]
    fn fs(&self) -> &(impl crate::Fs + use<Self>);

    /// Enabled by the `fs` feature flag.
    ///
    /// Returns a mutable reference to the file system interface.
    #[cfg(feature = "fs")]
    fn fs_mut(&mut self) -> &mut (impl crate::Fs + use<Self>);

    /// Enabled by the `process` feature flag.
    ///
    /// Returns a reference to the process interface.
    #[cfg(feature = "process")]
    fn process(&self) -> &(impl crate::Process + use<Self>);

    /// Enabled by the `process` feature flag.
    ///
    /// Returns a mutable reference to the process interface.
    #[cfg(feature = "process")]
    fn process_mut(&mut self) -> &mut (impl crate::Process + use<Self>);

    /// Enabled by the `request` feature flag.
    ///
    /// Returns a reference to the network request interface.
    #[cfg(feature = "request")]
    fn request(&self) -> &(impl crate::Request + use<Self>);

    /// Enabled by the `request` feature flag.
    ///
    /// Returns a mutable reference to the network request interface.
    #[cfg(feature = "request")]
    fn request_mut(&mut self) -> &mut (impl crate::Request + use<Self>);
}

pub(crate) struct PlatformImpl {
    pub(crate) env: EnvImpl,
    #[cfg(feature = "fs")]
    pub(crate) fs: crate::fs::FsImpl,
    #[cfg(feature = "process")]
    pub(crate) process: crate::process::ProcessImpl,
    #[cfg(feature = "request")]
    pub(crate) request: crate::request::RequestImpl,
}

impl Platform for PlatformImpl {
    type E = EnvImpl;

    /// Returns an opaque reference to the environment interface.
    fn env(&self) -> &(impl Env + use<>) {
        &self.env
    }

    /// Returns an opaque mutable reference to the environment interface.
    fn env_mut(&mut self) -> PlatformEnv<'_, Self::E> {
        PlatformEnv {
            env: &mut self.env,
            #[cfg(feature = "fs")]
            fs: &mut self.fs,
            #[cfg(feature = "process")]
            process: &mut self.process,
            #[cfg(feature = "request")]
            request: &mut self.request,
        }
    }

    /// Returns an opaque reference to the file system interface.
    #[cfg(feature = "fs")]
    fn fs(&self) -> &(impl crate::Fs + use<>) {
        &self.fs
    }

    /// Returns an opaque mutable reference to the file system interface.
    #[cfg(feature = "fs")]
    fn fs_mut(&mut self) -> &mut (impl crate::Fs + use<>) {
        &mut self.fs
    }

    /// Returns an opaque reference to the process interface.
    #[cfg(feature = "process")]
    fn process(&self) -> &(impl crate::Process + use<>) {
        &self.process
    }

    /// Returns an opaque mutable reference to the process interface.
    #[cfg(feature = "process")]
    fn process_mut(&mut self) -> &mut (impl crate::Process + use<>) {
        &mut self.process
    }

    /// Returns an opaque reference to the network request interface.
    #[cfg(feature = "request")]
    fn request(&self) -> &(impl crate::Request + use<>) {
        &self.request
    }

    /// Returns an opaque mutable reference to the network request
    /// interface.
    #[cfg(feature = "request")]
    fn request_mut(&mut self) -> &mut (impl crate::Request + use<>) {
        &mut self.request
    }
}
