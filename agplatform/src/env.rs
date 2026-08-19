use std::path::Path;
use std::path::PathBuf;

/// The trait represents an abstract interface to
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
/// as long as it is consistenly used throughout the process
/// with no explicit calls to the std::env module.
///
/// Example:
///
/// ```rust
/// use agplatform::Env;
///
/// let platform = agplatform::platform();
/// let env = platform.env();
/// let current_dir = env.current_dir();
/// assert_eq!(current_dir, std::env::current_dir().unwrap());
///
/// let mut env_mut = platform.env_mut();
/// env_mut.set_var("TEST_VAR", "value");
/// let value = env.var("TEST_VAR");
/// assert_eq!(value, Some("value".to_string()));
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
                .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
                .collect(),
            current_dir: std::env::current_dir().unwrap_or_default(),
        }
    }
}

impl Env for EnvImpl {
    /// Returns a reference to the current directory.
    fn current_dir(&self) -> &Path {
        &self.current_dir
    }

    /// Removes the environment variable with the given key and returns its value if it existed.
    fn remove_var<T: AsRef<str>>(&mut self, key: T) -> Option<String> {
        let key = key.as_ref();

        if let Some(pos) = self.vars.iter().position(|(k, _)| k == key) {
            Some(self.vars.remove(pos).1)
        } else {
            None
        }
    }

    /// Sets the current directory and returns the previous current directory.
    fn set_current_dir<P: Into<PathBuf>>(&mut self, path: P) -> PathBuf {
        std::mem::replace(&mut self.current_dir, path.into())
    }

    /// Sets the environment variable with the given key and value, returning the previous value if it existed.
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
    fn var<T: AsRef<str>>(&self, key: T) -> Option<&str> {
        let key = key.as_ref();
        self.vars
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Returns an iterator over the environment variables as key-value pairs.
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
        assert_eq!(current_dir, std::env::current_dir().unwrap());

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
