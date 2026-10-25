#[macro_use]
mod error;
mod env;
#[macro_use]
mod fs;
mod platform;
#[macro_use]
mod process;
#[cfg(feature = "request")]
#[macro_use]
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
pub use process::CommandExt;
pub use process::CommandOptions;
pub use process::CommandOutput;
pub use process::CommandRun;
pub use process::Process;
pub use process::ProcessObserver;
pub use process::RetryIf;
#[cfg(feature = "request")]
pub use request::{
    BlockingRequestBuilderExt, ConfiguredRequest, Request, RequestObserver, RequestOptions,
    RequestOutput, RequestRetryIf, RequestRun, TransportError, TransportErrorKind,
};
#[cfg(all(feature = "request", feature = "tokio"))]
pub use request::{RequestBuilderExt, RequestCancelled};
#[cfg(feature = "request")]
pub use reqwest;
#[cfg(all(feature = "testing", feature = "request"))]
pub use test_platform::test_request::{TestGate, TestRequest, TestRequestCall};
#[cfg(feature = "testing")]
pub use test_platform::{
    test_env::TestEnv, test_fs::TestFs, test_process::TestCall, test_process::TestProcess,
};
#[cfg(any(test, feature = "testing"))]
pub use testing::{TestDir, TestFile};

pub type Result<T = ()> = std::result::Result<T, Error>;

/// Returns an opaque default platform implementation.
pub fn platform() -> impl platform::Platform {
    let env = env::EnvImpl::new_from_std();
    let fs = fs::FsImpl::new(env.current_dir.clone());
    let process = process::ProcessImpl::new(env.current_dir.clone());
    #[cfg(feature = "request")]
    let request = request::RequestImpl::new().with_env(&env);

    platform::PlatformImpl {
        env,
        fs,
        process,
        #[cfg(feature = "request")]
        request,
    }
}
