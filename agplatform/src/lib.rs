#[macro_use]
mod error;
mod env;
mod fs;
mod platform;
#[cfg(feature = "testing")]
pub mod test_platform;
mod utils;

pub use env::Env;
pub use env::EnvArgs;
pub use env::EnvVars;
pub use error::Error;
pub use error::ErrorKind;
pub use fs::Fs;
pub use platform::Platform;
#[cfg(feature = "testing")]
pub use test_platform::{test_env::TestEnv, test_fs::TestFs};

pub type Result<T = ()> = std::result::Result<T, Error>;

/// Returns an opaque default platform implementation.
pub fn platform() -> impl platform::Platform {
    let env = env::EnvImpl::new_from_std();
    let fs = fs::FsImpl::new(env.current_dir.clone());

    platform::PlatformImpl { env, fs }
}
