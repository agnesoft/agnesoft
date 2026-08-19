use std::path::Path;
use std::path::PathBuf;

/// The `Env` trait representing an abstract interface to
/// a platform implementing environment functionality
/// such as current directory and environment variables.
pub trait Env {
    fn current_dir(&self) -> &Path;
    fn remove_var<T: AsRef<str>>(&mut self, key: T) -> Option<String>;
    fn set_current_dir<P: Into<PathBuf>>(&mut self, path: P) -> PathBuf;
    fn set_var<T: Into<String>, U: Into<String>>(&mut self, key: T, value: U) -> Option<String>;
    fn var<T: AsRef<str>>(&self, key: T) -> Option<&str>;
    fn vars(&self) -> EnvVars<'_>;
}

/// In process implementation of the `Env` providing
/// similar capabilities to the std::env module but with a memory-only
/// implementation that can be used for both production and testing
/// as long as it is consistently used throughout the process
/// with no explicit calls to the std::env module.
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
    vars: Vec<(String, String)>,
    current_dir: PathBuf,
}

/// Type alias for an iterator over environment variables as key-value pairs.
pub type EnvVars<'a> = std::slice::Iter<'a, (String, String)>;

impl EnvImpl {
    /// Creates a new instance of the `Env` struct with an empty environment and
    /// empty current directory.
    pub fn new() -> Self {
        Self {
            vars: Vec::new(),
            current_dir: PathBuf::new(),
        }
    }

    /// Creates a new Env instance with the current process environment variables and current directory
    /// populated from std::env. Non-UTF-8 variables are ignored and inaccessible current directory is defaulted
    /// to an empty path.
    pub fn new_from_std() -> Self {
        Self {
            vars: std::env::vars_os()
                .filter_map(|(k, v)| k.into_string().ok().zip(v.into_string().ok()))
                .collect(),
            current_dir: std::env::current_dir().unwrap_or_default(),
        }
    }
}

impl Env for EnvImpl {
    /// Returns a reference to the current directory.
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

    /// Removes the environment variable with the given key and returns its value if it existed.
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::{Env, Platform};
    ///
/// let mut platform = agplatform::platform();
/// assert_eq!(platform.env_mut().set_var("TEST_VAR", "value"), None);
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

    /// Sets the current directory and returns the previous current directory.
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::{Env, Platform};
    ///
    /// let mut platform = agplatform::platform();
    /// let old_dir = platform.env_mut().set_current_dir("/new/path");
    /// assert_eq!(platform.env().current_dir(), std::path::Path::new("/new/path"));
    /// ```
    fn set_current_dir<P: Into<PathBuf>>(&mut self, path: P) -> PathBuf {
        std::mem::replace(&mut self.current_dir, path.into())
    }

    /// Sets the environment variable with the given key and value, returning the previous value if it existed.
    ///
    /// Example:
    ///
    /// ```rust
    /// use agplatform::{Env, Platform};
    ///
    /// let mut platform = agplatform::platform();
    /// let old_var = platform.env_mut().set_var("TEST_VAR", "value");
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

    /// Returns the value of the environment variable with the given key if it exists.
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

    /// Returns an iterator over the environment variables as key-value pairs.
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
