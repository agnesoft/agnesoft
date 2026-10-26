use std::borrow::Cow;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;

use reqwest::Method;
use reqwest::Url;
use reqwest::header::HeaderMap;

use crate::Result;
use crate::env::SharedEnv;
use crate::utils::ObserverCell;

macro_rules! request_error {
    ($($arg:tt)*) => {
        $crate::Error::request(format!($($arg)*))
    };
}

/// Enabled by the `request` feature flag.
///
/// Observes the requests sent by a [`Request`], e.g. to log them.
///
/// Every run of a valid request is reported once to
/// [`started`](Self::started) and once to [`finished`](Self::finished).
/// An async run cancelled by dropping its future is finished with an error
/// whose source is [`RequestCancelled`]. Requests that cannot be built
/// (e.g. with an invalid URL) are not reported, nor is the end of a run
/// that panics. Each
/// failed attempt that is retried (see [`RequestOptions::retries`]) is
/// reported to [`retrying`](Self::retrying), the attempt being available
/// from [`RequestRun::attempt`]. The response body of each attempt of a
/// run with [`RequestOptions::stream`] is reported to [`body`](Self::body)
/// as it arrives.
///
/// Installed with [`Request::set_observer`], e.g. on
/// [`Platform::request`](crate::Platform::request).
///
/// # Examples
///
/// ```rust
/// use agplatform::{BlockingRequestBuilderExt, Request, RequestObserver, RequestOutput, RequestRun, Result};
/// use std::sync::{Arc, Mutex};
///
/// #[derive(Default)]
/// struct Failures(Mutex<Vec<String>>);
///
/// impl RequestObserver for Failures {
///     fn finished(&self, run: &RequestRun, result: &Result<RequestOutput>) {
///         if let Err(error) = result {
///             self.0.lock().unwrap().push(error.to_string());
///         }
///     }
/// }
///
/// let failures = Arc::new(Failures::default());
/// let mut request = agplatform::TestRequest::new();
/// request.set_observer(failures.clone());
/// request.set_response("", RequestOutput { status: 404, ..Default::default() });
///
/// let client = request.blocking_client().unwrap();
/// let _ = client.get("https://example.com/missing?token=secret").send_blocking(&request);
///
/// assert_eq!(
///     *failures.0.lock().unwrap(),
///     ["[Request] 'GET https://example.com/missing' failed with status 404"]
/// );
/// ```
#[allow(unused_variables)]
pub trait RequestObserver: Send + Sync {
    /// Called with the error of a request that cannot be built, e.g. with
    /// an invalid URL or header, so that there is no run. A request whose
    /// body fails to be read before it is sent is reported as a run
    /// finished with the error instead.
    fn invalid(&self, error: &crate::Error) {}

    /// Called before the request is sent.
    fn started(&self, run: &RequestRun) {}

    /// Called with each chunk of the response body of a streamed run as it
    /// arrives. The chunks are as received, not split into lines.
    fn body(&self, run: &RequestRun, data: &[u8]) {}

    /// Called with the failed `result` of an attempt before the request is
    /// sent again after [`RequestOptions::retry_delay`].
    fn retrying(&self, run: &RequestRun, result: &Result<RequestOutput>) {}

    /// Called with the `result` of the last attempt of the run, which is
    /// also returned by [`Request::send_blocking`].
    fn finished(&self, run: &RequestRun, result: &Result<RequestOutput>) {}
}

/// [internal]
///
/// The [`RequestObserver`] used when none is set.
pub(crate) struct NoObserver;

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
    /// so that it can be observed and retried. The whole response body is
    /// read too.
    ///
    /// # Errors
    ///
    /// The errors name only the method and the URL without its
    /// credentials, query and fragment, as they may hold secrets. A
    /// [`RequestObserver`] gets the whole request, e.g. to log it
    /// redacted.
    ///
    /// Returns an error if:
    /// - The request cannot be built, e.g. its URL is invalid, or its body
    ///   cannot be read.
    /// - It cannot be sent or its response received, e.g. the connection
    ///   fails or times out. Its [`TransportError`] is returned by
    ///   [`Error::source_as`](crate::Error::source_as).
    /// - The response status is not a success (2xx). Its [`RequestOutput`]
    ///   is returned by
    ///   [`Error::request_output`](crate::Error::request_output).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{BlockingRequestBuilderExt, ConfiguredRequest, Platform, Request, RequestOptions, RequestOutput};
    /// use std::time::Duration;
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
    /// request.send_blocking(client.get("https://example.com/ping").retry(3, Duration::from_secs(1))).unwrap();
    /// let options = RequestOptions { retries: 3, ..Default::default() };
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
    /// An attempt failing after `sink` accepted a part of the body is not
    /// retried, even when [`RequestOptions::retry_if`] accepts the failure,
    /// as `sink` would get the body again from its start. A caller can
    /// resume it instead, e.g. with a `Range` header and the response head
    /// from [`TransportError::output`].
    ///
    /// # Errors
    ///
    /// As [`Self::send_blocking`], or the error returned by `sink`, which
    /// ends the attempt. It is retried only if `sink` accepted no part of
    /// the body before and [`RequestOptions::retry_if`] accepts it.
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
    /// enabled. Dropping the future cancels the request, reported to
    /// [`RequestObserver::finished`] as failed.
    ///
    /// # Errors
    ///
    /// As [`Self::send_blocking`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Platform, Request, RequestBuilderExt, RequestOutput};
    /// use std::time::Duration;
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
    ///
    /// let ping = client.get("https://example.com/ping").retry(3, Duration::from_secs(1));
    /// request.send_async(ping).await.unwrap();
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

    /// Sets the observer of the runs, replacing the previous one.
    ///
    /// See [`RequestObserver`].
    fn set_observer(&self, observer: Arc<dyn RequestObserver>);
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
/// use std::time::Duration;
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
///     .retry(3, Duration::from_millis(100))
///     .retry_backoff(2)
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

    /// Returns a [`ConfiguredRequest`] retried up to `retries` times after
    /// `delay` while it fails in a way safe to retry.
    ///
    /// See [`RequestOptions::retries`].
    fn retry(self, retries: u32, delay: Duration) -> ConfiguredRequest<Self>;

    /// Returns a [`ConfiguredRequest`] whose retry delay is multiplied by
    /// `factor` after each retry.
    ///
    /// See [`RequestOptions::retry_backoff`].
    fn retry_backoff(self, factor: u32) -> ConfiguredRequest<Self>;

    /// Returns a [`ConfiguredRequest`] waiting at most `max` before a retry.
    ///
    /// See [`RequestOptions::retry_max_delay`].
    fn retry_max_delay(self, max: Duration) -> ConfiguredRequest<Self>;

    /// Returns a [`ConfiguredRequest`] retrying only the failed attempts
    /// for which `predicate` returns `true`, see [`Self::retry`].
    ///
    /// See [`RequestOptions::retry_if`].
    fn retry_if<F>(self, predicate: F) -> ConfiguredRequest<Self>
    where
        F: Fn(&RequestRun, &Result<RequestOutput>) -> bool + Send + Sync + 'static;

    /// Returns a [`ConfiguredRequest`] whose response shows its last
    /// `lines` when displayed.
    ///
    /// See [`RequestOptions::display_lines`].
    fn display_lines(self, lines: usize) -> ConfiguredRequest<Self>;

    /// Returns a [`ConfiguredRequest`] streaming its response body.
    ///
    /// See [`RequestOptions::stream`].
    fn stream(self) -> ConfiguredRequest<Self>;
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
/// use std::time::Duration;
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
///     .retry(3, Duration::from_millis(100))
///     .stream()
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

    /// Returns a [`ConfiguredRequest`] retried up to `retries` times after
    /// `delay` while it fails in a way safe to retry.
    ///
    /// See [`RequestOptions::retries`].
    fn retry(self, retries: u32, delay: Duration) -> ConfiguredRequest<Self>;

    /// Returns a [`ConfiguredRequest`] whose retry delay is multiplied by
    /// `factor` after each retry.
    ///
    /// See [`RequestOptions::retry_backoff`].
    fn retry_backoff(self, factor: u32) -> ConfiguredRequest<Self>;

    /// Returns a [`ConfiguredRequest`] waiting at most `max` before a retry.
    ///
    /// See [`RequestOptions::retry_max_delay`].
    fn retry_max_delay(self, max: Duration) -> ConfiguredRequest<Self>;

    /// Returns a [`ConfiguredRequest`] retrying only the failed attempts
    /// for which `predicate` returns `true`, see [`Self::retry`].
    ///
    /// See [`RequestOptions::retry_if`].
    fn retry_if<F>(self, predicate: F) -> ConfiguredRequest<Self>
    where
        F: Fn(&RequestRun, &Result<RequestOutput>) -> bool + Send + Sync + 'static;

    /// Returns a [`ConfiguredRequest`] whose response shows its last
    /// `lines` when displayed.
    ///
    /// See [`RequestOptions::display_lines`].
    fn display_lines(self, lines: usize) -> ConfiguredRequest<Self>;

    /// Returns a [`ConfiguredRequest`] streaming its response body.
    ///
    /// See [`RequestOptions::stream`].
    fn stream(self) -> ConfiguredRequest<Self>;
}

/// Enabled by the `request` feature flag.
///
/// A [`reqwest`] request builder `B` to be sent with [`RequestOptions`].
///
/// Usually obtained from [`RequestBuilderExt`] or
/// [`BlockingRequestBuilderExt`].
pub struct ConfiguredRequest<B> {
    pub(crate) builder: B,
    pub(crate) options: RequestOptions,
}

/// Enabled by the `request` feature flag.
///
/// A request being sent by a [`Request`], reported to a
/// [`RequestObserver`] and to a [`RequestRetryIf`].
pub struct RequestRun<'a> {
    method: &'a Method,
    url: &'a Url,
    headers: &'a HeaderMap,
    body: Option<&'a [u8]>,
    timeout: Option<Duration>,
    options: &'a RequestOptions,
    pub(crate) attempt: u32,
    id: u64,
    body_sunk: AtomicBool,
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

    /// Reports the response body to [`RequestObserver::body`] as it
    /// arrives, in addition to returning it.
    pub stream: bool,

    /// Sends the request again up to this many times while it fails, i.e.
    /// [`Request::send_blocking`] would return an error that the
    /// [`RequestOptions::retry_if`] predicate accepts (by default the
    /// failures [`RequestRetryIf::safe`] to retry for the method), waiting
    /// [`RequestOptions::retry_delay`] before each retry. Each attempt is
    /// limited by the timeouts of the request and its client on its own.
    /// Not after a sink accepted a part of the body, see
    /// [`Request::send_blocking_to`].
    pub retries: u32,

    /// The delay before the first retry, see [`RequestOptions::retries`].
    pub retry_delay: Duration,

    /// The factor the delay is multiplied by after each retry, see
    /// [`RequestOptions::retry_delay`]. `1` (the default) keeps the delay
    /// fixed, `2` doubles it. A failed response with a `Retry-After` header
    /// (as seconds or an HTTP date), e.g. 429 or 503, is retried no sooner
    /// than it asks.
    pub retry_backoff: u32,

    /// The longest delay before a retry, limiting the
    /// [`RequestOptions::retry_backoff`] and `Retry-After`. `None` (the
    /// default) for no limit.
    pub retry_max_delay: Option<Duration>,

    /// Retries only the failed attempts for which the predicate returns
    /// `true`, see [`RequestOptions::retries`]. When `None` only the
    /// failures accepted by [`RequestRetryIf::safe`] are retried, i.e. the
    /// transient ones of idempotent methods and of the others only those
    /// after which the request was not processed.
    pub retry_if: Option<RequestRetryIf>,
}

/// Enabled by the `request` feature flag.
///
/// A predicate deciding whether a failed attempt is retried, see
/// [`RequestOptions::retry_if`].
///
/// Two predicates are equal when they are the same closure.
#[derive(Clone)]
pub struct RequestRetryIf(Arc<RequestRetryPredicate>);

/// [internal]
///
/// Receives the body of a successful response, see
/// [`Request::send_blocking_to`].
pub(crate) type Sink<'a> = dyn FnMut(&[u8]) -> Result + Send + 'a;

type RequestRetryPredicate = dyn Fn(&RequestRun, &Result<RequestOutput>) -> bool + Send + Sync;

/// Enabled by the `request` feature flag.
///
/// The response to a request.
///
/// Returned from [`Request::send_blocking`] on success and by
/// [`Error::request_output`](crate::Error::request_output) of its error
/// when the status is not a success.
///
/// Outputs are equal when they differ at most in their
/// [`display_lines`](Self::display_lines), which only affects how they are
/// displayed.
///
/// Its [`Debug`] shows only the length of the body and its start, as the
/// body may be large, e.g. in a `{:?}` of an error.
#[derive(Clone)]
pub struct RequestOutput {
    /// The status code.
    pub status: u16,

    /// The response headers.
    pub headers: HeaderMap,

    /// The whole response body, empty when passed to a sink, see
    /// [`Request::send_blocking_to`].
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
    source: Option<reqwest::Error>,
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
/// A client built on the first use from the builder set, or from the
/// default one of the [`ReqwestEnv`], which is built again when the env
/// differs from the one it was built with.
pub(crate) struct DefaultClient<B, C>(Mutex<ClientState<B, C>>);

/// [internal]
struct ClientState<B, C> {
    builder: Option<B>,
    client: Option<Result<C>>,
    /// The env the default client was built with.
    env: ReqwestEnv,
    custom: bool,
}

/// [internal]
///
/// The default clients of a request with the proxies of the
/// [`REQWEST_ENV_VARS`] of its env, built again when they change.
pub(crate) struct DefaultClients {
    pub(crate) env: SharedEnv,
    /// The variables taken from `env` with the generation of its variables
    /// they were taken at.
    reqwest_env: Mutex<Option<(u64, ReqwestEnv)>>,
    blocking: DefaultClient<reqwest::blocking::ClientBuilder, reqwest::blocking::Client>,
    #[cfg(feature = "tokio")]
    client: DefaultClient<reqwest::ClientBuilder, reqwest::Client>,
}

/// Enabled by the `request` and `tokio` feature flags.
///
/// The source of the error an async run cancelled by dropping its future
/// is reported with to [`RequestObserver::finished`].
///
/// # Examples
///
/// ```rust
/// use agplatform::{RequestCancelled, RequestObserver, RequestOutput, RequestRun, Result};
///
/// struct Log;
///
/// impl RequestObserver for Log {
///     fn finished(&self, run: &RequestRun, result: &Result<RequestOutput>) {
///         match result {
///             Err(error) if error.source_as::<RequestCancelled>().is_some() => {
///                 println!("{} cancelled", run.target())
///             }
///             _ => {}
///         }
///     }
/// }
/// ```
#[cfg(feature = "tokio")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestCancelled;

/// [internal]
///
/// The variables of the platform environment that reqwest reads, see
/// [`REQWEST_ENV_VARS`]. Their names ignore the ASCII case on Windows like
/// its environment, e.g. `https_proxy` is `HTTPS_PROXY` there.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReqwestEnv(Vec<(String, String)>);

/// [internal]
///
/// Collects the response body, or passes it to the sink, and reports it to
/// the observer of a streamed run.
pub(crate) struct ObservedBody<'a, 'b, 'c> {
    pub(crate) body: Vec<u8>,
    observer: Option<&'a dyn RequestObserver>,
    run: &'a RequestRun<'b>,
    sink: Option<&'a mut Sink<'c>>,
    sink_error: Option<crate::Error>,
}

/// The production [`Request`] implementation backed by [`reqwest`].
///
/// Normally obtained via [`platform()`](crate::platform) rather than
/// constructed directly.
pub struct RequestImpl {
    clients: DefaultClients,
    observer: ObserverCell<dyn RequestObserver>,
}

/// [internal]
///
/// The attempts of an async run sent by [`RequestImpl`].
#[cfg(feature = "tokio")]
struct ReqwestAttempts<'a, 's> {
    request: &'a RequestImpl,
    observer: &'a dyn RequestObserver,
    client: &'a reqwest::Client,
    http: &'a reqwest::Request,
    sink: Option<&'a mut Sink<'s>>,
}

/// [internal]
///
/// The attempts of an async run, see [`run_attempts_async`].
#[cfg(feature = "tokio")]
pub(crate) trait AsyncAttempts: Send {
    /// Sends the attempt of `run`.
    fn attempt<'a>(&'a mut self, run: &'a RequestRun<'_>) -> BoxFuture<'a, Result<RequestOutput>>;

    /// Waits `delay` before a retry.
    fn delay(&self, delay: Duration) -> BoxFuture<'static, ()>;
}

/// [internal]
#[cfg(feature = "tokio")]
pub(crate) type BoxFuture<'a, T> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// [internal]
///
/// A run reported to the observer as started, and as finished once, as
/// cancelled when dropped before, unless panicking.
#[cfg(feature = "tokio")]
struct ObservedRun<'a, 'b> {
    observer: &'a dyn RequestObserver,
    run: RequestRun<'b>,
    finished: bool,
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

    fn retry(self, retries: u32, delay: Duration) -> ConfiguredRequest<Self> {
        ConfiguredRequest::new(self).retry(retries, delay)
    }

    fn retry_backoff(self, factor: u32) -> ConfiguredRequest<Self> {
        ConfiguredRequest::new(self).retry_backoff(factor)
    }

    fn retry_max_delay(self, max: Duration) -> ConfiguredRequest<Self> {
        ConfiguredRequest::new(self).retry_max_delay(max)
    }

    fn retry_if<F>(self, predicate: F) -> ConfiguredRequest<Self>
    where
        F: Fn(&RequestRun, &Result<RequestOutput>) -> bool + Send + Sync + 'static,
    {
        ConfiguredRequest::new(self).retry_if(predicate)
    }

    fn display_lines(self, lines: usize) -> ConfiguredRequest<Self> {
        ConfiguredRequest::new(self).display_lines(lines)
    }

    fn stream(self) -> ConfiguredRequest<Self> {
        ConfiguredRequest::new(self).stream()
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

    fn retry(self, retries: u32, delay: Duration) -> ConfiguredRequest<Self> {
        ConfiguredRequest::new(self).retry(retries, delay)
    }

    fn retry_backoff(self, factor: u32) -> ConfiguredRequest<Self> {
        ConfiguredRequest::new(self).retry_backoff(factor)
    }

    fn retry_max_delay(self, max: Duration) -> ConfiguredRequest<Self> {
        ConfiguredRequest::new(self).retry_max_delay(max)
    }

    fn retry_if<F>(self, predicate: F) -> ConfiguredRequest<Self>
    where
        F: Fn(&RequestRun, &Result<RequestOutput>) -> bool + Send + Sync + 'static,
    {
        ConfiguredRequest::new(self).retry_if(predicate)
    }

    fn display_lines(self, lines: usize) -> ConfiguredRequest<Self> {
        ConfiguredRequest::new(self).display_lines(lines)
    }

    fn stream(self) -> ConfiguredRequest<Self> {
        ConfiguredRequest::new(self).stream()
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
    /// let options = RequestOptions { retries: 3, ..Default::default() };
    /// let builder = agplatform::reqwest::blocking::Client::new().get("https://example.com");
    /// let request = ConfiguredRequest::with_options(builder, options);
    ///
    /// assert_eq!(request.options().retries, 3);
    /// ```
    pub fn with_options(builder: B, options: RequestOptions) -> Self {
        Self { builder, options }
    }

    /// Returns the options.
    pub fn options(&self) -> &RequestOptions {
        &self.options
    }

    /// Retries the request up to `retries` times after `delay` while it
    /// fails in a way safe to retry.
    ///
    /// See [`RequestOptions::retries`].
    pub fn retry(mut self, retries: u32, delay: Duration) -> Self {
        self.options.retries = retries;
        self.options.retry_delay = delay;
        self
    }

    /// Multiplies the retry delay by `factor` after each retry.
    ///
    /// See [`RequestOptions::retry_backoff`].
    pub fn retry_backoff(mut self, factor: u32) -> Self {
        self.options.retry_backoff = factor;
        self
    }

    /// Waits at most `max` before a retry.
    ///
    /// See [`RequestOptions::retry_max_delay`].
    pub fn retry_max_delay(mut self, max: Duration) -> Self {
        self.options.retry_max_delay = Some(max);
        self
    }

    /// Retries only the failed attempts for which `predicate` returns
    /// `true`, see [`Self::retry`].
    ///
    /// See [`RequestOptions::retry_if`].
    pub fn retry_if<F>(mut self, predicate: F) -> Self
    where
        F: Fn(&RequestRun, &Result<RequestOutput>) -> bool + Send + Sync + 'static,
    {
        self.options.retry_if = Some(RequestRetryIf::new(predicate));
        self
    }

    /// Shows the last `lines` of the response body when displayed.
    ///
    /// See [`RequestOptions::display_lines`].
    pub fn display_lines(mut self, lines: usize) -> Self {
        self.options.display_lines = lines;
        self
    }

    /// Streams the response body.
    ///
    /// See [`RequestOptions::stream`].
    pub fn stream(mut self) -> Self {
        self.options.stream = true;
        self
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
}

impl<'a> RequestRun<'a> {
    /// Returns the attempt of the run, starting at `1`, see
    /// [`RequestOptions::retries`].
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Returns the id of the run, unique within the process among the runs
    /// of commands and requests and the same for all of its attempts, e.g.
    /// to tell concurrent runs apart in a [`RequestObserver`].
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Returns the request body, `None` when it has none.
    pub fn body(&self) -> Option<&'a [u8]> {
        self.body
    }

    /// Returns the request headers.
    pub fn headers(&self) -> &'a HeaderMap {
        self.headers
    }

    /// Returns the method.
    pub fn method(&self) -> &'a Method {
        self.method
    }

    /// Returns the options.
    pub fn options(&self) -> &'a RequestOptions {
        self.options
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
    pub fn url(&self) -> &'a Url {
        self.url
    }

    /// Returns the delay before the retry of the attempt that failed with
    /// `result`, see [`RequestOptions::retry_backoff`].
    fn retry_delay(&self, result: &Result<RequestOutput>) -> Duration {
        let delay = self.options.retry_delay.saturating_mul(
            self.options
                .retry_backoff
                .saturating_pow(self.attempt.saturating_sub(1)),
        );
        let delay = result
            .as_ref()
            .err()
            .and_then(retry_after)
            .map_or(delay, |after| delay.max(after));

        self.options
            .retry_max_delay
            .map_or(delay, |max| delay.min(max))
    }
}

impl Default for RequestOptions {
    fn default() -> Self {
        Self {
            display_lines: DISPLAY_LINES,
            stream: false,
            retries: 0,
            retry_delay: Duration::ZERO,
            retry_backoff: 1,
            retry_max_delay: None,
            retry_if: None,
        }
    }
}

impl RequestRetryIf {
    /// Creates a predicate deciding whether a failed attempt is retried.
    pub fn new<F>(predicate: F) -> Self
    where
        F: Fn(&RequestRun, &Result<RequestOutput>) -> bool + Send + Sync + 'static,
    {
        Self(Arc::new(predicate))
    }

    /// Returns `true` if the failed `result` of the attempt of `run` may
    /// pass when retried without the risk of processing the request twice:
    /// of an idempotent method (`GET`, `HEAD`, `PUT`, `DELETE`, `OPTIONS`
    /// and `TRACE`, see RFC 9110) a [`TransportError`] or the status `408`
    /// (Request Timeout), `429` (Too Many Requests) or `5xx`, and of the
    /// others (e.g. `POST` and `PATCH`) only a
    /// [`TransportErrorKind::Connect`] error or the status `408`, `429` or
    /// `503` (Service Unavailable).
    /// Used when [`RequestOptions::retry_if`] is `None`, e.g. to retry
    /// more:
    ///
    /// ```rust
    /// use agplatform::{ConfiguredRequest, RequestRetryIf};
    /// # let builder = reqwest::Client::new().get("https://example.com");
    ///
    /// let request = ConfiguredRequest::new(builder).retry_if(|run, result| {
    ///     RequestRetryIf::safe(run, result)
    ///         || result.as_ref().is_err_and(|e| e.request_output().is_some_and(|o| o.status == 412))
    /// });
    /// ```
    pub fn safe(run: &RequestRun, result: &Result<RequestOutput>) -> bool {
        if run.method.is_idempotent() {
            return result.as_ref().is_err_and(|error| {
                error.source_as::<TransportError>().is_some()
                    || error
                        .request_output()
                        .is_some_and(|output| matches!(output.status, 408 | 429 | 500..=599))
            });
        }

        result.as_ref().is_err_and(|error| {
            error
                .source_as::<TransportError>()
                .is_some_and(|error| error.kind == TransportErrorKind::Connect)
                || error
                    .request_output()
                    .is_some_and(|output| matches!(output.status, 408 | 429 | 503))
        })
    }
}

impl std::fmt::Debug for RequestRetryIf {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "RequestRetryIf")
    }
}

impl PartialEq for RequestRetryIf {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for RequestRetryIf {}

impl RequestOutput {
    /// Returns `true` if the status is a success (2xx).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::RequestOutput;
    ///
    /// assert!(RequestOutput { status: 204, ..Default::default() }.success());
    /// assert!(!RequestOutput { status: 404, ..Default::default() }.success());
    /// ```
    pub fn success(&self) -> bool {
        (200..300).contains(&self.status)
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
            status: 0,
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
    /// not streamed (see [`RequestOptions::stream`]).
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
            source: Some(error.without_url()),
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
            .map(|source| source as &(dyn std::error::Error + 'static))
    }
}

#[cfg(feature = "tokio")]
impl std::fmt::Display for RequestCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "cancelled")
    }
}

#[cfg(feature = "tokio")]
impl std::error::Error for RequestCancelled {}

impl std::fmt::Display for TransportErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            TransportErrorKind::Connect => write!(f, "failed to connect"),
            TransportErrorKind::Timeout => write!(f, "timed out"),
            TransportErrorKind::Other => write!(f, "failed"),
        }
    }
}

impl<B, C: Clone> DefaultClient<B, C> {
    pub(crate) fn new() -> Self {
        Self(Mutex::new(ClientState {
            builder: None,
            client: None,
            env: ReqwestEnv::default(),
            custom: false,
        }))
    }

    /// Returns the client, built with `build` from the builder set on the
    /// first call, or from the `default` builder of `env` when not built
    /// with `env` yet. Fails when the builder set was lost by a build that
    /// panicked rather than building the default one.
    ///
    /// WHY: compared with `env` on each call, rather than reset when it
    /// changes, so that a client built with an env taken before a change
    /// is not kept after it.
    pub(crate) fn get(
        &self,
        env: &ReqwestEnv,
        default: &dyn Fn(&ReqwestEnv) -> B,
        build: fn(B) -> Result<C>,
    ) -> Result<C> {
        let mut state = self.state();

        if let Some(client) = &state.client
            && (state.custom || state.env == *env)
        {
            return client.clone();
        }

        let client = match state.builder.take() {
            Some(builder) => build(builder),
            None if state.custom => Err(request_error!(
                "client could not be built: its builder was lost by a panic"
            )),
            None => {
                state.env = env.clone();
                build(default(env))
            }
        };
        state.client = Some(client.clone());
        client
    }

    /// Replaces the client by one to be built from `builder`.
    pub(crate) fn set(&self, builder: B) {
        *self.state() = ClientState {
            builder: Some(builder),
            client: None,
            env: ReqwestEnv::default(),
            custom: true,
        };
    }

    fn state(&self) -> std::sync::MutexGuard<'_, ClientState<B, C>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl DefaultClients {
    pub(crate) fn new(env: SharedEnv) -> Self {
        Self {
            env,
            reqwest_env: Mutex::new(None),
            blocking: DefaultClient::new(),
            #[cfg(feature = "tokio")]
            client: DefaultClient::new(),
        }
    }

    /// Takes the variables from `env` instead, e.g. of a
    /// [`TestPlatform`](crate::test_platform::TestPlatform), for the
    /// default clients to be built with.
    #[cfg(feature = "testing")]
    pub(crate) fn set_env(&mut self, env: SharedEnv) {
        self.env = env;
        *self
            .reqwest_env
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// Returns the [`REQWEST_ENV_VARS`] of the env, taken again when its
    /// variables changed since last taken.
    pub(crate) fn reqwest_env(&self) -> ReqwestEnv {
        self.env.with_vars(|generation, vars| {
            let mut taken = self
                .reqwest_env
                .lock()
                .unwrap_or_else(PoisonError::into_inner);

            match &*taken {
                Some((taken_generation, reqwest_env)) if *taken_generation == generation => {
                    reqwest_env.clone()
                }
                _ => {
                    let reqwest_env = ReqwestEnv::new(vars);
                    *taken = Some((generation, reqwest_env.clone()));
                    reqwest_env
                }
            }
        })
    }

    pub(crate) fn blocking_client(&self) -> Result<reqwest::blocking::Client> {
        self.blocking.get(
            &self.reqwest_env(),
            &ReqwestEnv::blocking_client_builder,
            build_blocking_client,
        )
    }

    pub(crate) fn blocking_client_builder(&self) -> reqwest::blocking::ClientBuilder {
        self.reqwest_env().blocking_client_builder()
    }

    pub(crate) fn set_blocking_client(&self, builder: reqwest::blocking::ClientBuilder) {
        self.blocking.set(builder);
    }

    #[cfg(feature = "tokio")]
    pub(crate) fn client(&self) -> Result<reqwest::Client> {
        self.client.get(
            &self.reqwest_env(),
            &ReqwestEnv::client_builder,
            build_client,
        )
    }

    #[cfg(feature = "tokio")]
    pub(crate) fn client_builder(&self) -> reqwest::ClientBuilder {
        self.reqwest_env().client_builder()
    }

    #[cfg(feature = "tokio")]
    pub(crate) fn set_client(&self, builder: reqwest::ClientBuilder) {
        self.client.set(builder);
    }
}

impl ReqwestEnv {
    /// Returns the [`REQWEST_ENV_VARS`] of `vars`.
    pub(crate) fn new<'a>(vars: impl IntoIterator<Item = &'a (String, String)>) -> Self {
        Self(
            vars.into_iter()
                .filter(|(key, _)| is_reqwest_var(key))
                .cloned()
                .collect(),
        )
    }

    #[cfg(feature = "tokio")]
    pub(crate) fn client_builder(&self) -> reqwest::ClientBuilder {
        self.client_builder_with(CONNECT_TIMEOUT, READ_TIMEOUT)
    }

    pub(crate) fn blocking_client_builder(&self) -> reqwest::blocking::ClientBuilder {
        self.blocking_client_builder_with(CONNECT_TIMEOUT, READ_TIMEOUT)
    }

    /// Returns a client builder with the proxies and the `connect` and
    /// `read` timeouts.
    #[cfg(feature = "tokio")]
    fn client_builder_with(&self, connect: Duration, read: Duration) -> reqwest::ClientBuilder {
        self.proxies().into_iter().fold(
            reqwest::Client::builder()
                .connect_timeout(connect)
                .read_timeout(read)
                .no_proxy(),
            reqwest::ClientBuilder::proxy,
        )
    }

    /// Returns a blocking client builder with the proxies and the `connect`
    /// timeout. Its `read` timeout replaces the total one of 30 seconds
    /// reqwest sets by default. It limits each read of the body, and the
    /// wait for the response head, since the body of a blocking response
    /// is read without the runtime the read timeout of reqwest needs.
    fn blocking_client_builder_with(
        &self,
        connect: Duration,
        read: Duration,
    ) -> reqwest::blocking::ClientBuilder {
        self.proxies().into_iter().fold(
            reqwest::blocking::Client::builder()
                .connect_timeout(connect)
                .timeout(read)
                .no_proxy(),
            reqwest::blocking::ClientBuilder::proxy,
        )
    }

    /// Returns the proxies resolved like hyper-util's `Matcher::from_env`,
    /// which reqwest uses for the process environment: the first set of
    /// the uppercase and lowercase variable, a scheme specific proxy that
    /// is not valid falling back to `ALL_PROXY`, invalid ones ignored.
    fn proxies(&self) -> Vec<reqwest::Proxy> {
        if self.var(&["REQUEST_METHOD"]).is_some() {
            return Vec::new();
        }

        let all = self.var(&["ALL_PROXY", "all_proxy"]);
        let no_proxy = self
            .var(&["NO_PROXY", "no_proxy"])
            .and_then(reqwest::NoProxy::from_string);
        let resolve = |names: &[&str], proxy: fn(&str) -> reqwest::Result<reqwest::Proxy>| {
            self.var(names)
                .and_then(|url| proxy(url).ok())
                .or_else(|| all.and_then(|url| proxy(url).ok()))
        };

        [
            resolve(&["HTTP_PROXY", "http_proxy"], |url| {
                reqwest::Proxy::http(url)
            }),
            resolve(&["HTTPS_PROXY", "https_proxy"], |url| {
                reqwest::Proxy::https(url)
            }),
        ]
        .into_iter()
        .flatten()
        .map(|proxy| proxy.no_proxy(no_proxy.clone()))
        .collect()
    }

    fn var(&self, names: &[&str]) -> Option<&str> {
        names.iter().find_map(|name| {
            self.0
                .iter()
                .find(|(key, _)| same_var(key, name))
                .map(|(_, value)| value.as_str())
        })
    }
}

impl<'a, 'b, 'c> ObservedBody<'a, 'b, 'c> {
    pub(crate) fn new(
        observer: &'a dyn RequestObserver,
        run: &'a RequestRun<'b>,
        sink: Option<&'a mut Sink<'c>>,
    ) -> Self {
        Self {
            body: Vec::new(),
            observer: run.options.stream.then_some(observer),
            run,
            sink,
            sink_error: None,
        }
    }

    pub(crate) fn push(&mut self, data: &[u8]) -> Result<()> {
        if let Some(observer) = self.observer {
            observer.body(self.run, data);
        }

        match &mut self.sink {
            Some(sink) => {
                sink(data)?;
                self.run.body_sunk.store(true, Ordering::Relaxed);
                Ok(())
            }
            None => {
                self.body.extend_from_slice(data);
                Ok(())
            }
        }
    }
}

impl std::io::Write for ObservedBody<'_, '_, '_> {
    /// Keeps the error of the sink to be returned instead of the I/O error.
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        match self.push(data) {
            Ok(()) => Ok(data.len()),
            Err(error) => {
                self.sink_error = Some(error);
                Err(std::io::Error::other("sink failed"))
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl RequestImpl {
    /// Creates a new `RequestImpl` with default clients that are built on
    /// first use.
    pub fn new() -> Self {
        Self::with_env(SharedEnv::new(Vec::new(), std::path::PathBuf::new()))
    }

    /// [internal]
    ///
    /// Creates a new `RequestImpl` taking the [`REQWEST_ENV_VARS`] from
    /// `env`, see [`Request`].
    pub(crate) fn with_env(env: SharedEnv) -> Self {
        Self {
            clients: DefaultClients::new(env),
            observer: ObserverCell::new(Arc::new(NoObserver)),
        }
    }

    fn send_blocking_with(
        &self,
        request: reqwest::blocking::RequestBuilder,
        options: &RequestOptions,
        mut sink: Option<&mut Sink<'_>>,
    ) -> Result<RequestOutput> {
        let observer = self.observer.get();
        let observer = observer.as_ref();
        let (client, request) = request.build_split();
        let mut request = built(observer, request)?;

        if let Err(error) = buffer_blocking_body(&mut request) {
            return unsent(observer, &blocking_run(&request, options), error);
        }

        run_attempts(
            observer,
            blocking_run(&request, options),
            &mut |run| self.attempt(observer, &client, &request, run, sink.as_deref_mut()),
            std::thread::sleep,
        )
    }

    fn attempt(
        &self,
        observer: &dyn RequestObserver,
        client: &reqwest::blocking::Client,
        request: &reqwest::blocking::Request,
        run: &RequestRun,
        sink: Option<&mut Sink<'_>>,
    ) -> Result<RequestOutput> {
        let start = Instant::now();
        let result = self.receive(observer, client, request, run, start, sink);
        finish(run, start, result)
    }

    fn receive(
        &self,
        observer: &dyn RequestObserver,
        client: &reqwest::blocking::Client,
        request: &reqwest::blocking::Request,
        run: &RequestRun,
        start: Instant,
        sink: Option<&mut Sink<'_>>,
    ) -> Result<(u16, HeaderMap, Vec<u8>)> {
        let mut response = client
            .execute(clone_blocking_request(request))
            .map_err(|error| transport_failure(run, error.into()))?;
        let status = response.status();
        let headers = std::mem::take(response.headers_mut());
        let interrupted = |error, body| interrupted(run, start, error, status, &headers, body);
        let sink = sink.filter(|_| status.is_success());
        // Read in parts, as the client timeout limits each read, but all of
        // `bytes`.
        let mut body = ObservedBody::new(observer, run, sink);

        if let Err(error) = response.copy_to(&mut body) {
            return Err(match body.sink_error {
                Some(error) => error,
                None if run.options.stream => interrupted(error, body.body),
                None => interrupted(error, Vec::new()),
            });
        }

        Ok((status.as_u16(), headers, body.body))
    }

    #[cfg(feature = "tokio")]
    async fn send_async_with(
        &self,
        request: reqwest::RequestBuilder,
        options: &RequestOptions,
        sink: Option<&mut Sink<'_>>,
    ) -> Result<RequestOutput> {
        let observer = self.observer.get();
        let observer = observer.as_ref();
        let (client, request) = request.build_split();
        let mut request = built(observer, request)?;

        if let Err(error) = buffer_body(&mut request).await {
            return unsent(observer, &async_run(&request, options), error);
        }

        let mut attempts = ReqwestAttempts {
            request: self,
            observer,
            client: &client,
            http: &request,
            sink,
        };

        run_attempts_async(observer, async_run(&request, options), &mut attempts).await
    }

    #[cfg(feature = "tokio")]
    async fn attempt_async(
        &self,
        observer: &dyn RequestObserver,
        client: &reqwest::Client,
        request: &reqwest::Request,
        run: &RequestRun<'_>,
        sink: Option<&mut Sink<'_>>,
    ) -> Result<RequestOutput> {
        let start = Instant::now();
        let result = self
            .receive_async(observer, client, request, run, start, sink)
            .await;
        finish(run, start, result)
    }

    #[cfg(feature = "tokio")]
    async fn receive_async(
        &self,
        observer: &dyn RequestObserver,
        client: &reqwest::Client,
        request: &reqwest::Request,
        run: &RequestRun<'_>,
        start: Instant,
        sink: Option<&mut Sink<'_>>,
    ) -> Result<(u16, HeaderMap, Vec<u8>)> {
        let mut response = client
            .execute(clone_request(request))
            .await
            .map_err(|error| transport_failure(run, error.into()))?;
        let status = response.status();
        let headers = std::mem::take(response.headers_mut());
        let interrupted = |error, body| interrupted(run, start, error, status, &headers, body);
        let sink = sink.filter(|_| status.is_success());
        let body = if sink.is_none() && !run.options.stream {
            response
                .bytes()
                .await
                .map_err(|error| interrupted(error, Vec::new()))?
                .into()
        } else {
            let mut body = ObservedBody::new(observer, run, sink);

            loop {
                match response.chunk().await {
                    Ok(Some(chunk)) => body.push(&chunk)?,
                    Ok(None) => break body.body,
                    Err(error) => return Err(interrupted(error, body.body)),
                }
            }
        };

        Ok((status.as_u16(), headers, body))
    }
}

#[cfg(feature = "tokio")]
impl AsyncAttempts for ReqwestAttempts<'_, '_> {
    fn attempt<'a>(&'a mut self, run: &'a RequestRun<'_>) -> BoxFuture<'a, Result<RequestOutput>> {
        Box::pin(self.request.attempt_async(
            self.observer,
            self.client,
            self.http,
            run,
            self.sink.as_deref_mut(),
        ))
    }

    fn delay(&self, delay: Duration) -> BoxFuture<'static, ()> {
        Box::pin(tokio::time::sleep(delay))
    }
}

#[cfg(feature = "tokio")]
impl<'a, 'b> ObservedRun<'a, 'b> {
    fn new(observer: &'a dyn RequestObserver, run: RequestRun<'b>) -> Self {
        observer.started(&run);

        Self {
            observer,
            run,
            finished: false,
        }
    }

    fn finish(mut self, result: Result<RequestOutput>) -> Result<RequestOutput> {
        self.finished = true;
        self.observer.finished(&self.run, &result);
        result
    }
}

#[cfg(feature = "tokio")]
impl Drop for ObservedRun<'_, '_> {
    fn drop(&mut self) {
        if !self.finished && !std::thread::panicking() {
            let target = self.run.target();
            let cancelled = Err(request_error!("'{target}' {RequestCancelled}")
                .with_subject(target)
                .with_source(RequestCancelled));
            self.observer.finished(&self.run, &cancelled);
        }
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
        let request = request.into();
        self.send_blocking_with(request.builder, &request.options, None)
    }

    fn send_blocking_to(
        &self,
        request: impl Into<ConfiguredRequest<reqwest::blocking::RequestBuilder>>,
        sink: &mut (dyn FnMut(&[u8]) -> Result + Send),
    ) -> Result<RequestOutput> {
        let request = request.into();
        self.send_blocking_with(request.builder, &request.options, Some(sink))
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
        let request = request.into();
        async move {
            self.send_async_with(request.builder, &request.options, None)
                .await
        }
    }

    #[cfg(feature = "tokio")]
    fn send_async_to(
        &self,
        request: impl Into<ConfiguredRequest<reqwest::RequestBuilder>>,
        sink: &mut (dyn FnMut(&[u8]) -> Result + Send),
    ) -> impl Future<Output = Result<RequestOutput>> + Send {
        let request = request.into();
        async move {
            self.send_async_with(request.builder, &request.options, Some(sink))
                .await
        }
    }

    fn set_observer(&self, observer: Arc<dyn RequestObserver>) {
        self.observer.set(observer);
    }
}

impl RequestObserver for NoObserver {}

const DISPLAY_LINES: usize = 10;

/// The number of the first bytes of the body shown by the [`Debug`] of a
/// [`RequestOutput`].
const DEBUG_BODY_BYTES: usize = 256;

/// The timeout of connecting of the clients from
/// [`Request::client_builder`] and [`Request::blocking_client_builder`].
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// The timeout of each read of a response of the clients from
/// [`Request::client_builder`] and [`Request::blocking_client_builder`].
const READ_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// The environment variables reqwest reads, see [`Request`].
const REQWEST_ENV_VARS: [&str; 9] = [
    "ALL_PROXY",
    "all_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "NO_PROXY",
    "no_proxy",
    "REQUEST_METHOD",
];

/// Returns whether `key` is one of the [`REQWEST_ENV_VARS`].
fn is_reqwest_var(key: &str) -> bool {
    REQWEST_ENV_VARS.iter().any(|name| same_var(name, key))
}

/// Returns whether the variable names `a` and `b` are the same, ignoring
/// the ASCII case like the environment of Windows.
#[cfg(windows)]
fn same_var(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Returns whether the variable names `a` and `b` are the same.
#[cfg(not(windows))]
fn same_var(a: &str, b: &str) -> bool {
    a == b
}

/// [internal]
///
/// Returns the error of a client failing to be built.
pub(crate) fn client_error(error: reqwest::Error) -> crate::Error {
    request_error!("client could not be built: {error}").with_source(error)
}

/// [internal]
pub(crate) fn build_blocking_client(
    builder: reqwest::blocking::ClientBuilder,
) -> Result<reqwest::blocking::Client> {
    builder.build().map_err(client_error)
}

/// [internal]
#[cfg(feature = "tokio")]
pub(crate) fn build_client(builder: reqwest::ClientBuilder) -> Result<reqwest::Client> {
    builder.build().map_err(client_error)
}

/// [internal]
///
/// Returns the error of a request failing to be built.
pub(crate) fn invalid_request(error: reqwest::Error) -> crate::Error {
    let error = error.without_url();
    request_error!("invalid request: {error}").with_source(error)
}

/// [internal]
///
/// Returns the built `request`, reporting the error of one that cannot be
/// built to `observer`.
pub(crate) fn built<T>(observer: &dyn RequestObserver, request: reqwest::Result<T>) -> Result<T> {
    request
        .map_err(invalid_request)
        .inspect_err(|error| observer.invalid(error))
}

/// [internal]
///
/// Reports `run` to `observer` as finished with `error` before it was sent
/// and returns the error.
pub(crate) fn unsent(
    observer: &dyn RequestObserver,
    run: &RequestRun,
    error: crate::Error,
) -> Result<RequestOutput> {
    observer.started(run);
    let result = Err(error);
    observer.finished(run, &result);
    result
}

/// [internal]
///
/// Returns the error of a request body failing to be read.
fn unreadable_body(error: reqwest::Error) -> crate::Error {
    let error = error.without_url();
    request_error!("request body could not be read: {error}").with_source(error)
}

/// [internal]
///
/// Reads a streamed body of `request` into memory.
pub(crate) fn buffer_blocking_body(request: &mut reqwest::blocking::Request) -> Result<()> {
    if let Some(body) = request.body_mut() {
        body.buffer().map_err(unreadable_body)?;
    }

    Ok(())
}

/// [internal]
///
/// Reads a streamed body of `request` into memory.
#[cfg(feature = "tokio")]
pub(crate) async fn buffer_body(request: &mut reqwest::Request) -> Result<()> {
    if let Some(body) = request.body_mut().take_if(|body| body.as_bytes().is_none()) {
        let body = http_body_util::BodyExt::collect(body)
            .await
            .map_err(unreadable_body)?;
        *request.body_mut() = Some(body.to_bytes().into());
    }

    Ok(())
}

/// [internal]
///
/// Returns a copy of `request` to be sent. A streamed body, never sent
/// here as it is buffered first, is dropped.
fn clone_blocking_request(request: &reqwest::blocking::Request) -> reqwest::blocking::Request {
    request.try_clone().unwrap_or_else(|| {
        let mut clone =
            reqwest::blocking::Request::new(request.method().clone(), request.url().clone());
        *clone.headers_mut() = request.headers().clone();
        *clone.timeout_mut() = request.timeout().copied();
        clone
    })
}

/// [internal]
///
/// Returns a copy of `request` to be sent. A streamed body, never sent
/// here as it is buffered first, is dropped.
#[cfg(feature = "tokio")]
fn clone_request(request: &reqwest::Request) -> reqwest::Request {
    request.try_clone().unwrap_or_else(|| {
        let mut clone = reqwest::Request::new(request.method().clone(), request.url().clone());
        *clone.headers_mut() = request.headers().clone();
        *clone.timeout_mut() = request.timeout().copied();
        clone
    })
}

/// [internal]
///
/// Returns the run of `request` with `options` to be reported.
pub(crate) fn blocking_run<'a>(
    request: &'a reqwest::blocking::Request,
    options: &'a RequestOptions,
) -> RequestRun<'a> {
    RequestRun {
        method: request.method(),
        url: request.url(),
        headers: request.headers(),
        body: request.body().and_then(reqwest::blocking::Body::as_bytes),
        timeout: request.timeout().copied(),
        options,
        attempt: 1,
        id: crate::utils::next_run_id(),
        body_sunk: AtomicBool::new(false),
    }
}

/// [internal]
///
/// Returns the run of `request` with `options` to be reported.
#[cfg(feature = "tokio")]
pub(crate) fn async_run<'a>(
    request: &'a reqwest::Request,
    options: &'a RequestOptions,
) -> RequestRun<'a> {
    RequestRun {
        method: request.method(),
        url: request.url(),
        headers: request.headers(),
        body: request.body().and_then(reqwest::Body::as_bytes),
        timeout: request.timeout().copied(),
        options,
        attempt: 1,
        id: crate::utils::next_run_id(),
        body_sunk: AtomicBool::new(false),
    }
}

/// [internal]
///
/// Returns `Ok(output)` if its status is a success, otherwise an error
/// with the `output` as its source.
pub(crate) fn check(run: &RequestRun, output: RequestOutput) -> Result<RequestOutput> {
    if output.success() {
        Ok(output)
    } else {
        let target = run.target();
        Err(
            request_error!("'{target}' failed with status {}", output.status)
                .with_subject(target)
                .with_source(output),
        )
    }
}

/// [internal]
///
/// Returns the error of `run` failing with the transport `error` as its
/// source.
pub(crate) fn transport_failure(run: &RequestRun, error: TransportError) -> crate::Error {
    let target = run.target();
    request_error!("'{target}' {error}")
        .with_subject(target)
        .with_source(error)
}

/// Returns the error of `run` started at `start` whose response body
/// failed to be received after its `body` part.
fn interrupted(
    run: &RequestRun,
    start: Instant,
    error: reqwest::Error,
    status: reqwest::StatusCode,
    headers: &HeaderMap,
    body: Vec<u8>,
) -> crate::Error {
    transport_failure(
        run,
        TransportError::from(error).with_output(RequestOutput {
            status: status.as_u16(),
            headers: headers.clone(),
            body,
            duration: start.elapsed(),
            display_lines: run.options.display_lines,
        }),
    )
}

/// Returns the output of the attempt of `run` started at `start` from its
/// `result`.
fn finish(
    run: &RequestRun,
    start: Instant,
    result: Result<(u16, HeaderMap, Vec<u8>)>,
) -> Result<RequestOutput> {
    let (status, headers, body) = result?;

    check(
        run,
        RequestOutput {
            status,
            headers,
            body,
            duration: start.elapsed(),
            display_lines: run.options.display_lines,
        },
    )
}

/// [internal]
///
/// Runs the attempts of `run` with `attempt` until one succeeds or is not
/// retried, waiting with `sleep` between them, and reports them to
/// `observer`.
pub(crate) fn run_attempts(
    observer: &dyn RequestObserver,
    mut run: RequestRun,
    attempt: &mut dyn FnMut(&RequestRun) -> Result<RequestOutput>,
    sleep: fn(Duration),
) -> Result<RequestOutput> {
    observer.started(&run);

    let result = loop {
        let result = attempt(&run);

        if !retry(&run, &result) {
            break result;
        }

        observer.retrying(&run, &result);
        sleep(run.retry_delay(&result));
        run.attempt += 1;
    };

    observer.finished(&run, &result);
    result
}

/// [internal]
///
/// Runs the async `attempts` of `run` like [`run_attempts`].
#[cfg(feature = "tokio")]
pub(crate) async fn run_attempts_async(
    observer: &dyn RequestObserver,
    run: RequestRun<'_>,
    attempts: &mut dyn AsyncAttempts,
) -> Result<RequestOutput> {
    let mut observed = ObservedRun::new(observer, run);

    let result = loop {
        let result = attempts.attempt(&observed.run).await;

        if !retry(&observed.run, &result) {
            break result;
        }

        observer.retrying(&observed.run, &result);
        attempts.delay(observed.run.retry_delay(&result)).await;
        observed.run.attempt += 1;
    };

    observed.finish(result)
}

/// Returns how long the response of the `error` asks to wait before a
/// retry with its `Retry-After` header, if it does.
fn retry_after(error: &crate::Error) -> Option<Duration> {
    let value = error
        .request_output()?
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

/// Parses an HTTP date in its preferred format (IMF-fixdate), e.g.
/// `Sun, 06 Nov 1994 08:49:37 GMT`.
fn http_date(value: &str) -> Option<SystemTime> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    let mut parts = value.split(' ');
    let [_, day, month, year, time, "GMT"] = std::array::from_fn(|_| parts.next().unwrap_or(""))
    else {
        return None;
    };
    let month = MONTHS.iter().position(|name| *name == month)? as i64 + 1;
    let (day, year) = (day.parse::<i64>().ok()?, year.parse::<i64>().ok()?);
    let mut time = time.split(':');
    let [hour, minute, second] =
        std::array::from_fn(|_| time.next().and_then(|part| part.parse::<i64>().ok()));
    let (hour, minute, second) = (hour?, minute?, second?);
    (parts.next().is_none() && time.next().is_none()).then_some(())?;

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

/// Returns whether the failed `result` of the attempt of `run` is retried.
/// A run whose body was accepted by the sink is never retried, as the sink
/// cannot be reset.
pub(crate) fn retry(run: &RequestRun, result: &Result<RequestOutput>) -> bool {
    result.is_err()
        && run.attempt <= run.options.retries
        && run.attempt < u32::MAX
        && !run.body_sunk.load(Ordering::Relaxed)
        && run.options.retry_if.as_ref().map_or_else(
            || RequestRetryIf::safe(run, result),
            |retry_if| (retry_if.0)(run, result),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _;
    use std::io::BufRead as _;
    use std::io::Read as _;
    use std::io::Write as _;
    use std::net::TcpListener;
    use std::net::TcpStream;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    struct Server {
        url: String,
        received: Arc<Mutex<Vec<String>>>,
    }

    impl Server {
        fn received(&self) -> Vec<String> {
            self.received.lock().unwrap().clone()
        }
    }

    /// Answers each connection with what `respond` returns for its index
    /// and request, then closes it.
    fn serve(respond: impl Fn(usize, &str) -> Vec<u8> + Send + 'static) -> Server {
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
    fn serve_chunks(pause: Duration, chunks: &'static [&'static str]) -> Server {
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

    fn response(status: u16, body: &str) -> Vec<u8> {
        format!(
            "HTTP/1.1 {status} Status\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn truncated(_: usize, _: &str) -> Vec<u8> {
        b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\nconnection: close\r\n\r\npartial".to_vec()
    }

    fn slow(_: usize, _: &str) -> Vec<u8> {
        std::thread::sleep(Duration::from_secs(2));
        response(200, "late")
    }

    fn ok(_: usize, _: &str) -> Vec<u8> {
        response(200, "body")
    }

    fn not_found(_: usize, _: &str) -> Vec<u8> {
        response(404, "missing")
    }

    fn unavailable_once(index: usize, _: &str) -> Vec<u8> {
        if index == 0 {
            response(503, "unavailable")
        } else {
            response(200, "body")
        }
    }

    fn closed_url() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    }

    fn invalid_client() -> reqwest::blocking::ClientBuilder {
        reqwest::blocking::Client::builder().user_agent("\n")
    }

    #[cfg(feature = "tokio")]
    fn invalid_async_client() -> reqwest::ClientBuilder {
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

    #[derive(Default)]
    struct RecordingObserver(Mutex<Vec<String>>);

    impl RecordingObserver {
        fn events(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    impl RequestObserver for RecordingObserver {
        fn invalid(&self, error: &crate::Error) {
            self.0.lock().unwrap().push(format!("invalid {error}"));
        }

        fn started(&self, run: &RequestRun) {
            self.0
                .lock()
                .unwrap()
                .push(format!("started {}", run.target()));
        }

        fn body(&self, _run: &RequestRun, data: &[u8]) {
            self.0
                .lock()
                .unwrap()
                .push(format!("body {}", String::from_utf8_lossy(data)));
        }

        fn retrying(&self, run: &RequestRun, _result: &Result<RequestOutput>) {
            self.0
                .lock()
                .unwrap()
                .push(format!("retrying attempt {}", run.attempt()));
        }

        fn finished(&self, _run: &RequestRun, result: &Result<RequestOutput>) {
            self.0.lock().unwrap().push(format!(
                "finished {}",
                result.as_ref().map_or_else(
                    |error| error.to_string(),
                    |output| output.status.to_string()
                )
            ));
        }
    }

    /// Records whether the runs finished as cancelled.
    #[cfg(feature = "tokio")]
    #[derive(Default)]
    struct CancelledObserver(Mutex<Vec<bool>>);

    #[cfg(feature = "tokio")]
    impl RequestObserver for CancelledObserver {
        fn finished(&self, run: &RequestRun, result: &Result<RequestOutput>) {
            self.0
                .lock()
                .unwrap()
                .push(result.as_ref().is_err_and(|error| {
                    error.source_as::<RequestCancelled>().is_some()
                        && error.request_target() == Some(&*run.target())
                }));
        }
    }

    #[cfg(feature = "tokio")]
    struct Observers(Arc<RecordingObserver>, Arc<CancelledObserver>);

    #[cfg(feature = "tokio")]
    impl RequestObserver for Observers {
        fn started(&self, run: &RequestRun) {
            self.0.started(run);
        }

        fn finished(&self, run: &RequestRun, result: &Result<RequestOutput>) {
            self.0.finished(run, result);
            self.1.finished(run, result);
        }
    }

    /// Changes of the env the request shares, like by its platform.
    impl RequestImpl {
        fn set_var(&self, key: String, value: String) {
            self.clients.env.set_var(key, value);
        }

        fn remove_var(&self, key: &str) {
            self.clients.env.remove_var(key);
        }
    }

    fn observed() -> (RequestImpl, Arc<RecordingObserver>) {
        let observer = Arc::new(RecordingObserver::default());
        let request = RequestImpl::new();
        request.set_observer(observer.clone());
        (request, observer)
    }

    fn blocking_get(request: &RequestImpl, url: &str) -> reqwest::blocking::RequestBuilder {
        request.blocking_client().unwrap().get(url)
    }

    #[cfg(feature = "tokio")]
    fn get(request: &RequestImpl, url: &str) -> reqwest::RequestBuilder {
        request.client().unwrap().get(url)
    }

    #[cfg(feature = "tokio")]
    fn block_on<F: Future>(future: F) -> F::Output {
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

    fn status_error(status: u16) -> crate::Error {
        crate::Error::request(format!("status {status}")).with_source(RequestOutput {
            status,
            ..Default::default()
        })
    }

    mod blocking_request_builder_ext {
        use super::*;

        mod send_blocking {
            use super::*;

            #[test]
            fn send_blocking_should_send_request_with_default_options() {
                let server = serve(ok);
                let request = RequestImpl::new();

                let output = blocking_get(&request, &format!("{}/path", server.url))
                    .send_blocking(&request)
                    .unwrap();

                assert_eq!(output.status, 200);
                assert_eq!(output.body, b"body");
                assert!(server.received()[0].starts_with("GET /path HTTP/1.1\r\n"));
            }
        }

        mod send_blocking_to {
            use super::*;

            #[test]
            fn send_blocking_to_should_send_request_with_default_options_to_sink() {
                let server = serve(ok);
                let request = RequestImpl::new();
                let mut received = Vec::new();

                let output = blocking_get(&request, &server.url)
                    .send_blocking_to(&request, &mut |data| {
                        received.extend_from_slice(data);
                        Ok(())
                    })
                    .unwrap();

                assert_eq!(output.status, 200);
                assert_eq!(received, b"body");
            }
        }

        mod retry {
            use super::*;

            #[test]
            fn retry_should_set_retries_and_delay() {
                let builder = reqwest::blocking::Client::new().get("http://localhost");

                let request = builder.retry(2, Duration::from_secs(1));

                assert_eq!(
                    request.options(),
                    &RequestOptions {
                        retries: 2,
                        retry_delay: Duration::from_secs(1),
                        ..Default::default()
                    }
                );
            }
        }

        mod stream {
            use super::*;

            #[test]
            fn stream_should_set_stream() {
                let builder = reqwest::blocking::Client::new().get("http://localhost");

                let request = builder.stream();

                assert!(request.options().stream);
            }
        }

        mod display_lines {
            use super::*;

            #[test]
            fn display_lines_should_set_lines() {
                let builder = reqwest::blocking::Client::new().get("http://localhost");

                let request = builder.display_lines(3);

                assert_eq!(request.options().display_lines, 3);
            }
        }

        mod retry_backoff {
            use super::*;

            #[test]
            fn retry_backoff_should_set_factor() {
                let builder = reqwest::blocking::Client::new().get("http://localhost");

                let request = builder.retry_backoff(2);

                assert_eq!(request.options().retry_backoff, 2);
            }
        }

        mod retry_max_delay {
            use super::*;

            #[test]
            fn retry_max_delay_should_set_max_delay() {
                let builder = reqwest::blocking::Client::new().get("http://localhost");

                let request = builder.retry_max_delay(Duration::from_secs(1));

                assert_eq!(
                    request.options().retry_max_delay,
                    Some(Duration::from_secs(1))
                );
            }
        }

        mod retry_if {
            use super::*;

            #[test]
            fn retry_if_should_set_predicate() {
                let builder = reqwest::blocking::Client::new().get("http://localhost");

                let request = builder.retry_if(|_, _| true);

                assert!(request.options().retry_if.is_some());
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

                let output =
                    block_on(get(&request, &format!("{}/path", server.url)).send_async(&request))
                        .unwrap();

                assert_eq!(output.status, 200);
                assert_eq!(output.body, b"body");
                assert!(server.received()[0].starts_with("GET /path HTTP/1.1\r\n"));
            }
        }

        mod send_async_to {
            use super::*;

            #[test]
            fn send_async_to_should_send_request_with_default_options_to_sink() {
                let server = serve(ok);
                let request = RequestImpl::new();
                let mut received = Vec::new();

                let output = block_on(get(&request, &server.url).send_async_to(
                    &request,
                    &mut |data| {
                        received.extend_from_slice(data);
                        Ok(())
                    },
                ))
                .unwrap();

                assert_eq!(output.status, 200);
                assert_eq!(received, b"body");
            }
        }

        mod retry {
            use super::*;

            #[test]
            fn retry_should_set_retries_and_delay() {
                let builder = reqwest::Client::new().get("http://localhost");

                let request = builder.retry(2, Duration::from_secs(1));

                assert_eq!(request.options().retries, 2);
                assert_eq!(request.options().retry_delay, Duration::from_secs(1));
            }
        }

        mod stream {
            use super::*;

            #[test]
            fn stream_should_set_stream() {
                let builder = reqwest::Client::new().get("http://localhost");

                let request = builder.stream();

                assert!(request.options().stream);
            }
        }

        mod display_lines {
            use super::*;

            #[test]
            fn display_lines_should_set_lines() {
                let builder = reqwest::Client::new().get("http://localhost");

                let request = builder.display_lines(3);

                assert_eq!(request.options().display_lines, 3);
            }
        }

        mod retry_backoff {
            use super::*;

            #[test]
            fn retry_backoff_should_set_factor() {
                let builder = reqwest::Client::new().get("http://localhost");

                let request = builder.retry_backoff(2);

                assert_eq!(request.options().retry_backoff, 2);
            }
        }

        mod retry_max_delay {
            use super::*;

            #[test]
            fn retry_max_delay_should_set_max_delay() {
                let builder = reqwest::Client::new().get("http://localhost");

                let request = builder.retry_max_delay(Duration::from_secs(1));

                assert_eq!(
                    request.options().retry_max_delay,
                    Some(Duration::from_secs(1))
                );
            }
        }

        mod retry_if {
            use super::*;

            #[test]
            fn retry_if_should_set_predicate() {
                let builder = reqwest::Client::new().get("http://localhost");

                let request = builder.retry_if(|_, _| true);

                assert!(request.options().retry_if.is_some());
            }
        }
    }

    mod configured_request {
        use super::*;

        mod display_lines {
            use super::*;

            #[test]
            fn display_lines_should_set_lines() {
                let blocking = reqwest::blocking::Client::new()
                    .get("http://localhost")
                    .stream()
                    .display_lines(3);

                assert_eq!(blocking.options().display_lines, 3);
                #[cfg(feature = "tokio")]
                assert_eq!(
                    ConfiguredRequest::new(reqwest::Client::new().get("http://localhost"))
                        .display_lines(4)
                        .options()
                        .display_lines,
                    4
                );
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
                    retries: 3,
                    stream: true,
                    ..Default::default()
                };

                let request = ConfiguredRequest::with_options(
                    reqwest::blocking::Client::new().get("http://localhost"),
                    options.clone(),
                );

                assert_eq!(request.options(), &options);
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

        mod retry_backoff {
            use super::*;

            #[test]
            fn retry_backoff_should_set_factor() {
                let blocking = reqwest::blocking::Client::new()
                    .get("http://localhost")
                    .retry(1, Duration::ZERO)
                    .retry_backoff(2);

                assert_eq!(blocking.options().retry_backoff, 2);
                #[cfg(feature = "tokio")]
                assert_eq!(
                    ConfiguredRequest::new(reqwest::Client::new().get("http://localhost"))
                        .retry_backoff(3)
                        .options()
                        .retry_backoff,
                    3
                );
            }
        }

        mod retry_max_delay {
            use super::*;

            #[test]
            fn retry_max_delay_should_set_max_delay() {
                let blocking = reqwest::blocking::Client::new()
                    .get("http://localhost")
                    .retry_max_delay(Duration::from_secs(1));

                assert_eq!(
                    blocking.options().retry_max_delay,
                    Some(Duration::from_secs(1))
                );
                #[cfg(feature = "tokio")]
                assert_eq!(
                    ConfiguredRequest::new(reqwest::Client::new().get("http://localhost"))
                        .retry_max_delay(Duration::from_secs(2))
                        .options()
                        .retry_max_delay,
                    Some(Duration::from_secs(2))
                );
            }
        }

        mod retry_if {
            use super::*;

            #[test]
            fn retry_if_should_set_predicate() {
                let blocking = reqwest::blocking::Client::new()
                    .get("http://localhost")
                    .retry(1, Duration::ZERO)
                    .retry_if(|_, _| true);

                assert!(blocking.options().retry_if.is_some());
                #[cfg(feature = "tokio")]
                assert!(
                    ConfiguredRequest::new(reqwest::Client::new().get("http://localhost"))
                        .retry_if(|_, _| true)
                        .options()
                        .retry_if
                        .is_some()
                );
            }
        }

        mod send_blocking {
            use super::*;

            #[test]
            fn send_blocking_should_send_request_with_options() {
                let server = serve(ok);
                let (request, observer) = observed();

                blocking_get(&request, &server.url)
                    .stream()
                    .send_blocking(&request)
                    .unwrap();

                assert!(observer.events().contains(&"body body".to_string()));
            }
        }

        mod send_blocking_to {
            use super::*;

            #[test]
            fn send_blocking_to_should_send_request_with_options_to_sink() {
                let server = serve(ok);
                let (request, observer) = observed();
                let mut received = Vec::new();

                blocking_get(&request, &server.url)
                    .stream()
                    .send_blocking_to(&request, &mut |data| {
                        received.extend_from_slice(data);
                        Ok(())
                    })
                    .unwrap();

                assert!(observer.events().contains(&"body body".to_string()));
                assert_eq!(received, b"body");
            }
        }

        #[cfg(feature = "tokio")]
        mod send_async {
            use super::*;

            #[test]
            fn send_async_should_send_request_with_options() {
                let server = serve(ok);
                let (request, observer) = observed();

                block_on(get(&request, &server.url).stream().send_async(&request)).unwrap();

                assert!(observer.events().contains(&"body body".to_string()));
            }
        }

        #[cfg(feature = "tokio")]
        mod send_async_to {
            use super::*;

            #[test]
            fn send_async_to_should_send_request_with_options_to_sink() {
                let server = serve(ok);
                let (request, observer) = observed();
                let mut received = Vec::new();

                block_on(get(&request, &server.url).stream().send_async_to(
                    &request,
                    &mut |data| {
                        received.extend_from_slice(data);
                        Ok(())
                    },
                ))
                .unwrap();

                assert!(observer.events().contains(&"body body".to_string()));
                assert_eq!(received, b"body");
            }
        }
    }

    mod no_observer {
        use super::*;

        mod request_observer {
            use super::*;

            mod invalid {
                use super::*;

                #[test]
                fn invalid_should_ignore_error() {
                    let request = RequestImpl::new();

                    let error = blocking_get(&request, "not a url")
                        .send_blocking(&request)
                        .unwrap_err();

                    assert_eq!(error.description(), "invalid request: builder error");
                }
            }
        }
    }

    mod request_run {
        use super::*;

        #[test]
        fn request_run_should_expose_request_and_options() {
            // GIVEN a request with headers, a body and a timeout
            let url = Url::parse("https://example.com/path").unwrap();
            let mut request = reqwest::blocking::Request::new(Method::POST, url.clone());
            request
                .headers_mut()
                .insert("x-key", "value".parse().unwrap());
            *request.body_mut() = Some("body".into());
            *request.timeout_mut() = Some(Duration::from_secs(5));
            let options = RequestOptions {
                retries: 1,
                ..Default::default()
            };

            // WHEN observing it
            let run = blocking_run(&request, &options);

            // THEN the run shows its first attempt
            assert_eq!(run.attempt(), 1);
            assert_eq!(run.method(), Method::POST);
            assert_eq!(run.url(), &url);
            assert_eq!(run.headers()["x-key"], "value");
            assert_eq!(run.body(), Some(b"body".as_slice()));
            assert_eq!(run.timeout(), Some(Duration::from_secs(5)));
            assert_eq!(run.options(), &options);
        }

        mod id {
            use super::*;

            #[derive(Default)]
            struct Ids(Mutex<Vec<u64>>);

            impl RequestObserver for Ids {
                fn started(&self, run: &RequestRun) {
                    self.0.lock().unwrap().push(run.id());
                }

                fn retrying(&self, run: &RequestRun, _result: &Result<RequestOutput>) {
                    self.0.lock().unwrap().push(run.id());
                }

                fn finished(&self, run: &RequestRun, result: &Result<RequestOutput>) {
                    assert!(result.is_err());
                    self.0.lock().unwrap().push(run.id());
                }
            }

            fn ids_of_two_runs(send: impl Fn(&RequestImpl, &str)) -> Vec<u64> {
                let ids = Arc::new(Ids::default());
                let request = RequestImpl::new();
                request.set_observer(ids.clone());
                let url = closed_url();

                send(&request, &url);
                send(&request, &url);

                ids.0.lock().unwrap().clone()
            }

            #[test]
            fn id_should_be_same_for_attempts_and_unique_per_run() {
                let ids = ids_of_two_runs(|request, url| {
                    let _ = blocking_get(request, url)
                        .retry(1, Duration::ZERO)
                        .send_blocking(request);
                });

                assert_ne!(ids[0], 0);
                assert_eq!(ids[..3], [ids[0]; 3]);
                assert_eq!(ids[3..], [ids[3]; 3]);
                assert_ne!(ids[0], ids[3]);
            }

            #[cfg(feature = "tokio")]
            #[test]
            fn id_async_should_be_same_for_attempts_and_unique_per_run() {
                let ids = ids_of_two_runs(|request, url| {
                    let _ = block_on(
                        get(request, url)
                            .retry(1, Duration::ZERO)
                            .send_async(request),
                    );
                });

                assert_ne!(ids[0], 0);
                assert_eq!(ids[..3], [ids[0]; 3]);
                assert_eq!(ids[3..], [ids[3]; 3]);
                assert_ne!(ids[0], ids[3]);
            }
        }

        mod target {
            use super::*;

            #[test]
            fn target_should_strip_credentials_query_and_fragment() {
                let url = Url::parse("https://user:secret@example.com:8080/path?token=secret#part")
                    .unwrap();
                let request = reqwest::blocking::Request::new(Method::GET, url);
                let options = RequestOptions::default();

                let target = blocking_run(&request, &options).target();

                assert_eq!(target, "GET https://example.com:8080/path");
            }
        }

        mod retry_delay {
            use super::*;

            #[test]
            fn retry_delay_should_multiply_delay_by_backoff_per_attempt() {
                let request =
                    reqwest::blocking::Request::new(Method::GET, Url::parse("http://h").unwrap());

                for (delay, backoff, attempt, expected) in [
                    (Duration::from_millis(100), 1, 3, Duration::from_millis(100)),
                    (Duration::from_millis(100), 2, 1, Duration::from_millis(100)),
                    (Duration::from_millis(100), 2, 3, Duration::from_millis(400)),
                    (Duration::MAX, 2, 2, Duration::MAX),
                ] {
                    let options = RequestOptions {
                        retry_delay: delay,
                        retry_backoff: backoff,
                        ..Default::default()
                    };
                    let mut run = blocking_run(&request, &options);
                    run.attempt = attempt;

                    assert_eq!(
                        run.retry_delay(&Err(crate::Error::request("failed"))),
                        expected,
                        "delay: {delay:?}, backoff: {backoff}, attempt: {attempt}"
                    );
                }
            }

            #[test]
            fn retry_delay_should_wait_at_least_retry_after_and_at_most_max_delay() {
                // GIVEN failed responses asking to wait 5 and 30 seconds,
                // since a date long passed and until one far ahead
                let request =
                    reqwest::blocking::Request::new(Method::GET, Url::parse("http://h").unwrap());
                let failed = |retry_after: &str| {
                    let mut output = RequestOutput {
                        status: 503,
                        ..Default::default()
                    };
                    output
                        .headers
                        .insert(reqwest::header::RETRY_AFTER, retry_after.parse().unwrap());
                    Err(crate::Error::request("failed").with_source(output))
                };
                let options = RequestOptions {
                    retry_delay: Duration::from_secs(10),
                    retry_max_delay: Some(Duration::from_secs(20)),
                    ..Default::default()
                };
                let run = blocking_run(&request, &options);

                // WHEN getting the delays before their retries
                let delays = [
                    failed("5"),
                    failed(" 30 "),
                    failed("Sun, 06 Nov 1994 08:49:37 GMT"),
                    failed("Sun, 06 Nov 9994 08:49:37 GMT"),
                    failed("soon"),
                    failed("\u{e9}"),
                    Err(crate::Error::request("failed").with_source(RequestOutput::default())),
                ]
                .map(|result| run.retry_delay(&result));

                // THEN they are at least the delay and at most the max delay
                assert_eq!(
                    delays.map(|delay| delay.as_secs()),
                    [10, 20, 10, 20, 10, 10, 10]
                );
            }
        }
    }

    mod http_date {
        use super::*;

        #[test]
        fn http_date_should_parse_imf_fixdate() {
            for (value, seconds) in [
                ("Sun, 06 Nov 1994 08:49:37 GMT", 784_111_777),
                ("Thu, 01 Jan 1970 00:00:00 GMT", 0),
                ("Tue, 29 Feb 2000 23:59:59 GMT", 951_868_799),
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
            ] {
                assert_eq!(http_date(value), None, "value: {value}");
            }
        }
    }

    mod request_options {
        use super::*;

        mod default {
            use super::*;

            #[test]
            fn default_should_not_stream_or_retry() {
                let options = RequestOptions::default();

                assert_eq!(options.display_lines, 10);
                assert!(!options.stream);
                assert_eq!(options.retries, 0);
                assert_eq!(options.retry_delay, Duration::ZERO);
                assert_eq!(options.retry_backoff, 1);
                assert_eq!(options.retry_max_delay, None);
                assert_eq!(options.retry_if, None);
            }
        }
    }

    mod request_retry_if {
        use super::*;

        mod new {
            use super::*;

            #[test]
            fn new_should_wrap_predicate() {
                let retry_if = RequestRetryIf::new(|run, _| run.attempt() == 1);
                let request =
                    reqwest::blocking::Request::new(Method::GET, Url::parse("http://h").unwrap());
                let options = RequestOptions::default();

                assert!((retry_if.0)(
                    &blocking_run(&request, &options),
                    &Ok(RequestOutput::default())
                ));
            }
        }

        mod safe {
            use super::*;

            fn transport(kind: TransportErrorKind) -> Result<RequestOutput> {
                Err(crate::Error::request("transport").with_source(TransportError::new(kind)))
            }

            /// Returns whether `safe` accepts each failure for `method`.
            fn accepted(method: Method) -> Vec<bool> {
                let request =
                    reqwest::blocking::Request::new(method, Url::parse("http://h").unwrap());
                let options = RequestOptions::default();
                let run = blocking_run(&request, &options);

                [
                    Ok(RequestOutput::default()),
                    Err(crate::Error::request("other")),
                    transport(TransportErrorKind::Connect),
                    transport(TransportErrorKind::Timeout),
                    transport(TransportErrorKind::Other),
                    Err(status_error(404)),
                    Err(status_error(408)),
                    Err(status_error(429)),
                    Err(status_error(500)),
                    Err(status_error(502)),
                    Err(status_error(503)),
                ]
                .iter()
                .map(|result| RequestRetryIf::safe(&run, result))
                .collect()
            }

            #[test]
            fn safe_idempotent_method_should_accept_transient_failures() {
                for method in [
                    Method::GET,
                    Method::HEAD,
                    Method::PUT,
                    Method::DELETE,
                    Method::OPTIONS,
                    Method::TRACE,
                ] {
                    assert_eq!(
                        accepted(method.clone()),
                        [
                            false, false, true, true, true, false, true, true, true, true, true
                        ],
                        "method: {method}"
                    );
                }
            }

            #[test]
            fn safe_non_idempotent_method_should_accept_failures_before_processing_only() {
                for method in [Method::POST, Method::PATCH, Method::CONNECT] {
                    assert_eq!(
                        accepted(method.clone()),
                        [
                            false, false, true, false, false, false, true, true, false, false, true
                        ],
                        "method: {method}"
                    );
                }
            }
        }

        mod std_fmt_debug {
            use super::*;

            #[test]
            fn fmt_should_name_type() {
                assert_eq!(
                    format!("{:?}", RequestRetryIf::new(|_, _| true)),
                    "RequestRetryIf"
                );
            }
        }

        mod partial_eq {
            use super::*;

            mod eq {
                use super::*;

                #[test]
                fn eq_should_compare_closure_identity() {
                    let retry_if = RequestRetryIf::new(|_, _| true);

                    assert_eq!(retry_if, retry_if.clone());
                    assert_ne!(retry_if, RequestRetryIf::new(|_, _| true));
                }
            }
        }
    }

    mod request_output {
        use super::*;

        mod success {
            use super::*;

            #[test]
            fn success_should_accept_2xx_only() {
                for (status, expected) in [
                    (199, false),
                    (200, true),
                    (204, true),
                    (299, true),
                    (300, false),
                    (404, false),
                    (500, false),
                ] {
                    let output = RequestOutput {
                        status,
                        ..Default::default()
                    };

                    assert_eq!(output.success(), expected, "status: {status}");
                }
            }
        }

        mod text {
            use super::*;

            #[test]
            fn text_should_return_utf8_body() {
                let output = RequestOutput {
                    body: b"body".to_vec(),
                    ..Default::default()
                };

                assert_eq!(output.text().unwrap(), "body");
            }

            #[test]
            fn text_invalid_utf8_should_return_error() {
                let output = RequestOutput {
                    body: b"\xFF".to_vec(),
                    ..Default::default()
                };

                let error = output.text().unwrap_err();

                assert!(error.description().starts_with("body is not valid UTF-8: "));
                assert!(error.source_as::<std::str::Utf8Error>().is_some());
            }
        }

        mod default {
            use super::*;

            #[test]
            fn default_should_display_10_lines() {
                let output = RequestOutput::default();

                assert_eq!(output.status, 0);
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
                fn eq_should_ignore_display_lines() {
                    let output = RequestOutput {
                        status: 200,
                        body: b"body".to_vec(),
                        ..Default::default()
                    };

                    assert_eq!(
                        output,
                        RequestOutput {
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

        mod text_lossy {
            use super::*;

            #[test]
            fn text_lossy_should_replace_invalid_sequences() {
                let output = RequestOutput {
                    body: b"body\xFF".to_vec(),
                    ..Default::default()
                };

                assert_eq!(output.text_lossy(), "body\u{FFFD}");
            }
        }

        mod std_fmt_debug {
            use super::*;

            #[test]
            fn fmt_should_show_fields_and_body() {
                let output = RequestOutput {
                    status: 200,
                    body: b"body".to_vec(),
                    ..Default::default()
                };

                assert_eq!(
                    format!("{output:?}"),
                    "RequestOutput { status: 200, headers: {}, body: 4 bytes \"body\", \
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

        mod output {
            use super::*;

            #[test]
            fn output_should_return_response_recorded() {
                let output = RequestOutput {
                    status: 200,
                    ..Default::default()
                };

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

    #[cfg(feature = "tokio")]
    mod request_cancelled {
        use super::*;

        mod std_fmt_display {
            use super::*;

            #[test]
            fn fmt_should_display_cancelled() {
                assert_eq!(RequestCancelled.to_string(), "cancelled");
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

    mod default_clients {
        use super::*;

        mod reqwest_env {
            use super::*;

            #[test]
            fn reqwest_env_changed_proxy_should_rebuild_default_clients() {
                // GIVEN built default clients
                let proxy = serve(ok);
                let request = RequestImpl::new();
                request.blocking_client().unwrap();
                #[cfg(feature = "tokio")]
                request.client().unwrap();

                // WHEN setting a proxy and an unrelated variable
                request.set_var("HTTP_PROXY".to_string(), proxy.url.clone());
                request.set_var("PATH".to_string(), "/bin".to_string());

                // THEN the requests of both go through the proxy
                blocking_get(&request, "http://example.invalid/")
                    .send_blocking(&request)
                    .unwrap();
                #[cfg(feature = "tokio")]
                block_on(get(&request, "http://example.invalid/").send_async(&request)).unwrap();
                #[cfg(feature = "tokio")]
                assert_eq!(proxy.received().len(), 2);
            }

            #[test]
            fn reqwest_env_changed_proxy_should_keep_clients_set() {
                let request = RequestImpl::new();
                request.set_blocking_client(invalid_client());
                #[cfg(feature = "tokio")]
                request.set_client(invalid_async_client());

                request.set_var("HTTP_PROXY".to_string(), "http://proxy".to_string());

                assert!(request.blocking_client().is_err());
                #[cfg(feature = "tokio")]
                assert!(request.client().is_err());
            }

            #[test]
            fn reqwest_env_removed_proxy_should_send_directly() {
                // GIVEN a default client using a proxy
                let proxy = serve(ok);
                let server = serve(ok);
                let env = crate::env::EnvImpl::new();
                crate::Env::set_var(&env, "HTTP_PROXY", proxy.url.as_str());
                let request = RequestImpl::with_env(env.shared.clone());
                request.blocking_client().unwrap();

                // WHEN removing the proxy
                request.remove_var("HTTP_PROXY");

                // THEN requests go directly
                blocking_get(&request, &server.url)
                    .send_blocking(&request)
                    .unwrap();
                assert!(proxy.received().is_empty());
                assert_eq!(server.received().len(), 1);
            }
        }
    }

    mod default_client {
        use super::*;

        fn default_builder(_: &ReqwestEnv) -> reqwest::blocking::ClientBuilder {
            reqwest::blocking::Client::builder()
        }

        fn invalid_default(_: &ReqwestEnv) -> reqwest::blocking::ClientBuilder {
            invalid_client()
        }

        fn proxied() -> ReqwestEnv {
            ReqwestEnv::new(&[("HTTP_PROXY".to_string(), "http://proxy".to_string())])
        }

        mod get {
            use super::*;

            #[test]
            fn get_should_build_client_once() {
                static BUILDS: AtomicUsize = AtomicUsize::new(0);
                fn build(
                    builder: reqwest::blocking::ClientBuilder,
                ) -> Result<reqwest::blocking::Client> {
                    BUILDS.fetch_add(1, Ordering::Relaxed);
                    build_blocking_client(builder)
                }
                let client = DefaultClient::new();

                client
                    .get(&ReqwestEnv::default(), &default_builder, build)
                    .unwrap();
                client
                    .get(&ReqwestEnv::default(), &default_builder, build)
                    .unwrap();

                assert_eq!(BUILDS.load(Ordering::Relaxed), 1);
            }

            #[test]
            fn get_should_build_default_builder() {
                let client = DefaultClient::new();

                let result = client.get(
                    &ReqwestEnv::default(),
                    &invalid_default,
                    build_blocking_client,
                );

                assert!(result.is_err());
            }

            #[test]
            fn get_failed_build_should_return_error_every_time() {
                let client = DefaultClient::new();
                client.set(invalid_client());

                let first = client
                    .get(
                        &ReqwestEnv::default(),
                        &default_builder,
                        build_blocking_client,
                    )
                    .unwrap_err();
                let second = client
                    .get(
                        &ReqwestEnv::default(),
                        &default_builder,
                        build_blocking_client,
                    )
                    .unwrap_err();

                assert!(
                    first
                        .description()
                        .starts_with("client could not be built: ")
                );
                assert_eq!(first.description(), second.description());
            }

            #[test]
            fn get_after_panicking_build_of_set_builder_should_return_error() {
                // GIVEN a builder set whose build panicked
                fn panicking(
                    _: reqwest::blocking::ClientBuilder,
                ) -> Result<reqwest::blocking::Client> {
                    panic!("build failed")
                }
                let client = DefaultClient::new();
                client.set(reqwest::blocking::Client::builder());
                let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    client.get(&ReqwestEnv::default(), &default_builder, panicking)
                }));

                // WHEN getting the client again
                let result = client.get(
                    &ReqwestEnv::default(),
                    &default_builder,
                    build_blocking_client,
                );

                // THEN it fails instead of building the default builder
                assert!(panicked.is_err());
                assert_eq!(
                    result.unwrap_err().description(),
                    "client could not be built: its builder was lost by a panic"
                );
            }

            #[test]
            fn get_other_env_should_build_default_builder_again() {
                // GIVEN a client built from the default builder
                let client = DefaultClient::new();
                client
                    .get(
                        &ReqwestEnv::default(),
                        &default_builder,
                        build_blocking_client,
                    )
                    .unwrap();

                // WHEN getting it with another env
                let result = client.get(&proxied(), &invalid_default, build_blocking_client);

                // THEN the default builder is built again
                assert!(result.is_err());
            }

            #[test]
            fn get_earlier_env_after_later_should_build_later_again() {
                // GIVEN a client built with an env taken before a change
                let client = DefaultClient::new();
                client
                    .get(&proxied(), &default_builder, build_blocking_client)
                    .unwrap();
                client
                    .get(
                        &ReqwestEnv::default(),
                        &default_builder,
                        build_blocking_client,
                    )
                    .unwrap();

                // WHEN getting it with the env after the change
                let result = client.get(&proxied(), &invalid_default, build_blocking_client);

                // THEN it is not kept
                assert!(result.is_err());
            }

            #[test]
            fn get_set_builder_other_env_should_keep_client() {
                // GIVEN a client built from a builder set
                let client = DefaultClient::new();
                client.set(reqwest::blocking::Client::builder());
                client
                    .get(
                        &ReqwestEnv::default(),
                        &default_builder,
                        build_blocking_client,
                    )
                    .unwrap();

                // WHEN getting it with another env
                let result = client.get(&proxied(), &invalid_default, build_blocking_client);

                // THEN it is kept
                assert!(result.is_ok());
            }
        }

        mod set {
            use super::*;

            #[test]
            fn set_should_replace_built_client() {
                // GIVEN a built default client
                let client = DefaultClient::new();
                client
                    .get(
                        &ReqwestEnv::default(),
                        &default_builder,
                        build_blocking_client,
                    )
                    .unwrap();

                // WHEN setting an invalid builder
                client.set(invalid_client());

                // THEN the client is built again from it
                assert!(
                    client
                        .get(
                            &ReqwestEnv::default(),
                            &default_builder,
                            build_blocking_client
                        )
                        .is_err()
                );
            }
        }
    }

    mod reqwest_env {
        use super::*;

        fn vars(vars: &[(&str, &str)]) -> Vec<(String, String)> {
            vars.iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect()
        }

        /// Returns which of the proxies (`a` or `b`, or `direct`) a GET of
        /// `url` through a client from `env` reaches.
        fn route(env: &ReqwestEnv, url: &str, a: &Server, b: &Server) -> &'static str {
            let client = env.blocking_client_builder().build().unwrap();
            let result = client.get(url).send();
            match (a.received().len(), b.received().len()) {
                (1, 0) => "a",
                (0, 1) => "b",
                _ => {
                    assert!(result.is_err(), "{result:?}");
                    "direct"
                }
            }
        }

        mod new {
            use super::*;

            #[test]
            fn new_should_keep_proxy_variables_only() {
                let all = vars(&[
                    ("PATH", "/bin"),
                    ("HTTPS_PROXY", "http://p"),
                    ("no_proxy", "h"),
                ]);

                let env = ReqwestEnv::new(&all);

                assert_eq!(
                    env,
                    ReqwestEnv(vars(&[("HTTPS_PROXY", "http://p"), ("no_proxy", "h")]))
                );
            }

            #[cfg(windows)]
            #[test]
            fn new_should_keep_proxy_variables_of_any_case() {
                let all = vars(&[("Path", "/bin"), ("Https_Proxy", "http://p")]);

                let env = ReqwestEnv::new(&all);

                assert_eq!(env, ReqwestEnv(vars(&[("Https_Proxy", "http://p")])));
            }

            #[cfg(not(windows))]
            #[test]
            fn new_should_keep_proxy_variables_of_exact_case_only() {
                let all = vars(&[("Https_Proxy", "http://p")]);

                let env = ReqwestEnv::new(&all);

                assert_eq!(env, ReqwestEnv::default());
            }
        }

        mod var {
            use super::*;

            #[cfg(windows)]
            #[test]
            fn var_should_ignore_case() {
                let env = ReqwestEnv::new(&vars(&[("Http_Proxy", "http://a")]));

                assert_eq!(env.var(&["HTTP_PROXY"]), Some("http://a"));
            }

            #[cfg(not(windows))]
            #[test]
            fn var_should_match_exact_case_only() {
                let env = ReqwestEnv::new(&vars(&[("http_proxy", "http://a")]));

                assert_eq!(env.var(&["HTTP_PROXY"]), None);
                assert_eq!(env.var(&["HTTP_PROXY", "http_proxy"]), Some("http://a"));
            }
        }

        mod blocking_client_builder {
            use super::*;

            #[test]
            fn blocking_client_builder_should_resolve_proxies_like_hyper_util() {
                // GIVEN two proxies a and b and a target that is not reachable
                let target = closed_url();

                for (env, expected) in [
                    (vec![], "direct"),
                    (vec![("HTTP_PROXY", "A")], "a"),
                    (vec![("http_proxy", "A")], "a"),
                    (vec![("ALL_PROXY", "A")], "a"),
                    (vec![("all_proxy", "A")], "a"),
                    (vec![("HTTP_PROXY", "A"), ("http_proxy", "B")], "a"),
                    (vec![("HTTP_PROXY", "B"), ("ALL_PROXY", "A")], "b"),
                    (vec![("HTTP_PROXY", "::invalid"), ("ALL_PROXY", "A")], "a"),
                    (vec![("HTTPS_PROXY", "A")], "direct"),
                    (
                        vec![("HTTP_PROXY", "A"), ("NO_PROXY", "127.0.0.1")],
                        "direct",
                    ),
                    (
                        vec![("HTTP_PROXY", "A"), ("no_proxy", "127.0.0.1")],
                        "direct",
                    ),
                    (vec![("HTTP_PROXY", "A"), ("NO_PROXY", "other.com")], "a"),
                    (
                        vec![("HTTP_PROXY", "A"), ("REQUEST_METHOD", "GET")],
                        "direct",
                    ),
                ] {
                    let a = serve(ok);
                    let b = serve(ok);
                    let env = ReqwestEnv::new(&vars(
                        &env.iter()
                            .map(|(key, value)| {
                                let value = match *value {
                                    "A" => a.url.as_str(),
                                    "B" => b.url.as_str(),
                                    other => other,
                                };
                                (*key, value)
                            })
                            .collect::<Vec<_>>(),
                    ));

                    // WHEN sending a request through a client from the variables
                    let route = route(&env, &target, &a, &b);

                    // THEN it takes the expected proxy
                    assert_eq!(route, expected, "env: {env:?}");
                }
            }

            #[test]
            fn blocking_client_builder_should_proxy_https_by_connect() {
                let proxy = serve(not_found);
                let env = ReqwestEnv::new(&vars(&[("HTTPS_PROXY", &proxy.url)]));
                let client = env.blocking_client_builder().build().unwrap();

                let _ = client.get("https://example.invalid/path").send();

                assert!(proxy.received()[0].starts_with("CONNECT example.invalid:443 "));
            }

            #[test]
            fn blocking_client_builder_should_time_out_connecting_after_30_seconds() {
                let builder = ReqwestEnv::default().blocking_client_builder();

                assert!(format!("{builder:?}").contains("connect_timeout: 30s"));
            }
        }

        mod blocking_client_builder_with {
            use super::*;

            #[test]
            fn blocking_client_builder_with_should_time_out_each_read_not_whole_body() {
                // GIVEN a body arriving in pauses shorter than the read timeout but
                // longer than it in total
                let server =
                    serve_chunks(Duration::from_millis(100), &["a", "b", "c", "d", "e", "f"]);
                let client = ReqwestEnv::default()
                    .blocking_client_builder_with(CONNECT_TIMEOUT, Duration::from_millis(500))
                    .build()
                    .unwrap();
                let mut body = Vec::new();

                // WHEN reading it in parts
                let result = client.get(&server.url).send().unwrap().copy_to(&mut body);

                // THEN it is not cut
                assert!(result.is_ok(), "{result:?}");
                assert_eq!(body, b"abcdef");
            }

            #[test]
            fn blocking_client_builder_with_should_time_out_stalled_read() {
                let server = serve_chunks(Duration::from_secs(10), &["late"]);
                let client = ReqwestEnv::default()
                    .blocking_client_builder_with(CONNECT_TIMEOUT, Duration::from_secs(1))
                    .build()
                    .unwrap();

                let error = client
                    .get(&server.url)
                    .send()
                    .unwrap()
                    .copy_to(&mut Vec::new())
                    .unwrap_err();

                assert_eq!(
                    TransportError::from(error).kind(),
                    TransportErrorKind::Timeout
                );
            }
        }

        #[cfg(feature = "tokio")]
        mod client_builder_with {
            use super::*;

            #[test]
            fn client_builder_with_should_not_time_out_slow_response_in_total() {
                // GIVEN a body arriving in pauses shorter than the read timeout but
                // longer than it in total
                let server =
                    serve_chunks(Duration::from_millis(100), &["a", "b", "c", "d", "e", "f"]);
                let client = ReqwestEnv::default()
                    .client_builder_with(CONNECT_TIMEOUT, Duration::from_millis(500))
                    .build()
                    .unwrap();

                // WHEN receiving it whole
                let body =
                    block_on(async { client.get(&server.url).send().await.unwrap().bytes().await });

                // THEN it is not cut
                assert_eq!(body.unwrap(), "abcdef");
            }

            #[test]
            fn client_builder_with_should_time_out_stalled_read() {
                let server = serve_chunks(Duration::from_secs(10), &["late"]);
                let client = ReqwestEnv::default()
                    .client_builder_with(CONNECT_TIMEOUT, Duration::from_secs(1))
                    .build()
                    .unwrap();

                let error =
                    block_on(async { client.get(&server.url).send().await.unwrap().bytes().await })
                        .unwrap_err();

                assert_eq!(
                    TransportError::from(error).kind(),
                    TransportErrorKind::Timeout
                );
            }
        }

        #[cfg(feature = "tokio")]
        mod client_builder {
            use super::*;

            #[test]
            fn client_builder_should_use_proxies() {
                let proxy = serve(ok);
                let env = ReqwestEnv::new(&vars(&[("HTTP_PROXY", &proxy.url)]));
                let client = env.client_builder().build().unwrap();

                let output = block_on(async {
                    client
                        .get("http://example.invalid/path")
                        .send()
                        .await
                        .unwrap()
                        .status()
                });

                assert_eq!(output, 200);
                assert!(
                    proxy.received()[0].starts_with("GET http://example.invalid/path HTTP/1.1")
                );
            }

            #[test]
            fn client_builder_should_time_out_connecting_after_30_seconds_only() {
                let debug = format!("{:?}", ReqwestEnv::default().client_builder());

                assert!(debug.contains("connect_timeout: 30s"), "{debug}");
                assert!(!debug.contains(", timeout: "), "{debug}");
            }
        }
    }

    mod observed_body {
        use super::*;

        fn streamed_options() -> RequestOptions {
            RequestOptions {
                stream: true,
                ..Default::default()
            }
        }

        fn get_request() -> reqwest::blocking::Request {
            reqwest::blocking::Request::new(Method::GET, Url::parse("http://h").unwrap())
        }

        mod new {
            use super::*;

            #[test]
            fn new_without_stream_should_not_report_data() {
                let observer = RecordingObserver::default();
                let request = get_request();
                let options = RequestOptions::default();
                let run = blocking_run(&request, &options);
                let mut body = ObservedBody::new(&observer, &run, None);

                body.push(b"a").unwrap();

                assert_eq!(body.body, b"a");
                assert!(observer.events().is_empty());
            }
        }

        mod push {
            use super::*;

            #[test]
            fn push_should_collect_and_report_data() {
                let observer = RecordingObserver::default();
                let request = get_request();
                let options = streamed_options();
                let run = blocking_run(&request, &options);
                let mut body = ObservedBody::new(&observer, &run, None);

                body.push(b"a").unwrap();
                body.push(b"b").unwrap();

                assert_eq!(body.body, b"ab");
                assert_eq!(observer.events(), ["body a", "body b"]);
            }

            #[test]
            fn push_with_sink_should_pass_data_instead_of_collecting() {
                let observer = RecordingObserver::default();
                let request = get_request();
                let options = streamed_options();
                let run = blocking_run(&request, &options);
                let mut received = Vec::new();
                let mut sink = |data: &[u8]| {
                    received.extend_from_slice(data);
                    Ok(())
                };
                let mut body = ObservedBody::new(&observer, &run, Some(&mut sink));

                body.push(b"a").unwrap();
                body.push(b"b").unwrap();

                assert!(body.body.is_empty());
                assert_eq!(observer.events(), ["body a", "body b"]);
                assert_eq!(received, b"ab");
            }

            #[test]
            fn push_failing_sink_should_return_its_error() {
                let observer = RecordingObserver::default();
                let request = get_request();
                let options = RequestOptions::default();
                let run = blocking_run(&request, &options);
                let mut sink = |_: &[u8]| Err(crate::Error::fs("disk full"));
                let mut body = ObservedBody::new(&observer, &run, Some(&mut sink));

                let error = body.push(b"a").unwrap_err();

                assert_eq!(error.description(), "disk full");
            }
        }

        mod std_io_write {
            use super::*;

            mod write {
                use super::*;

                #[test]
                fn write_should_push_whole_data() {
                    let observer = RecordingObserver::default();
                    let request = get_request();
                    let options = streamed_options();
                    let run = blocking_run(&request, &options);
                    let mut body = ObservedBody::new(&observer, &run, None);

                    assert_eq!(body.write(b"data").unwrap(), 4);

                    assert_eq!(body.body, b"data");
                    assert_eq!(observer.events(), ["body data"]);
                }

                #[test]
                fn write_failing_sink_should_keep_its_error() {
                    let observer = RecordingObserver::default();
                    let request = get_request();
                    let options = RequestOptions::default();
                    let run = blocking_run(&request, &options);
                    let mut sink = |_: &[u8]| Err(crate::Error::fs("disk full"));
                    let mut body = ObservedBody::new(&observer, &run, Some(&mut sink));

                    let error = body.write(b"data").unwrap_err();

                    assert_eq!(error.to_string(), "sink failed");
                    assert_eq!(body.sink_error.unwrap().description(), "disk full");
                }
            }

            mod flush {
                use super::*;

                #[test]
                fn flush_should_succeed() {
                    let observer = RecordingObserver::default();
                    let request = get_request();
                    let options = RequestOptions::default();
                    let run = blocking_run(&request, &options);

                    assert!(ObservedBody::new(&observer, &run, None).flush().is_ok());
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
                let env = crate::env::EnvImpl::new();
                crate::Env::set_var(&env, "HTTP_PROXY", proxy.url.as_str());
                let request = RequestImpl::with_env(env.shared.clone());

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
                    request.set_var("HTTP_PROXY".to_string(), proxy.url.clone());
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
                    request.set_var("HTTP_PROXY".to_string(), proxy.url.clone());
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
                fn send_blocking_should_return_response_and_report_run() {
                    let server = serve(ok);
                    let (request, observer) = observed();
                    let url = format!("{}/path?token=secret", server.url);

                    let output = blocking_get(&request, &url)
                        .send_blocking(&request)
                        .unwrap();

                    assert_eq!(output.status, 200);
                    assert_eq!(output.headers["content-length"], "4");
                    assert_eq!(output.body, b"body");
                    assert_eq!(
                        observer.events(),
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
                fn send_blocking_failed_status_should_return_and_report_error_with_output() {
                    let server = serve(not_found);
                    let (request, observer) = observed();

                    let error =
                        blocking_get(&request, &format!("{}/path?token=secret", server.url))
                            .send_blocking(&request)
                            .unwrap_err();

                    let description = format!("'GET {}/path' failed with status 404", server.url);
                    assert_eq!(error.description(), description);
                    let output = error.request_output().unwrap();
                    assert_eq!(output.status, 404);
                    assert_eq!(output.body, b"missing");
                    assert_eq!(
                        observer.events().last().unwrap(),
                        &format!("finished [Request] {description}")
                    );
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
                fn send_blocking_invalid_request_should_return_error_reporting_it_as_invalid() {
                    let (request, observer) = observed();

                    let error = blocking_get(&request, "not a url")
                        .send_blocking(&request)
                        .unwrap_err();

                    assert_eq!(error.description(), "invalid request: builder error");
                    assert!(error.source_as::<reqwest::Error>().unwrap().is_builder());
                    assert_eq!(
                        observer.events(),
                        ["invalid [Request] invalid request: builder error"]
                    );
                }

                #[test]
                fn send_blocking_unreadable_body_should_return_error_reporting_finished_run() {
                    let (request, observer) = observed();

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
                    let events = observer.events();
                    assert_eq!(events.len(), 2, "{events:?}");
                    assert!(events[0].starts_with("started POST "), "{events:?}");
                    assert!(
                        events[1]
                            .starts_with("finished [Request] request body could not be read: "),
                        "{events:?}"
                    );
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
                    let client = ReqwestEnv::default()
                        .blocking_client_builder_with(CONNECT_TIMEOUT, Duration::from_millis(500))
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
                            builder.stream()
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
                fn send_blocking_streaming_should_report_body_between_start_and_end() {
                    let server = serve(ok);
                    let (request, observer) = observed();

                    let output = blocking_get(&request, &server.url)
                        .stream()
                        .send_blocking(&request)
                        .unwrap();

                    assert_eq!(output.body, b"body");
                    assert_eq!(
                        observer.events(),
                        [
                            format!("started GET {}/", server.url),
                            "body body".to_string(),
                            "finished 200".to_string()
                        ]
                    );
                }

                #[test]
                fn send_blocking_streaming_without_observer_should_return_body() {
                    let server = serve(ok);
                    let request = RequestImpl::new();

                    let output = blocking_get(&request, &server.url)
                        .stream()
                        .send_blocking(&request)
                        .unwrap();

                    assert_eq!(output.body, b"body");
                }

                #[test]
                fn send_blocking_transient_failure_should_retry() {
                    let server = serve(unavailable_once);
                    let (request, observer) = observed();

                    let output = blocking_get(&request, &server.url)
                        .retry(2, Duration::from_millis(1))
                        .retry_backoff(2)
                        .send_blocking(&request)
                        .unwrap();

                    assert_eq!(output.status, 200);
                    assert_eq!(server.received().len(), 2);
                    assert_eq!(
                        observer.events(),
                        [
                            format!("started GET {}/", server.url),
                            "retrying attempt 1".to_string(),
                            "finished 200".to_string()
                        ]
                    );
                }

                #[test]
                fn send_blocking_permanent_failure_should_not_retry() {
                    let server = serve(not_found);
                    let request = RequestImpl::new();

                    let result = blocking_get(&request, &server.url)
                        .retry(2, Duration::ZERO)
                        .send_blocking(&request);

                    assert!(result.is_err());
                    assert_eq!(server.received().len(), 1);
                }

                #[test]
                fn send_blocking_non_idempotent_server_error_should_not_retry() {
                    let server = serve(|_, _| response(500, "error"));
                    let request = RequestImpl::new();

                    let result = request
                        .blocking_client()
                        .unwrap()
                        .post(&server.url)
                        .retry(2, Duration::ZERO)
                        .send_blocking(&request);

                    assert_eq!(result.unwrap_err().request_output().unwrap().status, 500);
                    assert_eq!(server.received().len(), 1);
                }

                #[test]
                fn send_blocking_idempotent_server_error_should_retry() {
                    let server = serve(|_, _| response(500, "error"));
                    let request = RequestImpl::new();

                    let result = request
                        .blocking_client()
                        .unwrap()
                        .put(&server.url)
                        .retry(2, Duration::ZERO)
                        .send_blocking(&request);

                    assert!(result.is_err());
                    assert_eq!(server.received().len(), 3);
                }

                #[test]
                fn send_blocking_retry_if_should_decide_retries() {
                    // GIVEN a permanent failure
                    let server = serve(not_found);
                    let request = RequestImpl::new();

                    // WHEN retrying it once more by a predicate
                    let result = blocking_get(&request, &server.url)
                        .retry(5, Duration::ZERO)
                        .retry_if(|run, _| run.attempt() < 2)
                        .send_blocking(&request);

                    // THEN it is sent twice
                    assert!(result.is_err());
                    assert_eq!(server.received().len(), 2);
                }

                #[test]
                fn send_blocking_streamed_body_should_be_sent_by_each_attempt() {
                    let server = serve(unavailable_once);
                    let request = RequestImpl::new();

                    request
                        .blocking_client()
                        .unwrap()
                        .post(&server.url)
                        .body(reqwest::blocking::Body::new(std::io::Cursor::new(b"data")))
                        .retry(1, Duration::ZERO)
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
                fn send_blocking_to_streaming_should_report_body_to_observer_and_sink() {
                    let server = serve(ok);
                    let (request, observer) = observed();
                    let mut received = Vec::new();

                    blocking_get(&request, &server.url)
                        .stream()
                        .send_blocking_to(&request, &mut |data| {
                            received.extend_from_slice(data);
                            Ok(())
                        })
                        .unwrap();

                    assert!(observer.events().contains(&"body body".to_string()));
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

                    // WHEN sending a request to a sink
                    let result = blocking_get(&request, &server.url).send_blocking_to(
                        &request,
                        &mut |data| {
                            received.extend_from_slice(data);
                            Ok(())
                        },
                    );

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
                    // GIVEN a server closing the connection in the middle of the body
                    let server = serve(truncated);
                    let request = RequestImpl::new();
                    let mut received = Vec::new();

                    // WHEN sending it to a sink retrying any failure
                    let result = blocking_get(&request, &server.url)
                        .retry(2, Duration::ZERO)
                        .retry_if(|_, _| true)
                        .send_blocking_to(&request, &mut collect_into(&mut received));

                    // THEN it is not retried as the sink got a part of the body
                    assert_eq!(transport_kind(&result), Some(TransportErrorKind::Other));
                    assert_eq!(received, b"partial");
                    assert_eq!(server.received().len(), 1);
                }

                #[test]
                fn send_blocking_to_failure_before_body_should_be_retried() {
                    let server = serve(unavailable_once);
                    let request = RequestImpl::new();
                    let mut received = Vec::new();

                    let output = blocking_get(&request, &server.url)
                        .retry(1, Duration::ZERO)
                        .send_blocking_to(&request, &mut collect_into(&mut received))
                        .unwrap();

                    assert_eq!(output.status, 200);
                    assert_eq!(received, b"body");
                    assert_eq!(server.received().len(), 2);
                }

                #[test]
                fn send_blocking_to_failing_sink_should_return_its_error_without_retrying() {
                    let server = serve(ok);
                    let request = RequestImpl::new();

                    let error = blocking_get(&request, &server.url)
                        .retry(2, Duration::ZERO)
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
                fn send_async_should_return_response_and_report_run() {
                    let server = serve(ok);
                    let (request, observer) = observed();

                    let output = block_on(get(&request, &server.url).send_async(&request)).unwrap();

                    assert_eq!(output.status, 200);
                    assert_eq!(output.headers["content-length"], "4");
                    assert_eq!(output.body, b"body");
                    assert_eq!(
                        observer.events(),
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
                fn send_async_invalid_request_should_return_error_reporting_it_as_invalid() {
                    let (request, observer) = observed();

                    let error =
                        block_on(get(&request, "not a url").send_async(&request)).unwrap_err();

                    assert_eq!(error.description(), "invalid request: builder error");
                    assert_eq!(
                        observer.events(),
                        ["invalid [Request] invalid request: builder error"]
                    );
                }

                #[test]
                fn send_async_unreadable_body_should_return_error_reporting_finished_run() {
                    let (request, observer) = observed();
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
                    let events = observer.events();
                    assert_eq!(events.len(), 2, "{events:?}");
                    assert!(events[0].starts_with("started POST "), "{events:?}");
                    assert!(
                        events[1]
                            .starts_with("finished [Request] request body could not be read: "),
                        "{events:?}"
                    );
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
                            builder.stream()
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
                fn send_async_streaming_should_report_body_between_start_and_end() {
                    let server = serve(ok);
                    let (request, observer) = observed();

                    let output =
                        block_on(get(&request, &server.url).stream().send_async(&request)).unwrap();

                    assert_eq!(output.body, b"body");
                    assert_eq!(
                        observer.events(),
                        [
                            format!("started GET {}/", server.url),
                            "body body".to_string(),
                            "finished 200".to_string()
                        ]
                    );
                }

                #[test]
                fn send_async_cancelled_should_report_finished() {
                    // GIVEN a slow server
                    let server = serve(slow);
                    let observer = Arc::new(RecordingObserver::default());
                    let cancelled = Arc::new(CancelledObserver::default());
                    let request = RequestImpl::new();
                    request.set_observer(Arc::new(Observers(observer.clone(), cancelled.clone())));
                    let builder = get(&request, &server.url);

                    // WHEN the request is cancelled by a timeout
                    let result = block_on(async {
                        tokio::time::timeout(
                            Duration::from_millis(100),
                            builder.send_async(&request),
                        )
                        .await
                    });

                    // THEN the run is reported as finished
                    assert!(result.is_err());
                    assert_eq!(
                        observer.events(),
                        [
                            format!("started GET {}/", server.url),
                            format!("finished [Request] 'GET {}/' cancelled", server.url),
                        ]
                    );
                    assert!(cancelled.0.lock().unwrap()[0]);
                }

                #[test]
                fn send_async_transient_failure_should_retry() {
                    let server = serve(unavailable_once);
                    let (request, observer) = observed();

                    let output = block_on(
                        get(&request, &server.url)
                            .retry(2, Duration::from_millis(1))
                            .send_async(&request),
                    )
                    .unwrap();

                    assert_eq!(output.status, 200);
                    assert_eq!(
                        observer.events(),
                        [
                            format!("started GET {}/", server.url),
                            "retrying attempt 1".to_string(),
                            "finished 200".to_string()
                        ]
                    );
                }

                #[test]
                fn send_async_non_idempotent_server_error_should_not_retry() {
                    let server = serve(|_, _| response(500, "error"));
                    let request = RequestImpl::new();
                    let builder = request
                        .client()
                        .unwrap()
                        .patch(&server.url)
                        .retry(2, Duration::ZERO);

                    let result = block_on(builder.send_async(&request));

                    assert_eq!(result.unwrap_err().request_output().unwrap().status, 500);
                    assert_eq!(server.received().len(), 1);
                }

                #[test]
                fn send_async_streamed_body_should_be_sent_by_each_attempt() {
                    let server = serve(unavailable_once);
                    let request = RequestImpl::new();
                    let builder = request
                        .client()
                        .unwrap()
                        .post(&server.url)
                        .body(streamed(b"data"))
                        .retry(1, Duration::ZERO);

                    block_on(builder.send_async(&request)).unwrap();

                    let received = server.received();
                    assert_eq!(received.len(), 2);
                    assert!(
                        received
                            .iter()
                            .all(|request| request.ends_with("\r\n\r\ndata"))
                    );
                }
            }

            #[cfg(feature = "tokio")]
            mod send_async_to {
                use super::*;

                #[test]
                fn send_async_to_should_pass_body_to_sink() {
                    let server = serve(ok);
                    let (request, observer) = observed();
                    let mut received = Vec::new();
                    let builder = get(&request, &server.url).stream();

                    let output = block_on(builder.send_async_to(&request, &mut |data| {
                        received.extend_from_slice(data);
                        Ok(())
                    }))
                    .unwrap();

                    assert!(output.body.is_empty());
                    assert_eq!(received, b"body");
                    assert!(observer.events().contains(&"body body".to_string()));
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
                    let result = block_on(builder.send_async_to(&request, &mut |data| {
                        received.extend_from_slice(data);
                        Ok(())
                    }));

                    // THEN it fails after the sink got the part received
                    assert_eq!(transport_kind(&result), Some(TransportErrorKind::Other));
                    assert_eq!(received, b"partial");
                    let output = interrupted_output(&result);
                    assert_eq!(output.status, 200);
                    assert_eq!(output.headers["content-length"], "100");
                    assert!(output.body.is_empty());
                }

                #[test]
                fn send_async_to_interrupted_body_should_not_be_retried() {
                    // GIVEN a server closing the connection in the middle of the body
                    let server = serve(truncated);
                    let request = RequestImpl::new();
                    let mut received = Vec::new();
                    let builder = get(&request, &server.url)
                        .retry(2, Duration::ZERO)
                        .retry_if(|_, _| true);

                    // WHEN sending it to a sink retrying any failure
                    let result =
                        block_on(builder.send_async_to(&request, &mut collect_into(&mut received)));

                    // THEN it is not retried as the sink got a part of the body
                    assert_eq!(transport_kind(&result), Some(TransportErrorKind::Other));
                    assert_eq!(received, b"partial");
                    assert_eq!(server.received().len(), 1);
                }

                #[test]
                fn send_async_to_failing_sink_should_return_its_error_without_retrying() {
                    let server = serve(ok);
                    let request = RequestImpl::new();
                    let builder = get(&request, &server.url).retry(2, Duration::ZERO);

                    let error = block_on(
                        builder
                            .send_async_to(&request, &mut |_| Err(crate::Error::fs("disk full"))),
                    )
                    .unwrap_err();

                    assert_eq!(error.description(), "disk full");
                    assert_eq!(server.received().len(), 1);
                }
            }
        }
    }

    mod buffer_blocking_body {
        use super::*;

        #[test]
        fn buffer_blocking_body_should_read_streamed_body_into_memory() {
            for body in [
                None,
                Some(reqwest::blocking::Body::from("data")),
                Some(reqwest::blocking::Body::new(std::io::Cursor::new(b"data"))),
            ] {
                let expected = body.as_ref().map(|_| b"data".as_slice());
                let mut request =
                    reqwest::blocking::Request::new(Method::POST, Url::parse("http://h").unwrap());
                *request.body_mut() = body;

                buffer_blocking_body(&mut request).unwrap();

                assert_eq!(
                    request.body().and_then(reqwest::blocking::Body::as_bytes),
                    expected
                );
            }
        }
    }

    #[cfg(feature = "tokio")]
    mod buffer_body {
        use super::*;

        #[test]
        fn buffer_body_should_read_streamed_body_into_memory() {
            for body in [
                None,
                Some(reqwest::Body::from("data")),
                Some(streamed(b"data")),
            ] {
                let expected = body.as_ref().map(|_| b"data".as_slice());
                let mut request =
                    reqwest::Request::new(Method::POST, Url::parse("http://h").unwrap());
                *request.body_mut() = body;

                block_on(buffer_body(&mut request)).unwrap();

                assert_eq!(request.body().and_then(reqwest::Body::as_bytes), expected);
            }
        }
    }

    mod clone_blocking_request {
        use super::*;

        #[test]
        fn clone_blocking_request_should_copy_request() {
            let mut request =
                reqwest::blocking::Request::new(Method::POST, Url::parse("http://h").unwrap());
            *request.body_mut() = Some("data".into());

            let clone = clone_blocking_request(&request);

            assert_eq!(clone.method(), Method::POST);
            assert_eq!(
                clone.body().and_then(reqwest::blocking::Body::as_bytes),
                Some(b"data".as_slice())
            );
        }

        #[test]
        fn clone_blocking_request_streamed_body_should_drop_body() {
            let mut request =
                reqwest::blocking::Request::new(Method::POST, Url::parse("http://h").unwrap());
            request
                .headers_mut()
                .insert("x-key", "value".parse().unwrap());
            *request.timeout_mut() = Some(Duration::from_secs(1));
            *request.body_mut() = Some(reqwest::blocking::Body::new(std::io::Cursor::new(b"data")));

            let clone = clone_blocking_request(&request);

            assert_eq!(clone.method(), Method::POST);
            assert_eq!(clone.url(), request.url());
            assert_eq!(clone.headers(), request.headers());
            assert_eq!(clone.timeout(), Some(&Duration::from_secs(1)));
            assert!(clone.body().is_none());
        }
    }

    #[cfg(feature = "tokio")]
    mod clone_request {
        use super::*;

        #[test]
        fn clone_request_streamed_body_should_drop_body() {
            let mut request = reqwest::Request::new(Method::POST, Url::parse("http://h").unwrap());
            request
                .headers_mut()
                .insert("x-key", "value".parse().unwrap());
            *request.timeout_mut() = Some(Duration::from_secs(1));
            *request.body_mut() = Some(streamed(b"data"));

            let clone = clone_request(&request);

            assert_eq!(clone.method(), Method::POST);
            assert_eq!(clone.url(), request.url());
            assert_eq!(clone.headers(), request.headers());
            assert_eq!(clone.timeout(), Some(&Duration::from_secs(1)));
            assert!(clone.body().is_none());
        }
    }

    #[cfg(feature = "tokio")]
    mod async_run {
        use super::*;

        #[test]
        fn async_run_should_expose_request() {
            let url = Url::parse("https://example.com/path").unwrap();
            let mut request = reqwest::Request::new(Method::PUT, url.clone());
            *request.body_mut() = Some("body".into());
            *request.timeout_mut() = Some(Duration::from_secs(5));
            let options = RequestOptions::default();

            let run = async_run(&request, &options);

            assert_eq!(run.attempt(), 1);
            assert_eq!(run.method(), Method::PUT);
            assert_eq!(run.url(), &url);
            assert_eq!(run.body(), Some(b"body".as_slice()));
            assert_eq!(run.timeout(), Some(Duration::from_secs(5)));
        }
    }
}
