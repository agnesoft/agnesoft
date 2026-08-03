use crate::Env;
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
#[derive(Default)]
pub struct TestEnv {
    envs: std::collections::BTreeMap<String, String>,
}

impl TestEnv {
    /// Creates a new instance of the `TestEnv` struct
    /// that implements the `Env` trait via the mock
    /// implementations.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Env for TestEnv {
    fn remove_var<T: AsRef<str>>(&mut self, key: T) -> Option<String> {
        self.envs.remove(key.as_ref())
    }

    fn set_var<T: AsRef<str>, U: AsRef<str>>(&mut self, key: T, value: U) -> Option<String> {
        self.envs
            .insert(key.as_ref().to_string(), value.as_ref().to_string())
    }

    fn var<T: AsRef<str>>(&self, key: T) -> Option<String> {
        self.envs.get(key.as_ref()).cloned()
    }

    fn vars(&self) -> EnvVars {
        EnvVars(self.envs.clone().into_iter().into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn var() {
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
