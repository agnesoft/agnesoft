use std::collections::VecDeque;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::time::Duration;

#[cfg(feature = "tokio")]
use crate::BoxFuture;
use crate::CommandInterceptors;
use crate::CommandOptions;
use crate::CommandOutput;
use crate::CommandRun;
use crate::OutputStream;
use crate::Process;
use crate::ProcessInterceptor;
use crate::Result;
use crate::StdStream;
use crate::env::SharedEnv;
use crate::env::record_change;
use crate::process::Attempt;
use crate::process::Outputs;
use crate::process::deliver;
use crate::process::io_error;
use crate::process::run_chain;
#[cfg(feature = "tokio")]
use crate::process::run_chain_async;
use crate::test_platform::ROOT;
use crate::test_platform::gate::Gate;
use crate::test_platform::gate::HeldGate;
#[cfg(feature = "tokio")]
use crate::test_platform::gate::YieldOnce;
use crate::utils::SharedCell;

/// Enabled by the `testing` feature flag.
///
/// A memory-only [`crate::Process`] implementation for tests. No real
/// process is ever spawned. Commands are matched by their command line
/// (the program and the arguments separated by spaces) against the
/// patterns set with [`TestProcess::set_output`] and
/// [`TestProcess::set_io_error`]. The runs pass through the interceptors
/// like with any [`Process`], [`StdStream`] by default, and are recorded
/// when their command would run, i.e. not when an interceptor sets their
/// output, e.g. [`CommandDryRun`](crate::CommandDryRun). Delays, e.g. of
/// [`CommandRetry`](crate::CommandRetry), are not waited for but recorded,
/// see [`TestProcess::sleeps`]. An async one is pending once, so that a run
/// can be cancelled while it waits.
///
/// # Examples
///
/// ```rust
/// use agplatform::{CommandExt, CommandOutput, TestProcess};
/// use std::process::Command;
///
/// let process = TestProcess::new();
/// process.set_output("git status", CommandOutput { status: Some(0), stdout: b"clean".to_vec(), ..Default::default() });
///
/// let output = Command::new("git").arg("status").run(&process).unwrap();
///
/// assert_eq!(output.stdout_str().unwrap(), "clean");
/// assert_eq!(process.calls(), ["git status"]);
/// ```
pub struct TestProcess {
    env: SharedEnv,
    data: Mutex<TestProcessData>,
    interceptors: SharedCell<CommandInterceptors>,
}

/// Enabled by the `testing` feature flag.
///
/// A command run by [`TestProcess`], see [`TestProcess::call_details`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestCall {
    /// The program and the arguments separated by spaces, as changed by the
    /// interceptors.
    pub command_line: String,

    /// The directory the command ran in, i.e. its own resolved against the
    /// current directory.
    pub current_dir: PathBuf,

    /// The environment variables the command got set (`Some`) or removed
    /// (`None`) on top of the inherited environment, which is cleared
    /// before with [`CommandOptions::clear_env`]: those changed on the
    /// platform followed by those of the command.
    pub vars: Vec<(String, Option<String>)>,

    /// The options of the run.
    pub options: CommandOptions,
}

#[derive(Default)]
struct TestProcessData {
    responses: Vec<(String, VecDeque<TestResponse>)>,
    calls: Vec<TestCall>,
    sleeps: Vec<Duration>,
}

#[derive(Clone)]
struct TestResponse {
    result: std::result::Result<CommandOutput, std::io::ErrorKind>,
    gate: Option<Gate<std::convert::Infallible>>,
}

/// Enabled by the `testing` and `process` feature flags.
///
/// Holds the runs answered by [`TestProcess::set_pending`] until it is
/// released, e.g. to check what happens while a command is running or
/// when its run is cancelled. Clones share the gate, so it can be released
/// from another thread or task.
///
/// Dropping all clones releases it, so that a test failing before it
/// releases the gate does not hang waiting for the held runs, e.g. at the
/// end of [`std::thread::scope`] if it is created in the scope.
#[derive(Clone)]
pub struct TestProcessGate(HeldGate<std::convert::Infallible>);

impl TestProcess {
    /// Creates a new test process interface whose current directory is
    /// the [`ROOT`] directory.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::TestProcess;
    ///
    /// let process = TestProcess::new();
    /// ```
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        let mut interceptors = CommandInterceptors::new();
        interceptors.push(StdStream);

        Self {
            env: SharedEnv::new(Vec::new(), PathBuf::from(ROOT)),
            data: Mutex::new(TestProcessData::default()),
            interceptors: SharedCell::new(interceptors),
        }
    }

    /// Returns the command lines of all commands run so far, oldest
    /// first.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{CommandExt, TestProcess};
    /// use std::process::Command;
    ///
    /// let process = TestProcess::new();
    /// let _ = Command::new("git").arg("status").run(&process);
    ///
    /// assert_eq!(process.calls(), ["git status"]);
    /// ```
    pub fn calls(&self) -> Vec<String> {
        self.data()
            .calls
            .iter()
            .map(|call| call.command_line.clone())
            .collect()
    }

    /// Returns all commands run so far with their directory, environment
    /// variables and options, oldest first.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{CommandExt, TestProcess};
    /// use std::process::Command;
    ///
    /// let process = TestProcess::new();
    /// let _ = Command::new("kubectl")
    ///     .current_dir("repo")
    ///     .env("KUBECONFIG", "config")
    ///     .input("manifest")
    ///     .run(&process);
    ///
    /// let call = &process.call_details()[0];
    /// assert_eq!(call.command_line, "kubectl");
    /// assert!(call.current_dir.ends_with("repo"));
    /// assert_eq!(call.vars, [("KUBECONFIG".to_string(), Some("config".to_string()))]);
    /// assert_eq!(call.options.input.as_deref(), Some(b"manifest".as_slice()));
    /// ```
    pub fn call_details(&self) -> Vec<TestCall> {
        self.data().calls.clone()
    }

    /// Forgets the commands run so far.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{CommandExt, TestProcess};
    /// use std::process::Command;
    ///
    /// let process = TestProcess::new();
    /// let _ = Command::new("git").arg("status").run(&process);
    /// process.clear_calls();
    ///
    /// assert!(process.calls().is_empty());
    /// ```
    pub fn clear_calls(&self) {
        self.data().calls.clear();
    }

    /// Returns the delays the interceptors waited for so far, oldest first,
    /// e.g. to check the backoff of [`CommandRetry`](crate::CommandRetry).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{CommandExt, CommandOutput, CommandRetry, Process, TestProcess};
    /// use std::process::Command;
    /// use std::time::Duration;
    ///
    /// let process = TestProcess::new();
    /// process.add_interceptor(CommandRetry::new(2, Duration::from_secs(1)).backoff(2));
    /// process.set_output("git", CommandOutput { status: Some(1), ..Default::default() });
    ///
    /// let _ = Command::new("git").arg("fetch").run(&process);
    ///
    /// assert_eq!(process.sleeps(), [Duration::from_secs(1), Duration::from_secs(2)]);
    /// ```
    pub fn sleeps(&self) -> Vec<Duration> {
        self.data().sleeps.clone()
    }

    /// Sets the `output` of the commands whose command line starts with
    /// `pattern`. When more patterns match, the most recently set one
    /// (including by [`Self::set_io_error`]) wins, so an empty pattern
    /// set first acts as a default.
    ///
    /// A [`CommandOutput::duration`] longer than the
    /// [`CommandOptions::timeout`] of a command makes it time out, without
    /// waiting. The output with non-zero status will return an error with
    /// the output attached to it that can be retrieved via [`Error::command_output`](crate::Error::command_output).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{CommandExt, CommandOutput, TestProcess};
    /// use std::process::Command;
    ///
    /// let process = TestProcess::new();
    /// process.set_output("", CommandOutput { status: Some(0), ..Default::default() });
    /// process.set_output("git push", CommandOutput { status: Some(1), ..Default::default() });
    ///
    /// assert!(Command::new("git").arg("pull").run(&process).is_ok());
    /// assert!(Command::new("git").arg("push").run(&process).is_err());
    /// ```
    pub fn set_output<T: Into<String>>(&self, pattern: T, output: CommandOutput) {
        self.data().responses.push((
            pattern.into(),
            VecDeque::from([TestResponse::new(Ok(output))]),
        ));
    }

    /// Sets the `outputs` of the commands whose command line starts with
    /// `pattern`, returned one per run in order with the last one repeated,
    /// e.g. to make a command fail before it succeeds on a retry. Patterns
    /// match as in [`Self::set_output`]. No `outputs` set nothing.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{CommandExt, CommandOutput, CommandRetry, Process, TestProcess};
    /// use std::process::Command;
    /// use std::time::Duration;
    ///
    /// let process = TestProcess::new();
    /// let failure = CommandOutput { status: Some(1), ..Default::default() };
    /// let success = CommandOutput { status: Some(0), ..Default::default() };
    /// process.set_outputs("curl", [failure, success]);
    ///
    /// process.add_interceptor(CommandRetry::new(3, Duration::from_secs(1)));
    ///
    /// let output = Command::new("curl").run(&process).unwrap();
    ///
    /// assert_eq!(output.status, Some(0));
    /// assert_eq!(process.calls(), ["curl", "curl"]);
    /// ```
    pub fn set_outputs<T: Into<String>, I: IntoIterator<Item = CommandOutput>>(
        &self,
        pattern: T,
        outputs: I,
    ) {
        let outputs: VecDeque<_> = outputs
            .into_iter()
            .map(|output| TestResponse::new(Ok(output)))
            .collect();

        if !outputs.is_empty() {
            self.data().responses.push((pattern.into(), outputs));
        }
    }

    /// Makes the commands whose command line starts with `pattern` fail to
    /// start with an [`std::io::Error`] of `kind`, e.g.
    /// [`NotFound`](std::io::ErrorKind::NotFound) for a program that is not
    /// installed. Patterns match as in [`Self::set_output`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{CommandExt, TestProcess};
    /// use std::io::ErrorKind;
    /// use std::process::Command;
    ///
    /// let process = TestProcess::new();
    /// process.set_io_error("docker", ErrorKind::NotFound);
    ///
    /// let error = Command::new("docker").run(&process).unwrap_err();
    ///
    /// assert_eq!(error.source_as::<std::io::Error>().unwrap().kind(), ErrorKind::NotFound);
    /// assert!(error.command_output().is_none());
    /// ```
    pub fn set_io_error<T: Into<String>>(&self, pattern: T, kind: std::io::ErrorKind) {
        self.data().responses.push((
            pattern.into(),
            VecDeque::from([TestResponse::new(Err(kind))]),
        ));
    }

    /// Makes the commands whose command line starts with `pattern` return
    /// `output` like [`Self::set_output`], held until the returned gate is
    /// released. The runs are recorded when they start, before being held.
    /// A [`CommandOptions::timeout`] does not end the hold.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{CommandExt, CommandOutput, TestProcess};
    /// use std::process::Command;
    ///
    /// let process = TestProcess::new();
    ///
    /// std::thread::scope(|scope| {
    ///     let gate = process.set_pending("cargo build", CommandOutput { status: Some(0), ..Default::default() });
    ///     let building = scope.spawn(|| Command::new("cargo").arg("build").run(&process));
    ///
    ///     while process.calls().is_empty() {
    ///         std::thread::yield_now();
    ///     }
    ///
    ///     assert!(!building.is_finished());
    ///     gate.release();
    ///     assert!(building.join().unwrap().is_ok());
    /// });
    /// ```
    pub fn set_pending<T: Into<String>>(
        &self,
        pattern: T,
        output: CommandOutput,
    ) -> TestProcessGate {
        let gate = TestProcessGate(HeldGate::new());
        self.data().responses.push((
            pattern.into(),
            VecDeque::from([TestResponse {
                gate: Some(gate.0.gate().clone()),
                ..TestResponse::new(Ok(output))
            }]),
        ));
        gate
    }

    /// [internal]
    ///
    /// Takes the current directory and the variable changes from `env`,
    /// e.g. of a [`TestPlatform`](crate::test_platform::TestPlatform).
    pub(crate) fn set_env(&mut self, env: SharedEnv) {
        self.env = env;
    }

    /// [internal]
    ///
    /// Returns the current directory the commands run in.
    #[cfg(test)]
    pub(crate) fn current_dir(&self) -> PathBuf {
        self.env.current_dir().to_path_buf()
    }

    /// Records the call of `run` and returns the response set for it, the
    /// most recent one matching.
    fn select(&self, run: &CommandRun) -> Result<TestResponse> {
        let options = run.options();
        let command_line = run.command_line();
        let mut vars = if options.clear_env {
            Vec::new()
        } else {
            run.var_changes().to_vec()
        };

        for (key, value) in run.envs() {
            record_change(
                &mut vars,
                key.to_string_lossy().into_owned(),
                value
                    .as_ref()
                    .map(|value| value.to_string_lossy().into_owned()),
            );
        }

        let mut data = self.data();
        data.calls.push(TestCall {
            command_line: command_line.clone(),
            // WHY: only runs resolved against the env are run.
            current_dir: run.current_dir().unwrap_or(Path::new("")).to_path_buf(),
            vars,
            options: options.clone(),
        });
        let response = data
            .responses
            .iter_mut()
            .rev()
            .find(|(pattern, _)| command_line.starts_with(pattern.as_str()))
            .and_then(|(_, responses)| {
                if responses.len() > 1 {
                    responses.pop_front()
                } else {
                    responses.front().cloned()
                }
            })
            .ok_or_else(|| process_error!("'{command_line}': no output set"));
        drop(data);
        response
    }

    fn data(&self) -> std::sync::MutexGuard<'_, TestProcessData> {
        self.data.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl TestResponse {
    fn new(result: std::result::Result<CommandOutput, std::io::ErrorKind>) -> Self {
        Self { result, gate: None }
    }
}

impl TestProcessGate {
    /// Lets the held runs return their output, and the later ones at once.
    /// Does nothing if the gate was released already.
    pub fn release(&self) {
        self.0.gate().open(None);
    }
}

impl Attempt for TestProcess {
    fn attempt(&self, run: &CommandRun, outputs: Outputs) -> Result<CommandOutput> {
        let response = self.select(run)?;

        if let Some(gate) = &response.gate {
            gate.wait();
        }

        respond(run, response.result, outputs)
    }

    #[cfg(feature = "tokio")]
    fn attempt_async<'a>(
        &'a self,
        run: &'a CommandRun,
        outputs: Outputs<'a>,
    ) -> BoxFuture<'a, Result<CommandOutput>> {
        Box::pin(async move {
            let response = self.select(run)?;

            if let Some(gate) = &response.gate {
                gate.wait_async().await;
            }

            respond(run, response.result, outputs)
        })
    }

    /// Delays are recorded instead of waited for, like timeouts.
    fn sleep(&self, delay: Duration) {
        self.data().sleeps.push(delay);
    }

    #[cfg(feature = "tokio")]
    fn delay(&self, delay: Duration) -> BoxFuture<'static, ()> {
        self.data().sleeps.push(delay);
        Box::pin(YieldOnce(false))
    }
}

impl Process for TestProcess {
    /// Records the command and returns the output set for it, without
    /// reading the input. Its non-empty output is passed to the
    /// interceptors at once. With [`CommandOptions::silent`] or
    /// [`CommandOptions::interactive`] the output set is dropped, as no
    /// output would be captured.
    ///
    /// See [`crate::Process::run`].
    ///
    /// # Errors
    ///
    /// Returns an error if no output was set for the command, an I/O error
    /// was set for it, or as [`crate::Process::run`] for the options and
    /// the output set.
    fn run(&self, run: impl Into<CommandRun>) -> Result<CommandOutput> {
        run_chain(
            self,
            &self.interceptors,
            run.into().resolve(&self.env),
            None,
        )
    }

    /// Runs the command like [`Self::run`], passing the output set for it to
    /// `sink`, the standard output and then the standard error, each in one
    /// chunk unless empty.
    ///
    /// See [`crate::Process::run_to`].
    ///
    /// # Errors
    ///
    /// As [`Self::run`], or as [`crate::Process::run_to`].
    fn run_to(
        &self,
        run: impl Into<CommandRun>,
        sink: &mut (dyn FnMut(OutputStream, &[u8]) -> Result + Send),
    ) -> Result<CommandOutput> {
        run_chain(
            self,
            &self.interceptors,
            run.into().resolve(&self.env).with_sink(),
            Some(sink),
        )
    }

    /// Runs the command like [`Self::run`] once awaited, holding only the
    /// task while a [`Self::set_pending`] gate is closed. Dropping the
    /// future cancels the run, which is told to the interceptors.
    ///
    /// See [`crate::Process::run_async`].
    ///
    /// # Errors
    ///
    /// As [`Self::run`].
    #[cfg(feature = "tokio")]
    fn run_async(
        &self,
        run: impl Into<CommandRun>,
    ) -> impl Future<Output = Result<CommandOutput>> + Send {
        run_chain_async(
            self,
            &self.interceptors,
            run.into().resolve(&self.env),
            None,
        )
    }

    /// Runs the command like [`Self::run_async`], passing the output set for
    /// it to `sink` like [`Self::run_to`].
    ///
    /// See [`crate::Process::run_async_to`].
    ///
    /// # Errors
    ///
    /// As [`Self::run_to`].
    #[cfg(feature = "tokio")]
    fn run_async_to(
        &self,
        run: impl Into<CommandRun>,
        sink: &mut (dyn FnMut(OutputStream, &[u8]) -> Result + Send),
    ) -> impl Future<Output = Result<CommandOutput>> + Send {
        run_chain_async(
            self,
            &self.interceptors,
            run.into().resolve(&self.env).with_sink(),
            Some(sink),
        )
    }

    fn interceptors(&self) -> CommandInterceptors {
        self.interceptors.get()
    }

    fn clear_interceptors(&self) {
        self.interceptors
            .update(|interceptors| *interceptors = CommandInterceptors::new());
    }

    fn add_interceptor(&self, interceptor: impl ProcessInterceptor) {
        self.interceptors
            .update(|interceptors| interceptors.push(interceptor));
    }
}

/// Returns the output of `run` from the `result` set for it, applying its
/// options.
fn respond(
    run: &CommandRun,
    result: std::result::Result<CommandOutput, std::io::ErrorKind>,
    outputs: Outputs,
) -> Result<CommandOutput> {
    let output = deliver(
        run,
        result.map_err(|kind| io_error(&run.program(), kind.into()))?,
        outputs,
    )?;

    Ok(match run.options().timeout {
        Some(timeout) if output.duration > timeout => CommandOutput {
            status: None,
            duration: timeout,
            timed_out: true,
            ..output
        },
        _ => output,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CommandExt;
    use crate::CommandNext;
    use crate::CommandRetry;
    use std::process::Command;
    use std::sync::Arc;
    use std::time::Duration;

    /// Changes of the env the process shares, like by its platform.
    impl TestProcess {
        fn set_var(&self, key: String, value: String) {
            self.env.set_var(key, value);
        }

        fn remove_var(&self, key: &str) {
            self.env.remove_var(key);
        }
    }

    fn exit(status: i32) -> CommandOutput {
        CommandOutput {
            status: Some(status),
            ..Default::default()
        }
    }

    #[cfg(feature = "tokio")]
    fn block_on<T>(future: impl Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    /// Records the runs it handles and the output passing through it.
    #[derive(Clone, Default)]
    struct Recorder(Arc<Mutex<Vec<String>>>);

    impl Recorder {
        fn events(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    impl ProcessInterceptor for Recorder {
        fn handle(&self, mut next: CommandNext) -> Result<CommandOutput> {
            self.0.lock().unwrap().push(format!(
                "started {} in {}",
                next.run().command_line(),
                next.run().current_dir().unwrap().display()
            ));
            let result = next.next();
            self.0.lock().unwrap().push(format!(
                "finished {}",
                result.as_ref().is_ok_and(|output| output.success)
            ));
            result
        }

        fn output(&self, chunk: crate::CommandChunk) -> Result {
            self.0.lock().unwrap().push(format!(
                "{:?} {}",
                chunk.stream(),
                String::from_utf8_lossy(chunk.data())
            ));
            chunk.next()
        }

        #[cfg(feature = "tokio")]
        fn cancelled(&self, run: &CommandRun) {
            self.0
                .lock()
                .unwrap()
                .push(format!("cancelled {}", run.program()));
        }
    }

    /// Returns a test process whose only interceptor is the returned
    /// recorder.
    fn observed() -> (TestProcess, Recorder) {
        let recorder = Recorder::default();
        let process = TestProcess::new();
        process.clear_interceptors();
        process.add_interceptor(recorder.clone());
        (process, recorder)
    }

    mod test_process {
        use super::*;

        mod calls {
            use super::*;

            #[test]
            fn calls_should_return_command_lines_in_order() {
                let process = TestProcess::new();
                process.set_output("", exit(0));

                Command::new("git").arg("pull").run(&process).unwrap();
                Command::new("git").arg("push").run(&process).unwrap();

                assert_eq!(process.calls(), ["git pull", "git push"]);
            }
        }

        mod call_details {
            use super::*;

            #[test]
            fn call_details_should_return_dir_vars_and_options() {
                // GIVEN platform variable changes
                let process = TestProcess::new();
                process.set_output("", exit(0));
                process.set_var("PLATFORM".to_string(), "1".to_string());
                process.set_var("OVERRIDDEN".to_string(), "1".to_string());
                process.remove_var("REMOVED");

                // WHEN running a command changing variables in a directory
                Command::new("sort")
                    .current_dir("repo")
                    .env("OVERRIDDEN", "2")
                    .env_remove("PLATFORM")
                    .input("b\na")
                    .run(&process)
                    .unwrap();

                // THEN the call has the resolved directory and the changes
                assert_eq!(
                    process.call_details(),
                    [TestCall {
                        command_line: "sort".to_string(),
                        current_dir: PathBuf::from(ROOT).join("repo"),
                        vars: vec![
                            ("REMOVED".to_string(), None),
                            ("OVERRIDDEN".to_string(), Some("2".to_string())),
                            ("PLATFORM".to_string(), None),
                        ],
                        options: CommandOptions {
                            input: Some(b"b\na".to_vec()),
                            ..Default::default()
                        },
                    }]
                );
            }

            #[test]
            fn call_details_clear_env_should_have_only_command_vars() {
                let process = TestProcess::new();
                process.set_output("", exit(0));
                process.set_var("PLATFORM".to_string(), "1".to_string());

                Command::new("git")
                    .env("OWN", "1")
                    .clear_env()
                    .run(&process)
                    .unwrap();

                assert_eq!(
                    process.call_details()[0].vars,
                    [("OWN".to_string(), Some("1".to_string()))]
                );
            }

            #[cfg(windows)]
            #[test]
            fn call_details_should_merge_vars_ignoring_case() {
                let process = TestProcess::new();
                process.set_output("", exit(0));
                process.set_var("Path".to_string(), "platform".to_string());

                Command::new("git")
                    .env("PATH", "own")
                    .run(&process)
                    .unwrap();

                assert_eq!(
                    process.call_details()[0].vars,
                    [("PATH".to_string(), Some("own".to_string()))]
                );
            }
        }

        mod clear_calls {
            use super::*;

            #[test]
            fn clear_calls_should_forget_calls() {
                let process = TestProcess::new();
                process.set_output("", exit(0));
                Command::new("git").run(&process).unwrap();

                process.clear_calls();

                assert!(process.calls().is_empty());
            }
        }

        mod sleeps {
            use super::*;

            /// Returns the command `git` retried twice after 1 s doubled
            /// by the interceptors before `recorder`.
            fn retried(recorder: &Recorder) -> crate::CommandRun {
                let mut interceptors = CommandInterceptors::new();
                interceptors.push(CommandRetry::new(2, Duration::from_secs(1)).backoff(2));
                interceptors.push(recorder.clone());
                Command::new("git").interceptors(interceptors)
            }

            #[test]
            fn sleeps_should_return_delays_in_order() {
                let (process, recorder) = observed();
                process.set_output("git", exit(1));

                let _ = retried(&recorder).run(&process);

                assert_eq!(
                    process.sleeps(),
                    [Duration::from_secs(1), Duration::from_secs(2)]
                );
            }

            #[cfg(feature = "tokio")]
            #[test]
            fn sleeps_should_return_async_delays_in_order() {
                let (process, recorder) = observed();
                process.set_output("git", exit(1));

                let _ = block_on(retried(&recorder).run_async(&process));

                assert_eq!(
                    process.sleeps(),
                    [Duration::from_secs(1), Duration::from_secs(2)]
                );
            }

            #[cfg(feature = "tokio")]
            #[test]
            fn sleeps_cancelled_should_tell_interceptors() {
                // GIVEN an async run waiting to be retried
                let (process, recorder) = observed();
                process.set_output("git", exit(1));
                let mut future = Box::pin(retried(&recorder).run_async(&process));
                let mut context = std::task::Context::from_waker(std::task::Waker::noop());
                assert!(future.as_mut().poll(&mut context).is_pending());

                // WHEN cancelling it
                drop(future);

                // THEN the interceptors are told after the delay started
                assert_eq!(process.sleeps(), [Duration::from_secs(1)]);
                assert_eq!(recorder.events().last().unwrap(), "cancelled git");
            }
        }

        mod set_output {
            use super::*;

            #[test]
            fn set_output_should_prefer_most_recent_matching_pattern() {
                // GIVEN a specific pattern followed by a more general one
                let process = TestProcess::new();
                process.set_output("git push", exit(1));
                process.set_output("git", exit(0));

                // WHEN running a command matching both
                let output = Command::new("git").arg("push").run(&process);

                // THEN the more recent one wins
                assert_eq!(output.unwrap(), exit(0));
            }
        }

        mod set_outputs {
            use super::*;

            #[test]
            fn set_outputs_should_return_outputs_in_order_repeating_last() {
                let process = TestProcess::new();
                process.set_outputs("git", vec![exit(1), exit(2), exit(0)]);

                let statuses: Vec<_> = (0..4)
                    .map(|_| match Command::new("git").run(&process) {
                        Ok(output) => output.status,
                        Err(error) => error.command_output().unwrap().status,
                    })
                    .collect();

                assert_eq!(statuses, [Some(1), Some(2), Some(0), Some(0)]);
            }

            #[test]
            fn set_outputs_empty_should_set_nothing() {
                let process = TestProcess::new();
                process.set_output("", exit(0));

                process.set_outputs("git", Vec::new());

                assert!(Command::new("git").run(&process).is_ok());
            }

            #[test]
            fn set_outputs_with_retries_should_succeed_without_waiting() {
                // GIVEN a command failing twice before it succeeds
                let process = TestProcess::new();
                process.set_outputs("curl", vec![exit(7), exit(7), exit(0)]);
                let start = std::time::Instant::now();

                // WHEN retrying it with a long delay
                process.add_interceptor(CommandRetry::new(2, Duration::from_secs(3600)));
                let output = Command::new("curl").run(&process).unwrap();

                // THEN the last attempt succeeds at once
                assert_eq!(output.status, Some(0));
                assert_eq!(process.calls(), ["curl", "curl", "curl"]);
                assert!(start.elapsed() < Duration::from_secs(60));
            }
        }

        mod set_io_error {
            use super::*;

            #[test]
            fn set_io_error_should_fail_run_with_io_source_naming_program() {
                let process = TestProcess::new();
                process.set_io_error("docker", std::io::ErrorKind::NotFound);

                let error = Command::new("docker")
                    .arg("version")
                    .run(&process)
                    .unwrap_err();

                assert_eq!(error.kind(), crate::ErrorKind::Process);
                assert!(error.description().starts_with("'docker': "));
                assert_eq!(
                    error
                        .source_as::<std::io::Error>()
                        .map(std::io::Error::kind),
                    Some(std::io::ErrorKind::NotFound)
                );
                assert!(error.command_output().is_none());
                assert_eq!(process.calls(), ["docker version"]);
            }

            #[test]
            fn set_io_error_should_override_earlier_matching_output() {
                // GIVEN a default output followed by an I/O error
                let process = TestProcess::new();
                process.set_output("", exit(0));
                process.set_io_error("docker", std::io::ErrorKind::PermissionDenied);

                // WHEN running a command matching both
                let result = Command::new("docker").run(&process);

                // THEN the more recent I/O error wins
                assert_eq!(
                    result
                        .unwrap_err()
                        .source_as::<std::io::Error>()
                        .map(std::io::Error::kind),
                    Some(std::io::ErrorKind::PermissionDenied)
                );
            }
        }

        mod set_pending {
            use super::*;

            #[test]
            fn set_pending_gate_dropped_should_release_run() {
                let process = TestProcess::new();
                let command = Command::new("cargo");

                std::thread::scope(|scope| {
                    let gate = process.set_pending("cargo", exit(0));
                    let running = scope.spawn(|| command.run(&process));
                    let _ = std::iter::repeat_with(std::thread::yield_now)
                        .find(|()| !process.calls().is_empty());

                    drop(gate);

                    assert_eq!(running.join().unwrap().unwrap(), exit(0));
                });
            }

            #[test]
            fn set_pending_should_hold_run_until_released() {
                // GIVEN a pending output
                let process = TestProcess::new();
                let gate = process.set_pending("cargo", exit(0));
                let command = Command::new("cargo");

                std::thread::scope(|scope| {
                    // WHEN running a command matching it
                    let running = scope.spawn(|| command.run(&process));

                    // THEN it is recorded and held until the gate is released
                    let _ = std::iter::repeat_with(std::thread::yield_now)
                        .find(|()| !process.calls().is_empty());
                    assert!(!running.is_finished());
                    gate.release();
                    assert_eq!(running.join().unwrap().unwrap(), exit(0));
                });
            }

            #[test]
            fn set_pending_released_should_not_hold_later_runs() {
                let process = TestProcess::new();
                let gate = process.set_pending("cargo", exit(0));

                gate.release();

                assert_eq!(Command::new("cargo").run(&process).unwrap(), exit(0));
            }

            #[cfg(feature = "tokio")]
            #[test]
            fn set_pending_should_hold_async_run_until_released() {
                // GIVEN an async run matching a pending output
                let process = TestProcess::new();
                let gate = process.set_pending("cargo", exit(0));
                let command = Command::new("cargo");
                let mut future = std::pin::pin!(command.run_async(&process));
                let mut context = std::task::Context::from_waker(std::task::Waker::noop());

                // WHEN polling it before and after releasing the gate
                let held = future.as_mut().poll(&mut context);
                gate.release();
                let result = block_on(future);

                // THEN it is recorded and held until released
                assert!(held.is_pending());
                assert_eq!(process.calls(), ["cargo"]);
                assert_eq!(result.unwrap(), exit(0));
            }

            #[cfg(feature = "tokio")]
            #[test]
            fn set_pending_released_should_not_hold_later_async_runs() {
                let process = TestProcess::new();
                let gate = process.set_pending("cargo", exit(0));

                gate.release();

                assert_eq!(
                    block_on(Command::new("cargo").run_async(&process)).unwrap(),
                    exit(0)
                );
            }

            #[cfg(feature = "tokio")]
            #[test]
            fn set_pending_cancelled_should_tell_interceptors() {
                // GIVEN an async run held by a pending output
                let (process, recorder) = observed();
                let _gate = process.set_pending("cargo", exit(0));
                let command = Command::new("cargo");
                let mut future = Box::pin(command.run_async(&process));
                let mut context = std::task::Context::from_waker(std::task::Waker::noop());
                assert!(future.as_mut().poll(&mut context).is_pending());

                // WHEN cancelling it
                drop(future);

                // THEN the interceptors are told
                assert_eq!(recorder.events(), ["cancelled cargo"]);
            }
        }

        mod process {
            use super::*;

            mod run {
                use super::*;

                #[test]
                fn run_should_return_output_and_record_call() {
                    let process = TestProcess::new();
                    let output = CommandOutput {
                        status: Some(0),
                        stdout: b"out".to_vec(),
                        ..Default::default()
                    };
                    process.set_output("git", output.clone());

                    let result = Command::new("git").arg("status").run(&process);

                    assert_eq!(result.unwrap(), output);
                    assert_eq!(process.calls(), ["git status"]);
                }

                #[test]
                fn run_env_changed_while_retrying_should_keep_env_of_run() {
                    // GIVEN an interceptor changing the env after each attempt
                    struct Changing(SharedEnv);

                    impl ProcessInterceptor for Changing {
                        fn handle(&self, mut next: CommandNext) -> Result<CommandOutput> {
                            let result = next.next();
                            self.0.set_var("A".to_string(), "2".to_string());
                            self.0.replace_current_dir(PathBuf::from("/other"));
                            result
                        }
                    }

                    let process = TestProcess::new();
                    process.set_var("A".to_string(), "1".to_string());
                    process.add_interceptor(CommandRetry::new(1, Duration::ZERO));
                    process.add_interceptor(Changing(process.env.clone()));
                    process.set_outputs("curl", vec![exit(7), exit(0)]);

                    // WHEN running a command retried once
                    Command::new("curl").run(&process).unwrap();

                    // THEN both attempts get the env the run started with
                    let calls = process.call_details();
                    assert_eq!(calls.len(), 2);
                    for call in calls {
                        assert_eq!(call.vars, [("A".to_string(), Some("1".to_string()))]);
                        assert_eq!(call.current_dir, Path::new(ROOT));
                    }
                }

                #[test]
                fn run_unmatched_should_return_error_and_record_call() {
                    let process = TestProcess::new();
                    process.set_output("cargo", exit(0));

                    let error = Command::new("git").run(&process).unwrap_err();

                    assert_eq!(error.description(), "'git': no output set");
                    assert_eq!(process.calls(), ["git"]);
                }

                #[test]
                fn run_invalid_options_should_return_error_without_running() {
                    let process = TestProcess::new();
                    process.set_output("", exit(0));

                    let error = Command::new("git")
                        .interactive()
                        .timeout(Duration::from_secs(1))
                        .run(&process)
                        .unwrap_err();

                    assert_eq!(
                        error.description(),
                        "'git': interactive cannot be combined with timeout"
                    );
                    assert!(process.calls().is_empty());
                }

                #[test]
                fn run_failed_output_should_return_error_with_output() {
                    let process = TestProcess::new();
                    process.set_output("git", exit(1));

                    let error = Command::new("git").run(&process).unwrap_err();

                    assert_eq!(error.command_output(), Some(&exit(1)));
                }

                #[test]
                fn run_accepted_timeout_should_return_output() {
                    struct AcceptTimeout;

                    impl ProcessInterceptor for AcceptTimeout {
                        fn handle(&self, mut next: CommandNext) -> Result<CommandOutput> {
                            next.next().map(|output| CommandOutput {
                                success: output.timed_out,
                                ..output
                            })
                        }
                    }

                    let process = TestProcess::new();
                    process.add_interceptor(AcceptTimeout);
                    process.set_output(
                        "git",
                        CommandOutput {
                            duration: Duration::from_secs(60),
                            ..exit(0)
                        },
                    );

                    let output = Command::new("git")
                        .timeout(Duration::from_secs(1))
                        .run(&process)
                        .unwrap();

                    assert!(output.timed_out);
                    assert_eq!(output.duration, Duration::from_secs(1));
                }

                #[test]
                fn run_duration_exceeding_timeout_should_time_out() {
                    let process = TestProcess::new();
                    process.set_output(
                        "git",
                        CommandOutput {
                            duration: Duration::from_secs(60),
                            ..exit(0)
                        },
                    );

                    let error = Command::new("git")
                        .timeout(Duration::from_secs(1))
                        .run(&process)
                        .unwrap_err();

                    assert_eq!(error.description(), "'git' timed out after 1s");
                    assert_eq!(
                        error.command_output(),
                        Some(&CommandOutput {
                            status: None,
                            duration: Duration::from_secs(1),
                            timed_out: true,
                            ..Default::default()
                        })
                    );
                }

                #[test]
                fn run_display_lines_should_return_output_equal_to_set() {
                    let process = TestProcess::new();
                    let output = CommandOutput {
                        stdout: b"out".to_vec(),
                        ..exit(0)
                    };
                    process.set_output("git", output.clone());

                    let result = Command::new("git").display_lines(3).run(&process).unwrap();

                    assert_eq!(result, output);
                    assert_eq!(result.display_lines, 3);
                }

                #[test]
                fn run_duration_within_timeout_should_return_output() {
                    let process = TestProcess::new();
                    let output = CommandOutput {
                        duration: Duration::from_secs(1),
                        ..exit(0)
                    };
                    process.set_output("git", output.clone());

                    let result = Command::new("git")
                        .timeout(Duration::from_secs(60))
                        .run(&process);

                    assert_eq!(result.unwrap(), output);
                }

                #[test]
                fn run_silent_should_drop_output_set() {
                    // GIVEN a recorder and an output with both streams
                    let (process, recorder) = observed();
                    process.set_output(
                        "git",
                        CommandOutput {
                            stdout: b"out".to_vec(),
                            stderr: b"err".to_vec(),
                            ..exit(0)
                        },
                    );

                    // WHEN streaming a matching command silently
                    let output = Command::new("git").stream().silent().run(&process).unwrap();

                    // THEN no output is returned or streamed
                    assert!(output.stdout.is_empty());
                    assert!(output.stderr.is_empty());
                    assert_eq!(
                        recorder.events(),
                        [
                            format!("started git in {ROOT}"),
                            "finished true".to_string()
                        ]
                    );
                }

                #[test]
                fn run_should_pass_output_set_to_interceptors() {
                    // GIVEN a recorder and an output with both streams
                    let (process, recorder) = observed();
                    process.set_output(
                        "git",
                        CommandOutput {
                            stdout: b"out".to_vec(),
                            stderr: b"err".to_vec(),
                            ..exit(0)
                        },
                    );

                    // WHEN running a matching command
                    Command::new("git").run(&process).unwrap();

                    // THEN the recorder gets the output between start and end
                    assert_eq!(
                        recorder.events(),
                        [
                            format!("started git in {ROOT}"),
                            "Stdout out".to_string(),
                            "Stderr err".to_string(),
                            "finished true".to_string()
                        ]
                    );
                }
            }

            #[cfg(feature = "tokio")]
            mod run_async {
                use super::*;

                #[test]
                fn run_async_should_return_output_and_record_call() {
                    let process = TestProcess::new();
                    process.set_output("git", exit(0));
                    let command = Command::new("git");

                    let result = tokio::runtime::Builder::new_current_thread()
                        .build()
                        .unwrap()
                        .block_on(command.run_async(&process));

                    assert_eq!(result.unwrap(), exit(0));
                    assert_eq!(process.calls(), ["git"]);
                }

                #[test]
                fn run_async_unmatched_should_fail() {
                    let process = TestProcess::new();
                    let command = Command::new("git");

                    let error = block_on(command.run_async(&process)).unwrap_err();

                    assert_eq!(error.description(), "'git': no output set");
                }

                #[test]
                fn run_async_should_retry_without_waiting() {
                    let process = TestProcess::new();
                    process.set_outputs("git", [exit(1), exit(0)]);
                    let command = Command::new("git");

                    process.add_interceptor(CommandRetry::new(1, Duration::from_secs(3600)));

                    let result = block_on(command.run_async(&process));

                    assert_eq!(result.unwrap(), exit(0));
                    assert_eq!(process.calls(), ["git", "git"]);
                }

                #[test]
                fn run_async_invalid_options_should_fail_without_running() {
                    let process = TestProcess::new();
                    let command = Command::new("vim");

                    let error = block_on(command.interactive().input("text").run_async(&process))
                        .unwrap_err();

                    assert_eq!(
                        error.description(),
                        "'vim': interactive cannot be combined with input"
                    );
                    assert!(process.calls().is_empty());
                }

                #[test]
                fn run_async_panicking_interceptor_should_not_tell_cancelled() {
                    // GIVEN a recorder outside an interceptor panicking on the
                    // output of a streamed run
                    struct Panicking;

                    impl ProcessInterceptor for Panicking {
                        fn output(&self, chunk: crate::CommandChunk) -> Result {
                            if chunk.run().options().stream {
                                panic!("interceptor");
                            }

                            chunk.next()
                        }
                    }

                    let (process, recorder) = observed();
                    process.add_interceptor(Panicking);
                    process.set_output(
                        "cargo",
                        CommandOutput {
                            stdout: b"output".to_vec(),
                            ..exit(0)
                        },
                    );
                    let command = Command::new("cargo");
                    block_on(command.run_async(&process)).unwrap();

                    // WHEN running it streamed
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        block_on(command.stream().run_async(&process))
                    }));

                    // THEN the run is not told as cancelled
                    assert!(result.is_err());
                    assert_eq!(recorder.events(), ["Stdout output"]);
                }
            }

            mod run_to {
                use super::*;

                fn output() -> CommandOutput {
                    CommandOutput {
                        status: Some(0),
                        stdout: b"out".to_vec(),
                        stderr: b"err".to_vec(),
                        ..Default::default()
                    }
                }

                #[test]
                fn run_to_should_pass_output_set_to_sink_by_stream() {
                    let process = TestProcess::new();
                    process.set_output("git", output());
                    let mut sunk = Vec::new();

                    let result = Command::new("git").run_to(&process, &mut |stream, data| {
                        sunk.push((stream, data.to_vec()));
                        Ok(())
                    });

                    assert_eq!(
                        result.unwrap(),
                        CommandOutput {
                            stdout: Vec::new(),
                            stderr: Vec::new(),
                            ..output()
                        }
                    );
                    assert_eq!(
                        sunk,
                        [
                            (OutputStream::Stdout, b"out".to_vec()),
                            (OutputStream::Stderr, b"err".to_vec())
                        ]
                    );
                }

                #[test]
                fn run_to_failed_without_output_should_be_retried() {
                    let process = TestProcess::new();
                    process.set_outputs("git", [exit(1), output()]);
                    let mut sunk = Vec::new();

                    process.add_interceptor(CommandRetry::new(1, Duration::from_secs(60)));

                    let result = Command::new("git").run_to(&process, &mut |_, data| {
                        sunk.extend_from_slice(data);
                        Ok(())
                    });

                    assert_eq!(result.unwrap().status, Some(0));
                    assert_eq!(sunk, b"outerr");
                    assert_eq!(process.calls(), ["git", "git"]);
                }

                #[test]
                fn run_to_failing_sink_should_return_its_error() {
                    for failing in [OutputStream::Stdout, OutputStream::Stderr] {
                        let process = TestProcess::new();
                        process.set_output("git", output());
                        let mut streams = Vec::new();

                        let error = Command::new("git")
                            .run_to(&process, &mut |stream, _| {
                                streams.push(stream);
                                if stream == failing {
                                    Err(crate::Error::process("full"))
                                } else {
                                    Ok(())
                                }
                            })
                            .unwrap_err();

                        assert_eq!(error.description(), "full", "{failing:?}");
                        assert_eq!(*streams.last().unwrap(), failing);
                    }
                }
            }

            #[cfg(feature = "tokio")]
            mod run_async_to {
                use super::*;

                #[test]
                fn run_async_to_should_pass_output_set_to_sink() {
                    let process = TestProcess::new();
                    process.set_output(
                        "git",
                        CommandOutput {
                            status: Some(0),
                            stdout: b"out".to_vec(),
                            ..Default::default()
                        },
                    );
                    let mut sunk = Vec::new();

                    let output =
                        block_on(Command::new("git").run_async_to(&process, &mut |_, data| {
                            sunk.extend_from_slice(data);
                            Ok(())
                        }))
                        .unwrap();

                    assert!(output.stdout.is_empty());
                    assert_eq!(sunk, b"out");
                }
            }

            mod interceptors {
                use super::*;

                #[test]
                fn interceptors_should_return_std_stream_by_default() {
                    let process = TestProcess::new();

                    assert_eq!(process.interceptors().len(), 1);
                }
            }

            mod clear_interceptors {
                use super::*;

                #[test]
                fn clear_interceptors_should_remove_interceptors_of_later_runs() {
                    let (process, recorder) = observed();
                    process.set_output("", exit(0));

                    process.clear_interceptors();
                    Command::new("git").run(&process).unwrap();

                    assert!(recorder.events().is_empty());
                }
            }

            mod add_interceptor {
                use super::*;

                #[test]
                fn add_interceptor_should_pass_runs_in_resolved_dir() {
                    let (process, recorder) = observed();

                    let _ = Command::new("git").current_dir("repo").run(&process);

                    assert_eq!(
                        recorder.events(),
                        [
                            format!(
                                "started git in {}",
                                PathBuf::from(ROOT).join("repo").display()
                            ),
                            "finished false".to_string()
                        ]
                    );
                }

                #[test]
                fn add_interceptor_setting_output_should_not_record_call() {
                    let process = TestProcess::new();
                    process.add_interceptor(crate::CommandDryRun);

                    let output = Command::new("kubectl").run(&process).unwrap();

                    assert_eq!(output, exit(0));
                    assert!(process.calls().is_empty());
                }
            }
        }
    }
}
