use std::borrow::Cow;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
#[cfg(feature = "tokio")]
use std::sync::Mutex;
#[cfg(feature = "tokio")]
use std::sync::PoisonError;
use std::time::Duration;
use std::time::Instant;

use crate::Result;
use crate::env::EnvImpl;
#[cfg(feature = "tokio")]
use crate::interceptor::AsyncExecutor;
use crate::interceptor::BlockingExecutor;
use crate::interceptor::BoxFuture;
use crate::interceptor::Cache;
use crate::interceptor::Captured;
use crate::interceptor::Chunk;
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
use crate::utils::Tail;
use crate::utils::conclude;

macro_rules! process_error {
    ($($arg:tt)*) => {
        $crate::Error::process(format!($($arg)*))
    };
}

#[cfg(feature = "tokio")]
mod tokio_process;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix::{configure, exit_signal, wait_with_output};
#[cfg(windows)]
use windows::{configure, exit_signal, wait_with_output};

/// [internal]
///
/// Receives the output of a child process as it arrives, see
/// [`Process::run_to`].
pub(crate) type OutputSink<'a> = dyn FnMut(OutputStream, &[u8]) -> Result + Send + 'a;

/// The output of a child process a chunk is from, see [`Process::run_to`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputStream {
    /// The standard output.
    Stdout,
    /// The standard error.
    Stderr,
}

/// [internal]
///
/// The output of an attempt as it arrives, passed on with `emit`, e.g.
/// through the interceptors to the sink. The first error of `emit` ends
/// the attempt.
pub(crate) struct Outputs<'a> {
    emit: Box<OutputSink<'a>>,
    error: Option<crate::Error>,
}

/// A command run by a [`Process`] with its [`CommandOptions`], the
/// [`Subject`] of the [`Interceptor`]s of the runs of commands.
///
/// Holds a copy of the program, the arguments, the environment variables
/// and the current directory of a [`Command`], which is left unchanged.
/// Usually run as a [`Run`] from [`CommandExt`], passed to the
/// interceptors with the directory the command runs in, which may change
/// it, see [`Self::current_dir`].
///
/// The interceptors of a [`Process`] are [`StdStream`] by default, see
/// [`Process::add_interceptor`], e.g. on
/// [`Platform::process`](crate::Platform::process). A run uses them unless
/// it has its own, see [`Run::interceptors`](Run#method.interceptors). A
/// failed output
/// is returned as `Ok` inside the chain, the methods of [`Process`] return
/// it as an error. The output of a run replayed instead of running its
/// command, e.g. by [`DryRun`](crate::interceptor::DryRun) or a
/// [`CommandCache`], has its success set from its status like for a
/// command that ran, its duration is not compared with the
/// [`CommandOptions::timeout`].
///
/// # Examples
///
/// ```rust
/// use agplatform::interceptor::{BoxFuture, Interceptor, Next};
/// use agplatform::{CommandExt, CommandOutput, Process, ProcessCommand, Result, TestProcess};
/// use std::process::Command;
/// use std::sync::{Arc, Mutex};
///
/// #[derive(Clone, Default)]
/// struct Failures(Arc<Mutex<Vec<String>>>);
///
/// impl Interceptor<ProcessCommand> for Failures {
///     fn handle<'a>(
///         &'a self,
///         mut next: Next<'a, ProcessCommand>,
///     ) -> BoxFuture<'a, Result<CommandOutput>> {
///         Box::pin(async move {
///             let result = next.attempt().await;
///
///             if result.as_ref().is_ok_and(|output| !output.success) {
///                 self.0.lock().unwrap().push(next.run().subject().command_line());
///             }
///
///             result
///         })
///     }
/// }
///
/// let failures = Failures::default();
/// let process = TestProcess::new();
/// process.add_interceptor(failures.clone());
/// process.set_output("git", CommandOutput { status: Some(1), ..Default::default() });
///
/// let _ = Command::new("git").arg("push").run(&process);
///
/// assert_eq!(*failures.0.lock().unwrap(), ["git push"]);
/// ```
pub struct ProcessCommand {
    program: OsString,
    args: Vec<OsString>,
    envs: Vec<(OsString, Option<OsString>)>,
    dir: Option<PathBuf>,
    options: CommandOptions,
    current_dir: Option<PathBuf>,
    /// The variables of the env the command was resolved in as its run
    /// started, `None` while they are those of the current process, none
    /// before.
    vars: Option<Vec<(OsString, OsString)>>,
}

/// Options for [`Process::run`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandOptions {
    /// The number of the last lines of the standard output and error shown
    /// when displaying the [`CommandOutput`], e.g. as the source of an
    /// error, at most as many KiB of each. 10 by default.
    pub display_lines: usize,

    /// Passes only the environment variables set by the command to the
    /// child process, without the configured ones.
    pub clear_env: bool,

    /// Discards the standard output and error of the child process instead
    /// of capturing (and streaming) them, e.g. when they are large or hold
    /// secrets. The [`CommandOutput`] then has only the status and the
    /// duration.
    pub silent: bool,

    /// Writes the bytes to the standard input of the child process and
    /// closes it, instead of a null standard input.
    pub input: Option<Vec<u8>>,

    /// Connects the child process to the standard input, output and error
    /// of the current process, e.g. for prompts or editors. Its output is
    /// therefore not captured and [`CommandOptions::stream`] has no effect.
    ///
    /// Cannot be combined with [`CommandOptions::input`] or
    /// [`CommandOptions::timeout`], as a process group on Unix cannot read from
    /// the terminal, nor with [`CommandOptions::silent`], as it would hide
    /// the prompts.
    pub interactive: bool,

    /// Writes the output of the child process to the standard output and
    /// error of the current process as it arrives, in addition to
    /// capturing it, by [`StdStream`]. Has no effect when the interceptors
    /// of the run do not include it, e.g. after
    /// [`Process::clear_interceptors`] or with its own ones built from
    /// [`Interceptors::new`].
    pub stream: bool,

    /// Kills the child process with its process group on Unix, or its job
    /// on Windows, when it does not finish in time. Descendants that leave
    /// them, e.g. daemons calling `setsid`, keep running.
    ///
    /// On Unix the child process becomes the leader of a new process
    /// group, so it does not receive the signals of the terminal, e.g.
    /// Ctrl-C. On Linux it is therefore killed when the thread that started
    /// it exits, e.g. when the current process is killed. That covers only
    /// the child process, its descendants in the group survive. A
    /// synchronous run blocks that thread until it ends, so the child
    /// outlives the thread only when the current process dies. macOS and the
    /// other Unix systems have no equivalent. On Windows the job of every
    /// child process is killed when the current process dies.
    ///
    /// A [`Duration`] too long for a deadline, e.g. [`Duration::MAX`], does
    /// not limit the run.
    pub timeout: Option<Duration>,
}

/// The output of a finished child process.
///
/// Returned from [`Process::run`] on success and by
/// [`Error::command_output`](crate::Error::command_output) of its error
/// otherwise.
///
/// Outputs are equal when they differ at most in their
/// [`success`](Self::success), which is decided by the run, and their
/// [`display_lines`](Self::display_lines), which only affects how they are
/// displayed.
#[derive(Clone, Debug)]
pub struct CommandOutput {
    /// Whether the run succeeded. Set for each attempt to whether the child
    /// process exited with code `0` in time, then may be changed by the
    /// interceptors, e.g. to accept the exit code `1` of a command
    /// answering "no".
    pub success: bool,

    /// The exit code, `None` when the process was terminated without one,
    /// e.g. by a signal or when it timed out.
    pub status: Option<i32>,

    /// The signal that terminated the process, e.g. `9` when it was killed
    /// (`SIGKILL`) or `11` when it crashed (`SIGSEGV`). Unix only, `None`
    /// on Windows, where a terminated process has an exit code.
    pub signal: Option<i32>,

    /// The captured standard output, all of it however large, see
    /// [`CommandOptions::silent`] to discard it.
    pub stdout: Vec<u8>,

    /// The captured standard error, all of it however large.
    pub stderr: Vec<u8>,

    /// The wall time the process took.
    pub duration: Duration,

    /// Whether the process was killed for exceeding
    /// [`CommandOptions::timeout`].
    pub timed_out: bool,

    /// The number of the last lines of the standard output and error shown
    /// when displayed, see [`CommandOptions::display_lines`]. 10 by
    /// default.
    pub display_lines: usize,
}

/// The [`Cache`] of the runs of commands by their
/// [`ProcessCommand::cache_key`]. The outputs of
/// [`CommandOptions::interactive`] runs are not cached, nor those the
/// [`Cache`] does not cache, e.g. passed to a sink.
///
/// The variables of the platform are not part of the key unless named
/// with [`with_vars`](Cache::with_vars), so that unrelated ones, e.g. the
/// id of a CI job, do not change it.
///
/// # Examples
///
/// ```rust
/// use agplatform::{CommandCache, CommandExt, CommandOutput, Process, TestProcess};
/// use std::process::Command;
///
/// let cache = CommandCache::default();
/// let process = TestProcess::new();
/// process.add_interceptor(cache.clone());
/// process.set_output("git", CommandOutput { stdout: b"abc".to_vec(), ..Default::default() });
///
/// Command::new("git").args(["rev-parse", "HEAD"]).run(&process).unwrap();
/// let output = Command::new("git").args(["rev-parse", "HEAD"]).run(&process).unwrap();
///
/// assert_eq!(output.stdout, b"abc");
/// assert_eq!(process.calls().len(), 1);
/// assert_eq!(cache.entries().len(), 1);
/// ```
pub type CommandCache = Cache<ProcessCommand>;

/// The [`Interceptor`] writing the output of the runs with
/// [`CommandOptions::stream`] to the standard output and error of the
/// current process, the only interceptor of a process by default.
#[derive(Clone, Copy, Debug, Default)]
pub struct StdStream;

/// Abstract process interface.
///
/// Runs child processes described by [`std::process::Command`] to
/// completion. Child processes run in the configured current directory
/// unless the command sets an absolute one, a relative one is resolved
/// against it. They get the configured environment variables, updated by
/// the variables the command sets or removes. Each run passes through
/// [`Interceptor`]s, see [`ProcessCommand`].
///
/// Usually used through [`CommandExt`] rather than directly.
///
/// The two provided implementations are `ProcessImpl` (delegates to
/// [`std::process`]) and
/// [`TestProcess`](crate::test_platform::test_process::TestProcess)
/// (purely in-memory, behind the `testing` feature).
///
/// Processes are [`Sync`] so that the futures of
/// [`run_async`](Self::run_async) borrowing them are [`Send`].
pub trait Process: Send + Sync {
    /// Runs `run`, a command with its [`CommandOptions`], to completion and
    /// returns its output. A plain [`Command`] runs with the default
    /// options, a [`Run`] (e.g. from [`CommandExt`]) with its own.
    ///
    /// Only the program, the arguments, the environment variables and the
    /// current directory are taken from `command`, which is left
    /// unchanged. Other settings such as
    /// [`env_clear`](Command::env_clear) (use [`CommandOptions::clear_env`]
    /// instead) or the platform specific ones are not observable and
    /// therefore ignored. The standard input of the
    /// child process is null (or [`CommandOptions::input`]), its standard
    /// output and error are captured (and streamed with
    /// [`CommandOptions::stream`]), unless it is [`CommandOptions::interactive`].
    ///
    /// A program given by a relative path with a separator, e.g.
    /// `./tool.sh`, is resolved against the directory the command runs in.
    /// On Unix a bare program name is looked up in the `PATH` the child
    /// process would get without [`CommandOptions::clear_env`], unless set
    /// by the command, so changing it or clearing the environment does not
    /// prevent finding programs.
    ///
    /// The run ends when the child process exits. Its output is read until
    /// it closes, or at most 100 ms after the exit if descendants,
    /// e.g. daemons, hold it open. They keep running and the rest of their
    /// output is dropped.
    ///
    /// # Errors
    ///
    /// The errors name only the program of `command`, as its arguments may
    /// hold secrets. An [`Interceptor`] gets the whole command, e.g. to log
    /// it redacted.
    ///
    /// Returns an error if:
    /// - [`CommandOptions::interactive`] is combined with
    ///   [`CommandOptions::input`] or [`CommandOptions::timeout`].
    /// - The process cannot be started.
    /// - The output is not a [success](CommandOutput::success), e.g. the
    ///   process exits with a non-zero code, is terminated without an exit
    ///   code or times out. The [`CommandOutput`] is returned by
    ///   [`Error::command_output`](crate::Error::command_output).
    /// - An interceptor returns an error.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{CommandExt, CommandOptions, CommandOutput, Platform, Process, ProcessCommand};
    /// use std::process::Command;
    /// use std::time::Duration;
    ///
    /// let platform = agplatform::platform();
    /// let process = platform.process();
    /// # let process = agplatform::TestProcess::new();
    /// # process.set_output("git", CommandOutput { stdout: b"git version 2.0".to_vec(), ..Default::default() });
    ///
    /// let output = process.run(Command::new("git").arg("--version")).unwrap();
    /// assert!(output.stdout_str().unwrap().starts_with("git version"));
    ///
    /// let mut fetch = Command::new("git");
    /// fetch.arg("fetch");
    /// process.run(fetch.timeout(Duration::from_secs(60))).unwrap();
    ///
    /// let options = CommandOptions { silent: true, ..Default::default() };
    /// process.run(ProcessCommand::with_options(&fetch, options)).unwrap();
    /// ```
    fn run(&self, run: impl Into<Run<ProcessCommand>>) -> Result<CommandOutput>;

    /// Runs `run` like [`Self::run`], passing the standard output and error
    /// to `sink` as they arrive, each chunk with the [`OutputStream`] it is
    /// from, instead of capturing them, e.g. for output too large to hold
    /// in memory. The [`CommandOutput`] then has no output, nor does the
    /// error of a failed run show any. The chunks pass through the
    /// interceptors before, see [`Interceptor::chunk`].
    ///
    /// The `sink` gets the output whether the run succeeds or not, which is
    /// known only once it ends. The chunks are as read, not split into
    /// lines. An attempt failing after a chunk was passed to `sink` is not
    /// retried by [`Retry`](crate::interceptor::Retry), as `sink` would get
    /// the output again from its start, see [`Run::streamed`].
    ///
    /// # Errors
    ///
    /// As [`Self::run`], or the error returned by `sink`, which ends the
    /// attempt, killing the child process like [`CommandOptions::timeout`].
    /// Fails as well if the run is [`CommandOptions::interactive`] or
    /// [`CommandOptions::silent`], which leave no output to pass.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{CommandOutput, OutputStream, Platform, Process};
    /// use std::process::Command;
    ///
    /// let platform = agplatform::platform();
    /// let process = platform.process();
    /// # let process = agplatform::TestProcess::new();
    /// # process.set_output("git", CommandOutput { stdout: b"archive".to_vec(), ..Default::default() });
    /// let mut archive = Vec::new();
    ///
    /// let output = process
    ///     .run_to(Command::new("git").args(["archive", "HEAD"]), &mut |stream, chunk| {
    ///         if stream == OutputStream::Stdout {
    ///             archive.extend_from_slice(chunk);
    ///         }
    ///         Ok(())
    ///     })
    ///     .unwrap();
    /// assert!(output.stdout.is_empty());
    /// assert_eq!(archive, b"archive");
    /// ```
    fn run_to(
        &self,
        run: impl Into<Run<ProcessCommand>>,
        sink: &mut (dyn FnMut(OutputStream, &[u8]) -> Result + Send),
    ) -> Result<CommandOutput>;

    /// Enabled by the `tokio` feature flag.
    ///
    /// Runs `run` to completion like [`Self::run`], without blocking the
    /// current thread. Must be awaited
    /// on a [`tokio`] runtime with I/O and time enabled.
    ///
    /// Dropping the future before it completes, e.g. to cancel the run,
    /// kills the child process with its process group on Unix, or its job
    /// on Windows, like [`CommandOptions::timeout`]. Therefore on Unix the
    /// child process always leads a new process group, unless it is
    /// [`CommandOptions::interactive`], when only the child process is
    /// killed. The interceptors are told by [`Interceptor::cancelled`].
    ///
    /// A child process leading a process group is also killed when the
    /// current process dies, see [`CommandOptions::timeout`]. It is started
    /// on the thread polling the future, a runtime worker or the one
    /// blocking on it, which outlives the run unless the runtime shuts down,
    /// cancelling the run anyway. The exception on Linux is a worker that
    /// calls `tokio::task::block_in_place`: it is replaced and continues as
    /// a blocking thread, which exits once idle (after 10 s by default),
    /// killing the children started on it that still run.
    ///
    /// # Errors
    ///
    /// As [`Self::run`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{CommandOutput, Platform, Process};
    /// use std::process::Command;
    ///
    /// # tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
    /// let platform = agplatform::platform();
    /// let process = platform.process();
    /// # let process = agplatform::TestProcess::new();
    /// # process.set_output("git", CommandOutput { stdout: b"git version 2.0".to_vec(), ..Default::default() });
    ///
    /// let output = process.run_async(Command::new("git").arg("--version")).await.unwrap();
    /// assert!(output.stdout_str().unwrap().starts_with("git version"));
    /// # });
    /// ```
    #[cfg(feature = "tokio")]
    fn run_async(
        &self,
        run: impl Into<Run<ProcessCommand>>,
    ) -> impl Future<Output = Result<CommandOutput>> + Send;

    /// Enabled by the `tokio` feature flag.
    ///
    /// Runs `run` like [`Self::run_async`], passing its output to `sink`
    /// like [`Self::run_to`].
    ///
    /// # Errors
    ///
    /// As [`Self::run_to`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{CommandOutput, Platform, Process};
    /// use std::process::Command;
    ///
    /// # tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
    /// let platform = agplatform::platform();
    /// let process = platform.process();
    /// # let process = agplatform::TestProcess::new();
    /// # process.set_output("cargo", CommandOutput { stderr: b"Compiling".to_vec(), ..Default::default() });
    /// let mut log = Vec::new();
    ///
    /// process
    ///     .run_async_to(Command::new("cargo").arg("build"), &mut |_, chunk| {
    ///         log.extend_from_slice(chunk);
    ///         Ok(())
    ///     })
    ///     .await
    ///     .unwrap();
    /// assert_eq!(log, b"Compiling");
    /// # });
    /// ```
    #[cfg(feature = "tokio")]
    fn run_async_to(
        &self,
        run: impl Into<Run<ProcessCommand>>,
        sink: &mut (dyn FnMut(OutputStream, &[u8]) -> Result + Send),
    ) -> impl Future<Output = Result<CommandOutput>> + Send;

    /// Returns the interceptors of the runs, e.g. to set them with changes
    /// on a run, see [`Run::interceptors`](Run#method.interceptors).
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::interceptor::Retry;
    /// use agplatform::{CommandExt, CommandOutput, Process, TestProcess};
    /// use std::process::Command;
    /// use std::time::Duration;
    ///
    /// let process = TestProcess::new();
    /// let failure = CommandOutput { status: Some(1), ..Default::default() };
    /// process.set_outputs("git", [failure, CommandOutput::default()]);
    ///
    /// let mut interceptors = process.interceptors();
    /// interceptors.insert(0, Retry::new(3, Duration::from_secs(1)));
    /// Command::new("git").arg("fetch").interceptors(interceptors).run(&process).unwrap();
    ///
    /// assert_eq!(process.calls(), ["git fetch", "git fetch"]);
    /// ```
    fn interceptors(&self) -> Interceptors<ProcessCommand>;

    /// Removes the interceptors, [`StdStream`] by default, for the runs
    /// starting afterwards, e.g. before adding others with
    /// [`Self::add_interceptor`]. Without [`StdStream`]
    /// [`CommandOptions::stream`] has no effect.
    fn clear_interceptors(&self);

    /// Adds `interceptor` as the innermost one for the runs starting
    /// afterwards. Its state to be read later, e.g. what it recorded, is
    /// shared through an [`Arc`](std::sync::Arc) it holds.
    ///
    /// See [`ProcessCommand`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::interceptor::{DryRun, Retry};
    /// use agplatform::{CommandExt, CommandOutput, DryRunOutput, Process, TestProcess};
    /// use std::process::Command;
    /// use std::time::Duration;
    ///
    /// let process = TestProcess::new();
    /// process.add_interceptor(Retry::new(3, Duration::from_secs(5)).when(|_, result| {
    ///     result
    ///         .as_ref()
    ///         .is_ok_and(|output: &CommandOutput| output.stderr_lossy().contains("Could not resolve host"))
    /// }));
    /// process.add_interceptor(DryRun);
    ///
    /// let applied = Command::new("kubectl").arg("apply").run(&process).unwrap();
    /// let pods = Command::new("kubectl")
    ///     .args(["get", "pods"])
    ///     .with(DryRunOutput(CommandOutput { stdout: b"pod-1".to_vec(), ..Default::default() }))
    ///     .run(&process)
    ///     .unwrap();
    ///
    /// assert!(applied.stdout.is_empty());
    /// assert_eq!(pods.stdout, b"pod-1");
    /// assert!(process.calls().is_empty());
    /// ```
    fn add_interceptor(&self, interceptor: impl Interceptor<ProcessCommand>);
}

/// Extends [`std::process::Command`] with the ability to run it through a
/// [`Process`].
///
/// # Examples
///
/// ```rust
/// use agplatform::{CommandExt, CommandOutput, Platform};
/// use std::process::Command;
/// use std::time::Duration;
///
/// let platform = agplatform::platform();
/// let process = platform.process();
/// # let process = &agplatform::TestProcess::new();
/// # process.set_output("", CommandOutput::default());
///
/// Command::new("git").arg("status").run(process).unwrap();
/// Command::new("cargo")
///     .arg("build")
///     .stream()
///     .timeout(Duration::from_secs(600))
///     .run(process)
///     .unwrap();
/// ```
pub trait CommandExt {
    /// Runs the command with the default [`CommandOptions`].
    ///
    /// See [`Process::run`].
    fn run<P: Process + ?Sized>(&self, process: &P) -> Result<CommandOutput>;

    /// Runs the command with the default [`CommandOptions`], passing its
    /// output to `sink`.
    ///
    /// See [`Process::run_to`].
    fn run_to<P: Process + ?Sized>(
        &self,
        process: &P,
        sink: &mut (dyn FnMut(OutputStream, &[u8]) -> Result + Send),
    ) -> Result<CommandOutput>;

    /// Enabled by the `tokio` feature flag.
    ///
    /// Runs the command asynchronously with the default [`CommandOptions`].
    ///
    /// See [`Process::run_async`].
    #[cfg(feature = "tokio")]
    fn run_async<P: Process + ?Sized>(
        &self,
        process: &P,
    ) -> impl Future<Output = Result<CommandOutput>> + Send;

    /// Enabled by the `tokio` feature flag.
    ///
    /// Runs the command asynchronously with the default [`CommandOptions`],
    /// passing its output to `sink`.
    ///
    /// See [`Process::run_async_to`].
    #[cfg(feature = "tokio")]
    fn run_async_to<P: Process + ?Sized>(
        &self,
        process: &P,
        sink: &mut (dyn FnMut(OutputStream, &[u8]) -> Result + Send),
    ) -> impl Future<Output = Result<CommandOutput>> + Send;

    /// Returns a run of the command without the configured environment
    /// variables.
    ///
    /// See [`CommandOptions::clear_env`].
    fn clear_env(&self) -> Run<ProcessCommand>;

    /// Returns a run of the command whose output shows its last `lines`
    /// when displayed.
    ///
    /// See [`CommandOptions::display_lines`].
    fn display_lines(&self, lines: usize) -> Run<ProcessCommand>;

    /// Returns a run of the command whose output is discarded.
    ///
    /// See [`CommandOptions::silent`].
    fn silent(&self) -> Run<ProcessCommand>;

    /// Returns a run of the command reading `input`.
    ///
    /// See [`CommandOptions::input`].
    fn input<T: Into<Vec<u8>>>(&self, input: T) -> Run<ProcessCommand>;

    /// Returns a run of the command connected to the terminal.
    ///
    /// See [`CommandOptions::interactive`].
    fn interactive(&self) -> Run<ProcessCommand>;

    /// Returns a run of the command that streams its output.
    ///
    /// See [`CommandOptions::stream`].
    fn stream(&self) -> Run<ProcessCommand>;

    /// Returns a run of the command limited to `timeout`.
    ///
    /// See [`CommandOptions::timeout`].
    fn timeout(&self, timeout: Duration) -> Run<ProcessCommand>;

    /// Returns a run of the command with its own `interceptors`.
    ///
    /// See [`Run::interceptors`](Run#method.interceptors).
    fn interceptors(&self, interceptors: Interceptors<ProcessCommand>) -> Run<ProcessCommand>;

    /// Returns a run of the command with `value` for the interceptors.
    ///
    /// See [`Run::with`].
    fn with<T: std::any::Any + Send + Sync>(&self, value: T) -> Run<ProcessCommand>;
}

/// [`Process`] implementation backed by [`std::process`].
///
/// Child processes run in the configured current directory with the
/// configured environment variables.
///
/// Normally obtained via [`platform()`](crate::platform) rather than
/// constructed directly.
pub struct ProcessImpl {
    env: EnvImpl,
    runs: Runs<ProcessCommand>,
}

/// [internal]
///
/// The [`Executor`] of the blocking runs of a [`ProcessImpl`].
struct StdExecutor<'a>(&'a ProcessImpl);

/// [internal]
///
/// The [`Executor`] of the async runs of a [`ProcessImpl`] on [`tokio`].
#[cfg(feature = "tokio")]
struct TokioExecutor<'a>(&'a ProcessImpl);

impl ProcessCommand {
    /// Creates the command of `command` with the default
    /// [`CommandOptions`].
    pub fn new(command: &Command) -> Self {
        Self::with_options(command, CommandOptions::default())
    }

    /// Creates the command of `command` with `options`, e.g. read from a
    /// configuration.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{CommandOptions, ProcessCommand};
    /// use std::process::Command;
    ///
    /// let options = CommandOptions { stream: true, ..Default::default() };
    /// let command = ProcessCommand::with_options(&Command::new("git"), options);
    ///
    /// assert!(command.options().stream);
    /// ```
    pub fn with_options(command: &Command, options: CommandOptions) -> Self {
        Self {
            program: command.get_program().to_owned(),
            args: command.get_args().map(OsStr::to_owned).collect(),
            envs: command
                .get_envs()
                .map(|(key, value)| (key.to_owned(), value.map(OsStr::to_owned)))
                .collect(),
            dir: command.get_current_dir().map(Path::to_path_buf),
            options,
            current_dir: None,
            vars: Some(Vec::new()),
        }
    }

    /// Returns the arguments.
    pub fn args(&self) -> &[OsString] {
        &self.args
    }

    /// Returns the arguments to be changed, e.g. by an interceptor.
    pub fn args_mut(&mut self) -> &mut Vec<OsString> {
        &mut self.args
    }

    /// Returns the key of the command for a [`Cache`]: the directory, the
    /// program and the arguments, the environment variables set or removed
    /// by the command and the platform variables `vars` (unless
    /// [`CommandOptions::clear_env`]), the options changing the output and
    /// the input, each quoted or escaped so that different commands have
    /// different keys. `None` for an [`CommandOptions::interactive`]
    /// command, whose output is not captured.
    ///
    /// The variables are named with whether they are set, their values are
    /// only hashed together (FNV-1a, the same on every Rust version), as
    /// they may hold secrets, e.g. tokens, that the persisted
    /// [`entries`](Cache::entries) must not show.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::interceptor::{Cache, Run};
    /// use agplatform::{CommandExt, CommandOutput, Process, ProcessCommand, TestProcess};
    /// use std::process::Command;
    ///
    /// let cache = Cache::new(|run: &Run<ProcessCommand>| {
    ///     let command = run.subject();
    ///     (command.program() == "git").then(|| command.cache_key(&[])).flatten()
    /// });
    /// let process = TestProcess::new();
    /// process.add_interceptor(cache.clone());
    /// process.set_output("", CommandOutput::default());
    ///
    /// Command::new("git").arg("log").run(&process).unwrap();
    /// Command::new("date").run(&process).unwrap();
    ///
    /// assert_eq!(cache.entries().len(), 1);
    /// ```
    pub fn cache_key(&self, vars: &[OsString]) -> Option<String> {
        let options = self.options();

        if options.interactive {
            return None;
        }

        let vars: Vec<(&OsStr, Option<OsString>)> = if options.clear_env {
            Vec::new()
        } else {
            vars.iter()
                .map(|name| (name.as_os_str(), self.var(name)))
                .collect()
        };
        fn names<'a>(
            vars: impl Iterator<Item = (&'a OsStr, bool)>,
            unset: &'static str,
        ) -> Vec<(&'a OsStr, &'static str)> {
            vars.map(|(name, set)| (name, if set { "set" } else { unset }))
                .collect()
        }

        let values = crate::utils::fnv1a(
            self.envs()
                .iter()
                .map(|(_, value)| value.as_deref())
                .chain(vars.iter().map(|(_, value)| value.as_deref()))
                .map(|value| value.map(OsStr::as_encoded_bytes)),
        );
        let mut key = format!(
            "dir: {:?}\ncommand: {:?}\nenvs: {:?}\nclear_env: {}\nsilent: {}",
            self.current_dir().unwrap_or(Path::new("")),
            std::iter::once(&self.program)
                .chain(&self.args)
                .collect::<Vec<_>>(),
            names(
                self.envs()
                    .iter()
                    .map(|(name, value)| (name.as_os_str(), value.is_some())),
                "removed"
            ),
            options.clear_env,
            options.silent,
        );

        if !options.clear_env {
            key.push_str(&format!(
                "\nvars: {:?}",
                names(
                    vars.iter().map(|(name, value)| (*name, value.is_some())),
                    "unset"
                )
            ));
        }

        key.push_str(&format!("\nvalues: {values:016x}"));

        if let Some(input) = &options.input {
            key.push_str(&format!("\ninput: {}", input.escape_ascii()));
        }

        Some(key)
    }

    /// Returns the program and the arguments separated by spaces.
    pub fn command_line(&self) -> String {
        std::iter::once(&self.program)
            .chain(&self.args)
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Returns the directory the command runs in, i.e. its own resolved
    /// against the current directory of the process, once its run started,
    /// e.g. when passed to an interceptor. `None` before.
    pub fn current_dir(&self) -> Option<&Path> {
        self.current_dir.as_deref()
    }

    /// Returns the environment variables set (`Some`) or removed (`None`) by
    /// the command, in the order they were set.
    pub fn envs(&self) -> &[(OsString, Option<OsString>)] {
        &self.envs
    }

    /// Returns the options.
    pub fn options(&self) -> &CommandOptions {
        &self.options
    }

    /// Returns the options to be changed, e.g. by an interceptor.
    pub fn options_mut(&mut self) -> &mut CommandOptions {
        &mut self.options
    }

    /// Returns the program, lossily converted to UTF-8.
    pub fn program(&self) -> Cow<'_, str> {
        self.program.to_string_lossy()
    }

    /// Removes the environment variable `key` for the command, e.g. in an
    /// interceptor.
    pub fn remove_env<K: AsRef<OsStr>>(&mut self, key: K) {
        self.change_env(key.as_ref(), None);
    }

    /// Returns the [`Retry`] of the failed attempts of the runs [safe to
    /// retry](Self::safe_to_retry), up to `retries` times after `delay`.
    /// Its [`when`](Retry::when) decides instead of the safety, e.g. to
    /// retry also a timeout. Each attempt is limited by the
    /// [`CommandOptions::timeout`] on its own.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{CommandExt, CommandOutput, Process, ProcessCommand, TestProcess};
    /// use std::process::Command;
    /// use std::time::Duration;
    ///
    /// let process = TestProcess::new();
    /// process.add_interceptor(ProcessCommand::retry(3, Duration::from_secs(5)).when(|run, result| {
    ///     run.subject().safe_to_retry(result)
    ///         && result.as_ref().is_ok_and(|output| output.stderr_lossy().contains("Could not resolve host"))
    /// }));
    /// process.set_output("git push", CommandOutput { status: Some(1), ..Default::default() });
    /// process.set_io_error("svn", std::io::ErrorKind::NotFound);
    ///
    /// assert!(Command::new("git").arg("push").run(&process).is_err());
    /// assert!(Command::new("svn").run(&process).is_err());
    /// assert_eq!(process.calls(), ["git push", "svn"]);
    /// ```
    pub fn retry(retries: u32, delay: Duration) -> Retry<ProcessCommand> {
        Retry::<Self>::new(retries, delay).when(|run, result| run.subject().safe_to_retry(result))
    }

    /// Returns `true` if the failed `result` of an attempt of the command
    /// may pass when retried: an output that is not a success, e.g. of a
    /// non-zero exit code or a signal, unless it timed out, which would
    /// take as long again. Not an error, e.g. of a program not found, which
    /// would fail the same way. Used by [`Self::retry`].
    pub fn safe_to_retry(&self, result: &Result<CommandOutput>) -> bool {
        let _ = self;
        result
            .as_ref()
            .is_ok_and(|output| !output.success && !output.timed_out)
    }

    /// Sets the environment variable `key` to `value` for the command, e.g.
    /// in an interceptor.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::interceptor::{BoxFuture, Interceptor, Next};
    /// use agplatform::{CommandOutput, ProcessCommand, Result};
    ///
    /// struct Token;
    ///
    /// impl Interceptor<ProcessCommand> for Token {
    ///     fn handle<'a>(
    ///         &'a self,
    ///         mut next: Next<'a, ProcessCommand>,
    ///     ) -> BoxFuture<'a, Result<CommandOutput>> {
    ///         next.run_mut().subject_mut().set_env("TOKEN", "secret");
    ///         next.forward()
    ///     }
    /// }
    /// ```
    pub fn set_env<K: AsRef<OsStr>, V: AsRef<OsStr>>(&mut self, key: K, value: V) {
        self.change_env(key.as_ref(), Some(value.as_ref().to_owned()));
    }

    /// Sets the program, e.g. in an interceptor.
    pub fn set_program<T: Into<OsString>>(&mut self, program: T) {
        self.program = program.into();
    }

    /// [internal]
    ///
    /// Resolves the directory of the command against the current directory
    /// of `env` and takes its variables, as the run starts.
    ///
    /// WHY: taken once so that all attempts of the run get the same env,
    /// even when it is changed in between.
    pub(crate) fn resolve(&mut self, env: &EnvImpl) {
        let current_dir = env.current_dir_arc();
        self.current_dir = Some(
            self.dir
                .as_ref()
                .map_or(current_dir.to_path_buf(), |dir| current_dir.join(dir)),
        );
        self.vars = env.changed_vars();
    }

    /// [internal]
    ///
    /// Returns the variables of the env the command was resolved in, `None`
    /// while they are those of the current process.
    pub(crate) fn vars(&self) -> Option<&[(OsString, OsString)]> {
        self.vars.as_deref()
    }

    /// [internal]
    ///
    /// Returns the variable `key` of the env the command was resolved in.
    pub(crate) fn var(&self, key: &OsStr) -> Option<OsString> {
        match &self.vars {
            Some(vars) => vars
                .iter()
                .find(|(k, _)| crate::env::is_same_key(k, key))
                .map(|(_, value)| value.clone()),
            None => std::env::var_os(key),
        }
    }

    /// Records the change of the variable `key` to `value`, replacing an
    /// earlier one.
    ///
    /// WHY: the keys are compared like the platform does, case-insensitively
    /// on Windows.
    fn change_env(&mut self, key: &OsStr, value: Option<OsString>) {
        #[cfg(windows)]
        self.envs.retain(|(k, _)| !k.eq_ignore_ascii_case(key));
        #[cfg(not(windows))]
        self.envs.retain(|(k, _)| k != key);
        self.envs.push((key.to_owned(), value));
    }
}

impl Subject for ProcessCommand {
    type Output = CommandOutput;
    type Stream = OutputStream;

    /// Rejects the combinations of the options that cannot work, also with
    /// a sink, see [`Process::run`] and [`Process::run_to`].
    fn validate(run: &Run<Self>) -> Result {
        validate(
            &run.subject().program(),
            run.subject().options(),
            run.has_sink(),
        )
    }

    /// Passes the standard output, then the standard error of `output` on,
    /// each in one chunk unless empty, none when the run is
    /// [`CommandOptions::silent`] or [`CommandOptions::interactive`], which
    /// capture no output.
    fn replay<'a>(
        run: &'a Run<Self>,
        output: &'a CommandOutput,
        emit: &'a mut Emit<'_, Self>,
    ) -> BoxFuture<'a, Result> {
        Box::pin(async move {
            let options = run.subject().options();

            if !options.silent && !options.interactive {
                for (stream, data) in [
                    (OutputStream::Stdout, &output.stdout),
                    (OutputStream::Stderr, &output.stderr),
                ] {
                    if !data.is_empty() {
                        emit.emit(stream, data).await?;
                    }
                }
            }

            Ok(())
        })
    }

    /// Returns `output` successful if it exited with code `0` in time, with
    /// the standard output and error `captured` and the
    /// [`CommandOptions::display_lines`] of the run.
    fn finish(
        run: &Run<Self>,
        output: CommandOutput,
        mut captured: Captured<Self>,
    ) -> CommandOutput {
        CommandOutput {
            success: output.status == Some(0) && !output.timed_out,
            stdout: captured.take(OutputStream::Stdout),
            stderr: captured.take(OutputStream::Stderr),
            display_lines: run.subject().options().display_lines,
            ..output
        }
    }
}

impl Run<ProcessCommand> {
    /// Passes only the environment variables set by the command.
    ///
    /// See [`CommandOptions::clear_env`].
    pub fn clear_env(mut self) -> Self {
        self.subject_mut().options.clear_env = true;
        self
    }

    /// Shows the last `lines` of the output when displayed.
    ///
    /// See [`CommandOptions::display_lines`].
    pub fn display_lines(mut self, lines: usize) -> Self {
        self.subject_mut().options.display_lines = lines;
        self
    }

    /// Discards the output of the command.
    ///
    /// See [`CommandOptions::silent`].
    pub fn silent(mut self) -> Self {
        self.subject_mut().options.silent = true;
        self
    }

    /// Writes `input` to the standard input of the command.
    ///
    /// See [`CommandOptions::input`].
    pub fn input<T: Into<Vec<u8>>>(mut self, input: T) -> Self {
        self.subject_mut().options.input = Some(input.into());
        self
    }

    /// Connects the command to the terminal.
    ///
    /// See [`CommandOptions::interactive`].
    pub fn interactive(mut self) -> Self {
        self.subject_mut().options.interactive = true;
        self
    }

    /// Streams the output of the command.
    ///
    /// See [`CommandOptions::stream`].
    pub fn stream(mut self) -> Self {
        self.subject_mut().options.stream = true;
        self
    }

    /// Limits the command to `timeout`.
    ///
    /// See [`CommandOptions::timeout`].
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.subject_mut().options.timeout = Some(timeout);
        self
    }

    /// Runs the command with its own `interceptors` instead of those of the
    /// process, see [`Process::interceptors`]. They are taken once as the
    /// run starts, so the interceptors cannot change them.
    pub fn interceptors(mut self, interceptors: Interceptors<ProcessCommand>) -> Self {
        self.insert(OwnInterceptors(interceptors));
        self
    }

    /// Runs the command with the options.
    ///
    /// See [`Process::run`].
    pub fn run<P: Process + ?Sized>(self, process: &P) -> Result<CommandOutput> {
        process.run(self)
    }

    /// Runs the command with the options, passing its output to `sink`.
    ///
    /// See [`Process::run_to`].
    pub fn run_to<P: Process + ?Sized>(
        self,
        process: &P,
        sink: &mut (dyn FnMut(OutputStream, &[u8]) -> Result + Send),
    ) -> Result<CommandOutput> {
        process.run_to(self, sink)
    }

    /// Enabled by the `tokio` feature flag.
    ///
    /// Runs the command asynchronously with the options.
    ///
    /// See [`Process::run_async`].
    #[cfg(feature = "tokio")]
    pub fn run_async<P: Process + ?Sized>(
        self,
        process: &P,
    ) -> impl Future<Output = Result<CommandOutput>> + Send {
        process.run_async(self)
    }

    /// Enabled by the `tokio` feature flag.
    ///
    /// Runs the command asynchronously with the options, passing its output
    /// to `sink`.
    ///
    /// See [`Process::run_async_to`].
    #[cfg(feature = "tokio")]
    pub fn run_async_to<P: Process + ?Sized>(
        self,
        process: &P,
        sink: &mut (dyn FnMut(OutputStream, &[u8]) -> Result + Send),
    ) -> impl Future<Output = Result<CommandOutput>> + Send {
        process.run_async_to(self, sink)
    }
}

impl From<&Command> for Run<ProcessCommand> {
    fn from(command: &Command) -> Self {
        Self::new(ProcessCommand::new(command))
    }
}

impl From<&mut Command> for Run<ProcessCommand> {
    fn from(command: &mut Command) -> Self {
        Self::new(ProcessCommand::new(command))
    }
}

impl From<ProcessCommand> for Run<ProcessCommand> {
    fn from(command: ProcessCommand) -> Self {
        Self::new(command)
    }
}

impl CommandExt for Command {
    fn run<P: Process + ?Sized>(&self, process: &P) -> Result<CommandOutput> {
        process.run(self)
    }

    fn run_to<P: Process + ?Sized>(
        &self,
        process: &P,
        sink: &mut (dyn FnMut(OutputStream, &[u8]) -> Result + Send),
    ) -> Result<CommandOutput> {
        process.run_to(self, sink)
    }

    #[cfg(feature = "tokio")]
    fn run_async<P: Process + ?Sized>(
        &self,
        process: &P,
    ) -> impl Future<Output = Result<CommandOutput>> + Send {
        process.run_async(self)
    }

    #[cfg(feature = "tokio")]
    fn run_async_to<P: Process + ?Sized>(
        &self,
        process: &P,
        sink: &mut (dyn FnMut(OutputStream, &[u8]) -> Result + Send),
    ) -> impl Future<Output = Result<CommandOutput>> + Send {
        process.run_async_to(self, sink)
    }

    fn clear_env(&self) -> Run<ProcessCommand> {
        Run::from(self).clear_env()
    }

    fn display_lines(&self, lines: usize) -> Run<ProcessCommand> {
        Run::from(self).display_lines(lines)
    }

    fn silent(&self) -> Run<ProcessCommand> {
        Run::from(self).silent()
    }

    fn input<T: Into<Vec<u8>>>(&self, input: T) -> Run<ProcessCommand> {
        Run::from(self).input(input)
    }

    fn interactive(&self) -> Run<ProcessCommand> {
        Run::from(self).interactive()
    }

    fn stream(&self) -> Run<ProcessCommand> {
        Run::from(self).stream()
    }

    fn timeout(&self, timeout: Duration) -> Run<ProcessCommand> {
        Run::from(self).timeout(timeout)
    }

    fn interceptors(&self, interceptors: Interceptors<ProcessCommand>) -> Run<ProcessCommand> {
        Run::from(self).interceptors(interceptors)
    }

    fn with<T: std::any::Any + Send + Sync>(&self, value: T) -> Run<ProcessCommand> {
        Run::from(self).with(value)
    }
}

impl Default for CommandOptions {
    fn default() -> Self {
        Self {
            display_lines: DISPLAY_LINES,
            clear_env: false,
            input: None,
            interactive: false,
            silent: false,
            stream: false,
            timeout: None,
        }
    }
}

impl Cache<ProcessCommand> {
    /// Creates the cache of the runs of commands by their
    /// [`ProcessCommand::cache_key`] with the platform variables `names`,
    /// e.g. those configuring the commands like `KUBECONFIG`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{CommandCache, CommandExt, CommandOutput, Env, Platform, Process};
    /// use std::process::Command;
    ///
    /// let cache = CommandCache::with_vars(["KUBECONFIG"]);
    /// let platform = agplatform::test_platform::test_platform();
    /// platform.process.add_interceptor(cache.clone());
    /// platform.process.set_output("kubectl", CommandOutput::default());
    ///
    /// for config in ["a", "b", "a"] {
    ///     platform.env().set_var("KUBECONFIG", config).unwrap();
    ///     Command::new("kubectl").arg("get").run(platform.process()).unwrap();
    /// }
    ///
    /// assert_eq!(platform.process.calls().len(), 2);
    /// ```
    pub fn with_vars<K: Into<OsString>>(names: impl IntoIterator<Item = K>) -> Self {
        let vars: Vec<OsString> = names.into_iter().map(Into::into).collect();
        Self::new(move |run| run.subject().cache_key(&vars))
    }
}

/// The cache of the runs of commands by their [`ProcessCommand::cache_key`]
/// without platform variables.
impl Default for Cache<ProcessCommand> {
    fn default() -> Self {
        Self::new(|run| run.subject().cache_key(&[]))
    }
}

impl CommandOutput {
    /// Returns the standard output as UTF-8.
    ///
    /// # Errors
    ///
    /// Returns an error if the standard output is not valid UTF-8.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::CommandOutput;
    ///
    /// let output = CommandOutput { stdout: b"out".to_vec(), ..Default::default() };
    /// assert_eq!(output.stdout_str().unwrap(), "out");
    /// ```
    pub fn stdout_str(&self) -> Result<&str> {
        utf8("stdout", &self.stdout)
    }

    /// Returns the standard error as UTF-8.
    ///
    /// # Errors
    ///
    /// Returns an error if the standard error is not valid UTF-8.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::CommandOutput;
    ///
    /// let output = CommandOutput { stderr: b"err".to_vec(), ..Default::default() };
    /// assert_eq!(output.stderr_str().unwrap(), "err");
    /// ```
    pub fn stderr_str(&self) -> Result<&str> {
        utf8("stderr", &self.stderr)
    }

    /// Returns the standard output as UTF-8, replacing invalid sequences
    /// with `U+FFFD`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::CommandOutput;
    ///
    /// let output = CommandOutput { stdout: b"out\xFF".to_vec(), ..Default::default() };
    /// assert_eq!(output.stdout_lossy(), "out\u{FFFD}");
    /// ```
    pub fn stdout_lossy(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.stdout)
    }

    /// Returns the standard error as UTF-8, replacing invalid sequences
    /// with `U+FFFD`.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::CommandOutput;
    ///
    /// let output = CommandOutput { stderr: b"err\xFF".to_vec(), ..Default::default() };
    /// assert_eq!(output.stderr_lossy(), "err\u{FFFD}");
    /// ```
    pub fn stderr_lossy(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.stderr)
    }
}

/// The empty output of a process exiting with code `0`, e.g. replayed by
/// [`DryRun`](crate::interceptor::DryRun), whose success is set by the run
/// like for a command that ran. Not a success until then, like an output
/// an interceptor returns without passing the run on.
impl Default for CommandOutput {
    fn default() -> Self {
        Self {
            success: false,
            status: Some(0),
            signal: None,
            stdout: Vec::new(),
            stderr: Vec::new(),
            duration: Duration::ZERO,
            timed_out: false,
            display_lines: DISPLAY_LINES,
        }
    }
}

impl PartialEq for CommandOutput {
    fn eq(&self, other: &Self) -> bool {
        self.status == other.status
            && self.signal == other.signal
            && self.stdout == other.stdout
            && self.stderr == other.stderr
            && self.duration == other.duration
            && self.timed_out == other.timed_out
    }
}

impl Eq for CommandOutput {}

impl std::fmt::Display for CommandOutput {
    /// Shows the status, the duration and the last
    /// [`display_lines`](Self::display_lines) of the standard output and
    /// error.
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(
            f,
            "status: {}\nduration: {:?}\nstdout:\n{}\nstderr:\n{}",
            match (self.status, self.signal) {
                (Some(code), _) => code.to_string(),
                (None, Some(signal)) => format!("signal {signal}"),
                (None, None) => "none".to_string(),
            },
            self.duration,
            Tail {
                text: &self.stdout,
                lines: self.display_lines,
            },
            Tail {
                text: &self.stderr,
                lines: self.display_lines,
            }
        )
    }
}

impl std::error::Error for CommandOutput {}

impl Outcome for CommandOutput {
    /// Returns [`Self::success`].
    fn success(&self) -> bool {
        self.success
    }
}

impl Interceptor<ProcessCommand> for StdStream {
    fn chunk(&self, chunk: Chunk<ProcessCommand>) -> Result {
        if chunk.run().subject().options().stream {
            let _ = match chunk.stream() {
                OutputStream::Stdout => write_flushed(&mut std::io::stdout().lock(), chunk.data()),
                OutputStream::Stderr => write_flushed(&mut std::io::stderr().lock(), chunk.data()),
            };
        }

        chunk.forward()
    }
}

impl ProcessImpl {
    /// [internal]
    ///
    /// Creates a new `ProcessImpl` running child processes in
    /// `current_dir` with a copy of the variables of the current process.
    #[cfg(test)]
    pub(crate) fn new(current_dir: PathBuf) -> Self {
        Self::with_env(EnvImpl::new(std::env::vars_os().collect(), current_dir))
    }

    /// [internal]
    ///
    /// Creates a new `ProcessImpl` running child processes in the current
    /// directory of `env` with its variables.
    pub(crate) fn with_env(env: EnvImpl) -> Self {
        ProcessImpl {
            env,
            runs: std_runs(),
        }
    }

    /// Runs the command once with its `outputs`, blocking the thread, and
    /// returns its output without the standard output and error.
    fn attempt(&self, command: &ProcessCommand, mut outputs: Outputs) -> Result<CommandOutput> {
        let options = command.options();
        let start = Instant::now();
        let deadline = options
            .timeout
            .and_then(|timeout| start.checked_add(timeout));
        let result = self
            .child_command(command)
            .and_then(|mut child| {
                configure(&mut child, options.timeout.is_some());
                child.spawn()
            })
            .and_then(|mut child| {
                wait_with_output(
                    &mut child,
                    options.input.as_deref().unwrap_or_default(),
                    deadline,
                    &mut outputs,
                )
            });

        exited(command, start, result, outputs)
    }

    /// Runs the command once with its `outputs` like [`Self::attempt`]
    /// asynchronously.
    #[cfg(feature = "tokio")]
    async fn attempt_async(
        &self,
        command: &ProcessCommand,
        outputs: Outputs<'_>,
    ) -> Result<CommandOutput> {
        let outputs = Mutex::new(outputs);
        let options = command.options();
        let group = !options.interactive;
        let start = Instant::now();
        let deadline = options
            .timeout
            .and_then(|timeout| start.checked_add(timeout));
        let result = match self.child_command(command).and_then(|mut child| {
            configure(&mut child, group);
            tokio::process::Command::from(child)
                .kill_on_drop(true)
                .spawn()
        }) {
            Ok(mut child) => {
                tokio_process::wait_with_output(
                    &mut child,
                    group,
                    options.input.as_deref().unwrap_or_default(),
                    deadline,
                    &outputs,
                )
                .await
            }
            Err(error) => Err(error),
        };

        exited(
            command,
            start,
            result,
            outputs.into_inner().unwrap_or_else(PoisonError::into_inner),
        )
    }

    /// Returns a new command with the program, the arguments and the
    /// environment variables of `command` resolved against the
    /// configuration. The child gets exactly the variables of the platform,
    /// inherited while they are those of the current process, none with
    /// [`CommandOptions::clear_env`], updated by those of the command.
    ///
    /// # Errors
    ///
    /// Returns an error if the program is not found, see
    /// [`Self::program`].
    fn child_command(&self, command: &ProcessCommand) -> std::io::Result<Command> {
        let options = command.options();
        // WHY: only commands resolved against the env are run.
        let dir = command.current_dir().unwrap_or(Path::new(""));
        #[cfg(unix)]
        let program = self.program(command, dir)?;
        #[cfg(windows)]
        let program = self.program(command, dir);
        let mut child = Command::new(program.as_os_str());
        child.args(command.args()).current_dir(dir);

        let (stdin, stdout, stderr) = if options.interactive {
            (Stdio::inherit(), Stdio::inherit(), Stdio::inherit())
        } else if options.input.is_some() {
            (Stdio::piped(), Stdio::piped(), Stdio::piped())
        } else {
            (Stdio::null(), Stdio::piped(), Stdio::piped())
        };
        let (stdout, stderr) = if options.silent {
            (Stdio::null(), Stdio::null())
        } else {
            (stdout, stderr)
        };
        child.stdin(stdin).stdout(stdout).stderr(stderr);

        if options.clear_env {
            child.env_clear();
        } else if let Some(vars) = command.vars() {
            child.env_clear();
            child.envs(vars.iter().map(|(key, value)| (key, value)));
        }

        for (key, value) in command.envs() {
            match value {
                Some(value) => child.env(key, value),
                None => child.env_remove(key),
            };
        }

        Ok(child)
    }

    /// Returns the program of `command` as the child process running in
    /// `dir` finds it.
    ///
    /// WHY: std resolves a relative path with a separator against the
    /// current directory of the current process on Windows. On Unix it
    /// would look a bare name up in the `PATH` of the child process, which
    /// [`CommandOptions::clear_env`] removes, and only by forking when the
    /// environment of the child is set.
    #[cfg(unix)]
    fn program<'a>(
        &self,
        command: &'a ProcessCommand,
        dir: &Path,
    ) -> std::io::Result<Cow<'a, Path>> {
        let program = Path::new(&command.program);

        if program
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .any(|byte| std::path::is_separator(char::from(*byte)))
        {
            Ok(Cow::Owned(dir.join(program)))
        } else {
            self.search_path(command, program, dir)
        }
    }

    /// Returns the program of `command` as the child process running in
    /// `dir` finds it.
    ///
    /// WHY: std resolves a relative path with a separator against the
    /// current directory of the current process on Windows. A bare name is
    /// looked up by std from the child's `PATH`.
    #[cfg(windows)]
    fn program<'a>(&self, command: &'a ProcessCommand, dir: &Path) -> Cow<'a, Path> {
        let program = Path::new(&command.program);

        if program
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .any(|byte| std::path::is_separator(char::from(*byte)))
        {
            Cow::Owned(dir.join(program))
        } else {
            self.search_path(program)
        }
    }

    /// Returns the bare `program` for std to look up.
    ///
    /// WHY: std on Windows looks it up in the `PATH` of the child process,
    /// that of the platform, falling back to the one of the current process.
    #[cfg(windows)]
    fn search_path<'a>(&self, program: &'a Path) -> Cow<'a, Path> {
        let _ = self;
        Cow::Borrowed(program)
    }

    /// Returns the bare `program` of `command` found in its own `PATH`, or
    /// that of the platform otherwise, even with
    /// [`CommandOptions::clear_env`]. The bare `program` for std to look up
    /// when the command removes it or neither sets it.
    #[cfg(unix)]
    fn search_path<'a>(
        &self,
        command: &ProcessCommand,
        program: &'a Path,
        dir: &Path,
    ) -> std::io::Result<Cow<'a, Path>> {
        let _ = self;
        let path = match command.envs().iter().find(|(key, _)| key == "PATH") {
            Some((_, value)) => value.clone(),
            None => command.var(OsStr::new("PATH")),
        };

        match path {
            Some(path) => unix::find_program(program, &path, dir).map(Cow::Owned),
            None => Ok(Cow::Borrowed(program)),
        }
    }
}

impl Process for ProcessImpl {
    fn run(&self, run: impl Into<Run<ProcessCommand>>) -> Result<CommandOutput> {
        run_blocking(&self.runs, &self.env, &StdExecutor(self), run.into(), None)
    }

    fn run_to(
        &self,
        run: impl Into<Run<ProcessCommand>>,
        sink: &mut (dyn FnMut(OutputStream, &[u8]) -> Result + Send),
    ) -> Result<CommandOutput> {
        run_blocking(
            &self.runs,
            &self.env,
            &StdExecutor(self),
            run.into(),
            Some(sink),
        )
    }

    #[cfg(feature = "tokio")]
    fn run_async(
        &self,
        run: impl Into<Run<ProcessCommand>>,
    ) -> impl Future<Output = Result<CommandOutput>> + Send {
        let run = run.into();
        async move { run_async(&self.runs, &self.env, &TokioExecutor(self), run, None).await }
    }

    #[cfg(feature = "tokio")]
    fn run_async_to(
        &self,
        run: impl Into<Run<ProcessCommand>>,
        sink: &mut (dyn FnMut(OutputStream, &[u8]) -> Result + Send),
    ) -> impl Future<Output = Result<CommandOutput>> + Send {
        let run = run.into();
        async move { run_async(&self.runs, &self.env, &TokioExecutor(self), run, Some(sink)).await }
    }

    fn interceptors(&self) -> Interceptors<ProcessCommand> {
        self.runs.interceptors()
    }

    fn clear_interceptors(&self) {
        self.runs.clear_interceptors();
    }

    fn add_interceptor(&self, interceptor: impl Interceptor<ProcessCommand>) {
        self.runs.add_interceptor(interceptor);
    }
}

impl Executor for StdExecutor<'_> {
    type Subject = ProcessCommand;

    fn execute<'a>(
        &'a self,
        run: &'a Run<ProcessCommand>,
        mut emit: Emit<'a, ProcessCommand>,
    ) -> BoxFuture<'a, Result<CommandOutput>> {
        Box::pin(async move {
            let outputs = Outputs::new(move |stream, data| emitted(emit.emit(stream, data)));
            self.0.attempt(run.subject(), outputs)
        })
    }

    fn sleep(&self, delay: Duration) -> BoxFuture<'static, ()> {
        Box::pin(async move { std::thread::sleep(delay) })
    }
}

impl BlockingExecutor for StdExecutor<'_> {}

#[cfg(feature = "tokio")]
impl Executor for TokioExecutor<'_> {
    type Subject = ProcessCommand;

    fn execute<'a>(
        &'a self,
        run: &'a Run<ProcessCommand>,
        mut emit: Emit<'a, ProcessCommand>,
    ) -> BoxFuture<'a, Result<CommandOutput>> {
        Box::pin(async move {
            let outputs = Outputs::new(move |stream, data| emitted(emit.emit(stream, data)));
            self.0.attempt_async(run.subject(), outputs).await
        })
    }

    fn sleep(&self, delay: Duration) -> BoxFuture<'static, ()> {
        Box::pin(tokio::time::sleep(delay))
    }
}

#[cfg(feature = "tokio")]
impl AsyncExecutor for TokioExecutor<'_> {}

/// Writes `data` to `out` and flushes it.
fn write_flushed(out: &mut dyn Write, data: &[u8]) -> std::io::Result<()> {
    out.write_all(data)?;
    out.flush()
}

/// [internal]
///
/// Returns the runs through [`StdStream`], the interceptors of a process by
/// default.
pub(crate) fn std_runs() -> Runs<ProcessCommand> {
    let mut interceptors = Interceptors::new();
    interceptors.push(StdStream);
    Runs::new(interceptors)
}

/// [internal]
///
/// Runs `run` by `executor` through `runs` in the current directory of
/// `env` with its variables, passing its output to `sink`, if any, and
/// returns its output, an error when it is not a success, see
/// [`Process::run`].
pub(crate) fn run_blocking<E: BlockingExecutor<Subject = ProcessCommand>>(
    runs: &Runs<ProcessCommand>,
    env: &EnvImpl,
    executor: &E,
    mut run: Run<ProcessCommand>,
    sink: Option<&mut OutputSink<'_>>,
) -> Result<CommandOutput> {
    run.subject_mut().resolve(env);
    let mut sink = sink;
    let result = runs.run_blocking(
        executor,
        &mut run,
        sink.as_mut()
            .map(|sink| sink as &mut dyn Sink<ProcessCommand>),
    );
    conclude(result, |output| failure(run.subject(), output))
}

/// [internal]
///
/// Runs `run` like [`run_blocking`] asynchronously, see
/// [`Process::run_async`].
#[cfg(feature = "tokio")]
pub(crate) async fn run_async<E: AsyncExecutor<Subject = ProcessCommand>>(
    runs: &Runs<ProcessCommand>,
    env: &EnvImpl,
    executor: &E,
    mut run: Run<ProcessCommand>,
    sink: Option<&mut OutputSink<'_>>,
) -> Result<CommandOutput> {
    run.subject_mut().resolve(env);
    let mut sink = sink;
    let result = runs
        .run(
            executor,
            &mut run,
            sink.as_mut()
                .map(|sink| sink as &mut dyn Sink<ProcessCommand>),
        )
        .await;
    conclude(result, |output| failure(run.subject(), output))
}

/// [internal]
///
/// Waits for passing a chunk of the output on with `future`, see
/// [`Emit::emit`], on the current thread.
///
/// WHY: the parts of the executors reading the output of a child process
/// are sync, so they cannot await it. It is ready at once, as the chunk
/// interceptors are sync and the sinks of the runs of commands are
/// closures, so it does not block a runtime worker in [`TokioExecutor`].
pub(crate) fn emitted(future: impl Future<Output = Result>) -> Result {
    let mut future = std::pin::pin!(future);
    let mut polled = false;

    crate::interceptor::block_on(std::future::poll_fn(|context| {
        debug_assert!(!polled, "passing a chunk of a command on should not wait");
        polled = true;
        future.as_mut().poll(context)
    }))
}

/// [internal]
///
/// Returns the error of `command` failing with `output` as its source.
pub(crate) fn failure(command: &ProcessCommand, output: CommandOutput) -> crate::Error {
    let program = command.program();
    let error = match (output.timed_out, output.status) {
        (true, _) => process_error!("'{program}' timed out after {:?}", output.duration),
        (false, Some(0)) => process_error!("'{program}' was not accepted with exit code 0"),
        (false, Some(code)) => process_error!("'{program}' failed with exit code {code}"),
        (false, None) => match output.signal {
            Some(signal) => process_error!("'{program}' was terminated by signal {signal}"),
            None => process_error!("'{program}' was terminated without an exit code"),
        },
    };

    error.with_subject(program).with_source(output)
}

/// [internal]
///
/// Returns the error of `program` failing to be started or waited for with
/// the I/O `error` as its source.
pub(crate) fn io_error(program: &str, error: std::io::Error) -> crate::Error {
    process_error!("'{program}': {error}")
        .with_subject(program)
        .with_source(error)
}

fn utf8<'a>(name: &str, bytes: &'a [u8]) -> Result<&'a str> {
    std::str::from_utf8(bytes)
        .map_err(|error| process_error!("{name} is not valid UTF-8: {error}").with_source(error))
}

/// Rejects the combinations of `options` that cannot work, also with a
/// `sink`.
fn validate(program: &str, options: &CommandOptions, sink: bool) -> Result<()> {
    if options.interactive && options.input.is_some() {
        Err(process_error!(
            "'{program}': interactive cannot be combined with input"
        ))
    } else if options.interactive && options.silent {
        Err(process_error!(
            "'{program}': interactive cannot be combined with silent"
        ))
    } else if options.interactive && options.timeout.is_some() {
        Err(process_error!(
            "'{program}': interactive cannot be combined with timeout"
        ))
    } else if sink && options.interactive {
        Err(process_error!(
            "'{program}': interactive cannot be combined with a sink"
        ))
    } else if sink && options.silent {
        Err(process_error!(
            "'{program}': silent cannot be combined with a sink"
        ))
    } else {
        Ok(())
    }
}

/// Returns the output of the attempt of `command` started at `start` from
/// the `result` of its child process, without the standard output and
/// error, which went to `outputs`.
fn exited(
    command: &ProcessCommand,
    start: Instant,
    result: std::io::Result<Option<std::process::ExitStatus>>,
    outputs: Outputs,
) -> Result<CommandOutput> {
    outputs.finish()?;
    let status = result.map_err(|error| io_error(&command.program(), error))?;

    Ok(CommandOutput {
        status: status.and_then(|status| status.code()),
        signal: status.as_ref().and_then(exit_signal),
        duration: start.elapsed(),
        timed_out: status.is_none(),
        ..Default::default()
    })
}

const DISPLAY_LINES: usize = 10;

/// How long the output of a child process is read at most after it exited,
/// see [`Process::run`].
pub(crate) const EXIT_GRACE: Duration = Duration::from_millis(100);

impl<'a> Outputs<'a> {
    /// Creates the outputs passing the chunks on with `emit`.
    pub(crate) fn new(emit: impl FnMut(OutputStream, &[u8]) -> Result + Send + 'a) -> Self {
        Self {
            emit: Box::new(emit),
            error: None,
        }
    }

    /// Passes the `data` of the output `stream` of the child process on.
    pub(crate) fn push(&mut self, stream: OutputStream, data: &[u8]) {
        if let Err(error) = (self.emit)(stream, data) {
            self.error = Some(error);
        }
    }

    /// Returns `true` once passing a chunk on failed, which ends the attempt.
    pub(crate) fn failed(&self) -> bool {
        self.error.is_some()
    }

    /// Returns the error of passing a chunk on if it failed.
    pub(crate) fn finish(self) -> Result {
        self.error.map_or(Ok(()), Err)
    }
}

/// [internal]
///
/// Returns the milliseconds until `deadline` rounded up, `0` when it has
/// passed, at most `i32::MAX`.
pub(crate) fn millis_until(deadline: Instant) -> u32 {
    deadline
        .saturating_duration_since(Instant::now())
        .as_nanos()
        .div_ceil(1_000_000)
        .min(i32::MAX as u128) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Env;
    use crate::TestDir;
    use crate::interceptor::Next;
    use crate::interceptor::Retry;
    use std::error::Error as _;
    use std::sync::Arc;
    use std::task::Poll;

    #[cfg(unix)]
    fn shell(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    #[cfg(windows)]
    fn shell(script: &str) -> Command {
        let mut command = Command::new("cmd");
        std::os::windows::process::CommandExt::raw_arg(command.arg("/C"), script);
        command
    }

    #[cfg(unix)]
    const SHELL: &str = "sh";
    #[cfg(windows)]
    const SHELL: &str = "cmd";

    #[cfg(unix)]
    const SLEEP_THEN_ECHO: &str = "sleep 10; echo done";
    #[cfg(windows)]
    const SLEEP_THEN_ECHO: &str = "ping -n 11 127.0.0.1 >NUL & echo done";

    #[cfg(unix)]
    const ECHO_THEN_SLEEP: &str = "echo out; sleep 10";
    #[cfg(windows)]
    const ECHO_THEN_SLEEP: &str = "echo out& ping -n 11 127.0.0.1 >NUL";

    /// Returns a sink collecting the standard output and error into
    /// `sunk`.
    fn sink_into(
        sunk: &mut (Vec<u8>, Vec<u8>),
    ) -> impl FnMut(OutputStream, &[u8]) -> Result + Send + '_ {
        |stream, data| {
            match stream {
                OutputStream::Stdout => sunk.0.extend_from_slice(data),
                OutputStream::Stderr => sunk.1.extend_from_slice(data),
            }
            Ok(())
        }
    }

    #[cfg(unix)]
    const INHERITED_VAR: &str = "HOME";
    #[cfg(windows)]
    const INHERITED_VAR: &str = "USERPROFILE";

    #[cfg(all(unix, feature = "tokio"))]
    const MARK_LATER_THEN_SLEEP: &str = "(sleep 0.5; touch marker) & sleep 10";
    #[cfg(all(windows, feature = "tokio"))]
    const MARK_LATER_THEN_SLEEP: &str = "start \"\" /b cmd /C \"ping -n 2 127.0.0.1 >NUL & type nul > marker\" & ping -n 11 127.0.0.1 >NUL";

    /// When [`MARK_LATER_THEN_SLEEP`] would have created the marker.
    #[cfg(all(unix, feature = "tokio"))]
    const AFTER_MARKER: Duration = Duration::from_millis(1000);
    #[cfg(all(windows, feature = "tokio"))]
    const AFTER_MARKER: Duration = Duration::from_millis(2000);

    /// Prints the pid of a sleeping descendant holding the output open.
    #[cfg(unix)]
    const BACKGROUND_SLEEPER: &str = "sleep 10 & echo $!";
    /// Prints 0.
    #[cfg(windows)]
    const BACKGROUND_SLEEPER: &str = "echo 0";

    #[cfg(unix)]
    const EXITED_CHILD_WAIT_MAX: Duration = Duration::from_secs(2);
    #[cfg(windows)]
    const EXITED_CHILD_WAIT_MAX: Duration = Duration::from_secs(6);

    #[cfg(unix)]
    const TOOL: &str = "agplatform_tool";
    #[cfg(windows)]
    const TOOL: &str = "agplatform_tool.cmd";

    #[cfg(unix)]
    const FAIL_ONCE: &str = "test -f marker || { touch marker; exit 1; }";
    #[cfg(windows)]
    const FAIL_ONCE: &str = "if exist marker (exit 0) else (type nul > marker & exit 1)";

    #[cfg(unix)]
    const PRINT_DIR: &str = "pwd";
    #[cfg(windows)]
    const PRINT_DIR: &str = "cd";

    #[cfg(unix)]
    fn echo_stdin() -> Command {
        Command::new("cat")
    }

    #[cfg(windows)]
    fn echo_stdin() -> Command {
        let mut command = Command::new("findstr");
        command.arg("^");
        command
    }

    #[cfg(unix)]
    fn print_var(name: &str) -> Command {
        shell(&format!("echo \"${{{name}-unset}}\""))
    }

    #[cfg(windows)]
    fn print_var(name: &str) -> Command {
        let mut command = Command::new("cmd");
        std::os::windows::process::CommandExt::raw_arg(
            command.args(["/V:ON", "/C"]),
            format!("if defined {name} (echo(!{name}!) else (echo unset)"),
        );
        command
    }

    #[cfg(unix)]
    fn kill_sleeper(pid: &str) {
        Command::new("kill").arg(pid.trim()).status().unwrap();
    }

    #[cfg(windows)]
    fn kill_sleeper(_pid: &str) {}

    /// Writes the program [`TOOL`] printing `tool` to `dir`.
    ///
    /// WHY: written by a child process, as the children forked meanwhile by
    /// other tests would inherit a descriptor writing it, failing its
    /// execution with ETXTBSY.
    #[cfg(unix)]
    fn write_tool(dir: &Path) {
        let status = shell("printf '#!/bin/sh\\necho tool\\n' > \"$0\" && chmod 755 \"$0\"")
            .arg(dir.join(TOOL))
            .status()
            .unwrap();

        assert!(status.success());
    }

    #[cfg(windows)]
    fn write_tool(dir: &Path) {
        std::fs::write(dir.join(TOOL), "@echo tool\r\n").unwrap();
    }

    /// Runs a child with a timeout from the process of a test that holds
    /// the fifo in `dir` open and exits the process without ending the run
    /// once the child is ready.
    #[cfg(target_os = "linux")]
    fn exit_while_running_child(dir: PathBuf) -> ! {
        struct ExitOnOutput;

        impl Interceptor<ProcessCommand> for ExitOnOutput {
            fn chunk(&self, _chunk: Chunk<ProcessCommand>) -> Result {
                std::process::exit(0);
            }
        }

        let process = process(dir);
        process.add_interceptor(ExitOnOutput);
        let _ = shell("exec 3> fifo; echo ready; exec sleep 10")
            .stream()
            .timeout(Duration::from_secs(10))
            .run(&process);
        std::process::exit(1)
    }

    fn process(current_dir: PathBuf) -> ProcessImpl {
        ProcessImpl::new(current_dir)
    }

    /// Records the runs it handles and the output passing through it.
    #[derive(Clone, Default)]
    struct Recorder(Arc<std::sync::Mutex<Recorded>>);

    #[derive(Default)]
    struct Recorded {
        events: Vec<String>,
        options: Vec<CommandOptions>,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    }

    impl Recorder {
        fn recorded(&self) -> std::sync::MutexGuard<'_, Recorded> {
            self.0.lock().unwrap()
        }

        fn events(&self) -> Vec<String> {
            self.recorded().events.clone()
        }

        fn options(&self) -> CommandOptions {
            self.recorded().options[0].clone()
        }

        fn stdout(&self) -> Vec<u8> {
            self.recorded().stdout.clone()
        }

        fn stderr(&self) -> Vec<u8> {
            self.recorded().stderr.clone()
        }
    }

    impl Interceptor<ProcessCommand> for Recorder {
        fn handle<'a>(
            &'a self,
            mut next: Next<'a, ProcessCommand>,
        ) -> BoxFuture<'a, Result<CommandOutput>> {
            Box::pin(async move {
                {
                    let command = next.run().subject();
                    let mut recorded = self.recorded();
                    recorded.events.push(format!(
                        "started in {}",
                        command.current_dir().unwrap().display()
                    ));
                    recorded.options.push(command.options().clone());
                }

                let result = next.attempt().await;
                self.recorded().events.push(match &result {
                    Ok(output) if output.success => "finished ok".to_string(),
                    Ok(output) => format!("finished failed {:?}", output.status),
                    Err(error) => format!("finished {error}"),
                });
                result
            })
        }

        fn chunk(&self, chunk: Chunk<ProcessCommand>) -> Result {
            match chunk.stream() {
                OutputStream::Stdout => self.recorded().stdout.extend_from_slice(chunk.data()),
                OutputStream::Stderr => self.recorded().stderr.extend_from_slice(chunk.data()),
            }

            chunk.forward()
        }

        fn cancelled(&self, run: &Run<ProcessCommand>) {
            self.recorded()
                .events
                .push(format!("cancelled {}", run.subject().program()));
        }
    }

    /// Calls the closure with the run before passing it on.
    struct Inspect<F>(F);

    impl<F: Fn(&mut Run<ProcessCommand>) + Send + Sync + 'static> Interceptor<ProcessCommand>
        for Inspect<F>
    {
        fn handle<'a>(
            &'a self,
            mut next: Next<'a, ProcessCommand>,
        ) -> BoxFuture<'a, Result<CommandOutput>> {
            (self.0)(next.run_mut());
            next.forward()
        }
    }

    /// Accepts the outputs with the exit code.
    struct Accept(i32);

    impl Interceptor<ProcessCommand> for Accept {
        fn handle<'a>(
            &'a self,
            mut next: Next<'a, ProcessCommand>,
        ) -> BoxFuture<'a, Result<CommandOutput>> {
            Box::pin(async move {
                next.attempt().await.map(|output| CommandOutput {
                    success: output.success || output.status == Some(self.0),
                    ..output
                })
            })
        }
    }

    /// Sets the replay output of the runs.
    struct Replay(CommandOutput);

    impl Interceptor<ProcessCommand> for Replay {
        fn handle<'a>(
            &'a self,
            mut next: Next<'a, ProcessCommand>,
        ) -> BoxFuture<'a, Result<CommandOutput>> {
            next.set_replay_output(self.0.clone());
            next.forward()
        }
    }

    /// Changes of the env the process shares, like by its platform.
    impl ProcessImpl {
        fn set_var(&self, key: String, value: String) {
            self.env.set_var(key, value).unwrap();
        }

        fn remove_var(&self, key: &str) {
            self.env.remove_var(key);
        }

        fn set_current_dir(&self, path: PathBuf) {
            self.env.replace_current_dir(path);
        }
    }

    /// Returns a process in the temporary directory whose only interceptor
    /// is the returned recorder.
    fn observed() -> (ProcessImpl, Recorder) {
        let recorder = Recorder::default();
        let process = process(std::env::temp_dir());
        process.clear_interceptors();
        process.add_interceptor(recorder.clone());
        (process, recorder)
    }

    /// Returns the interceptors `retry` and `recorder`, in that order.
    fn retried(retry: Retry<ProcessCommand>, recorder: &Recorder) -> Interceptors<ProcessCommand> {
        let mut interceptors = Interceptors::new();
        interceptors.push(retry);
        interceptors.push(recorder.clone());
        interceptors
    }

    /// Returns the run of `command` resolved against the env of `process`,
    /// as when it starts.
    fn resolved(command: &Command, process: &ProcessImpl) -> Run<ProcessCommand> {
        let mut run = Run::from(command);
        run.subject_mut().resolve(&process.env);
        run
    }

    /// Returns the command of `command` with `options`.
    fn with_options(command: &Command, options: CommandOptions) -> ProcessCommand {
        ProcessCommand::with_options(command, options)
    }

    /// Returns an output with the exit code `status`.
    fn exit(status: i32) -> CommandOutput {
        CommandOutput {
            status: Some(status),
            ..Default::default()
        }
    }

    #[cfg(feature = "tokio")]
    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    mod process_command {
        use super::*;

        mod new {
            use super::*;

            #[test]
            fn new_should_use_default_options() {
                let command = ProcessCommand::new(&Command::new("git"));

                assert_eq!(*command.options(), CommandOptions::default());
            }
        }

        mod with_options {
            use super::*;

            #[test]
            fn with_options_should_run_command_with_options() {
                let (process, recorder) = observed();
                let options = CommandOptions {
                    display_lines: 3,
                    ..Default::default()
                };

                process
                    .run(with_options(&shell("exit 0"), options.clone()))
                    .unwrap();

                assert_eq!(recorder.options(), options);
            }
        }

        mod args {
            use super::*;

            #[test]
            fn args_should_return_args_of_command() {
                let mut command = Command::new("git");
                command.args(["commit", "-m", "message"]);

                assert_eq!(
                    ProcessCommand::new(&command).args(),
                    ["commit", "-m", "message"]
                );
            }
        }

        mod args_mut {
            use super::*;

            #[test]
            fn args_mut_should_change_args_of_command() {
                let mut command = ProcessCommand::new(&Command::new("git"));

                command.args_mut().push("status".into());

                assert_eq!(command.args(), ["status"]);
            }
        }

        mod cache_key {
            use super::*;

            /// Returns the command `printf` with `args` in the env with
            /// `vars`.
            fn printf(args: &[&str], vars: &[(&str, &str)]) -> ProcessCommand {
                let mut command = ProcessCommand::new(Command::new("printf").args(args));
                command.vars = Some(
                    vars.iter()
                        .map(|(key, value)| (key.into(), value.into()))
                        .collect(),
                );
                command
            }

            #[test]
            fn cache_key_should_name_dir_command_vars_and_input() {
                let mut command = Command::new("git");
                command.arg("status").env("SET", "1").env_remove("REMOVED");
                let options = CommandOptions {
                    input: Some(b"in".to_vec()),
                    ..Default::default()
                };

                assert_eq!(
                    with_options(&command, options).cache_key(&["UNSET".into()]),
                    Some(format!(
                        "dir: \"\"\ncommand: [\"git\", \"status\"]\n\
                         envs: [(\"REMOVED\", \"removed\"), (\"SET\", \"set\")]\n\
                         clear_env: false\nsilent: false\nvars: [(\"UNSET\", \"unset\")]\n\
                         values: {:016x}\ninput: in",
                        crate::utils::fnv1a([None, Some(b"1".as_slice()), None])
                    ))
                );
            }

            #[test]
            fn cache_key_should_ignore_platform_vars_not_named() {
                let without = printf(&[], &[]).cache_key(&["NAMED".into()]);

                let with = printf(&[], &[("CI_JOB_ID", "42")]).cache_key(&["NAMED".into()]);

                assert_eq!(with, without);
            }

            #[test]
            fn cache_key_should_hash_values_of_vars() {
                let mut command = printf(&[], &[("PLATFORM_TOKEN", "t0k3n")]);
                command.set_env("TOKEN", "s3cr3t");

                let key = command.cache_key(&["PLATFORM_TOKEN".into()]).unwrap();

                assert!(!key.contains("s3cr3t") && !key.contains("t0k3n"), "{key}");
                assert!(
                    key.contains("vars: [(\"PLATFORM_TOKEN\", \"set\")]"),
                    "{key}"
                );
            }

            #[test]
            fn cache_key_should_differ_for_commands_with_different_output() {
                let changed = |change: fn(&mut CommandOptions)| {
                    let mut command = printf(&["a"], &[]);
                    change(command.options_mut());
                    command
                };
                let commands = [
                    printf(&["a b"], &[]),
                    printf(&["a", "b"], &[]),
                    printf(&["a"], &[]),
                    changed(|options| options.silent = true),
                    changed(|options| options.clear_env = true),
                    printf(&["a"], &[("VAR", "1")]),
                    printf(&["a"], &[("VAR", "2")]),
                    changed(|options| options.input = Some(b"\xFF".to_vec())),
                    changed(|options| options.input = Some(b"\xFE".to_vec())),
                ];

                let keys = commands
                    .iter()
                    .map(|command| command.cache_key(&["VAR".into()]))
                    .collect::<std::collections::BTreeSet<_>>();

                assert_eq!(keys.len(), commands.len(), "{keys:?}");
            }

            #[test]
            fn cache_key_interactive_should_be_none() {
                let mut command = printf(&[], &[]);
                command.options_mut().interactive = true;

                assert_eq!(command.cache_key(&[]), None);
            }
        }

        mod command_line {
            use super::*;

            #[test]
            fn command_line_should_join_program_and_args() {
                let mut command = Command::new("git");
                command.args(["commit", "-m", "message"]);

                assert_eq!(
                    ProcessCommand::new(&command).command_line(),
                    "git commit -m message"
                );
            }
        }

        mod current_dir {
            use super::*;

            #[test]
            fn current_dir_should_be_resolved_dir_of_command_when_run() {
                let (process, recorder) = observed();
                let mut command = shell("exit 0");
                command.current_dir("sub");

                let _ = process.run(&command);

                assert_eq!(ProcessCommand::new(&command).current_dir(), None);
                assert_eq!(
                    recorder.events()[0],
                    format!("started in {}", std::env::temp_dir().join("sub").display())
                );
            }
        }

        mod envs {
            use super::*;

            #[test]
            fn envs_should_return_vars_set_and_removed_by_command() {
                let mut command = Command::new("git");
                command.env("SET", "1").env_remove("REMOVED");

                assert_eq!(
                    ProcessCommand::new(&command).envs(),
                    [
                        (OsString::from("REMOVED"), None),
                        (OsString::from("SET"), Some(OsString::from("1")))
                    ]
                );
            }
        }

        mod options_mut {
            use super::*;

            #[test]
            fn options_mut_should_change_options_of_command() {
                let mut command = ProcessCommand::new(&Command::new("git"));

                command.options_mut().silent = true;

                assert!(command.options().silent);
            }
        }

        mod program {
            use super::*;

            #[test]
            fn program_should_return_program_without_args() {
                let mut command = Command::new("git");
                command.args(["clone", "https://user:token@example.com/repo"]);

                assert_eq!(ProcessCommand::new(&command).program(), "git");
            }
        }

        mod remove_env {
            use super::*;

            #[test]
            fn remove_env_should_remove_var_for_command() {
                let process = process(std::env::temp_dir());
                process.add_interceptor(Inspect(|run: &mut Run<ProcessCommand>| {
                    run.subject_mut().remove_env("AGPLATFORM_REMOVED");
                }));

                let output = process
                    .run(print_var("AGPLATFORM_REMOVED").env("AGPLATFORM_REMOVED", "1"))
                    .unwrap();

                assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "unset");
            }
        }

        mod retry {
            use super::*;

            #[test]
            fn retry_should_retry_failures_safe_to_retry_only() {
                for (command, attempts) in [
                    (shell("exit 3"), 2),
                    (shell(SLEEP_THEN_ECHO), 1),
                    (Command::new("agplatform_missing_program"), 1),
                ] {
                    let (process, recorder) = observed();
                    let program = command.get_program().to_owned();

                    let _ = Run::from(&command)
                        .timeout(Duration::from_millis(100))
                        .interceptors(retried(ProcessCommand::retry(1, Duration::ZERO), &recorder))
                        .run(&process);

                    assert_eq!(
                        recorder.events().len(),
                        attempts * 2,
                        "program: {program:?}"
                    );
                }
            }
        }

        mod safe_to_retry {
            use super::*;

            #[test]
            fn safe_to_retry_should_accept_failed_outputs_that_did_not_time_out() {
                let command = ProcessCommand::new(&Command::new("git"));

                for (result, safe) in [
                    (Ok(exit(1)), true),
                    (
                        Ok(CommandOutput {
                            status: None,
                            signal: Some(9),
                            ..Default::default()
                        }),
                        true,
                    ),
                    (
                        Ok(CommandOutput {
                            status: None,
                            timed_out: true,
                            ..Default::default()
                        }),
                        false,
                    ),
                    (
                        Ok(CommandOutput {
                            success: true,
                            ..Default::default()
                        }),
                        false,
                    ),
                    (Err(crate::Error::process("not found")), false),
                ] {
                    assert_eq!(command.safe_to_retry(&result), safe, "{result:?}");
                }
            }
        }

        mod set_env {
            use super::*;

            #[test]
            fn set_env_should_replace_var_of_command() {
                let mut command =
                    ProcessCommand::new(Command::new("git").env("A", "1").env("B", "1"));

                command.set_env("A", "2");

                assert_eq!(
                    command.envs(),
                    [
                        (OsString::from("B"), Some(OsString::from("1"))),
                        (OsString::from("A"), Some(OsString::from("2")))
                    ]
                );
            }

            #[test]
            fn set_env_should_pass_var_to_child() {
                let process = process(std::env::temp_dir());
                process.add_interceptor(Inspect(|run: &mut Run<ProcessCommand>| {
                    run.subject_mut().set_env("AGPLATFORM_CHANGED", "changed");
                }));

                let output = process.run(&print_var("AGPLATFORM_CHANGED")).unwrap();

                assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "changed");
            }
        }

        mod set_program {
            use super::*;

            #[test]
            fn set_program_should_change_program_of_command() {
                let mut command = ProcessCommand::new(&Command::new("git"));

                command.set_program("hg");

                assert_eq!(command.program(), "hg");
            }
        }

        mod var {
            use super::*;

            #[test]
            fn var_should_return_var_of_env_command_was_resolved_in() {
                let env = EnvImpl::new(vec![("KEY".into(), "value".into())], PathBuf::new());
                let mut command = ProcessCommand::new(&shell("exit 0"));

                command.resolve(&env);

                assert_eq!(command.var(OsStr::new("KEY")), Some("value".into()));
                assert_eq!(command.var(OsStr::new("MISSING")), None);
            }

            #[test]
            fn var_unchanged_vars_of_process_should_return_var_of_process() {
                let mut command = ProcessCommand::new(&shell("exit 0"));

                command.resolve(&EnvImpl::new_from_std());

                assert_eq!(command.vars(), None);
                assert_eq!(
                    command.var(OsStr::new(INHERITED_VAR)),
                    std::env::var_os(INHERITED_VAR)
                );
            }
        }

        mod subject {
            use super::*;

            /// Returns the output of `run` by `process` through no
            /// interceptors, failed or not.
            fn ran(process: &ProcessImpl, mut run: Run<ProcessCommand>) -> Result<CommandOutput> {
                run.subject_mut().resolve(&process.env);
                Interceptors::new().run_blocking(&StdExecutor(process), &mut run, None)
            }

            mod validate {
                use super::*;

                #[test]
                fn validate_should_reject_options_not_combining_with_sink() {
                    let process = process(std::env::temp_dir());
                    let mut sunk = (Vec::new(), Vec::new());

                    for (run, option) in [
                        (shell("exit 0").silent(), "silent"),
                        (shell("exit 0").interactive(), "interactive"),
                    ] {
                        let error = process.run_to(run, &mut sink_into(&mut sunk)).unwrap_err();

                        assert!(
                            error
                                .description()
                                .ends_with(&format!("': {option} cannot be combined with a sink")),
                            "{error}"
                        );
                    }
                }

                #[test]
                fn validate_options_made_invalid_by_interceptor_should_fail_attempt() {
                    let (process, recorder) = observed();
                    process.add_interceptor(Inspect(|run: &mut Run<ProcessCommand>| {
                        run.subject_mut().options_mut().interactive = true;
                        run.subject_mut().options_mut().silent = true;
                    }));

                    let error = process.run(&shell("exit 0")).unwrap_err();

                    assert!(
                        error
                            .description()
                            .ends_with("': interactive cannot be combined with silent")
                    );
                    assert_eq!(recorder.events()[1], format!("finished {error}"));
                }
            }

            mod replay {
                use super::*;

                #[test]
                fn replay_should_pass_output_on_as_if_it_ran() {
                    // GIVEN an interceptor replaying an output of a streamed run
                    let dir = TestDir::new();
                    let (process, recorder) = observed();
                    process.set_current_dir(dir.path().to_path_buf());
                    process.add_interceptor(Replay(CommandOutput {
                        stdout: b"set".to_vec(),
                        stderr: b"err".to_vec(),
                        ..exit(1)
                    }));

                    // WHEN running a command that would create a marker
                    let error = shell("touch marker").stream().run(&process).unwrap_err();

                    // THEN the output is returned and passed on as if it ran
                    let output = error.command_output().unwrap();
                    assert_eq!(
                        (&*output.stdout, &*output.stderr),
                        (&b"set"[..], &b"err"[..])
                    );
                    assert_eq!(
                        (recorder.stdout(), recorder.stderr()),
                        (b"set".to_vec(), b"err".to_vec())
                    );
                    assert!(!dir.path().join("marker").exists());
                }

                #[test]
                fn replay_silent_or_interactive_should_pass_nothing_on() {
                    let (process, recorder) = observed();
                    process.add_interceptor(Replay(CommandOutput {
                        stdout: b"out".to_vec(),
                        ..exit(0)
                    }));

                    for run in [shell("exit 0").silent(), shell("exit 0").interactive()] {
                        let output = process.run(run).unwrap();

                        assert!(output.stdout.is_empty());
                    }

                    assert!(recorder.stdout().is_empty());
                }

                #[test]
                fn replay_failing_to_pass_on_should_stop_with_error() {
                    let process = process(std::env::temp_dir());
                    process.add_interceptor(Replay(CommandOutput {
                        stdout: b"out".to_vec(),
                        stderr: b"err".to_vec(),
                        ..exit(0)
                    }));
                    let mut streams = Vec::new();

                    let error = process
                        .run_to(&shell("exit 0"), &mut |stream, _| {
                            streams.push(stream);
                            Err(crate::Error::process("full"))
                        })
                        .unwrap_err();

                    assert_eq!(error.description(), "full");
                    assert_eq!(streams, [OutputStream::Stdout]);
                }
            }

            mod finish {
                use super::*;

                #[test]
                fn finish_should_set_success_from_status_and_timeout() {
                    let process = process(std::env::temp_dir());

                    for (status, timed_out, success) in [
                        (Some(0), false, true),
                        (Some(1), false, false),
                        (None, false, false),
                        (Some(0), true, false),
                    ] {
                        let output = CommandOutput {
                            success: !success,
                            status,
                            timed_out,
                            ..Default::default()
                        };
                        let run = Run::from(&shell("exit 0")).with_replay_output(output);

                        let finished = ran(&process, run).unwrap();

                        assert_eq!(finished.success, success, "{status:?} {timed_out}");
                    }
                }

                #[test]
                fn finish_should_take_display_lines_of_run() {
                    let process = process(std::env::temp_dir());

                    let output = ran(&process, shell("exit 0").display_lines(3)).unwrap();

                    assert_eq!(output.display_lines, 3);
                }

                #[test]
                fn finish_should_take_output_as_passed_on() {
                    // GIVEN an interceptor redacting the chunks
                    struct Redact;

                    impl Interceptor<ProcessCommand> for Redact {
                        fn chunk(&self, mut chunk: Chunk<ProcessCommand>) -> Result {
                            chunk.set_data(b"***".to_vec());
                            chunk.forward()
                        }
                    }

                    let process = process(std::env::temp_dir());
                    process.add_interceptor(Redact);

                    // WHEN running a command printing a secret
                    let output = process.run(&shell("echo secret")).unwrap();

                    // THEN the output has the redacted data
                    assert_eq!(output.stdout, b"***");
                }
            }
        }
    }

    mod run {
        use super::*;

        #[test]
        fn run_should_combine_options() {
            let (process, recorder) = observed();

            let _ = shell("exit 0")
                .timeout(Duration::from_secs(10))
                .stream()
                .clear_env()
                .silent()
                .display_lines(3)
                .input("in")
                .interactive()
                .run(&process);

            assert_eq!(
                recorder.options(),
                CommandOptions {
                    display_lines: 3,
                    clear_env: true,
                    silent: true,
                    input: Some(b"in".to_vec()),
                    interactive: true,
                    stream: true,
                    timeout: Some(Duration::from_secs(10))
                }
            );
        }

        mod clear_env {
            use super::*;

            #[test]
            fn clear_env_should_set_option() {
                let run = Run::from(&Command::new("git")).clear_env();

                assert!(run.subject().options().clear_env);
            }
        }

        mod display_lines {
            use super::*;

            #[test]
            fn display_lines_should_set_option() {
                let run = Run::from(&Command::new("git")).display_lines(3);

                assert_eq!(run.subject().options().display_lines, 3);
            }
        }

        mod silent {
            use super::*;

            #[test]
            fn silent_should_set_option() {
                let run = Run::from(&Command::new("git")).silent();

                assert!(run.subject().options().silent);
            }
        }

        mod input {
            use super::*;

            #[test]
            fn input_should_set_option() {
                let run = Run::from(&Command::new("git")).input("in");

                assert_eq!(
                    run.subject().options().input.as_deref(),
                    Some(b"in".as_slice())
                );
            }
        }

        mod interactive {
            use super::*;

            #[test]
            fn interactive_should_set_option() {
                let run = Run::from(&Command::new("git")).interactive();

                assert!(run.subject().options().interactive);
            }
        }

        mod stream {
            use super::*;

            #[test]
            fn stream_should_set_option() {
                let run = Run::from(&Command::new("git")).stream();

                assert!(run.subject().options().stream);
            }
        }

        mod timeout {
            use super::*;

            #[test]
            fn timeout_should_set_option() {
                let run = Run::from(&Command::new("git")).timeout(Duration::from_secs(1));

                assert_eq!(
                    run.subject().options().timeout,
                    Some(Duration::from_secs(1))
                );
            }
        }

        mod interceptors {
            use super::*;

            #[test]
            fn interceptors_should_run_command_with_own_interceptors_only() {
                let (process, recorder) = observed();
                let own = Recorder::default();
                let mut interceptors = Interceptors::new();
                interceptors.push(own.clone());

                Run::from(&shell("exit 0"))
                    .interceptors(interceptors)
                    .run(&process)
                    .unwrap();

                assert!(recorder.events().is_empty());
                assert_eq!(own.events().len(), 2);
            }
        }

        #[allow(clippy::module_inception)]
        mod run {
            use super::*;

            #[test]
            fn run_should_run_with_options() {
                let (process, recorder) = observed();

                Run::from(&shell("exit 0"))
                    .display_lines(3)
                    .run(&process)
                    .unwrap();

                assert_eq!(recorder.options().display_lines, 3);
            }
        }

        mod run_to {
            use super::*;

            #[test]
            fn run_to_should_run_with_options_to_sink() {
                let (process, recorder) = observed();
                let command = shell("echo out");
                let mut sunk = (Vec::new(), Vec::new());

                command
                    .display_lines(3)
                    .run_to(&process, &mut sink_into(&mut sunk))
                    .unwrap();

                assert_eq!(recorder.options().display_lines, 3);
                assert_eq!(String::from_utf8_lossy(&sunk.0).trim(), "out");
            }
        }

        #[cfg(feature = "tokio")]
        mod run_async {
            use super::*;

            #[test]
            fn run_async_should_run_with_options() {
                let (process, recorder) = observed();
                let command = shell("exit 0");

                block_on(command.display_lines(3).run_async(&process)).unwrap();

                assert_eq!(recorder.options().display_lines, 3);
            }
        }

        #[cfg(feature = "tokio")]
        mod run_async_to {
            use super::*;

            #[test]
            fn run_async_to_should_run_with_options_to_sink() {
                let (process, recorder) = observed();
                let command = shell("echo out");
                let mut sunk = (Vec::new(), Vec::new());

                block_on(
                    command
                        .display_lines(3)
                        .run_async_to(&process, &mut sink_into(&mut sunk)),
                )
                .unwrap();

                assert_eq!(recorder.options().display_lines, 3);
                assert_eq!(String::from_utf8_lossy(&sunk.0).trim(), "out");
            }
        }
    }

    mod command_options {
        use super::*;

        mod default {
            use super::*;

            #[allow(clippy::module_inception)]
            mod default {
                use super::*;

                #[test]
                fn default_should_show_ten_lines_only() {
                    assert_eq!(
                        CommandOptions::default(),
                        CommandOptions {
                            display_lines: 10,
                            clear_env: false,
                            silent: false,
                            input: None,
                            interactive: false,
                            stream: false,
                            timeout: None,
                        }
                    );
                }
            }
        }
    }

    mod cache {
        use super::*;

        /// Returns a process caching in `cache` the runs in `dir`.
        fn cached(dir: &TestDir, cache: &CommandCache) -> ProcessImpl {
            let process = process(dir.path().to_path_buf());
            process.add_interceptor(cache.clone());
            process
        }

        #[test]
        fn cache_should_return_cached_output_of_same_command() {
            // GIVEN a command run once through the cache
            let dir = TestDir::new();
            let cache = CommandCache::default();
            let process = cached(&dir, &cache);
            let first = process.run(&shell("echo out&& touch marker")).unwrap();
            std::fs::remove_file(dir.path().join("marker")).unwrap();

            // WHEN running it again
            let second = process.run(&shell("echo out&& touch marker")).unwrap();

            // THEN its cached output is returned without running it
            assert_eq!(second, first);
            assert!(!dir.path().join("marker").exists());
        }

        #[test]
        fn cache_should_not_cache_failed_replayed_sunk_or_interactive_outputs() {
            // GIVEN a cache after an interceptor replaying an output
            let dir = TestDir::new();
            let cache = CommandCache::default();
            let process = cached(&dir, &cache);
            let mut sunk = (Vec::new(), Vec::new());
            let mut replayed = Interceptors::new();
            replayed.push(Replay(exit(0)));
            replayed.push(cache.clone());

            // WHEN running a failing, a replayed, a sunk and an interactive
            // command
            let _ = process.run(&shell("exit 1"));
            shell("exit 0")
                .interceptors(replayed)
                .run(&process)
                .unwrap();
            process
                .run_to(&shell("echo out"), &mut sink_into(&mut sunk))
                .unwrap();
            shell("exit 0").interactive().run(&process).unwrap();

            // THEN none is cached
            assert!(cache.entries().is_empty());
        }

        #[test]
        fn cache_loaded_should_return_output_instead_of_running() {
            // GIVEN a cache loaded with the output of a command
            let dir = TestDir::new();
            let cache = CommandCache::default();
            let process = cached(&dir, &cache);
            let command = shell("touch marker");
            let key = cache.key(&resolved(&command, &process)).unwrap();
            cache.load([(key, exit(0))]);

            // WHEN running it
            let output = process.run(&command).unwrap();

            // THEN the loaded output is returned without running it
            assert_eq!(output, exit(0));
            assert!(!dir.path().join("marker").exists());
        }

        #[test]
        fn cache_loaded_longer_than_timeout_should_return_output() {
            let dir = TestDir::new();
            let cache = CommandCache::default();
            let process = cached(&dir, &cache);
            let command = shell("exit 1");
            let slow = CommandOutput {
                duration: Duration::from_secs(3),
                ..exit(0)
            };
            let mut run = resolved(&command, &process);
            run.subject_mut().options_mut().timeout = Some(Duration::from_secs(1));
            cache.load([(cache.key(&run).unwrap(), slow.clone())]);

            let output = command.timeout(Duration::from_secs(1)).run(&process);

            assert_eq!(output.unwrap(), slow);
        }

        #[cfg(feature = "tokio")]
        #[test]
        fn cache_async_should_return_cached_output_of_same_command() {
            let dir = TestDir::new();
            let cache = CommandCache::default();
            let process = cached(&dir, &cache);
            let first = block_on(process.run_async(&shell("echo out"))).unwrap();

            let second = block_on(process.run_async(&shell("echo out"))).unwrap();

            assert_eq!(second, first);
            assert_eq!(cache.entries().len(), 1);
        }

        mod with_vars {
            use super::*;

            #[test]
            fn with_vars_should_key_runs_by_named_platform_vars() {
                // GIVEN a cache keyed by a platform variable
                let dir = TestDir::new();
                let cache = CommandCache::with_vars(["AGPLATFORM_CONFIG"]);
                let process = cached(&dir, &cache);

                // WHEN running a command with its values changed in between
                for config in ["a", "b", "a"] {
                    process.set_var("AGPLATFORM_CONFIG".to_string(), config.to_string());
                    process.run(&print_var("AGPLATFORM_CONFIG")).unwrap();
                }

                // THEN the command is cached by each value
                let entries = cache.entries();
                assert_eq!(entries.len(), 2);
                assert!(
                    entries[0]
                        .0
                        .contains("vars: [(\"AGPLATFORM_CONFIG\", \"set\")]"),
                    "{entries:?}"
                );
            }
        }
    }

    mod from {
        use super::*;

        #[allow(clippy::module_inception)]
        mod from {
            use super::*;

            #[test]
            fn from_command_should_run_it_with_default_options() {
                let (process, recorder) = observed();
                let mut command = shell("exit 0");

                process.run(&command).unwrap();
                process.run(&mut command).unwrap();
                process.run(ProcessCommand::new(&command)).unwrap();

                assert_eq!(recorder.options(), CommandOptions::default());
                assert_eq!(recorder.events().len(), 6);
            }
        }
    }

    mod default {
        use super::*;

        #[allow(clippy::module_inception)]
        mod default {
            use super::*;

            #[test]
            fn default_should_key_runs_without_platform_vars() {
                let dir = TestDir::new();
                let cache = CommandCache::default();
                let process = process(dir.path().to_path_buf());
                process.add_interceptor(cache.clone());

                for config in ["a", "b"] {
                    process.set_var("AGPLATFORM_CONFIG".to_string(), config.to_string());
                    process.run(&print_var("AGPLATFORM_CONFIG")).unwrap();
                }

                let entries = cache.entries();
                assert_eq!(entries.len(), 1);
                assert!(entries[0].0.contains("vars: []"), "{entries:?}");
            }
        }
    }

    mod command_ext {
        use super::*;

        mod run {
            use super::*;

            #[test]
            fn run_should_run_command_with_default_options() {
                let (process, recorder) = observed();

                shell("exit 0").run(&process).unwrap();

                assert_eq!(recorder.options(), CommandOptions::default());
            }
        }

        mod run_to {
            use super::*;

            #[test]
            fn run_to_should_run_command_with_default_options_to_sink() {
                let (process, recorder) = observed();
                let mut sunk = (Vec::new(), Vec::new());

                shell("echo out")
                    .run_to(&process, &mut sink_into(&mut sunk))
                    .unwrap();

                assert_eq!(recorder.options(), CommandOptions::default());
                assert_eq!(String::from_utf8_lossy(&sunk.0).trim(), "out");
            }
        }

        #[cfg(feature = "tokio")]
        mod run_async {
            use super::*;

            #[test]
            fn run_async_should_run_command_with_default_options() {
                let (process, recorder) = observed();

                block_on(shell("exit 0").run_async(&process)).unwrap();

                assert_eq!(recorder.options(), CommandOptions::default());
            }
        }

        #[cfg(feature = "tokio")]
        mod run_async_to {
            use super::*;

            #[test]
            fn run_async_to_should_run_command_with_default_options_to_sink() {
                let (process, recorder) = observed();
                let mut sunk = (Vec::new(), Vec::new());

                block_on(shell("echo out").run_async_to(&process, &mut sink_into(&mut sunk)))
                    .unwrap();

                assert_eq!(recorder.options(), CommandOptions::default());
                assert_eq!(String::from_utf8_lossy(&sunk.0).trim(), "out");
            }
        }

        mod clear_env {
            use super::*;

            #[test]
            fn clear_env_should_run_command_without_configured_vars() {
                let (process, recorder) = observed();

                shell("exit 0").clear_env().run(&process).unwrap();

                assert_eq!(
                    recorder.options(),
                    CommandOptions {
                        clear_env: true,
                        ..Default::default()
                    }
                );
            }
        }

        mod display_lines {
            use super::*;

            #[test]
            fn display_lines_should_limit_lines_of_displayed_output() {
                let (process, recorder) = observed();

                let error = shell("echo 1&& echo 2&& echo 3&& exit 1")
                    .display_lines(1)
                    .run(&process)
                    .unwrap_err();

                assert_eq!(recorder.options().display_lines, 1);
                let output = error.command_output().unwrap();
                assert_eq!(output.display_lines, 1);
                assert!(output.to_string().contains("stdout:\n(2 lines omitted)\n3"));
            }
        }

        mod input {
            use super::*;

            #[test]
            fn input_should_run_command_with_input() {
                let (process, recorder) = observed();

                shell("exit 0").input("b\na").run(&process).unwrap();

                assert_eq!(
                    recorder.options(),
                    CommandOptions {
                        input: Some(b"b\na".to_vec()),
                        ..Default::default()
                    }
                );
            }
        }

        mod interactive {
            use super::*;

            #[test]
            fn interactive_should_run_command_interactively() {
                let (process, recorder) = observed();

                shell("exit 0").interactive().run(&process).unwrap();

                assert_eq!(
                    recorder.options(),
                    CommandOptions {
                        interactive: true,
                        ..Default::default()
                    }
                );
            }
        }

        mod silent {
            use super::*;

            #[test]
            fn silent_should_run_command_discarding_output() {
                let (process, recorder) = observed();

                shell("exit 0").silent().run(&process).unwrap();

                assert_eq!(
                    recorder.options(),
                    CommandOptions {
                        silent: true,
                        ..Default::default()
                    }
                );
            }
        }

        mod stream {
            use super::*;

            #[test]
            fn stream_should_run_command_streaming_output() {
                let (process, recorder) = observed();

                shell("exit 0").stream().run(&process).unwrap();

                assert_eq!(
                    recorder.options(),
                    CommandOptions {
                        stream: true,
                        ..Default::default()
                    }
                );
            }
        }

        mod timeout {
            use super::*;

            #[test]
            fn timeout_should_run_command_with_timeout() {
                let (process, recorder) = observed();

                shell("exit 0")
                    .timeout(Duration::from_secs(10))
                    .run(&process)
                    .unwrap();

                assert_eq!(
                    recorder.options(),
                    CommandOptions {
                        timeout: Some(Duration::from_secs(10)),
                        ..Default::default()
                    }
                );
            }
        }

        mod interceptors {
            use super::*;

            #[test]
            fn interceptors_should_run_command_with_own_interceptors_only() {
                let (process, recorder) = observed();
                let own = Recorder::default();
                let mut interceptors = Interceptors::new();
                interceptors.push(own.clone());

                shell("exit 0")
                    .interceptors(interceptors)
                    .run(&process)
                    .unwrap();

                assert!(recorder.events().is_empty());
                assert_eq!(own.events().len(), 2);
            }
        }

        mod with {
            use super::*;

            #[test]
            fn with_should_set_value_for_interceptors() {
                let seen = Arc::new(std::sync::Mutex::new(None));
                let process = process(std::env::temp_dir());
                let found = seen.clone();
                process.add_interceptor(Inspect(move |run: &mut Run<ProcessCommand>| {
                    *found.lock().unwrap() = run.get::<u32>().copied();
                }));

                shell("exit 0").with(7_u32).run(&process).unwrap();

                assert_eq!(*seen.lock().unwrap(), Some(7));
            }
        }
    }

    mod command_output {
        use super::*;

        mod default {
            use super::*;

            #[allow(clippy::module_inception)]
            mod default {
                use super::*;

                #[test]
                fn default_should_be_empty_exit_with_code_0_not_yet_a_success() {
                    let output = CommandOutput::default();

                    assert!(!output.success);
                    assert_eq!(output.status, Some(0));
                    assert!(output.stdout.is_empty() && output.stderr.is_empty());
                    assert_eq!(output.display_lines, 10);
                }

                #[test]
                fn default_replayed_should_succeed_but_returned_by_interceptor_should_fail() {
                    // GIVEN interceptors replaying or returning the default output
                    struct Returning;

                    impl Interceptor<ProcessCommand> for Returning {
                        fn handle<'a>(
                            &'a self,
                            _next: Next<'a, ProcessCommand>,
                        ) -> BoxFuture<'a, Result<CommandOutput>> {
                            Box::pin(std::future::ready(Ok(CommandOutput::default())))
                        }
                    }

                    let replayed = process(std::env::temp_dir());
                    replayed.add_interceptor(crate::interceptor::DryRun);
                    let returned = process(std::env::temp_dir());
                    returned.add_interceptor(Returning);

                    // WHEN running a command through them
                    let replayed = replayed.run(&shell("exit 1"));
                    let error = returned.run(&shell("exit 0")).unwrap_err();

                    // THEN only the replayed one gets its success set by the run
                    assert!(replayed.unwrap().success);
                    assert!(
                        error
                            .description()
                            .ends_with("' was not accepted with exit code 0")
                    );
                }
            }
        }

        mod stdout_str {
            use super::*;

            #[test]
            fn stdout_str_should_return_utf8_output() {
                let output = CommandOutput {
                    stdout: b"out".to_vec(),
                    ..Default::default()
                };

                assert_eq!(output.stdout_str().unwrap(), "out");
            }

            #[test]
            fn stdout_str_invalid_utf8_should_return_error() {
                let output = CommandOutput {
                    stdout: b"\xFF".to_vec(),
                    ..Default::default()
                };

                let error = output.stdout_str().unwrap_err();

                assert_eq!(error.kind(), crate::ErrorKind::Process);
                assert!(
                    error
                        .description()
                        .starts_with("stdout is not valid UTF-8: ")
                );
                assert!(error.source_as::<std::str::Utf8Error>().is_some());
            }
        }

        mod stderr_str {
            use super::*;

            #[test]
            fn stderr_str_should_return_utf8_output() {
                let output = CommandOutput {
                    stderr: b"err".to_vec(),
                    ..Default::default()
                };

                assert_eq!(output.stderr_str().unwrap(), "err");
            }

            #[test]
            fn stderr_str_invalid_utf8_should_return_error() {
                let output = CommandOutput {
                    stderr: b"\xFF".to_vec(),
                    ..Default::default()
                };

                let error = output.stderr_str().unwrap_err();

                assert!(
                    error
                        .description()
                        .starts_with("stderr is not valid UTF-8: ")
                );
            }
        }

        mod stdout_lossy {
            use super::*;

            #[test]
            fn stdout_lossy_should_replace_invalid_utf8() {
                let output = CommandOutput {
                    stdout: b"out\xFF".to_vec(),
                    ..Default::default()
                };

                assert_eq!(output.stdout_lossy(), "out\u{FFFD}");
            }
        }

        mod stderr_lossy {
            use super::*;

            #[test]
            fn stderr_lossy_should_replace_invalid_utf8() {
                let output = CommandOutput {
                    stderr: b"err\xFF".to_vec(),
                    ..Default::default()
                };

                assert_eq!(output.stderr_lossy(), "err\u{FFFD}");
            }
        }

        mod partial_eq {
            use super::*;

            #[test]
            fn eq_should_ignore_success_and_display_lines() {
                let output = CommandOutput {
                    stdout: b"out".to_vec(),
                    ..Default::default()
                };

                assert_eq!(
                    output,
                    CommandOutput {
                        success: false,
                        display_lines: 3,
                        ..output.clone()
                    }
                );
            }

            #[test]
            fn eq_should_compare_data() {
                let output = CommandOutput {
                    success: true,
                    status: Some(0),
                    signal: None,
                    stdout: b"out".to_vec(),
                    stderr: b"err".to_vec(),
                    duration: Duration::from_secs(1),
                    timed_out: false,
                    display_lines: 10,
                };

                for (different, field) in [
                    (
                        CommandOutput {
                            status: Some(1),
                            ..output.clone()
                        },
                        "status",
                    ),
                    (
                        CommandOutput {
                            signal: Some(9),
                            ..output.clone()
                        },
                        "signal",
                    ),
                    (
                        CommandOutput {
                            stdout: Vec::new(),
                            ..output.clone()
                        },
                        "stdout",
                    ),
                    (
                        CommandOutput {
                            stderr: Vec::new(),
                            ..output.clone()
                        },
                        "stderr",
                    ),
                    (
                        CommandOutput {
                            duration: Duration::ZERO,
                            ..output.clone()
                        },
                        "duration",
                    ),
                    (
                        CommandOutput {
                            timed_out: true,
                            ..output.clone()
                        },
                        "timed_out",
                    ),
                ] {
                    assert_ne!(output, different, "field: {field}");
                }
            }
        }

        mod std_fmt_display {
            use super::*;

            #[test]
            fn fmt_should_show_status_duration_and_output() {
                let output = CommandOutput {
                    success: false,
                    status: Some(1),
                    signal: None,
                    stdout: b"out".to_vec(),
                    stderr: b"err".to_vec(),
                    duration: Duration::from_millis(5),
                    timed_out: false,
                    display_lines: 10,
                };

                assert_eq!(
                    output.to_string(),
                    "status: 1\nduration: 5ms\nstdout:\nout\nstderr:\nerr"
                );
            }

            #[test]
            fn fmt_long_output_should_show_last_lines() {
                let output = CommandOutput {
                    stdout: (1..=12)
                        .map(|line| format!("{line}\n"))
                        .collect::<String>()
                        .into_bytes(),
                    ..Default::default()
                };

                assert_eq!(
                    output.to_string(),
                    "status: 0\nduration: 0ns\nstdout:\n(2 lines omitted)\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n\nstderr:\n"
                );
            }

            #[test]
            fn fmt_without_status_should_show_none() {
                let output = CommandOutput {
                    status: None,
                    ..Default::default()
                };

                assert_eq!(
                    output.to_string(),
                    "status: none\nduration: 0ns\nstdout:\n\nstderr:\n"
                );
            }

            #[test]
            fn fmt_with_signal_should_show_signal() {
                let output = CommandOutput {
                    status: None,
                    signal: Some(9),
                    ..Default::default()
                };

                assert_eq!(
                    output.to_string(),
                    "status: signal 9\nduration: 0ns\nstdout:\n\nstderr:\n"
                );
            }
        }

        mod outcome {
            use super::*;

            mod success {
                use super::*;

                #[test]
                fn success_should_return_success_field() {
                    for success in [true, false] {
                        let output = CommandOutput {
                            success,
                            ..Default::default()
                        };

                        assert_eq!(Outcome::success(&output), success);
                    }
                }
            }
        }
    }

    mod std_stream {
        use super::*;

        mod interceptor {
            use super::*;

            mod chunk {
                use super::*;

                #[test]
                fn chunk_should_pass_streamed_and_other_chunks_on() {
                    let recorder = Recorder::default();
                    let process = process(std::env::temp_dir());
                    process.add_interceptor(recorder.clone());
                    let mut interceptors = Interceptors::new();
                    interceptors.push(recorder.clone());
                    interceptors.push(StdStream);

                    for run in [
                        shell("echo out&& echo err 1>&2").stream(),
                        shell("echo out&& echo err 1>&2").interceptors(interceptors),
                    ] {
                        process.run(run).unwrap();
                    }

                    assert_eq!(
                        String::from_utf8_lossy(&recorder.stdout()).lines().count(),
                        2
                    );
                    assert_eq!(
                        String::from_utf8_lossy(&recorder.stderr()).lines().count(),
                        2
                    );
                }
            }
        }
    }

    mod std_executor {
        use super::*;

        mod executor {
            use super::*;

            mod execute {
                use super::*;

                #[test]
                fn execute_should_run_attempts_of_retried_command() {
                    // GIVEN a command failing on its first attempt only
                    let dir = TestDir::new();
                    let (process, recorder) = observed();
                    process.set_current_dir(dir.path().to_path_buf());

                    // WHEN running it with retries
                    let output = shell(FAIL_ONCE)
                        .interceptors(retried(Retry::new(2, Duration::from_millis(1)), &recorder))
                        .run(&process)
                        .unwrap();

                    // THEN the second attempt succeeds
                    assert_eq!(output.status, Some(0));
                    assert_eq!(
                        recorder.events()[1..],
                        [
                            "finished failed Some(1)".to_string(),
                            format!("started in {}", dir.path().display()),
                            "finished ok".to_string()
                        ]
                    );
                }
            }

            mod sleep {
                use super::*;

                #[test]
                fn sleep_should_block_thread_for_delay() {
                    let start = Instant::now();

                    let _ = shell("exit 1")
                        .interceptors(retried(
                            Retry::new(1, Duration::from_millis(50)),
                            &Recorder::default(),
                        ))
                        .run(&process(std::env::temp_dir()));

                    assert!(start.elapsed() >= Duration::from_millis(50));
                }
            }
        }
    }

    #[cfg(feature = "tokio")]
    mod tokio_executor {
        use super::*;

        mod executor {
            use super::*;

            mod execute {
                use super::*;

                #[test]
                fn execute_should_run_attempts_of_retried_command() {
                    let dir = TestDir::new();
                    let (process, recorder) = observed();
                    process.set_current_dir(dir.path().to_path_buf());

                    let output = block_on(
                        shell(FAIL_ONCE)
                            .interceptors(retried(
                                Retry::new(2, Duration::from_millis(1)),
                                &recorder,
                            ))
                            .run_async(&process),
                    )
                    .unwrap();

                    assert_eq!(output.status, Some(0));
                    assert_eq!(recorder.events().len(), 4);
                }
            }

            mod sleep {
                use super::*;

                #[test]
                fn sleep_should_wait_for_delay() {
                    let start = Instant::now();

                    let _ = block_on(
                        shell("exit 1")
                            .interceptors(retried(
                                Retry::new(1, Duration::from_millis(50)),
                                &Recorder::default(),
                            ))
                            .run_async(&process(std::env::temp_dir())),
                    );

                    assert!(start.elapsed() >= Duration::from_millis(50));
                }

                #[test]
                fn sleep_cancelled_should_tell_interceptors() {
                    // GIVEN a recorder outside a retry waiting long after a failure
                    let (process, recorder) = observed();
                    let mut interceptors = Interceptors::new();
                    interceptors.push(recorder.clone());
                    interceptors.push(Retry::new(1, Duration::from_secs(3600)));

                    // WHEN the run is cancelled while it waits
                    let result = block_on(async {
                        tokio::time::timeout(
                            Duration::from_millis(200),
                            shell("exit 1")
                                .interceptors(interceptors)
                                .run_async(&process),
                        )
                        .await
                    });

                    // THEN the interceptors are told
                    assert!(result.is_err());
                    assert_eq!(
                        *recorder.events().last().unwrap(),
                        format!("cancelled {SHELL}")
                    );
                }
            }
        }
    }

    mod process_impl {
        use super::*;

        mod process {
            use super::*;

            mod run {
                use super::*;

                #[test]
                fn run_should_capture_output() {
                    let process = process(std::env::temp_dir());

                    let output = process.run(&shell("echo out&& echo err 1>&2")).unwrap();

                    assert_eq!(output.status, Some(0));
                    assert_eq!(output.stdout_str().unwrap().trim(), "out");
                    assert_eq!(output.stderr_str().unwrap().trim(), "err");
                    assert!(!output.timed_out);
                }

                #[cfg(unix)]
                #[test]
                fn run_killed_by_signal_should_report_signal() {
                    let process = process(std::env::temp_dir());

                    let error = process.run(&shell("kill -9 $$")).unwrap_err();

                    assert_eq!(error.description(), "'sh' was terminated by signal 9");
                    assert_eq!(error.command_output().unwrap().signal, Some(9));
                }

                #[cfg(unix)]
                #[test]
                fn run_large_input_should_be_written_whole() {
                    let process = process(std::env::temp_dir());
                    let input = vec![b'x'; 1 << 20];

                    let output = process
                        .run(Run::from(&shell("wc -c")).input(input))
                        .unwrap();

                    assert_eq!(output.stdout_str().unwrap().trim(), "1048576");
                }

                #[test]
                fn run_streaming_should_capture_output() {
                    let process = process(std::env::temp_dir());
                    let options = CommandOptions {
                        display_lines: 10,
                        clear_env: false,
                        silent: false,
                        input: None,
                        interactive: false,
                        stream: true,
                        timeout: None,
                    };

                    let output = process
                        .run(with_options(
                            &shell("echo out&& echo err 1>&2"),
                            options.clone(),
                        ))
                        .unwrap();

                    assert_eq!(output.stdout_str().unwrap().trim(), "out");
                    assert_eq!(output.stderr_str().unwrap().trim(), "err");
                }

                #[test]
                fn run_streaming_should_pass_output_to_interceptors() {
                    let (process, recorder) = observed();
                    let options = CommandOptions {
                        stream: true,
                        ..Default::default()
                    };

                    let output = process
                        .run(with_options(
                            &shell("echo out&& echo err 1>&2"),
                            options.clone(),
                        ))
                        .unwrap();

                    assert_eq!(recorder.stdout(), output.stdout);
                    assert_eq!(recorder.stderr(), output.stderr);
                    assert!(!output.stdout.is_empty());
                }

                #[test]
                fn run_silent_should_neither_capture_nor_stream_output() {
                    let (process, recorder) = observed();
                    let options = CommandOptions {
                        silent: true,
                        stream: true,
                        ..Default::default()
                    };

                    let output = process
                        .run(with_options(
                            &shell("echo out&& echo err 1>&2"),
                            options.clone(),
                        ))
                        .unwrap();

                    assert_eq!(output.status, Some(0));
                    assert!(output.stdout.is_empty());
                    assert!(output.stderr.is_empty());
                    assert!(recorder.stdout().is_empty());
                    assert!(recorder.stderr().is_empty());
                }

                #[test]
                fn run_should_pass_run_and_result_to_interceptors() {
                    let (process, recorder) = observed();

                    process.run(&shell("echo out")).unwrap();
                    let _ = process.run(&shell("exit 3"));

                    let events = recorder.events();
                    let dir = std::env::temp_dir();
                    assert_eq!(events[0], format!("started in {}", dir.display()));
                    assert_eq!(events[1], "finished ok");
                    assert_eq!(events[2], format!("started in {}", dir.display()));
                    assert_eq!(events[3], "finished failed Some(3)");
                    assert_eq!(String::from_utf8_lossy(&recorder.stdout()).trim(), "out");
                }

                #[test]
                fn run_should_run_in_current_dir() {
                    let dir = TestDir::new();
                    let process = process(dir.path().to_path_buf());

                    let output = process.run(&shell(PRINT_DIR)).unwrap();

                    assert_eq!(
                        std::fs::canonicalize(output.stdout_str().unwrap().trim()).unwrap(),
                        std::fs::canonicalize(dir.path()).unwrap()
                    );
                }

                #[test]
                fn run_relative_dir_should_resolve_against_current_dir() {
                    // GIVEN a sub directory of the current directory
                    let dir = TestDir::new();
                    std::fs::create_dir(dir.path().join("sub")).unwrap();
                    let process = process(dir.path().to_path_buf());

                    // WHEN running a command in it by a relative path
                    let output = process.run(shell(PRINT_DIR).current_dir("sub")).unwrap();

                    // THEN the command runs in the sub directory
                    assert_eq!(
                        std::fs::canonicalize(output.stdout_str().unwrap().trim()).unwrap(),
                        std::fs::canonicalize(dir.path().join("sub")).unwrap()
                    );
                }

                #[test]
                fn run_relative_program_should_resolve_against_dir() {
                    // GIVEN a program in a sub directory of the current directory
                    let dir = TestDir::new();
                    std::fs::create_dir(dir.path().join("sub")).unwrap();
                    write_tool(&dir.path().join("sub"));
                    let process = process(dir.path().to_path_buf());

                    // WHEN running it by a path relative to the sub directory
                    // it runs in
                    let output = process
                        .run(Command::new(Path::new(".").join(TOOL)).current_dir("sub"))
                        .unwrap();

                    // THEN it is found there
                    assert_eq!(output.stdout_str().unwrap().trim(), "tool");
                }

                #[test]
                fn run_should_pass_vars_of_platform() {
                    let process = ProcessImpl::new(std::env::temp_dir());

                    let output = process.run(&print_var("PATH")).unwrap();

                    assert_ne!(output.stdout_str().unwrap().trim(), "unset");
                }

                #[test]
                fn run_unchanged_vars_of_process_should_inherit_them() {
                    // GIVEN a platform with the unchanged variables of the process
                    let process = ProcessImpl::with_env(EnvImpl::new_from_std());

                    // WHEN running commands with and without own variables
                    let outputs = [
                        process.run(&print_var(INHERITED_VAR)).unwrap(),
                        process
                            .run(print_var(INHERITED_VAR).env("AGPLATFORM_VAR", "1"))
                            .unwrap(),
                    ];

                    // THEN they get the variables of the process
                    let expected = std::env::var(INHERITED_VAR).unwrap();
                    for output in outputs {
                        assert_eq!(output.stdout_str().unwrap().trim(), expected);
                    }
                }

                #[test]
                fn run_should_not_pass_removed_inherited_var() {
                    let process = process(std::env::temp_dir());
                    process.remove_var(INHERITED_VAR);

                    let output = process.run(&print_var(INHERITED_VAR)).unwrap();

                    assert_eq!(output.stdout_str().unwrap().trim(), "unset");
                }

                #[test]
                fn run_should_pass_vars() {
                    let process = process(std::env::temp_dir());
                    process.set_var("AGPLATFORM_VAR".to_string(), "value".to_string());

                    let output = process.run(&print_var("AGPLATFORM_VAR")).unwrap();

                    assert_eq!(output.stdout_str().unwrap().trim(), "value");
                }

                #[test]
                fn run_command_vars_should_override_vars() {
                    // GIVEN configured variables
                    let process = process(std::env::temp_dir());
                    process.set_var("AGPLATFORM_SET".to_string(), "1".to_string());
                    process.set_var("AGPLATFORM_REMOVED".to_string(), "1".to_string());

                    // WHEN commands set or remove them
                    let set = process
                        .run(print_var("AGPLATFORM_SET").env("AGPLATFORM_SET", "2"))
                        .unwrap();
                    let removed = process
                        .run(print_var("AGPLATFORM_REMOVED").env_remove("AGPLATFORM_REMOVED"))
                        .unwrap();

                    // THEN the commands win
                    assert_eq!(set.stdout_str().unwrap().trim(), "2");
                    assert_eq!(removed.stdout_str().unwrap().trim(), "unset");
                }

                #[test]
                fn run_clear_env_should_pass_only_command_vars() {
                    // GIVEN a configured variable
                    let process = process(std::env::temp_dir());
                    process.set_var("AGPLATFORM_CONFIGURED".to_string(), "1".to_string());
                    let options = CommandOptions {
                        display_lines: 10,
                        clear_env: true,
                        ..Default::default()
                    };

                    // WHEN running commands without the configured variables
                    let configured = process
                        .run(with_options(
                            &print_var("AGPLATFORM_CONFIGURED"),
                            options.clone(),
                        ))
                        .unwrap();
                    let own = process
                        .run(with_options(
                            print_var("AGPLATFORM_OWN").env("AGPLATFORM_OWN", "2"),
                            options.clone(),
                        ))
                        .unwrap();

                    // THEN only the variables of the commands are passed
                    assert_eq!(configured.stdout_str().unwrap().trim(), "unset");
                    assert_eq!(own.stdout_str().unwrap().trim(), "2");
                }

                #[cfg(unix)]
                #[test]
                fn run_changed_path_should_find_program_in_it() {
                    // GIVEN a program found only in a directory
                    let dir = TestDir::new();
                    write_tool(dir.path());
                    let path = dir.path().display().to_string();
                    let configured = process(std::env::temp_dir());
                    configured.set_var("PATH".to_string(), path.clone());
                    let unconfigured = process(std::env::temp_dir());
                    let mut own = Command::new(TOOL);
                    own.env("PATH", &path);

                    // WHEN the platform or the command sets the PATH to it,
                    // with and without the configured variables
                    let outputs = [
                        configured.run(&Command::new(TOOL)),
                        configured.run(Command::new(TOOL).clear_env()),
                        unconfigured.run(&own),
                        unconfigured.run(own.clear_env()),
                    ];

                    // THEN the program is found
                    for (index, output) in outputs.into_iter().enumerate() {
                        assert_eq!(
                            output.unwrap().stdout_str().unwrap().trim(),
                            "tool",
                            "case {index}"
                        );
                    }
                }

                #[cfg(unix)]
                #[test]
                fn run_without_path_should_find_program_in_default_path() {
                    // GIVEN a platform without a PATH and one whose command
                    // removes it
                    let without =
                        ProcessImpl::with_env(EnvImpl::new(Vec::new(), std::env::temp_dir()));
                    let process = process(std::env::temp_dir());

                    // WHEN running a program by its bare name
                    let outputs = [
                        without.run(Command::new("sh").args(["-c", "echo found"])),
                        process.run(
                            Command::new("sh")
                                .args(["-c", "echo found"])
                                .env_remove("PATH"),
                        ),
                    ];

                    // THEN std finds it in the default path of the OS
                    for (index, output) in outputs.into_iter().enumerate() {
                        assert_eq!(
                            output.unwrap().stdout_str().unwrap().trim(),
                            "found",
                            "case {index}"
                        );
                    }
                }

                #[cfg(unix)]
                #[test]
                fn run_missing_program_in_changed_path_should_return_not_found() {
                    let dir = TestDir::new();
                    let process = process(std::env::temp_dir());
                    process.set_var("PATH".to_string(), dir.path().display().to_string());

                    let error = process
                        .run(&Command::new("agplatform_missing_program"))
                        .unwrap_err();

                    assert_eq!(
                        error.description(),
                        "'agplatform_missing_program': No such file or directory (os error 2)"
                    );
                    assert_eq!(
                        error
                            .source_as::<std::io::Error>()
                            .map(std::io::Error::kind),
                        Some(std::io::ErrorKind::NotFound)
                    );
                }

                #[test]
                fn run_input_should_write_input_to_stdin() {
                    let process = process(std::env::temp_dir());
                    let options = CommandOptions {
                        input: Some(b"b\na\n".to_vec()),
                        ..Default::default()
                    };

                    let output = process
                        .run(with_options(&echo_stdin(), options.clone()))
                        .unwrap();

                    assert_eq!(
                        output.stdout_str().unwrap().lines().collect::<Vec<_>>(),
                        ["b", "a"]
                    );
                }

                #[test]
                fn run_input_larger_than_pipe_should_not_deadlock() {
                    // GIVEN input exceeding the pipe buffers
                    let process = process(std::env::temp_dir());
                    let options = CommandOptions {
                        input: Some("a\n".repeat(500_000).into_bytes()),
                        ..Default::default()
                    };

                    // WHEN the command echoes it back while reading
                    let output = process
                        .run(with_options(&echo_stdin(), options.clone()))
                        .unwrap();

                    // THEN it is read and output whole
                    assert_eq!(output.stdout_str().unwrap().lines().count(), 500_000);
                }

                #[test]
                fn run_input_not_read_should_succeed() {
                    let process = process(std::env::temp_dir());
                    let options = CommandOptions {
                        input: Some("a\n".repeat(500_000).into_bytes()),
                        ..Default::default()
                    };

                    let output = process
                        .run(with_options(&shell("exit 0"), options.clone()))
                        .unwrap();

                    assert_eq!(output.status, Some(0));
                }

                #[test]
                fn run_interactive_should_not_capture_output() {
                    let process = process(std::env::temp_dir());
                    let options = CommandOptions {
                        interactive: true,
                        ..Default::default()
                    };

                    let error = process
                        .run(with_options(&shell("exit 3"), options.clone()))
                        .unwrap_err();

                    let output = error.command_output().unwrap();
                    assert_eq!(output.status, Some(3));
                    assert!(output.stdout.is_empty());
                    assert!(output.stderr.is_empty());
                }

                #[test]
                fn run_invalid_options_should_return_error() {
                    let process = process(std::env::temp_dir());
                    let options = CommandOptions {
                        interactive: true,
                        input: Some(Vec::new()),
                        ..Default::default()
                    };

                    let error = process
                        .run(with_options(&shell("exit 0"), options.clone()))
                        .unwrap_err();

                    assert!(
                        error
                            .description()
                            .ends_with("': interactive cannot be combined with input")
                    );
                }

                #[test]
                fn run_should_leave_command_unchanged() {
                    let process = process(std::env::temp_dir());
                    let command = shell("echo out");

                    process.run(&command).unwrap();

                    assert_eq!(command.get_current_dir(), None);
                    assert_eq!(command.get_envs().count(), 0);
                }

                #[test]
                fn run_non_zero_exit_should_return_error_with_output() {
                    let process = process(std::env::temp_dir());

                    let error = process.run(&shell("echo out&& exit 3")).unwrap_err();

                    let output = error.command_output().unwrap();
                    assert_eq!(error.kind(), crate::ErrorKind::Process);
                    assert_eq!(output.status, Some(3));
                    assert_eq!(output.stdout_str().unwrap().trim(), "out");
                }

                #[test]
                fn run_missing_program_should_return_error_with_io_source() {
                    let process = process(std::env::temp_dir());

                    let error = process
                        .run(&Command::new("agplatform_missing_program"))
                        .unwrap_err();

                    assert_eq!(error.kind(), crate::ErrorKind::Process);
                    assert!(
                        error
                            .description()
                            .starts_with("'agplatform_missing_program': ")
                    );
                    assert_eq!(
                        error
                            .source()
                            .and_then(|source| source.downcast_ref::<std::io::Error>())
                            .map(std::io::Error::kind),
                        Some(std::io::ErrorKind::NotFound)
                    );
                }

                #[test]
                fn run_within_timeout_should_succeed() {
                    let process = process(std::env::temp_dir());
                    let options = CommandOptions {
                        display_lines: 10,
                        clear_env: false,
                        silent: false,
                        input: None,
                        interactive: false,
                        stream: false,
                        timeout: Some(Duration::from_secs(10)),
                    };

                    let output = process
                        .run(with_options(&shell("echo out"), options.clone()))
                        .unwrap();

                    assert_eq!(output.stdout_str().unwrap().trim(), "out");
                    assert!(!output.timed_out);
                }

                #[test]
                fn run_maximum_timeout_should_not_limit_run() {
                    let process = process(std::env::temp_dir());

                    let output = process
                        .run(shell("echo out").timeout(Duration::MAX))
                        .unwrap();

                    assert_eq!(output.stdout_str().unwrap().trim(), "out");
                }

                #[test]
                fn run_exited_child_should_not_wait_for_descendants_holding_output() {
                    // GIVEN a child that exits leaving a sleeping descendant
                    // holding its output open
                    let process = process(std::env::temp_dir());
                    let command = shell(BACKGROUND_SLEEPER);

                    for timeout in [None, Some(Duration::from_secs(10))] {
                        let options = CommandOptions {
                            timeout,
                            ..Default::default()
                        };

                        // WHEN running it
                        let output = process
                            .run(with_options(&command, options.clone()))
                            .unwrap();
                        kill_sleeper(output.stdout_str().unwrap());

                        // THEN the run ends with the child, with the output
                        // before its exit
                        assert_eq!(output.status, Some(0), "timeout: {timeout:?}");
                        assert!(
                            output.stdout_str().unwrap().trim().parse::<u32>().is_ok(),
                            "timeout: {timeout:?}"
                        );
                        assert!(
                            output.duration < EXITED_CHILD_WAIT_MAX,
                            "timeout: {timeout:?}"
                        );
                    }
                }

                #[cfg(target_os = "linux")]
                #[test]
                fn run_timeout_exited_parent_should_kill_child() {
                    const PARENT_DIR: &str = "AGPLATFORM_TEST_PARENT_DIR";

                    if let Some(dir) = std::env::var_os(PARENT_DIR) {
                        exit_while_running_child(PathBuf::from(dir));
                    }

                    // GIVEN a parent process running a child with a timeout,
                    // which holds a fifo open
                    let dir = TestDir::new();
                    assert!(
                        Command::new("mkfifo")
                            .arg(dir.path().join("fifo"))
                            .status()
                            .unwrap()
                            .success()
                    );
                    let test = format!(
                        "{}::run_timeout_exited_parent_should_kill_child",
                        module_path!().split_once("::").unwrap().1
                    );
                    let mut parent = Command::new(std::env::current_exe().unwrap())
                        .args([test.as_str(), "--exact"])
                        .env(PARENT_DIR, dir.path())
                        .stdout(Stdio::null())
                        .spawn()
                        .unwrap();
                    let mut fifo = std::fs::File::open(dir.path().join("fifo")).unwrap();
                    let start = Instant::now();

                    // WHEN the parent exits without ending the run
                    assert!(parent.wait().unwrap().success());

                    // THEN the child is killed, closing the fifo
                    std::io::Read::read_to_end(&mut fifo, &mut Vec::new()).unwrap();
                    assert!(start.elapsed() < Duration::from_secs(5));
                }

                #[test]
                fn run_exceeding_timeout_should_kill_process_tree() {
                    // GIVEN a shell whose sleeping child holds its output open
                    let process = process(std::env::temp_dir());
                    let options = CommandOptions {
                        display_lines: 10,
                        clear_env: false,
                        silent: false,
                        input: None,
                        interactive: false,
                        stream: false,
                        timeout: Some(Duration::from_millis(200)),
                    };

                    // WHEN it exceeds the timeout
                    let error = process
                        .run(with_options(&shell(SLEEP_THEN_ECHO), options.clone()))
                        .unwrap_err();

                    // THEN both are killed so the output ends right away
                    let output = error.command_output().unwrap();
                    assert!(output.timed_out);
                    assert_eq!(output.status, None);
                    assert!(output.stdout.is_empty());
                    assert!(output.duration < Duration::from_secs(5));
                }
            }

            #[cfg(feature = "tokio")]
            mod run_async {
                use super::*;

                #[test]
                fn run_async_should_capture_output() {
                    let process = process(std::env::temp_dir());

                    let output =
                        block_on(process.run_async(&shell("echo out&& echo err 1>&2"))).unwrap();

                    assert_eq!(output.status, Some(0));
                    assert_eq!(output.stdout_str().unwrap().trim(), "out");
                    assert_eq!(output.stderr_str().unwrap().trim(), "err");
                    assert!(!output.timed_out);
                }

                #[test]
                fn run_async_streaming_should_pass_output_to_interceptors() {
                    let (process, recorder) = observed();
                    let options = CommandOptions {
                        stream: true,
                        ..Default::default()
                    };

                    let output = block_on(process.run_async(with_options(
                        &shell("echo out&& echo err 1>&2"),
                        options.clone(),
                    )))
                    .unwrap();

                    assert_eq!(recorder.stdout(), output.stdout);
                    assert_eq!(recorder.stderr(), output.stderr);
                    assert_eq!(output.stdout_str().unwrap().trim(), "out");
                }

                #[test]
                fn run_async_silent_should_neither_capture_nor_stream_output() {
                    let (process, recorder) = observed();
                    let options = CommandOptions {
                        silent: true,
                        stream: true,
                        ..Default::default()
                    };

                    let output = block_on(process.run_async(with_options(
                        &shell("echo out&& echo err 1>&2"),
                        options.clone(),
                    )))
                    .unwrap();

                    assert!(output.stdout.is_empty());
                    assert!(output.stderr.is_empty());
                    assert!(recorder.stdout().is_empty());
                }

                #[test]
                fn run_async_input_should_write_input_to_stdin() {
                    let process = process(std::env::temp_dir());
                    let options = CommandOptions {
                        input: Some(b"b\na\n".to_vec()),
                        ..Default::default()
                    };

                    let output =
                        block_on(process.run_async(with_options(&echo_stdin(), options.clone())))
                            .unwrap();

                    assert_eq!(
                        output.stdout_str().unwrap().lines().collect::<Vec<_>>(),
                        ["b", "a"]
                    );
                }

                #[test]
                fn run_async_input_larger_than_pipe_should_not_deadlock() {
                    // GIVEN input exceeding the pipe buffers
                    let process = process(std::env::temp_dir());
                    let options = CommandOptions {
                        input: Some("a\n".repeat(500_000).into_bytes()),
                        ..Default::default()
                    };

                    // WHEN the command echoes it back while reading
                    let output =
                        block_on(process.run_async(with_options(&echo_stdin(), options.clone())))
                            .unwrap();

                    // THEN it is read and output whole
                    assert_eq!(output.stdout_str().unwrap().lines().count(), 500_000);
                }

                #[test]
                fn run_async_input_not_read_should_succeed() {
                    let process = process(std::env::temp_dir());
                    let options = CommandOptions {
                        input: Some("a\n".repeat(500_000).into_bytes()),
                        ..Default::default()
                    };

                    let output = block_on(
                        process.run_async(with_options(&shell("exit 0"), options.clone())),
                    )
                    .unwrap();

                    assert_eq!(output.status, Some(0));
                }

                #[test]
                fn run_async_interactive_should_not_capture_output() {
                    let process = process(std::env::temp_dir());
                    let options = CommandOptions {
                        interactive: true,
                        ..Default::default()
                    };

                    let error = block_on(
                        process.run_async(with_options(&shell("exit 3"), options.clone())),
                    )
                    .unwrap_err();

                    let output = error.command_output().unwrap();
                    assert_eq!(output.status, Some(3));
                    assert!(output.stdout.is_empty());
                }

                #[test]
                fn run_async_invalid_options_should_return_error() {
                    let (process, recorder) = observed();
                    let options = CommandOptions {
                        interactive: true,
                        timeout: Some(Duration::from_secs(1)),
                        ..Default::default()
                    };

                    let error = block_on(
                        process.run_async(with_options(&shell("exit 0"), options.clone())),
                    )
                    .unwrap_err();

                    assert!(
                        error
                            .description()
                            .ends_with("': interactive cannot be combined with timeout")
                    );
                    assert_eq!(recorder.events()[1], format!("finished {error}"));
                }

                #[test]
                fn run_async_missing_program_should_return_error_with_io_source() {
                    let process = process(std::env::temp_dir());

                    let error =
                        block_on(process.run_async(&Command::new("agplatform_missing_program")))
                            .unwrap_err();

                    assert!(
                        error
                            .description()
                            .starts_with("'agplatform_missing_program': ")
                    );
                    assert_eq!(
                        error
                            .source_as::<std::io::Error>()
                            .map(std::io::Error::kind),
                        Some(std::io::ErrorKind::NotFound)
                    );
                }

                #[test]
                fn run_async_within_timeout_should_succeed() {
                    let process = process(std::env::temp_dir());
                    let options = CommandOptions {
                        timeout: Some(Duration::from_secs(10)),
                        ..Default::default()
                    };

                    let output = block_on(
                        process.run_async(with_options(&shell("echo out"), options.clone())),
                    )
                    .unwrap();

                    assert_eq!(output.stdout_str().unwrap().trim(), "out");
                    assert!(!output.timed_out);
                }

                #[test]
                fn run_async_maximum_timeout_should_not_limit_run() {
                    let process = process(std::env::temp_dir());

                    let output =
                        block_on(process.run_async(shell("echo out").timeout(Duration::MAX)))
                            .unwrap();

                    assert_eq!(output.stdout_str().unwrap().trim(), "out");
                }

                #[test]
                fn run_async_exited_child_should_not_wait_for_descendants_holding_output() {
                    // GIVEN a child that exits leaving a sleeping descendant
                    // holding its output open
                    let process = process(std::env::temp_dir());
                    let command = shell(BACKGROUND_SLEEPER);

                    for timeout in [None, Some(Duration::from_secs(10))] {
                        let options = CommandOptions {
                            timeout,
                            ..Default::default()
                        };

                        // WHEN running it
                        let output =
                            block_on(process.run_async(with_options(&command, options.clone())))
                                .unwrap();
                        kill_sleeper(output.stdout_str().unwrap());

                        // THEN the run ends with the child, with the output
                        // before its exit
                        assert_eq!(output.status, Some(0), "timeout: {timeout:?}");
                        assert!(
                            output.stdout_str().unwrap().trim().parse::<u32>().is_ok(),
                            "timeout: {timeout:?}"
                        );
                        assert!(
                            output.duration < EXITED_CHILD_WAIT_MAX,
                            "timeout: {timeout:?}"
                        );
                    }
                }

                #[test]
                fn run_async_exceeding_timeout_should_kill_process_tree_keeping_output() {
                    // GIVEN a shell printing before its sleeping child holds its
                    // output open
                    let process = process(std::env::temp_dir());
                    let options = CommandOptions {
                        timeout: Some(Duration::from_millis(500)),
                        ..Default::default()
                    };

                    // WHEN it exceeds the timeout
                    let error = block_on(
                        process.run_async(with_options(&shell(ECHO_THEN_SLEEP), options.clone())),
                    )
                    .unwrap_err();

                    // THEN both are killed and the output so far is kept
                    let output = error.command_output().unwrap();
                    assert!(output.timed_out);
                    assert_eq!(output.status, None);
                    assert_eq!(output.stdout_str().unwrap().trim(), "out");
                    assert!(output.duration < Duration::from_secs(5));
                }

                #[test]
                fn run_async_cancelled_should_kill_process_tree() {
                    // GIVEN a child with a descendant, which would create a
                    // marker later
                    let dir = TestDir::new();
                    let process = process(dir.path().to_path_buf());
                    let command = shell(MARK_LATER_THEN_SLEEP);

                    // WHEN the run is cancelled by dropping it
                    let result = block_on(async {
                        tokio::time::timeout(
                            Duration::from_millis(200),
                            process.run_async(&command),
                        )
                        .await
                    });
                    std::thread::sleep(AFTER_MARKER);

                    // THEN the descendant was killed before creating the marker
                    assert!(result.is_err());
                    assert!(!dir.path().join("marker").exists());
                }

                #[test]
                fn run_async_cancelled_should_tell_interceptors() {
                    let (process, recorder) = observed();
                    let command = shell(SLEEP_THEN_ECHO);

                    let result = block_on(async {
                        tokio::time::timeout(
                            Duration::from_millis(200),
                            process.run_async(&command),
                        )
                        .await
                    });

                    assert!(result.is_err());
                    assert_eq!(
                        *recorder.events().last().unwrap(),
                        format!("cancelled {SHELL}")
                    );
                }
            }

            mod run_to {
                use super::*;

                #[test]
                fn run_to_should_pass_output_to_sink_instead_of_capturing() {
                    let process = process(std::env::temp_dir());
                    let mut sunk = (Vec::new(), Vec::new());

                    let output = process
                        .run_to(
                            &shell("echo out&& echo err 1>&2"),
                            &mut sink_into(&mut sunk),
                        )
                        .unwrap();

                    assert_eq!(output.status, Some(0));
                    assert!(output.stdout.is_empty() && output.stderr.is_empty());
                    assert_eq!(String::from_utf8_lossy(&sunk.0).trim(), "out");
                    assert_eq!(String::from_utf8_lossy(&sunk.1).trim(), "err");
                }

                #[test]
                fn run_to_should_pass_output_through_interceptors_to_sink() {
                    let (process, recorder) = observed();
                    let mut sunk = (Vec::new(), Vec::new());

                    process
                        .run_to(shell("echo out").stream(), &mut sink_into(&mut sunk))
                        .unwrap();

                    assert_eq!(recorder.stdout(), sunk.0);
                    assert!(!sunk.0.is_empty());
                }

                #[test]
                fn run_to_failing_sink_should_kill_child_and_return_sink_error() {
                    // GIVEN a command that writes and then runs for long
                    let process = process(std::env::temp_dir());
                    let command = shell(ECHO_THEN_SLEEP);
                    let start = Instant::now();

                    // WHEN its sink fails on the first chunk
                    let error = process
                        .run_to(&command, &mut |_, _| Err(crate::Error::process("full")))
                        .unwrap_err();

                    // THEN the run ends at once with the error of the sink
                    assert_eq!(error.description(), "full");
                    assert!(start.elapsed() < Duration::from_secs(5));
                }

                #[test]
                fn run_to_silent_should_fail_without_running() {
                    let process = process(std::env::temp_dir());
                    let mut sunk = (Vec::new(), Vec::new());

                    let error = process
                        .run_to(shell("echo out").silent(), &mut sink_into(&mut sunk))
                        .unwrap_err();

                    assert!(
                        error
                            .description()
                            .ends_with("': silent cannot be combined with a sink")
                    );
                    assert!(sunk.0.is_empty());
                }
            }

            #[cfg(feature = "tokio")]
            mod run_async_to {
                use super::*;

                #[test]
                fn run_async_to_should_pass_output_to_sink_instead_of_capturing() {
                    let process = process(std::env::temp_dir());
                    let mut sunk = (Vec::new(), Vec::new());

                    let output = block_on(process.run_async_to(
                        &shell("echo out&& echo err 1>&2"),
                        &mut sink_into(&mut sunk),
                    ))
                    .unwrap();

                    assert!(output.stdout.is_empty() && output.stderr.is_empty());
                    assert_eq!(String::from_utf8_lossy(&sunk.0).trim(), "out");
                    assert_eq!(String::from_utf8_lossy(&sunk.1).trim(), "err");
                }

                #[test]
                fn run_async_to_failing_sink_should_kill_child_and_return_sink_error() {
                    let process = process(std::env::temp_dir());
                    let command = shell(ECHO_THEN_SLEEP);
                    let start = Instant::now();

                    let error =
                        block_on(process.run_async_to(&command, &mut |_, _| {
                            Err(crate::Error::process("full"))
                        }))
                        .unwrap_err();

                    assert_eq!(error.description(), "full");
                    assert!(start.elapsed() < Duration::from_secs(5));
                }
            }

            mod interceptors {
                use super::*;

                #[test]
                fn interceptors_should_return_std_stream_by_default() {
                    let process = process(std::env::temp_dir());

                    let interceptors = process.interceptors();

                    assert_eq!(interceptors.len(), 1);
                    assert_eq!(interceptors, process.interceptors());
                }
            }

            mod clear_interceptors {
                use super::*;

                #[test]
                fn clear_interceptors_should_remove_interceptors_of_later_runs() {
                    let (process, recorder) = observed();

                    process.clear_interceptors();
                    process.run(&shell("exit 0")).unwrap();

                    assert!(process.interceptors().is_empty());
                    assert!(recorder.events().is_empty());
                }
            }

            mod add_interceptor {
                use super::*;

                #[test]
                fn add_interceptor_should_add_innermost_interceptor() {
                    // GIVEN a recorder after an interceptor accepting exit code 3
                    let process = process(std::env::temp_dir());
                    let recorder = Recorder::default();
                    process.clear_interceptors();
                    process.add_interceptor(Accept(3));
                    process.add_interceptor(recorder.clone());

                    // WHEN running a command exiting with it
                    let output = process.run(&shell("exit 3")).unwrap();

                    // THEN the recorder sees the failure before it is accepted
                    assert!(output.success);
                    assert_eq!(recorder.events()[1], "finished failed Some(3)");
                }
            }
        }
    }

    mod failure {
        use super::*;

        #[test]
        fn failure_should_return_error_with_output() {
            for (status, signal, timed_out, description) in [
                (Some(1), None, false, "'git' failed with exit code 1"),
                (
                    Some(0),
                    None,
                    false,
                    "'git' was not accepted with exit code 0",
                ),
                (
                    None,
                    None,
                    false,
                    "'git' was terminated without an exit code",
                ),
                (None, Some(9), false, "'git' was terminated by signal 9"),
                (None, Some(9), true, "'git' timed out after 1s"),
            ] {
                let output = CommandOutput {
                    status,
                    signal,
                    timed_out,
                    duration: Duration::from_secs(1),
                    ..Default::default()
                };

                let error = failure(&ProcessCommand::new(&Command::new("git")), output.clone());

                assert_eq!(error.description(), description);
                assert_eq!(error.command_output(), Some(&output), "{description}");
                assert_eq!(error.program(), Some("git"), "{description}");
            }
        }
    }

    mod write_flushed {
        use super::*;

        #[test]
        fn write_flushed_failing_should_return_error() {
            let mut buffer = [0_u8; 2];

            let error = write_flushed(&mut buffer.as_mut_slice(), b"data").unwrap_err();

            assert_eq!(error.kind(), std::io::ErrorKind::WriteZero);
        }
    }

    mod io_error {
        use super::*;

        #[test]
        fn io_error_should_return_error_with_program() {
            let error = io_error("git", std::io::Error::from(std::io::ErrorKind::NotFound));

            assert_eq!(error.program(), Some("git"));
            assert!(error.source_as::<std::io::Error>().is_some());
        }
    }

    mod validate {
        use super::*;

        #[test]
        fn validate_should_accept_compatible_options() {
            for options in [
                CommandOptions::default(),
                CommandOptions {
                    interactive: true,
                    stream: true,
                    clear_env: true,
                    ..Default::default()
                },
                CommandOptions {
                    input: Some(Vec::new()),
                    timeout: Some(Duration::from_secs(1)),
                    ..Default::default()
                },
            ] {
                assert!(validate("git", &options, false).is_ok(), "{options:?}");
                assert!(
                    validate("git", &options, true).is_ok() != options.interactive,
                    "{options:?}"
                );
            }
        }

        #[test]
        fn validate_interactive_with_incompatible_option_should_return_error() {
            for (options, description) in [
                (
                    CommandOptions {
                        interactive: true,
                        input: Some(Vec::new()),
                        ..Default::default()
                    },
                    "'git': interactive cannot be combined with input",
                ),
                (
                    CommandOptions {
                        interactive: true,
                        timeout: Some(Duration::from_secs(1)),
                        ..Default::default()
                    },
                    "'git': interactive cannot be combined with timeout",
                ),
                (
                    CommandOptions {
                        interactive: true,
                        silent: true,
                        ..Default::default()
                    },
                    "'git': interactive cannot be combined with silent",
                ),
            ] {
                assert_eq!(
                    validate("git", &options, false).unwrap_err().description(),
                    description
                );
            }
        }

        #[test]
        fn validate_sink_without_output_should_return_error() {
            for (options, description) in [
                (
                    CommandOptions {
                        interactive: true,
                        ..Default::default()
                    },
                    "'git': interactive cannot be combined with a sink",
                ),
                (
                    CommandOptions {
                        silent: true,
                        ..Default::default()
                    },
                    "'git': silent cannot be combined with a sink",
                ),
            ] {
                assert_eq!(
                    validate("git", &options, true).unwrap_err().description(),
                    description
                );
            }
        }
    }

    mod millis_until {
        use super::*;

        #[test]
        fn millis_until_should_round_up_remaining_time() {
            let millis = millis_until(Instant::now() + Duration::from_micros(10_500));

            assert!((10..=11).contains(&millis), "millis: {millis}");
        }

        #[test]
        fn millis_until_passed_deadline_should_return_zero() {
            assert_eq!(millis_until(Instant::now()), 0);
        }

        #[test]
        fn millis_until_distant_deadline_should_saturate() {
            let deadline = Instant::now() + Duration::from_secs(365 * 24 * 3600);

            assert_eq!(millis_until(deadline), i32::MAX as u32);
        }
    }

    mod run_blocking {
        use super::*;

        #[test]
        fn run_blocking_failed_output_should_return_error_after_chain() {
            let (process, recorder) = observed();

            let error = process.run(&shell("exit 3")).unwrap_err();

            assert_eq!(recorder.events()[1], "finished failed Some(3)");
            assert_eq!(
                error.description(),
                format!("'{SHELL}' failed with exit code 3")
            );
        }
    }

    mod emitted {
        use super::*;

        #[test]
        fn emitted_should_return_result_of_ready_future() {
            let result = emitted(std::future::ready(Err(crate::Error::process("full"))));

            assert_eq!(result.unwrap_err().description(), "full");
        }

        #[cfg(debug_assertions)]
        #[test]
        #[should_panic(expected = "passing a chunk of a command on should not wait")]
        fn emitted_pending_future_should_panic_in_debug_builds() {
            let _ = emitted(std::future::poll_fn(|context| {
                context.waker().wake_by_ref();
                Poll::Pending
            }));
        }
    }

    mod outputs {
        use super::*;

        mod push {
            use super::*;

            #[test]
            fn push_should_pass_chunks_on() {
                let mut chunks = Vec::new();
                let mut outputs = Outputs::new(|stream, data| {
                    chunks.push((stream, data.to_vec()));
                    Ok(())
                });

                outputs.push(OutputStream::Stdout, b"out");
                outputs.push(OutputStream::Stderr, b"err");

                assert!(!outputs.failed());
                outputs.finish().unwrap();
                assert_eq!(
                    chunks,
                    [
                        (OutputStream::Stdout, b"out".to_vec()),
                        (OutputStream::Stderr, b"err".to_vec())
                    ]
                );
            }

            #[test]
            fn push_failing_to_pass_on_should_fail_outputs() {
                let mut outputs = Outputs::new(|_, _| Err(crate::Error::process("full")));

                outputs.push(OutputStream::Stdout, b"out");

                assert!(outputs.failed());
                assert_eq!(outputs.finish().unwrap_err().description(), "full");
            }
        }
    }
}
