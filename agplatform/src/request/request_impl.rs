use std::time::Duration;
use std::time::Instant;

use crate::Result;
use crate::env::EnvImpl;
#[cfg(feature = "tokio")]
use crate::interceptor::AsyncExecutor;
use crate::interceptor::BlockingExecutor;
use crate::interceptor::BoxFuture;
use crate::interceptor::Emit;
use crate::interceptor::Executor;
use crate::interceptor::Interceptor;
use crate::interceptor::Interceptors;
use crate::interceptor::Run;
use crate::utils::Runs;

use super::BodySink;
use super::ConfiguredRequest;
use super::DefaultClients;
use super::HttpRequest;
use super::Received;
use super::Request;
use super::RequestOutput;
use super::TransportError;
#[cfg(feature = "tokio")]
use super::run_async;
use super::run_blocking;
use super::transport_failure;

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
    pub(super) clients: DefaultClients,
    runs: Runs<HttpRequest>,
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

    fn dispatch_blocking(
        &self,
        request: ConfiguredRequest<reqwest::blocking::RequestBuilder>,
        sink: Option<&mut BodySink<'_>>,
    ) -> Result<RequestOutput> {
        let (client, run) = request.into_run();
        run_blocking(&self.runs, &BlockingClient(&client), run?, sink)
    }

    #[cfg(feature = "tokio")]
    async fn dispatch_async(
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
        self.dispatch_blocking(request.into(), None)
    }

    fn send_blocking_with_sink(
        &self,
        request: impl Into<ConfiguredRequest<reqwest::blocking::RequestBuilder>>,
        sink: &mut (dyn FnMut(&[u8]) -> Result + Send),
    ) -> Result<RequestOutput> {
        self.dispatch_blocking(request.into(), Some(sink))
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
        self.dispatch_async(request.into(), None)
    }

    #[cfg(feature = "tokio")]
    fn send_async_with_sink(
        &self,
        request: impl Into<ConfiguredRequest<reqwest::RequestBuilder>>,
        sink: &mut (dyn FnMut(&[u8]) -> Result + Send),
    ) -> impl Future<Output = Result<RequestOutput>> + Send {
        self.dispatch_async(request.into(), Some(sink))
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

/// The size of the reads of a blocking response body, like
/// [`std::io::copy`].
const READ_BYTES: usize = 8 * 1024;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interceptor::Retry;
    use crate::request::BlockingRequestBuilderExt;
    #[cfg(feature = "tokio")]
    use crate::request::RequestBuilderExt;
    use crate::request::TransportErrorKind;
    use crate::request::tests::*;

    fn unavailable_once(index: usize, _: &str) -> Vec<u8> {
        if index == 0 {
            response(503, "unavailable")
        } else {
            response(200, "body")
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

            mod send_blocking_with_sink {
                use super::*;

                #[test]
                fn send_blocking_with_sink_should_pass_body_to_sink() {
                    let server = serve(ok);
                    let request = RequestImpl::new();
                    let mut received = Vec::new();

                    let output = blocking_get(&request, &server.url)
                        .send_blocking_with_sink(&request, &mut collect_into(&mut received))
                        .unwrap();

                    assert_eq!(output.status, 200);
                    assert!(output.body.is_empty());
                    assert_eq!(received, b"body");
                }

                #[test]
                fn send_blocking_with_sink_failed_status_should_return_body_in_output() {
                    let server = serve(not_found);
                    let request = RequestImpl::new();
                    let mut received = Vec::new();

                    let error = blocking_get(&request, &server.url)
                        .send_blocking_with_sink(&request, &mut collect_into(&mut received))
                        .unwrap_err();

                    assert_eq!(error.request_output().unwrap().body, b"missing");
                    assert!(received.is_empty());
                }

                #[test]
                fn send_blocking_with_sink_interrupted_body_should_keep_received_part() {
                    // GIVEN a server closing the connection in the middle of the body
                    let server = serve(truncated);
                    let request = RequestImpl::new();
                    let mut received = Vec::new();

                    // WHEN sending a streamed request to a sink
                    let result = blocking_get(&request, &server.url)
                        .keep_partial_body()
                        .send_blocking_with_sink(&request, &mut collect_into(&mut received));

                    // THEN it fails after the sink got the part received
                    assert_eq!(transport_kind(&result), Some(TransportErrorKind::Other));
                    assert_eq!(received, b"partial");
                    let output = interrupted_output(&result);
                    assert_eq!(output.status, 200);
                    assert_eq!(output.headers["content-length"], "100");
                    assert!(output.body.is_empty());
                }

                #[test]
                fn send_blocking_with_sink_interrupted_body_should_not_be_retried() {
                    let server = serve(truncated);
                    let request = intercepted(Retry::new(2, Duration::ZERO));

                    let result = blocking_get(&request, &server.url)
                        .send_blocking_with_sink(&request, &mut collect_into(&mut Vec::new()));

                    assert_eq!(transport_kind(&result), Some(TransportErrorKind::Other));
                    assert_eq!(server.received().len(), 1);
                }

                #[test]
                fn send_blocking_with_sink_failure_before_body_should_be_retried() {
                    let server = serve(unavailable_once);
                    let request = intercepted(HttpRequest::retry(1, Duration::ZERO));
                    let mut received = Vec::new();

                    let output = blocking_get(&request, &server.url)
                        .send_blocking_with_sink(&request, &mut collect_into(&mut received))
                        .unwrap();

                    assert_eq!(output.status, 200);
                    assert_eq!(received, b"body");
                    assert_eq!(server.received().len(), 2);
                }

                #[test]
                fn send_blocking_with_sink_failing_sink_should_return_its_error_without_retrying() {
                    let server = serve(ok);
                    let request = intercepted(Retry::new(2, Duration::ZERO));

                    let error = blocking_get(&request, &server.url)
                        .send_blocking_with_sink(&request, &mut |_| {
                            Err(crate::Error::fs("disk full"))
                        })
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
            mod send_async_with_sink {
                use super::*;

                #[test]
                fn send_async_with_sink_should_pass_body_to_sink() {
                    let server = serve(ok);
                    let (request, recorder) = observed();
                    let mut received = Vec::new();
                    let builder = get(&request, &server.url).keep_partial_body();

                    let output = block_on(
                        builder.send_async_with_sink(&request, &mut collect_into(&mut received)),
                    )
                    .unwrap();

                    assert!(output.body.is_empty());
                    assert_eq!(received, b"body");
                    assert!(recorder.events().contains(&"body body".to_string()));
                }

                #[test]
                fn send_async_with_sink_failed_status_should_return_body_in_output() {
                    let server = serve(not_found);
                    let request = RequestImpl::new();
                    let mut received = Vec::new();
                    let builder = get(&request, &server.url);

                    let error = block_on(
                        builder.send_async_with_sink(&request, &mut collect_into(&mut received)),
                    )
                    .unwrap_err();

                    assert_eq!(error.request_output().unwrap().body, b"missing");
                    assert!(received.is_empty());
                }

                #[test]
                fn send_async_with_sink_interrupted_body_should_keep_received_part() {
                    // GIVEN a server closing the connection in the middle of the body
                    let server = serve(truncated);
                    let request = RequestImpl::new();
                    let mut received = Vec::new();
                    let builder = get(&request, &server.url);

                    // WHEN sending a request to a sink
                    let result = block_on(
                        builder.send_async_with_sink(&request, &mut collect_into(&mut received)),
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
                fn send_async_with_sink_failing_sink_should_return_its_error() {
                    let server = serve(ok);
                    let request = RequestImpl::new();
                    let builder = get(&request, &server.url);

                    let error = block_on(builder.send_async_with_sink(&request, &mut |_| {
                        Err(crate::Error::fs("disk full"))
                    }))
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
}
