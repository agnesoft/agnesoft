#![cfg_attr(docsrs, feature(doc_cfg))]

mod error;
#[cfg(feature = "env")]
#[macro_use]
mod env;
#[cfg(feature = "fs")]
#[macro_use]
mod fs;
#[cfg(feature = "interceptor")]
pub mod interceptor;
#[cfg(feature = "env")]
mod platform;
#[cfg(feature = "process")]
#[macro_use]
mod process;
#[cfg(feature = "request")]
#[macro_use]
mod request;
#[cfg(all(feature = "testing", feature = "env"))]
pub mod test_platform;
#[cfg(any(test, feature = "testing"))]
mod testing;
#[cfg(feature = "env")]
mod utils;

#[cfg(feature = "env")]
pub use env::{Env, EnvArgs, EnvVars};
pub use error::Error;
pub use error::ErrorKind;
#[cfg(feature = "fs")]
pub use fs::{DirEntry, FileType, Fs, Metadata, OpenWriteOptions, ReadFile, WriteFile};
#[cfg(feature = "interceptor")]
pub use interceptor::{BoxFuture, DryRunOutput};
#[cfg(feature = "env")]
pub use platform::Platform;
#[cfg(feature = "process")]
pub use process::{
    CommandCache, CommandExt, CommandOptions, CommandOutput, OutputStream, Process, ProcessCommand,
    StdStream,
};
#[cfg(all(feature = "request", feature = "tokio"))]
pub use request::RequestBuilderExt;
#[cfg(feature = "request")]
pub use request::{
    BlockingRequestBuilderExt, ConfiguredRequest, HttpRequest, Request, RequestCache,
    RequestOptions, RequestOutput, TransportError, TransportErrorKind,
};
#[cfg(feature = "request")]
pub use reqwest;
#[cfg(all(feature = "testing", feature = "env"))]
pub use test_platform::test_env::TestEnv;
#[cfg(all(feature = "testing", feature = "fs"))]
pub use test_platform::test_fs::TestFs;
#[cfg(all(feature = "testing", feature = "process"))]
pub use test_platform::test_process::{TestCall, TestProcess, TestProcessGate};
#[cfg(all(feature = "testing", feature = "request"))]
pub use test_platform::test_request::{TestGate, TestRequest, TestRequestCall};
#[cfg(any(test, feature = "testing"))]
pub use testing::{TestDir, TestFile};

pub type Result<T = ()> = std::result::Result<T, Error>;

/// Enabled by the `env` feature flag.
///
/// Returns an opaque default platform implementation.
#[cfg(feature = "env")]
pub fn platform() -> impl platform::Platform {
    let env = env::EnvImpl::new_from_std();
    #[cfg(feature = "fs")]
    let fs = fs::FsImpl::with_env(env.clone());
    #[cfg(feature = "process")]
    let process = process::ProcessImpl::with_env(env.clone());
    #[cfg(feature = "request")]
    let request = request::RequestImpl::with_env(env.clone());

    platform::PlatformImpl {
        env,
        #[cfg(feature = "fs")]
        fs,
        #[cfg(feature = "process")]
        process,
        #[cfg(feature = "request")]
        request,
    }
}
