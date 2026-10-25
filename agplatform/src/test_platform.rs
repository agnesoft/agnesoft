pub mod test_env;
#[cfg(feature = "fs")]
pub mod test_fs;
#[cfg(feature = "process")]
pub mod test_process;
#[cfg(feature = "request")]
pub mod test_request;

use crate::Env;
use crate::Platform;
use crate::TestEnv;
use crate::platform::platform_env::PlatformEnv;

/// The root directory of a new [`TestEnv`], `TestFs` and `TestProcess`.
#[cfg(unix)]
pub const ROOT: &str = "/";
/// The root directory of a new [`TestEnv`], `TestFs` and `TestProcess`.
#[cfg(windows)]
pub const ROOT: &str = "C:\\";

/// Enabled by the `testing` feature flag.
///
/// The test platform is a memory-only but fully functional mock
/// implementation of the [`Platform`] trait. It is used in tests to be
/// passed where `impl Platform` is expected.
///
/// # Examples
///
/// ```rust
/// use agplatform::Platform;
/// use agplatform::test_platform::test_platform;
///
/// fn do_something(platform: &impl Platform) {}
///
/// let test_platform = test_platform();
/// do_something(&test_platform);
/// ```
pub struct TestPlatform {
    pub env: TestEnv,
    #[cfg(feature = "fs")]
    pub fs: crate::TestFs,
    #[cfg(feature = "process")]
    pub process: crate::TestProcess,
    #[cfg(feature = "request")]
    pub request: crate::TestRequest,
}

impl TestPlatform {
    /// Creates a test platform with the specified test environment.
    ///
    /// See [`Self::with_env`] and [`crate::TestEnv`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Platform, Env};
    /// use agplatform::test_platform::test_platform;
    ///
    /// let test_env = agplatform::TestEnv::new().with_args(vec!["arg1", "arg2"]);
    /// let test_platform = test_platform().with_env(test_env);
    /// assert_eq!(test_platform.env().args().collect::<Vec<_>>(), &["arg1", "arg2"]);
    /// ```
    pub fn with_env(mut self, env: TestEnv) -> Self {
        #[cfg(feature = "request")]
        self.request.set_env(&env);
        self.env = env;
        self
    }

    /// Enabled by the `fs` feature flag.
    ///
    /// Creates a test platform with the specified test file system.
    ///
    /// See [`Self::with_fs`] and [`crate::TestFs`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Platform, Fs};
    /// use agplatform::test_platform::test_platform;
    ///
    /// let test_fs = agplatform::TestFs::new();
    /// let test_platform = test_platform().with_fs(test_fs);
    /// ```
    #[cfg(feature = "fs")]
    pub fn with_fs(mut self, fs: crate::TestFs) -> Self {
        self.fs = fs;
        self
    }

    /// Enabled by the `process` feature flag.
    ///
    /// Creates a test platform with the specified test process interface.
    ///
    /// See [`crate::TestProcess`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::Platform;
    /// use agplatform::test_platform::test_platform;
    ///
    /// let test_process = agplatform::TestProcess::new();
    /// let test_platform = test_platform().with_process(test_process);
    /// ```
    #[cfg(feature = "process")]
    pub fn with_process(mut self, process: crate::TestProcess) -> Self {
        self.process = process;
        self
    }

    /// Enabled by the `request` feature flag.
    ///
    /// Creates a test platform with the specified test request interface.
    ///
    /// See [`crate::TestRequest`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::Platform;
    /// use agplatform::test_platform::test_platform;
    ///
    /// let test_request = agplatform::TestRequest::new();
    /// let test_platform = test_platform().with_request(test_request);
    /// ```
    #[cfg(feature = "request")]
    pub fn with_request(mut self, mut request: crate::TestRequest) -> Self {
        request.set_env(&self.env);
        self.request = request;
        self
    }
}

impl Platform for TestPlatform {
    type E = TestEnv;

    /// Returns a reference to the test environment.
    ///
    /// See [`crate::Platform::env`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Env, Platform};
    /// use agplatform::test_platform::test_platform;
    ///
    /// let platform = test_platform();
    /// let env = platform.env();
    /// ```
    fn env(&self) -> &(impl Env + use<>) {
        &self.env
    }

    /// Returns a mutable reference to the test environment.
    ///
    /// See [`crate::Platform::env_mut`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Env, Platform};
    /// use agplatform::test_platform::test_platform;
    ///
    /// let mut platform = test_platform();
    /// let env_mut = platform.env_mut();
    /// ```
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

    /// Returns a reference to the test file system.
    ///
    /// See [`crate::Platform::fs`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform};
    /// use agplatform::test_platform::test_platform;
    ///
    /// let platform = test_platform();
    /// let fs = platform.fs();
    /// ```
    #[cfg(feature = "fs")]
    fn fs(&self) -> &(impl crate::Fs + use<>) {
        &self.fs
    }

    /// Returns a mutable reference to the test file system.
    ///
    /// See [`crate::Platform::fs_mut`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Fs, Platform};
    /// use agplatform::test_platform::test_platform;
    ///
    /// let mut platform = test_platform();
    /// let fs_mut = platform.fs_mut();
    /// ```
    #[cfg(feature = "fs")]
    fn fs_mut(&mut self) -> &mut (impl crate::Fs + use<>) {
        &mut self.fs
    }

    /// Returns a reference to the test process interface.
    ///
    /// See [`crate::Platform::process`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Platform, Process};
    /// use agplatform::test_platform::test_platform;
    ///
    /// let platform = test_platform();
    /// let process = platform.process();
    /// ```
    #[cfg(feature = "process")]
    fn process(&self) -> &(impl crate::Process + use<>) {
        &self.process
    }

    /// Returns a mutable reference to the test process interface.
    ///
    /// See [`crate::Platform::process_mut`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Platform, Process};
    /// use agplatform::test_platform::test_platform;
    ///
    /// let mut platform = test_platform();
    /// let process_mut = platform.process_mut();
    /// ```
    #[cfg(feature = "process")]
    fn process_mut(&mut self) -> &mut (impl crate::Process + use<>) {
        &mut self.process
    }

    /// Returns a reference to the test request interface.
    ///
    /// See [`crate::Platform::request`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Platform, Request};
    /// use agplatform::test_platform::test_platform;
    ///
    /// let platform = test_platform();
    /// let request = platform.request();
    /// ```
    #[cfg(feature = "request")]
    fn request(&self) -> &(impl crate::Request + use<>) {
        &self.request
    }

    /// Returns a mutable reference to the test request interface.
    ///
    /// See [`crate::Platform::request_mut`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Platform, Request};
    /// use agplatform::test_platform::test_platform;
    ///
    /// let mut platform = test_platform();
    /// let request_mut = platform.request_mut();
    /// ```
    #[cfg(feature = "request")]
    fn request_mut(&mut self) -> &mut (impl crate::Request + use<>) {
        &mut self.request
    }
}

/// Returns an instance of [`TestPlatform`] that implements the
/// [`Platform`] trait via the mock implementations.
///
/// # Examples
///
/// ```rust
/// use agplatform::Platform;
/// use agplatform::test_platform::test_platform;
///
/// fn do_something(platform: &impl Platform) {}
///
/// let test_platform = test_platform();
/// do_something(&test_platform);
/// ```
pub fn test_platform() -> TestPlatform {
    TestPlatform {
        env: TestEnv::new(),
        #[cfg(feature = "fs")]
        fs: crate::TestFs::new(),
        #[cfg(feature = "process")]
        process: crate::TestProcess::new(),
        #[cfg(feature = "request")]
        request: crate::TestRequest::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "request")]
    fn reqwest_env(https_proxy: &str) -> crate::request::ReqwestEnv {
        crate::request::ReqwestEnv::new(&[("HTTPS_PROXY".to_string(), https_proxy.to_string())])
    }

    mod test_platform {
        use super::*;

        mod with_env {
            use super::*;

            #[test]
            fn with_env_should_set_custom_test_environment() {
                let test_env = TestEnv::new()
                    .with_args(vec!["arg1", "arg2"])
                    .with_current_dir("/home")
                    .with_current_exe("/tmp/project/app")
                    .with_home_dir("/home/user")
                    .with_tmp_dir("/tmp")
                    .with_vars(vec![("KEY1", "value1"), ("KEY2", "value2")]);
                let mut p = test_platform().with_env(test_env);
                let mut e = p.env_mut();

                assert_eq!(e.args().collect::<Vec<_>>(), &["arg1", "arg2"]);
                assert_eq!(e.current_dir(), std::path::Path::new("/home"));
                assert_eq!(e.set_current_dir("/tmp"), std::path::Path::new("/home"));
                assert_eq!(e.current_dir(), std::path::Path::new("/tmp"));
                assert_eq!(e.current_exe(), std::path::Path::new("/tmp/project/app"));
                assert_eq!(e.home_dir(), std::path::Path::new("/home/user"));
                assert_eq!(e.tmp_dir(), std::path::Path::new("/tmp"));
                assert_eq!(e.var("KEY1"), Some("value1"));
                assert_eq!(e.var("KEY2"), Some("value2"));
                assert_eq!(e.var("NON_EXISTENT_KEY"), None);
                assert_eq!(e.set_var("NON_EXISTENT_KEY", "value"), None);
                assert_eq!(e.remove_var("NON_EXISTENT_KEY"), Some("value".to_string()));
                assert_eq!(e.set_var("KEY1", "new_value"), Some("value1".to_string()));
                assert_eq!(
                    e.vars().collect::<Vec<_>>(),
                    vec![
                        &("KEY1".to_string(), "new_value".to_string()),
                        &("KEY2".to_string(), "value2".to_string())
                    ]
                );
            }

            #[cfg(feature = "request")]
            #[test]
            fn with_env_should_pass_proxy_variables_to_request() {
                let test_env = TestEnv::new().with_vars(vec![("HTTPS_PROXY", "http://proxy")]);

                let test_platform = test_platform().with_env(test_env);

                assert_eq!(
                    test_platform.request.reqwest_env,
                    reqwest_env("http://proxy")
                );
            }
        }

        #[cfg(feature = "request")]
        mod with_request {
            use super::*;

            #[test]
            fn with_request_should_pass_proxy_variables_of_env() {
                let test_env = TestEnv::new().with_vars(vec![("HTTPS_PROXY", "http://proxy")]);

                let test_platform = test_platform()
                    .with_env(test_env)
                    .with_request(crate::TestRequest::new());

                assert_eq!(
                    test_platform.request.reqwest_env,
                    reqwest_env("http://proxy")
                );
            }
        }

        #[cfg(feature = "fs")]
        mod with_fs {
            use super::*;
            use crate::Fs;

            #[test]
            fn with_fs_should_set_custom_test_filesystem() {
                let test_fs = crate::TestFs::new();
                let mut test_platform = test_platform().with_fs(test_fs);
                test_platform.env_mut().set_current_dir("/tmp");
                test_platform.fs().create_dir_all("/tmp").unwrap();
                test_platform.fs().write("/tmp/file.txt", "hello").unwrap();

                assert_eq!(
                    test_platform.fs().read("/tmp/file.txt").unwrap(),
                    vec![b'h', b'e', b'l', b'l', b'o'],
                );
                assert_eq!(
                    test_platform.fs().read_to_string("/tmp/file.txt").unwrap(),
                    "hello",
                );
            }
        }

        mod platform {
            use super::*;

            mod env_mut {
                use super::*;

                #[cfg(all(feature = "fs", feature = "process"))]
                #[test]
                fn env_mut_set_current_dir_should_update_fs_and_process() {
                    use crate::Fs;

                    // GIVEN an existing directory
                    let mut test_platform = test_platform();
                    test_platform.fs().create_dir_all("/tmp").unwrap();

                    #[cfg(unix)]
                    const CURRENT_DIR: &str = "/tmp";
                    #[cfg(windows)]
                    const CURRENT_DIR: &str = "C:\\tmp";

                    // WHEN it becomes the current directory
                    test_platform.env_mut().set_current_dir(CURRENT_DIR);

                    // THEN relative fs paths resolve against it
                    assert_eq!(
                        test_platform.fs().canonicalize(".").unwrap(),
                        std::path::Path::new(CURRENT_DIR)
                    );
                    assert_eq!(
                        test_platform.env.current_dir(),
                        std::path::Path::new(CURRENT_DIR)
                    );
                    assert_eq!(
                        test_platform.process.current_dir,
                        std::path::Path::new(CURRENT_DIR)
                    );
                }

                #[cfg(feature = "request")]
                #[test]
                fn env_mut_set_and_remove_var_should_update_request_proxy_variables() {
                    let mut test_platform = test_platform();

                    test_platform
                        .env_mut()
                        .set_var("HTTPS_PROXY", "http://proxy");
                    let set = test_platform.request.reqwest_env.clone();
                    test_platform.env_mut().remove_var("HTTPS_PROXY");

                    assert_eq!(set, reqwest_env("http://proxy"));
                    assert_eq!(
                        test_platform.request.reqwest_env,
                        crate::request::ReqwestEnv::default()
                    );
                }
            }
        }
    }
}
