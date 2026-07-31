use crate::Env;
use crate::Result;

/// A mock implementation of the `Env` trait that
/// uses memory-only data structures to simulate the
/// environment in which the program runs in. It allows
/// setting and getting env vars, current directory
/// and simulating errors.
pub struct TestEnv;

impl Env for TestEnv {
    fn var<T: AsRef<str>>(&self, _key: T) -> Result<Option<String>> {
        todo!()
    }
}
