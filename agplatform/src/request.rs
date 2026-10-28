use std::any::Any;
use std::any::TypeId;
use std::borrow::Cow;
use std::collections::BTreeMap;
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

use crate::DryRunOutput;
use crate::Result;
use crate::env::SharedEnv;
#[cfg(feature = "tokio")]
use crate::utils::BoxFuture;
use crate::utils::Extensions;
use crate::utils::MAX_DELAY;
use crate::utils::SharedCell;
use crate::utils::backoff;

macro_rules! request_error {
    ($($arg:tt)*) => {
        $crate::Error::request(format!($($arg)*))
    };
}

/// Enabled by the `request` feature flag.
///
/// Intercepts the requests sent by a [`Request`] like a middleware, e.g. to
/// change, retry, replace or log them.
///
/// The interceptors of a run form a chain around sending its request, the
/// first one being the outermost. [`handle`](Self::handle) of each one gets
/// the run in a [`RequestNext`], which it may change, e.g. add a header,
/// before passing it on to the rest of the chain with [`RequestNext::next`]
/// any number of times, e.g. to retry it, or not at all, and whose result
/// it may change, e.g. accept a `404` by setting
/// [`RequestOutput::success`]. The innermost one is followed by sending the
/// request. Each chunk of the response body passes through
/// [`output`](Self::output) of the interceptors as it arrives, from the
/// innermost one outwards, before it is passed to the sink of
/// [`Request::send_blocking_to`].
///
/// A request whose output is set on its run with
/// [`RequestRun::set_output`], e.g. by a dry run or a cache, is not sent:
/// its output is returned as if it was received, see
/// [`RequestRun::intercepted`]. That is how to short-circuit a run. An
/// output an interceptor returns instead of calling [`RequestNext::next`]
/// is taken as it is, e.g. the [default](RequestOutput::default) one is not
/// a success, and its body reaches neither the sink nor
/// [`output`](Self::output). Inside the chain an output whose `success` is
/// `false` is returned as `Ok`, the methods of [`Request`] return it as an
/// error. A request that cannot be built (e.g. with an invalid URL) or
/// whose body cannot be read has no run and is passed to
/// [`invalid`](Self::invalid) only.
///
/// Each attempt, e.g. of [`RequestRetry`], passes the run through the rest
/// of the chain again with the changes made to it before, so an interceptor
/// should replace what it sets, e.g. [`HeaderMap::insert`] a header rather
/// than [`append`](HeaderMap::append) it. The chunks of every attempt pass
/// through [`output`](Self::output), see [`RequestRun::attempt`].
///
/// Async runs (with the `tokio` feature) call
/// [`handle_async`](Self::handle_async) instead of [`handle`](Self::handle)
/// and [`cancelled`](Self::cancelled) when they are cancelled. Every method
/// passes the run, or the chunk, on to the rest of the chain by default.
///
/// The interceptors of a [`Request`] are none by default, see
/// [`Request::add_interceptor`], e.g. on
/// [`Platform::request`](crate::Platform::request). A run uses them unless
/// it has its own, see [`RequestOptions::interceptors`]. Interceptors are
/// told apart by their type, e.g. by [`RequestRun::intercepted_by`].
///
/// The headers added are seen by all interceptors, mark the values of
/// secrets [sensitive](reqwest::header::HeaderValue::set_sensitive) so
/// that their [`Debug`] hides them.
///
/// # Examples
///
/// ```rust
/// use agplatform::{BlockingRequestBuilderExt, Request, RequestInterceptor, RequestNext, RequestOutput, Result};
/// use std::sync::{Arc, Mutex};
///
/// #[derive(Clone, Default)]
/// struct Failures(Arc<Mutex<Vec<String>>>);
///
/// impl RequestInterceptor for Failures {
///     fn handle(&self, mut next: RequestNext) -> Result<RequestOutput> {
///         let result = next.next();
///
///         if let Ok(output) = &result
///             && !output.success
///         {
///             self.0.lock().unwrap().push(format!("{} {}", next.run().target(), output.status));
///         }
///
///         result
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
#[allow(unused_variables)]
pub trait RequestInterceptor: Any + Send + Sync {
    /// Handles the run of `next`, returning its result. Runs the rest of
    /// the chain by default.
    fn handle(&self, mut next: RequestNext) -> Result<RequestOutput> {
        next.next()
    }

    /// Enabled by the `tokio` feature flag.
    ///
    /// Handles the async run of `next` like [`Self::handle`]. Runs the rest
    /// of the chain by default, it does not call [`Self::handle`].
    #[cfg(feature = "tokio")]
    fn handle_async<'a>(
        &'a self,
        mut next: AsyncRequestNext<'a, '_>,
    ) -> BoxFuture<'a, Result<RequestOutput>> {
        Box::pin(async move { next.next().await })
    }

    /// Handles a chunk of the response body of the run as it arrives, as
    /// received rather than split into lines. Passes it on by default.
    ///
    /// # Errors
    ///
    /// An error ends the attempt.
    fn output(&self, chunk: RequestChunk) -> Result {
        chunk.next()
    }

    /// Enabled by the `tokio` feature flag.
    ///
    /// Called when an async run is cancelled by dropping its future while
    /// its request is sent or while an interceptor waits with
    /// [`AsyncRequestNext::sleep`], from the innermost interceptor
    /// outwards, see [`ProcessInterceptor::cancelled`](crate::ProcessInterceptor::cancelled).
    #[cfg(feature = "tokio")]
    fn cancelled(&self, run: &RequestRun) {}

    /// Called with the error of a request that cannot be built, e.g. with
    /// an invalid URL or header, or whose body cannot be read, so that
    /// there is no run.
    fn invalid(&self, error: &crate::Error) {}
}

/// Enabled by the `request` feature flag.
///
/// The rest of the chain of interceptors of a run, passed to
/// [`RequestInterceptor::handle`] with the run.
pub struct RequestNext<'a, 'r> {
    run: &'a mut RequestRun<'r>,
    layers: &'a [Arc<dyn RequestInterceptor>],
    inner: &'a mut dyn Execute,
}

/// Enabled by the `request` and `tokio` feature flags.
///
/// The rest of the chain of interceptors of an async run, passed to
/// [`RequestInterceptor::handle_async`] with the run.
#[cfg(feature = "tokio")]
pub struct AsyncRequestNext<'a, 'r> {
    run: &'a mut RequestRun<'r>,
    layers: &'a [Arc<dyn RequestInterceptor>],
    inner: &'a mut dyn ExecuteAsync,
}

/// Enabled by the `request` feature flag.
///
/// A chunk of the response body of a run passing through the interceptors,
/// see [`RequestInterceptor::output`].
pub struct RequestChunk<'a> {
    run: &'a RequestRun<'a>,
    data: Cow<'a, [u8]>,
    layers: &'a [Arc<dyn RequestInterceptor>],
    sink: Option<&'a mut Sink<'a>>,
}

/// Enabled by the `request` feature flag.
///
/// The interceptors of a [`Request`] or of a run, see
/// [`RequestOptions::interceptors`], the first one being the outermost.
///
/// Two are equal when they hold the same instances in the same order, e.g.
/// one is a clone of the other.
#[derive(Clone, Default)]
pub struct RequestInterceptors(Vec<Arc<dyn RequestInterceptor>>);

/// Enabled by the `request` feature flag.
///
/// The [`RequestInterceptor`] retrying the failed attempts of the runs, up
/// to `retries` times after a delay (see [`RequestNext::sleep`]) while
/// they fail in a way [safe to retry](RequestRun::safe_to_retry), unless
/// [`when`](Self::when) decides otherwise. Each attempt is limited by the
/// timeouts of the request and its client on its own. Not after a sink
/// accepted a part of the body, see [`Request::send_blocking_to`], nor when
/// the output is set by an interceptor, see [`RequestRun::intercepted`],
/// which would return it again.
///
/// The delay is multiplied by the [`backoff`](Self::backoff) after each
/// retry and limited by the [`max_delay`](Self::max_delay). A failed
/// response with a `Retry-After` header (as seconds or an HTTP date), e.g.
/// 429 or 503, is retried no sooner than it asks, or not at all if it asks
/// to wait longer than the `max_delay`.
///
/// It runs again all interceptors after it in the chain, so it is usually
/// the outermost one.
///
/// # Examples
///
/// ```rust
/// use agplatform::{BlockingRequestBuilderExt, Request, RequestOutput, RequestRetry, TestRequest};
/// use std::time::Duration;
///
/// let request = TestRequest::new();
/// request.add_interceptor(
///     RequestRetry::new(3, Duration::from_millis(100))
///         .backoff(2)
///         .when(|run, result| {
///             run.safe_to_retry(result) || result.as_ref().is_ok_and(|output| output.status == 412)
///         }),
/// );
/// request.set_responses("", [RequestOutput { status: 412, ..Default::default() }, RequestOutput { status: 200, ..Default::default() }]);
///
/// let client = request.blocking_client().unwrap();
/// client.put("https://example.com/item").body("data").send_blocking(&request).unwrap();
///
/// assert_eq!(request.calls().len(), 2);
/// ```
#[derive(Clone)]
pub struct RequestRetry {
    retries: u32,
    delay: Duration,
    backoff: u32,
    max_delay: Duration,
    condition: Option<Arc<RequestRetryCondition>>,
}

type RequestRetryCondition = dyn Fn(&RequestRun, &Result<RequestOutput>) -> bool + Send + Sync;

/// Enabled by the `request` feature flag.
///
/// The [`RequestInterceptor`] of a dry run, setting the output of the runs
/// (see [`RequestRun::set_output`]) instead of sending their requests: the
/// [`DryRunOutput`] set on the run (see [`ConfiguredRequest::with`]), or an
/// empty `200`, unless an interceptor before it set the output already.
///
/// # Examples
///
/// ```rust
/// use agplatform::{BlockingRequestBuilderExt, DryRunOutput, Request, RequestDryRun, RequestOutput, TestRequest};
///
/// let request = TestRequest::new();
/// request.add_interceptor(RequestDryRun);
/// let client = request.blocking_client().unwrap();
///
/// let deleted = client.delete("https://example.com/item").send_blocking(&request).unwrap();
/// let item = client
///     .get("https://example.com/item")
///     .with(DryRunOutput(RequestOutput { status: 200, body: b"item".to_vec(), ..Default::default() }))
///     .send_blocking(&request)
///     .unwrap();
///
/// assert_eq!(deleted.status, 200);
/// assert_eq!(item.body, b"item");
/// assert!(request.calls().is_empty());
/// ```
#[derive(Clone, Copy, Debug, Default)]
pub struct RequestDryRun;

/// Enabled by the `request` feature flag.
///
/// The [`RequestInterceptor`] caching the successful responses of the `GET`
/// and `HEAD` runs in memory by their [`key`](Self::key), returning them
/// for the runs of the same requests instead of sending them again. The
/// outputs set by other interceptors (see [`RequestRun::intercepted`]) and
/// those passed to a sink are not cached. The key has no headers, so a
/// response is returned whatever the credentials of the request, use a
/// cache per credentials when the responses depend on them.
///
/// Clones share the cache, e.g. to persist its [`entries`](Self::entries)
/// and [`load`](Self::load) them later.
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
#[derive(Clone, Default)]
pub struct RequestCache(Arc<Mutex<BTreeMap<String, RequestOutput>>>);

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
    /// so that it can be passed to the interceptors and retried. The whole
    /// response body is read too. Each run passes through interceptors,
    /// see [`RequestInterceptor`].
    ///
    /// # Errors
    ///
    /// The errors name only the method and the URL without its
    /// credentials, query and fragment, as they may hold secrets. A
    /// [`RequestInterceptor`] gets the whole request, e.g. to log it
    /// redacted.
    ///
    /// Returns an error if:
    /// - The request cannot be built, e.g. its URL is invalid, or its body
    ///   cannot be read.
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
    /// let options = RequestOptions { stream: true, ..Default::default() };
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
    /// [`RequestInterceptor::output`]. An attempt failing after `sink`
    /// accepted a part of the body is not retried by [`RequestRetry`], as
    /// `sink` would get the body again from its start, see
    /// [`RequestRun::sunk`]. A caller can resume it instead, e.g. with
    /// a `Range` header and the response head from
    /// [`TransportError::output`].
    ///
    /// # Errors
    ///
    /// As [`Self::send_blocking`], or the error returned by `sink`, which
    /// ends the attempt.
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
    /// interceptors are told by [`RequestInterceptor::cancelled`].
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
    /// on a run, see [`RequestOptions::interceptors`].
    fn interceptors(&self) -> RequestInterceptors;

    /// Removes the interceptors, none by default, for the runs starting
    /// afterwards.
    fn clear_interceptors(&self);

    /// Adds `interceptor` as the innermost one for the runs starting
    /// afterwards. Its state to be read later, e.g. what it recorded, is
    /// shared through an [`Arc`] it holds.
    ///
    /// See [`RequestInterceptor`].
    fn add_interceptor(&self, interceptor: impl RequestInterceptor);
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
    /// See [`RequestOptions::interceptors`].
    fn interceptors(self, interceptors: RequestInterceptors) -> ConfiguredRequest<Self>;

    /// Returns a [`ConfiguredRequest`] streaming its response body.
    ///
    /// See [`RequestOptions::stream`].
    fn stream(self) -> ConfiguredRequest<Self>;

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

    /// Returns a [`ConfiguredRequest`] whose response shows its last
    /// `lines` when displayed.
    ///
    /// See [`RequestOptions::display_lines`].
    fn display_lines(self, lines: usize) -> ConfiguredRequest<Self>;

    /// Returns a [`ConfiguredRequest`] with its own `interceptors`.
    ///
    /// See [`RequestOptions::interceptors`].
    fn interceptors(self, interceptors: RequestInterceptors) -> ConfiguredRequest<Self>;

    /// Returns a [`ConfiguredRequest`] streaming its response body.
    ///
    /// See [`RequestOptions::stream`].
    fn stream(self) -> ConfiguredRequest<Self>;

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
    pub(crate) builder: B,
    pub(crate) options: RequestOptions,
    pub(crate) extensions: Extensions,
}

/// Enabled by the `request` feature flag.
///
/// A request being sent by a [`Request`], passed to the interceptors, which
/// may change it, see [`RequestInterceptor`]. Its parts are borrowed from
/// the request until changed.
pub struct RequestRun<'a> {
    method: Cow<'a, Method>,
    url: Cow<'a, Url>,
    headers: Cow<'a, HeaderMap>,
    body: Option<Cow<'a, [u8]>>,
    timeout: Option<Duration>,
    options: Cow<'a, RequestOptions>,
    attempt: u32,
    id: u64,
    sink: bool,
    sunk: AtomicBool,
    output: Option<RequestOutput>,
    output_by: Option<TypeId>,
    /// The interceptor being called.
    layer: Option<TypeId>,
    extensions: Extensions,
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

    /// Marks the run as streamed for the interceptors, e.g. to show its
    /// response body as it arrives, see [`RequestInterceptor::output`].
    /// The body of a response that fails to be received is kept in its
    /// [`TransportError::output`] then.
    pub stream: bool,

    /// The interceptors of the run instead of those of the request, see
    /// [`Request::interceptors`]. Those of the request when `None`.
    pub interceptors: Option<RequestInterceptors>,
}

/// [internal]
///
/// Receives the body of a successful response, see
/// [`Request::send_blocking_to`].
pub(crate) type Sink<'a> = dyn FnMut(&[u8]) -> Result + Send + 'a;

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
/// displayed.
///
/// Its [`Debug`] shows only the length of the body and its start, as the
/// body may be large, e.g. in a `{:?}` of an error.
#[derive(Clone)]
pub struct RequestOutput {
    /// Whether the request succeeded. Set by the request for each attempt
    /// to whether the status is a success (2xx), then may be changed by the
    /// interceptors, e.g. to accept the status `404` of a lookup of
    /// something that may not exist.
    pub success: bool,

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

/// [internal]
///
/// The variables of the platform environment that reqwest reads, see
/// [`REQWEST_ENV_VARS`]. Their names ignore the ASCII case on Windows like
/// its environment, e.g. `https_proxy` is `HTTPS_PROXY` there.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReqwestEnv(Vec<(String, String)>);

/// [internal]
///
/// The response body of an attempt as it arrives: passed through the
/// interceptors with `emit` and captured unless it goes to a sink, which
/// only gets the body of a successful response. The first error of `emit`
/// ends the attempt.
pub(crate) struct Body<'a> {
    pub(crate) data: Vec<u8>,
    has_sink: bool,
    to_sink: bool,
    emit: &'a mut (dyn FnMut(&[u8], bool) -> Result + Send + 'a),
    error: Option<crate::Error>,
}

/// [internal]
///
/// The innermost part of the chain of a run, sending its attempts.
pub(crate) trait Execute {
    /// Sends the attempt of `run`.
    fn execute(&mut self, run: &mut RequestRun) -> Result<RequestOutput>;

    /// Waits `delay`, see [`RequestNext::sleep`].
    fn sleep(&self, delay: Duration);
}

/// [internal]
///
/// The innermost part of the chain of an async run, sending its attempts.
#[cfg(feature = "tokio")]
pub(crate) trait ExecuteAsync: Send {
    /// Sends the attempt of `run`.
    fn execute<'a>(
        &'a mut self,
        run: &'a mut RequestRun<'_>,
    ) -> BoxFuture<'a, Result<RequestOutput>>;

    /// Waits `delay`, see [`AsyncRequestNext::sleep`].
    fn delay(&self, delay: Duration) -> BoxFuture<'static, ()>;

    /// Returns the interceptors of the run.
    fn layers(&self) -> &[Arc<dyn RequestInterceptor>];
}

/// [internal]
///
/// Sends the attempts of the requests of a [`Request`] implementation.
pub(crate) trait Attempt {
    /// Sends the request of `run`, passing its response body to `body`.
    fn attempt(&self, run: &RequestRun, body: Body) -> Result<RequestOutput>;

    /// Waits `delay` between attempts.
    fn sleep(&self, delay: Duration);
}

/// [internal]
///
/// Sends the attempts of the async requests of a [`Request`]
/// implementation.
#[cfg(feature = "tokio")]
pub(crate) trait AttemptAsync: Sync {
    /// Sends the request of `run`, passing its response body to `body`.
    fn attempt_async<'a>(
        &'a self,
        run: &'a RequestRun<'_>,
        body: Body<'a>,
    ) -> BoxFuture<'a, Result<RequestOutput>>;

    /// Waits `delay` between async attempts.
    fn delay(&self, delay: Duration) -> BoxFuture<'static, ()>;
}

/// [internal]
///
/// The innermost part of the chain of a run by `attempts` of the run with
/// the interceptors `layers`, passing its body to `sink`, if any.
pub(crate) struct Executor<'a, 's, A: ?Sized> {
    pub(crate) attempts: &'a A,
    pub(crate) layers: &'a [Arc<dyn RequestInterceptor>],
    pub(crate) sink: Option<&'a mut Sink<'s>>,
}

/// [internal]
///
/// Tells the interceptors of a run that it is cancelled when dropped before
/// it is disarmed.
#[cfg(feature = "tokio")]
struct Cancel<'a> {
    run: &'a RequestRun<'a>,
    layers: &'a [Arc<dyn RequestInterceptor>],
    armed: bool,
}

/// [internal]
///
/// The attempts of a blocking request sent by `client`.
struct Blocking<'a> {
    client: &'a reqwest::blocking::Client,
    request: &'a reqwest::blocking::Request,
}

/// [internal]
///
/// The attempts of an async request sent by `client`.
#[cfg(feature = "tokio")]
struct Async<'a> {
    client: &'a reqwest::Client,
    request: &'a reqwest::Request,
}

/// The production [`Request`] implementation backed by [`reqwest`].
///
/// Normally obtained via [`platform()`](crate::platform) rather than
/// constructed directly.
pub struct RequestImpl {
    clients: DefaultClients,
    interceptors: SharedCell<RequestInterceptors>,
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

    fn interceptors(self, interceptors: RequestInterceptors) -> ConfiguredRequest<Self> {
        ConfiguredRequest::new(self).interceptors(interceptors)
    }

    fn stream(self) -> ConfiguredRequest<Self> {
        ConfiguredRequest::new(self).stream()
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

    fn interceptors(self, interceptors: RequestInterceptors) -> ConfiguredRequest<Self> {
        ConfiguredRequest::new(self).interceptors(interceptors)
    }

    fn stream(self) -> ConfiguredRequest<Self> {
        ConfiguredRequest::new(self).stream()
    }

    fn with<T: Any + Send + Sync>(self, value: T) -> ConfiguredRequest<Self> {
        ConfiguredRequest::new(self).with(value)
    }
}

impl<'r> RequestNext<'_, 'r> {
    /// Returns the run.
    pub fn run(&self) -> &RequestRun<'r> {
        self.run
    }

    /// Returns the run to be changed, e.g. before [`Self::next`].
    pub fn run_mut(&mut self) -> &mut RequestRun<'r> {
        self.run
    }

    /// Runs the rest of the chain, the next interceptor or the request, and
    /// returns its result. May be called again, e.g. to retry the run.
    ///
    /// # Errors
    ///
    /// Returns the error of the rest of the chain, e.g. when the request
    /// cannot be sent. A response that is not a
    /// [success](RequestOutput::success) is returned as `Ok`.
    // WHY: named after the next part of the chain like in middlewares, it
    // is not an iterator.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<RequestOutput> {
        match self.layers.split_first() {
            Some((layer, layers)) => {
                let outer = self.run.layer.replace(type_of(layer));
                let result = layer.handle(RequestNext {
                    run: &mut *self.run,
                    layers,
                    inner: &mut *self.inner,
                });
                self.run.layer = outer;
                result
            }
            None => self.inner.execute(self.run),
        }
    }

    /// Waits `delay`, e.g. before a retry, unless the request does not
    /// wait, like [`TestRequest`](crate::TestRequest).
    pub fn sleep(&self, delay: Duration) {
        self.inner.sleep(delay);
    }
}

#[cfg(feature = "tokio")]
impl<'r> AsyncRequestNext<'_, 'r> {
    /// Returns the run.
    pub fn run(&self) -> &RequestRun<'r> {
        self.run
    }

    /// Returns the run to be changed, e.g. before [`Self::next`].
    pub fn run_mut(&mut self) -> &mut RequestRun<'r> {
        self.run
    }

    /// Runs the rest of the chain like [`RequestNext::next`]
    /// asynchronously.
    ///
    /// # Errors
    ///
    /// As [`RequestNext::next`].
    // WHY: named after the next part of the chain like in middlewares, it
    // is not an iterator.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> BoxFuture<'_, Result<RequestOutput>> {
        match self.layers.split_first() {
            Some((layer, layers)) => Box::pin(async move {
                let outer = self.run.layer.replace(type_of(layer));
                let result = layer
                    .handle_async(AsyncRequestNext {
                        run: &mut *self.run,
                        layers,
                        inner: &mut *self.inner,
                    })
                    .await;
                self.run.layer = outer;
                result
            }),
            None => self.inner.execute(self.run),
        }
    }

    /// Waits `delay` like [`RequestNext::sleep`] asynchronously. Dropping
    /// the future while it waits cancels the run, see
    /// [`RequestInterceptor::cancelled`].
    pub fn sleep(&self, delay: Duration) -> BoxFuture<'_, ()> {
        let (delay, run, layers) = (self.inner.delay(delay), &*self.run, self.inner.layers());

        Box::pin(async move {
            let mut cancel = Cancel::new(run, layers);
            delay.await;
            cancel.disarm();
        })
    }
}

impl RequestChunk<'_> {
    /// Returns the run.
    pub fn run(&self) -> &RequestRun<'_> {
        self.run
    }

    /// Returns the data of the chunk.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Replaces the data of the chunk passed on, e.g. to redact a secret.
    /// The body captured in the [`RequestOutput`] is not changed.
    pub fn set_data(&mut self, data: Vec<u8>) {
        self.data = Cow::Owned(data);
    }

    /// Passes the chunk on to the next interceptor outwards, or the sink of
    /// [`Request::send_blocking_to`] when the response is a success. A
    /// chunk not passed on is dropped.
    ///
    /// # Errors
    ///
    /// Returns the error of the next interceptor or the sink.
    pub fn next(self) -> Result {
        match self.layers.split_last() {
            Some((layer, layers)) => layer.output(RequestChunk { layers, ..self }),
            None => match self.sink {
                Some(sink) => {
                    sink(&self.data)?;
                    self.run.sunk.store(true, Ordering::Relaxed);
                    Ok(())
                }
                None => Ok(()),
            },
        }
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
    /// let options = RequestOptions { stream: true, ..Default::default() };
    /// let builder = agplatform::reqwest::blocking::Client::new().get("https://example.com");
    /// let request = ConfiguredRequest::with_options(builder, options);
    ///
    /// assert!(request.options().stream);
    /// ```
    pub fn with_options(builder: B, options: RequestOptions) -> Self {
        Self {
            builder,
            options,
            extensions: Extensions::default(),
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

    /// Sends the request with its own `interceptors`.
    ///
    /// See [`RequestOptions::interceptors`].
    pub fn interceptors(mut self, interceptors: RequestInterceptors) -> Self {
        self.options.interceptors = Some(interceptors);
        self
    }

    /// Streams the response body.
    ///
    /// See [`RequestOptions::stream`].
    pub fn stream(mut self) -> Self {
        self.options.stream = true;
        self
    }

    /// Sets `value` of type `T` on the run for the interceptors, replacing
    /// the previous one, e.g. a [`DryRunOutput`] for [`RequestDryRun`].
    /// Read with [`RequestRun::get`].
    pub fn with<T: Any + Send + Sync>(mut self, value: T) -> Self {
        self.extensions.insert(value);
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
    /// Returns the number of the current or last attempt of the run, i.e.
    /// of the times its request was sent or its output set returned so far,
    /// `1` from the start of the first one and `0` before.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Returns the request body, `None` when it has none.
    pub fn body(&self) -> Option<&[u8]> {
        self.body.as_deref()
    }

    /// Returns `true` once the sink of the run accepted a part of its
    /// response body, see [`Request::send_blocking_to`], so that it cannot
    /// be retried.
    pub fn sunk(&self) -> bool {
        self.sunk.load(Ordering::Relaxed)
    }

    /// Returns the value of type `T` set on the run, see
    /// [`ConfiguredRequest::with`].
    pub fn get<T: Any>(&self) -> Option<&T> {
        self.extensions.get()
    }

    /// Returns `true` if the body of a successful response goes to a sink,
    /// see [`Request::send_blocking_to`], so that it is not captured.
    pub fn has_sink(&self) -> bool {
        self.sink
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
    /// use agplatform::{RequestInterceptor, RequestNext, RequestOutput, Result};
    /// use agplatform::reqwest::header::{AUTHORIZATION, HeaderValue};
    ///
    /// struct Token(String);
    ///
    /// impl RequestInterceptor for Token {
    ///     fn handle(&self, mut next: RequestNext) -> Result<RequestOutput> {
    ///         if next.run().url().host_str() == Some("api.example.com")
    ///             && let Ok(mut value) = HeaderValue::from_str(&format!("Bearer {}", self.0))
    ///         {
    ///             value.set_sensitive(true);
    ///             next.run_mut().headers_mut().insert(AUTHORIZATION, value);
    ///         }
    ///
    ///         next.next()
    ///     }
    /// }
    /// ```
    pub fn headers_mut(&mut self) -> &mut HeaderMap {
        self.headers.to_mut()
    }

    /// Returns the id of the run, unique within the process among the runs
    /// of commands and requests and the same for all of its attempts, e.g.
    /// to tell concurrent runs apart in an interceptor.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Sets `value` of type `T` on the run, replacing the previous one, e.g.
    /// for the next interceptors, see [`ConfiguredRequest::with`].
    pub fn insert<T: Any + Send + Sync>(&mut self, value: T) {
        self.extensions.insert(value);
    }

    /// Returns `true` if the output of the run was set, see
    /// [`Self::set_output`], so that its request is not sent, e.g. so that
    /// a cache does not store it.
    pub fn intercepted(&self) -> bool {
        self.output.is_some()
    }

    /// Returns `true` if the output of the run was set by an interceptor of
    /// type `T`, see [`Self::set_output`] and
    /// [`CommandRun::intercepted_by`](crate::CommandRun::intercepted_by) for
    /// an example.
    pub fn intercepted_by<T: RequestInterceptor>(&self) -> bool {
        self.intercepted() && self.output_by == Some(TypeId::of::<T>())
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
        self.options.to_mut()
    }

    /// Returns the output set on the run, see [`Self::set_output`].
    pub fn output(&self) -> Option<&RequestOutput> {
        self.output.as_ref()
    }

    /// Returns `true` if the failed `result` of the attempt of the run may
    /// pass when retried without the risk of processing the request twice:
    /// of an idempotent method (`GET`, `HEAD`, `PUT`, `DELETE`, `OPTIONS`
    /// and `TRACE`, see RFC 9110) a [`TransportError`] or the status `408`
    /// (Request Timeout), `429` (Too Many Requests) or `5xx`, and of the
    /// others (e.g. `POST` and `PATCH`) only a
    /// [`TransportErrorKind::Connect`] error or the status `408`, `429` or
    /// `503` (Service Unavailable). Used by [`RequestRetry`] by default.
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
        self.body = body.map(Cow::Owned);
    }

    /// Sets the method, e.g. in an interceptor.
    pub fn set_method(&mut self, method: Method) {
        self.method = Cow::Owned(method);
    }

    /// Sets the `output` of the run, returned for its attempts instead of
    /// sending its request as if it was received, e.g. passed to the
    /// interceptors and the sink. Its [`RequestOutput::success`] is set from
    /// its status like for a response. Set by the interceptor calling it,
    /// see [`Self::intercepted_by`].
    pub fn set_output(&mut self, output: RequestOutput) {
        self.output = Some(output);
        self.output_by = self.layer;
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
        let mut url = Url::clone(&self.url);
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
        self.url.to_mut()
    }

    /// [internal]
    ///
    /// Returns the run of the request of `method`, `url`, `headers`, `body`
    /// and `timeout` with `options` and `extensions`, passing its body to a
    /// sink if `sink`.
    pub(crate) fn new(
        (method, url, headers): (&'a Method, &'a Url, &'a HeaderMap),
        body: Option<&'a [u8]>,
        timeout: Option<Duration>,
        options: &'a RequestOptions,
        extensions: Extensions,
        sink: bool,
    ) -> Self {
        Self {
            method: Cow::Borrowed(method),
            url: Cow::Borrowed(url),
            headers: Cow::Borrowed(headers),
            body: body.map(Cow::Borrowed),
            timeout,
            options: Cow::Borrowed(options),
            attempt: 0,
            id: crate::utils::next_run_id(),
            sink,
            sunk: AtomicBool::new(false),
            output: None,
            output_by: None,
            layer: None,
            extensions,
        }
    }
}

impl Default for RequestOptions {
    fn default() -> Self {
        Self {
            display_lines: DISPLAY_LINES,
            stream: false,
            interceptors: None,
        }
    }
}

impl RequestInterceptors {
    /// Creates an empty list of interceptors.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts `interceptor` at `index`, the outermost at `0`.
    ///
    /// # Panics
    ///
    /// Panics if `index` is greater than [`Self::len`].
    pub fn insert(&mut self, index: usize, interceptor: impl RequestInterceptor) {
        self.0.insert(index, Arc::new(interceptor));
    }

    /// Returns `true` if there are no interceptors.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the number of the interceptors.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Adds `interceptor` as the innermost one.
    pub fn push(&mut self, interceptor: impl RequestInterceptor) {
        self.0.push(Arc::new(interceptor));
    }
}

impl std::fmt::Debug for RequestInterceptors {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "RequestInterceptors({})", self.0.len())
    }
}

impl PartialEq for RequestInterceptors {
    fn eq(&self, other: &Self) -> bool {
        crate::utils::same_interceptors(&self.0, &other.0)
    }
}

impl Eq for RequestInterceptors {}

impl RequestRetry {
    /// Creates the interceptor retrying the failed attempts up to `retries`
    /// times after `delay`.
    pub fn new(retries: u32, delay: Duration) -> Self {
        Self {
            retries,
            delay,
            backoff: 1,
            max_delay: MAX_DELAY,
            condition: None,
        }
    }

    /// Multiplies the delay by `factor` after each retry. `1` (the default)
    /// keeps the delay fixed, `2` doubles it.
    pub fn backoff(mut self, factor: u32) -> Self {
        self.backoff = factor;
        self
    }

    /// Waits at most `max` before a retry, limiting the backoff, and gives
    /// up when `Retry-After` asks for longer. 60 s by default.
    pub fn max_delay(mut self, max: Duration) -> Self {
        self.max_delay = max;
        self
    }

    /// Retries only the failed attempts for which `condition` returns
    /// `true` instead of those [safe to retry](RequestRun::safe_to_retry).
    pub fn when<F>(mut self, condition: F) -> Self
    where
        F: Fn(&RequestRun, &Result<RequestOutput>) -> bool + Send + Sync + 'static,
    {
        self.condition = Some(Arc::new(condition));
        self
    }

    /// Returns the delay before retrying the attempt of `run` with `result`
    /// after `retried` retries, `None` if it is not retried.
    fn retry(
        &self,
        run: &RequestRun,
        result: &Result<RequestOutput>,
        retried: u32,
    ) -> Option<Duration> {
        let retried_ok = retried < self.retries
            && !result.as_ref().is_ok_and(|output| output.success)
            && !run.sunk()
            && !run.intercepted()
            && self.condition.as_ref().map_or_else(
                || run.safe_to_retry(result),
                |condition| condition(run, result),
            );
        let delay = backoff(self.delay, self.backoff, retried, self.max_delay);

        match result.as_ref().ok().and_then(retry_after) {
            _ if !retried_ok => None,
            Some(after) if after > self.max_delay => None,
            Some(after) => Some(delay.max(after)),
            None => Some(delay),
        }
    }
}

impl std::fmt::Debug for RequestRetry {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("RequestRetry")
            .field("retries", &self.retries)
            .field("delay", &self.delay)
            .field("backoff", &self.backoff)
            .field("max_delay", &self.max_delay)
            .field("condition", &self.condition.is_some())
            .finish()
    }
}

impl RequestCache {
    /// Returns the cached outputs by their keys, e.g. to persist them.
    pub fn entries(&self) -> Vec<(String, RequestOutput)> {
        self.outputs()
            .iter()
            .map(|(key, output)| (key.clone(), output.clone()))
            .collect()
    }

    /// Returns the key of `run`: the method, the whole URL and the body
    /// escaped, but not the headers, as they may hold secrets that change.
    pub fn key(run: &RequestRun) -> String {
        let mut key = format!("{} {}", run.method(), run.url());

        if let Some(body) = run.body() {
            key.push_str(&format!("\n{}", body.escape_ascii()));
        }

        key
    }

    /// Returns `true` if the response of `run` may be cached.
    fn cacheable(run: &RequestRun) -> bool {
        matches!(*run.method(), Method::GET | Method::HEAD)
    }

    /// Adds the `entries`, e.g. persisted from [`Self::entries`], replacing
    /// those with the same keys.
    pub fn load(&self, entries: impl IntoIterator<Item = (String, RequestOutput)>) {
        self.outputs().extend(entries);
    }

    fn outputs(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, RequestOutput>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sets the cached output of `run`, if any, unless its output is set or
    /// its method is not cached.
    fn hit(&self, run: &mut RequestRun) -> String {
        let key = Self::key(run);

        if !run.intercepted()
            && Self::cacheable(run)
            && let Some(output) = self.outputs().get(&key).cloned()
        {
            run.set_output(output);
        }

        key
    }

    /// Caches the successful output of `run` from `result` by `key`.
    fn store(&self, run: &RequestRun, key: String, result: &Result<RequestOutput>) {
        if let Ok(output) = result
            && output.success
            && !run.intercepted()
            && !run.has_sink()
            && Self::cacheable(run)
        {
            self.outputs().insert(key, output.clone());
        }
    }
}

impl std::fmt::Debug for RequestCache {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "RequestCache({})", self.outputs().len())
    }
}

/// Returns the type of the `interceptor` behind its [`Arc`].
///
/// WHY: the [`Any::type_id`] of the [`Arc`] itself would be its own.
fn type_of(interceptor: &Arc<dyn RequestInterceptor>) -> TypeId {
    let interceptor: &dyn RequestInterceptor = interceptor.as_ref();
    Any::type_id(interceptor)
}

impl RequestOutput {
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

impl<'a> Body<'a> {
    /// Creates the body passing the chunks to `emit`, with whether they go
    /// to the sink, if `has_sink`.
    pub(crate) fn new(
        has_sink: bool,
        emit: &'a mut (dyn FnMut(&[u8], bool) -> Result + Send + 'a),
    ) -> Self {
        Self {
            data: Vec::new(),
            has_sink,
            to_sink: false,
            emit,
            error: None,
        }
    }

    /// Starts the body of a response whose status is a success if
    /// `success`, which only then goes to the sink.
    pub(crate) fn start(&mut self, success: bool) {
        self.to_sink = self.has_sink && success;
    }

    /// Takes the `data` of the body.
    ///
    /// # Errors
    ///
    /// Returns the error of passing it on, which ends the attempt.
    pub(crate) fn push(&mut self, data: &[u8]) -> Result<()> {
        (self.emit)(data, self.to_sink)?;

        if !self.to_sink {
            self.data.extend_from_slice(data);
        }

        Ok(())
    }
}

impl std::io::Write for Body<'_> {
    /// Keeps the error of passing the data on to be returned instead of the
    /// I/O error.
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        match self.push(data) {
            Ok(()) => Ok(data.len()),
            Err(error) => {
                self.error = Some(error);
                Err(std::io::Error::other("body could not be passed on"))
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<A: Attempt + ?Sized> Execute for Executor<'_, '_, A> {
    fn execute(&mut self, run: &mut RequestRun) -> Result<RequestOutput> {
        run.attempt = run.attempt.saturating_add(1);
        let run = &*run;
        let (attempts, layers, mut sink) = (self.attempts, self.layers, self.sink.as_deref_mut());
        let has_sink = sink.is_some();
        let mut emit = |data: &[u8], to_sink| emit(run, layers, &mut sink, to_sink, data);
        let body = Body::new(has_sink, &mut emit);

        match run.output.clone() {
            Some(output) => deliver(run, output, body),
            None => attempts.attempt(run, body),
        }
        .map(judge)
    }

    fn sleep(&self, delay: Duration) {
        self.attempts.sleep(delay);
    }
}

#[cfg(feature = "tokio")]
impl<A: AttemptAsync + ?Sized> ExecuteAsync for Executor<'_, '_, A> {
    fn execute<'a>(
        &'a mut self,
        run: &'a mut RequestRun<'_>,
    ) -> BoxFuture<'a, Result<RequestOutput>> {
        Box::pin(async move {
            run.attempt = run.attempt.saturating_add(1);
            let run = &*run;
            let (attempts, layers, mut sink) =
                (self.attempts, self.layers, self.sink.as_deref_mut());
            let has_sink = sink.is_some();
            let mut cancel = Cancel::new(run, layers);
            let mut emit = |data: &[u8], to_sink| emit(run, layers, &mut sink, to_sink, data);
            let body = Body::new(has_sink, &mut emit);
            let result = match run.output.clone() {
                Some(output) => deliver(run, output, body),
                None => attempts.attempt_async(run, body).await,
            };
            cancel.disarm();
            result.map(judge)
        })
    }

    fn delay(&self, delay: Duration) -> BoxFuture<'static, ()> {
        self.attempts.delay(delay)
    }

    fn layers(&self) -> &[Arc<dyn RequestInterceptor>] {
        self.layers
    }
}

#[cfg(feature = "tokio")]
impl<'a> Cancel<'a> {
    fn new(run: &'a RequestRun<'a>, layers: &'a [Arc<dyn RequestInterceptor>]) -> Self {
        Self {
            run,
            layers,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

#[cfg(feature = "tokio")]
impl Drop for Cancel<'_> {
    fn drop(&mut self) {
        if self.armed && !std::thread::panicking() {
            self.layers
                .iter()
                .rev()
                .for_each(|layer| layer.cancelled(self.run));
        }
    }
}

impl Attempt for Blocking<'_> {
    fn attempt(&self, run: &RequestRun, mut body: Body) -> Result<RequestOutput> {
        let start = Instant::now();
        let mut response = self
            .client
            .execute(clone_blocking_request(run, self.request))
            .map_err(|error| transport_failure(run, error.into()))?;
        let status = response.status();
        let headers = std::mem::take(response.headers_mut());
        body.start(status.is_success());

        // WHY: read in parts, as the client timeout limits each read, but
        // all of `bytes`.
        if let Err(error) = response.copy_to(&mut body) {
            return Err(match body.error {
                Some(error) => error,
                None => interrupted(run, start, error, status, headers, body.data),
            });
        }

        Ok(received(run, start, status, headers, body.data))
    }

    fn sleep(&self, delay: Duration) {
        std::thread::sleep(delay);
    }
}

#[cfg(feature = "tokio")]
impl AttemptAsync for Async<'_> {
    fn attempt_async<'a>(
        &'a self,
        run: &'a RequestRun<'_>,
        mut body: Body<'a>,
    ) -> BoxFuture<'a, Result<RequestOutput>> {
        Box::pin(async move {
            let start = Instant::now();
            let mut response = self
                .client
                .execute(clone_request(run, self.request))
                .await
                .map_err(|error| transport_failure(run, error.into()))?;
            let status = response.status();
            let headers = std::mem::take(response.headers_mut());
            body.start(status.is_success());

            loop {
                match response.chunk().await {
                    Ok(Some(chunk)) => body.push(&chunk)?,
                    Ok(None) => break Ok(received(run, start, status, headers, body.data)),
                    Err(error) => {
                        break Err(interrupted(run, start, error, status, headers, body.data));
                    }
                }
            }
        })
    }

    fn delay(&self, delay: Duration) -> BoxFuture<'static, ()> {
        Box::pin(tokio::time::sleep(delay))
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
            interceptors: SharedCell::new(RequestInterceptors::new()),
        }
    }

    fn send_blocking_with(
        &self,
        request: ConfiguredRequest<reqwest::blocking::RequestBuilder>,
        sink: Option<&mut Sink<'_>>,
    ) -> Result<RequestOutput> {
        let layers = layers(&self.interceptors, &request.options);
        let (client, request_built) = request.builder.build_split();
        let http = built(&layers, buffered_blocking(request_built))?;

        run_chain(
            &Blocking {
                client: &client,
                request: &http,
            },
            &layers,
            blocking_run(&http, &request.options, request.extensions, sink.is_some()),
            sink,
        )
    }

    #[cfg(feature = "tokio")]
    async fn send_async_with(
        &self,
        request: ConfiguredRequest<reqwest::RequestBuilder>,
        sink: Option<&mut Sink<'_>>,
    ) -> Result<RequestOutput> {
        let layers = layers(&self.interceptors, &request.options);
        let (client, request_built) = request.builder.build_split();
        let http = built(&layers, buffered(request_built).await)?;

        run_chain_async(
            &Async {
                client: &client,
                request: &http,
            },
            &layers,
            async_run(&http, &request.options, request.extensions, sink.is_some()),
            sink,
        )
        .await
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

    fn interceptors(&self) -> RequestInterceptors {
        self.interceptors.get()
    }

    fn clear_interceptors(&self) {
        self.interceptors
            .update(|interceptors| *interceptors = RequestInterceptors::new());
    }

    fn add_interceptor(&self, interceptor: impl RequestInterceptor) {
        self.interceptors
            .update(|interceptors| interceptors.push(interceptor));
    }
}

impl RequestInterceptor for RequestRetry {
    fn handle(&self, mut next: RequestNext) -> Result<RequestOutput> {
        let mut retried = 0;

        loop {
            let result = next.next();

            match self.retry(next.run(), &result, retried) {
                Some(delay) => next.sleep(delay),
                None => return result,
            }

            retried += 1;
        }
    }

    #[cfg(feature = "tokio")]
    fn handle_async<'a>(
        &'a self,
        mut next: AsyncRequestNext<'a, '_>,
    ) -> BoxFuture<'a, Result<RequestOutput>> {
        Box::pin(async move {
            let mut retried = 0;

            loop {
                let result = next.next().await;

                match self.retry(next.run(), &result, retried) {
                    Some(delay) => next.sleep(delay).await,
                    None => return result,
                }

                retried += 1;
            }
        })
    }
}

impl RequestInterceptor for RequestDryRun {
    fn handle(&self, mut next: RequestNext) -> Result<RequestOutput> {
        dry_run(next.run_mut());
        next.next()
    }

    #[cfg(feature = "tokio")]
    fn handle_async<'a>(
        &'a self,
        mut next: AsyncRequestNext<'a, '_>,
    ) -> BoxFuture<'a, Result<RequestOutput>> {
        dry_run(next.run_mut());
        Box::pin(async move { next.next().await })
    }
}

impl RequestInterceptor for RequestCache {
    fn handle(&self, mut next: RequestNext) -> Result<RequestOutput> {
        let key = self.hit(next.run_mut());
        let result = next.next();
        self.store(next.run(), key, &result);
        result
    }

    #[cfg(feature = "tokio")]
    fn handle_async<'a>(
        &'a self,
        mut next: AsyncRequestNext<'a, '_>,
    ) -> BoxFuture<'a, Result<RequestOutput>> {
        Box::pin(async move {
            let key = self.hit(next.run_mut());
            let result = next.next().await;
            self.store(next.run(), key, &result);
            result
        })
    }
}

/// Sets the dry run output of `run`, see [`RequestDryRun`].
fn dry_run(run: &mut RequestRun) {
    if !run.intercepted() {
        let output = run.get::<DryRunOutput<RequestOutput>>().map_or_else(
            || RequestOutput {
                status: 200,
                ..Default::default()
            },
            |output| output.0.clone(),
        );
        run.set_output(output);
    }
}

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
/// Returns the interceptors of the runs with `options`, their own or those
/// of the `request`.
pub(crate) fn layers(
    request: &SharedCell<RequestInterceptors>,
    options: &RequestOptions,
) -> Vec<Arc<dyn RequestInterceptor>> {
    options
        .interceptors
        .clone()
        .unwrap_or_else(|| request.get())
        .0
}

/// [internal]
///
/// Returns the built `request`, passing the error of one that cannot be
/// built, or whose body cannot be read, to the interceptors `layers`.
pub(crate) fn built<T>(
    layers: &[Arc<dyn RequestInterceptor>],
    request: std::result::Result<T, crate::Error>,
) -> Result<T> {
    request.inspect_err(|error| layers.iter().for_each(|layer| layer.invalid(error)))
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
/// Returns the built blocking `request` with its body read into memory.
pub(crate) fn buffered_blocking(
    request: reqwest::Result<reqwest::blocking::Request>,
) -> Result<reqwest::blocking::Request> {
    let mut request = request.map_err(invalid_request)?;
    buffer_blocking_body(&mut request)?;
    Ok(request)
}

/// [internal]
///
/// Returns the built async `request` with its body read into memory.
#[cfg(feature = "tokio")]
pub(crate) async fn buffered(
    request: reqwest::Result<reqwest::Request>,
) -> Result<reqwest::Request> {
    let mut request = request.map_err(invalid_request)?;
    buffer_body(&mut request).await?;
    Ok(request)
}

/// [internal]
///
/// Returns a copy of `request` to be sent with the changes of its `run`. A
/// streamed body, never sent here as it is buffered first, is dropped.
fn clone_blocking_request(
    run: &RequestRun,
    request: &reqwest::blocking::Request,
) -> reqwest::blocking::Request {
    let mut clone = request.try_clone().unwrap_or_else(|| {
        let mut clone =
            reqwest::blocking::Request::new(request.method().clone(), request.url().clone());
        *clone.headers_mut() = request.headers().clone();
        clone
    });

    if let Cow::Owned(method) = &run.method {
        *clone.method_mut() = method.clone();
    }

    if let Cow::Owned(url) = &run.url {
        *clone.url_mut() = url.clone();
    }

    if let Cow::Owned(headers) = &run.headers {
        *clone.headers_mut() = headers.clone();
    }

    match &run.body {
        Some(Cow::Owned(body)) => *clone.body_mut() = Some(body.clone().into()),
        Some(Cow::Borrowed(_)) => {}
        None => *clone.body_mut() = None,
    }

    *clone.timeout_mut() = run.timeout;
    clone
}

/// [internal]
///
/// Returns a copy of `request` to be sent with the changes of its `run`
/// like [`clone_blocking_request`].
#[cfg(feature = "tokio")]
fn clone_request(run: &RequestRun, request: &reqwest::Request) -> reqwest::Request {
    let mut clone = request.try_clone().unwrap_or_else(|| {
        let mut clone = reqwest::Request::new(request.method().clone(), request.url().clone());
        *clone.headers_mut() = request.headers().clone();
        clone
    });

    if let Cow::Owned(method) = &run.method {
        *clone.method_mut() = method.clone();
    }

    if let Cow::Owned(url) = &run.url {
        *clone.url_mut() = url.clone();
    }

    if let Cow::Owned(headers) = &run.headers {
        *clone.headers_mut() = headers.clone();
    }

    match &run.body {
        Some(Cow::Owned(body)) => *clone.body_mut() = Some(body.clone().into()),
        Some(Cow::Borrowed(_)) => {}
        None => *clone.body_mut() = None,
    }

    *clone.timeout_mut() = run.timeout;
    clone
}

/// [internal]
///
/// Returns the run of `request` with `options` and `extensions`, passing
/// its body to a sink if `sink`.
pub(crate) fn blocking_run<'a>(
    request: &'a reqwest::blocking::Request,
    options: &'a RequestOptions,
    extensions: Extensions,
    sink: bool,
) -> RequestRun<'a> {
    RequestRun::new(
        (request.method(), request.url(), request.headers()),
        request.body().and_then(reqwest::blocking::Body::as_bytes),
        request.timeout().copied(),
        options,
        extensions,
        sink,
    )
}

/// [internal]
///
/// Returns the run of `request` with `options` and `extensions`, passing
/// its body to a sink if `sink`.
#[cfg(feature = "tokio")]
pub(crate) fn async_run<'a>(
    request: &'a reqwest::Request,
    options: &'a RequestOptions,
    extensions: Extensions,
    sink: bool,
) -> RequestRun<'a> {
    RequestRun::new(
        (request.method(), request.url(), request.headers()),
        request.body().and_then(reqwest::Body::as_bytes),
        request.timeout().copied(),
        options,
        extensions,
        sink,
    )
}

/// [internal]
///
/// Runs `run` by `attempts` through the interceptors `layers`, passing its
/// body to `sink`, if any.
pub(crate) fn run_chain<A: Attempt>(
    attempts: &A,
    layers: &[Arc<dyn RequestInterceptor>],
    mut run: RequestRun,
    sink: Option<&mut Sink<'_>>,
) -> Result<RequestOutput> {
    let result = RequestNext {
        run: &mut run,
        layers,
        inner: &mut Executor {
            attempts,
            layers,
            sink,
        },
    }
    .next();

    conclude(&run, result)
}

/// [internal]
///
/// Runs `run` like [`run_chain`] asynchronously.
#[cfg(feature = "tokio")]
pub(crate) async fn run_chain_async<A: AttemptAsync>(
    attempts: &A,
    layers: &[Arc<dyn RequestInterceptor>],
    mut run: RequestRun<'_>,
    sink: Option<&mut Sink<'_>>,
) -> Result<RequestOutput> {
    let result = AsyncRequestNext {
        run: &mut run,
        layers,
        inner: &mut Executor {
            attempts,
            layers,
            sink,
        },
    }
    .next()
    .await;

    conclude(&run, result)
}

/// Passes the chunk of `data` of the response body of `run` through its
/// interceptors `layers` to `sink`, if any and `to_sink`.
fn emit(
    run: &RequestRun,
    layers: &[Arc<dyn RequestInterceptor>],
    sink: &mut Option<&mut Sink>,
    to_sink: bool,
    data: &[u8],
) -> Result {
    RequestChunk {
        run,
        data: Cow::Borrowed(data),
        layers,
        sink: sink
            .as_mut()
            .filter(|_| to_sink)
            .map(|sink| &mut **sink as &mut Sink),
    }
    .next()
}

/// Returns `output` with its success set from its status.
fn judge(output: RequestOutput) -> RequestOutput {
    RequestOutput {
        success: (200..300).contains(&output.status),
        ..output
    }
}

/// Returns the `result` of `run` with an output that is not a success as an
/// error.
fn conclude(run: &RequestRun, result: Result<RequestOutput>) -> Result<RequestOutput> {
    match result {
        Ok(output) if !output.success => Err(failure(run, output)),
        result => result,
    }
}

/// [internal]
///
/// Returns the error of `run` failing with `output` as its source.
pub(crate) fn failure(run: &RequestRun, output: RequestOutput) -> crate::Error {
    let target = run.target();
    let error = if (200..300).contains(&output.status) {
        request_error!("'{target}' was not accepted with status {}", output.status)
    } else {
        request_error!("'{target}' failed with status {}", output.status)
    };

    error.with_subject(target).with_source(output)
}

/// [internal]
///
/// Returns the output of `run` from the `output` set for it, e.g. by a test
/// or an interceptor, as if it was received, its body passed through
/// `body`.
pub(crate) fn deliver(
    run: &RequestRun,
    output: RequestOutput,
    mut body: Body,
) -> Result<RequestOutput> {
    body.start((200..300).contains(&output.status));

    if !output.body.is_empty() {
        body.push(&output.body)?;
    }

    Ok(RequestOutput {
        body: body.data,
        display_lines: run.options.display_lines,
        ..output
    })
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

/// Returns the response of `run` started at `start` with `status`,
/// `headers` and `body`.
fn received(
    run: &RequestRun,
    start: Instant,
    status: reqwest::StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
) -> RequestOutput {
    RequestOutput {
        success: false,
        status: status.as_u16(),
        headers,
        body,
        duration: start.elapsed(),
        display_lines: run.options.display_lines,
    }
}

/// Returns the error of `run` started at `start` whose response body
/// failed to be received with `error` after its `body` part, kept only
/// when the run is streamed.
fn interrupted(
    run: &RequestRun,
    start: Instant,
    error: reqwest::Error,
    status: reqwest::StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
) -> crate::Error {
    let body = if run.options.stream { body } else { Vec::new() };

    transport_failure(
        run,
        TransportError::from(error).with_output(received(run, start, status, headers, body)),
    )
}

/// Returns how long the response `output` asks to wait before a retry
/// with its `Retry-After` header, if it does.
fn retry_after(output: &RequestOutput) -> Option<Duration> {
    let value = output
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
mod tests {
    use super::*;
    use std::error::Error as _;
    use std::io::BufRead as _;
    use std::io::Read as _;
    use std::io::Write as _;
    use std::net::TcpListener;
    use std::net::TcpStream;
    use std::sync::atomic::AtomicUsize;

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

    /// Records the runs it handles, the chunks of the streamed ones passing
    /// through it and the invalid requests.
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<String>>>);

    impl Recorder {
        fn events(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }

        fn push(&self, event: String) {
            self.0.lock().unwrap().push(event);
        }

        fn finished(&self, result: &Result<RequestOutput>) {
            self.push(format!(
                "finished {}",
                result.as_ref().map_or_else(
                    |error| error.to_string(),
                    |output| output.status.to_string()
                )
            ));
        }
    }

    impl RequestInterceptor for Recorder {
        fn handle(&self, mut next: RequestNext) -> Result<RequestOutput> {
            self.push(format!("started {}", next.run().target()));
            let result = next.next();
            self.finished(&result);
            result
        }

        #[cfg(feature = "tokio")]
        fn handle_async<'a>(
            &'a self,
            mut next: AsyncRequestNext<'a, '_>,
        ) -> BoxFuture<'a, Result<RequestOutput>> {
            Box::pin(async move {
                self.push(format!("started {}", next.run().target()));
                let result = next.next().await;
                self.finished(&result);
                result
            })
        }

        fn output(&self, chunk: RequestChunk) -> Result {
            if chunk.run().options().stream {
                self.push(format!("body {}", String::from_utf8_lossy(chunk.data())));
            }

            chunk.next()
        }

        #[cfg(feature = "tokio")]
        fn cancelled(&self, run: &RequestRun) {
            self.push(format!("cancelled {}", run.target()));
        }

        fn invalid(&self, error: &crate::Error) {
            self.push(format!("invalid {error}"));
        }
    }

    /// Calls the closure with the run before passing it on.
    struct Inspect<F>(F);

    impl<F: Fn(&mut RequestRun) + Send + Sync + 'static> RequestInterceptor for Inspect<F> {
        fn handle(&self, mut next: RequestNext) -> Result<RequestOutput> {
            (self.0)(next.run_mut());
            next.next()
        }

        #[cfg(feature = "tokio")]
        fn handle_async<'a>(
            &'a self,
            mut next: AsyncRequestNext<'a, '_>,
        ) -> BoxFuture<'a, Result<RequestOutput>> {
            (self.0)(next.run_mut());
            Box::pin(async move { next.next().await })
        }
    }

    /// An interceptor with the default methods only.
    struct Passing;

    impl RequestInterceptor for Passing {}

    /// Changes of the env the request shares, like by its platform.
    impl RequestImpl {
        fn set_var(&self, key: String, value: String) {
            self.clients.env.set_var(key, value);
        }

        fn remove_var(&self, key: &str) {
            self.clients.env.remove_var(key);
        }
    }

    /// Returns a request whose only interceptor is the returned recorder.
    fn observed() -> (RequestImpl, Recorder) {
        let recorder = Recorder::default();
        let request = RequestImpl::new();
        request.add_interceptor(recorder.clone());
        (request, recorder)
    }

    /// Returns the interceptors `retry` and `recorder`, in that order.
    fn retried(retry: RequestRetry, recorder: &Recorder) -> RequestInterceptors {
        let mut interceptors = RequestInterceptors::new();
        interceptors.push(retry);
        interceptors.push(recorder.clone());
        interceptors
    }

    /// Returns a request with a single interceptor `interceptor`.
    fn intercepted(interceptor: impl RequestInterceptor) -> RequestImpl {
        let request = RequestImpl::new();
        request.add_interceptor(interceptor);
        request
    }

    fn output(status: u16, body: &[u8]) -> RequestOutput {
        RequestOutput {
            status,
            body: body.to_vec(),
            ..Default::default()
        }
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

    /// Returns the run of `request` with the default options, borrowed from
    /// `options`.
    fn run_of<'a>(
        request: &'a reqwest::blocking::Request,
        options: &'a RequestOptions,
    ) -> RequestRun<'a> {
        blocking_run(request, options, Extensions::default(), false)
    }

    fn plain(method: Method) -> reqwest::blocking::Request {
        reqwest::blocking::Request::new(method, Url::parse("http://h").unwrap())
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
            fn interceptors_should_set_interceptors() {
                let request = reqwest::blocking::Client::new()
                    .get("http://localhost")
                    .interceptors(RequestInterceptors::new());

                assert_eq!(
                    request.options().interceptors,
                    Some(RequestInterceptors::new())
                );
            }
        }

        mod stream {
            use super::*;

            #[test]
            fn stream_should_set_stream() {
                let request = reqwest::blocking::Client::new()
                    .get("http://localhost")
                    .stream();

                assert!(request.options().stream);
            }
        }

        mod with {
            use super::*;

            #[test]
            fn with_should_set_value_for_interceptors() {
                let request = reqwest::blocking::Client::new()
                    .get("http://localhost")
                    .with(7_u32);

                assert_eq!(request.extensions.get::<u32>(), Some(&7));
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
            fn interceptors_should_set_interceptors() {
                let request = reqwest::Client::new()
                    .get("http://localhost")
                    .interceptors(RequestInterceptors::new());

                assert_eq!(
                    request.options().interceptors,
                    Some(RequestInterceptors::new())
                );
            }
        }

        mod stream {
            use super::*;

            #[test]
            fn stream_should_set_stream() {
                let request = reqwest::Client::new().get("http://localhost").stream();

                assert!(request.options().stream);
            }
        }

        mod with {
            use super::*;

            #[test]
            fn with_should_set_value_for_interceptors() {
                let request = reqwest::Client::new().get("http://localhost").with(7_u32);

                assert_eq!(request.extensions.get::<u32>(), Some(&7));
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

        mod send_blocking {
            use super::*;

            #[test]
            fn send_blocking_should_send_request_with_options() {
                let server = serve(ok);
                let (request, recorder) = observed();

                blocking_get(&request, &server.url)
                    .stream()
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
                    .stream()
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

                block_on(get(&request, &server.url).stream().send_async(&request)).unwrap();

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
                        .stream()
                        .send_async_to(&request, &mut collect_into(&mut received)),
                )
                .unwrap();

                assert!(recorder.events().contains(&"body body".to_string()));
                assert_eq!(received, b"body");
            }
        }
    }

    mod request_interceptor {
        use super::*;

        mod handle {
            use super::*;

            #[test]
            fn handle_should_run_rest_of_chain_by_default() {
                let server = serve(ok);
                let request = intercepted(Passing);

                let output = blocking_get(&request, &server.url)
                    .send_blocking(&request)
                    .unwrap();

                assert_eq!(output.body, b"body");
            }
        }

        #[cfg(feature = "tokio")]
        mod handle_async {
            use super::*;

            #[test]
            fn handle_async_should_run_rest_of_chain_by_default() {
                let server = serve(ok);
                let request = intercepted(Passing);

                let output = block_on(get(&request, &server.url).send_async(&request)).unwrap();

                assert_eq!(output.body, b"body");
            }
        }

        mod output {
            use super::*;

            #[test]
            fn output_should_pass_chunk_on_by_default() {
                let server = serve(ok);
                let request = intercepted(Passing);
                let mut received = Vec::new();

                blocking_get(&request, &server.url)
                    .send_blocking_to(&request, &mut collect_into(&mut received))
                    .unwrap();

                assert_eq!(received, b"body");
            }
        }

        #[cfg(feature = "tokio")]
        mod cancelled {
            use super::*;

            #[test]
            fn cancelled_should_do_nothing_by_default() {
                let server = serve(slow);
                let (request, recorder) = observed();
                request.add_interceptor(Passing);

                let result = block_on(async {
                    tokio::time::timeout(
                        Duration::from_millis(100),
                        get(&request, &server.url).send_async(&request),
                    )
                    .await
                });

                assert!(result.is_err());
                assert_eq!(
                    *recorder.events().last().unwrap(),
                    format!("cancelled GET {}/", server.url)
                );
            }
        }

        mod invalid {
            use super::*;

            #[test]
            fn invalid_should_ignore_error_by_default() {
                let request = intercepted(Passing);

                let error = blocking_get(&request, "not a url")
                    .send_blocking(&request)
                    .unwrap_err();

                assert_eq!(error.description(), "invalid request: builder error");
            }
        }
    }

    mod request_next {
        use super::*;

        mod run {
            use super::*;

            #[test]
            fn run_should_return_run_as_changed_before() {
                let server = serve(ok);
                let recorder = Recorder::default();
                let request = intercepted(Inspect(|run: &mut RequestRun| {
                    run.url_mut().set_path("/changed");
                }));
                request.add_interceptor(recorder.clone());

                blocking_get(&request, &server.url)
                    .send_blocking(&request)
                    .unwrap();

                assert_eq!(
                    recorder.events()[0],
                    format!("started GET {}/changed", server.url)
                );
                assert!(server.received()[0].starts_with("GET /changed "));
            }
        }

        mod next {
            use super::*;

            #[test]
            fn next_should_run_interceptors_in_order_then_request() {
                // GIVEN interceptors recording their order around the request
                let server = serve(ok);
                let order = Arc::new(Mutex::new(Vec::new()));
                let request = RequestImpl::new();
                for label in ["outer", "inner"] {
                    let order = order.clone();
                    request.add_interceptor(Inspect(move |_: &mut RequestRun| {
                        order.lock().unwrap().push(label);
                    }));
                }

                // WHEN sending a request
                let output = blocking_get(&request, &server.url)
                    .send_blocking(&request)
                    .unwrap();

                // THEN the outer one runs first and the request last
                assert_eq!(*order.lock().unwrap(), ["outer", "inner"]);
                assert!(output.success);
            }

            #[test]
            fn next_failed_status_should_be_ok_inside_chain() {
                let server = serve(not_found);
                let (request, recorder) = observed();

                let error = blocking_get(&request, &server.url)
                    .send_blocking(&request)
                    .unwrap_err();

                assert_eq!(recorder.events()[1], "finished 404");
                assert_eq!(
                    error.description(),
                    format!("'GET {}/' failed with status 404", server.url)
                );
            }
        }

        mod sleep {
            use super::*;

            #[test]
            fn sleep_should_wait() {
                struct Sleeping;

                impl RequestInterceptor for Sleeping {
                    fn handle(&self, mut next: RequestNext) -> Result<RequestOutput> {
                        next.sleep(Duration::from_millis(50));
                        next.next()
                    }
                }

                let server = serve(ok);
                let request = intercepted(Sleeping);
                let start = Instant::now();

                blocking_get(&request, &server.url)
                    .send_blocking(&request)
                    .unwrap();

                assert!(start.elapsed() >= Duration::from_millis(50));
            }
        }
    }

    #[cfg(feature = "tokio")]
    mod async_request_next {
        use super::*;

        /// Waits `0` before passing the run on.
        struct Sleeping(Duration);

        impl RequestInterceptor for Sleeping {
            fn handle_async<'a>(
                &'a self,
                mut next: AsyncRequestNext<'a, '_>,
            ) -> BoxFuture<'a, Result<RequestOutput>> {
                Box::pin(async move {
                    next.sleep(self.0).await;
                    next.next().await
                })
            }
        }

        mod run {
            use super::*;

            #[test]
            fn run_should_return_run_as_changed_before() {
                let server = serve(ok);
                let recorder = Recorder::default();
                let request = intercepted(Inspect(|run: &mut RequestRun| {
                    run.url_mut().set_path("/changed");
                }));
                request.add_interceptor(recorder.clone());

                block_on(get(&request, &server.url).send_async(&request)).unwrap();

                assert_eq!(
                    recorder.events()[0],
                    format!("started GET {}/changed", server.url)
                );
            }
        }

        mod next {
            use super::*;

            #[test]
            fn next_should_run_interceptors_in_order_then_request() {
                let server = serve(ok);
                let order = Arc::new(Mutex::new(Vec::new()));
                let request = RequestImpl::new();
                for label in ["outer", "inner"] {
                    let order = order.clone();
                    request.add_interceptor(Inspect(move |_: &mut RequestRun| {
                        order.lock().unwrap().push(label);
                    }));
                }

                let output = block_on(get(&request, &server.url).send_async(&request)).unwrap();

                assert_eq!(*order.lock().unwrap(), ["outer", "inner"]);
                assert!(output.success);
            }
        }

        mod sleep {
            use super::*;

            #[test]
            fn sleep_should_wait() {
                let server = serve(ok);
                let request = intercepted(Sleeping(Duration::from_millis(50)));
                let start = Instant::now();

                block_on(get(&request, &server.url).send_async(&request)).unwrap();

                assert!(start.elapsed() >= Duration::from_millis(50));
            }

            #[test]
            fn sleep_cancelled_should_tell_interceptors() {
                let (request, recorder) = observed();
                request.add_interceptor(Sleeping(Duration::from_secs(3600)));

                let result = block_on(async {
                    tokio::time::timeout(
                        Duration::from_millis(50),
                        get(&request, "http://example.invalid/").send_async(&request),
                    )
                    .await
                });

                assert!(result.is_err());
                assert_eq!(
                    recorder.events(),
                    [
                        "started GET http://example.invalid/",
                        "cancelled GET http://example.invalid/"
                    ]
                );
            }
        }
    }

    mod request_chunk {
        use super::*;

        /// Replaces the data of the chunks with `0`.
        struct Replace(&'static [u8]);

        impl RequestInterceptor for Replace {
            fn output(&self, mut chunk: RequestChunk) -> Result {
                chunk.set_data(self.0.to_vec());
                chunk.next()
            }
        }

        mod data {
            use super::*;

            #[test]
            fn data_should_return_chunk_as_received() {
                let server = serve(ok);
                let (request, recorder) = observed();

                blocking_get(&request, &server.url)
                    .stream()
                    .send_blocking(&request)
                    .unwrap();

                assert_eq!(recorder.events()[1], "body body");
            }
        }

        mod set_data {
            use super::*;

            #[test]
            fn set_data_should_replace_data_passed_on_but_not_captured() {
                // GIVEN an interceptor replacing the chunks
                let server = serve(ok);
                let request = intercepted(Replace(b"****"));
                let mut received = Vec::new();

                // WHEN sending requests capturing or sinking their body
                let output = blocking_get(&request, &server.url)
                    .send_blocking(&request)
                    .unwrap();
                blocking_get(&request, &server.url)
                    .send_blocking_to(&request, &mut collect_into(&mut received))
                    .unwrap();

                // THEN the replaced data passes on, the captured one is kept
                assert_eq!(received, b"****");
                assert_eq!(output.body, b"body");
            }
        }

        mod next {
            use super::*;

            #[test]
            fn next_failing_should_end_attempt_with_error() {
                struct Failing;

                impl RequestInterceptor for Failing {
                    fn output(&self, _chunk: RequestChunk) -> Result {
                        Err(crate::Error::request("rejected"))
                    }
                }

                let server = serve(ok);
                let request = intercepted(Failing);

                let error = blocking_get(&request, &server.url)
                    .send_blocking(&request)
                    .unwrap_err();

                assert_eq!(error.description(), "rejected");
            }

            #[test]
            fn next_failed_status_should_not_pass_body_to_sink() {
                let server = serve(not_found);
                let request = RequestImpl::new();
                let mut received = Vec::new();

                let error = blocking_get(&request, &server.url)
                    .send_blocking_to(&request, &mut collect_into(&mut received))
                    .unwrap_err();

                assert!(received.is_empty());
                assert_eq!(error.request_output().unwrap().body, b"missing");
            }
        }
    }

    mod request_interceptors {
        use super::*;

        mod insert {
            use super::*;

            #[test]
            fn insert_should_add_interceptor_at_index() {
                let server = serve(ok);
                let order = Arc::new(Mutex::new(Vec::new()));
                let mut interceptors = RequestInterceptors::new();
                for (index, label) in [(0, "inner"), (0, "outer")] {
                    let order = order.clone();
                    interceptors.insert(
                        index,
                        Inspect(move |_: &mut RequestRun| order.lock().unwrap().push(label)),
                    );
                }
                let request = RequestImpl::new();

                blocking_get(&request, &server.url)
                    .interceptors(interceptors)
                    .send_blocking(&request)
                    .unwrap();

                assert_eq!(*order.lock().unwrap(), ["outer", "inner"]);
            }
        }

        mod is_empty {
            use super::*;

            #[test]
            fn is_empty_should_tell_whether_there_are_interceptors() {
                let mut interceptors = RequestInterceptors::new();
                assert!(interceptors.is_empty());

                interceptors.push(Passing);

                assert!(!interceptors.is_empty());
            }
        }

        mod len {
            use super::*;

            #[test]
            fn len_should_count_interceptors() {
                let mut interceptors = RequestInterceptors::default();
                interceptors.push(Passing);
                interceptors.insert(1, Passing);

                assert_eq!(interceptors.len(), 2);
            }
        }

        mod std_fmt_debug {
            use super::*;

            #[test]
            fn fmt_should_show_count() {
                let mut interceptors = RequestInterceptors::new();
                interceptors.push(Passing);

                assert_eq!(format!("{interceptors:?}"), "RequestInterceptors(1)");
            }
        }

        mod partial_eq {
            use super::*;

            #[test]
            fn eq_should_compare_instances() {
                let mut interceptors = RequestInterceptors::new();
                interceptors.push(Passing);
                let mut other = RequestInterceptors::new();
                other.push(Passing);

                assert_eq!(interceptors, interceptors.clone());
                assert_ne!(interceptors, other);
            }
        }
    }

    mod request_retry {
        use super::*;

        /// Returns the delays before the retries of `retry` of the results of
        /// a `GET` run, `None` when not retried, after `retried` retries.
        fn delays(
            retry: &RequestRetry,
            results: Vec<Result<RequestOutput>>,
            retried: u32,
        ) -> Vec<Option<Duration>> {
            let request = plain(Method::GET);
            let options = RequestOptions::default();
            let run = run_of(&request, &options);

            results
                .iter()
                .map(|result| retry.retry(&run, result, retried))
                .collect()
        }

        /// Returns a failed response asking to wait `retry_after`.
        fn asking(retry_after: &str) -> Result<RequestOutput> {
            let mut output = output(503, b"");
            output
                .headers
                .insert(reqwest::header::RETRY_AFTER, retry_after.parse().unwrap());
            Ok(output)
        }

        mod new {
            use super::*;

            #[test]
            fn new_should_retry_failures_safe_to_retry_until_success() {
                let server = serve(unavailable_once);
                let (request, recorder) = observed();

                let output = blocking_get(&request, &server.url)
                    .interceptors(retried(
                        RequestRetry::new(2, Duration::from_millis(1)),
                        &recorder,
                    ))
                    .send_blocking(&request)
                    .unwrap();

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
            fn new_should_not_retry_permanent_failure_or_success() {
                for respond in [not_found, ok] {
                    let server = serve(respond);
                    let request = intercepted(RequestRetry::new(2, Duration::ZERO));

                    let _ = blocking_get(&request, &server.url).send_blocking(&request);

                    assert_eq!(server.received().len(), 1);
                }
            }

            #[test]
            fn new_should_retry_delay_times_backoff_at_least_retry_after() {
                let retry = RequestRetry::new(3, Duration::from_secs(10));

                assert_eq!(
                    delays(
                        &retry,
                        vec![
                            asking("5"),
                            asking(" 30 "),
                            asking("Sun, 06 Nov 1994 08:49:37 GMT"),
                            asking("soon"),
                            asking("\u{e9}"),
                            Ok(output(503, b""))
                        ],
                        0
                    ),
                    [
                        Some(Duration::from_secs(10)),
                        Some(Duration::from_secs(30)),
                        Some(Duration::from_secs(10)),
                        Some(Duration::from_secs(10)),
                        Some(Duration::from_secs(10)),
                        Some(Duration::from_secs(10))
                    ]
                );
                assert_eq!(delays(&retry, vec![Ok(output(503, b""))], 3), [None]);
            }
        }

        mod backoff {
            use super::*;

            #[test]
            fn backoff_should_multiply_delay_per_retry() {
                for (delay, backoff, retried, expected) in [
                    (Duration::from_millis(100), 1, 2, Duration::from_millis(100)),
                    (Duration::from_millis(100), 2, 0, Duration::from_millis(100)),
                    (Duration::from_millis(100), 2, 2, Duration::from_millis(400)),
                    (Duration::MAX, 2, 1, Duration::MAX),
                ] {
                    let retry = RequestRetry::new(5, delay)
                        .backoff(backoff)
                        .max_delay(Duration::MAX);

                    assert_eq!(
                        delays(&retry, vec![Ok(output(503, b""))], retried),
                        [Some(expected)],
                        "delay: {delay:?}, backoff: {backoff}, retried: {retried}"
                    );
                }
            }
        }

        mod max_delay {
            use super::*;

            #[test]
            fn max_delay_should_limit_backoff() {
                let retry = RequestRetry::new(5, Duration::from_secs(10))
                    .backoff(4)
                    .max_delay(Duration::from_secs(20));

                assert_eq!(
                    delays(&retry, vec![asking("20"), Ok(output(503, b""))], 1),
                    [Some(Duration::from_secs(20)), Some(Duration::from_secs(20))]
                );
            }

            #[test]
            fn max_delay_shorter_than_retry_after_should_not_retry() {
                let retry = RequestRetry::new(5, Duration::from_secs(1));

                assert_eq!(
                    delays(
                        &retry,
                        vec![
                            asking("61"),
                            asking("18446744073709551615"),
                            asking("Sun, 06 Nov 9994 08:49:37 GMT")
                        ],
                        0
                    ),
                    [None, None, None]
                );
            }
        }

        mod when {
            use super::*;

            #[test]
            fn when_should_decide_retries_instead_of_safety() {
                // GIVEN a permanent failure
                let server = serve(not_found);
                let request = intercepted(
                    RequestRetry::new(5, Duration::ZERO).when(|run, _| run.attempt() < 2),
                );

                // WHEN retrying it once more by a condition
                let result = blocking_get(&request, &server.url).send_blocking(&request);

                // THEN it is sent twice
                assert!(result.is_err());
                assert_eq!(server.received().len(), 2);
            }
        }

        mod std_fmt_debug {
            use super::*;

            #[test]
            fn fmt_should_show_configuration() {
                let retry = RequestRetry::new(2, Duration::from_secs(1));

                assert_eq!(
                    format!("{:?}", retry.clone().when(|_, _| true)),
                    "RequestRetry { retries: 2, delay: 1s, backoff: 1, max_delay: 60s, condition: true }"
                );
                assert!(format!("{retry:?}").ends_with("condition: false }"));
            }
        }

        mod request_interceptor {
            use super::*;

            mod handle {
                use super::*;

                #[test]
                fn handle_intercepted_failure_should_not_retry() {
                    let request = intercepted(RequestRetry::new(2, Duration::from_secs(10)));
                    let attempts = Arc::new(AtomicUsize::new(0));
                    let counted = attempts.clone();
                    request.add_interceptor(Inspect(move |run: &mut RequestRun| {
                        counted.fetch_add(1, Ordering::Relaxed);
                        run.set_output(output(503, b""));
                    }));

                    let error = blocking_get(&request, "http://example.invalid/")
                        .send_blocking(&request)
                        .unwrap_err();

                    assert_eq!(error.request_output().unwrap().status, 503);
                    assert_eq!(attempts.load(Ordering::Relaxed), 1);
                }

                #[test]
                fn handle_non_idempotent_server_error_should_not_retry() {
                    let server = serve(|_, _| response(500, "error"));
                    let request = intercepted(RequestRetry::new(2, Duration::ZERO));

                    let result = request
                        .blocking_client()
                        .unwrap()
                        .post(&server.url)
                        .send_blocking(&request);

                    assert_eq!(result.unwrap_err().request_output().unwrap().status, 500);
                    assert_eq!(server.received().len(), 1);
                }

                #[test]
                fn handle_idempotent_server_error_should_retry() {
                    let server = serve(|_, _| response(500, "error"));
                    let request = intercepted(RequestRetry::new(2, Duration::ZERO));

                    let result = request
                        .blocking_client()
                        .unwrap()
                        .put(&server.url)
                        .send_blocking(&request);

                    assert!(result.is_err());
                    assert_eq!(server.received().len(), 3);
                }

                #[test]
                fn handle_streamed_body_should_be_sent_by_each_attempt() {
                    let server = serve(unavailable_once);
                    let request = intercepted(RequestRetry::new(1, Duration::ZERO));

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

                #[test]
                fn handle_interrupted_body_to_sink_should_not_be_retried() {
                    // GIVEN a server closing the connection in the middle of the body
                    let server = serve(truncated);
                    let request =
                        intercepted(RequestRetry::new(2, Duration::ZERO).when(|_, _| true));
                    let mut received = Vec::new();

                    // WHEN sending it to a sink retrying any failure
                    let result = blocking_get(&request, &server.url)
                        .send_blocking_to(&request, &mut collect_into(&mut received));

                    // THEN it is not retried as the sink got a part of the body
                    assert_eq!(transport_kind(&result), Some(TransportErrorKind::Other));
                    assert_eq!(received, b"partial");
                    assert_eq!(server.received().len(), 1);
                }

                #[test]
                fn handle_failure_before_body_to_sink_should_be_retried() {
                    let server = serve(unavailable_once);
                    let request = intercepted(RequestRetry::new(1, Duration::ZERO));
                    let mut received = Vec::new();

                    let output = blocking_get(&request, &server.url)
                        .send_blocking_to(&request, &mut collect_into(&mut received))
                        .unwrap();

                    assert_eq!(output.status, 200);
                    assert_eq!(received, b"body");
                    assert_eq!(server.received().len(), 2);
                }

                #[test]
                fn handle_failing_sink_should_return_its_error_without_retrying() {
                    let server = serve(ok);
                    let request = intercepted(RequestRetry::new(2, Duration::ZERO));

                    let error = blocking_get(&request, &server.url)
                        .send_blocking_to(&request, &mut |_| Err(crate::Error::fs("disk full")))
                        .unwrap_err();

                    assert_eq!(error.description(), "disk full");
                    assert_eq!(server.received().len(), 1);
                }
            }

            #[cfg(feature = "tokio")]
            mod handle_async {
                use super::*;

                #[test]
                fn handle_async_should_retry_failures_until_success() {
                    let server = serve(unavailable_once);
                    let (request, recorder) = observed();

                    let output = block_on(
                        get(&request, &server.url)
                            .interceptors(retried(
                                RequestRetry::new(2, Duration::from_millis(1)),
                                &recorder,
                            ))
                            .send_async(&request),
                    )
                    .unwrap();

                    assert_eq!(output.status, 200);
                    assert_eq!(recorder.events().len(), 4);
                }

                #[test]
                fn handle_async_non_idempotent_server_error_should_not_retry() {
                    let server = serve(|_, _| response(500, "error"));
                    let request = intercepted(RequestRetry::new(2, Duration::ZERO));
                    let builder = request.client().unwrap().patch(&server.url);

                    let result = block_on(builder.send_async(&request));

                    assert_eq!(result.unwrap_err().request_output().unwrap().status, 500);
                    assert_eq!(server.received().len(), 1);
                }

                #[test]
                fn handle_async_streamed_body_should_be_sent_by_each_attempt() {
                    let server = serve(unavailable_once);
                    let request = intercepted(RequestRetry::new(1, Duration::ZERO));
                    let builder = request
                        .client()
                        .unwrap()
                        .post(&server.url)
                        .body(streamed(b"data"));

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
        }
    }

    mod request_dry_run {
        use super::*;

        mod request_interceptor {
            use super::*;

            mod handle {
                use super::*;

                #[test]
                fn handle_should_return_output_set_on_run_or_empty_success() {
                    let server = serve(ok);
                    let request = intercepted(RequestDryRun);

                    let empty = blocking_get(&request, &server.url)
                        .send_blocking(&request)
                        .unwrap();
                    let set = blocking_get(&request, &server.url)
                        .with(DryRunOutput(output(201, b"set")))
                        .send_blocking(&request)
                        .unwrap();

                    assert_eq!(empty, output(200, b""));
                    assert_eq!(set, output(201, b"set"));
                    assert!(server.received().is_empty());
                }

                #[test]
                fn handle_should_keep_output_set_before() {
                    let request = intercepted(Inspect(|run: &mut RequestRun| {
                        run.set_output(output(404, b""));
                    }));
                    request.add_interceptor(RequestDryRun);

                    let error = blocking_get(&request, "http://example.invalid/")
                        .send_blocking(&request)
                        .unwrap_err();

                    assert_eq!(error.request_output().unwrap().status, 404);
                }
            }

            #[cfg(feature = "tokio")]
            mod handle_async {
                use super::*;

                #[test]
                fn handle_async_should_return_output_set_on_run() {
                    let request = intercepted(RequestDryRun);

                    let output = block_on(
                        get(&request, "http://example.invalid/")
                            .with(DryRunOutput(output(200, b"set")))
                            .send_async(&request),
                    )
                    .unwrap();

                    assert_eq!(output.body, b"set");
                }
            }
        }
    }

    mod request_cache {
        use super::*;

        mod entries {
            use super::*;

            #[test]
            fn entries_should_return_cached_outputs_by_key() {
                let server = serve(ok);
                let cache = RequestCache::default();
                let request = intercepted(cache.clone());

                blocking_get(&request, &server.url)
                    .send_blocking(&request)
                    .unwrap();

                let entries = cache.entries();
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].0, format!("GET {}/", server.url));
                assert_eq!(entries[0].1.body, b"body");
            }
        }

        mod key {
            use super::*;

            #[test]
            fn key_should_name_method_url_and_body() {
                let mut request = plain(Method::POST);
                *request.body_mut() = Some("data".into());
                let options = RequestOptions::default();

                assert_eq!(
                    RequestCache::key(&run_of(&request, &options)),
                    "POST http://h/\ndata"
                );
                assert_eq!(
                    RequestCache::key(&run_of(&plain(Method::GET), &options)),
                    "GET http://h/"
                );
            }

            #[test]
            fn key_should_escape_body() {
                let keys = [b"\xFF".as_slice(), b"\xFE"].map(|body| {
                    let mut request = plain(Method::GET);
                    *request.body_mut() = Some(body.to_vec().into());
                    RequestCache::key(&run_of(&request, &RequestOptions::default()))
                });

                assert_eq!(keys, ["GET http://h/\n\\xff", "GET http://h/\n\\xfe"]);
            }
        }

        mod load {
            use super::*;

            #[test]
            fn load_should_return_loaded_outputs_instead_of_sending() {
                let cache = RequestCache::default();
                cache.load([(
                    "GET http://example.invalid/".to_string(),
                    output(200, b"loaded"),
                )]);
                let request = intercepted(cache.clone());

                let output = blocking_get(&request, "http://example.invalid/")
                    .send_blocking(&request)
                    .unwrap();

                assert_eq!(output.body, b"loaded");
            }
        }

        mod std_fmt_debug {
            use super::*;

            #[test]
            fn fmt_should_show_count() {
                assert_eq!(format!("{:?}", RequestCache::default()), "RequestCache(0)");
            }
        }

        mod request_interceptor {
            use super::*;

            mod handle {
                use super::*;

                #[test]
                fn handle_should_return_cached_output_of_same_request() {
                    let server = serve(ok);
                    let cache = RequestCache::default();
                    let request = intercepted(cache.clone());

                    let first = blocking_get(&request, &server.url)
                        .send_blocking(&request)
                        .unwrap();
                    let second = blocking_get(&request, &server.url)
                        .send_blocking(&request)
                        .unwrap();

                    assert_eq!(second, first);
                    assert_eq!(server.received().len(), 1);
                }

                #[test]
                fn handle_should_not_cache_failed_intercepted_or_sunk_outputs() {
                    // GIVEN a cache after an interceptor setting the output
                    let missing = serve(not_found);
                    let found = serve(ok);
                    let cache = RequestCache::default();
                    let request = intercepted(cache.clone());
                    let mut dry = RequestInterceptors::new();
                    dry.push(Inspect(|run: &mut RequestRun| {
                        run.set_output(output(200, b""))
                    }));
                    dry.push(cache.clone());
                    let mut received = Vec::new();

                    // WHEN sending a failing, an intercepted and a sunk request
                    let _ = blocking_get(&request, &missing.url).send_blocking(&request);
                    blocking_get(&request, &found.url)
                        .interceptors(dry)
                        .send_blocking(&request)
                        .unwrap();
                    blocking_get(&request, &found.url)
                        .send_blocking_to(&request, &mut collect_into(&mut received))
                        .unwrap();

                    // THEN none is cached
                    assert!(cache.entries().is_empty());
                }

                #[test]
                fn handle_should_not_cache_nor_answer_other_methods_than_get_and_head() {
                    let server = serve(ok);
                    let cache = RequestCache::default();
                    let request = intercepted(cache.clone());
                    let client = request.blocking_client().unwrap();

                    for method in [Method::HEAD, Method::POST, Method::DELETE] {
                        for _ in 0..2 {
                            client
                                .request(method.clone(), &server.url)
                                .send_blocking(&request)
                                .unwrap();
                        }
                    }

                    assert_eq!(server.received().len(), 5);
                    assert_eq!(cache.entries().len(), 1);
                }

                #[test]
                fn handle_should_keep_output_set_before() {
                    let cache = RequestCache::default();
                    cache.load([("GET http://example.invalid/".to_string(), output(200, b""))]);
                    let request = intercepted(Inspect(|run: &mut RequestRun| {
                        run.set_output(output(404, b""));
                    }));
                    request.add_interceptor(cache);

                    let error = blocking_get(&request, "http://example.invalid/")
                        .send_blocking(&request)
                        .unwrap_err();

                    assert_eq!(error.request_output().unwrap().status, 404);
                }
            }

            #[cfg(feature = "tokio")]
            mod handle_async {
                use super::*;

                #[test]
                fn handle_async_should_return_cached_output_of_same_request() {
                    let server = serve(ok);
                    let cache = RequestCache::default();
                    let request = intercepted(cache.clone());

                    let first = block_on(get(&request, &server.url).send_async(&request)).unwrap();
                    let second = block_on(get(&request, &server.url).send_async(&request)).unwrap();

                    assert_eq!(second, first);
                    assert_eq!(server.received().len(), 1);
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
                stream: true,
                ..Default::default()
            };

            // WHEN taking its run
            let run = run_of(&request, &options);

            // THEN the run shows it before its first attempt
            assert_eq!(run.attempt(), 0);
            assert_eq!(run.method(), Method::POST);
            assert_eq!(run.url(), &url);
            assert_eq!(run.headers()["x-key"], "value");
            assert_eq!(run.body(), Some(b"body".as_slice()));
            assert_eq!(run.timeout(), Some(Duration::from_secs(5)));
            assert_eq!(run.options(), &options);
            assert!(!run.has_sink());
            assert!(!run.intercepted());
            assert_eq!(run.output(), None);
        }

        #[test]
        fn request_run_changes_should_be_sent() {
            // GIVEN an interceptor changing every part of the request
            let server = serve(ok);
            let request = intercepted(Inspect(|run: &mut RequestRun| {
                run.set_method(Method::PUT);
                run.url_mut().set_path("/changed");
                run.headers_mut()
                    .insert("x-key", "changed".parse().unwrap());
                run.set_body(Some(b"changed".to_vec()));
                run.set_timeout(Some(Duration::from_secs(5)));
                run.options_mut().display_lines = 3;
            }));

            // WHEN sending a request
            let output = request
                .blocking_client()
                .unwrap()
                .post(&server.url)
                .body("data")
                .send_blocking(&request)
                .unwrap();

            // THEN the changed request is sent
            let received = &server.received()[0];
            assert!(received.starts_with("PUT /changed "), "{received}");
            assert!(received.contains("x-key: changed\r\n"), "{received}");
            assert!(received.ends_with("\r\n\r\nchanged"), "{received}");
            assert_eq!(output.display_lines, 3);
        }

        #[cfg(feature = "tokio")]
        #[test]
        fn request_run_changes_should_be_sent_async() {
            let server = serve(ok);
            let request = intercepted(Inspect(|run: &mut RequestRun| {
                run.set_method(Method::PUT);
                run.url_mut().set_path("/changed");
                run.headers_mut()
                    .insert("x-key", "changed".parse().unwrap());
                run.set_body(Some(b"changed".to_vec()));
            }));

            block_on(
                request
                    .client()
                    .unwrap()
                    .post(&server.url)
                    .body("data")
                    .send_async(&request),
            )
            .unwrap();

            let received = &server.received()[0];
            assert!(received.starts_with("PUT /changed "), "{received}");
            assert!(received.contains("x-key: changed\r\n"), "{received}");
            assert!(received.ends_with("\r\n\r\nchanged"), "{received}");
        }

        #[test]
        fn request_run_removed_body_should_not_be_sent() {
            let server = serve(ok);
            let request = intercepted(Inspect(|run: &mut RequestRun| run.set_body(None)));

            request
                .blocking_client()
                .unwrap()
                .post(&server.url)
                .body("data")
                .send_blocking(&request)
                .unwrap();

            assert!(server.received()[0].ends_with("\r\n\r\n"));
        }

        mod attempt {
            use super::*;

            #[test]
            fn attempt_should_count_attempts_so_far() {
                let attempts = Arc::new(Mutex::new(Vec::new()));
                let seen = attempts.clone();
                let mut interceptors = RequestInterceptors::new();
                interceptors.push(RequestRetry::new(2, Duration::ZERO).when(|_, _| true));
                interceptors.push(Inspect(move |run: &mut RequestRun| {
                    seen.lock().unwrap().push(run.attempt());
                }));
                let request = RequestImpl::new();

                let _ = blocking_get(&request, &closed_url())
                    .interceptors(interceptors)
                    .send_blocking(&request);

                assert_eq!(*attempts.lock().unwrap(), [0, 1, 2]);
            }
        }

        mod sunk {
            use super::*;

            #[test]
            fn sunk_should_tell_whether_sink_accepted_body() {
                struct Sunk(Arc<Mutex<Vec<bool>>>);

                impl RequestInterceptor for Sunk {
                    fn handle(&self, mut next: RequestNext) -> Result<RequestOutput> {
                        let result = next.next();
                        self.0.lock().unwrap().push(next.run().sunk());
                        result
                    }
                }

                let server = serve(ok);
                let seen = Arc::new(Mutex::new(Vec::new()));
                let request = intercepted(Sunk(seen.clone()));
                let mut received = Vec::new();

                blocking_get(&request, &server.url)
                    .send_blocking(&request)
                    .unwrap();
                blocking_get(&request, &server.url)
                    .send_blocking_to(&request, &mut collect_into(&mut received))
                    .unwrap();

                assert_eq!(*seen.lock().unwrap(), [false, true]);
            }
        }

        mod get {
            use super::*;

            #[test]
            fn get_should_return_value_set_on_request() {
                let seen = Arc::new(Mutex::new(None));
                let found = seen.clone();
                let request = intercepted(Inspect(move |run: &mut RequestRun| {
                    *found.lock().unwrap() = run.get::<u32>().copied();
                }));

                let _ = blocking_get(&request, &closed_url())
                    .with(7_u32)
                    .send_blocking(&request);

                assert_eq!(*seen.lock().unwrap(), Some(7));
            }
        }

        mod has_sink {
            use super::*;

            #[test]
            fn has_sink_should_tell_whether_body_goes_to_sink() {
                let seen = Arc::new(Mutex::new(Vec::new()));
                let found = seen.clone();
                let request = intercepted(Inspect(move |run: &mut RequestRun| {
                    found.lock().unwrap().push(run.has_sink());
                }));
                let url = closed_url();

                let _ = blocking_get(&request, &url).send_blocking(&request);
                let _ = blocking_get(&request, &url)
                    .send_blocking_to(&request, &mut collect_into(&mut Vec::new()));

                assert_eq!(*seen.lock().unwrap(), [false, true]);
            }
        }

        mod id {
            use super::*;

            fn ids_of_two_runs(send: impl Fn(&RequestImpl, &str, RequestInterceptors)) -> Vec<u64> {
                let ids = Arc::new(Mutex::new(Vec::new()));
                let seen = ids.clone();
                let mut interceptors = RequestInterceptors::new();
                interceptors.push(RequestRetry::new(1, Duration::ZERO));
                interceptors.push(Inspect(move |run: &mut RequestRun| {
                    seen.lock().unwrap().push(run.id());
                }));
                let request = RequestImpl::new();
                let url = closed_url();

                send(&request, &url, interceptors.clone());
                send(&request, &url, interceptors);

                ids.lock().unwrap().clone()
            }

            #[test]
            fn id_should_be_same_for_attempts_and_unique_per_run() {
                let ids = ids_of_two_runs(|request, url, interceptors| {
                    let _ = blocking_get(request, url)
                        .interceptors(interceptors)
                        .send_blocking(request);
                });

                assert_ne!(ids[0], 0);
                assert_eq!(ids[..2], [ids[0]; 2]);
                assert_eq!(ids[2..], [ids[2]; 2]);
                assert_ne!(ids[0], ids[2]);
            }

            #[cfg(feature = "tokio")]
            #[test]
            fn id_async_should_be_same_for_attempts_and_unique_per_run() {
                let ids = ids_of_two_runs(|request, url, interceptors| {
                    let _ = block_on(
                        get(request, url)
                            .interceptors(interceptors)
                            .send_async(request),
                    );
                });

                assert_ne!(ids[0], 0);
                assert_eq!(ids[..2], [ids[0]; 2]);
                assert_eq!(ids[2..], [ids[2]; 2]);
                assert_ne!(ids[0], ids[2]);
            }
        }

        mod insert {
            use super::*;

            #[test]
            fn insert_should_replace_value_of_same_type() {
                let request = plain(Method::GET);
                let options = RequestOptions::default();
                let mut run = run_of(&request, &options);
                run.insert(1_u32);

                run.insert(2_u32);

                assert_eq!(run.get::<u32>(), Some(&2));
                assert_eq!(run.get::<u8>(), None);
            }
        }

        mod intercepted_by {
            use super::*;

            #[test]
            fn intercepted_by_should_name_interceptor_setting_output() {
                struct Seeing(Arc<Mutex<Vec<(bool, bool, bool)>>>);

                impl RequestInterceptor for Seeing {
                    fn handle(&self, mut next: RequestNext) -> Result<RequestOutput> {
                        let result = next.next();
                        let run = next.run();
                        self.0.lock().unwrap().push((
                            run.intercepted(),
                            run.intercepted_by::<RequestDryRun>(),
                            run.intercepted_by::<Seeing>(),
                        ));
                        result
                    }
                }

                let server = serve(ok);
                let seen = Arc::new(Mutex::new(Vec::new()));
                let request = intercepted(Seeing(seen.clone()));
                let mut dry = request.interceptors();
                dry.push(RequestDryRun);

                blocking_get(&request, &server.url)
                    .send_blocking(&request)
                    .unwrap();
                blocking_get(&request, &server.url)
                    .interceptors(dry)
                    .send_blocking(&request)
                    .unwrap();

                assert_eq!(
                    *seen.lock().unwrap(),
                    [(false, false, false), (true, true, false)]
                );
            }
        }

        mod safe_to_retry {
            use super::*;

            fn transport(kind: TransportErrorKind) -> Result<RequestOutput> {
                Err(crate::Error::request("transport").with_source(TransportError::new(kind)))
            }

            /// Returns whether each failure of `method` is safe to retry.
            fn safe(method: Method) -> Vec<bool> {
                let request = plain(method);
                let options = RequestOptions::default();
                let run = run_of(&request, &options);

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
                .map(|result| run.safe_to_retry(result))
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

        mod set_output {
            use super::*;

            #[test]
            fn set_output_should_return_output_instead_of_sending() {
                let recorder = Recorder::default();
                let request = intercepted(recorder.clone());
                request.add_interceptor(Inspect(|run: &mut RequestRun| {
                    run.set_output(output(200, b"set"));
                }));
                let mut received = Vec::new();

                let output = blocking_get(&request, "http://example.invalid/")
                    .stream()
                    .send_blocking_to(&request, &mut collect_into(&mut received))
                    .unwrap();

                assert!(output.body.is_empty());
                assert_eq!(received, b"set");
                assert_eq!(recorder.events()[1], "body set");
            }

            #[test]
            fn set_output_sink_failing_should_return_error() {
                let request = intercepted(Inspect(|run: &mut RequestRun| {
                    run.set_output(output(200, b"set"));
                }));

                let error = blocking_get(&request, "http://example.invalid/")
                    .send_blocking_to(&request, &mut |_| Err(crate::Error::fs("disk full")))
                    .unwrap_err();

                assert_eq!(error.description(), "disk full");
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

                let target = run_of(&request, &options).target();

                assert_eq!(target, "GET https://example.com:8080/path");
            }
        }
    }

    mod request_options {
        use super::*;

        mod default {
            use super::*;

            #[test]
            fn default_should_not_stream_with_interceptors_of_request() {
                let options = RequestOptions::default();

                assert_eq!(options.display_lines, 10);
                assert!(!options.stream);
                assert_eq!(options.interceptors, None);
            }
        }
    }

    mod request_output {
        use super::*;

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

        mod default {
            use super::*;

            #[test]
            fn default_should_be_empty_failure_displaying_10_lines() {
                let output = RequestOutput::default();

                assert!(!output.success);
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

        mod text_lossy {
            use super::*;

            #[test]
            fn text_lossy_should_replace_invalid_sequences() {
                assert_eq!(output(200, b"body\xFF").text_lossy(), "body\u{FFFD}");
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

    mod body {
        use super::*;

        /// The result of pushing, the body captured and the chunks passed
        /// on with whether to the sink.
        type Pushed = (Result<()>, Vec<u8>, Vec<(Vec<u8>, bool)>);

        /// Returns the result of pushing `data` to a body started for a
        /// response that is a success if `success`, with a sink if
        /// `has_sink`, the body captured and the chunks passed on with
        /// whether to the sink.
        fn pushed(has_sink: bool, success: bool, fail: bool) -> Pushed {
            let mut chunks = Vec::new();
            let mut emit = |data: &[u8], to_sink| {
                chunks.push((data.to_vec(), to_sink));

                if fail {
                    Err(crate::Error::request("full"))
                } else {
                    Ok(())
                }
            };
            let mut body = Body::new(has_sink, &mut emit);
            body.start(success);
            let result = body.push(b"data");
            let data = body.data;
            (result, data, chunks)
        }

        mod start {
            use super::*;

            #[test]
            fn start_should_pass_body_of_success_to_sink_only() {
                for (has_sink, success, to_sink) in [
                    (true, true, true),
                    (true, false, false),
                    (false, true, false),
                ] {
                    let (result, data, chunks) = pushed(has_sink, success, false);

                    assert!(result.is_ok());
                    assert_eq!(chunks, [(b"data".to_vec(), to_sink)]);
                    assert_eq!(
                        data.is_empty(),
                        to_sink,
                        "sink: {has_sink}, success: {success}"
                    );
                }
            }
        }

        mod push {
            use super::*;

            #[test]
            fn push_failing_to_pass_on_should_return_error_without_capturing() {
                let (result, data, _) = pushed(false, true, true);

                assert_eq!(result.unwrap_err().description(), "full");
                assert!(data.is_empty());
            }
        }

        mod std_io_write {
            use super::*;

            mod write {
                use super::*;

                #[test]
                fn write_should_push_whole_data() {
                    let mut emit = |_: &[u8], _| Ok(());
                    let mut body = Body::new(false, &mut emit);

                    assert_eq!(std::io::Write::write(&mut body, b"data").unwrap(), 4);
                    assert_eq!(body.data, b"data");
                }

                #[test]
                fn write_failing_to_pass_on_should_keep_its_error() {
                    let mut emit = |_: &[u8], _| Err(crate::Error::fs("disk full"));
                    let mut body = Body::new(true, &mut emit);

                    let error = std::io::Write::write(&mut body, b"data").unwrap_err();

                    assert_eq!(error.kind(), std::io::ErrorKind::Other);
                    assert_eq!(body.error.unwrap().description(), "disk full");
                }
            }

            mod flush {
                use super::*;

                #[test]
                fn flush_should_succeed() {
                    let mut emit = |_: &[u8], _| Ok(());
                    let mut body = Body::new(false, &mut emit);
                    std::io::Write::write(&mut body, b"data").unwrap();

                    assert!(std::io::Write::flush(&mut body).is_ok());
                }
            }
        }
    }

    mod failure {
        use super::*;

        #[test]
        fn failure_should_return_error_with_output() {
            let request = plain(Method::GET);
            let options = RequestOptions::default();

            for (status, description) in [
                (404, "'GET http://h/' failed with status 404"),
                (200, "'GET http://h/' was not accepted with status 200"),
            ] {
                let error = failure(&run_of(&request, &options), output(status, b""));

                assert_eq!(error.description(), description);
                assert_eq!(error.request_output().unwrap().status, status);
            }
        }
    }

    mod clone_blocking_request {
        use super::*;

        #[test]
        fn clone_blocking_request_should_copy_request() {
            let mut request = plain(Method::POST);
            *request.body_mut() = Some("data".into());
            let options = RequestOptions::default();

            let clone = clone_blocking_request(&run_of(&request, &options), &request);

            assert_eq!(clone.method(), Method::POST);
            assert_eq!(
                clone.body().and_then(reqwest::blocking::Body::as_bytes),
                Some(b"data".as_slice())
            );
        }

        #[test]
        fn clone_blocking_request_streamed_body_should_drop_body() {
            let mut request = plain(Method::POST);
            request
                .headers_mut()
                .insert("x-key", "value".parse().unwrap());
            *request.timeout_mut() = Some(Duration::from_secs(1));
            let options = RequestOptions::default();
            let run = run_of(&request, &options);
            let mut streamed = plain(Method::POST);
            *streamed.headers_mut() = request.headers().clone();
            *streamed.body_mut() =
                Some(reqwest::blocking::Body::new(std::io::Cursor::new(b"data")));

            let clone = clone_blocking_request(&run, &streamed);

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
            let options = RequestOptions::default();
            let run = async_run(&request, &options, Extensions::default(), false);

            let clone = clone_request(&run, &request);

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

            let run = async_run(&request, &options, Extensions::default(), true);

            assert_eq!(run.attempt(), 0);
            assert_eq!(run.method(), Method::PUT);
            assert_eq!(run.url(), &url);
            assert_eq!(run.body(), Some(b"body".as_slice()));
            assert_eq!(run.timeout(), Some(Duration::from_secs(5)));
            assert!(run.has_sink());
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
                fn send_blocking_should_return_response_and_pass_run_to_interceptors() {
                    let server = serve(ok);
                    let (request, recorder) = observed();
                    let url = format!("{}/path?token=secret", server.url);

                    let output = blocking_get(&request, &url)
                        .send_blocking(&request)
                        .unwrap();

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
                fn send_blocking_invalid_request_should_return_error_passing_it_as_invalid() {
                    let (request, recorder) = observed();

                    let error = blocking_get(&request, "not a url")
                        .send_blocking(&request)
                        .unwrap_err();

                    assert_eq!(error.description(), "invalid request: builder error");
                    assert!(error.source_as::<reqwest::Error>().unwrap().is_builder());
                    assert_eq!(
                        recorder.events(),
                        ["invalid [Request] invalid request: builder error"]
                    );
                }

                #[test]
                fn send_blocking_unreadable_body_should_return_error_passing_it_as_invalid() {
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
                    let events = recorder.events();
                    assert_eq!(events.len(), 1, "{events:?}");
                    assert!(
                        events[0].starts_with("invalid [Request] request body could not be read: "),
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
                fn send_blocking_streaming_should_pass_body_between_start_and_end() {
                    let server = serve(ok);
                    let (request, recorder) = observed();

                    let output = blocking_get(&request, &server.url)
                        .stream()
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
                fn send_blocking_streaming_without_interceptors_should_return_body() {
                    let server = serve(ok);
                    let request = RequestImpl::new();

                    let output = blocking_get(&request, &server.url)
                        .stream()
                        .send_blocking(&request)
                        .unwrap();

                    assert_eq!(output.body, b"body");
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
                fn send_blocking_to_streaming_should_pass_body_to_interceptors_and_sink() {
                    let server = serve(ok);
                    let (request, recorder) = observed();
                    let mut received = Vec::new();

                    blocking_get(&request, &server.url)
                        .stream()
                        .send_blocking_to(&request, &mut |data| {
                            received.extend_from_slice(data);
                            Ok(())
                        })
                        .unwrap();

                    assert!(recorder.events().contains(&"body body".to_string()));
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
                fn send_async_invalid_request_should_return_error_passing_it_as_invalid() {
                    let (request, recorder) = observed();

                    let error =
                        block_on(get(&request, "not a url").send_async(&request)).unwrap_err();

                    assert_eq!(error.description(), "invalid request: builder error");
                    assert_eq!(
                        recorder.events(),
                        ["invalid [Request] invalid request: builder error"]
                    );
                }

                #[test]
                fn send_async_unreadable_body_should_return_error_passing_it_as_invalid() {
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
                    let events = recorder.events();
                    assert_eq!(events.len(), 1, "{events:?}");
                    assert!(
                        events[0].starts_with("invalid [Request] request body could not be read: "),
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
                fn send_async_streaming_should_pass_body_between_start_and_end() {
                    let server = serve(ok);
                    let (request, recorder) = observed();

                    let output =
                        block_on(get(&request, &server.url).stream().send_async(&request)).unwrap();

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
                    let builder = get(&request, &server.url).stream();

                    let output = block_on(builder.send_async_to(&request, &mut |data| {
                        received.extend_from_slice(data);
                        Ok(())
                    }))
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
}
