#[macro_use]
mod error;
mod env;
#[macro_use]
mod fs;
mod platform;
mod process;
mod request;
#[cfg(feature = "testing")]
pub mod test_platform;
#[cfg(any(test, feature = "testing"))]
mod testing;
mod utils;

pub use env::Env;
pub use env::EnvArgs;
pub use env::EnvVars;
pub use error::Error;
pub use error::ErrorKind;
pub use fs::DirEntry;
pub use fs::FileType;
pub use fs::Fs;
pub use fs::Metadata;
pub use fs::OpenWriteOptions;
pub use fs::ReadFile;
pub use fs::WriteFile;
pub use platform::Platform;
pub use process::Process;
pub use request::Request;
#[cfg(feature = "testing")]
pub use test_platform::{
    test_env::TestEnv, test_fs::TestFs, test_process::TestProcess, test_request::TestRequest,
};
#[cfg(any(test, feature = "testing"))]
pub use testing::{TestDir, TestFile};

pub type Result<T = ()> = std::result::Result<T, Error>;

/// Returns an opaque default platform implementation.
pub fn platform() -> impl platform::Platform {
    let env = env::EnvImpl::new_from_std();
    let fs = fs::FsImpl::new(env.current_dir.clone());
    let process = process::ProcessImpl::new(env.current_dir.clone());

    platform::PlatformImpl {
        env,
        fs,
        process,
        request: request::RequestImpl,
    }
}
