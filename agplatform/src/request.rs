use std::any::Any;
use std::borrow::Cow;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use bytes::Bytes;
use reqwest::Method;
use reqwest::Url;
use reqwest::Version;
use reqwest::header::HeaderMap;

use crate::Result;
use crate::env::EnvImpl;
#[cfg(feature = "tokio")]
use crate::interceptor::AsyncExecutor;
use crate::interceptor::BlockingExecutor;
use crate::interceptor::BoxFuture;
use crate::interceptor::Cache;
use crate::interceptor::Captured;
use crate::interceptor::Emit;
use crate::interceptor::Executor;
use crate::interceptor::Interceptor;
use crate::interceptor::Interceptors;
use crate::interceptor::Outcome;
use crate::interceptor::Retry;
use crate::interceptor::Run;
use crate::interceptor::Sink;
use crate::interceptor::Subject;
use crate::utils::OwnInterceptors;
use crate::utils::Runs;
use crate::utils::conclude;

macro_rules! request_error {
    ($($arg:tt)*) => {
        $crate::Error::request(format!($($arg)*))
    };
}

mod clients;

pub(crate) use clients::DefaultClients;
#[cfg(test)]
pub(crate) use clients::ReqwestEnv;

/// Enabled by the `request` feature flag.
///
/// The [`Cache`] of the successful responses of the `GET` and `HEAD`
/// requests by their [`HttpRequest::cache_key`], the
/// [default](Self::default) one, or by another key with [`Cache::new`].
/// The key has no headers, so a response is returned whatever the
/// credentials or the content negotiation (e.g. `Accept`) of the request,
/// use a cache per credentials, or a key with the headers the responses
/// depend on, when they do.
///
/// # Examples
///
/// ```rust
/// use agplatform::{BlockingRequestBuilderExt, Request, RequestCache, RequestOutput, TestRequest};
///
/// let cache = RequestCache::default();
/// let request = TestRequest::new();
/// request.add_interceptor(cache.clone());
/// request.set_response("", RequestOutput { status: 200, body: b"manifest".to_vec(), ..Default::default() });
/// let client = request.blocking_client().unwrap();
///
/// client.get("https://registry.example.com/v2/app/manifests/latest").send_blocking(&request).unwrap();
/// let output = client.get("https://registry.example.com/v2/app/manifests/latest").send_blocking(&request).unwrap();
///
/// assert_eq!(output.body, b"manifest");
/// assert_eq!(request.calls().len(), 1);
/// assert_eq!(cache.entries().len(), 1);
/// ```
pub type RequestCache = Cache<HttpRequest>;

/// Enabled by the `request` feature flag.
///
/// Abstract network request interface.
///
/// Sends HTTP requests described by [`reqwest`] request builders and
/// receives their responses. Usually used through [`RequestBuilderExt`]
/// and [`BlockingRequestBuilderExt`] rather than directly.
///
/// Requests are sent by the [`reqwest`] client that built them: a default
/// one shared by all requests from [`client`](Self::client) (or
/// [`blocking_client`](Self::blocking_client)), configured with
/// [`set_client`](Self::set_client), or one with its own configuration
/// built from [`client_builder`](Self::client_builder), e.g. with a
/// proxy, a cookie store or extra root certificates, which reqwest only
/// supports per client. TLS needs the `tls` (rustls) or `native-tls` feature.
///
/// Proxies are configured from the platform environment by the rules of
/// hyper-util, which reqwest uses: `HTTP_PROXY`, `HTTPS_PROXY` and
/// `ALL_PROXY` (or lowercase) with the exceptions in `NO_PROXY`, none when
/// `REQUEST_METHOD` is set (CGI). Its changes, e.g. by
/// [`Env::set_var`](crate::Env::set_var) of the platform, apply to the
/// default clients and the builders from
/// [`client_builder`](Self::client_builder). A plain
/// [`reqwest::ClientBuilder`] reads the process environment instead, as do
/// the TLS backends (e.g. `SSL_CERT_FILE`) always.
///
/// Each request runs through the interceptors of the request, none by
/// default (see [`add_interceptor`](Self::add_interceptor)), or its own,
/// see [`ConfiguredRequest::interceptors`], as an [`HttpRequest`].
///
/// The two provided implementations are `RequestImpl` (delegates to
/// [`reqwest`]) and
/// [`TestRequest`](crate::test_platform::test_request::TestRequest)
/// (purely in-memory, behind the `testing` feature).
///
/// Requests are [`Sync`] so that the futures of
/// [`send_async`](Self::send_async) borrowing them are [`Send`].
pub trait Request: Send + Sync {
    /// Returns the default blocking client, built from the
    /// [`set_blocking_client`](Self::set_blocking_client) builder (the
    /// [`blocking_client_builder`](Self::blocking_client_builder) otherwise)
    /// on the first call, and again after the proxy variables change. The
    /// client is a cheap handle sharing the connection pool.
    ///
    /// The blocking client runs its own runtime thread, so it must not be
    /// built or used in an async context. Dropping it there is fine.
    ///
    /// # Errors
    ///
    /// Returns an error if the client cannot be built, every time.
    fn blocking_client(&self) -> Result<reqwest::blocking::Client>;

    /// Returns a blocking client builder with the proxies of the platform
    /// environment, see [`Request`]. Call
    /// [`no_proxy`](reqwest::blocking::ClientBuilder::no_proxy) on it before
    /// adding other proxies, as reqwest uses the first matching one.
    ///
    /// The default blocking client is built from it too. Connecting times
    /// out after 30 seconds, and the response after 5 minutes without data,
    /// instead of after 30 seconds in total like with a plain
    /// [`reqwest::blocking::ClientBuilder`]. The 5 minutes are its
    /// [`timeout`](reqwest::blocking::ClientBuilder::timeout), which limits
    /// sending the request until its response head arrives, and each read
    /// of the response body by [`send_blocking`](Self::send_blocking) or
    /// [`Read`](std::io::Read), but the whole body read by
    /// [`bytes`](reqwest::blocking::Response::bytes) or
    /// [`text`](reqwest::blocking::Response::text). The
    /// [`timeout`](reqwest::blocking::RequestBuilder::timeout) of a request
    /// limits all of it instead.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Platform, Request};
    ///
    /// let platform = agplatform::platform();
    /// let request = platform.request();
    /// let client = request.blocking_client_builder().user_agent("agent").build().unwrap();
    /// ```
    fn blocking_client_builder(&self) -> reqwest::blocking::ClientBuilder;

    /// Sets the builder of the default blocking client, replacing the
    /// previous client. The client is built by the next
    /// [`blocking_client`](Self::blocking_client) call.
    fn set_blocking_client(&self, builder: reqwest::blocking::ClientBuilder);

    /// Sends `request`, a request builder with its [`RequestOptions`], with
    /// the client that built it and returns the response. A plain builder
    /// is sent with the default options, a [`ConfiguredRequest`] (e.g. from
    /// [`BlockingRequestBuilderExt`]) with its own.
    ///
    /// A streamed request body (e.g. a reader) is read into memory first
    /// so that it can be passed to the interceptors and retried. The whole
    /// response body is read too. Each run passes through the interceptors,
    /// see [`HttpRequest`], unless the request cannot be built.
    ///
    /// # Errors
    ///
    /// The errors name only the method and the URL without its
    /// credentials, query and fragment, as they may hold secrets. An
    /// [`Interceptor`] gets the whole request, e.g. to log it redacted.
    ///
    /// Returns an error if:
    /// - The request cannot be built, e.g. its URL is invalid, or its body
    ///   cannot be read. It does not pass through the interceptors.
    /// - It cannot be sent or its response received, e.g. the connection
    ///   fails or times out. Its [`TransportError`] is returned by
    ///   [`Error::source_as`](crate::Error::source_as).
    /// - The response is not a [success](RequestOutput::success), e.g. its
    ///   status is not 2xx. Its [`RequestOutput`] is returned by
    ///   [`Error::request_output`](crate::Error::request_output).
    /// - An interceptor returns an error.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{BlockingRequestBuilderExt, ConfiguredRequest, Platform, Request, RequestOptions, RequestOutput};
    ///
    /// let platform = agplatform::platform();
    /// let request = platform.request();
    /// # let request = agplatform::TestRequest::new();
    /// # request.set_response("", RequestOutput { status: 200, body: b"pong".to_vec(), ..Default::default() });
    ///
    /// let client = request.blocking_client().unwrap();
    /// let output = request.send_blocking(client.get("https://example.com/ping")).unwrap();
    /// assert_eq!(output.text().unwrap(), "pong");
    ///
    /// request.send_blocking(client.get("https://example.com/ping").display_lines(3)).unwrap();
    /// let options = RequestOptions { keep_partial_body: true, ..Default::default() };
    /// request
    ///     .send_blocking(ConfiguredRequest::with_options(client.get("https://example.com/ping"), options))
    ///     .unwrap();
    /// ```
    fn send_blocking(
        &self,
        request: impl Into<ConfiguredRequest<reqwest::blocking::RequestBuilder>>,
    ) -> Result<RequestOutput>;

    /// Sends `request` like [`Self::send_blocking`], passing the body of a
    /// successful (2xx) response to `sink` as it arrives instead of
    /// returning it in [`RequestOutput::body`], e.g. to write a large
    /// download to a file. The body of a failed response is returned as
    /// usual.
    ///
    /// The body passes through the interceptors before, see
    /// [`Interceptor::chunk`]. An attempt failing after a part of the body
    /// was passed to `sink` is not retried by [`Retry`], as `sink` would get
    /// the body again from its start, see [`Run::streamed`]. A caller can
    /// resume it instead, e.g. with a `Range` header and the response head
    /// from [`TransportError::output`].
    ///
    /// # Errors
    ///
    /// As [`Self::send_blocking`], or the error returned by `sink`, which
    /// ends the run.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Platform, Request, RequestOptions, RequestOutput};
    ///
    /// let platform = agplatform::platform();
    /// let request = platform.request();
    /// # let request = agplatform::TestRequest::new();
    /// # request.set_response("", RequestOutput { status: 200, body: b"data".to_vec(), ..Default::default() });
    /// let mut file = Vec::new();
    ///
    /// let client = request.blocking_client().unwrap();
    /// let output = request
    ///     .send_blocking_to(client.get("https://example.com/download"), &mut |chunk| {
    ///         file.extend_from_slice(chunk);
    ///         Ok(())
    ///     })
    ///     .unwrap();
    /// assert!(output.body.is_empty());
    /// assert_eq!(file, b"data");
    /// ```
    fn send_blocking_to(
        &self,
        request: impl Into<ConfiguredRequest<reqwest::blocking::RequestBuilder>>,
        sink: &mut (dyn FnMut(&[u8]) -> Result + Send),
    ) -> Result<RequestOutput>;

    /// Enabled by the `tokio` feature flag.
    ///
    /// Returns the default client, built from the
    /// [`set_client`](Self::set_client) builder (the
    /// [`client_builder`](Self::client_builder) otherwise) on the first
    /// call, and again after the proxy variables change. The client is a
    /// cheap handle sharing the connection pool.
    ///
    /// # Errors
    ///
    /// Returns an error if the client cannot be built, every time.
    #[cfg(feature = "tokio")]
    fn client(&self) -> Result<reqwest::Client>;

    /// Enabled by the `tokio` feature flag.
    ///
    /// Returns a client builder with the proxies of the platform
    /// environment, see [`Request`]. Call
    /// [`no_proxy`](reqwest::ClientBuilder::no_proxy) on it before adding
    /// other proxies, as reqwest uses the first matching one.
    ///
    /// The default client is built from it too. Connecting times out after
    /// 30 seconds and each read of the response after 5 minutes of
    /// silence, while a slow response is never cut in total. The
    /// [`timeout`](reqwest::RequestBuilder::timeout) of a request limits
    /// all of it in addition.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Platform, Request};
    /// use std::time::Duration;
    ///
    /// let platform = agplatform::platform();
    /// let request = platform.request();
    /// let client = request
    ///     .client_builder()
    ///     .timeout(Duration::from_secs(60))
    ///     .build()
    ///     .unwrap();
    /// ```
    #[cfg(feature = "tokio")]
    fn client_builder(&self) -> reqwest::ClientBuilder;

    /// Enabled by the `tokio` feature flag.
    ///
    /// Sets the builder of the default client, replacing the previous
    /// client. The client is built by the next [`client`](Self::client)
    /// call.
    #[cfg(feature = "tokio")]
    fn set_client(&self, builder: reqwest::ClientBuilder);

    /// Enabled by the `tokio` feature flag.
    ///
    /// Sends `request` like [`Self::send_blocking`], without blocking the current
    /// thread. Must be awaited on a [`tokio`] runtime with I/O and time
    /// enabled. Dropping the future cancels the request, which the
    /// interceptors are told by [`Interceptor::cancelled`].
    ///
    /// # Errors
    ///
    /// As [`Self::send_blocking`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Platform, Request, RequestBuilderExt, RequestOutput};
    ///
    /// # tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
    /// let platform = agplatform::platform();
    /// let request = platform.request();
    /// # let request = agplatform::TestRequest::new();
    /// # request.set_response("", RequestOutput { status: 200, body: b"pong".to_vec(), ..Default::default() });
    ///
    /// let client = request.client().unwrap();
    /// let output = request.send_async(client.get("https://example.com/ping")).await.unwrap();
    /// assert_eq!(output.text().unwrap(), "pong");
    /// # });
    /// ```
    #[cfg(feature = "tokio")]
    fn send_async(
        &self,
        request: impl Into<ConfiguredRequest<reqwest::RequestBuilder>>,
    ) -> impl Future<Output = Result<RequestOutput>> + Send;

    /// Enabled by the `tokio` feature flag.
    ///
    /// Sends `request` like [`Self::send_async`], passing the body of a
    /// successful response to `sink` like [`Self::send_blocking_to`].
    ///
    /// # Errors
    ///
    /// As [`Self::send_blocking_to`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Platform, Request, RequestOptions, RequestOutput};
    ///
    /// # tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
    /// let platform = agplatform::platform();
    /// let request = platform.request();
    /// # let request = agplatform::TestRequest::new();
    /// # request.set_response("", RequestOutput { status: 200, body: b"data".to_vec(), ..Default::default() });
    /// let mut file = Vec::new();
    ///
    /// let client = request.client().unwrap();
    /// request
    ///     .send_async_to(client.get("https://example.com/download"), &mut |chunk| {
    ///         file.extend_from_slice(chunk);
    ///         Ok(())
    ///     })
    ///     .await
    ///     .unwrap();
    /// assert_eq!(file, b"data");
    /// # });
    /// ```
    #[cfg(feature = "tokio")]
    fn send_async_to(
        &self,
        request: impl Into<ConfiguredRequest<reqwest::RequestBuilder>>,
        sink: &mut (dyn FnMut(&[u8]) -> Result + Send),
    ) -> impl Future<Output = Result<RequestOutput>> + Send;

    /// Returns the interceptors of the runs, e.g. to set them with changes
    /// on a run, see [`ConfiguredRequest::interceptors`].
    fn interceptors(&self) -> Interceptors<HttpRequest>;

    /// Removes the interceptors, none by default, for the runs starting
    /// afterwards.
    fn clear_interceptors(&self);

    /// Adds `interceptor` as the innermost one for the runs starting
    /// afterwards. Its state to be read later, e.g. what it recorded, is
    /// shared through an [`Arc`](std::sync::Arc) it holds.
    ///
    /// See [`HttpRequest`].
    fn add_interceptor(&self, interceptor: impl Interceptor<HttpRequest>);
}

/// Enabled by the `request` and `tokio` feature flags.
///
/// Extends [`reqwest::RequestBuilder`] with the ability to send it
/// through a [`Request`].
///
/// # Examples
///
/// ```rust
/// use agplatform::{Platform, Request, RequestBuilderExt, RequestOutput};
///
/// # tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
/// let platform = agplatform::platform();
/// let request = platform.request();
/// # let request = &agplatform::TestRequest::new();
/// # request.set_response("", RequestOutput { status: 200, ..Default::default() });
/// let client = request.client().unwrap();
///
/// client.get("https://example.com").send_async(request).await.unwrap();
/// client
///     .post("https://example.com/upload")
///     .bearer_auth("token")
///     .body("data")
///     .display_lines(3)
///     .send_async(request)
///     .await
///     .unwrap();
/// # });
/// ```
#[cfg(feature = "tokio")]
pub trait RequestBuilderExt: Sized {
    /// Sends the request asynchronously with the default
    /// [`RequestOptions`].
    ///
    /// See [`Request::send_async`].
    fn send_async<R: Request + ?Sized>(
        self,
        request: &R,
    ) -> impl Future<Output = Result<RequestOutput>> + Send;

    /// Sends the request asynchronously with the default
    /// [`RequestOptions`], passing the body of a successful response to
    /// `sink`.
    ///
    /// See [`Request::send_async_to`].
    fn send_async_to<R: Request + ?Sized>(
        self,
        request: &R,
        sink: &mut (dyn FnMut(&[u8]) -> Result + Send),
    ) -> impl Future<Output = Result<RequestOutput>> + Send;

    /// Returns a [`ConfiguredRequest`] whose response shows its last
    /// `lines` when displayed.
    ///
    /// See [`RequestOptions::display_lines`].
    fn display_lines(self, lines: usize) -> ConfiguredRequest<Self>;

    /// Returns a [`ConfiguredRequest`] with its own `interceptors`.
    ///
    /// See [`ConfiguredRequest::interceptors`].
    fn interceptors(self, interceptors: Interceptors<HttpRequest>) -> ConfiguredRequest<Self>;

    /// Returns a [`ConfiguredRequest`] keeping the part of the response
    /// body received when it fails.
    ///
    /// See [`RequestOptions::keep_partial_body`].
    fn keep_partial_body(self) -> ConfiguredRequest<Self>;

    /// Returns a [`ConfiguredRequest`] with `value` for the interceptors.
    ///
    /// See [`ConfiguredRequest::with`].
    fn with<T: Any + Send + Sync>(self, value: T) -> ConfiguredRequest<Self>;
}

/// Enabled by the `request` feature flag.
///
/// Extends [`reqwest::blocking::RequestBuilder`] with the ability to send
/// it through a [`Request`].
///
/// # Examples
///
/// ```rust
/// use agplatform::{BlockingRequestBuilderExt, Platform, Request, RequestOutput};
///
/// let platform = agplatform::platform();
/// let request = platform.request();
/// # let request = &agplatform::TestRequest::new();
/// # request.set_response("", RequestOutput { status: 200, ..Default::default() });
/// let client = request.blocking_client().unwrap();
///
/// client.get("https://example.com").send_blocking(request).unwrap();
/// client
///     .get("https://example.com/download")
///     .keep_partial_body()
///     .send_blocking(request)
///     .unwrap();
/// ```
pub trait BlockingRequestBuilderExt: Sized {
    /// Sends the request with the default [`RequestOptions`].
    ///
    /// See [`Request::send_blocking`].
    fn send_blocking<R: Request + ?Sized>(self, request: &R) -> Result<RequestOutput>;

    /// Sends the request with the default [`RequestOptions`], passing the
    /// body of a successful response to `sink`.
    ///
    /// See [`Request::send_blocking_to`].
    fn send_blocking_to<R: Request + ?Sized>(
        self,
        request: &R,
        sink: &mut (dyn FnMut(&[u8]) -> Result + Send),
    ) -> Result<RequestOutput>;

    /// Returns a [`ConfiguredRequest`] whose response shows its last
    /// `lines` when displayed.
    ///
    /// See [`RequestOptions::display_lines`].
    fn display_lines(self, lines: usize) -> ConfiguredRequest<Self>;

    /// Returns a [`ConfiguredRequest`] with its own `interceptors`.
    ///
    /// See [`ConfiguredRequest::interceptors`].
    fn interceptors(self, interceptors: Interceptors<HttpRequest>) -> ConfiguredRequest<Self>;

    /// Returns a [`ConfiguredRequest`] keeping the part of the response
    /// body received when it fails.
    ///
    /// See [`RequestOptions::keep_partial_body`].
    fn keep_partial_body(self) -> ConfiguredRequest<Self>;

    /// Returns a [`ConfiguredRequest`] with `value` for the interceptors.
    ///
    /// See [`ConfiguredRequest::with`].
    fn with<T: Any + Send + Sync>(self, value: T) -> ConfiguredRequest<Self>;
}

/// Enabled by the `request` feature flag.
///
/// A [`reqwest`] request builder `B` to be sent with [`RequestOptions`].
///
/// Usually obtained from [`RequestBuilderExt`] or
/// [`BlockingRequestBuilderExt`].
pub struct ConfiguredRequest<B> {
    builder: B,
    options: RequestOptions,
    /// The values for the interceptors, set on the run once it is built.
    values: Vec<SetValue>,
}

/// [internal]
///
/// Sets a value of [`ConfiguredRequest::with`] on the run.
type SetValue = Box<dyn FnOnce(&mut Run<HttpRequest>) + Send>;

/// Enabled by the `request` feature flag.
///
/// A request sent by a [`Request`], the [`Subject`] of its runs through the
/// interceptors, see [`Run::subject`]. Its body is read into memory.
///
/// The [`Interceptor`]s of a request get its [`Run`] and may change it,
/// e.g. add a header, before passing it on, or retry it, e.g. with
/// [`Self::retry`], or replace its response, e.g. with
/// [`DryRun`](crate::interceptor::DryRun) or [`RequestCache`]. The chunks
/// of the response body pass through [`Interceptor::chunk`] as they arrive,
/// as received rather than split into lines, before they are passed to the
/// sink of [`Request::send_blocking_to`] when the response is a success.
///
/// The output of a run is a [success](RequestOutput::success) when its
/// status is (2xx), which an interceptor may change, e.g. to accept the
/// status `404` of a lookup of something that may not exist. Inside the
/// chain an output that is not a success is returned as `Ok`, the methods
/// of [`Request`] return it as an error.
///
/// Each attempt passes the run through the rest of the chain again with the
/// changes made to it before, so an interceptor should replace what it
/// sets, e.g. [`HeaderMap::insert`] a header rather than
/// [`append`](HeaderMap::append) it. The headers added are seen by all
/// interceptors, mark the values of secrets
/// [sensitive](reqwest::header::HeaderValue::set_sensitive) so that their
/// [`Debug`] hides them.
///
/// # Examples
///
/// ```rust
/// use agplatform::interceptor::{BoxFuture, Interceptor, Next};
/// use agplatform::{BlockingRequestBuilderExt, HttpRequest, Request, RequestOutput, Result};
/// use std::sync::{Arc, Mutex};
///
/// #[derive(Clone, Default)]
/// struct Failures(Arc<Mutex<Vec<String>>>);
///
/// impl Interceptor<HttpRequest> for Failures {
///     fn handle<'a>(&'a self, mut next: Next<'a, HttpRequest>) -> BoxFuture<'a, Result<RequestOutput>> {
///         Box::pin(async move {
///             let result = next.attempt().await;
///
///             if let Ok(output) = &result
///                 && !output.success
///             {
///                 let target = next.run().subject().target();
///                 self.0.lock().unwrap().push(format!("{target} {}", output.status));
///             }
///
///             result
///         })
///     }
/// }
///
/// let failures = Failures::default();
/// let request = agplatform::TestRequest::new();
/// request.add_interceptor(failures.clone());
/// request.set_response("", RequestOutput { status: 404, ..Default::default() });
///
/// let client = request.blocking_client().unwrap();
/// let _ = client.get("https://example.com/missing?token=secret").send_blocking(&request);
///
/// assert_eq!(*failures.0.lock().unwrap(), ["GET https://example.com/missing 404"]);
/// ```
pub struct HttpRequest {
    method: Method,
    url: Url,
    headers: HeaderMap,
    version: Version,
    timeout: Option<Duration>,
    body: Option<Bytes>,
    options: RequestOptions,
}

/// Enabled by the `request` feature flag.
///
/// Options for [`Request::send_blocking`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestOptions {
    /// The number of the last lines of the response body shown when
    /// displaying the [`RequestOutput`], e.g. as the source of an error, at
    /// most as many KiB of them. 10 by default.
    pub display_lines: usize,

    /// Keeps the part of the response body received, as the interceptors
    /// passed it on, in the [`TransportError::output`] of a response that
    /// fails to be received, e.g. to resume a download. Its body is not
    /// kept when it goes to a sink, see [`Request::send_blocking_to`].
    pub keep_partial_body: bool,
}

/// Enabled by the `request` feature flag.
///
/// The response to a request.
///
/// Returned from [`Request::send_blocking`] on success and by
/// [`Error::request_output`](crate::Error::request_output) of its error
/// when the status is not a success.
///
/// Outputs are equal when they differ at most in their
/// [`success`](Self::success), which is decided by the run, and their
/// [`display_lines`](Self::display_lines), which only affects how they are
/// displayed. The [default](Self::default) one is an empty `200`, e.g.
/// returned by a [`DryRun`](crate::interceptor::DryRun).
///
/// Its [`Debug`] shows only the length of the body and its start, as the
/// body may be large, e.g. in a `{:?}` of an error.
#[derive(Clone)]
pub struct RequestOutput {
    /// Whether the request succeeded. Set by the run for each attempt to
    /// whether the status is a success (2xx), then may be changed by the
    /// interceptors, see [`HttpRequest`].
    pub success: bool,

    /// The status code.
    pub status: u16,

    /// The response headers.
    pub headers: HeaderMap,

    /// The whole response body as passed on by the interceptors, empty
    /// when passed to a sink, see [`Request::send_blocking_to`].
    pub body: Vec<u8>,

    /// The wall time from sending the request until the whole body was
    /// received.
    pub duration: Duration,

    /// The number of the last lines of the body shown when displayed, see
    /// [`RequestOptions::display_lines`]. 10 by default.
    pub display_lines: usize,
}

/// Enabled by the `request` feature flag.
///
/// The failure to send a request or to receive its response, the source
/// of the error returned from [`Request::send_blocking`].
///
/// # Examples
///
/// ```rust
/// use agplatform::{BlockingRequestBuilderExt, Request, TestRequest, TransportError, TransportErrorKind};
///
/// let request = TestRequest::new();
/// request.set_transport_error("", TransportErrorKind::Connect);
///
/// let error = request.blocking_client().unwrap().get("https://example.com").send_blocking(&request).unwrap_err();
///
/// assert_eq!(error.source_as::<TransportError>().unwrap().kind(), TransportErrorKind::Connect);
/// ```
#[derive(Debug)]
pub struct TransportError {
    kind: TransportErrorKind,
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
    output: Option<RequestOutput>,
}

/// Enabled by the `request` feature flag.
///
/// Classifies [`TransportError`]s.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportErrorKind {
    /// The connection could not be established (including its timeout), so
    /// the request was not sent and retrying it is always safe.
    Connect,
    /// The request or the response timed out.
    Timeout,
    /// Any other failure, e.g. the connection was closed before the whole
    /// response was received.
    Other,
}

/// [internal]
///
/// Receives the body of a successful response, see
/// [`Request::send_blocking_to`].
pub(crate) type BodySink<'a> = dyn FnMut(&[u8]) -> Result + Send + 'a;

/// [internal]
///
/// The response of an attempt of a run as it arrives, its body passed to
/// `emit`, which passes only the body of a successful response to the
/// sink and captures the rest, kept for a [`TransportError::output`], see
/// [`RequestOptions::keep_partial_body`].
pub(crate) struct Received<'a> {
    subject: &'a HttpRequest,
    emit: Emit<'a, HttpRequest>,
    output: RequestOutput,
}

/// [internal]
///
/// Sends the blocking runs by its client.
struct BlockingClient<'a>(&'a reqwest::blocking::Client);

/// [internal]
///
/// Sends the async runs by its client.
#[cfg(feature = "tokio")]
struct AsyncClient<'a>(&'a reqwest::Client);

/// The production [`Request`] implementation backed by [`reqwest`].
///
/// Normally obtained via [`platform()`](crate::platform) rather than
/// constructed directly.
pub struct RequestImpl {
    clients: DefaultClients,
    runs: Runs<HttpRequest>,
}

#[cfg(feature = "tokio")]
impl RequestBuilderExt for reqwest::RequestBuilder {
    fn send_async<R: Request + ?Sized>(
        self,
        request: &R,
    ) -> impl Future<Output = Result<RequestOutput>> + Send {
        request.send_async(self)
    }

    fn send_async_to<R: Request + ?Sized>(
        self,
        request: &R,
        sink: &mut (dyn FnMut(&[u8]) -> Result + Send),
    ) -> impl Future<Output = Result<RequestOutput>> + Send {
        request.send_async_to(self, sink)
    }

    fn display_lines(self, lines: usize) -> ConfiguredRequest<Self> {
        ConfiguredRequest::new(self).display_lines(lines)
    }

    fn interceptors(self, interceptors: Interceptors<HttpRequest>) -> ConfiguredRequest<Self> {
        ConfiguredRequest::new(self).interceptors(interceptors)
    }

    fn keep_partial_body(self) -> ConfiguredRequest<Self> {
        ConfiguredRequest::new(self).keep_partial_body()
    }

    fn with<T: Any + Send + Sync>(self, value: T) -> ConfiguredRequest<Self> {
        ConfiguredRequest::new(self).with(value)
    }
}

impl BlockingRequestBuilderExt for reqwest::blocking::RequestBuilder {
    fn send_blocking<R: Request + ?Sized>(self, request: &R) -> Result<RequestOutput> {
        request.send_blocking(self)
    }

    fn send_blocking_to<R: Request + ?Sized>(
        self,
        request: &R,
        sink: &mut (dyn FnMut(&[u8]) -> Result + Send),
    ) -> Result<RequestOutput> {
        request.send_blocking_to(self, sink)
    }

    fn display_lines(self, lines: usize) -> ConfiguredRequest<Self> {
        ConfiguredRequest::new(self).display_lines(lines)
    }

    fn interceptors(self, interceptors: Interceptors<HttpRequest>) -> ConfiguredRequest<Self> {
        ConfiguredRequest::new(self).interceptors(interceptors)
    }

    fn keep_partial_body(self) -> ConfiguredRequest<Self> {
        ConfiguredRequest::new(self).keep_partial_body()
    }

    fn with<T: Any + Send + Sync>(self, value: T) -> ConfiguredRequest<Self> {
        ConfiguredRequest::new(self).with(value)
    }
}

impl<B> ConfiguredRequest<B> {
    /// Creates a request of `builder` with the default [`RequestOptions`].
    pub fn new(builder: B) -> Self {
        Self::with_options(builder, RequestOptions::default())
    }

    /// Creates a request of `builder` with `options`, e.g. read from a
    /// configuration.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{ConfiguredRequest, RequestOptions};
    ///
    /// let options = RequestOptions { keep_partial_body: true, ..Default::default() };
    /// let builder = agplatform::reqwest::blocking::Client::new().get("https://example.com");
    /// let request = ConfiguredRequest::with_options(builder, options);
    ///
    /// assert!(request.options().keep_partial_body);
    /// ```
    pub fn with_options(builder: B, options: RequestOptions) -> Self {
        Self {
            builder,
            options,
            values: Vec::new(),
        }
    }

    /// Returns the options.
    pub fn options(&self) -> &RequestOptions {
        &self.options
    }

    /// Shows the last `lines` of the response body when displayed.
    ///
    /// See [`RequestOptions::display_lines`].
    pub fn display_lines(mut self, lines: usize) -> Self {
        self.options.display_lines = lines;
        self
    }

    /// Sends the request with its own `interceptors` instead of those of
    /// the request, see [`Request::interceptors`]. They are taken once as
    /// the run starts, so the interceptors cannot change them.
    pub fn interceptors(self, interceptors: Interceptors<HttpRequest>) -> Self {
        self.with(OwnInterceptors(interceptors))
    }

    /// Keeps the part of the response body received when it fails.
    ///
    /// See [`RequestOptions::keep_partial_body`].
    pub fn keep_partial_body(mut self) -> Self {
        self.options.keep_partial_body = true;
        self
    }

    /// Sets `value` of type `T` on the run for the interceptors, replacing
    /// the previous one, e.g. a
    /// [`DryRunOutput`](crate::interceptor::DryRunOutput) for
    /// [`DryRun`](crate::interceptor::DryRun). Read with [`Run::get`].
    pub fn with<T: Any + Send + Sync>(mut self, value: T) -> Self {
        self.values.push(Box::new(move |run| run.insert(value)));
        self
    }

    /// Returns the run of `subject` with the values of the request.
    fn run(values: Vec<SetValue>, subject: HttpRequest) -> Run<HttpRequest> {
        let mut run = Run::new(subject);
        values.into_iter().for_each(|set| set(&mut run));
        run
    }
}

impl ConfiguredRequest<reqwest::blocking::RequestBuilder> {
    /// Sends the request with the options.
    ///
    /// See [`Request::send_blocking`].
    pub fn send_blocking<R: Request + ?Sized>(self, request: &R) -> Result<RequestOutput> {
        request.send_blocking(self)
    }

    /// Sends the request with the options, passing the body of a
    /// successful response to `sink`.
    ///
    /// See [`Request::send_blocking_to`].
    pub fn send_blocking_to<R: Request + ?Sized>(
        self,
        request: &R,
        sink: &mut (dyn FnMut(&[u8]) -> Result + Send),
    ) -> Result<RequestOutput> {
        request.send_blocking_to(self, sink)
    }

    /// [internal]
    ///
    /// Returns the client of the request and its run, failing when the
    /// request cannot be built or its body cannot be read.
    pub(crate) fn into_run(self) -> (reqwest::blocking::Client, Result<Run<HttpRequest>>) {
        let (client, request) = self.builder.build_split();
        let run = request
            .map_err(invalid_request)
            .and_then(|request| HttpRequest::from_blocking(request, self.options))
            .map(|subject| Self::run(self.values, subject));
        (client, run)
    }
}

impl From<reqwest::blocking::RequestBuilder>
    for ConfiguredRequest<reqwest::blocking::RequestBuilder>
{
    fn from(builder: reqwest::blocking::RequestBuilder) -> Self {
        Self::new(builder)
    }
}

#[cfg(feature = "tokio")]
impl From<reqwest::RequestBuilder> for ConfiguredRequest<reqwest::RequestBuilder> {
    fn from(builder: reqwest::RequestBuilder) -> Self {
        Self::new(builder)
    }
}

#[cfg(feature = "tokio")]
impl ConfiguredRequest<reqwest::RequestBuilder> {
    /// Enabled by the `tokio` feature flag.
    ///
    /// Sends the request asynchronously with the options.
    ///
    /// See [`Request::send_async`].
    pub fn send_async<R: Request + ?Sized>(
        self,
        request: &R,
    ) -> impl Future<Output = Result<RequestOutput>> + Send {
        request.send_async(self)
    }

    /// Enabled by the `tokio` feature flag.
    ///
    /// Sends the request asynchronously with the options, passing the body
    /// of a successful response to `sink`.
    ///
    /// See [`Request::send_async_to`].
    pub fn send_async_to<R: Request + ?Sized>(
        self,
        request: &R,
        sink: &mut (dyn FnMut(&[u8]) -> Result + Send),
    ) -> impl Future<Output = Result<RequestOutput>> + Send {
        request.send_async_to(self, sink)
    }

    /// [internal]
    ///
    /// Returns the client of the request and its run like
    /// [`ConfiguredRequest::into_run`] of a blocking one.
    pub(crate) async fn into_run(self) -> (reqwest::Client, Result<Run<HttpRequest>>) {
        let (client, request) = self.builder.build_split();
        let run = match request.map_err(invalid_request) {
            Ok(request) => HttpRequest::from_async(request, self.options)
                .await
                .map(|subject| Self::run(self.values, subject)),
            Err(error) => Err(error),
        };
        (client, run)
    }
}

impl HttpRequest {
    /// Returns the request body, `None` when it has none.
    pub fn body(&self) -> Option<&[u8]> {
        self.body.as_deref()
    }

    /// Returns the key of the request for [`RequestCache`]: the method, the
    /// whole URL and the body escaped, but not the headers, as they may hold
    /// secrets that change. `None` for the methods other than `GET` and
    /// `HEAD`, which are not cached.
    pub fn cache_key(&self) -> Option<String> {
        matches!(self.method, Method::GET | Method::HEAD).then(|| {
            let key = format!("{} {}", self.method, self.url);

            match &self.body {
                Some(body) => format!("{key}\n{}", body.escape_ascii()),
                None => key,
            }
        })
    }

    /// Returns the request headers.
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Returns the request headers to be changed, e.g. by an interceptor.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::interceptor::{BoxFuture, Interceptor, Next};
    /// use agplatform::reqwest::header::{AUTHORIZATION, HeaderValue};
    /// use agplatform::{HttpRequest, RequestOutput, Result};
    ///
    /// struct Token(String);
    ///
    /// impl Interceptor<HttpRequest> for Token {
    ///     fn handle<'a>(&'a self, mut next: Next<'a, HttpRequest>) -> BoxFuture<'a, Result<RequestOutput>> {
    ///         let request = next.run_mut().subject_mut();
    ///
    ///         if request.url().host_str() == Some("api.example.com")
    ///             && let Ok(mut value) = HeaderValue::from_str(&format!("Bearer {}", self.0))
    ///         {
    ///             value.set_sensitive(true);
    ///             request.headers_mut().insert(AUTHORIZATION, value);
    ///         }
    ///
    ///         next.forward()
    ///     }
    /// }
    /// ```
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        &mut self.headers
    }

    /// Returns the method.
    pub fn method(&self) -> &Method {
        &self.method
    }

    /// Returns the options.
    pub fn options(&self) -> &RequestOptions {
        &self.options
    }

    /// Returns the options to be changed, e.g. by an interceptor.
    pub fn options_mut(&mut self) -> &mut RequestOptions {
        &mut self.options
    }

    /// Returns the [`Retry`] of the failed attempts of the runs [safe to
    /// retry](Self::safe_to_retry), up to `retries` times after `delay`,
    /// waiting at least as long as a failed response asks with its
    /// `Retry-After` header (as seconds or an HTTP date, e.g. of a 429 or
    /// 503), see [`RequestOutput::retry_after`]. Its
    /// [`when`](Retry::when) decides instead of the safety, e.g. to retry
    /// also another status. Each attempt is limited by the timeouts of the
    /// request and its client on its own.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{BlockingRequestBuilderExt, HttpRequest, Request, RequestOutput, TestRequest};
    /// use std::time::Duration;
    ///
    /// let request = TestRequest::new();
    /// request.add_interceptor(
    ///     HttpRequest::retry(3, Duration::from_millis(100))
    ///         .backoff(2)
    ///         .when(|run, result| {
    ///             run.subject().safe_to_retry(result) || result.as_ref().is_ok_and(|output| output.status == 412)
    ///         }),
    /// );
    /// request.set_responses("", [RequestOutput { status: 412, ..Default::default() }, RequestOutput { status: 200, ..Default::default() }]);
    ///
    /// let client = request.blocking_client().unwrap();
    /// client.put("https://example.com/item").body("data").send_blocking(&request).unwrap();
    ///
    /// assert_eq!(request.calls().len(), 2);
    /// ```
    pub fn retry(retries: u32, delay: Duration) -> Retry<HttpRequest> {
        Retry::<Self>::new(retries, delay)
            .when(|run, result| run.subject().safe_to_retry(result))
            .retry_after(RequestOutput::retry_after)
    }

    /// Returns `true` if the failed `result` of an attempt of the request
    /// may pass when retried without the risk of processing the request
    /// twice: of an idempotent method (`GET`, `HEAD`, `PUT`, `DELETE`,
    /// `OPTIONS` and `TRACE`, see RFC 9110) a [`TransportError`] or the
    /// status `408` (Request Timeout), `429` (Too Many Requests) or `5xx`,
    /// and of the others (e.g. `POST` and `PATCH`) only a
    /// [`TransportErrorKind::Connect`] error or the status `408`, `429` or
    /// `503` (Service Unavailable). Used by [`Self::retry`].
    pub fn safe_to_retry(&self, result: &Result<RequestOutput>) -> bool {
        let idempotent = self.method.is_idempotent();

        match result {
            Ok(output) if output.success => false,
            Ok(output) if idempotent => matches!(output.status, 408 | 429 | 500..=599),
            Ok(output) => matches!(output.status, 408 | 429 | 503),
            Err(error) => error
                .source_as::<TransportError>()
                .is_some_and(|error| idempotent || error.kind == TransportErrorKind::Connect),
        }
    }

    /// Sets the request body, `None` for none, e.g. in an interceptor.
    pub fn set_body(&mut self, body: Option<Vec<u8>>) {
        self.body = body.map(Bytes::from);
    }

    /// Sets the method, e.g. in an interceptor.
    pub fn set_method(&mut self, method: Method) {
        self.method = method;
    }

    /// Sets the timeout of the request (not of its client), `None` for
    /// none, e.g. in an interceptor.
    pub fn set_timeout(&mut self, timeout: Option<Duration>) {
        self.timeout = timeout;
    }

    /// Returns the method and the URL without its credentials, query and
    /// fragment, e.g. `GET https://example.com/path`, as named by the
    /// errors.
    pub fn target(&self) -> String {
        let mut url = self.url.clone();
        url.set_query(None);
        url.set_fragment(None);
        let _ = url.set_username("");
        let _ = url.set_password(None);
        format!("{} {url}", self.method)
    }

    /// Returns the timeout of the request (not of its client).
    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    /// Returns the URL.
    pub fn url(&self) -> &Url {
        &self.url
    }

    /// Returns the URL to be changed, e.g. by an interceptor.
    pub fn url_mut(&mut self) -> &mut Url {
        &mut self.url
    }

    /// [internal]
    ///
    /// Returns the blocking request sent by an attempt.
    pub(crate) fn blocking_request(&self) -> reqwest::blocking::Request {
        let mut request = reqwest::blocking::Request::new(self.method.clone(), self.url.clone());
        *request.headers_mut() = self.headers.clone();
        *request.version_mut() = self.version;
        *request.timeout_mut() = self.timeout;
        *request.body_mut() = self.body.clone().map(Into::into);
        request
    }

    /// [internal]
    ///
    /// Returns the async request sent by an attempt.
    #[cfg(feature = "tokio")]
    pub(crate) fn async_request(&self) -> reqwest::Request {
        let mut request = reqwest::Request::new(self.method.clone(), self.url.clone());
        *request.headers_mut() = self.headers.clone();
        *request.version_mut() = self.version;
        *request.timeout_mut() = self.timeout;
        *request.body_mut() = self.body.clone().map(Into::into);
        request
    }

    /// [internal]
    ///
    /// Returns the request of `request` with `options`, its body read into
    /// memory, copied as a blocking body cannot be taken.
    pub(crate) fn from_blocking(
        mut request: reqwest::blocking::Request,
        options: RequestOptions,
    ) -> Result<Self> {
        let body = request
            .body_mut()
            .as_mut()
            .map(|body| body.buffer().map(Bytes::copy_from_slice))
            .transpose()
            .map_err(unreadable_body)?;

        Ok(Self {
            method: request.method().clone(),
            url: request.url().clone(),
            headers: std::mem::take(request.headers_mut()),
            version: request.version(),
            timeout: request.timeout().copied(),
            body,
            options,
        })
    }

    /// [internal]
    ///
    /// Returns the request of `request` with `options`, its body read into
    /// memory.
    #[cfg(feature = "tokio")]
    pub(crate) async fn from_async(
        mut request: reqwest::Request,
        options: RequestOptions,
    ) -> Result<Self> {
        let body = match request.body_mut().take() {
            Some(body) => Some(
                http_body_util::BodyExt::collect(body)
                    .await
                    .map_err(unreadable_body)?
                    .to_bytes(),
            ),
            None => None,
        };

        Ok(Self {
            method: request.method().clone(),
            url: request.url().clone(),
            headers: std::mem::take(request.headers_mut()),
            version: request.version(),
            timeout: request.timeout().copied(),
            body,
            options,
        })
    }
}

impl Subject for HttpRequest {
    type Output = RequestOutput;
    type Stream = ();

    fn validate(_run: &Run<Self>) -> Result {
        Ok(())
    }

    /// Passes the body to the sink only when the status is a success, like
    /// a response.
    fn replay<'a>(
        _run: &'a Run<Self>,
        output: &'a RequestOutput,
        emit: &'a mut Emit<'_, Self>,
    ) -> BoxFuture<'a, Result> {
        Box::pin(async move {
            if !is_success(output.status) {
                emit.skip_sink();
            }

            if output.body.is_empty() {
                Ok(())
            } else {
                emit.emit((), &output.body).await
            }
        })
    }

    /// Sets the success from the status, the body captured and the lines
    /// displayed of the options.
    fn finish(
        run: &Run<Self>,
        output: RequestOutput,
        mut captured: Captured<Self>,
    ) -> RequestOutput {
        RequestOutput {
            success: is_success(output.status),
            body: captured.take(()),
            display_lines: run.subject().options.display_lines,
            ..output
        }
    }
}

impl Default for RequestOptions {
    fn default() -> Self {
        Self {
            display_lines: DISPLAY_LINES,
            keep_partial_body: false,
        }
    }
}

impl Default for Cache<HttpRequest> {
    /// Returns the cache of the requests by their
    /// [`HttpRequest::cache_key`].
    fn default() -> Self {
        Self::new(|run| run.subject().cache_key())
    }
}

impl RequestOutput {
    /// Returns how long the response asks to wait before a retry with its
    /// `Retry-After` header, as seconds or an HTTP date, if it does, see
    /// [`HttpRequest::retry`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::RequestOutput;
    /// use agplatform::reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};
    /// use std::time::Duration;
    ///
    /// let mut headers = HeaderMap::new();
    /// headers.insert(RETRY_AFTER, HeaderValue::from_static("120"));
    /// let output = RequestOutput { status: 503, headers, ..Default::default() };
    ///
    /// assert_eq!(output.retry_after(), Some(Duration::from_secs(120)));
    /// ```
    pub fn retry_after(&self) -> Option<Duration> {
        let value = self
            .headers
            .get(reqwest::header::RETRY_AFTER)?
            .to_str()
            .ok()?
            .trim();

        value.parse().map(Duration::from_secs).ok().or_else(|| {
            http_date(value).map(|date| {
                date.duration_since(SystemTime::now())
                    .unwrap_or(Duration::ZERO)
            })
        })
    }

    /// Returns the body as UTF-8.
    ///
    /// # Errors
    ///
    /// Returns an error if the body is not valid UTF-8.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::RequestOutput;
    ///
    /// let output = RequestOutput { body: b"body".to_vec(), ..Default::default() };
    /// assert_eq!(output.text().unwrap(), "body");
    /// ```
    pub fn text(&self) -> Result<&str> {
        std::str::from_utf8(&self.body)
            .map_err(|error| request_error!("body is not valid UTF-8: {error}").with_source(error))
    }

    /// Returns the body as UTF-8, replacing invalid sequences with
    /// `U+FFFD`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::RequestOutput;
    ///
    /// let output = RequestOutput { body: b"body\xFF".to_vec(), ..Default::default() };
    /// assert_eq!(output.text_lossy(), "body\u{FFFD}");
    /// ```
    pub fn text_lossy(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.body)
    }
}

impl Outcome for RequestOutput {
    fn success(&self) -> bool {
        self.success
    }
}

impl std::fmt::Display for RequestOutput {
    /// Shows the status, the duration and the last lines of the body.
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "status: {}\nduration: {:?}\nbody:\n{}",
            self.status,
            self.duration,
            crate::utils::Tail {
                text: &self.body,
                lines: self.display_lines,
            }
        )
    }
}

impl std::fmt::Debug for RequestOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let excerpt = &self.body[..self.body.len().min(DEBUG_BODY_BYTES)];
        let omitted = if excerpt.len() < self.body.len() {
            "..."
        } else {
            ""
        };

        f.debug_struct("RequestOutput")
            .field("success", &self.success)
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field(
                "body",
                &format_args!(
                    "{} bytes {:?}{omitted}",
                    self.body.len(),
                    String::from_utf8_lossy(excerpt)
                ),
            )
            .field("duration", &self.duration)
            .field("display_lines", &self.display_lines)
            .finish()
    }
}

impl std::error::Error for RequestOutput {}

impl Default for RequestOutput {
    fn default() -> Self {
        Self {
            success: false,
            status: 200,
            headers: HeaderMap::new(),
            body: Vec::new(),
            duration: Duration::ZERO,
            display_lines: DISPLAY_LINES,
        }
    }
}

impl PartialEq for RequestOutput {
    fn eq(&self, other: &Self) -> bool {
        self.status == other.status
            && self.headers == other.headers
            && self.body == other.body
            && self.duration == other.duration
    }
}

impl Eq for RequestOutput {}

impl TransportError {
    /// [internal]
    ///
    /// Creates an error of `kind` without a source, e.g. a simulated one.
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn new(kind: TransportErrorKind) -> Self {
        Self {
            kind,
            source: None,
            output: None,
        }
    }

    /// Returns the kind of the error.
    pub fn kind(&self) -> TransportErrorKind {
        self.kind
    }

    /// Returns the response whose body failed to be received, e.g. to
    /// resume a download with its headers, `None` when the request failed
    /// before the response arrived. Its body is the part received, empty
    /// when it was passed to a sink (see [`Request::send_blocking_to`]) or
    /// not streamed (see [`RequestOptions::keep_partial_body`]).
    pub fn output(&self) -> Option<&RequestOutput> {
        self.output.as_ref()
    }

    /// [internal]
    ///
    /// Records the response whose body failed to be received.
    pub(crate) fn with_output(mut self, output: RequestOutput) -> Self {
        self.output = Some(output);
        self
    }

    /// Classifies the `error` of reading a blocking response body, which
    /// holds the [`reqwest::Error`], if any.
    fn from_io(error: std::io::Error) -> Self {
        match error.downcast::<reqwest::Error>() {
            Ok(error) => error.into(),
            Err(error) => Self {
                kind: TransportErrorKind::Other,
                source: Some(Box::new(error)),
                output: None,
            },
        }
    }
}

impl From<reqwest::Error> for TransportError {
    /// Classifies `error`, dropping its URL as it may hold secrets.
    fn from(error: reqwest::Error) -> Self {
        let kind = if error.is_connect() {
            TransportErrorKind::Connect
        } else if error.is_timeout() {
            TransportErrorKind::Timeout
        } else {
            TransportErrorKind::Other
        };

        Self {
            kind,
            source: Some(Box::new(error.without_url())),
            output: None,
        }
    }
}

impl std::fmt::Display for TransportError {
    /// Shows the kind followed by the chain of the sources, as the
    /// messages of the [`reqwest`] errors alone are not specific.
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let sources: String =
            std::iter::successors(std::error::Error::source(self), |&error| error.source())
                .map(|error| format!(": {error}"))
                .collect();
        write!(f, "{}{sources}", self.kind)
    }
}

impl std::error::Error for TransportError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source
            .as_ref()
            .map(|source| &**source as &(dyn std::error::Error + 'static))
    }
}

impl std::fmt::Display for TransportErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            TransportErrorKind::Connect => write!(f, "failed to connect"),
            TransportErrorKind::Timeout => write!(f, "timed out"),
            TransportErrorKind::Other => write!(f, "failed"),
        }
    }
}

impl<'a> Received<'a> {
    /// Starts the response to `run` with `status` and `headers`, its body
    /// passed to `emit`.
    pub(crate) fn new(
        run: &'a Run<HttpRequest>,
        mut emit: Emit<'a, HttpRequest>,
        status: u16,
        headers: HeaderMap,
    ) -> Self {
        let (subject, success) = (run.subject(), is_success(status));

        if !success {
            emit.skip_sink();
        }

        Self {
            subject,
            emit,
            output: RequestOutput {
                status,
                headers,
                display_lines: subject.options.display_lines,
                ..Default::default()
            },
        }
    }

    /// Passes the `data` of the body on.
    ///
    /// # Errors
    ///
    /// Returns the error of an interceptor or the sink, which ends the
    /// attempt.
    pub(crate) async fn push(&mut self, data: &[u8]) -> Result {
        self.emit.emit((), data).await
    }

    /// Returns the response received after `duration`, its body captured
    /// by the engine.
    pub(crate) fn end(self, duration: Duration) -> RequestOutput {
        RequestOutput {
            duration,
            ..self.output
        }
    }

    /// Returns the error of the body failing to be received with `error`
    /// after `duration`, with the part of the body captured if kept.
    pub(crate) fn interrupted(mut self, duration: Duration, error: TransportError) -> crate::Error {
        let body = if self.subject.options.keep_partial_body {
            self.emit.take_captured(())
        } else {
            Vec::new()
        };

        transport_failure(
            self.subject,
            error.with_output(RequestOutput {
                body,
                duration,
                ..self.output
            }),
        )
    }
}

impl Executor for BlockingClient<'_> {
    type Subject = HttpRequest;

    fn execute<'a>(
        &'a self,
        run: &'a Run<HttpRequest>,
        emit: Emit<'a, HttpRequest>,
    ) -> BoxFuture<'a, Result<RequestOutput>> {
        Box::pin(async move {
            let start = Instant::now();
            let mut response = self
                .0
                .execute(run.subject().blocking_request())
                .map_err(|error| transport_failure(run.subject(), error.into()))?;
            let headers = std::mem::take(response.headers_mut());
            let mut received = Received::new(run, emit, response.status().as_u16(), headers);
            let mut buffer = vec![0; READ_BYTES];

            // WHY: read in parts, as the client timeout limits each read,
            // but all of `bytes`.
            loop {
                match std::io::Read::read(&mut response, &mut buffer) {
                    Ok(0) => return Ok(received.end(start.elapsed())),
                    Ok(read) => received.push(&buffer[..read]).await?,
                    Err(error) => {
                        return Err(
                            received.interrupted(start.elapsed(), TransportError::from_io(error))
                        );
                    }
                }
            }
        })
    }

    fn sleep(&self, delay: Duration) -> BoxFuture<'static, ()> {
        Box::pin(async move { std::thread::sleep(delay) })
    }
}

impl BlockingExecutor for BlockingClient<'_> {}

#[cfg(feature = "tokio")]
impl Executor for AsyncClient<'_> {
    type Subject = HttpRequest;

    fn execute<'a>(
        &'a self,
        run: &'a Run<HttpRequest>,
        emit: Emit<'a, HttpRequest>,
    ) -> BoxFuture<'a, Result<RequestOutput>> {
        Box::pin(async move {
            let start = Instant::now();
            let mut response = self
                .0
                .execute(run.subject().async_request())
                .await
                .map_err(|error| transport_failure(run.subject(), error.into()))?;
            let headers = std::mem::take(response.headers_mut());
            let mut received = Received::new(run, emit, response.status().as_u16(), headers);

            loop {
                match response.chunk().await {
                    Ok(Some(chunk)) => received.push(&chunk).await?,
                    Ok(None) => return Ok(received.end(start.elapsed())),
                    Err(error) => return Err(received.interrupted(start.elapsed(), error.into())),
                }
            }
        })
    }

    fn sleep(&self, delay: Duration) -> BoxFuture<'static, ()> {
        Box::pin(tokio::time::sleep(delay))
    }
}

#[cfg(feature = "tokio")]
impl AsyncExecutor for AsyncClient<'_> {}

impl RequestImpl {
    /// Creates a new `RequestImpl` with default clients that are built on
    /// first use.
    pub fn new() -> Self {
        Self::with_env(EnvImpl::new(Vec::new(), std::path::PathBuf::new()))
    }

    /// [internal]
    ///
    /// Creates a new `RequestImpl` taking the proxy variables from `env`,
    /// see [`Request`].
    pub(crate) fn with_env(env: EnvImpl) -> Self {
        Self {
            clients: DefaultClients::new(env),
            runs: Runs::new(Interceptors::new()),
        }
    }

    fn send_blocking_with(
        &self,
        request: ConfiguredRequest<reqwest::blocking::RequestBuilder>,
        sink: Option<&mut BodySink<'_>>,
    ) -> Result<RequestOutput> {
        let (client, run) = request.into_run();
        run_blocking(&self.runs, &BlockingClient(&client), run?, sink)
    }

    #[cfg(feature = "tokio")]
    async fn send_async_with(
        &self,
        request: ConfiguredRequest<reqwest::RequestBuilder>,
        sink: Option<&mut BodySink<'_>>,
    ) -> Result<RequestOutput> {
        let (client, run) = request.into_run().await;
        run_async(&self.runs, &AsyncClient(&client), run?, sink).await
    }
}

impl Default for RequestImpl {
    fn default() -> Self {
        Self::new()
    }
}

impl Request for RequestImpl {
    fn blocking_client(&self) -> Result<reqwest::blocking::Client> {
        self.clients.blocking_client()
    }

    fn blocking_client_builder(&self) -> reqwest::blocking::ClientBuilder {
        self.clients.blocking_client_builder()
    }

    fn set_blocking_client(&self, builder: reqwest::blocking::ClientBuilder) {
        self.clients.set_blocking_client(builder);
    }

    fn send_blocking(
        &self,
        request: impl Into<ConfiguredRequest<reqwest::blocking::RequestBuilder>>,
    ) -> Result<RequestOutput> {
        self.send_blocking_with(request.into(), None)
    }

    fn send_blocking_to(
        &self,
        request: impl Into<ConfiguredRequest<reqwest::blocking::RequestBuilder>>,
        sink: &mut (dyn FnMut(&[u8]) -> Result + Send),
    ) -> Result<RequestOutput> {
        self.send_blocking_with(request.into(), Some(sink))
    }

    #[cfg(feature = "tokio")]
    fn client(&self) -> Result<reqwest::Client> {
        self.clients.client()
    }

    #[cfg(feature = "tokio")]
    fn client_builder(&self) -> reqwest::ClientBuilder {
        self.clients.client_builder()
    }

    #[cfg(feature = "tokio")]
    fn set_client(&self, builder: reqwest::ClientBuilder) {
        self.clients.set_client(builder);
    }

    #[cfg(feature = "tokio")]
    fn send_async(
        &self,
        request: impl Into<ConfiguredRequest<reqwest::RequestBuilder>>,
    ) -> impl Future<Output = Result<RequestOutput>> + Send {
        self.send_async_with(request.into(), None)
    }

    #[cfg(feature = "tokio")]
    fn send_async_to(
        &self,
        request: impl Into<ConfiguredRequest<reqwest::RequestBuilder>>,
        sink: &mut (dyn FnMut(&[u8]) -> Result + Send),
    ) -> impl Future<Output = Result<RequestOutput>> + Send {
        self.send_async_with(request.into(), Some(sink))
    }

    fn interceptors(&self) -> Interceptors<HttpRequest> {
        self.runs.interceptors()
    }

    fn clear_interceptors(&self) {
        self.runs.clear_interceptors();
    }

    fn add_interceptor(&self, interceptor: impl Interceptor<HttpRequest>) {
        self.runs.add_interceptor(interceptor);
    }
}

const DISPLAY_LINES: usize = 10;

/// The number of the first bytes of the body shown by the [`Debug`] of a
/// [`RequestOutput`].
const DEBUG_BODY_BYTES: usize = 256;

/// The size of the reads of a blocking response body, like
/// [`std::io::copy`].
const READ_BYTES: usize = 8 * 1024;

/// Returns whether `status` is a success (2xx).
fn is_success(status: u16) -> bool {
    (200..300).contains(&status)
}

/// Returns the error of a request failing to be built.
fn invalid_request(error: reqwest::Error) -> crate::Error {
    let error = error.without_url();
    request_error!("invalid request: {error}").with_source(error)
}

/// Returns the error of a request body failing to be read.
fn unreadable_body(error: reqwest::Error) -> crate::Error {
    let error = error.without_url();
    request_error!("request body could not be read: {error}").with_source(error)
}

/// [internal]
///
/// Runs `run` by `executor` through `runs`, passing the body of a
/// successful response to `sink`, if any, and returns its output, an error
/// when it is not a success, see [`Request::send_blocking`].
pub(crate) fn run_blocking<E: BlockingExecutor<Subject = HttpRequest>>(
    runs: &Runs<HttpRequest>,
    executor: &E,
    mut run: Run<HttpRequest>,
    sink: Option<&mut BodySink<'_>>,
) -> Result<RequestOutput> {
    let mut sink = sink.map(|sink| move |(): (), data: &[u8]| sink(data));
    let result = runs.run_blocking(
        executor,
        &mut run,
        sink.as_mut().map(|sink| sink as &mut dyn Sink<HttpRequest>),
    );
    conclude(result, |output| failure(run.subject(), output))
}

/// [internal]
///
/// Runs `run` like [`run_blocking`] asynchronously, see
/// [`Request::send_async`].
#[cfg(feature = "tokio")]
pub(crate) async fn run_async<E: AsyncExecutor<Subject = HttpRequest>>(
    runs: &Runs<HttpRequest>,
    executor: &E,
    mut run: Run<HttpRequest>,
    sink: Option<&mut BodySink<'_>>,
) -> Result<RequestOutput> {
    let mut sink = sink.map(|sink| move |(): (), data: &[u8]| sink(data));
    let result = runs
        .run(
            executor,
            &mut run,
            sink.as_mut().map(|sink| sink as &mut dyn Sink<HttpRequest>),
        )
        .await;
    conclude(result, |output| failure(run.subject(), output))
}

/// Returns the error of `request` failing with `output` as its source.
fn failure(request: &HttpRequest, output: RequestOutput) -> crate::Error {
    let target = request.target();
    let error = if is_success(output.status) {
        request_error!("'{target}' was not accepted with status {}", output.status)
    } else {
        request_error!("'{target}' failed with status {}", output.status)
    };

    error.with_subject(target).with_source(output)
}

/// [internal]
///
/// Returns the error of `request` failing with the transport `error` as its
/// source.
pub(crate) fn transport_failure(request: &HttpRequest, error: TransportError) -> crate::Error {
    let target = request.target();
    request_error!("'{target}' {error}")
        .with_subject(target)
        .with_source(error)
}

/// Parses an HTTP date in one of its formats (see RFC 9110), the preferred
/// `Sun, 06 Nov 1994 08:49:37 GMT` (IMF-fixdate) or the obsolete
/// `Sunday, 06-Nov-94 08:49:37 GMT` (RFC 850, its year taken from 1970 to
/// 2069) and `Sun Nov  6 08:49:37 1994` (asctime).
fn http_date(value: &str) -> Option<SystemTime> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    let parts = value.split_ascii_whitespace().collect::<Vec<_>>();
    let (day, month, year, time) = match parts[..] {
        [_, day, month, year, time, "GMT"] => (day, month, year.parse::<i64>().ok()?, time),
        [_, date, time, "GMT"] => {
            let mut date = date.split('-');
            let [day, month, year] = std::array::from_fn(|_| date.next().unwrap_or(""));
            let year = year
                .parse::<i64>()
                .ok()
                .filter(|_| year.len() == 2 && date.next().is_none())?;
            (day, month, year + if year < 70 { 2000 } else { 1900 }, time)
        }
        [_, month, day, time, year] => (day, month, year.parse::<i64>().ok()?, time),
        _ => return None,
    };
    let month = MONTHS.iter().position(|name| *name == month)? as i64 + 1;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = [
        31,
        28 + i64::from(leap),
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let day = day
        .parse::<i64>()
        .ok()
        .filter(|day| (1..=days[month as usize - 1]).contains(day))?;
    let mut time = time.split(':');
    let [hour, minute, second] =
        std::array::from_fn(|_| time.next().and_then(|part| part.parse::<i64>().ok()));
    let hour = hour.filter(|hour| (0..24).contains(hour))?;
    let minute = minute.filter(|minute| (0..60).contains(minute))?;
    let second = second.filter(|second| (0..=60).contains(second))?;
    (time.next().is_none() && (0..=9999).contains(&year)).then_some(())?;

    // WHY: the days since the epoch of the civil date, see
    // https://howardhinnant.github.io/date_algorithms.html#days_from_civil
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let seconds = u64::try_from(days * 86_400 + hour * 3_600 + minute * 60 + second).ok()?;

    SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::TestRequest;
    use crate::interceptor::Chunk;
    use crate::interceptor::DryRun;
    use crate::interceptor::DryRunOutput;
    use crate::interceptor::Next;
    use std::error::Error as _;
    use std::io::BufRead as _;
    use std::io::Read as _;
    use std::io::Write as _;
    use std::net::TcpListener;
    use std::net::TcpStream;
    use std::sync::Arc;
    use std::sync::Mutex;

    pub(crate) struct Server {
        pub(crate) url: String,
        received: Arc<Mutex<Vec<String>>>,
    }

    impl Server {
        pub(crate) fn received(&self) -> Vec<String> {
            self.received.lock().unwrap().clone()
        }
    }

    /// Answers each connection with what `respond` returns for its index
    /// and request, then closes it.
    pub(crate) fn serve(respond: impl Fn(usize, &str) -> Vec<u8> + Send + 'static) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let received = Arc::new(Mutex::new(Vec::new()));
        let log = received.clone();

        std::thread::spawn(move || {
            for (index, stream) in listener.incoming().enumerate() {
                let mut stream = stream.unwrap();
                let request = read_request(&stream);
                log.lock().unwrap().push(request.clone());
                let _ = stream.write_all(&respond(index, &request));
            }
        });

        Server { url, received }
    }

    /// Answers each connection with a success whose body is the `chunks`,
    /// each sent after `pause`.
    pub(crate) fn serve_chunks(pause: Duration, chunks: &'static [&'static str]) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                read_request(&stream);
                let length: usize = chunks.iter().map(|chunk| chunk.len()).sum();
                let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {length}\r\n\r\n");
                let _ = stream.write_all(head.as_bytes());

                for chunk in chunks {
                    std::thread::sleep(pause);
                    let _ = stream.write_all(chunk.as_bytes());
                }
            }
        });

        Server {
            url,
            received: Arc::default(),
        }
    }

    fn read_request(stream: &TcpStream) -> String {
        let mut reader = std::io::BufReader::new(stream);
        let mut request = String::new();
        while reader.read_line(&mut request).unwrap() > 2 {}
        let length = request
            .to_lowercase()
            .lines()
            .find_map(|line| line.strip_prefix("content-length: ").map(str::to_string))
            .map_or(0, |length| length.parse().unwrap());
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        request + &String::from_utf8_lossy(&body)
    }

    pub(crate) fn response(status: u16, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status} Status\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    pub(crate) fn truncated(_: usize, _: &str) -> Vec<u8> {
        b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\nconnection: close\r\n\r\npartial".to_vec()
    }

    pub(crate) fn slow(_: usize, _: &str) -> Vec<u8> {
        std::thread::sleep(Duration::from_secs(2));
        response(200, "late")
    }

    pub(crate) fn ok(_: usize, _: &str) -> Vec<u8> {
        response(200, "body")
    }

    pub(crate) fn not_found(_: usize, _: &str) -> Vec<u8> {
        response(404, "missing")
    }

    pub(crate) fn unavailable_once(index: usize, _: &str) -> Vec<u8> {
        if index == 0 {
            response(503, "unavailable")
        } else {
            response(200, "body")
        }
    }

    pub(crate) fn closed_url() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    }

    pub(crate) fn invalid_client() -> reqwest::blocking::ClientBuilder {
        reqwest::blocking::Client::builder().user_agent("\n")
    }

    #[cfg(feature = "tokio")]
    pub(crate) fn invalid_async_client() -> reqwest::ClientBuilder {
        reqwest::Client::builder().user_agent("\n")
    }

    struct FailingReader;

    impl std::io::Read for FailingReader {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("read failed"))
        }
    }

    #[cfg(feature = "tokio")]
    struct FailingBody;

    #[cfg(feature = "tokio")]
    impl http_body::Body for FailingBody {
        type Data = &'static [u8];
        type Error = std::io::Error;

        fn poll_frame(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<std::result::Result<http_body::Frame<Self::Data>, Self::Error>>>
        {
            std::task::Poll::Ready(Some(Err(std::io::Error::other("read failed"))))
        }
    }

    #[cfg(feature = "tokio")]
    fn streamed(data: &'static [u8]) -> reqwest::Body {
        reqwest::Body::wrap(http_body_util::Full::new(data))
    }

    /// Records the runs it handles, the chunks of the streamed ones passing
    /// through it and the cancelled runs.
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<String>>>);

    impl Recorder {
        fn events(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }

        fn push(&self, event: String) {
            self.0.lock().unwrap().push(event);
        }
    }

    impl Interceptor<HttpRequest> for Recorder {
        fn handle<'a>(
            &'a self,
            mut next: Next<'a, HttpRequest>,
        ) -> BoxFuture<'a, Result<RequestOutput>> {
            Box::pin(async move {
                self.push(format!("started {}", next.run().subject().target()));
                let result = next.attempt().await;
                self.push(format!(
                    "finished {}",
                    result.as_ref().map_or_else(
                        |error| error.to_string(),
                        |output| output.status.to_string()
                    )
                ));
                result
            })
        }

        fn chunk(&self, chunk: Chunk<HttpRequest>) -> Result {
            if chunk.run().subject().options().keep_partial_body {
                self.push(format!("body {}", String::from_utf8_lossy(chunk.data())));
            }

            chunk.forward()
        }

        fn cancelled(&self, run: &Run<HttpRequest>) {
            self.push(format!("cancelled {}", run.subject().target()));
        }
    }

    /// Changes the request before passing it on.
    struct Change(fn(&mut HttpRequest));

    impl Interceptor<HttpRequest> for Change {
        fn handle<'a>(
            &'a self,
            mut next: Next<'a, HttpRequest>,
        ) -> BoxFuture<'a, Result<RequestOutput>> {
            (self.0)(next.run_mut().subject_mut());
            next.forward()
        }
    }

    /// Changes of the env the request shares, like by its platform.
    impl RequestImpl {
        pub(crate) fn set_var(&self, key: &str, value: &str) {
            crate::Env::set_var(&self.clients.env, key, value).unwrap();
        }

        pub(crate) fn remove_var(&self, key: &str) {
            crate::Env::remove_var(&self.clients.env, key);
        }
    }

    /// Returns a request whose only interceptor is the returned recorder.
    fn observed() -> (RequestImpl, Recorder) {
        let recorder = Recorder::default();
        let request = RequestImpl::new();
        request.add_interceptor(recorder.clone());
        (request, recorder)
    }

    /// Returns a request with a single interceptor `interceptor`.
    fn intercepted(interceptor: impl Interceptor<HttpRequest>) -> RequestImpl {
        let request = RequestImpl::new();
        request.add_interceptor(interceptor);
        request
    }

    /// Returns the interceptors `first` and `second`, in that order.
    fn chain(
        first: impl Interceptor<HttpRequest>,
        second: impl Interceptor<HttpRequest>,
    ) -> Interceptors<HttpRequest> {
        let mut interceptors = Interceptors::new();
        interceptors.push(first);
        interceptors.push(second);
        interceptors
    }

    fn output(status: u16, body: &[u8]) -> RequestOutput {
        RequestOutput {
            status,
            body: body.to_vec(),
            ..Default::default()
        }
    }

    /// Returns the request of `method` to `http://h`.
    fn subject(method: Method) -> HttpRequest {
        let request = reqwest::blocking::Request::new(method, Url::parse("http://h").unwrap());
        HttpRequest::from_blocking(request, RequestOptions::default()).unwrap()
    }

    pub(crate) fn blocking_get(
        request: &RequestImpl,
        url: &str,
    ) -> reqwest::blocking::RequestBuilder {
        request.blocking_client().unwrap().get(url)
    }

    #[cfg(feature = "tokio")]
    pub(crate) fn get(request: &RequestImpl, url: &str) -> reqwest::RequestBuilder {
        request.client().unwrap().get(url)
    }

    #[cfg(feature = "tokio")]
    pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    fn transport_kind(result: &Result<RequestOutput>) -> Option<TransportErrorKind> {
        result
            .as_ref()
            .err()
            .and_then(|error| error.source_as::<TransportError>())
            .map(TransportError::kind)
    }

    fn collect_into(received: &mut Vec<u8>) -> impl FnMut(&[u8]) -> Result + Send + '_ {
        |data| {
            received.extend_from_slice(data);
            Ok(())
        }
    }

    fn interrupted_output(result: &Result<RequestOutput>) -> RequestOutput {
        result
            .as_ref()
            .unwrap_err()
            .source_as::<TransportError>()
            .and_then(TransportError::output)
            .unwrap()
            .clone()
    }

    mod blocking_request_builder_ext {
        use super::*;

        mod send_blocking {
            use super::*;

            #[test]
            fn send_blocking_should_send_request_with_default_options() {
                let server = serve(ok);
                let request = RequestImpl::new();

                let output = blocking_get(&request, &server.url)
                    .send_blocking(&request)
                    .unwrap();

                assert_eq!(output.body, b"body");
            }
        }

        mod send_blocking_to {
            use super::*;

            #[test]
            fn send_blocking_to_should_send_request_with_default_options_to_sink() {
                let server = serve(ok);
                let request = RequestImpl::new();
                let mut received = Vec::new();

                blocking_get(&request, &server.url)
                    .send_blocking_to(&request, &mut collect_into(&mut received))
                    .unwrap();

                assert_eq!(received, b"body");
            }
        }

        mod display_lines {
            use super::*;

            #[test]
            fn display_lines_should_set_lines() {
                let request = reqwest::blocking::Client::new()
                    .get("http://localhost")
                    .display_lines(3);

                assert_eq!(request.options().display_lines, 3);
            }
        }

        mod interceptors {
            use super::*;

            #[test]
            fn interceptors_should_set_own_interceptors_of_run() {
                let mut interceptors = Interceptors::new();
                interceptors.push(DryRun);

                let (_, run) = reqwest::blocking::Client::new()
                    .get("http://localhost")
                    .interceptors(interceptors.clone())
                    .into_run();

                assert_eq!(
                    run.unwrap()
                        .get::<OwnInterceptors<HttpRequest>>()
                        .map(|own| &own.0),
                    Some(&interceptors)
                );
            }
        }

        mod keep_partial_body {
            use super::*;

            #[test]
            fn keep_partial_body_should_set_option() {
                let request = reqwest::blocking::Client::new()
                    .get("http://localhost")
                    .keep_partial_body();

                assert!(request.options().keep_partial_body);
            }
        }

        mod with {
            use super::*;

            #[test]
            fn with_should_set_value_for_interceptors() {
                let request = reqwest::blocking::Client::new()
                    .get("http://localhost")
                    .with(7_u32);

                let (_, run) = request.into_run();

                assert_eq!(run.unwrap().get::<u32>(), Some(&7));
            }
        }
    }

    #[cfg(feature = "tokio")]
    mod request_builder_ext {
        use super::*;

        mod send_async {
            use super::*;

            #[test]
            fn send_async_should_send_request_with_default_options() {
                let server = serve(ok);
                let request = RequestImpl::new();

                let output = block_on(get(&request, &server.url).send_async(&request)).unwrap();

                assert_eq!(output.body, b"body");
            }
        }

        mod send_async_to {
            use super::*;

            #[test]
            fn send_async_to_should_send_request_with_default_options_to_sink() {
                let server = serve(ok);
                let request = RequestImpl::new();
                let mut received = Vec::new();

                block_on(
                    get(&request, &server.url)
                        .send_async_to(&request, &mut collect_into(&mut received)),
                )
                .unwrap();

                assert_eq!(received, b"body");
            }
        }

        mod display_lines {
            use super::*;

            #[test]
            fn display_lines_should_set_lines() {
                let request = reqwest::Client::new()
                    .get("http://localhost")
                    .display_lines(3);

                assert_eq!(request.options().display_lines, 3);
            }
        }

        mod interceptors {
            use super::*;

            #[test]
            fn interceptors_should_set_own_interceptors_of_run() {
                let mut interceptors = Interceptors::new();
                interceptors.push(DryRun);

                let (_, run) = block_on(
                    reqwest::Client::new()
                        .get("http://localhost")
                        .interceptors(interceptors.clone())
                        .into_run(),
                );

                assert_eq!(
                    run.unwrap()
                        .get::<OwnInterceptors<HttpRequest>>()
                        .map(|own| &own.0),
                    Some(&interceptors)
                );
            }
        }

        mod keep_partial_body {
            use super::*;

            #[test]
            fn keep_partial_body_should_set_option() {
                let request = reqwest::Client::new()
                    .get("http://localhost")
                    .keep_partial_body();

                assert!(request.options().keep_partial_body);
            }
        }

        mod with {
            use super::*;

            #[test]
            fn with_should_set_value_for_interceptors() {
                let request = reqwest::Client::new().get("http://localhost").with(7_u32);

                let (_, run) = block_on(request.into_run());

                assert_eq!(run.unwrap().get::<u32>(), Some(&7));
            }
        }
    }

    mod configured_request {
        use super::*;

        mod display_lines {
            use super::*;

            #[test]
            fn display_lines_should_set_lines() {
                let request = ConfiguredRequest::new(
                    reqwest::blocking::Client::new().get("http://localhost"),
                )
                .display_lines(4);

                assert_eq!(request.options().display_lines, 4);
            }
        }

        mod new {
            use super::*;

            #[test]
            fn new_should_have_default_options() {
                let blocking = ConfiguredRequest::new(
                    reqwest::blocking::Client::new().get("http://localhost"),
                );

                assert_eq!(blocking.options(), &RequestOptions::default());
            }
        }

        mod with_options {
            use super::*;

            #[test]
            fn with_options_should_keep_options() {
                let options = RequestOptions {
                    keep_partial_body: true,
                    ..Default::default()
                };

                let request = ConfiguredRequest::with_options(
                    reqwest::blocking::Client::new().get("http://localhost"),
                    options.clone(),
                );

                assert_eq!(request.options(), &options);
            }
        }

        mod with {
            use super::*;

            #[test]
            fn with_should_replace_value_of_same_type() {
                let request = ConfiguredRequest::new(
                    reqwest::blocking::Client::new().get("http://localhost"),
                )
                .with(1_u32)
                .with("other")
                .with(2_u32);

                let run = request.into_run().1.unwrap();

                assert_eq!(run.get::<u32>(), Some(&2));
                assert_eq!(run.get::<&str>(), Some(&"other"));
            }
        }

        mod from {
            use super::*;

            #[allow(clippy::module_inception)]
            mod from {
                use super::*;

                #[test]
                fn from_builder_should_have_default_options() {
                    let blocking = ConfiguredRequest::from(
                        reqwest::blocking::Client::new().get("http://localhost"),
                    );

                    assert_eq!(blocking.options(), &RequestOptions::default());
                }

                #[cfg(feature = "tokio")]
                #[test]
                fn from_async_builder_should_have_default_options() {
                    let request =
                        ConfiguredRequest::from(reqwest::Client::new().get("http://localhost"));

                    assert_eq!(request.options(), &RequestOptions::default());
                }
            }
        }

        mod send_blocking {
            use super::*;

            #[test]
            fn send_blocking_should_send_request_with_options() {
                let server = serve(ok);
                let (request, recorder) = observed();

                blocking_get(&request, &server.url)
                    .keep_partial_body()
                    .send_blocking(&request)
                    .unwrap();

                assert!(recorder.events().contains(&"body body".to_string()));
            }
        }

        mod send_blocking_to {
            use super::*;

            #[test]
            fn send_blocking_to_should_send_request_with_options_to_sink() {
                let server = serve(ok);
                let (request, recorder) = observed();
                let mut received = Vec::new();

                blocking_get(&request, &server.url)
                    .keep_partial_body()
                    .send_blocking_to(&request, &mut collect_into(&mut received))
                    .unwrap();

                assert!(recorder.events().contains(&"body body".to_string()));
                assert_eq!(received, b"body");
            }
        }

        #[cfg(feature = "tokio")]
        mod send_async {
            use super::*;

            #[test]
            fn send_async_should_send_request_with_options() {
                let server = serve(ok);
                let (request, recorder) = observed();

                block_on(
                    get(&request, &server.url)
                        .keep_partial_body()
                        .send_async(&request),
                )
                .unwrap();

                assert!(recorder.events().contains(&"body body".to_string()));
            }
        }

        #[cfg(feature = "tokio")]
        mod send_async_to {
            use super::*;

            #[test]
            fn send_async_to_should_send_request_with_options_to_sink() {
                let server = serve(ok);
                let (request, recorder) = observed();
                let mut received = Vec::new();

                block_on(
                    get(&request, &server.url)
                        .keep_partial_body()
                        .send_async_to(&request, &mut collect_into(&mut received)),
                )
                .unwrap();

                assert!(recorder.events().contains(&"body body".to_string()));
                assert_eq!(received, b"body");
            }
        }

        mod into_run {
            use super::*;

            #[test]
            fn into_run_should_build_request_with_options() {
                let client = reqwest::blocking::Client::new();
                let request = client
                    .put("http://h/path")
                    .header("x-key", "value")
                    .timeout(Duration::from_secs(5))
                    .version(Version::HTTP_2)
                    .body("data")
                    .display_lines(3);

                let run = request.into_run().1.unwrap();

                let subject = run.subject();
                assert_eq!(subject.method(), Method::PUT);
                assert_eq!(subject.url().as_str(), "http://h/path");
                assert_eq!(subject.headers()["x-key"], "value");
                assert_eq!(subject.timeout(), Some(Duration::from_secs(5)));
                assert_eq!(subject.version, Version::HTTP_2);
                assert_eq!(subject.body(), Some(b"data".as_slice()));
                assert_eq!(subject.options().display_lines, 3);
            }

            #[test]
            fn into_run_invalid_request_should_fail() {
                let request = ConfiguredRequest::new(reqwest::blocking::Client::new().get("::"));

                let error = request.into_run().1.err().unwrap();

                assert_eq!(error.description(), "invalid request: builder error");
                assert!(error.source_as::<reqwest::Error>().unwrap().is_builder());
            }

            #[cfg(feature = "tokio")]
            #[test]
            fn into_run_async_should_build_request_with_options() {
                let request = reqwest::Client::new()
                    .post("http://h/path")
                    .body(streamed(b"data"))
                    .keep_partial_body();

                let run = block_on(request.into_run()).1.unwrap();

                assert_eq!(run.subject().method(), Method::POST);
                assert_eq!(run.subject().body(), Some(b"data".as_slice()));
                assert!(run.subject().options().keep_partial_body);
            }

            #[cfg(feature = "tokio")]
            #[test]
            fn into_run_async_invalid_request_should_fail() {
                let request = ConfiguredRequest::new(reqwest::Client::new().get("::"));

                let error = block_on(request.into_run()).1.err().unwrap();

                assert_eq!(error.description(), "invalid request: builder error");
            }
        }
    }

    mod http_request {
        use super::*;

        mod body {
            use super::*;

            #[test]
            fn body_should_return_body_or_none() {
                let mut request = subject(Method::POST);
                assert_eq!(request.body(), None);

                request.set_body(Some(b"data".to_vec()));

                assert_eq!(request.body(), Some(b"data".as_slice()));
            }
        }

        mod cache_key {
            use super::*;

            #[test]
            fn cache_key_should_name_method_url_and_body_of_get_and_head() {
                let mut get = subject(Method::GET);
                assert_eq!(get.cache_key().as_deref(), Some("GET http://h/"));
                get.set_body(Some(b"a\nb\xff".to_vec()));
                assert_eq!(
                    get.cache_key().as_deref(),
                    Some("GET http://h/\na\\nb\\xff")
                );

                assert_eq!(
                    subject(Method::HEAD).cache_key().as_deref(),
                    Some("HEAD http://h/")
                );
                for method in [Method::POST, Method::PUT, Method::DELETE] {
                    assert_eq!(subject(method).cache_key(), None);
                }
            }
        }

        mod headers {
            use super::*;

            #[test]
            fn headers_should_return_headers() {
                assert!(subject(Method::GET).headers().is_empty());
            }
        }

        mod headers_mut {
            use super::*;

            #[test]
            fn headers_mut_should_change_headers_sent() {
                let server = serve(ok);
                let request = intercepted(Change(|request| {
                    request
                        .headers_mut()
                        .insert("x-key", "value".parse().unwrap());
                }));

                blocking_get(&request, &server.url)
                    .send_blocking(&request)
                    .unwrap();

                assert!(server.received()[0].contains("x-key: value\r\n"));
            }
        }

        mod method {
            use super::*;

            #[test]
            fn method_should_return_method() {
                assert_eq!(subject(Method::PATCH).method(), Method::PATCH);
            }
        }

        mod options {
            use super::*;

            #[test]
            fn options_should_return_options() {
                assert_eq!(subject(Method::GET).options(), &RequestOptions::default());
            }
        }

        mod options_mut {
            use super::*;

            #[test]
            fn options_mut_should_change_options_of_run() {
                let server = serve(not_found);
                let request = intercepted(Change(|request| {
                    request.options_mut().display_lines = 1;
                }));

                let error = blocking_get(&request, &server.url)
                    .send_blocking(&request)
                    .unwrap_err();

                assert_eq!(error.request_output().unwrap().display_lines, 1);
            }
        }

        mod retry {
            use super::*;

            /// Returns a failed response asking to wait `retry_after`.
            fn asking(retry_after: &str) -> RequestOutput {
                let mut output = output(503, b"");
                output
                    .headers
                    .insert(reqwest::header::RETRY_AFTER, retry_after.parse().unwrap());
                output
            }

            fn test_request() -> TestRequest {
                let request = TestRequest::new();
                request.add_interceptor(HttpRequest::retry(2, Duration::from_secs(1)));
                request
            }

            #[test]
            fn retry_should_retry_failures_safe_to_retry_until_success() {
                let request = test_request();
                request.set_responses("", [output(503, b""), output(200, b"ok")]);

                let output = request
                    .blocking_client()
                    .unwrap()
                    .get("http://h")
                    .send_blocking(&request)
                    .unwrap();

                assert_eq!(output.body, b"ok");
                assert_eq!(request.sleeps(), [Duration::from_secs(1)]);
            }

            #[test]
            fn retry_should_not_retry_failures_unsafe_to_retry() {
                let request = test_request();
                request.set_response("", output(500, b""));

                let result = request
                    .blocking_client()
                    .unwrap()
                    .post("http://h")
                    .send_blocking(&request);

                assert!(result.is_err());
                assert_eq!(request.calls().len(), 1);
            }

            #[test]
            fn retry_should_wait_at_least_retry_after() {
                let request = test_request();
                request.set_responses("", [asking("5"), asking(" 30 "), output(200, b"")]);

                request
                    .blocking_client()
                    .unwrap()
                    .get("http://h")
                    .send_blocking(&request)
                    .unwrap();

                assert_eq!(
                    request.sleeps(),
                    [Duration::from_secs(5), Duration::from_secs(30)]
                );
            }

            #[test]
            fn retry_when_should_decide_instead_of_safety() {
                let request = TestRequest::new();
                request.add_interceptor(
                    HttpRequest::retry(1, Duration::ZERO).when(|_, result| result.is_ok()),
                );
                request.set_response("", output(404, b""));

                let _ = request
                    .blocking_client()
                    .unwrap()
                    .post("http://h")
                    .send_blocking(&request);

                assert_eq!(request.calls().len(), 2);
            }
        }

        mod safe_to_retry {
            use super::*;

            fn transport(kind: TransportErrorKind) -> Result<RequestOutput> {
                Err(crate::Error::request("transport").with_source(TransportError::new(kind)))
            }

            /// Returns whether each failure of `method` is safe to retry.
            fn safe(method: Method) -> Vec<bool> {
                let request = subject(method);

                [
                    Ok(RequestOutput {
                        success: true,
                        ..output(500, b"")
                    }),
                    Err(crate::Error::request("other")),
                    transport(TransportErrorKind::Connect),
                    transport(TransportErrorKind::Timeout),
                    transport(TransportErrorKind::Other),
                    Ok(output(404, b"")),
                    Ok(output(408, b"")),
                    Ok(output(429, b"")),
                    Ok(output(500, b"")),
                    Ok(output(502, b"")),
                    Ok(output(503, b"")),
                ]
                .iter()
                .map(|result| request.safe_to_retry(result))
                .collect()
            }

            #[test]
            fn safe_to_retry_idempotent_method_should_accept_transient_failures() {
                for method in [
                    Method::GET,
                    Method::HEAD,
                    Method::PUT,
                    Method::DELETE,
                    Method::OPTIONS,
                    Method::TRACE,
                ] {
                    assert_eq!(
                        safe(method.clone()),
                        [
                            false, false, true, true, true, false, true, true, true, true, true
                        ],
                        "method: {method}"
                    );
                }
            }

            #[test]
            fn safe_to_retry_non_idempotent_method_should_accept_failures_before_processing_only() {
                for method in [Method::POST, Method::PATCH, Method::CONNECT] {
                    assert_eq!(
                        safe(method.clone()),
                        [
                            false, false, true, false, false, false, true, true, false, false, true
                        ],
                        "method: {method}"
                    );
                }
            }
        }

        mod set_body {
            use super::*;

            #[test]
            fn set_body_should_replace_or_remove_body_sent() {
                for (body, expected) in [(Some(b"new".to_vec()), "\r\n\r\nnew"), (None, "\r\n\r\n")]
                {
                    let server = serve(ok);
                    let request = RequestImpl::new();
                    let mut interceptors = Interceptors::new();
                    let body = Arc::new(body);
                    interceptors.push(SetBody(body.clone()));

                    request
                        .blocking_client()
                        .unwrap()
                        .post(&server.url)
                        .body("old")
                        .interceptors(interceptors)
                        .send_blocking(&request)
                        .unwrap();

                    assert!(server.received()[0].ends_with(expected), "{body:?}");
                }
            }

            /// Sets the body before passing the run on.
            struct SetBody(Arc<Option<Vec<u8>>>);

            impl Interceptor<HttpRequest> for SetBody {
                fn handle<'a>(
                    &'a self,
                    mut next: Next<'a, HttpRequest>,
                ) -> BoxFuture<'a, Result<RequestOutput>> {
                    next.run_mut().subject_mut().set_body((*self.0).clone());
                    next.forward()
                }
            }
        }

        mod set_method {
            use super::*;

            #[test]
            fn set_method_should_change_method_sent() {
                let server = serve(ok);
                let request = intercepted(Change(|request| request.set_method(Method::PUT)));

                blocking_get(&request, &server.url)
                    .send_blocking(&request)
                    .unwrap();

                assert!(server.received()[0].starts_with("PUT / "));
            }
        }

        mod set_timeout {
            use super::*;

            #[test]
            fn set_timeout_should_change_timeout_of_request() {
                let server = serve(slow);
                let request = intercepted(Change(|request| {
                    request.set_timeout(Some(Duration::from_millis(50)));
                }));

                let result = blocking_get(&request, &server.url).send_blocking(&request);

                assert_eq!(transport_kind(&result), Some(TransportErrorKind::Timeout));
            }
        }

        mod target {
            use super::*;

            #[test]
            fn target_should_strip_credentials_query_and_fragment() {
                let url = Url::parse("https://user:secret@example.com:8080/path?token=secret#part")
                    .unwrap();
                let request = reqwest::blocking::Request::new(Method::GET, url);

                let target = HttpRequest::from_blocking(request, RequestOptions::default())
                    .unwrap()
                    .target();

                assert_eq!(target, "GET https://example.com:8080/path");
            }
        }

        mod timeout {
            use super::*;

            #[test]
            fn timeout_should_return_timeout_of_request() {
                let mut request = subject(Method::GET);
                assert_eq!(request.timeout(), None);

                request.set_timeout(Some(Duration::from_secs(1)));

                assert_eq!(request.timeout(), Some(Duration::from_secs(1)));
            }
        }

        mod url {
            use super::*;

            #[test]
            fn url_should_return_url() {
                assert_eq!(subject(Method::GET).url().as_str(), "http://h/");
            }
        }

        mod url_mut {
            use super::*;

            #[test]
            fn url_mut_should_change_url_sent() {
                let server = serve(ok);
                let request = intercepted(Change(|request| request.url_mut().set_path("/other")));

                blocking_get(&request, &server.url)
                    .send_blocking(&request)
                    .unwrap();

                assert!(server.received()[0].starts_with("GET /other "));
            }
        }

        mod blocking_request {
            use super::*;

            #[test]
            fn blocking_request_should_copy_request() {
                let mut request = subject(Method::POST);
                request.headers_mut().insert("x-key", "v".parse().unwrap());
                request.set_timeout(Some(Duration::from_secs(1)));
                request.set_body(Some(b"data".to_vec()));
                request.version = Version::HTTP_2;

                let sent = request.blocking_request();

                assert_eq!(sent.method(), Method::POST);
                assert_eq!(sent.url().as_str(), "http://h/");
                assert_eq!(sent.headers()["x-key"], "v");
                assert_eq!(sent.timeout(), Some(&Duration::from_secs(1)));
                assert_eq!(sent.version(), Version::HTTP_2);
                assert_eq!(
                    sent.body().and_then(reqwest::blocking::Body::as_bytes),
                    Some(b"data".as_slice())
                );
            }
        }

        #[cfg(feature = "tokio")]
        mod async_request {
            use super::*;

            #[test]
            fn async_request_should_copy_request() {
                let mut request = subject(Method::POST);
                request.headers_mut().insert("x-key", "v".parse().unwrap());
                request.set_timeout(Some(Duration::from_secs(1)));
                request.set_body(Some(b"data".to_vec()));
                request.version = Version::HTTP_2;

                let sent = request.async_request();

                assert_eq!(sent.method(), Method::POST);
                assert_eq!(sent.url().as_str(), "http://h/");
                assert_eq!(sent.headers()["x-key"], "v");
                assert_eq!(sent.timeout(), Some(&Duration::from_secs(1)));
                assert_eq!(sent.version(), Version::HTTP_2);
                assert_eq!(
                    sent.body().and_then(reqwest::Body::as_bytes),
                    Some(b"data".as_slice())
                );
            }
        }

        mod from_blocking {
            use super::*;

            #[test]
            fn from_blocking_should_read_body_into_memory() {
                for body in [
                    None,
                    Some(reqwest::blocking::Body::from("data")),
                    Some(reqwest::blocking::Body::new(std::io::Cursor::new(b"data"))),
                ] {
                    let expected = body.as_ref().map(|_| b"data".as_slice());
                    let mut request = reqwest::blocking::Request::new(
                        Method::POST,
                        Url::parse("http://h").unwrap(),
                    );
                    *request.body_mut() = body;

                    let request =
                        HttpRequest::from_blocking(request, RequestOptions::default()).unwrap();

                    assert_eq!(request.body(), expected);
                }
            }

            #[test]
            fn from_blocking_unreadable_body_should_fail() {
                let mut request =
                    reqwest::blocking::Request::new(Method::POST, Url::parse("http://h").unwrap());
                *request.body_mut() = Some(reqwest::blocking::Body::new(FailingReader));

                let error = HttpRequest::from_blocking(request, RequestOptions::default())
                    .err()
                    .unwrap();

                assert!(
                    error
                        .description()
                        .starts_with("request body could not be read: "),
                    "{error}"
                );
            }
        }

        #[cfg(feature = "tokio")]
        mod from_async {
            use super::*;

            #[test]
            fn from_async_should_read_body_into_memory() {
                for body in [
                    None,
                    Some(reqwest::Body::from("data")),
                    Some(streamed(b"data")),
                ] {
                    let expected = body.as_ref().map(|_| b"data".as_slice());
                    let mut request =
                        reqwest::Request::new(Method::POST, Url::parse("http://h").unwrap());
                    *request.body_mut() = body;

                    let request =
                        block_on(HttpRequest::from_async(request, RequestOptions::default()))
                            .unwrap();

                    assert_eq!(request.body(), expected);
                }
            }

            #[test]
            fn from_async_unreadable_body_should_fail() {
                let mut request =
                    reqwest::Request::new(Method::POST, Url::parse("http://h").unwrap());
                *request.body_mut() = Some(reqwest::Body::wrap(FailingBody));

                let error = block_on(HttpRequest::from_async(request, RequestOptions::default()))
                    .err()
                    .unwrap();

                assert!(
                    error
                        .description()
                        .starts_with("request body could not be read: "),
                    "{error}"
                );
            }
        }

        mod subject {
            use super::*;

            mod validate {
                use super::*;

                #[test]
                fn validate_should_accept_every_request() {
                    assert!(HttpRequest::validate(&Run::new(subject(Method::GET))).is_ok());
                }
            }

            mod replay {
                use super::*;

                #[test]
                fn replay_should_pass_body_of_success_only_to_sink() {
                    for (status, sunk, body) in
                        [(200, b"set".as_slice(), b"".as_slice()), (404, b"", b"set")]
                    {
                        let request = intercepted(DryRun);
                        let mut received = Vec::new();

                        let result = blocking_get(&request, "http://example.invalid/")
                            .with(DryRunOutput(output(status, b"set")))
                            .send_blocking_to(&request, &mut collect_into(&mut received));

                        let output =
                            result.unwrap_or_else(|error| error.request_output().unwrap().clone());
                        assert_eq!(received, sunk, "status: {status}");
                        assert_eq!(output.body, body, "status: {status}");
                    }
                }

                #[test]
                fn replay_empty_body_should_pass_nothing() {
                    let (request, recorder) = observed();
                    request.add_interceptor(DryRun);

                    let output = blocking_get(&request, "http://example.invalid/")
                        .keep_partial_body()
                        .send_blocking(&request)
                        .unwrap();

                    assert_eq!(output, output_of(200));
                    assert_eq!(
                        recorder.events(),
                        [
                            "started GET http://example.invalid/".to_string(),
                            "finished 200".to_string()
                        ]
                    );
                }

                fn output_of(status: u16) -> RequestOutput {
                    output(status, b"")
                }
            }

            mod finish {
                use super::*;

                #[test]
                fn finish_should_set_success_from_status_and_display_lines() {
                    let request = intercepted(DryRun);

                    let ok = blocking_get(&request, "http://example.invalid/")
                        .display_lines(2)
                        .send_blocking(&request)
                        .unwrap();
                    let accepted = blocking_get(&request, "http://example.invalid/")
                        .with(DryRunOutput(RequestOutput {
                            success: true,
                            ..output(404, b"")
                        }))
                        .send_blocking(&request)
                        .unwrap_err();

                    assert!(ok.success);
                    assert_eq!(ok.display_lines, 2);
                    assert!(!accepted.request_output().unwrap().success);
                }
            }
        }
    }

    mod request_options {
        use super::*;

        mod default {
            use super::*;

            #[test]
            fn default_should_show_ten_lines_without_keeping_partial_body() {
                let options = RequestOptions::default();

                assert_eq!(options.display_lines, 10);
                assert!(!options.keep_partial_body);
            }
        }
    }

    mod default {
        use super::*;

        #[test]
        fn default_should_cache_get_and_head_by_cache_key() {
            let cache = RequestCache::default();
            let request = TestRequest::new();
            request.add_interceptor(cache.clone());
            request.set_response("", output(200, b"body"));
            let client = request.blocking_client().unwrap();

            for _ in 0..2 {
                client.get("http://h/a").send_blocking(&request).unwrap();
                client.post("http://h/a").send_blocking(&request).unwrap();
            }

            assert_eq!(request.calls().len(), 3);
            assert_eq!(cache.entries()[0].0, "GET http://h/a");
        }
    }

    mod request_output {
        use super::*;

        mod retry_after {
            use super::*;

            fn asking(retry_after: &str) -> RequestOutput {
                let mut output = output(503, b"");
                output
                    .headers
                    .insert(reqwest::header::RETRY_AFTER, retry_after.parse().unwrap());
                output
            }

            #[test]
            fn retry_after_should_read_seconds_or_http_date() {
                for (value, expected) in [
                    ("5", Some(Duration::from_secs(5))),
                    (" 30 ", Some(Duration::from_secs(30))),
                    ("Sun, 06 Nov 1994 08:49:37 GMT", Some(Duration::ZERO)),
                    ("soon", None),
                    ("\u{e9}", None),
                ] {
                    assert_eq!(asking(value).retry_after(), expected, "value: {value}");
                }
                assert_eq!(output(503, b"").retry_after(), None);
            }

            #[test]
            fn retry_after_future_http_date_should_wait_until_then() {
                let after = asking("Sun, 06 Nov 9994 08:49:37 GMT").retry_after();

                assert!(after.unwrap() > Duration::from_secs(1 << 37));
            }
        }

        mod text {
            use super::*;

            #[test]
            fn text_should_return_utf8_body() {
                assert_eq!(output(200, b"body").text().unwrap(), "body");
            }

            #[test]
            fn text_invalid_utf8_should_return_error() {
                let error = output(200, b"\xFF").text().unwrap_err();

                assert!(error.description().starts_with("body is not valid UTF-8: "));
                assert!(error.source_as::<std::str::Utf8Error>().is_some());
            }
        }

        mod text_lossy {
            use super::*;

            #[test]
            fn text_lossy_should_replace_invalid_sequences() {
                assert_eq!(output(200, b"body\xFF").text_lossy(), "body\u{FFFD}");
            }
        }

        mod outcome {
            use super::*;

            mod success {
                use super::*;

                #[test]
                fn success_should_return_success_field() {
                    let output = RequestOutput {
                        success: true,
                        ..output(500, b"")
                    };

                    assert!(Outcome::success(&output));
                    assert!(!Outcome::success(&RequestOutput::default()));
                }
            }
        }

        mod std_fmt_display {
            use super::*;

            #[test]
            fn fmt_should_show_status_duration_and_last_lines_of_body() {
                let body: String = (1..=12).map(|line| format!("{line}\n")).collect();
                let output = RequestOutput {
                    status: 500,
                    body: body.into_bytes(),
                    duration: Duration::from_millis(5),
                    ..Default::default()
                };

                assert_eq!(
                    output.to_string(),
                    "status: 500\nduration: 5ms\nbody:\n(2 lines omitted)\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n"
                );
                assert_eq!(
                    RequestOutput {
                        display_lines: 1,
                        ..output
                    }
                    .to_string(),
                    "status: 500\nduration: 5ms\nbody:\n(11 lines omitted)\n12\n"
                );
            }
        }

        mod std_fmt_debug {
            use super::*;

            #[test]
            fn fmt_should_show_fields_and_body() {
                assert_eq!(
                    format!("{:?}", output(200, b"body")),
                    "RequestOutput { success: false, status: 200, headers: {}, body: 4 bytes \"body\", \
                     duration: 0ns, display_lines: 10 }"
                );
            }

            #[test]
            fn fmt_large_body_should_show_length_and_start_only() {
                let output = RequestOutput {
                    status: 500,
                    body: vec![b'x'; 100_000],
                    ..Default::default()
                };

                let debug = format!("{output:?}");

                assert!(debug.len() < 500, "{debug}");
                assert!(debug.contains(&format!(
                    "body: 100000 bytes \"{}\"...,",
                    "x".repeat(DEBUG_BODY_BYTES)
                )));
            }

            #[test]
            fn fmt_should_bound_debug_of_errors_holding_large_output() {
                let output = RequestOutput {
                    status: 500,
                    body: vec![b'x'; 100_000],
                    ..Default::default()
                };
                let status = crate::Error::request("status").with_source(output.clone());
                let transport = crate::Error::request("transport").with_source(
                    TransportError::new(TransportErrorKind::Other).with_output(output),
                );

                for error in [status, transport] {
                    let debug = format!("{error:?}");

                    assert!(debug.len() < 1000, "{debug}");
                    assert!(debug.contains("100000 bytes"), "{debug}");
                }
            }
        }

        mod default {
            use super::*;

            #[test]
            fn default_should_be_empty_200_displaying_10_lines() {
                let output = RequestOutput::default();

                assert!(!output.success);
                assert_eq!(output.status, 200);
                assert!(output.headers.is_empty());
                assert!(output.body.is_empty());
                assert_eq!(output.duration, Duration::ZERO);
                assert_eq!(output.display_lines, 10);
            }
        }

        mod partial_eq {
            use super::*;

            mod eq {
                use super::*;

                #[test]
                fn eq_should_ignore_success_and_display_lines() {
                    let output = output(200, b"body");

                    assert_eq!(
                        output,
                        RequestOutput {
                            success: true,
                            display_lines: 1,
                            ..output.clone()
                        }
                    );
                    for other in [
                        RequestOutput {
                            status: 404,
                            ..output.clone()
                        },
                        RequestOutput {
                            headers: HeaderMap::from_iter([(
                                reqwest::header::ETAG,
                                "tag".parse().unwrap(),
                            )]),
                            ..output.clone()
                        },
                        RequestOutput {
                            body: b"other".to_vec(),
                            ..output.clone()
                        },
                        RequestOutput {
                            duration: Duration::from_secs(1),
                            ..output.clone()
                        },
                    ] {
                        assert_ne!(output, other);
                    }
                }
            }
        }
    }

    mod received {
        use super::*;

        mod interrupted {
            use super::*;

            #[test]
            fn interrupted_should_keep_partial_body_as_passed_on() {
                // GIVEN an interceptor redacting a secret of a body cut off
                struct Redact;

                impl Interceptor<HttpRequest> for Redact {
                    fn chunk(&self, mut chunk: Chunk<HttpRequest>) -> Result {
                        let data = String::from_utf8_lossy(chunk.data()).replace("secret", "***");
                        chunk.set_data(data.into_bytes());
                        chunk.forward()
                    }
                }

                let request = TestRequest::new();
                request.add_interceptor(Redact);
                request.set_interrupted(
                    "",
                    output(200, b""),
                    [b"token=secret".to_vec()],
                    TransportErrorKind::Other,
                );

                // WHEN it is received keeping the partial body
                let error = request
                    .blocking_client()
                    .unwrap()
                    .get("https://example.com")
                    .keep_partial_body()
                    .send_blocking(&request)
                    .unwrap_err();

                // THEN the body kept is the redacted one
                let body = &error
                    .source_as::<TransportError>()
                    .unwrap()
                    .output()
                    .unwrap()
                    .body;
                assert_eq!(body, b"token=***");
            }
        }
    }

    mod transport_error {
        use super::*;

        mod new {
            use super::*;

            #[test]
            fn new_should_create_error_without_source() {
                let error = TransportError::new(TransportErrorKind::Timeout);

                assert_eq!(error.kind(), TransportErrorKind::Timeout);
                assert!(error.source().is_none());
                assert!(error.output().is_none());
            }
        }

        mod kind {
            use super::*;

            #[test]
            fn kind_should_return_kind() {
                assert_eq!(
                    TransportError::new(TransportErrorKind::Connect).kind(),
                    TransportErrorKind::Connect
                );
            }
        }

        mod output {
            use super::*;

            #[test]
            fn output_should_return_response_recorded() {
                let output = output(200, b"");

                let error =
                    TransportError::new(TransportErrorKind::Other).with_output(output.clone());

                assert_eq!(error.output(), Some(&output));
            }
        }

        mod with_output {
            use super::*;

            #[test]
            fn with_output_should_keep_kind() {
                let error = TransportError::new(TransportErrorKind::Other)
                    .with_output(RequestOutput::default());

                assert_eq!(error.kind(), TransportErrorKind::Other);
            }
        }

        mod from_io {
            use super::*;

            #[test]
            fn from_io_should_classify_reqwest_error_it_holds() {
                let failure = reqwest::blocking::Client::new()
                    .get(closed_url())
                    .send()
                    .unwrap_err();

                let error = TransportError::from_io(std::io::Error::other(failure));

                assert_eq!(error.kind(), TransportErrorKind::Connect);
                assert!(error.source().unwrap().is::<reqwest::Error>());
            }

            #[test]
            fn from_io_other_error_should_keep_it_as_source() {
                let error = TransportError::from_io(std::io::Error::other("reset"));

                assert_eq!(error.kind(), TransportErrorKind::Other);
                assert_eq!(error.to_string(), "failed: reset");
            }
        }

        mod from {
            use super::*;

            #[test]
            fn from_should_classify_reqwest_errors_without_url() {
                // GIVEN a connect, a timeout and a body failure
                let client = reqwest::blocking::Client::new();
                let slow_server = serve(slow);
                let truncated_server = serve(truncated);
                let failures = [
                    (
                        client.get(closed_url()).send().unwrap_err(),
                        TransportErrorKind::Connect,
                    ),
                    (
                        client
                            .get(&slow_server.url)
                            .timeout(Duration::from_millis(50))
                            .send()
                            .unwrap_err(),
                        TransportErrorKind::Timeout,
                    ),
                    (
                        client
                            .get(&truncated_server.url)
                            .send()
                            .unwrap()
                            .bytes()
                            .unwrap_err(),
                        TransportErrorKind::Other,
                    ),
                ];

                for (failure, expected) in failures {
                    // WHEN converting them
                    let error = TransportError::from(failure);

                    // THEN they are classified and the URL is dropped
                    assert_eq!(error.kind(), expected);
                    assert!(!error.to_string().contains("127.0.0.1"), "{error}");
                }
            }
        }

        mod std_fmt_display {
            use super::*;

            #[test]
            fn fmt_should_show_kind_and_source_chain() {
                let failure = reqwest::blocking::Client::new()
                    .get(closed_url())
                    .send()
                    .unwrap_err();
                let chain: String =
                    std::iter::successors(failure.source(), |&error| error.source())
                        .map(|error| format!(": {error}"))
                        .collect();
                let error = TransportError::from(failure);

                assert_eq!(
                    error.to_string(),
                    format!("failed to connect: error sending request{chain}")
                );
                assert_eq!(
                    TransportError::new(TransportErrorKind::Timeout).to_string(),
                    "timed out"
                );
            }
        }

        mod std_error_error {
            use super::*;

            mod source {
                use super::*;

                #[test]
                fn source_should_return_reqwest_error() {
                    let failure = reqwest::blocking::Client::new()
                        .get(closed_url())
                        .send()
                        .unwrap_err();

                    let error = TransportError::from(failure);

                    assert!(error.source().unwrap().is::<reqwest::Error>());
                }
            }
        }
    }

    mod transport_error_kind {
        use super::*;

        mod std_fmt_display {
            use super::*;

            #[test]
            fn fmt_should_describe_kind() {
                for (kind, displayed) in [
                    (TransportErrorKind::Connect, "failed to connect"),
                    (TransportErrorKind::Timeout, "timed out"),
                    (TransportErrorKind::Other, "failed"),
                ] {
                    assert_eq!(kind.to_string(), displayed);
                }
            }
        }
    }

    mod request_impl {
        use super::*;

        mod default {
            use super::*;

            #[test]
            fn default_should_send_requests() {
                let server = serve(ok);
                let request = RequestImpl::default();

                let output = blocking_get(&request, &server.url)
                    .send_blocking(&request)
                    .unwrap();

                assert_eq!(output.status, 200);
            }
        }

        mod with_env {
            use super::*;

            #[test]
            fn with_env_should_send_through_proxy_of_env() {
                let proxy = serve(ok);
                let env = crate::env::EnvImpl::new(Vec::new(), std::path::PathBuf::new());
                crate::Env::set_var(&env, "HTTP_PROXY", proxy.url.as_str()).unwrap();
                let request = RequestImpl::with_env(env.clone());

                blocking_get(&request, "http://example.invalid/path")
                    .send_blocking(&request)
                    .unwrap();

                assert!(proxy.received()[0].starts_with("GET http://example.invalid/path "));
            }
        }

        mod request {
            use super::*;

            mod blocking_client_builder {
                use super::*;

                #[test]
                fn blocking_client_builder_should_use_proxies_of_env() {
                    let proxy = serve(ok);
                    let request = RequestImpl::new();
                    request.set_var("HTTP_PROXY", &proxy.url);
                    let client = request.blocking_client_builder().build().unwrap();

                    client
                        .get("http://example.invalid/")
                        .send_blocking(&request)
                        .unwrap();

                    assert_eq!(proxy.received().len(), 1);
                }
            }

            #[cfg(feature = "tokio")]
            mod client_builder {
                use super::*;

                #[test]
                fn client_builder_should_use_proxies_of_env() {
                    let proxy = serve(ok);
                    let request = RequestImpl::new();
                    request.set_var("HTTP_PROXY", &proxy.url);
                    let client = request.client_builder().build().unwrap();

                    block_on(client.get("http://example.invalid/").send_async(&request)).unwrap();

                    assert_eq!(proxy.received().len(), 1);
                }
            }

            mod blocking_client {
                use super::*;

                #[test]
                fn blocking_client_invalid_builder_should_return_error_every_time() {
                    let request = RequestImpl::new();
                    request.set_blocking_client(invalid_client());

                    let first = request.blocking_client().unwrap_err();
                    let second = request.blocking_client().unwrap_err();

                    assert!(
                        first
                            .description()
                            .starts_with("client could not be built: ")
                    );
                    assert_eq!(first.description(), second.description());
                }
            }

            mod set_blocking_client {
                use super::*;

                #[test]
                fn set_blocking_client_should_replace_default_client() {
                    let server = serve(ok);
                    let request = RequestImpl::new();
                    request.blocking_client().unwrap();

                    request.set_blocking_client(
                        reqwest::blocking::Client::builder().user_agent("agplatform-test"),
                    );
                    blocking_get(&request, &server.url)
                        .send_blocking(&request)
                        .unwrap();

                    assert!(server.received()[0].contains("user-agent: agplatform-test\r\n"));
                }
            }

            mod send_blocking {
                use super::*;

                #[test]
                fn send_blocking_should_return_response_and_pass_run_to_interceptors() {
                    let server = serve(ok);
                    let (request, recorder) = observed();
                    let url = format!("{}/path?token=secret", server.url);

                    let output = blocking_get(&request, &url)
                        .send_blocking(&request)
                        .unwrap();

                    assert!(output.success);
                    assert_eq!(output.status, 200);
                    assert_eq!(output.headers["content-length"], "4");
                    assert_eq!(output.body, b"body");
                    assert_eq!(
                        recorder.events(),
                        [
                            format!("started GET {}/path", server.url),
                            "finished 200".to_string()
                        ]
                    );
                }

                #[test]
                fn send_blocking_should_send_request_body_and_headers() {
                    let server = serve(ok);
                    let request = RequestImpl::new();

                    request
                        .blocking_client()
                        .unwrap()
                        .post(&server.url)
                        .bearer_auth("token")
                        .body("data")
                        .send_blocking(&request)
                        .unwrap();

                    let received = &server.received()[0];
                    assert!(received.starts_with("POST / HTTP/1.1\r\n"), "{received}");
                    assert!(received.contains("authorization: Bearer token\r\n"));
                    assert!(received.ends_with("\r\n\r\ndata"));
                }

                #[test]
                fn send_blocking_failed_status_should_return_error_with_output() {
                    let server = serve(not_found);
                    let (request, recorder) = observed();

                    let error =
                        blocking_get(&request, &format!("{}/path?token=secret", server.url))
                            .send_blocking(&request)
                            .unwrap_err();

                    let description = format!("'GET {}/path' failed with status 404", server.url);
                    assert_eq!(error.description(), description);
                    assert_eq!(
                        error.request_target(),
                        Some(format!("GET {}/path", server.url).as_str())
                    );
                    let output = error.request_output().unwrap();
                    assert_eq!(output.status, 404);
                    assert_eq!(output.body, b"missing");
                    assert_eq!(recorder.events().last().unwrap(), "finished 404");
                }

                #[test]
                fn send_blocking_display_lines_should_limit_displayed_body() {
                    let server = serve(|_, _| response(500, "1\n2\n3"));
                    let request = RequestImpl::new();

                    let error = blocking_get(&request, &server.url)
                        .display_lines(1)
                        .send_blocking(&request)
                        .unwrap_err();

                    let output = error.request_output().unwrap();
                    assert_eq!(output.display_lines, 1);
                    assert!(output.to_string().ends_with("body:\n(2 lines omitted)\n3"));
                }

                #[test]
                fn send_blocking_invalid_request_should_return_error_bypassing_interceptors() {
                    let (request, recorder) = observed();

                    let error = blocking_get(&request, "not a url")
                        .send_blocking(&request)
                        .unwrap_err();

                    assert_eq!(error.description(), "invalid request: builder error");
                    assert!(recorder.events().is_empty());
                }

                #[test]
                fn send_blocking_unreadable_body_should_return_error_bypassing_interceptors() {
                    let (request, recorder) = observed();

                    let error = request
                        .blocking_client()
                        .unwrap()
                        .post(closed_url())
                        .body(reqwest::blocking::Body::new(FailingReader))
                        .send_blocking(&request)
                        .unwrap_err();

                    assert!(
                        error
                            .description()
                            .starts_with("request body could not be read: "),
                        "{error}"
                    );
                    assert!(recorder.events().is_empty());
                }

                #[test]
                fn send_blocking_connect_failure_should_return_connect_transport_error() {
                    let url = closed_url();
                    let request = RequestImpl::new();

                    let result = blocking_get(&request, &url).send_blocking(&request);

                    assert_eq!(transport_kind(&result), Some(TransportErrorKind::Connect));
                    assert!(
                        result
                            .unwrap_err()
                            .description()
                            .starts_with(&format!("'GET {url}/' failed to connect: "))
                    );
                }

                #[test]
                fn send_blocking_timeout_should_return_timeout_transport_error() {
                    let server = serve(slow);
                    let request = RequestImpl::new();

                    let result = blocking_get(&request, &server.url)
                        .timeout(Duration::from_millis(50))
                        .send_blocking(&request);

                    assert_eq!(transport_kind(&result), Some(TransportErrorKind::Timeout));
                }

                #[test]
                fn send_blocking_slow_body_should_time_out_each_read_not_whole_body() {
                    // GIVEN a client whose read timeout is shorter than the body takes
                    let server =
                        serve_chunks(Duration::from_millis(100), &["a", "b", "c", "d", "e", "f"]);
                    let request = RequestImpl::new();
                    let client = reqwest::blocking::Client::builder()
                        .timeout(Duration::from_millis(500))
                        .build()
                        .unwrap();

                    // WHEN receiving the body whole
                    let result = client.get(&server.url).send_blocking(&request);

                    // THEN it is not cut
                    assert_eq!(result.unwrap().body, b"abcdef");
                }

                #[test]
                fn send_blocking_truncated_body_should_return_other_transport_error() {
                    for stream in [false, true] {
                        let server = serve(truncated);
                        let request = RequestImpl::new();
                        let builder = blocking_get(&request, &server.url);
                        let builder = if stream {
                            builder.keep_partial_body()
                        } else {
                            ConfiguredRequest::new(builder)
                        };

                        let result = builder.send_blocking(&request);

                        assert_eq!(
                            transport_kind(&result),
                            Some(TransportErrorKind::Other),
                            "stream: {stream}"
                        );
                        let partial: &[u8] = if stream { b"partial" } else { b"" };
                        assert_eq!(interrupted_output(&result).body, partial);
                    }
                }

                #[test]
                fn send_blocking_streaming_should_pass_body_between_start_and_end() {
                    let server = serve(ok);
                    let (request, recorder) = observed();

                    let output = blocking_get(&request, &server.url)
                        .keep_partial_body()
                        .send_blocking(&request)
                        .unwrap();

                    assert_eq!(output.body, b"body");
                    assert_eq!(
                        recorder.events(),
                        [
                            format!("started GET {}/", server.url),
                            "body body".to_string(),
                            "finished 200".to_string()
                        ]
                    );
                }

                #[test]
                fn send_blocking_with_retry_should_retry_until_success() {
                    // GIVEN a server unavailable once
                    let server = serve(unavailable_once);
                    let request = RequestImpl::new();
                    let recorder = Recorder::default();

                    // WHEN sending a request retried around a recorder
                    let output = blocking_get(&request, &server.url)
                        .interceptors(chain(
                            HttpRequest::retry(2, Duration::ZERO),
                            recorder.clone(),
                        ))
                        .send_blocking(&request)
                        .unwrap();

                    // THEN the second attempt succeeds
                    assert_eq!(output.status, 200);
                    assert_eq!(
                        recorder.events(),
                        [
                            format!("started GET {}/", server.url),
                            "finished 503".to_string(),
                            format!("started GET {}/", server.url),
                            "finished 200".to_string()
                        ]
                    );
                }

                #[test]
                fn send_blocking_with_retry_should_send_streamed_body_by_each_attempt() {
                    let server = serve(unavailable_once);
                    let request = intercepted(HttpRequest::retry(1, Duration::ZERO));

                    request
                        .blocking_client()
                        .unwrap()
                        .post(&server.url)
                        .body(reqwest::blocking::Body::new(std::io::Cursor::new(b"data")))
                        .send_blocking(&request)
                        .unwrap();

                    let received = server.received();
                    assert_eq!(received.len(), 2);
                    assert!(
                        received
                            .iter()
                            .all(|request| request.ends_with("\r\n\r\ndata"))
                    );
                }
            }

            mod send_blocking_to {
                use super::*;

                #[test]
                fn send_blocking_to_should_pass_body_to_sink() {
                    let server = serve(ok);
                    let request = RequestImpl::new();
                    let mut received = Vec::new();

                    let output = blocking_get(&request, &server.url)
                        .send_blocking_to(&request, &mut collect_into(&mut received))
                        .unwrap();

                    assert_eq!(output.status, 200);
                    assert!(output.body.is_empty());
                    assert_eq!(received, b"body");
                }

                #[test]
                fn send_blocking_to_failed_status_should_return_body_in_output() {
                    let server = serve(not_found);
                    let request = RequestImpl::new();
                    let mut received = Vec::new();

                    let error = blocking_get(&request, &server.url)
                        .send_blocking_to(&request, &mut collect_into(&mut received))
                        .unwrap_err();

                    assert_eq!(error.request_output().unwrap().body, b"missing");
                    assert!(received.is_empty());
                }

                #[test]
                fn send_blocking_to_interrupted_body_should_keep_received_part() {
                    // GIVEN a server closing the connection in the middle of the body
                    let server = serve(truncated);
                    let request = RequestImpl::new();
                    let mut received = Vec::new();

                    // WHEN sending a streamed request to a sink
                    let result = blocking_get(&request, &server.url)
                        .keep_partial_body()
                        .send_blocking_to(&request, &mut collect_into(&mut received));

                    // THEN it fails after the sink got the part received
                    assert_eq!(transport_kind(&result), Some(TransportErrorKind::Other));
                    assert_eq!(received, b"partial");
                    let output = interrupted_output(&result);
                    assert_eq!(output.status, 200);
                    assert_eq!(output.headers["content-length"], "100");
                    assert!(output.body.is_empty());
                }

                #[test]
                fn send_blocking_to_interrupted_body_should_not_be_retried() {
                    let server = serve(truncated);
                    let request = intercepted(Retry::new(2, Duration::ZERO));

                    let result = blocking_get(&request, &server.url)
                        .send_blocking_to(&request, &mut collect_into(&mut Vec::new()));

                    assert_eq!(transport_kind(&result), Some(TransportErrorKind::Other));
                    assert_eq!(server.received().len(), 1);
                }

                #[test]
                fn send_blocking_to_failure_before_body_should_be_retried() {
                    let server = serve(unavailable_once);
                    let request = intercepted(HttpRequest::retry(1, Duration::ZERO));
                    let mut received = Vec::new();

                    let output = blocking_get(&request, &server.url)
                        .send_blocking_to(&request, &mut collect_into(&mut received))
                        .unwrap();

                    assert_eq!(output.status, 200);
                    assert_eq!(received, b"body");
                    assert_eq!(server.received().len(), 2);
                }

                #[test]
                fn send_blocking_to_failing_sink_should_return_its_error_without_retrying() {
                    let server = serve(ok);
                    let request = intercepted(Retry::new(2, Duration::ZERO));

                    let error = blocking_get(&request, &server.url)
                        .send_blocking_to(&request, &mut |_| Err(crate::Error::fs("disk full")))
                        .unwrap_err();

                    assert_eq!(error.description(), "disk full");
                    assert_eq!(server.received().len(), 1);
                }
            }

            #[cfg(feature = "tokio")]
            mod client {
                use super::*;

                #[test]
                fn client_invalid_builder_should_return_error_every_time() {
                    let request = RequestImpl::new();
                    request.set_client(invalid_async_client());

                    let first = request.client().unwrap_err();
                    let second = request.client().unwrap_err();

                    assert!(
                        first
                            .description()
                            .starts_with("client could not be built: ")
                    );
                    assert_eq!(first.description(), second.description());
                }
            }

            #[cfg(feature = "tokio")]
            mod set_client {
                use super::*;

                #[test]
                fn set_client_should_replace_default_client() {
                    let server = serve(ok);
                    let request = RequestImpl::new();
                    request.client().unwrap();

                    request.set_client(reqwest::Client::builder().user_agent("agplatform-test"));
                    block_on(get(&request, &server.url).send_async(&request)).unwrap();

                    assert!(server.received()[0].contains("user-agent: agplatform-test\r\n"));
                }
            }

            #[cfg(feature = "tokio")]
            mod send_async {
                use super::*;

                #[test]
                fn send_async_should_return_response_and_pass_run_to_interceptors() {
                    let server = serve(ok);
                    let (request, recorder) = observed();

                    let output = block_on(get(&request, &server.url).send_async(&request)).unwrap();

                    assert_eq!(output.status, 200);
                    assert_eq!(output.headers["content-length"], "4");
                    assert_eq!(output.body, b"body");
                    assert_eq!(
                        recorder.events(),
                        [
                            format!("started GET {}/", server.url),
                            "finished 200".to_string()
                        ]
                    );
                }

                #[test]
                fn send_async_failed_status_should_return_error_with_output() {
                    let server = serve(not_found);
                    let request = RequestImpl::new();

                    let error =
                        block_on(get(&request, &server.url).send_async(&request)).unwrap_err();

                    assert_eq!(error.request_output().unwrap().status, 404);
                }

                #[test]
                fn send_async_invalid_request_should_return_error_bypassing_interceptors() {
                    let (request, recorder) = observed();

                    let error =
                        block_on(get(&request, "not a url").send_async(&request)).unwrap_err();

                    assert_eq!(error.description(), "invalid request: builder error");
                    assert!(recorder.events().is_empty());
                }

                #[test]
                fn send_async_unreadable_body_should_return_error_bypassing_interceptors() {
                    let (request, recorder) = observed();
                    let builder = request
                        .client()
                        .unwrap()
                        .post(closed_url())
                        .body(reqwest::Body::wrap(FailingBody));

                    let error = block_on(builder.send_async(&request)).unwrap_err();

                    assert!(
                        error
                            .description()
                            .starts_with("request body could not be read: "),
                        "{error}"
                    );
                    assert!(recorder.events().is_empty());
                }

                #[test]
                fn send_async_connect_failure_should_return_connect_transport_error() {
                    let request = RequestImpl::new();

                    let result = block_on(get(&request, &closed_url()).send_async(&request));

                    assert_eq!(transport_kind(&result), Some(TransportErrorKind::Connect));
                }

                #[test]
                fn send_async_timeout_should_return_timeout_transport_error() {
                    let server = serve(slow);
                    let request = RequestImpl::new();

                    let result = block_on(
                        get(&request, &server.url)
                            .timeout(Duration::from_millis(50))
                            .send_async(&request),
                    );

                    assert_eq!(transport_kind(&result), Some(TransportErrorKind::Timeout));
                }

                #[test]
                fn send_async_truncated_body_should_return_other_transport_error() {
                    for stream in [false, true] {
                        let server = serve(truncated);
                        let request = RequestImpl::new();
                        let builder = get(&request, &server.url);
                        let builder = if stream {
                            builder.keep_partial_body()
                        } else {
                            ConfiguredRequest::new(builder)
                        };

                        let result = block_on(builder.send_async(&request));

                        assert_eq!(
                            transport_kind(&result),
                            Some(TransportErrorKind::Other),
                            "stream: {stream}"
                        );
                        let partial: &[u8] = if stream { b"partial" } else { b"" };
                        assert_eq!(interrupted_output(&result).body, partial);
                    }
                }

                #[test]
                fn send_async_with_retry_should_retry_until_success() {
                    let server = serve(unavailable_once);
                    let request = intercepted(HttpRequest::retry(1, Duration::ZERO));

                    let output = block_on(get(&request, &server.url).send_async(&request)).unwrap();

                    assert_eq!(output.status, 200);
                    assert_eq!(server.received().len(), 2);
                }

                #[test]
                fn send_async_cancelled_should_tell_interceptors() {
                    // GIVEN a slow server
                    let server = serve(slow);
                    let (request, recorder) = observed();
                    let builder = get(&request, &server.url);

                    // WHEN the request is cancelled by a timeout
                    let result = block_on(async {
                        tokio::time::timeout(
                            Duration::from_millis(100),
                            builder.send_async(&request),
                        )
                        .await
                    });

                    // THEN the interceptors are told
                    assert!(result.is_err());
                    assert_eq!(
                        recorder.events(),
                        [
                            format!("started GET {}/", server.url),
                            format!("cancelled GET {}/", server.url),
                        ]
                    );
                }
            }

            #[cfg(feature = "tokio")]
            mod send_async_to {
                use super::*;

                #[test]
                fn send_async_to_should_pass_body_to_sink() {
                    let server = serve(ok);
                    let (request, recorder) = observed();
                    let mut received = Vec::new();
                    let builder = get(&request, &server.url).keep_partial_body();

                    let output =
                        block_on(builder.send_async_to(&request, &mut collect_into(&mut received)))
                            .unwrap();

                    assert!(output.body.is_empty());
                    assert_eq!(received, b"body");
                    assert!(recorder.events().contains(&"body body".to_string()));
                }

                #[test]
                fn send_async_to_failed_status_should_return_body_in_output() {
                    let server = serve(not_found);
                    let request = RequestImpl::new();
                    let mut received = Vec::new();
                    let builder = get(&request, &server.url);

                    let error =
                        block_on(builder.send_async_to(&request, &mut collect_into(&mut received)))
                            .unwrap_err();

                    assert_eq!(error.request_output().unwrap().body, b"missing");
                    assert!(received.is_empty());
                }

                #[test]
                fn send_async_to_interrupted_body_should_keep_received_part() {
                    // GIVEN a server closing the connection in the middle of the body
                    let server = serve(truncated);
                    let request = RequestImpl::new();
                    let mut received = Vec::new();
                    let builder = get(&request, &server.url);

                    // WHEN sending a request to a sink
                    let result =
                        block_on(builder.send_async_to(&request, &mut collect_into(&mut received)));

                    // THEN it fails after the sink got the part received
                    assert_eq!(transport_kind(&result), Some(TransportErrorKind::Other));
                    assert_eq!(received, b"partial");
                    let output = interrupted_output(&result);
                    assert_eq!(output.status, 200);
                    assert_eq!(output.headers["content-length"], "100");
                    assert!(output.body.is_empty());
                }

                #[test]
                fn send_async_to_failing_sink_should_return_its_error() {
                    let server = serve(ok);
                    let request = RequestImpl::new();
                    let builder = get(&request, &server.url);

                    let error = block_on(
                        builder
                            .send_async_to(&request, &mut |_| Err(crate::Error::fs("disk full"))),
                    )
                    .unwrap_err();

                    assert_eq!(error.description(), "disk full");
                    assert_eq!(server.received().len(), 1);
                }
            }

            mod interceptors {
                use super::*;

                #[test]
                fn interceptors_should_be_none_by_default() {
                    assert!(RequestImpl::new().interceptors().is_empty());
                }
            }

            mod clear_interceptors {
                use super::*;

                #[test]
                fn clear_interceptors_should_remove_interceptors_of_later_runs() {
                    let (request, recorder) = observed();

                    request.clear_interceptors();
                    let _ = blocking_get(&request, &closed_url()).send_blocking(&request);

                    assert!(request.interceptors().is_empty());
                    assert!(recorder.events().is_empty());
                }
            }

            mod add_interceptor {
                use super::*;

                #[test]
                fn add_interceptor_should_add_innermost_interceptor() {
                    let (request, recorder) = observed();
                    let inner = Recorder::default();

                    request.add_interceptor(inner.clone());
                    let _ = blocking_get(&request, &closed_url()).send_blocking(&request);

                    assert_eq!(request.interceptors().len(), 2);
                    assert_eq!(inner.events().len(), 2);
                    assert_eq!(recorder.events().len(), 2);
                }
            }
        }
    }

    mod failure {
        use super::*;

        #[test]
        fn failure_should_return_error_with_output() {
            for (status, description) in [
                (404, "'GET http://h/' failed with status 404"),
                (200, "'GET http://h/' was not accepted with status 200"),
            ] {
                let error = failure(&subject(Method::GET), output(status, b""));

                assert_eq!(error.description(), description);
                assert_eq!(error.request_output().unwrap().status, status);
                assert_eq!(error.request_target(), Some("GET http://h/"));
            }
        }
    }

    mod http_date {
        use super::*;

        #[test]
        fn http_date_should_parse_all_formats() {
            for (value, seconds) in [
                ("Sun, 06 Nov 1994 08:49:37 GMT", 784_111_777),
                ("Thu, 01 Jan 1970 00:00:00 GMT", 0),
                ("Tue, 29 Feb 2000 23:59:59 GMT", 951_868_799),
                ("Sunday, 06-Nov-94 08:49:37 GMT", 784_111_777),
                ("Thursday, 01-Jan-70 00:00:00 GMT", 0),
                ("Tuesday, 29-Feb-00 23:59:59 GMT", 951_868_799),
                ("Sun Nov  6 08:49:37 1994", 784_111_777),
                ("Thu Jan 01 00:00:00 1970", 0),
            ] {
                assert_eq!(
                    http_date(value),
                    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)),
                    "value: {value}"
                );
            }
        }

        #[test]
        fn http_date_invalid_should_return_none() {
            for value in [
                "",
                "Sun, 06 Nov 1994 08:49:37 UTC",
                "Sun, 06 Nov 1994 08:49:37 GMT extra",
                "Sun, 06 Foo 1994 08:49:37 GMT",
                "Sun, xx Nov 1994 08:49:37 GMT",
                "Sun, 06 Nov 19x4 08:49:37 GMT",
                "Sun, 06 Nov 1994 xx:49:37 GMT",
                "Sun, 06 Nov 1994 08:xx:37 GMT",
                "Sun, 06 Nov 1994 08:49:xx GMT",
                "Sun, 06 Nov 1994 08 GMT",
                "Sun, 06 Nov 1994 08:49 GMT",
                "Sun, 06 Nov 1994 08:49:37:00 GMT",
                "Wed, 31 Dec 1969 23:59:59 GMT",
                "Sun, 00 Nov 1994 08:49:37 GMT",
                "Sun, 31 Nov 1994 08:49:37 GMT",
                "Sun, 29 Feb 1900 08:49:37 GMT",
                "Sun, 29 Feb 1994 08:49:37 GMT",
                "Sun, 06 Nov 1994 24:49:37 GMT",
                "Sun, 06 Nov 1994 08:60:37 GMT",
                "Sun, 06 Nov 1994 08:49:61 GMT",
                "Sun, 06 Nov 10000 08:49:37 GMT",
                "Sun, 06 Nov 9223372036854775807 08:49:37 GMT",
                "Sun, 06 Nov 1994 9223372036854775807:49:37 GMT",
                "Sunday, 06-Nov-1994 08:49:37 GMT",
                "Sunday, 06-Nov-94-01 08:49:37 GMT",
                "Sunday, 06-Nov-x4 08:49:37 GMT",
                "Sun Nov  6 08:49:37 19x4",
            ] {
                assert_eq!(http_date(value), None, "value: {value}");
            }
        }
    }
}
