use std::path::Path;
use std::path::PathBuf;

/// The [`Env`] trait representing an abstract interface to a
/// platform implementing environment functionality such as current
/// directory and environment variables.
pub trait Env {
    /// Returns an iterator over the command-line arguments as strings.
    fn args(&self) -> EnvArgs<'_>;

    /// Returns a reference to the current directory as a `Path`.
    fn current_dir(&self) -> &Path;

    /// Removes the environment variable with the given key and returns its value if it existed.
    fn remove_var<T: AsRef<str>>(&mut self, key: T) -> Option<String>;

    /// Sets the current directory to the given path and returns the previous current directory.
    fn set_current_dir<P: Into<PathBuf>>(&mut self, path: P) -> PathBuf;

    /// Sets the environment variable with the given key to the given value and returns the previous value if it existed.
    fn set_var<T: Into<String>, U: Into<String>>(&mut self, key: T, value: U) -> Option<String>;

    /// Returns the value of the environment variable with the given key, if it exists.
    fn var<T: AsRef<str>>(&self, key: T) -> Option<&str>;

    /// Returns an iterator over all environment variables as key-value pairs.
    fn vars(&self) -> EnvVars<'_>;
}

/// In-process implementation of [`Env`] providing similar capabilities
/// to [`std::env`] but with a memory-only implementation that can be
/// used for both production and testing as long as it is consistently
/// used throughout the process with no explicit calls to [`std::env`].
///
/// The values are loaded upon creation ([`Self::new_from_std`]) from
/// [`std::env`] once and then used throughout the lifetime of the
/// [`EnvImpl`] instance. It is not recommended to create multiple
/// instances of [`EnvImpl`] as changes to environment variables or the
/// current directory are not persisted process wide.
///
/// Example:
///
/// ```rust
/// use agplatform::{Env, Platform};
///
/// let mut platform = agplatform::platform();
/// let current_dir = platform.env().current_dir();
/// assert_eq!(current_dir, std::env::current_dir().unwrap());
///
/// platform.env_mut().set_var("TEST_VAR", "value");
/// let var = platform.env().var("TEST_VAR");
/// assert_eq!(var, Some("value"));
/// ```
pub struct EnvImpl {
    pub(crate) args: Vec<String>,
    pub(crate) vars: Vec<(String, String)>,
    pub(crate) current_dir: PathBuf,
}

/// Type alias for an iterator over environment variables as key-value pairs.
pub type EnvVars<'a> = std::slice::Iter<'a, (String, String)>;

/// Type alias for an iterator over command-line arguments.
pub type EnvArgs<'a> = std::slice::Iter<'a, String>;

impl EnvImpl {
    /// Creates a new instance of [`EnvImpl`] with an empty environment
    /// and empty current directory.
    pub fn new() -> Self {
        Self {
            args: Vec::new(),
            vars: Vec::new(),
            current_dir: PathBuf::new(),
        }
    }

    /// Creates a new [`EnvImpl`] instance with the current process
    /// environment variables, program arguments, and current directory
    /// populated from [`std::env`]. Non-UTF-8 variables and arguments
    /// are ignored and an inaccessible current directory defaults to
    /// an empty path.
    pub fn new_from_std() -> Self {
        Self {
            args: std::env::args_os()
                .filter_map(|arg| arg.into_string().ok())
                .collect(),
            vars: std::env::vars_os()
                .filter_map(|(k, v)| k.into_string().ok().zip(v.into_string().ok()))
                .collect(),
            current_dir: std::env::current_dir().unwrap_or_default(),
        }
    }
}

impl Env for EnvImpl {
    /// Returns an iterator over the arguments as strings.
    ///
    /// See [`Self::args`].
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::{Env, Platform};
    ///
    /// let platform = agplatform::platform();
    /// platform.env().args().for_each(|arg| {
    ///     println!("{}", arg);
    /// });
    /// ```
    fn args(&self) -> EnvArgs<'_> {
        self.args.iter()
    }

    /// Returns a reference to the current directory.
    ///
    /// See [`Self::current_dir`].
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::{Env, Platform};
    ///
    /// let platform = agplatform::platform();
    /// let current_dir = platform.env().current_dir();
    /// assert_eq!(current_dir, std::env::current_dir().unwrap());
    /// ```
    fn current_dir(&self) -> &Path {
        &self.current_dir
    }

    /// Removes the environment variable with the given key and returns
    /// its value if it existed.
    ///
    /// See [`Self::remove_var`].
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::{Env, Platform};
    ///
    /// let mut platform = agplatform::platform();
    /// platform.env_mut().set_var("TEST_VAR", "value");
    /// let removed_var = platform.env_mut().remove_var("TEST_VAR");
    /// assert_eq!(removed_var, Some("value".to_string()));
    /// ```
    fn remove_var<T: AsRef<str>>(&mut self, key: T) -> Option<String> {
        let key = key.as_ref();

        if let Some(pos) = self.vars.iter().position(|(k, _)| k == key) {
            Some(self.vars.remove(pos).1)
        } else {
            None
        }
    }

    /// Sets the current directory and returns the previous current
    /// directory.
    ///
    /// See [`Self::set_current_dir`].
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::{Env, Platform};
    ///
    /// let mut platform = agplatform::platform();
    /// let old_dir = platform.env_mut().set_current_dir("/new/path");
    /// assert_eq!(old_dir, std::env::current_dir().unwrap_or_default());
    /// let current_dir = platform.env().current_dir();
    /// assert_eq!(current_dir, std::path::Path::new("/new/path"));
    /// ```
    fn set_current_dir<P: Into<PathBuf>>(&mut self, path: P) -> PathBuf {
        std::mem::replace(&mut self.current_dir, path.into())
    }

    /// Sets the environment variable with the given key and value,
    /// returning the previous value if it existed.
    ///
    /// See [`Self::set_var`].
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::{Env, Platform};
    ///
    /// let mut platform = agplatform::platform();
    /// let old_var = platform.env_mut().set_var("TEST_VAR", "value");
    /// assert_eq!(old_var, None);
    /// let var = platform.env().var("TEST_VAR");
    /// assert_eq!(var, Some("value"));
    /// ```
    fn set_var<T: Into<String>, U: Into<String>>(&mut self, key: T, value: U) -> Option<String> {
        let key = key.into();
        let value = value.into();

        if let Some((_, v)) = self.vars.iter_mut().find(|(k, _)| k == &key) {
            Some(std::mem::replace(v, value))
        } else {
            self.vars.push((key, value));
            None
        }
    }

    /// Returns the value of the environment variable with the given
    /// key if it exists.
    ///
    /// See [`Self::var`].
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::{Env, Platform};
    ///
    /// let platform = agplatform::platform();
    /// if let Some(path) = platform.env().var("PATH") {
    ///     println!("PATH: {}", path);
    /// }
    /// ```
    fn var<T: AsRef<str>>(&self, key: T) -> Option<&str> {
        let key = key.as_ref();
        self.vars
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Returns an iterator over the environment variables as key-value
    /// pairs.
    ///
    /// See [`Self::vars`].
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::{Env, Platform};
    ///
    /// let platform = agplatform::platform();
    /// platform.env().vars().for_each(|(key, value)| {
    ///     println!("{}: {}", key, value);
    /// });
    /// ```
    fn vars(&self) -> EnvVars<'_> {
        self.vars.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args() {
        let env = EnvImpl::new_from_std();
        assert!(
            env.args().len() > 0,
            "Expected at least one command-line argument"
        );
    }

    #[test]
    fn current_dir() {
        let mut env = EnvImpl::new_from_std();

        let current_dir = env.current_dir();
        let expected = std::env::current_dir().unwrap_or_default();
        assert_eq!(current_dir, expected.as_path());

        let new_dir = PathBuf::from("/some/path");
        env.set_current_dir(new_dir.clone());
        let current_dir = env.current_dir();
        assert_eq!(current_dir, new_dir.as_path());
    }

    #[test]
    fn env_var() {
        const KEY: &str = "TEST_VAR";
        let mut env = EnvImpl::new_from_std();

        assert_eq!(env.var(KEY), None);

        assert_eq!(env.set_var(KEY, "value"), None);
        assert_eq!(env.var(KEY), Some("value"));
        assert_eq!(
            env.vars().find(|(k, _)| k == KEY),
            Some(&(KEY.to_string(), "value".to_string()))
        );

        assert_eq!(env.set_var(KEY, "value2"), Some("value".to_string()));
        assert_eq!(env.var(KEY), Some("value2"));
        assert_eq!(
            env.vars().find(|(k, _)| k == KEY),
            Some(&(KEY.to_string(), "value2".to_string()))
        );

        assert_eq!(env.remove_var(KEY), Some("value2".to_string()));
        assert_eq!(env.var(KEY), None);
        assert_eq!(env.vars().find(|(k, _)| k == KEY), None);

        assert_eq!(env.remove_var(KEY), None);
    }
}
