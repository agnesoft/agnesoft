use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use reqwest::header::HeaderMap;

use crate::ConfiguredRequest;
use crate::Request;
use crate::RequestObserver;
use crate::RequestOptions;
use crate::RequestOutput;
use crate::RequestRun;
use crate::Result;
use crate::TransportError;
use crate::TransportErrorKind;
use crate::env::SharedEnv;
use crate::request::DefaultClients;
use crate::request::NoObserver;
use crate::request::ObservedBody;
use crate::request::Sink;
use crate::request::blocking_run;
use crate::request::buffer_blocking_body;
use crate::request::built;
use crate::request::check;
use crate::request::run_attempts;
use crate::request::transport_failure;
use crate::request::unsent;
#[cfg(feature = "tokio")]
use crate::request::{AsyncAttempts, BoxFuture, async_run, buffer_body, run_attempts_async};
use crate::test_platform::gate::Gate;
use crate::test_platform::gate::HeldGate;
use crate::utils::ObserverCell;

/// Enabled by the `testing` and `request` feature flags.
///
/// A memory-only [`crate::Request`] implementation for tests. No network
/// is ever accessed. Requests are matched by their method and URL separated by a space (e.g.
/// `GET https://example.com/path?query`) containing the patterns set with
/// [`TestRequest::set_response`] and [`TestRequest::set_transport_error`],
/// or by the predicates set with [`TestRequest::set_response_if`].
///
/// The clients are real [`reqwest`] clients, only built and never used to
/// send anything. Their proxies come from the test environment (see
/// [`crate::Request`]), not from the process environment.
///
/// # Examples
///
/// ```rust
/// use agplatform::{BlockingRequestBuilderExt, Request, RequestOutput, TestRequest};
///
/// let request = TestRequest::new();
/// request.set_response("/status", RequestOutput { status: 200, body: b"ok".to_vec(), ..Default::default() });
///
/// let output = request.blocking_client().unwrap().get("https://example.com/status").send_blocking(&request).unwrap();
///
/// assert_eq!(output.text().unwrap(), "ok");
/// assert_eq!(request.calls(), ["GET https://example.com/status"]);
/// ```
pub struct TestRequest {
    data: Mutex<TestRequestData>,
    pub(crate) clients: DefaultClients,
    observer: ObserverCell<dyn RequestObserver>,
}

/// Enabled by the `testing` and `request` feature flags.
///
/// A request sent through [`TestRequest`], see
/// [`TestRequest::call_details`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestRequestCall {
    /// The method, e.g. `GET`.
    pub method: String,

    /// The whole URL.
    pub url: String,

    /// The request headers.
    pub headers: HeaderMap,

    /// The request body, read into memory, empty when it has none.
    pub body: Vec<u8>,

    /// The options of the run.
    pub options: RequestOptions,
}

/// Enabled by the `testing` and `request` feature flags.
///
/// Holds the requests answered by [`TestRequest::set_pending`] until it is
/// released, e.g. to send other requests while one is in flight. Clones
/// share the gate, so it can be released from another thread or task.
///
/// Dropping all clones releases it, so that a test failing before it
/// releases the gate does not hang waiting for the held requests, e.g. at
/// the end of [`std::thread::scope`] if it is created in the scope.
#[derive(Clone)]
pub struct TestGate(HeldGate<TransportErrorKind>);

#[cfg(feature = "tokio")]
struct TestAttempts<'a, 's> {
    request: &'a TestRequest,
    observer: &'a dyn RequestObserver,
    sink: Option<&'a mut Sink<'s>>,
}

#[derive(Clone)]
struct TestResponse {
    output: RequestOutput,
    chunks: Vec<Vec<u8>>,
    error: Option<TransportErrorKind>,
    gate: Option<Gate<TransportErrorKind>>,
}

type CallPredicate = dyn Fn(&TestRequestCall) -> bool + Send + Sync;

/// Evaluated without holding the lock of [`TestRequestData`], as a
/// predicate may use the [`TestRequest`].
#[derive(Clone)]
enum Matcher {
    Pattern(Arc<str>),
    Predicate(Arc<CallPredicate>),
}

#[derive(Default)]
struct TestRequestData {
    responses: Vec<(Matcher, VecDeque<TestResponse>)>,
    calls: Vec<TestRequestCall>,
    client_failure: bool,
}

impl TestRequest {
    /// Creates a new test request interface.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::TestRequest;
    ///
    /// let request = TestRequest::new();
    /// ```
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            data: Mutex::new(TestRequestData::default()),
            clients: DefaultClients::new(SharedEnv::new(
                Vec::new(),
                std::path::PathBuf::from(crate::test_platform::ROOT),
            )),
            observer: ObserverCell::new(Arc::new(NoObserver)),
        }
    }

    /// Returns the method and the URL of all requests sent so far, oldest
    /// first.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{BlockingRequestBuilderExt, Request, TestRequest};
    ///
    /// let request = TestRequest::new();
    /// let _ = request.blocking_client().unwrap().delete("https://example.com/item?id=1").send_blocking(&request);
    ///
    /// assert_eq!(request.calls(), ["DELETE https://example.com/item?id=1"]);
    /// ```
    pub fn calls(&self) -> Vec<String> {
        self.data()
            .calls
            .iter()
            .map(TestRequestCall::line)
            .collect()
    }

    /// Returns all requests sent so far with their headers, body and
    /// options, oldest first.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{BlockingRequestBuilderExt, Request, TestRequest};
    ///
    /// let request = TestRequest::new();
    /// let client = request.blocking_client().unwrap();
    /// let _ = client.post("https://example.com").bearer_auth("token").body("data").send_blocking(&request);
    ///
    /// let call = &request.call_details()[0];
    /// assert_eq!(call.headers["authorization"], "Bearer token");
    /// assert_eq!(call.body, b"data");
    /// ```
    pub fn call_details(&self) -> Vec<TestRequestCall> {
        self.data().calls.clone()
    }

    /// Forgets the requests sent so far.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{BlockingRequestBuilderExt, Request, TestRequest};
    ///
    /// let request = TestRequest::new();
    /// let _ = request.blocking_client().unwrap().get("https://example.com").send_blocking(&request);
    /// request.clear_calls();
    ///
    /// assert!(request.calls().is_empty());
    /// ```
    pub fn clear_calls(&self) {
        self.data().calls.clear();
    }

    /// Makes building the default clients fail while `fail` is `true`, see
    /// [`crate::Request::client`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{Request, TestRequest};
    ///
    /// let request = TestRequest::new();
    /// request.set_client_failure(true);
    ///
    /// assert!(request.blocking_client().is_err());
    /// ```
    pub fn set_client_failure(&self, fail: bool) {
        self.data().client_failure = fail;
    }

    /// Sets the `output` of the requests whose method and URL contain
    /// `pattern`. When more patterns or predicates match, the most
    /// recently set one wins, so an empty pattern set first acts as a
    /// default.
    ///
    /// A [`RequestOutput::duration`] longer than the timeout of a request
    /// makes it time out, without waiting. An output whose status is not a
    /// success makes the request fail with the output available from
    /// [`Error::request_output`](crate::Error::request_output).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{BlockingRequestBuilderExt, Request, RequestOutput, TestRequest};
    ///
    /// let request = TestRequest::new();
    /// request.set_response("", RequestOutput { status: 200, ..Default::default() });
    /// request.set_response("POST https://example.com/", RequestOutput { status: 500, ..Default::default() });
    ///
    /// let client = request.blocking_client().unwrap();
    /// assert!(client.get("https://example.com").send_blocking(&request).is_ok());
    /// assert!(client.post("https://example.com").send_blocking(&request).is_err());
    /// ```
    pub fn set_response<T: Into<String>>(&self, pattern: T, output: RequestOutput) {
        self.respond_with(
            Matcher::Pattern(pattern.into().into()),
            VecDeque::from([TestResponse::new(output)]),
        );
    }

    /// Sets the `outputs` of the requests whose method and URL contain
    /// `pattern`, returned one per request in order with the last one
    /// repeated, e.g. to make a request fail before it succeeds on a
    /// retry. Patterns match as in [`Self::set_response`]. No `outputs`
    /// set nothing.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{BlockingRequestBuilderExt, Request, RequestOutput, TestRequest};
    /// use std::time::Duration;
    ///
    /// let request = TestRequest::new();
    /// let unavailable = RequestOutput { status: 503, ..Default::default() };
    /// let ok = RequestOutput { status: 200, ..Default::default() };
    /// request.set_responses("example.com", [unavailable, ok]);
    ///
    /// let client = request.blocking_client().unwrap();
    /// let output = client.get("https://example.com").retry(3, Duration::from_secs(1)).send_blocking(&request).unwrap();
    ///
    /// assert_eq!(output.status, 200);
    /// assert_eq!(request.calls().len(), 2);
    /// ```
    pub fn set_responses<T: Into<String>, I: IntoIterator<Item = RequestOutput>>(
        &self,
        pattern: T,
        outputs: I,
    ) {
        self.respond_with(
            Matcher::Pattern(pattern.into().into()),
            outputs.into_iter().map(TestResponse::new).collect(),
        );
    }

    /// Sets the `output` of the requests for which `predicate` returns
    /// `true`, e.g. to match their body. Matches as in
    /// [`Self::set_response`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{BlockingRequestBuilderExt, Request, RequestOutput, TestRequest};
    ///
    /// let request = TestRequest::new();
    /// request.set_response_if(
    ///     |call| call.body.starts_with(b"query"),
    ///     RequestOutput { status: 200, body: b"result".to_vec(), ..Default::default() },
    /// );
    ///
    /// let client = request.blocking_client().unwrap();
    /// let output = client.post("https://example.com/graphql").body("query").send_blocking(&request).unwrap();
    ///
    /// assert_eq!(output.text().unwrap(), "result");
    /// ```
    pub fn set_response_if<F>(&self, predicate: F, output: RequestOutput)
    where
        F: Fn(&TestRequestCall) -> bool + Send + Sync + 'static,
    {
        self.respond_with(
            Matcher::Predicate(Arc::new(predicate)),
            VecDeque::from([TestResponse::new(output)]),
        );
    }

    /// Sets the `output` of the requests whose method and URL contain
    /// `pattern` with its body arriving in `chunks`, e.g. to test a sink
    /// (see [`crate::Request::send_blocking_to`]) or an observer of a
    /// streamed run. The body of the `output` is replaced by the chunks.
    /// Patterns match as in [`Self::set_response`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{BlockingRequestBuilderExt, Request, RequestOutput, TestRequest};
    ///
    /// let request = TestRequest::new();
    /// request.set_chunks("", RequestOutput { status: 200, ..Default::default() }, [b"ab".to_vec(), b"c".to_vec()]);
    /// let mut chunks = Vec::new();
    ///
    /// request
    ///     .blocking_client()
    ///     .unwrap()
    ///     .get("https://example.com/download")
    ///     .send_blocking_to(&request, &mut |chunk| {
    ///         chunks.push(chunk.to_vec());
    ///         Ok(())
    ///     })
    ///     .unwrap();
    ///
    /// assert_eq!(chunks, [b"ab".to_vec(), b"c".to_vec()]);
    /// ```
    pub fn set_chunks<T: Into<String>, I: IntoIterator<Item = Vec<u8>>>(
        &self,
        pattern: T,
        output: RequestOutput,
        chunks: I,
    ) {
        self.respond_with(
            Matcher::Pattern(pattern.into().into()),
            VecDeque::from([TestResponse {
                chunks: chunks.into_iter().collect(),
                ..TestResponse::new(output)
            }]),
        );
    }

    /// Makes the requests whose method and URL contain `pattern` receive
    /// the `chunks` of the body of `output` and then fail with a
    /// [`TransportError`] of `kind`, e.g. for a connection lost in the
    /// middle of a download. The `output` is available from
    /// [`TransportError::output`] with the part of the body received like
    /// [`crate::Request`] does. Patterns match as in [`Self::set_response`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{BlockingRequestBuilderExt, Request, RequestOutput, TestRequest, TransportErrorKind};
    ///
    /// let request = TestRequest::new();
    /// let output = RequestOutput { status: 200, ..Default::default() };
    /// request.set_interrupted("", output, [b"first half".to_vec()], TransportErrorKind::Other);
    /// let mut file = Vec::new();
    ///
    /// let result = request
    ///     .blocking_client()
    ///     .unwrap()
    ///     .get("https://example.com/download")
    ///     .send_blocking_to(&request, &mut |chunk| {
    ///         file.extend_from_slice(chunk);
    ///         Ok(())
    ///     });
    ///
    /// assert!(result.is_err());
    /// assert_eq!(file, b"first half");
    /// ```
    pub fn set_interrupted<T: Into<String>, I: IntoIterator<Item = Vec<u8>>>(
        &self,
        pattern: T,
        output: RequestOutput,
        chunks: I,
        kind: TransportErrorKind,
    ) {
        self.respond_with(
            Matcher::Pattern(pattern.into().into()),
            VecDeque::from([TestResponse {
                chunks: chunks.into_iter().collect(),
                error: Some(kind),
                ..TestResponse::new(output)
            }]),
        );
    }

    /// Sets the `output` of the requests whose method and URL contain
    /// `pattern` like [`Self::set_response`], held until the returned gate
    /// is released or failed. The requests are recorded when they are
    /// sent, before being held.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{BlockingRequestBuilderExt, Request, RequestOutput, TestRequest};
    ///
    /// let request = TestRequest::new();
    /// let client = request.blocking_client().unwrap();
    ///
    /// std::thread::scope(|scope| {
    ///     let gate = request.set_pending("", RequestOutput { status: 200, ..Default::default() });
    ///     let pending = scope.spawn(|| client.get("https://example.com").send_blocking(&request));
    ///
    ///     while request.calls().is_empty() {
    ///         std::thread::yield_now();
    ///     }
    ///
    ///     assert!(!pending.is_finished());
    ///     gate.release();
    ///     assert!(pending.join().unwrap().is_ok());
    /// });
    /// ```
    pub fn set_pending<T: Into<String>>(&self, pattern: T, output: RequestOutput) -> TestGate {
        let gate = TestGate::new();
        self.respond_with(
            Matcher::Pattern(pattern.into().into()),
            VecDeque::from([TestResponse {
                gate: Some(gate.0.gate().clone()),
                ..TestResponse::new(output)
            }]),
        );
        gate
    }

    /// Makes the requests whose method and URL contain `pattern` fail with
    /// a [`TransportError`] of `kind`, e.g.
    /// [`Connect`](TransportErrorKind::Connect) for a server that is down.
    /// Patterns match as in [`Self::set_response`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{BlockingRequestBuilderExt, Request, TestRequest, TransportError, TransportErrorKind};
    ///
    /// let request = TestRequest::new();
    /// request.set_transport_error("example.com", TransportErrorKind::Timeout);
    ///
    /// let error = request.blocking_client().unwrap().get("https://example.com").send_blocking(&request).unwrap_err();
    ///
    /// assert_eq!(error.source_as::<TransportError>().unwrap().kind(), TransportErrorKind::Timeout);
    /// assert!(error.request_output().is_none());
    /// ```
    pub fn set_transport_error<T: Into<String>>(&self, pattern: T, kind: TransportErrorKind) {
        self.respond_with(
            Matcher::Pattern(pattern.into().into()),
            VecDeque::from([TestResponse {
                gate: Some(failed_gate(kind)),
                ..TestResponse::new(RequestOutput::default())
            }]),
        );
    }

    fn respond_with(&self, matcher: Matcher, responses: VecDeque<TestResponse>) {
        if !responses.is_empty() {
            self.data().responses.push((matcher, responses));
        }
    }

    fn send_blocking_with(
        &self,
        request: reqwest::blocking::RequestBuilder,
        options: &RequestOptions,
        mut sink: Option<&mut Sink<'_>>,
    ) -> Result<RequestOutput> {
        #[cfg(feature = "tokio")]
        assert!(
            tokio::runtime::Handle::try_current().is_err(),
            "TestRequest: a blocking request must not be sent in an async context, \
             as it would block its thread, use send_async instead"
        );

        let observer = self.observer.get();
        let observer = observer.as_ref();
        let (_, request) = request.build_split();
        let mut request = built(observer, request)?;

        if let Err(error) = buffer_blocking_body(&mut request) {
            return unsent(observer, &blocking_run(&request, options), error);
        }

        run_attempts(
            observer,
            blocking_run(&request, options),
            &mut |run| self.respond(observer, run, sink.as_deref_mut()),
            |_| {},
        )
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
        let (_, request) = request.build_split();
        let mut request = built(observer, request)?;

        if let Err(error) = buffer_body(&mut request).await {
            return unsent(observer, &async_run(&request, options), error);
        }

        run_attempts_async(
            observer,
            async_run(&request, options),
            &mut TestAttempts {
                request: self,
                observer,
                sink,
            },
        )
        .await
    }

    fn respond(
        &self,
        observer: &dyn RequestObserver,
        run: &RequestRun,
        sink: Option<&mut Sink<'_>>,
    ) -> Result<RequestOutput> {
        let response = self.select(run)?;
        let failure = response.gate.as_ref().and_then(Gate::wait);
        self.deliver(observer, run, response, failure, sink)
    }

    #[cfg(feature = "tokio")]
    async fn respond_async(
        &self,
        observer: &dyn RequestObserver,
        run: &RequestRun<'_>,
        sink: Option<&mut Sink<'_>>,
    ) -> Result<RequestOutput> {
        let response = self.select(run)?;
        let failure = match &response.gate {
            Some(gate) => gate.wait_async().await,
            None => None,
        };
        self.deliver(observer, run, response, failure, sink)
    }

    /// Records the call of `run` and returns the response set for it, the
    /// most recent one matching.
    fn select(&self, run: &RequestRun) -> Result<TestResponse> {
        let call = TestRequestCall {
            method: run.method().to_string(),
            url: run.url().to_string(),
            headers: run.headers().clone(),
            body: run.body().unwrap_or_default().to_vec(),
            options: run.options().clone(),
        };
        let line = call.line();
        let matchers: Vec<Matcher> = self
            .data()
            .responses
            .iter()
            .map(|(matcher, _)| matcher.clone())
            .collect();
        let matched = matchers.iter().rposition(|matcher| match matcher {
            Matcher::Pattern(pattern) => line.contains(&**pattern),
            Matcher::Predicate(predicate) => predicate(&call),
        });
        let mut data = self.data();
        let response = matched.and_then(|index| {
            let responses = &mut data.responses[index].1;

            if responses.len() > 1 {
                responses.pop_front()
            } else {
                responses.front().cloned()
            }
        });
        data.calls.push(call);
        drop(data);

        response.ok_or_else(|| request_error!("'{}': no response set", run.target()))
    }

    /// Returns the output of `response` to `run`, passing its body to the
    /// `sink` and the observer, unless the gate `failure` ends it first.
    fn deliver(
        &self,
        observer: &dyn RequestObserver,
        run: &RequestRun,
        response: TestResponse,
        failure: Option<TransportErrorKind>,
        sink: Option<&mut Sink<'_>>,
    ) -> Result<RequestOutput> {
        let simulated = |kind| transport_failure(run, TransportError::new(kind));

        if let Some(kind) = failure {
            return Err(simulated(kind));
        }

        if run
            .timeout()
            .is_some_and(|timeout| response.output.duration > timeout)
        {
            return Err(simulated(TransportErrorKind::Timeout));
        }

        let sink = sink.filter(|_| response.output.success());
        let mut body = ObservedBody::new(observer, run, sink);

        for chunk in &response.chunks {
            body.push(chunk)?;
        }

        let output = RequestOutput {
            body: body.body,
            display_lines: run.options().display_lines,
            ..response.output
        };

        match response.error {
            Some(kind) => Err(transport_failure(
                run,
                TransportError::new(kind).with_output(RequestOutput {
                    body: if run.options().stream {
                        output.body
                    } else {
                        Vec::new()
                    },
                    ..output
                }),
            )),
            None => check(run, output),
        }
    }

    /// [internal]
    ///
    /// Takes the variables reqwest reads from `env`, e.g. of a
    /// [`TestPlatform`](crate::test_platform::TestPlatform), replacing the
    /// default clients built.
    pub(crate) fn set_env(&mut self, env: SharedEnv) {
        self.clients.set_env(env);
    }

    fn fail_clients(&self) -> Result<()> {
        if self.data().client_failure {
            Err(request_error!(
                "client could not be built: simulated failure"
            ))
        } else {
            Ok(())
        }
    }

    fn data(&self) -> std::sync::MutexGuard<'_, TestRequestData> {
        self.data.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl TestRequestCall {
    fn line(&self) -> String {
        format!("{} {}", self.method, self.url)
    }
}

/// Returns a gate failing with `kind`, e.g. for a request failing before
/// its response arrives.
fn failed_gate(kind: TransportErrorKind) -> Gate<TransportErrorKind> {
    let gate = Gate::new();
    gate.open(Some(kind));
    gate
}

impl TestGate {
    fn new() -> Self {
        Self(HeldGate::new())
    }

    /// Lets the held requests receive their response, and the later ones
    /// at once. Does nothing if the gate was released or failed already.
    pub fn release(&self) {
        self.0.gate().open(None);
    }

    /// Makes the held requests fail with a [`TransportError`] of `kind`,
    /// e.g. for a connection that hangs and then drops, and the later ones
    /// at once. Does nothing if the gate was released or failed already.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{BlockingRequestBuilderExt, Request, RequestOutput, TestRequest, TransportErrorKind};
    ///
    /// let request = TestRequest::new();
    /// let gate = request.set_pending("", RequestOutput { status: 200, ..Default::default() });
    /// gate.fail(TransportErrorKind::Timeout);
    ///
    /// let result = request.blocking_client().unwrap().get("https://example.com").send_blocking(&request);
    ///
    /// assert!(result.is_err());
    /// ```
    pub fn fail(&self, kind: TransportErrorKind) {
        self.0.gate().open(Some(kind));
    }
}

#[cfg(feature = "tokio")]
impl AsyncAttempts for TestAttempts<'_, '_> {
    fn attempt<'a>(&'a mut self, run: &'a RequestRun<'_>) -> BoxFuture<'a, Result<RequestOutput>> {
        Box::pin(
            self.request
                .respond_async(self.observer, run, self.sink.as_deref_mut()),
        )
    }

    /// Retries are not waited for.
    fn delay(&self, _: std::time::Duration) -> BoxFuture<'static, ()> {
        Box::pin(std::future::ready(()))
    }
}

impl TestResponse {
    /// Returns the response of `output` with its body in one chunk, none
    /// when it is empty.
    fn new(mut output: RequestOutput) -> Self {
        let body = std::mem::take(&mut output.body);

        Self {
            output,
            chunks: if body.is_empty() { vec![] } else { vec![body] },
            error: None,
            gate: None,
        }
    }
}

impl Request for TestRequest {
    /// Returns the default blocking client like
    /// [`crate::Request::blocking_client`], built only.
    ///
    /// # Errors
    ///
    /// Returns an error if the client cannot be built or
    /// [`TestRequest::set_client_failure`] is set.
    fn blocking_client(&self) -> Result<reqwest::blocking::Client> {
        self.fail_clients()?;
        self.clients.blocking_client()
    }

    /// Returns a blocking client builder with the proxies of the test
    /// environment like [`crate::Request::blocking_client_builder`].
    fn blocking_client_builder(&self) -> reqwest::blocking::ClientBuilder {
        self.clients.blocking_client_builder()
    }

    /// Sets the builder of the default blocking client.
    ///
    /// See [`crate::Request::set_blocking_client`].
    fn set_blocking_client(&self, builder: reqwest::blocking::ClientBuilder) {
        self.clients.set_blocking_client(builder);
    }

    /// Records the request and returns the response set for it. With
    /// [`RequestOptions::stream`] the chunks of the body set are reported
    /// to the observer. Retries are not waited for, the gates of
    /// [`TestRequest::set_pending`] are.
    ///
    /// See [`crate::Request::send_blocking`].
    ///
    /// # Errors
    ///
    /// Returns an error if no response was set for the request, a
    /// transport error was set for it, or as [`crate::Request::send_blocking`] for
    /// the request and the response set.
    ///
    /// # Panics
    ///
    /// With the `tokio` feature, panics when called in an async context,
    /// where the blocking client cannot be used (see
    /// [`crate::Request::blocking_client`]), as does
    /// [`send_blocking_to`](Self::send_blocking_to).
    fn send_blocking(
        &self,
        request: impl Into<ConfiguredRequest<reqwest::blocking::RequestBuilder>>,
    ) -> Result<RequestOutput> {
        let request = request.into();
        self.send_blocking_with(request.builder, &request.options, None)
    }

    /// Sends the request like [`Self::send_blocking`], passing the chunks
    /// of the body set to `sink` when its status is a success.
    ///
    /// See [`crate::Request::send_blocking_to`].
    ///
    /// # Errors
    ///
    /// As [`Self::send_blocking`], or the error returned by `sink`.
    fn send_blocking_to(
        &self,
        request: impl Into<ConfiguredRequest<reqwest::blocking::RequestBuilder>>,
        sink: &mut (dyn FnMut(&[u8]) -> Result + Send),
    ) -> Result<RequestOutput> {
        let request = request.into();
        self.send_blocking_with(request.builder, &request.options, Some(sink))
    }

    /// Returns the default client like [`crate::Request::client`], built
    /// only.
    ///
    /// # Errors
    ///
    /// As [`Self::blocking_client`].
    #[cfg(feature = "tokio")]
    fn client(&self) -> Result<reqwest::Client> {
        self.fail_clients()?;
        self.clients.client()
    }

    /// Returns a client builder with the proxies of the test environment
    /// like [`crate::Request::client_builder`].
    #[cfg(feature = "tokio")]
    fn client_builder(&self) -> reqwest::ClientBuilder {
        self.clients.client_builder()
    }

    /// Sets the builder of the default client.
    ///
    /// See [`crate::Request::set_client`].
    #[cfg(feature = "tokio")]
    fn set_client(&self, builder: reqwest::ClientBuilder) {
        self.clients.set_client(builder);
    }

    /// Sends the request like [`Self::send_blocking`] once awaited,
    /// without blocking on the gates.
    ///
    /// See [`crate::Request::send_async`].
    ///
    /// # Errors
    ///
    /// As [`Self::send_blocking`].
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

    /// Sends the request like [`Self::send_async`], passing the body to
    /// `sink` like [`Self::send_blocking_to`].
    ///
    /// See [`crate::Request::send_async_to`].
    ///
    /// # Errors
    ///
    /// As [`Self::send_blocking_to`].
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

    /// Sets the observer of the runs, replacing the previous one.
    ///
    /// See [`crate::Request::set_observer`].
    fn set_observer(&self, observer: Arc<dyn RequestObserver>) {
        self.observer.set(observer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::request::ReqwestEnv;

    /// Changes of the env the request shares, like by its platform.
    impl TestRequest {
        fn set_var(&self, key: String, value: String) {
            self.clients.env.set_var(key, value);
        }

        fn remove_var(&self, key: &str) {
            self.clients.env.remove_var(key);
        }
    }
    use crate::BlockingRequestBuilderExt;
    use crate::ConfiguredRequest;
    #[cfg(feature = "tokio")]
    use crate::RequestBuilderExt;
    use std::time::Duration;

    const URL: &str = "https://example.com/path";

    fn status(status: u16) -> RequestOutput {
        RequestOutput {
            status,
            ..Default::default()
        }
    }

    fn get(request: &TestRequest, url: &str) -> reqwest::blocking::RequestBuilder {
        request.blocking_client().unwrap().get(url)
    }

    fn statuses(results: impl IntoIterator<Item = Result<RequestOutput>>) -> Vec<u16> {
        results
            .into_iter()
            .map(|result| match result {
                Ok(output) => output.status,
                Err(error) => error.request_output().map_or(0, |output| output.status),
            })
            .collect()
    }

    fn invalid_client() -> reqwest::blocking::ClientBuilder {
        reqwest::blocking::Client::builder().user_agent("\n")
    }

    fn collect_into(received: &mut Vec<Vec<u8>>) -> impl FnMut(&[u8]) -> Result + Send + '_ {
        |data| {
            received.push(data.to_vec());
            Ok(())
        }
    }

    fn chunks() -> [Vec<u8>; 2] {
        [b"ab".to_vec(), b"c".to_vec()]
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
    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[derive(Default)]
    struct RecordingObserver(Mutex<Vec<String>>);

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

        fn finished(&self, _run: &RequestRun, result: &Result<RequestOutput>) {
            self.0
                .lock()
                .unwrap()
                .push(format!("finished {}", result.is_ok()));
        }
    }

    mod test_request {
        use super::*;

        mod new {
            use super::*;

            #[test]
            fn new_should_have_no_calls() {
                assert!(TestRequest::new().calls().is_empty());
            }
        }

        mod calls {
            use super::*;

            #[test]
            fn calls_should_return_methods_and_urls_in_order() {
                let request = TestRequest::new();
                request.set_response("", status(200));
                let client = request.blocking_client().unwrap();

                client.get(URL).send_blocking(&request).unwrap();
                client
                    .post(format!("{URL}?q=1"))
                    .send_blocking(&request)
                    .unwrap();

                assert_eq!(
                    request.calls(),
                    [format!("GET {URL}"), format!("POST {URL}?q=1")]
                );
            }
        }

        mod call_details {
            use super::*;

            #[test]
            fn call_details_should_return_headers_body_and_options() {
                let request = TestRequest::new();
                request.set_response("", status(200));

                request
                    .blocking_client()
                    .unwrap()
                    .put(URL)
                    .header("x-key", "value")
                    .body("data")
                    .stream()
                    .send_blocking(&request)
                    .unwrap();

                let mut headers = HeaderMap::new();
                headers.insert("x-key", "value".parse().unwrap());
                assert_eq!(
                    request.call_details(),
                    [TestRequestCall {
                        method: "PUT".to_string(),
                        url: URL.to_string(),
                        headers,
                        body: b"data".to_vec(),
                        options: RequestOptions {
                            stream: true,
                            ..Default::default()
                        },
                    }]
                );
            }
        }

        mod clear_calls {
            use super::*;

            #[test]
            fn clear_calls_should_forget_calls() {
                let request = TestRequest::new();
                let _ = get(&request, URL).send_blocking(&request);

                request.clear_calls();

                assert!(request.calls().is_empty());
            }
        }

        mod set_client_failure {
            use super::*;

            #[test]
            fn set_client_failure_should_fail_client_building_until_unset() {
                // GIVEN simulated client failures
                let request = TestRequest::new();
                request.set_client_failure(true);

                // WHEN getting the default clients
                let errors = [
                    request.blocking_client().map(|_| ()),
                    #[cfg(feature = "tokio")]
                    request.client().map(|_| ()),
                ];

                // THEN they fail until the failures are unset
                for error in errors {
                    assert_eq!(
                        error.unwrap_err().description(),
                        "client could not be built: simulated failure"
                    );
                }
                request.set_client_failure(false);
                assert!(request.blocking_client().is_ok());
            }
        }

        mod set_response {
            use super::*;

            #[test]
            fn set_response_should_prefer_most_recent_matching_pattern() {
                // GIVEN a specific pattern followed by a more general one
                let request = TestRequest::new();
                request.set_response("POST https://example.com/", status(500));
                request.set_response("example.com", status(200));

                // WHEN sending a request matching both
                let result = request
                    .blocking_client()
                    .unwrap()
                    .post(URL)
                    .send_blocking(&request);

                // THEN the more recent one wins
                assert_eq!(result.unwrap().status, 200);
            }

            #[test]
            fn set_response_duration_exceeding_timeout_should_time_out() {
                let request = TestRequest::new();
                request.set_response(
                    "",
                    RequestOutput {
                        duration: Duration::from_secs(60),
                        ..status(200)
                    },
                );

                let error = get(&request, URL)
                    .timeout(Duration::from_secs(1))
                    .send_blocking(&request)
                    .unwrap_err();

                assert_eq!(error.description(), format!("'GET {URL}' timed out"));
                assert_eq!(error.request_target(), Some(&*format!("GET {URL}")));
                assert_eq!(
                    error.source_as::<TransportError>().unwrap().kind(),
                    TransportErrorKind::Timeout
                );
            }

            #[test]
            fn set_response_duration_within_timeout_should_return_output() {
                let request = TestRequest::new();
                let output = RequestOutput {
                    duration: Duration::from_secs(1),
                    ..status(200)
                };
                request.set_response("", output.clone());

                let result = get(&request, URL)
                    .timeout(Duration::from_secs(60))
                    .send_blocking(&request);

                assert_eq!(result.unwrap(), output);
            }
        }

        mod set_responses {
            use super::*;

            #[test]
            fn set_responses_should_return_outputs_in_order_repeating_last() {
                let request = TestRequest::new();
                request.set_responses("", [status(500), status(404), status(200)]);

                let results = (0..4).map(|_| get(&request, URL).send_blocking(&request));

                assert_eq!(statuses(results), [500, 404, 200, 200]);
            }

            #[test]
            fn set_responses_empty_should_set_nothing() {
                let request = TestRequest::new();
                request.set_response("", status(200));

                request.set_responses("", []);

                assert!(get(&request, URL).send_blocking(&request).is_ok());
            }

            #[test]
            fn set_responses_with_retries_should_succeed_without_waiting() {
                // GIVEN a request failing transiently twice before it succeeds
                let observer = Arc::new(RecordingObserver::default());
                let request = TestRequest::new();
                request.set_observer(observer.clone());
                request.set_responses("", [status(503), status(503), status(200)]);
                let start = std::time::Instant::now();

                // WHEN retrying it with a long delay
                let output = get(&request, URL)
                    .retry(2, Duration::from_secs(3600))
                    .send_blocking(&request)
                    .unwrap();

                // THEN the last attempt of the one run succeeds at once
                assert_eq!(output.status, 200);
                assert_eq!(request.calls().len(), 3);
                assert!(start.elapsed() < Duration::from_secs(60));
                assert_eq!(
                    *observer.0.lock().unwrap(),
                    [format!("started GET {URL}"), "finished true".to_string()]
                );
            }
        }

        mod set_response_if {
            use super::*;

            #[test]
            fn set_response_if_should_match_by_predicate() {
                // GIVEN a default and a response for bodies with a query
                let request = TestRequest::new();
                request.set_response("", status(404));
                request.set_response_if(|call| call.body.starts_with(b"query"), status(200));
                let client = request.blocking_client().unwrap();

                // WHEN sending requests with and without it
                let matching = client.post(URL).body("query").send_blocking(&request);
                let other = client.post(URL).body("mutation").send_blocking(&request);

                // THEN only the one with the query gets the response
                assert_eq!(statuses([matching, other]), [200, 404]);
            }

            #[test]
            fn set_response_if_predicate_using_request_should_not_deadlock() {
                // GIVEN a predicate matching only the first call
                let request = Arc::new(TestRequest::new());
                let calls = request.clone();
                request.set_response("", status(404));
                request.set_response_if(move |_| calls.calls().is_empty(), status(200));

                // WHEN sending two requests
                let first = get(&request, URL).send_blocking(&*request);
                let second = get(&request, URL).send_blocking(&*request);

                // THEN the predicate sees the calls recorded
                assert_eq!(statuses([first, second]), [200, 404]);
            }
        }

        mod set_chunks {
            use super::*;

            #[test]
            fn set_chunks_should_return_chunks_as_body() {
                let request = TestRequest::new();
                request.set_chunks(
                    "",
                    RequestOutput {
                        body: b"replaced".to_vec(),
                        ..status(200)
                    },
                    chunks(),
                );

                let output = get(&request, URL).send_blocking(&request).unwrap();

                assert_eq!(output.body, b"abc");
            }

            #[test]
            fn set_chunks_streaming_should_report_each_chunk() {
                let observer = Arc::new(RecordingObserver::default());
                let request = TestRequest::new();
                request.set_observer(observer.clone());
                request.set_chunks("", status(200), chunks());

                get(&request, URL).stream().send_blocking(&request).unwrap();

                assert_eq!(
                    *observer.0.lock().unwrap(),
                    [
                        format!("started GET {URL}"),
                        "body ab".to_string(),
                        "body c".to_string(),
                        "finished true".to_string(),
                    ]
                );
            }

            #[test]
            fn set_chunks_duration_exceeding_timeout_should_time_out_without_chunks() {
                let request = TestRequest::new();
                let mut received = Vec::new();
                request.set_chunks(
                    "",
                    RequestOutput {
                        duration: Duration::from_secs(60),
                        ..status(200)
                    },
                    chunks(),
                );

                let error = get(&request, URL)
                    .timeout(Duration::from_secs(1))
                    .send_blocking_to(&request, &mut collect_into(&mut received))
                    .unwrap_err();

                assert_eq!(
                    error.source_as::<TransportError>().unwrap().kind(),
                    TransportErrorKind::Timeout
                );
                assert!(received.is_empty());
            }
        }

        mod set_interrupted {
            use super::*;

            #[test]
            fn set_interrupted_should_pass_chunks_to_sink_before_failing() {
                // GIVEN a download interrupted after its chunks
                let request = TestRequest::new();
                request.set_interrupted("", status(200), chunks(), TransportErrorKind::Other);
                let mut received = Vec::new();

                // WHEN sending it to a sink
                let error = get(&request, URL)
                    .send_blocking_to(&request, &mut collect_into(&mut received))
                    .unwrap_err();

                // THEN the sink gets the chunks before the transport error
                assert_eq!(received, chunks());
                assert_eq!(error.description(), format!("'GET {URL}' failed"));
                let transport = error.source_as::<TransportError>().unwrap();
                assert_eq!(transport.kind(), TransportErrorKind::Other);
                assert_eq!(transport.output(), Some(&status(200)));
            }

            #[test]
            fn set_interrupted_streaming_should_return_part_received_in_error() {
                let request = TestRequest::new();
                request.set_interrupted("", status(200), chunks(), TransportErrorKind::Other);

                let streamed = get(&request, URL).stream().send_blocking(&request);
                let buffered = get(&request, URL).send_blocking(&request);

                let body = |result: &Result<RequestOutput>| {
                    result
                        .as_ref()
                        .unwrap_err()
                        .source_as::<TransportError>()
                        .and_then(TransportError::output)
                        .unwrap()
                        .body
                        .clone()
                };
                assert_eq!(body(&streamed), b"abc");
                assert!(body(&buffered).is_empty());
            }

            #[test]
            fn set_interrupted_with_sink_should_not_be_retried() {
                // GIVEN an interrupted download
                let request = TestRequest::new();
                request.set_interrupted("", status(200), chunks(), TransportErrorKind::Other);
                let mut received = Vec::new();

                // WHEN sending it to a sink retrying any failure
                let result = get(&request, URL)
                    .retry(2, Duration::ZERO)
                    .retry_if(|_, _| true)
                    .send_blocking_to(&request, &mut collect_into(&mut received));

                // THEN the sink gets the body once
                assert!(result.is_err());
                assert_eq!(request.calls().len(), 1);
                assert_eq!(received, chunks());
            }

            #[test]
            fn set_interrupted_without_sink_should_be_retried_as_transient() {
                let request = TestRequest::new();
                request.set_interrupted("", status(200), chunks(), TransportErrorKind::Other);

                let result = get(&request, URL)
                    .retry(1, Duration::from_secs(3600))
                    .send_blocking(&request);

                assert!(result.is_err());
                assert_eq!(request.calls().len(), 2);
            }

            #[cfg(feature = "tokio")]
            #[test]
            fn set_interrupted_with_async_sink_should_not_be_retried() {
                let request = TestRequest::new();
                request.set_interrupted("", status(200), chunks(), TransportErrorKind::Other);
                let mut received = Vec::new();
                let builder = request.client().unwrap().get(URL).retry(2, Duration::ZERO);

                let result =
                    block_on(builder.send_async_to(&request, &mut collect_into(&mut received)));

                assert!(result.is_err());
                assert_eq!(request.calls().len(), 1);
                assert_eq!(received, chunks());
            }
        }

        mod set_pending {
            use super::*;

            #[test]
            fn set_pending_gate_should_release_once_all_clones_are_dropped() {
                // GIVEN a request held by a gate with two clones
                let request = TestRequest::new();

                std::thread::scope(|scope| {
                    let gate = request.set_pending("", status(200));
                    let clone = gate.clone();
                    let pending = scope.spawn(|| get(&request, URL).send_blocking(&request));
                    let _ = std::iter::repeat_with(std::thread::yield_now)
                        .find(|()| !request.calls().is_empty());

                    // WHEN dropping one clone and then the other
                    drop(gate);
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    let held = !pending.is_finished();
                    drop(clone);

                    // THEN the request is held until both are dropped
                    assert!(held);
                    assert!(pending.join().unwrap().is_ok());
                });
            }

            #[test]
            fn set_pending_should_hold_request_until_released() {
                // GIVEN a pending response
                let request = TestRequest::new();
                let gate = request.set_pending(
                    "",
                    RequestOutput {
                        body: b"body".to_vec(),
                        ..status(200)
                    },
                );
                let builder = get(&request, URL);

                std::thread::scope(|scope| {
                    // WHEN sending a request matching it
                    let pending = scope.spawn(|| builder.send_blocking(&request));

                    // THEN it is recorded and held until the gate is released
                    let _ = std::iter::repeat_with(std::thread::yield_now)
                        .find(|()| !request.calls().is_empty());
                    assert!(!pending.is_finished());
                    gate.release();
                    assert_eq!(pending.join().unwrap().unwrap().body, b"body");
                });
            }

            #[test]
            fn set_pending_failed_should_fail_request_with_kind() {
                let request = TestRequest::new();
                let gate = request.set_pending("", status(200));
                gate.fail(TransportErrorKind::Connect);

                let error = get(&request, URL).send_blocking(&request).unwrap_err();

                assert_eq!(
                    error.source_as::<TransportError>().unwrap().kind(),
                    TransportErrorKind::Connect
                );
            }

            #[cfg(feature = "tokio")]
            #[test]
            fn set_pending_should_hold_async_request_until_released() {
                // GIVEN an async request matching a pending response
                let request = TestRequest::new();
                let gate = request.set_pending("", status(200));
                let builder = request.client().unwrap().get(URL);
                let mut future = std::pin::pin!(builder.send_async(&request));
                let mut context = std::task::Context::from_waker(std::task::Waker::noop());

                // WHEN polling it before and after releasing the gate
                let held = future.as_mut().poll(&mut context);
                gate.release();
                let result = block_on(future);

                // THEN it is recorded and held until released
                assert!(held.is_pending());
                assert_eq!(request.calls(), [format!("GET {URL}")]);
                assert_eq!(result.unwrap(), status(200));
            }

            #[cfg(feature = "tokio")]
            #[test]
            fn set_pending_cancelled_should_report_finished() {
                // GIVEN an async request held by a pending response
                let observer = Arc::new(RecordingObserver::default());
                let request = TestRequest::new();
                request.set_observer(observer.clone());
                let _gate = request.set_pending("", status(200));
                let builder = request.client().unwrap().get(URL);
                let mut future = Box::pin(builder.send_async(&request));
                let mut context = std::task::Context::from_waker(std::task::Waker::noop());
                assert!(future.as_mut().poll(&mut context).is_pending());

                // WHEN cancelling it
                drop(future);

                // THEN the run is reported as finished
                assert_eq!(
                    *observer.0.lock().unwrap(),
                    [format!("started GET {URL}"), "finished false".to_string()]
                );
            }

            #[cfg(feature = "tokio")]
            #[test]
            fn set_pending_failed_should_fail_async_request_at_once() {
                let request = TestRequest::new();
                let gate = request.set_pending("", status(200));
                gate.fail(TransportErrorKind::Timeout);
                let builder = request.client().unwrap().get(URL);

                let error = block_on(builder.send_async(&request)).unwrap_err();

                assert_eq!(
                    error.source_as::<TransportError>().unwrap().kind(),
                    TransportErrorKind::Timeout
                );
            }
        }

        mod set_transport_error {
            use super::*;

            #[test]
            fn set_transport_error_should_fail_request_with_kind() {
                let request = TestRequest::new();
                request.set_transport_error("example.com", TransportErrorKind::Connect);

                let error = get(&request, &format!("{URL}?token=secret"))
                    .send_blocking(&request)
                    .unwrap_err();

                assert_eq!(
                    error.description(),
                    format!("'GET {URL}' failed to connect")
                );
                assert_eq!(
                    error.source_as::<TransportError>().unwrap().kind(),
                    TransportErrorKind::Connect
                );
                assert!(error.request_output().is_none());
                assert!(
                    error
                        .source_as::<TransportError>()
                        .unwrap()
                        .output()
                        .is_none()
                );
            }
        }

        mod set_env {
            use super::*;

            #[test]
            fn set_env_should_take_proxy_variables() {
                let mut request = TestRequest::new();
                request.blocking_client().unwrap();
                let env = crate::TestEnv::new()
                    .with_vars(vec![("HTTP_PROXY", "http://proxy"), ("PATH", "/bin")]);

                request.set_env(env.shared().clone());

                assert_eq!(
                    request.clients.reqwest_env(),
                    ReqwestEnv::new(&[("HTTP_PROXY".to_string(), "http://proxy".to_string())])
                );
                assert!(request.blocking_client().is_ok());
            }

            #[test]
            fn set_env_vars_changed_should_take_proxy_variables_only() {
                let request = TestRequest::new();
                request.blocking_client().unwrap();

                request.set_var("HTTPS_PROXY".to_string(), "http://proxy".to_string());
                request.set_var("PATH".to_string(), "/bin".to_string());

                assert_eq!(
                    request.clients.reqwest_env(),
                    ReqwestEnv::new(&[("HTTPS_PROXY".to_string(), "http://proxy".to_string())])
                );
                assert!(request.blocking_client().is_ok());
            }

            #[test]
            fn set_env_proxy_removed_should_be_forgotten() {
                let request = TestRequest::new();
                request.set_var("HTTPS_PROXY".to_string(), "http://proxy".to_string());

                request.remove_var("HTTPS_PROXY");

                assert_eq!(request.clients.reqwest_env(), ReqwestEnv::default());
            }
        }

        mod request {
            use super::*;

            mod blocking_client_builder {
                use super::*;

                #[test]
                fn blocking_client_builder_should_build_client() {
                    let request = TestRequest::new();
                    request.set_var("HTTP_PROXY".to_string(), "http://proxy".to_string());

                    assert!(request.blocking_client_builder().build().is_ok());
                }
            }

            #[cfg(feature = "tokio")]
            mod client_builder {
                use super::*;

                #[test]
                fn client_builder_should_build_client() {
                    let request = TestRequest::new();
                    request.set_var("HTTP_PROXY".to_string(), "http://proxy".to_string());

                    assert!(request.client_builder().build().is_ok());
                }
            }

            mod blocking_client {
                use super::*;

                #[test]
                fn blocking_client_should_build_default_client() {
                    assert!(TestRequest::new().blocking_client().is_ok());
                }
            }

            mod set_blocking_client {
                use super::*;

                #[test]
                fn set_blocking_client_should_replace_default_client() {
                    let request = TestRequest::new();

                    request.set_blocking_client(invalid_client());

                    assert!(request.blocking_client().is_err());
                }
            }

            mod send_blocking {
                use super::*;

                #[test]
                fn send_blocking_should_return_output_and_record_call() {
                    let request = TestRequest::new();
                    let output = RequestOutput {
                        body: b"body".to_vec(),
                        ..status(200)
                    };
                    request.set_response("", output.clone());

                    let result = get(&request, URL).send_blocking(&request);

                    assert_eq!(result.unwrap(), output);
                    assert_eq!(request.calls(), [format!("GET {URL}")]);
                }

                #[test]
                fn send_blocking_display_lines_should_return_output_equal_to_set() {
                    let request = TestRequest::new();
                    let output = RequestOutput {
                        body: b"body".to_vec(),
                        ..status(200)
                    };
                    request.set_response("", output.clone());

                    let result = get(&request, URL)
                        .display_lines(3)
                        .send_blocking(&request)
                        .unwrap();

                    assert_eq!(result, output);
                    assert_eq!(result.display_lines, 3);
                }

                #[test]
                fn send_blocking_unmatched_should_return_error_and_record_call() {
                    let request = TestRequest::new();
                    request.set_response("other.com", status(200));

                    let error = get(&request, &format!("{URL}?q=1"))
                        .send_blocking(&request)
                        .unwrap_err();

                    assert_eq!(error.description(), format!("'GET {URL}': no response set"));
                    assert_eq!(request.calls(), [format!("GET {URL}?q=1")]);
                }

                #[test]
                fn send_blocking_failed_status_should_return_error_with_output() {
                    let request = TestRequest::new();
                    request.set_response("", status(404));

                    let error = get(&request, URL).send_blocking(&request).unwrap_err();

                    assert_eq!(
                        error.description(),
                        format!("'GET {URL}' failed with status 404")
                    );
                    assert_eq!(error.request_output(), Some(&status(404)));
                    assert_eq!(error.request_target(), Some(&*format!("GET {URL}")));
                }

                #[test]
                fn send_blocking_invalid_request_should_report_it_as_invalid_without_recording_call()
                 {
                    let observer = Arc::new(RecordingObserver::default());
                    let request = TestRequest::new();
                    request.set_observer(observer.clone());

                    let error = get(&request, "not a url")
                        .send_blocking(&request)
                        .unwrap_err();

                    assert_eq!(error.description(), "invalid request: builder error");
                    assert!(request.calls().is_empty());
                    assert_eq!(
                        *observer.0.lock().unwrap(),
                        ["invalid [Request] invalid request: builder error"]
                    );
                }

                #[test]
                fn send_blocking_unreadable_body_should_report_finished_run_without_recording_call()
                {
                    let observer = Arc::new(RecordingObserver::default());
                    let request = TestRequest::new();
                    request.set_observer(observer.clone());

                    let error = request
                        .blocking_client()
                        .unwrap()
                        .post(URL)
                        .body(reqwest::blocking::Body::new(FailingReader))
                        .send_blocking(&request)
                        .unwrap_err();

                    assert!(
                        error
                            .description()
                            .starts_with("request body could not be read: ")
                    );
                    assert!(request.calls().is_empty());
                    assert_eq!(
                        *observer.0.lock().unwrap(),
                        [format!("started POST {URL}"), "finished false".to_string()]
                    );
                }

                #[test]
                fn send_blocking_streamed_body_should_be_recorded() {
                    let request = TestRequest::new();
                    request.set_response("", status(200));

                    request
                        .blocking_client()
                        .unwrap()
                        .post(URL)
                        .body(reqwest::blocking::Body::new(std::io::Cursor::new(b"data")))
                        .send_blocking(&request)
                        .unwrap();

                    assert_eq!(request.call_details()[0].body, b"data");
                }

                #[test]
                fn send_blocking_streaming_should_report_non_empty_body_to_observer() {
                    // GIVEN an observer and responses with and without a body
                    let observer = Arc::new(RecordingObserver::default());
                    let request = TestRequest::new();
                    request.set_observer(observer.clone());
                    request.set_responses(
                        "",
                        [
                            RequestOutput {
                                body: b"body".to_vec(),
                                ..status(200)
                            },
                            status(200),
                        ],
                    );

                    // WHEN streaming both
                    for _ in 0..2 {
                        get(&request, URL).stream().send_blocking(&request).unwrap();
                    }

                    // THEN only the body is reported between start and end
                    assert_eq!(
                        *observer.0.lock().unwrap(),
                        [
                            format!("started GET {URL}"),
                            "body body".to_string(),
                            "finished true".to_string(),
                            format!("started GET {URL}"),
                            "finished true".to_string(),
                        ]
                    );
                }

                #[test]
                fn send_blocking_with_options_should_record_options() {
                    let request = TestRequest::new();
                    request.set_response("", status(200));

                    ConfiguredRequest::new(get(&request, URL))
                        .retry(1, Duration::from_secs(1))
                        .send_blocking(&request)
                        .unwrap();

                    assert_eq!(request.call_details()[0].options.retries, 1);
                }

                #[test]
                fn send_blocking_should_retry_by_default_only_failures_safe_for_method() {
                    fn calls(method: reqwest::Method, failure: &dyn Fn(&TestRequest)) -> usize {
                        let request = TestRequest::new();
                        failure(&request);

                        let _ = request
                            .blocking_client()
                            .unwrap()
                            .request(method, URL)
                            .retry(1, Duration::ZERO)
                            .send_blocking(&request);

                        request.calls().len()
                    }
                    let server_error =
                        |request: &TestRequest| request.set_response("", status(500));
                    let unavailable = |request: &TestRequest| request.set_response("", status(503));
                    let timeout = |request: &TestRequest| {
                        request.set_transport_error("", TransportErrorKind::Timeout)
                    };
                    let connect = |request: &TestRequest| {
                        request.set_transport_error("", TransportErrorKind::Connect)
                    };

                    for (method, failure, expected) in [
                        (
                            reqwest::Method::GET,
                            &server_error as &dyn Fn(&TestRequest),
                            2,
                        ),
                        (reqwest::Method::GET, &timeout, 2),
                        (reqwest::Method::POST, &server_error, 1),
                        (reqwest::Method::POST, &timeout, 1),
                        (reqwest::Method::POST, &unavailable, 2),
                        (reqwest::Method::POST, &connect, 2),
                    ] {
                        assert_eq!(calls(method.clone(), failure), expected, "method: {method}");
                    }
                }

                #[cfg(feature = "tokio")]
                #[test]
                #[should_panic(expected = "must not be sent in an async context")]
                fn send_blocking_in_async_context_should_panic() {
                    let request = TestRequest::new();
                    request.set_response("", status(200));
                    let builder = get(&request, URL);

                    let _ = block_on(async { builder.send_blocking(&request) });
                }
            }

            mod send_blocking_to {
                use super::*;

                #[test]
                fn send_blocking_to_should_pass_body_to_sink() {
                    let request = TestRequest::new();
                    request.set_chunks("", status(200), chunks());
                    let mut received = Vec::new();

                    let output = get(&request, URL)
                        .send_blocking_to(&request, &mut collect_into(&mut received))
                        .unwrap();

                    assert!(output.body.is_empty());
                    assert_eq!(received, chunks());
                }

                #[test]
                fn send_blocking_to_failed_status_should_return_body_in_output() {
                    let request = TestRequest::new();
                    request.set_response(
                        "",
                        RequestOutput {
                            body: b"missing".to_vec(),
                            ..status(404)
                        },
                    );
                    let mut received = Vec::new();

                    let error = get(&request, URL)
                        .send_blocking_to(&request, &mut collect_into(&mut received))
                        .unwrap_err();

                    assert_eq!(error.request_output().unwrap().body, b"missing");
                    assert!(received.is_empty());
                }

                #[test]
                fn send_blocking_to_failure_before_body_should_be_retried() {
                    let request = TestRequest::new();
                    request.set_responses(
                        "",
                        [
                            status(503),
                            RequestOutput {
                                body: b"ok".to_vec(),
                                ..status(200)
                            },
                        ],
                    );
                    let mut received = Vec::new();

                    get(&request, URL)
                        .retry(1, Duration::ZERO)
                        .send_blocking_to(&request, &mut collect_into(&mut received))
                        .unwrap();

                    assert_eq!(received, [b"ok".to_vec()]);
                    assert_eq!(request.calls().len(), 2);
                }

                #[test]
                fn send_blocking_to_failing_sink_accepted_by_retry_if_should_be_retried() {
                    let request = TestRequest::new();
                    request.set_chunks("", status(200), chunks());
                    let mut attempts = 0;

                    let result = get(&request, URL)
                        .retry(1, Duration::ZERO)
                        .retry_if(|_, _| true)
                        .send_blocking_to(&request, &mut |_| {
                            attempts += 1;
                            Err(crate::Error::fs("disk full"))
                        });

                    assert!(result.is_err());
                    assert_eq!(attempts, 2);
                    assert_eq!(request.calls().len(), 2);
                }

                #[test]
                fn send_blocking_to_failing_sink_should_return_its_error() {
                    let request = TestRequest::new();
                    request.set_chunks("", status(200), chunks());

                    let error = get(&request, URL)
                        .retry(1, Duration::ZERO)
                        .send_blocking_to(&request, &mut |_| Err(crate::Error::fs("disk full")))
                        .unwrap_err();

                    assert_eq!(error.description(), "disk full");
                    assert_eq!(request.calls().len(), 1);
                }

                #[cfg(feature = "tokio")]
                #[test]
                #[should_panic(expected = "must not be sent in an async context")]
                fn send_blocking_to_in_async_context_should_panic() {
                    let request = TestRequest::new();
                    request.set_response("", status(200));
                    let builder = get(&request, URL);

                    let _ = block_on(async {
                        builder.send_blocking_to(&request, &mut collect_into(&mut Vec::new()))
                    });
                }
            }

            #[cfg(feature = "tokio")]
            mod client {
                use super::*;

                #[test]
                fn client_should_build_default_client() {
                    assert!(TestRequest::new().client().is_ok());
                }
            }

            #[cfg(feature = "tokio")]
            mod set_client {
                use super::*;

                #[test]
                fn set_client_should_replace_default_client() {
                    let request = TestRequest::new();

                    request.set_client(reqwest::Client::builder().user_agent("\n"));

                    assert!(request.client().is_err());
                }
            }

            #[cfg(feature = "tokio")]
            mod send_async {
                use super::*;

                #[test]
                fn send_async_should_return_output_and_record_call() {
                    let request = TestRequest::new();
                    request.set_response("", status(200));
                    let builder = request.client().unwrap().get(URL);

                    let result = block_on(builder.send_async(&request));

                    assert_eq!(result.unwrap(), status(200));
                    assert_eq!(request.calls(), [format!("GET {URL}")]);
                }

                #[test]
                fn send_async_with_retries_should_succeed_without_waiting() {
                    let request = TestRequest::new();
                    request.set_responses("", [status(503), status(200)]);
                    let builder = request
                        .client()
                        .unwrap()
                        .get(URL)
                        .retry(1, Duration::from_secs(3600));

                    let output = block_on(builder.send_async(&request)).unwrap();

                    assert_eq!(output.status, 200);
                    assert_eq!(request.calls().len(), 2);
                }

                #[test]
                fn send_async_non_idempotent_server_error_should_not_retry() {
                    let request = TestRequest::new();
                    request.set_responses("", [status(500), status(200)]);
                    let builder = request.client().unwrap().post(URL).retry(1, Duration::ZERO);

                    let error = block_on(builder.send_async(&request)).unwrap_err();

                    assert_eq!(error.request_output().unwrap().status, 500);
                    assert_eq!(request.calls().len(), 1);
                }

                #[test]
                fn send_async_invalid_request_should_report_it_as_invalid_without_recording_call() {
                    let observer = Arc::new(RecordingObserver::default());
                    let request = TestRequest::new();
                    request.set_observer(observer.clone());
                    let builder = request.client().unwrap().get("not a url");

                    let error = block_on(builder.send_async(&request)).unwrap_err();

                    assert_eq!(error.description(), "invalid request: builder error");
                    assert!(request.calls().is_empty());
                    assert_eq!(
                        *observer.0.lock().unwrap(),
                        ["invalid [Request] invalid request: builder error"]
                    );
                }

                #[test]
                fn send_async_unreadable_body_should_report_finished_run_without_recording_call() {
                    let observer = Arc::new(RecordingObserver::default());
                    let request = TestRequest::new();
                    request.set_observer(observer.clone());
                    let builder = request
                        .client()
                        .unwrap()
                        .post(URL)
                        .body(reqwest::Body::wrap(FailingBody));

                    let error = block_on(builder.send_async(&request)).unwrap_err();

                    assert!(
                        error
                            .description()
                            .starts_with("request body could not be read: ")
                    );
                    assert!(request.calls().is_empty());
                    assert_eq!(
                        *observer.0.lock().unwrap(),
                        [format!("started POST {URL}"), "finished false".to_string()]
                    );
                }
            }

            #[cfg(feature = "tokio")]
            mod send_async_to {
                use super::*;

                #[test]
                fn send_async_to_should_pass_body_to_sink() {
                    let request = TestRequest::new();
                    request.set_chunks("", status(200), chunks());
                    let mut received = Vec::new();
                    let builder = request.client().unwrap().get(URL);

                    let output =
                        block_on(builder.send_async_to(&request, &mut collect_into(&mut received)))
                            .unwrap();

                    assert!(output.body.is_empty());
                    assert_eq!(received, chunks());
                }

                #[test]
                fn send_async_to_panicking_sink_should_not_report_finished() {
                    // GIVEN an observed async request whose sink panics
                    let observer = Arc::new(RecordingObserver::default());
                    let request = TestRequest::new();
                    request.set_observer(observer.clone());
                    request.set_chunks("", status(200), chunks());
                    let builder = request.client().unwrap().get(URL);

                    // WHEN sending it
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        block_on(builder.send_async_to(&request, &mut |_| panic!("sink")))
                    }));

                    // THEN only its start is reported, like a blocking one
                    assert!(result.is_err());
                    assert_eq!(*observer.0.lock().unwrap(), [format!("started GET {URL}")]);
                }

                #[test]
                fn send_async_to_unmatched_should_return_error() {
                    let request = TestRequest::new();
                    let mut received = Vec::new();
                    let builder = request.client().unwrap().get(URL);

                    let error =
                        block_on(builder.send_async_to(&request, &mut collect_into(&mut received)))
                            .unwrap_err();

                    assert_eq!(error.description(), format!("'GET {URL}': no response set"));
                }
            }

            mod set_observer {
                use super::*;

                #[test]
                fn set_observer_should_report_runs() {
                    let observer = Arc::new(RecordingObserver::default());
                    let request = TestRequest::new();
                    request.set_observer(observer.clone());

                    let _ = get(&request, URL).send_blocking(&request);

                    assert_eq!(
                        *observer.0.lock().unwrap(),
                        [format!("started GET {URL}"), "finished false".to_string()]
                    );
                }
            }
        }
    }

    mod test_gate {
        use super::*;

        mod release {
            use super::*;

            #[test]
            fn release_after_fail_should_keep_failure() {
                let request = TestRequest::new();
                let gate = request.set_pending("", status(200));

                gate.fail(TransportErrorKind::Other);
                gate.release();

                assert!(get(&request, URL).send_blocking(&request).is_err());
            }
        }

        mod fail {
            use super::*;

            #[test]
            fn fail_after_release_should_keep_release() {
                let request = TestRequest::new();
                let gate = request.set_pending("", status(200));

                gate.release();
                gate.fail(TransportErrorKind::Other);

                assert!(get(&request, URL).send_blocking(&request).is_ok());
            }
        }
    }
}
