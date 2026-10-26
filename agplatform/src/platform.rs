use std::sync::Arc;

use crate::Env;
use crate::env::EnvImpl;

/// Enabled by the `env` feature flag.
///
/// The trait represents a static (no `dyn`) abstract interface to a platform
/// implementing env and, with the respective `fs`, `process` and `request`
/// features, fs, process and request functionality. For a production
/// implementation use [`platform()`](crate::platform) to get an opaque
/// default implementation. For testing use `test_platform()`, which
/// provides memory-only but fully functional mock implementations (enabled
/// via the `testing` feature).
///
/// All of its interfaces work through shared references, including
/// changing the environment, so a platform can be shared between threads
/// and tasks in an [`Arc`], which is a platform itself.
///
/// # Examples
///
/// ```rust
/// use agplatform::{Env, Platform};
/// use std::sync::Arc;
///
/// fn do_something(platform: &impl Platform) {
///     let current_dir = platform.env().current_dir();
/// }
///
/// let platform = Arc::new(agplatform::platform());
/// let shared = platform.clone();
/// std::thread::spawn(move || do_something(&shared)).join().unwrap();
/// do_something(&platform);
/// ```
pub trait Platform: Send + Sync {
    /// Returns a reference to the environment interface.
    fn env(&self) -> &(impl Env + use<Self>);

    /// Enabled by the `fs` feature flag.
    ///
    /// Returns a reference to the file system interface.
    #[cfg(feature = "fs")]
    fn fs(&self) -> &(impl crate::Fs + use<Self>);

    /// Enabled by the `process` feature flag.
    ///
    /// Returns a reference to the process interface.
    #[cfg(feature = "process")]
    fn process(&self) -> &(impl crate::Process + use<Self>);

    /// Enabled by the `request` feature flag.
    ///
    /// Returns a reference to the network request interface.
    #[cfg(feature = "request")]
    fn request(&self) -> &(impl crate::Request + use<Self>);
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
    /// Returns an opaque reference to the environment interface.
    fn env(&self) -> &(impl Env + use<>) {
        &self.env
    }

    /// Returns an opaque reference to the file system interface.
    #[cfg(feature = "fs")]
    fn fs(&self) -> &(impl crate::Fs + use<>) {
        &self.fs
    }

    /// Returns an opaque reference to the process interface.
    #[cfg(feature = "process")]
    fn process(&self) -> &(impl crate::Process + use<>) {
        &self.process
    }

    /// Returns an opaque reference to the network request interface.
    #[cfg(feature = "request")]
    fn request(&self) -> &(impl crate::Request + use<>) {
        &self.request
    }
}

/// The platform shared by the [`Arc`].
impl<P: Platform> Platform for Arc<P> {
    fn env(&self) -> &(impl Env + use<P>) {
        (**self).env()
    }

    #[cfg(feature = "fs")]
    fn fs(&self) -> &(impl crate::Fs + use<P>) {
        (**self).fs()
    }

    #[cfg(feature = "process")]
    fn process(&self) -> &(impl crate::Process + use<P>) {
        (**self).process()
    }

    #[cfg(feature = "request")]
    fn request(&self) -> &(impl crate::Request + use<P>) {
        (**self).request()
    }
}
