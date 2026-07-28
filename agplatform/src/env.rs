/// A trait that represents the environment in
/// which the program runs in. It offers read
/// operations of the environment variables
/// and the current working directory.
pub trait Env {}

pub(crate) struct EnvImpl;

/// Default implementation of the `Env` trait that
/// uses the `std::env` implementations internally.
impl Env for EnvImpl {}
