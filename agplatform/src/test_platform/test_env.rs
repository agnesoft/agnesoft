use std::path::Path;
use std::path::PathBuf;

use crate::Env;
use crate::EnvVars;
use crate::env::EnvArgs;
use crate::env::EnvImpl;

/// Enabled by the `testing` feature flag.
///
/// A memory-only [`crate::Env`] implementation for tests.
/// Use [`Self::new`] together with [`Self::with_args`],
/// [`Self::with_vars`], and [`Self::with_current_dir`] to seed
/// arguments, variables, and the current directory before passing it
/// to code that expects an environment implementation.
///
/// This is a lightweight wrapper around [`crate::env::EnvImpl`].
///
/// Example:
///
/// ```rust
/// use agplatform::Env;
/// use agplatform::TestEnv;
///
/// let env = TestEnv::new()
///     .with_args(vec!["app".to_string(), "--verbose".to_string()])
///     .with_vars(vec![("TEST_VAR".to_string(), "value".to_string())]);
///
/// assert_eq!(env.args().cloned().collect::<Vec<_>>(), vec!["app".to_string(), "--verbose".to_string()]);
/// assert_eq!(env.var("TEST_VAR"), Some("value"));
/// ```
pub struct TestEnv(EnvImpl);

impl TestEnv {
    /// Creates a new empty test environment.
    ///
    /// This is the test-only counterpart to [`crate::env::EnvImpl::new`].
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::TestEnv;
    ///
    /// let env = TestEnv::new();
    /// ```
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self(EnvImpl::new())
    }

    /// Sets the command-line arguments used by this test environment.
    ///
    /// See [`crate::Env::args`] for how the values are read back.
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::Env;
    /// use agplatform::TestEnv;
    ///
    /// let env = TestEnv::new().with_args(vec!["app", "--help"]);
    /// assert_eq!(env.args().cloned().collect::<Vec<_>>(), vec!["app", "--help"]);
    /// ```
    pub fn with_args<T: Into<String>>(mut self, args: Vec<T>) -> Self {
        self.0.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// Sets the environment variables used by this test environment.
    ///
    /// See [`crate::Env::vars`] and [`crate::Env::var`] for the read-side APIs.
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::Env;
    /// use agplatform::TestEnv;
    ///
    /// let env = TestEnv::new().with_vars(vec![("TEST_VAR".to_string(), "value".to_string())]);
    /// assert_eq!(env.var("TEST_VAR"), Some("value"));
    /// ```
    pub fn with_vars<K: Into<String>, V: Into<String>>(mut self, vars: Vec<(K, V)>) -> Self {
        self.0.vars = vars
            .into_iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect();
        self
    }

    /// Sets the current directory used by this test environment.
    ///
    /// See [`crate::Env::current_dir`] for the read-side API.
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::Env;
    /// use agplatform::TestEnv;
    /// use std::path::PathBuf;
    ///
    /// let env = TestEnv::new().with_current_dir(PathBuf::from("/tmp/project"));
    /// assert_eq!(env.current_dir(), std::path::Path::new("/tmp/project"));
    /// ```
    pub fn with_current_dir<P: Into<PathBuf>>(mut self, current_dir: P) -> Self {
        self.0.current_dir = current_dir.into();
        self
    }
}

impl Env for TestEnv {
    /// Returns the command-line arguments stored in the test environment.
    ///
    /// See [`crate::Env::args`].
    fn args(&self) -> EnvArgs<'_> {
        self.0.args()
    }

    /// Returns the environment variables stored in the test environment.
    ///
    /// See [`crate::Env::vars`].
    fn vars(&self) -> EnvVars<'_> {
        self.0.vars()
    }

    /// Returns the current directory stored in the test environment.
    ///
    /// See [`crate::Env::current_dir`].
    fn current_dir(&self) -> &Path {
        self.0.current_dir()
    }

    /// Removes an environment variable from the test environment.
    ///
    /// See [`crate::Env::remove_var`].
    fn remove_var<T: AsRef<str>>(&mut self, key: T) -> Option<String> {
        self.0.remove_var(key)
    }

    /// Sets the current directory in the test environment.
    ///
    /// See [`crate::Env::set_current_dir`].
    fn set_current_dir<P: Into<PathBuf>>(&mut self, path: P) -> PathBuf {
        self.0.set_current_dir(path)
    }

    /// Sets an environment variable in the test environment.
    ///
    /// See [`crate::Env::set_var`].
    fn set_var<T: Into<String>, U: Into<String>>(&mut self, key: T, value: U) -> Option<String> {
        self.0.set_var(key, value)
    }

    /// Returns an environment variable from the test environment.
    ///
    /// See [`crate::Env::var`].
    fn var<T: AsRef<str>>(&self, key: T) -> Option<&str> {
        self.0.var(key)
    }
}
