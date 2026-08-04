use crate::Env;
use crate::Result;
use crate::env::EnvVars;

/// A mock implementation of the `Env` trait that
/// uses memory-only data structures to simulate the
/// environment in which the program runs in. It allows
/// setting and getting env vars and current directory.
///
/// Example:
///
/// ```rust
/// use agplatform::{Platform, Env};
///
/// let mut platform = agplatform::test_platform::test_platform();
/// platform.env_mut().set_var("TEST_VAR", "value");
/// let value = platform.env().var("TEST_VAR");
/// assert_eq!(value, Some("value".to_string()));
/// ```
pub struct TestEnv {
    pub vars: std::collections::BTreeMap<String, String>,
    pub current_dir: Result<std::path::PathBuf>,
    pub set_current_dir: Result<()>,
}

impl TestEnv {
    /// Creates a new instance of the `TestEnv` struct
    /// that implements the `Env` trait via the mock
    /// implementations.
    pub fn new() -> Self {
        Self {
            vars: std::collections::BTreeMap::new(),
            current_dir: Ok(std::path::PathBuf::new()),
            set_current_dir: Ok(()),
        }
    }

    /// Creates a new instance of the `TestEnv` struct
    /// with the given current directory. This is useful
    /// for testing code that depends on the current directory.
    pub fn with_current_dir<P: AsRef<std::path::Path>>(path: P) -> Self {
        Self {
            vars: std::collections::BTreeMap::new(),
            current_dir: Ok(path.as_ref().to_path_buf()),
            set_current_dir: Ok(()),
        }
    }
}

impl Env for TestEnv {
    fn current_dir(&self) -> Result<std::path::PathBuf> {
        self.current_dir.clone()
    }

    fn remove_var<T: AsRef<str>>(&mut self, key: T) -> Option<String> {
        self.vars.remove(key.as_ref())
    }

    fn set_current_dir<P: AsRef<std::path::Path>>(&mut self, path: P) -> Result<()> {
        if let Err(ref e) = self.set_current_dir {
            return Err(e.clone());
        }

        self.current_dir = Ok(path.as_ref().to_path_buf());
        Ok(())
    }

    fn set_var<T: AsRef<str>, U: AsRef<str>>(&mut self, key: T, value: U) -> Option<String> {
        self.vars
            .insert(key.as_ref().to_string(), value.as_ref().to_string())
    }

    fn var<T: AsRef<str>>(&self, key: T) -> Option<String> {
        self.vars.get(key.as_ref()).cloned()
    }

    fn vars(&self) -> EnvVars {
        EnvVars(self.vars.clone().into_iter().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;

    #[test]
    fn current_dir() {
        // Get current dir on default TestEnv (should be empty)
        let current_dir = TestEnv::new().current_dir().unwrap();
        assert_eq!(current_dir, std::path::PathBuf::new());

        let mut env = TestEnv::with_current_dir("/some/path");

        // Get current dir
        let current_dir = env.current_dir().unwrap();
        assert_eq!(current_dir, std::path::PathBuf::from("/some/path"));

        // Set current dir
        let new_dir = std::path::PathBuf::from("/some/other/path");
        env.set_current_dir(&new_dir).unwrap();
        let current_dir = env.current_dir().unwrap();
        assert_eq!(current_dir, new_dir);

        // Set current dir to an error
        let err = Error::io("some error");
        env.set_current_dir = Err(err.clone());
        let result = env.set_current_dir(&new_dir).unwrap_err();
        assert_eq!(result.kind(), err.kind());
        assert_eq!(result.description(), err.description());

        // Get current dir (should still be the previous value)
        let current_dir = env.current_dir().unwrap();
        assert_eq!(current_dir, new_dir);

        // Set current dir to an error
        env.current_dir = Err(err.clone());
        let result = env.current_dir().unwrap_err();
        assert_eq!(result.kind(), err.kind());
        assert_eq!(result.description(), err.description());
    }

    #[test]
    fn vars() {
        let mut env = TestEnv::new();

        const KEY: &str = "TEST_VAR";

        // Get non-existent env var
        let value = env.var(KEY);
        assert_eq!(value, None);

        // Set regular env var
        env.set_var(KEY, "value");
        let value = env.var(KEY);
        assert_eq!(value, Some("value".to_string()));
        assert_eq!(
            env.vars().find(|(k, _)| k == KEY),
            Some((KEY.to_string(), "value".to_string()))
        );

        // Overwrite regular env var
        env.set_var(KEY, "value2");
        let value = env.var(KEY);
        assert_eq!(value, Some("value2".to_string()));
        assert_eq!(
            env.vars().find(|(k, _)| k == KEY),
            Some((KEY.to_string(), "value2".to_string()))
        );

        // Remove env var
        let removed_value = env.remove_var(KEY);
        assert_eq!(removed_value, Some("value2".to_string()));

        // Remove removed env var
        let removed_value = env.remove_var(KEY);
        assert_eq!(removed_value, None);

        // Get non-existent env var
        let value = env.var(KEY);
        assert_eq!(value, None);
    }
}
