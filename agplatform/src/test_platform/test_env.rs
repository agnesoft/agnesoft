use crate::Env;
use crate::Result;

/// A mock implementation of the `Env` trait that
/// uses memory-only data structures to simulate the
/// environment in which the program runs in. It allows
/// setting and getting env vars, current directory
/// and simulating errors.
///
/// Example:
///
/// ```rust
/// use agplatform::{Platform, Env};
///
/// let mut platform = agplatform::test_platform::test_platform();
/// platform.env_mut().set_var("TEST_VAR", "value").unwrap();
/// let value = platform.env().var("TEST_VAR").unwrap();
/// assert_eq!(value, Some("value".to_string()));
/// ```
#[derive(Default)]
pub struct TestEnv {
    envs: std::collections::BTreeMap<String, Result<Option<String>>>,
}

impl TestEnv {
    /// Creates a new instance of the `TestEnv` struct
    /// that implements the `Env` trait via the mock
    /// implementations.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the environment variable with the given `key`
    /// to the given `Result<Option<String>>`. This allows
    /// simulating errors. If the key already exists,
    /// the previous value is overwritten.
    ///
    /// ```rust
    /// use agplatform::test_platform::TestPlatform;
    /// use agplatform::{Platform, Env};
    ///
    ///
    /// let mut platform = agplatform::test_platform::test_platform();
    /// platform.env.set("TEST_VAR", Err(std::env::VarError::NotPresent.into()));
    /// let err = platform.env().var("TEST_VAR").unwrap_err();
    /// assert_eq!(err.kind(), agplatform::ErrorKind::Env);
    /// ```
    pub fn set<K: AsRef<str>>(&mut self, key: K, value: Result<Option<String>>) {
        self.envs.insert(key.as_ref().to_string(), value);
    }
}

impl Env for TestEnv {
    fn remove_var<T: AsRef<str>>(&mut self, key: T) -> Result<Option<String>> {
        self.envs.remove(key.as_ref()).unwrap_or_else(|| Ok(None))
    }

    fn set_var<T: AsRef<str>, U: AsRef<str>>(
        &mut self,
        key: T,
        value: U,
    ) -> Result<Option<String>> {
        self.envs
            .insert(
                key.as_ref().to_string(),
                Ok(Some(value.as_ref().to_string())),
            )
            .unwrap_or_else(|| Ok(None))
    }

    fn var<T: AsRef<str>>(&self, key: T) -> Result<Option<String>> {
        self.envs
            .get(key.as_ref())
            .cloned()
            .unwrap_or_else(|| Ok(None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
    use crate::ErrorKind;

    #[test]
    fn var() {
        let mut env = TestEnv::new();

        const KEY: &str = "TEST_VAR";

        // Get non-existent env var
        let value = env.var(KEY).unwrap();
        assert_eq!(value, None);

        // Set regular env var
        env.set_var(KEY, "value").unwrap();
        let value = env.var(KEY).unwrap();
        assert_eq!(value, Some("value".to_string()));

        // Overwrite regular env var
        env.set_var(KEY, "value2").unwrap();
        let value = env.var(KEY).unwrap();
        assert_eq!(value, Some("value2".to_string()));

        // Remove env var
        let removed_value = env.remove_var(KEY).unwrap();
        assert_eq!(removed_value, Some("value2".to_string()));

        // Remove removed env var
        let removed_value = env.remove_var(KEY).unwrap();
        assert_eq!(removed_value, None);

        // Get non-existent env var
        let value = env.var(KEY).unwrap();
        assert_eq!(value, None);

        // Set env var with error
        env.set(KEY, Err(Error::env("Simulated error")));
        let err = env.var(KEY).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Env);
        assert_eq!(err.description(), "Simulated error");

        // Set env var with error
        env.set(KEY, Err(Error::env("Another simulated error")));
        let err = env.var(KEY).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Env);
        assert_eq!(err.description(), "Another simulated error");
    }
}
