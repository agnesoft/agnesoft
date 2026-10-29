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

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Returns the output of a successful run without running it.
    #[cfg(any(feature = "process", feature = "request"))]
    struct Stub;

    #[cfg(feature = "process")]
    impl crate::interceptor::Interceptor<crate::ProcessCommand> for Stub {
        fn handle<'a>(
            &'a self,
            _: crate::interceptor::Next<'a, crate::ProcessCommand>,
        ) -> crate::BoxFuture<'a, crate::Result<crate::CommandOutput>> {
            Box::pin(std::future::ready(Ok(crate::CommandOutput {
                success: true,
                status: Some(7),
                ..Default::default()
            })))
        }
    }

    #[cfg(feature = "request")]
    impl crate::interceptor::Interceptor<crate::HttpRequest> for Stub {
        fn handle<'a>(
            &'a self,
            _: crate::interceptor::Next<'a, crate::HttpRequest>,
        ) -> crate::BoxFuture<'a, crate::Result<crate::RequestOutput>> {
            Box::pin(std::future::ready(Ok(crate::RequestOutput {
                success: true,
                status: 299,
                ..Default::default()
            })))
        }
    }

    /// The contract cases of [`Platform`].
    pub(crate) mod platform {
        use super::*;

        pub(crate) mod env {
            use super::*;

            pub(crate) fn env_should_keep_changes_across_calls(platform: &impl Platform) {
                platform.env().set_var("KEY", "value").unwrap();

                assert_eq!(platform.env().var("KEY"), Some("value".into()));
            }
        }

        #[cfg(feature = "fs")]
        pub(crate) mod fs {
            use super::*;
            use crate::Fs;

            /// `dir` is an existing absolute directory of the fs of `platform`.
            pub(crate) fn fs_should_resolve_relative_paths_against_current_dir_of_env(
                platform: &impl Platform,
                dir: &std::path::Path,
            ) {
                platform.env().set_current_dir(dir).unwrap();

                platform.fs().write("file.txt", "content").unwrap();

                assert_eq!(
                    platform.fs().read_to_string(dir.join("file.txt")).unwrap(),
                    "content"
                );
            }
        }

        #[cfg(feature = "process")]
        pub(crate) mod process {
            use super::*;
            use crate::CommandExt;
            use crate::Process;

            pub(crate) fn process_should_run_commands_through_its_interceptors(
                platform: &impl Platform,
            ) {
                platform.process().add_interceptor(Stub);

                let output = std::process::Command::new("agplatform_missing_program")
                    .run_blocking(platform.process())
                    .unwrap();

                assert_eq!(output.status, Some(7));
            }
        }

        #[cfg(feature = "request")]
        pub(crate) mod request {
            use super::*;
            use crate::BlockingRequestBuilderExt;
            use crate::Request;

            pub(crate) fn request_should_send_requests_through_its_interceptors(
                platform: &impl Platform,
            ) {
                platform.request().add_interceptor(Stub);

                let output = platform
                    .request()
                    .blocking_client()
                    .unwrap()
                    .get("http://example.invalid/")
                    .send_blocking(platform.request())
                    .unwrap();

                assert_eq!(output.status, 299);
            }
        }
    }

    mod platform_impl {

        mod platform {

            mod env {

                #[test]
                fn env_should_keep_changes_across_calls() {
                    crate::platform::tests::platform::env::env_should_keep_changes_across_calls(
                        &crate::platform(),
                    );
                }
            }

            #[cfg(feature = "fs")]
            mod fs {

                #[test]
                fn fs_should_resolve_relative_paths_against_current_dir_of_env() {
                    let dir = crate::TestDir::new();

                    crate::platform::tests::platform::fs::fs_should_resolve_relative_paths_against_current_dir_of_env(
                        &crate::platform(),
                        dir.path(),
                    );
                }
            }

            #[cfg(feature = "process")]
            mod process {

                #[test]
                fn process_should_run_commands_through_its_interceptors() {
                    crate::platform::tests::platform::process::process_should_run_commands_through_its_interceptors(
                        &crate::platform(),
                    );
                }
            }

            #[cfg(feature = "request")]
            mod request {

                #[test]
                fn request_should_send_requests_through_its_interceptors() {
                    crate::platform::tests::platform::request::request_should_send_requests_through_its_interceptors(
                        &crate::platform(),
                    );
                }
            }
        }
    }
}
