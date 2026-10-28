use std::ffi::OsStr;
use std::ffi::OsString;
use std::path::Path;
use std::path::PathBuf;

use crate::Env;
use crate::EnvVars;
use crate::env::EnvArgs;
use crate::env::EnvImpl;
use crate::test_platform::ROOT;

/// Enabled by the `testing` feature flag.
///
/// A memory-only [`crate::Env`] implementation for tests.
/// Use [`Self::new`] together with [`Self::with_args`],
/// [`Self::with_vars`], and [`Self::with_current_dir`] to seed
/// arguments, variables, and the current directory before passing it
/// to code that expects an environment implementation.
///
/// This is a lightweight wrapper around the internal `EnvImpl`.
///
/// # Examples
///
/// ```rust
/// use agplatform::Env;
/// use agplatform::TestEnv;
///
/// let env = TestEnv::new()
///     .with_args(vec!["app".to_string(), "--verbose".to_string()])
///     .with_vars(vec![("TEST_VAR".to_string(), "value".to_string())]);
///
/// assert_eq!(env.args().collect::<Vec<_>>(), ["app", "--verbose"]);
/// assert_eq!(env.var("TEST_VAR"), Some("value".into()));
/// ```
pub struct TestEnv(EnvImpl);

impl TestEnv {
    /// Creates a new empty test environment whose current directory is the
    /// [`ROOT`] directory.
    ///
    /// This is the test-only counterpart to `EnvImpl::new`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::TestEnv;
    ///
    /// let env = TestEnv::new();
    /// ```
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self(EnvImpl::new(Vec::new(), PathBuf::from(ROOT)))
    }

    /// Sets the command-line arguments used by this test environment.
    ///
    /// See [`crate::Env::args`] for how the values are read back.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::Env;
    /// use agplatform::TestEnv;
    ///
    /// let env = TestEnv::new().with_args(vec!["app", "--help"]);
    /// assert_eq!(env.args().collect::<Vec<_>>(), ["app", "--help"]);
    /// ```
    pub fn with_args<T: Into<OsString>>(mut self, args: Vec<T>) -> Self {
        self.0
            .replace_args(args.into_iter().map(Into::into).collect());
        self
    }

    /// Sets the current directory used by this test environment.
    ///
    /// See [`crate::Env::current_dir`] for the read-side API.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::Env;
    /// use agplatform::TestEnv;
    /// use std::path::PathBuf;
    ///
    /// let env = TestEnv::new().with_current_dir(PathBuf::from("/tmp/project"));
    /// assert_eq!(env.current_dir(), std::path::Path::new("/tmp/project"));
    /// ```
    pub fn with_current_dir<P: Into<PathBuf>>(self, current_dir: P) -> Self {
        self.0.replace_current_dir(current_dir.into());
        self
    }

    /// Sets the current executable used by this test environment.
    ///
    /// See [`crate::Env::current_exe`] for the read-side API.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::Env;
    /// use agplatform::TestEnv;
    /// use std::path::PathBuf;
    ///
    /// let env = TestEnv::new().with_current_exe(PathBuf::from("/tmp/project/app"));
    /// assert_eq!(env.current_exe(), std::path::Path::new("/tmp/project/app"));
    /// ```
    pub fn with_current_exe<P: Into<PathBuf>>(mut self, current_exe: P) -> Self {
        self.0.replace_current_exe(current_exe.into());
        self
    }

    /// Sets the environment variables used by this test environment.
    ///
    /// See [`crate::Env::vars`] and [`crate::Env::var`] for the read-side APIs.
    ///
    /// # Panics
    ///
    /// In debug builds, if a variable would be rejected by
    /// [`crate::Env::set_var`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::Env;
    /// use agplatform::TestEnv;
    ///
    /// let env = TestEnv::new().with_vars(vec![("TEST_VAR", "value")]);
    /// assert_eq!(env.var("TEST_VAR"), Some("value".into()));
    /// ```
    pub fn with_vars<K: Into<OsString>, V: Into<OsString>>(self, vars: Vec<(K, V)>) -> Self {
        self.0.replace_vars(
            vars.into_iter()
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
        );
        self
    }

    /// [internal]
    ///
    /// Returns a handle to this environment that sees its changes.
    #[cfg(any(feature = "fs", feature = "process", feature = "request"))]
    pub(crate) fn share(&self) -> EnvImpl {
        self.0.clone()
    }
}

impl Env for TestEnv {
    /// Returns the command-line arguments stored in the test environment.
    ///
    /// See [`crate::Env::args`].
    fn args(&self) -> EnvArgs<'_> {
        self.0.args()
    }

    /// Returns the current directory stored in the test environment.
    ///
    /// See [`crate::Env::current_dir`].
    fn current_dir(&self) -> PathBuf {
        self.0.current_dir()
    }

    /// Returns the current executable stored in the test environment.
    ///
    /// See [`crate::Env::current_exe`].
    fn current_exe(&self) -> &Path {
        self.0.current_exe()
    }

    /// Returns the home directory stored in the test environment.
    ///
    /// See [`crate::Env::home_dir`].
    fn home_dir(&self) -> Option<PathBuf> {
        self.0.home_dir()
    }

    /// Removes an environment variable from the test environment.
    ///
    /// See [`crate::Env::remove_var`].
    fn remove_var<K: AsRef<OsStr>>(&self, key: K) -> Option<OsString> {
        self.0.remove_var(key)
    }

    /// Sets the current directory in the test environment, without
    /// checking that it exists.
    ///
    /// See [`crate::Env::set_current_dir`].
    fn set_current_dir<P: AsRef<Path>>(&self, path: P) -> crate::Result<PathBuf> {
        self.0.set_current_dir_checked(path.as_ref(), &|_| Ok(()))
    }

    /// Sets an environment variable in the test environment.
    ///
    /// See [`crate::Env::set_var`].
    fn set_var<K: Into<OsString>, V: Into<OsString>>(
        &self,
        key: K,
        value: V,
    ) -> crate::Result<Option<OsString>> {
        self.0.set_var(key, value)
    }

    /// Returns the temporary directory of the test environment.
    ///
    /// See [`crate::Env::tmp_dir`].
    fn tmp_dir(&self) -> PathBuf {
        self.0.tmp_dir()
    }

    /// Returns an environment variable from the test environment.
    ///
    /// See [`crate::Env::var`].
    fn var<K: AsRef<OsStr>>(&self, key: K) -> Option<OsString> {
        self.0.var(key)
    }

    /// Returns the environment variables stored in the test environment.
    ///
    /// See [`crate::Env::vars`].
    fn vars(&self) -> EnvVars {
        self.0.vars()
    }
}
