use std::any::Any;
use std::any::TypeId;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use crate::DryRunOutput;
use crate::Result;
use crate::env::SharedEnv;
#[cfg(feature = "tokio")]
use crate::utils::BoxFuture;
use crate::utils::Extensions;
use crate::utils::MAX_DELAY;
use crate::utils::SharedCell;
use crate::utils::Tail;
use crate::utils::backoff;

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
/// The output of an attempt as it arrives: passed through the interceptors
/// with `emit` and captured unless it goes to a sink. The first error of
/// `emit` ends the attempt.
pub(crate) struct Outputs<'a> {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    capture: bool,
    emit: Box<OutputSink<'a>>,
    error: Option<crate::Error>,
}

/// Intercepts the runs of commands by a [`Process`] like a middleware, e.g.
/// to change, retry, replace or log them.
///
/// The interceptors of a run form a chain around running its command, the
/// first one being the outermost. [`handle`](Self::handle) of each one gets
/// the run in a [`CommandNext`], which it may change, e.g. add an argument
/// or a variable, before passing it on to the rest of the chain with
/// [`CommandNext::next`] any number of times, e.g. to retry it, or not at
/// all, and whose result it may change, e.g. accept a failure by setting
/// [`CommandOutput::success`]. The innermost one is followed by the run of
/// the command. Each chunk of its output passes through
/// [`output`](Self::output) of the interceptors as it arrives, from the
/// innermost one outwards, before it is passed to the sink of
/// [`Process::run_to`].
///
/// A command whose output is set on its run with
/// [`CommandRun::set_output`], e.g. by a dry run or a cache, is not run:
/// its output is returned as if it was, see [`CommandRun::intercepted`].
/// That is how to short-circuit a run. An output an interceptor returns
/// instead of calling [`CommandNext::next`] is taken as it is, e.g. the
/// [default](CommandOutput::default) one is not a success, and its stdout
/// and stderr reach neither the sink nor [`output`](Self::output). Inside
/// the chain an output whose `success` is `false` is returned as `Ok`, the
/// methods of [`Process`] return it as an error. A run whose options
/// cannot be combined does not enter the chain, it is passed to
/// [`invalid`](Self::invalid) only.
///
/// Each attempt, e.g. of [`CommandRetry`], passes the run through the rest
/// of the chain again with the changes made to it before, so an interceptor
/// should replace what it sets rather than add to it. The chunks of every
/// attempt pass through [`output`](Self::output), see
/// [`CommandRun::attempt`].
///
/// Async runs (with the `tokio` feature) call
/// [`handle_async`](Self::handle_async) instead of [`handle`](Self::handle)
/// and [`cancelled`](Self::cancelled) when they are cancelled. Every method
/// passes the run, or the chunk, on to the rest of the chain by default.
///
/// The interceptors of a [`Process`] are [`StdStream`] by default, see
/// [`Process::add_interceptor`], e.g. on
/// [`Platform::process`](crate::Platform::process). A run uses them unless
/// it has its own, see [`CommandOptions::interceptors`]. Interceptors are
/// told apart by their type, e.g. by [`CommandRun::intercepted_by`].
///
/// # Examples
///
/// ```rust
/// use agplatform::{CommandExt, CommandNext, CommandOutput, Process, ProcessInterceptor, Result};
/// use std::process::Command;
/// use std::sync::{Arc, Mutex};
///
/// #[derive(Clone, Default)]
/// struct Failures(Arc<Mutex<Vec<String>>>);
///
/// impl ProcessInterceptor for Failures {
///     fn handle(&self, mut next: CommandNext) -> Result<CommandOutput> {
///         let result = next.next();
///
///         if result.as_ref().is_ok_and(|output| !output.success) {
///             self.0.lock().unwrap().push(next.run().command_line());
///         }
///
///         result
///     }
/// }
///
/// let failures = Failures::default();
/// let process = agplatform::TestProcess::new();
/// process.add_interceptor(failures.clone());
/// process.set_output("git", CommandOutput { status: Some(1), ..Default::default() });
///
/// let _ = Command::new("git").arg("push").run(&process);
///
/// assert_eq!(*failures.0.lock().unwrap(), ["git push"]);
/// ```
#[allow(unused_variables)]
pub trait ProcessInterceptor: Any + Send + Sync {
    /// Handles the run of `next`, returning its result. Runs the rest of
    /// the chain by default.
    fn handle(&self, mut next: CommandNext) -> Result<CommandOutput> {
        next.next()
    }

    /// Enabled by the `tokio` feature flag.
    ///
    /// Handles the async run of `next` like [`Self::handle`]. Runs the rest
    /// of the chain by default, it does not call [`Self::handle`].
    #[cfg(feature = "tokio")]
    fn handle_async<'a>(
        &'a self,
        mut next: AsyncCommandNext<'a>,
    ) -> BoxFuture<'a, Result<CommandOutput>> {
        Box::pin(async move { next.next().await })
    }

    /// Handles a chunk of the output of the run as it arrives, as read
    /// rather than split into lines. Passes it on by default.
    ///
    /// # Errors
    ///
    /// An error ends the attempt, killing the child process like
    /// [`CommandOptions::timeout`].
    fn output(&self, chunk: CommandChunk) -> Result {
        chunk.next()
    }

    /// Enabled by the `tokio` feature flag.
    ///
    /// Called when an async run is cancelled by dropping its future while
    /// its command is running or while an interceptor waits with
    /// [`AsyncCommandNext::sleep`], from the innermost interceptor
    /// outwards. Not called when the run panics or is cancelled while an
    /// interceptor awaits another future, which it would have to notice
    /// itself, e.g. with a guard dropped with its future.
    #[cfg(feature = "tokio")]
    fn cancelled(&self, run: &CommandRun) {}

    /// Called with the error of a run whose options cannot be combined,
    /// e.g. [`CommandOptions::interactive`] with
    /// [`CommandOptions::timeout`], which is therefore not run.
    fn invalid(&self, run: &CommandRun, error: &crate::Error) {}
}

/// The rest of the chain of interceptors of a run, passed to
/// [`ProcessInterceptor::handle`] with the run.
pub struct CommandNext<'a> {
    run: &'a mut CommandRun,
    layers: &'a [Arc<dyn ProcessInterceptor>],
    inner: &'a mut dyn Execute,
}

/// Enabled by the `tokio` feature flag.
///
/// The rest of the chain of interceptors of an async run, passed to
/// [`ProcessInterceptor::handle_async`] with the run.
#[cfg(feature = "tokio")]
pub struct AsyncCommandNext<'a> {
    run: &'a mut CommandRun,
    layers: &'a [Arc<dyn ProcessInterceptor>],
    inner: &'a mut dyn ExecuteAsync,
}

/// A chunk of the output of a run passing through the interceptors, see
/// [`ProcessInterceptor::output`].
pub struct CommandChunk<'a> {
    run: &'a CommandRun,
    stream: OutputStream,
    data: Cow<'a, [u8]>,
    layers: &'a [Arc<dyn ProcessInterceptor>],
    sink: Option<&'a mut OutputSink<'a>>,
}

/// The interceptors of a [`Process`] or of a run, see
/// [`CommandOptions::interceptors`], the first one being the outermost.
///
/// Two are equal when they hold the same instances in the same order, e.g.
/// one is a clone of the other.
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
/// process.set_outputs("git", [failure, CommandOutput { status: Some(0), ..Default::default() }]);
///
/// let mut interceptors = process.interceptors();
/// interceptors.insert(0, CommandRetry::new(3, Duration::from_secs(1)));
/// Command::new("git").arg("fetch").interceptors(interceptors).run(&process).unwrap();
///
/// assert_eq!(process.calls(), ["git fetch", "git fetch"]);
/// ```
#[derive(Clone, Default)]
pub struct CommandInterceptors(Vec<Arc<dyn ProcessInterceptor>>);

/// The [`ProcessInterceptor`] writing the output of the runs with
/// [`CommandOptions::stream`] to the standard output and error of the
/// current process, the only interceptor of a process by default.
#[derive(Clone, Copy, Debug, Default)]
pub struct StdStream;

/// The [`ProcessInterceptor`] retrying the failed attempts of the runs, up
/// to `retries` times after `delay` (see [`CommandNext::sleep`]) while
/// they fail, i.e. their output is not a
/// [success](CommandOutput::success) or they return an error. Each attempt
/// gets the [`CommandOptions::input`] and is limited by
/// [`CommandOptions::timeout`] on its own. Not after a sink accepted a
/// part of the output, see [`Process::run_to`], nor when the output is set
/// by an interceptor, see [`CommandRun::intercepted`], which would return
/// it again.
///
/// The delay is multiplied by the [`backoff`](Self::backoff) after each
/// retry and limited by the [`max_delay`](Self::max_delay).
///
/// It runs again all interceptors after it in the chain, so it is usually
/// the outermost one.
///
/// # Examples
///
/// ```rust
/// use agplatform::{CommandExt, CommandOutput, CommandRetry, Process, TestProcess};
/// use std::process::Command;
/// use std::time::Duration;
///
/// let process = TestProcess::new();
/// process.add_interceptor(CommandRetry::new(3, Duration::from_secs(5)).when(|_, result| {
///     result
///         .as_ref()
///         .is_ok_and(|output| output.stderr_lossy().contains("Could not resolve host"))
/// }));
/// process.set_output("git push", CommandOutput { status: Some(1), ..Default::default() });
///
/// assert!(Command::new("git").arg("push").run(&process).is_err());
/// assert_eq!(process.calls(), ["git push"]);
/// ```
#[derive(Clone)]
pub struct CommandRetry {
    retries: u32,
    delay: Duration,
    backoff: u32,
    max_delay: Duration,
    condition: Option<Arc<CommandRetryCondition>>,
}

type CommandRetryCondition = dyn Fn(&CommandRun, &Result<CommandOutput>) -> bool + Send + Sync;

/// The [`ProcessInterceptor`] of a dry run, setting the output of the runs
/// (see [`CommandRun::set_output`]) instead of running their commands: the
/// [`DryRunOutput`] set on the run (see [`CommandRun::with`]), or an empty
/// success, unless an interceptor before it set the output already.
///
/// # Examples
///
/// ```rust
/// use agplatform::{CommandDryRun, CommandExt, CommandOutput, DryRunOutput, Process, TestProcess};
/// use std::process::Command;
///
/// let process = TestProcess::new();
/// process.add_interceptor(CommandDryRun);
///
/// let applied = Command::new("kubectl").arg("apply").run(&process).unwrap();
/// let pods = Command::new("kubectl")
///     .args(["get", "pods"])
///     .with(DryRunOutput(CommandOutput { status: Some(0), stdout: b"pod-1".to_vec(), ..Default::default() }))
///     .run(&process)
///     .unwrap();
///
/// assert!(applied.stdout.is_empty());
/// assert_eq!(pods.stdout, b"pod-1");
/// assert!(process.calls().is_empty());
/// ```
#[derive(Clone, Copy, Debug, Default)]
pub struct CommandDryRun;

/// The [`ProcessInterceptor`] caching the successful outputs of the runs in
/// memory by their [`key`](Self::key), returning them for the runs of the
/// same commands instead of running them again. The outputs set by other
/// interceptors (see [`CommandRun::intercepted`]), those passed to a sink
/// and those of [`CommandOptions::interactive`] runs are not cached.
///
/// Clones share the cache, e.g. to persist its [`entries`](Self::entries)
/// and [`load`](Self::load) them later.
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
/// process.set_output("git", CommandOutput { status: Some(0), stdout: b"abc".to_vec(), ..Default::default() });
///
/// Command::new("git").args(["rev-parse", "HEAD"]).run(&process).unwrap();
/// let output = Command::new("git").args(["rev-parse", "HEAD"]).run(&process).unwrap();
///
/// assert_eq!(output.stdout, b"abc");
/// assert_eq!(process.calls().len(), 1);
/// assert_eq!(cache.entries().len(), 1);
/// ```
#[derive(Clone, Default)]
pub struct CommandCache(Arc<Mutex<BTreeMap<String, CommandOutput>>>);

/// Abstract process interface.
///
/// Runs child processes described by [`std::process::Command`] to
/// completion. Child processes run in the configured current directory
/// unless the command sets an absolute one, a relative one is resolved
/// against it. They get the configured environment variables, updated by
/// the variables the command sets or removes. Each run passes through
/// interceptors, see [`ProcessInterceptor`].
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
    /// options, a [`CommandRun`] (e.g. from [`CommandExt`]) with its own.
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
    /// hold secrets. A [`ProcessInterceptor`] gets the whole command, e.g. to
    /// log it redacted.
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
    /// use agplatform::{CommandExt, CommandOptions, CommandOutput, CommandRun, Platform, Process};
    /// use std::process::Command;
    /// use std::time::Duration;
    ///
    /// let platform = agplatform::platform();
    /// let process = platform.process();
    /// # let process = agplatform::TestProcess::new();
    /// # process.set_output("git", CommandOutput { status: Some(0), stdout: b"git version 2.0".to_vec(), ..Default::default() });
    ///
    /// let output = process.run(Command::new("git").arg("--version")).unwrap();
    /// assert!(output.stdout_str().unwrap().starts_with("git version"));
    ///
    /// let mut fetch = Command::new("git");
    /// fetch.arg("fetch");
    /// process.run(fetch.timeout(Duration::from_secs(60))).unwrap();
    ///
    /// let options = CommandOptions { silent: true, ..Default::default() };
    /// process.run(CommandRun::with_options(&fetch, options)).unwrap();
    /// ```
    fn run(&self, run: impl Into<CommandRun>) -> Result<CommandOutput>;

    /// Runs `run` like [`Self::run`], passing the standard output and error
    /// to `sink` as they arrive, each chunk with the [`OutputStream`] it is
    /// from, instead of capturing them, e.g. for output too large to hold
    /// in memory. The [`CommandOutput`] then has no output, nor does the
    /// error of a failed run show any. The chunks pass through the
    /// interceptors before, see [`ProcessInterceptor::output`].
    ///
    /// The `sink` gets the output whether the run succeeds or not, which is
    /// known only once it ends. The chunks are as read, not split into
    /// lines. An attempt failing after `sink` accepted a chunk is not
    /// retried by [`CommandRetry`], as `sink` would get the output again
    /// from its start, see [`CommandRun::sunk`].
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
    /// # process.set_output("git", CommandOutput { status: Some(0), stdout: b"archive".to_vec(), ..Default::default() });
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
        run: impl Into<CommandRun>,
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
    /// killed. The interceptors are told by
    /// [`ProcessInterceptor::cancelled`].
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
    /// # process.set_output("git", CommandOutput { status: Some(0), stdout: b"git version 2.0".to_vec(), ..Default::default() });
    ///
    /// let output = process.run_async(Command::new("git").arg("--version")).await.unwrap();
    /// assert!(output.stdout_str().unwrap().starts_with("git version"));
    /// # });
    /// ```
    #[cfg(feature = "tokio")]
    fn run_async(
        &self,
        run: impl Into<CommandRun>,
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
    /// # process.set_output("cargo", CommandOutput { status: Some(0), stderr: b"Compiling".to_vec(), ..Default::default() });
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
        run: impl Into<CommandRun>,
        sink: &mut (dyn FnMut(OutputStream, &[u8]) -> Result + Send),
    ) -> impl Future<Output = Result<CommandOutput>> + Send;

    /// Returns the interceptors of the runs, e.g. to set them with changes
    /// on a run, see [`CommandOptions::interceptors`].
    fn interceptors(&self) -> CommandInterceptors;

    /// Removes the interceptors, [`StdStream`] by default, for the runs
    /// starting afterwards, e.g. before adding others with
    /// [`Self::add_interceptor`]. Without [`StdStream`]
    /// [`CommandOptions::stream`] has no effect.
    fn clear_interceptors(&self);

    /// Adds `interceptor` as the innermost one for the runs starting
    /// afterwards. Its state to be read later, e.g. what it recorded, is
    /// shared through an [`Arc`] it holds.
    ///
    /// See [`ProcessInterceptor`].
    fn add_interceptor(&self, interceptor: impl ProcessInterceptor);
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
/// # process.set_output("", CommandOutput { status: Some(0), ..Default::default() });
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

    /// Returns a [`CommandRun`] of the command without the configured
    /// environment variables.
    ///
    /// See [`CommandOptions::clear_env`].
    fn clear_env(&self) -> CommandRun;

    /// Returns a [`CommandRun`] of the command whose output shows its last
    /// `lines` when displayed.
    ///
    /// See [`CommandOptions::display_lines`].
    fn display_lines(&self, lines: usize) -> CommandRun;

    /// Returns a [`CommandRun`] of the command whose output is discarded.
    ///
    /// See [`CommandOptions::silent`].
    fn silent(&self) -> CommandRun;

    /// Returns a [`CommandRun`] of the command reading `input`.
    ///
    /// See [`CommandOptions::input`].
    fn input<T: Into<Vec<u8>>>(&self, input: T) -> CommandRun;

    /// Returns a [`CommandRun`] of the command connected to the terminal.
    ///
    /// See [`CommandOptions::interactive`].
    fn interactive(&self) -> CommandRun;

    /// Returns a [`CommandRun`] of the command that streams its output.
    ///
    /// See [`CommandOptions::stream`].
    fn stream(&self) -> CommandRun;

    /// Returns a [`CommandRun`] of the command limited to `timeout`.
    ///
    /// See [`CommandOptions::timeout`].
    fn timeout(&self, timeout: Duration) -> CommandRun;

    /// Returns a [`CommandRun`] of the command with its own `interceptors`.
    ///
    /// See [`CommandOptions::interceptors`].
    fn interceptors(&self, interceptors: CommandInterceptors) -> CommandRun;

    /// Returns a [`CommandRun`] of the command with `value` for the
    /// interceptors.
    ///
    /// See [`CommandRun::with`].
    fn with<T: Any + Send + Sync>(&self, value: T) -> CommandRun;
}

/// A command to be run with [`CommandOptions`].
///
/// Usually obtained from [`CommandExt`]. Holds a copy of the program, the
/// arguments, the environment variables and the current directory of the
/// command, which is left unchanged. Passed to the interceptors, then with
/// the directory the command runs in, which may change it, see
/// [`ProcessInterceptor`].
pub struct CommandRun {
    program: OsString,
    args: Vec<OsString>,
    envs: Vec<(OsString, Option<OsString>)>,
    dir: Option<PathBuf>,
    options: CommandOptions,
    current_dir: Option<PathBuf>,
    /// The variables set or removed in the env the run was resolved in.
    var_changes: Vec<(String, Option<String>)>,
    attempt: u32,
    id: u64,
    sink: bool,
    sunk: AtomicBool,
    output: Option<CommandOutput>,
    output_by: Option<TypeId>,
    /// The interceptor being called.
    layer: Option<TypeId>,
    extensions: Extensions,
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
    /// [`CommandInterceptors::new`].
    pub stream: bool,

    /// The interceptors of the run instead of those of the process, see
    /// [`Process::interceptors`]. Those of the process when `None`.
    pub interceptors: Option<CommandInterceptors>,

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
    /// Whether the run succeeded. Set by the process for each attempt to
    /// whether the child process exited with code `0` in time, then may be
    /// changed by the interceptors, e.g. to accept the exit code `1` of a
    /// command answering "no".
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

/// [internal]
///
/// The innermost part of the chain of a run, running its attempts.
pub(crate) trait Execute {
    /// Runs the attempt of `run`.
    fn execute(&mut self, run: &mut CommandRun) -> Result<CommandOutput>;

    /// Waits `delay`, see [`CommandNext::sleep`].
    fn sleep(&self, delay: Duration);
}

/// [internal]
///
/// The innermost part of the chain of an async run, running its attempts.
#[cfg(feature = "tokio")]
pub(crate) trait ExecuteAsync: Send {
    /// Runs the attempt of `run`.
    fn execute<'a>(&'a mut self, run: &'a mut CommandRun) -> BoxFuture<'a, Result<CommandOutput>>;

    /// Waits `delay`, see [`AsyncCommandNext::sleep`].
    fn delay(&self, delay: Duration) -> BoxFuture<'static, ()>;

    /// Returns the interceptors of the run.
    fn layers(&self) -> &[Arc<dyn ProcessInterceptor>];
}

/// [internal]
///
/// Runs the attempts of the commands of a [`Process`] implementation.
pub(crate) trait Attempt: Sync {
    /// Runs the command of `run` with its `outputs`.
    fn attempt(&self, run: &CommandRun, outputs: Outputs) -> Result<CommandOutput>;

    /// Runs the command of `run` with its `outputs` asynchronously.
    #[cfg(feature = "tokio")]
    fn attempt_async<'a>(
        &'a self,
        run: &'a CommandRun,
        outputs: Outputs<'a>,
    ) -> BoxFuture<'a, Result<CommandOutput>>;

    /// Waits `delay` between attempts.
    fn sleep(&self, delay: Duration);

    /// Waits `delay` between async attempts.
    #[cfg(feature = "tokio")]
    fn delay(&self, delay: Duration) -> BoxFuture<'static, ()>;
}

/// [internal]
///
/// The innermost part of the chain of a run by `attempts` of the run with
/// the interceptors `layers`, passing its output to `sink`, if any.
pub(crate) struct Executor<'a, 's, A: ?Sized> {
    pub(crate) attempts: &'a A,
    pub(crate) layers: &'a [Arc<dyn ProcessInterceptor>],
    pub(crate) sink: Option<&'a mut OutputSink<'s>>,
}

/// [internal]
///
/// Tells the interceptors of a run that it is cancelled when dropped before
/// it is disarmed.
#[cfg(feature = "tokio")]
struct Cancel<'a> {
    run: &'a CommandRun,
    layers: &'a [Arc<dyn ProcessInterceptor>],
    armed: bool,
}

/// [`Process`] implementation backed by [`std::process`].
///
/// Child processes run in the configured current directory with the
/// configured environment variables.
///
/// Normally obtained via [`platform()`](crate::platform) rather than
/// constructed directly.
pub struct ProcessImpl {
    env: SharedEnv,
    interceptors: SharedCell<CommandInterceptors>,
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

    fn clear_env(&self) -> CommandRun {
        CommandRun::new(self).clear_env()
    }

    fn display_lines(&self, lines: usize) -> CommandRun {
        CommandRun::new(self).display_lines(lines)
    }

    fn silent(&self) -> CommandRun {
        CommandRun::new(self).silent()
    }

    fn input<T: Into<Vec<u8>>>(&self, input: T) -> CommandRun {
        CommandRun::new(self).input(input)
    }

    fn interactive(&self) -> CommandRun {
        CommandRun::new(self).interactive()
    }

    fn stream(&self) -> CommandRun {
        CommandRun::new(self).stream()
    }

    fn timeout(&self, timeout: Duration) -> CommandRun {
        CommandRun::new(self).timeout(timeout)
    }

    fn interceptors(&self, interceptors: CommandInterceptors) -> CommandRun {
        CommandRun::new(self).interceptors(interceptors)
    }

    fn with<T: Any + Send + Sync>(&self, value: T) -> CommandRun {
        CommandRun::new(self).with(value)
    }
}

impl CommandNext<'_> {
    /// Returns the run.
    pub fn run(&self) -> &CommandRun {
        self.run
    }

    /// Returns the run to be changed, e.g. before [`Self::next`].
    pub fn run_mut(&mut self) -> &mut CommandRun {
        self.run
    }

    /// Runs the rest of the chain, the next interceptor or the command, and
    /// returns its result. May be called again, e.g. to retry the run.
    ///
    /// # Errors
    ///
    /// Returns the error of the rest of the chain, e.g. when the process
    /// cannot be started. An output that is not a
    /// [success](CommandOutput::success) is returned as `Ok`.
    // WHY: named after the next part of the chain like in middlewares, it
    // is not an iterator.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<CommandOutput> {
        match self.layers.split_first() {
            Some((layer, layers)) => {
                let outer = self.run.layer.replace(type_of(layer));
                let result = layer.handle(CommandNext {
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

    /// Waits `delay`, e.g. before a retry, unless the process does not
    /// wait, like [`TestProcess`](crate::TestProcess).
    pub fn sleep(&self, delay: Duration) {
        self.inner.sleep(delay);
    }
}

#[cfg(feature = "tokio")]
impl AsyncCommandNext<'_> {
    /// Returns the run.
    pub fn run(&self) -> &CommandRun {
        self.run
    }

    /// Returns the run to be changed, e.g. before [`Self::next`].
    pub fn run_mut(&mut self) -> &mut CommandRun {
        self.run
    }

    /// Runs the rest of the chain like [`CommandNext::next`]
    /// asynchronously.
    ///
    /// # Errors
    ///
    /// As [`CommandNext::next`].
    // WHY: named after the next part of the chain like in middlewares, it
    // is not an iterator.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> BoxFuture<'_, Result<CommandOutput>> {
        match self.layers.split_first() {
            Some((layer, layers)) => Box::pin(async move {
                let outer = self.run.layer.replace(type_of(layer));
                let result = layer
                    .handle_async(AsyncCommandNext {
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

    /// Waits `delay` like [`CommandNext::sleep`] asynchronously. Dropping
    /// the future while it waits cancels the run, see
    /// [`ProcessInterceptor::cancelled`].
    pub fn sleep(&self, delay: Duration) -> BoxFuture<'_, ()> {
        let (delay, run, layers) = (self.inner.delay(delay), &*self.run, self.inner.layers());

        Box::pin(async move {
            let mut cancel = Cancel::new(run, layers);
            delay.await;
            cancel.disarm();
        })
    }
}

impl<'a> CommandChunk<'a> {
    /// Returns the run.
    pub fn run(&self) -> &CommandRun {
        self.run
    }

    /// Returns the output the chunk is from.
    pub fn stream(&self) -> OutputStream {
        self.stream
    }

    /// Returns the data of the chunk.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Replaces the data of the chunk passed on, e.g. to redact a secret.
    /// The output captured in the [`CommandOutput`] is not changed.
    pub fn set_data(&mut self, data: Vec<u8>) {
        self.data = Cow::Owned(data);
    }

    /// Passes the chunk on to the next interceptor outwards, or the sink of
    /// [`Process::run_to`]. A chunk not passed on is dropped.
    ///
    /// # Errors
    ///
    /// Returns the error of the next interceptor or the sink.
    pub fn next(self) -> Result {
        match self.layers.split_last() {
            Some((layer, layers)) => layer.output(CommandChunk { layers, ..self }),
            None => match self.sink {
                Some(sink) => {
                    sink(self.stream, &self.data)?;
                    self.run.sunk.store(true, Ordering::Relaxed);
                    Ok(())
                }
                None => Ok(()),
            },
        }
    }
}

impl CommandRun {
    /// Creates a run of `command` with the default [`CommandOptions`].
    pub fn new(command: &Command) -> Self {
        Self::with_options(command, CommandOptions::default())
    }

    /// Creates a run of `command` with `options`, e.g. read from a
    /// configuration.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{CommandOptions, CommandRun};
    /// use std::process::Command;
    ///
    /// let options = CommandOptions { stream: true, ..Default::default() };
    /// let run = CommandRun::with_options(&Command::new("git"), options);
    ///
    /// assert!(run.options().stream);
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
            var_changes: Vec::new(),
            attempt: 0,
            id: 0,
            sink: false,
            sunk: AtomicBool::new(false),
            output: None,
            output_by: None,
            layer: None,
            extensions: Extensions::default(),
        }
    }

    /// Returns the arguments of the command.
    pub fn args(&self) -> &[OsString] {
        &self.args
    }

    /// Returns the arguments of the command to be changed, e.g. by an
    /// interceptor.
    pub fn args_mut(&mut self) -> &mut Vec<OsString> {
        &mut self.args
    }

    /// Returns the number of the current or last attempt of the run, i.e.
    /// of the times its command was run or its output set returned so far,
    /// `1` from the start of the first one and `0` before.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Returns the program and the arguments of the command separated by
    /// spaces.
    pub fn command_line(&self) -> String {
        std::iter::once(&self.program)
            .chain(&self.args)
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Returns the directory the command runs in, i.e. its own resolved
    /// against the current directory of the process, when passed to an
    /// interceptor. `None` otherwise.
    pub fn current_dir(&self) -> Option<&Path> {
        self.current_dir.as_deref()
    }

    /// Returns the environment variables set (`Some`) or removed (`None`) by
    /// the command, in the order they were set.
    pub fn envs(&self) -> &[(OsString, Option<OsString>)] {
        &self.envs
    }

    /// Returns the value of type `T` set on the run, see [`Self::with`].
    pub fn get<T: Any>(&self) -> Option<&T> {
        self.extensions.get()
    }

    /// Returns `true` if the output of the run goes to a sink, see
    /// [`Process::run_to`], so that it is not captured.
    pub fn has_sink(&self) -> bool {
        self.sink
    }

    /// Returns the id of the run, unique within the process among the runs
    /// of commands and requests and the same for all of its attempts, e.g.
    /// to tell concurrent runs apart in an interceptor. It is assigned when
    /// the run starts, `0` before.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Sets `value` of type `T` on the run, replacing the previous one, e.g.
    /// for the next interceptors, see [`Self::with`].
    pub fn insert<T: Any + Send + Sync>(&mut self, value: T) {
        self.extensions.insert(value);
    }

    /// Returns `true` if the output of the run was set, see
    /// [`Self::set_output`], so that its command does not run, e.g. so that
    /// a cache does not store it.
    pub fn intercepted(&self) -> bool {
        self.output.is_some()
    }

    /// Returns `true` if the output of the run was set by an interceptor of
    /// type `T`, see [`Self::set_output`].
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{CommandDryRun, CommandExt, CommandNext, CommandOutput, Process, ProcessInterceptor, Result, TestProcess};
    /// use std::process::Command;
    /// use std::sync::{Arc, Mutex};
    ///
    /// #[derive(Clone, Default)]
    /// struct Log(Arc<Mutex<Vec<String>>>);
    ///
    /// impl ProcessInterceptor for Log {
    ///     fn handle(&self, mut next: CommandNext) -> Result<CommandOutput> {
    ///         let result = next.next();
    ///         let suffix = if next.run().intercepted_by::<CommandDryRun>() { " (DRY RUN)" } else { "" };
    ///         self.0.lock().unwrap().push(format!("{}{suffix}", next.run().command_line()));
    ///         result
    ///     }
    /// }
    ///
    /// let log = Log::default();
    /// let process = TestProcess::new();
    /// process.add_interceptor(log.clone());
    /// process.add_interceptor(CommandDryRun);
    ///
    /// Command::new("kubectl").arg("apply").run(&process).unwrap();
    ///
    /// assert_eq!(*log.0.lock().unwrap(), ["kubectl apply (DRY RUN)"]);
    /// ```
    pub fn intercepted_by<T: ProcessInterceptor>(&self) -> bool {
        self.intercepted() && self.output_by == Some(TypeId::of::<T>())
    }

    /// Returns the options.
    pub fn options(&self) -> &CommandOptions {
        &self.options
    }

    /// Returns the options to be changed, e.g. by an interceptor.
    pub fn options_mut(&mut self) -> &mut CommandOptions {
        &mut self.options
    }

    /// Returns the output set on the run, see [`Self::set_output`].
    pub fn output(&self) -> Option<&CommandOutput> {
        self.output.as_ref()
    }

    /// Returns `true` once the sink of the run accepted a chunk of its
    /// output, see [`Process::run_to`], so that it cannot be retried.
    pub fn sunk(&self) -> bool {
        self.sunk.load(Ordering::Relaxed)
    }

    /// Returns the program of the command, lossily converted to UTF-8.
    pub fn program(&self) -> Cow<'_, str> {
        self.program.to_string_lossy()
    }

    /// Removes the environment variable `key` for the command, e.g. in an
    /// interceptor.
    pub fn remove_env<K: AsRef<OsStr>>(&mut self, key: K) {
        self.change_env(key.as_ref(), None);
    }

    /// Sets the environment variable `key` to `value` for the command, e.g.
    /// in an interceptor.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{CommandNext, CommandOutput, ProcessInterceptor, Result};
    ///
    /// struct Token;
    ///
    /// impl ProcessInterceptor for Token {
    ///     fn handle(&self, mut next: CommandNext) -> Result<CommandOutput> {
    ///         next.run_mut().set_env("TOKEN", "secret");
    ///         next.next()
    ///     }
    /// }
    /// ```
    pub fn set_env<K: AsRef<OsStr>, V: AsRef<OsStr>>(&mut self, key: K, value: V) {
        self.change_env(key.as_ref(), Some(value.as_ref().to_owned()));
    }

    /// Sets the `output` of the run, returned for its attempts instead of
    /// running its command as if it ran, e.g. passed to the interceptors and
    /// the sink. Its [`CommandOutput::success`] is set from its status like
    /// for a command that ran, its duration is not compared with the
    /// [`CommandOptions::timeout`]. Set by the interceptor calling it, see
    /// [`Self::intercepted_by`].
    pub fn set_output(&mut self, output: CommandOutput) {
        self.output = Some(output);
        self.output_by = self.layer;
    }

    /// Sets the program of the command, e.g. in an interceptor.
    pub fn set_program<T: Into<OsString>>(&mut self, program: T) {
        self.program = program.into();
    }

    /// Passes only the environment variables set by the command.
    ///
    /// See [`CommandOptions::clear_env`].
    pub fn clear_env(mut self) -> Self {
        self.options.clear_env = true;
        self
    }

    /// Shows the last `lines` of the output when displayed.
    ///
    /// See [`CommandOptions::display_lines`].
    pub fn display_lines(mut self, lines: usize) -> Self {
        self.options.display_lines = lines;
        self
    }

    /// Discards the output of the command.
    ///
    /// See [`CommandOptions::silent`].
    pub fn silent(mut self) -> Self {
        self.options.silent = true;
        self
    }

    /// Writes `input` to the standard input of the command.
    ///
    /// See [`CommandOptions::input`].
    pub fn input<T: Into<Vec<u8>>>(mut self, input: T) -> Self {
        self.options.input = Some(input.into());
        self
    }

    /// Connects the command to the terminal.
    ///
    /// See [`CommandOptions::interactive`].
    pub fn interactive(mut self) -> Self {
        self.options.interactive = true;
        self
    }

    /// Streams the output of the command.
    ///
    /// See [`CommandOptions::stream`].
    pub fn stream(mut self) -> Self {
        self.options.stream = true;
        self
    }

    /// Limits the command to `timeout`.
    ///
    /// See [`CommandOptions::timeout`].
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.options.timeout = Some(timeout);
        self
    }

    /// Runs the command with its own `interceptors`.
    ///
    /// See [`CommandOptions::interceptors`].
    pub fn interceptors(mut self, interceptors: CommandInterceptors) -> Self {
        self.options.interceptors = Some(interceptors);
        self
    }

    /// Sets `value` of type `T` on the run for the interceptors, replacing
    /// the previous one, e.g. a [`DryRunOutput`] for [`CommandDryRun`].
    /// Read with [`Self::get`].
    pub fn with<T: Any + Send + Sync>(mut self, value: T) -> Self {
        self.insert(value);
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

    /// [internal]
    ///
    /// Returns the run passing its output to a sink, see
    /// [`Process::run_to`].
    pub(crate) fn with_sink(mut self) -> Self {
        self.sink = true;
        self
    }

    /// [internal]
    ///
    /// Returns the run to be passed to the interceptors, in the directory of
    /// its command resolved against the current directory of `env` and with
    /// its variable changes.
    ///
    /// WHY: taken once so that all attempts of the run get the same env,
    /// even when it is changed in between.
    pub(crate) fn resolve(mut self, env: &SharedEnv) -> Self {
        let current_dir = env.current_dir();
        self.current_dir = Some(
            self.dir
                .as_ref()
                .map_or(current_dir.to_path_buf(), |dir| current_dir.join(dir)),
        );
        self.var_changes = env.changes();
        self
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

    /// [internal]
    ///
    /// Returns the variables set or removed in the env the run was
    /// resolved in.
    pub(crate) fn var_changes(&self) -> &[(String, Option<String>)] {
        &self.var_changes
    }

    /// [internal]
    ///
    /// Returns the interceptors of the run, its own or those of the process.
    pub(crate) fn layers(
        &self,
        process: &SharedCell<CommandInterceptors>,
    ) -> Vec<Arc<dyn ProcessInterceptor>> {
        self.options
            .interceptors
            .clone()
            .unwrap_or_else(|| process.get())
            .0
    }
}

impl From<&Command> for CommandRun {
    fn from(command: &Command) -> Self {
        Self::new(command)
    }
}

impl From<&mut Command> for CommandRun {
    fn from(command: &mut Command) -> Self {
        Self::new(command)
    }
}

impl Default for CommandOptions {
    fn default() -> Self {
        Self {
            display_lines: DISPLAY_LINES,
            clear_env: false,
            input: None,
            interactive: false,
            interceptors: None,
            silent: false,
            stream: false,
            timeout: None,
        }
    }
}

impl CommandInterceptors {
    /// Creates an empty list of interceptors.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts `interceptor` at `index`, the outermost at `0`.
    ///
    /// # Panics
    ///
    /// Panics if `index` is greater than [`Self::len`].
    pub fn insert(&mut self, index: usize, interceptor: impl ProcessInterceptor) {
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
    pub fn push(&mut self, interceptor: impl ProcessInterceptor) {
        self.0.push(Arc::new(interceptor));
    }
}

impl std::fmt::Debug for CommandInterceptors {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "CommandInterceptors({})", self.0.len())
    }
}

impl PartialEq for CommandInterceptors {
    fn eq(&self, other: &Self) -> bool {
        crate::utils::same_interceptors(&self.0, &other.0)
    }
}

impl Eq for CommandInterceptors {}

impl CommandRetry {
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

    /// Waits at most `max` before a retry, limiting the backoff. 60 s by
    /// default.
    pub fn max_delay(mut self, max: Duration) -> Self {
        self.max_delay = max;
        self
    }

    /// Retries only the failed attempts for which `condition` returns
    /// `true`, e.g. those failing for a network error.
    pub fn when<F>(mut self, condition: F) -> Self
    where
        F: Fn(&CommandRun, &Result<CommandOutput>) -> bool + Send + Sync + 'static,
    {
        self.condition = Some(Arc::new(condition));
        self
    }

    /// Returns the delay before retrying the attempt of `run` with `result`
    /// after `retried` retries, `None` if it is not retried.
    fn retry(
        &self,
        run: &CommandRun,
        result: &Result<CommandOutput>,
        retried: u32,
    ) -> Option<Duration> {
        let retried_ok = retried < self.retries
            && !result.as_ref().is_ok_and(|output| output.success)
            && !run.sunk()
            && !run.intercepted()
            && self
                .condition
                .as_ref()
                .is_none_or(|condition| condition(run, result));

        retried_ok.then(|| backoff(self.delay, self.backoff, retried, self.max_delay))
    }
}

impl std::fmt::Debug for CommandRetry {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("CommandRetry")
            .field("retries", &self.retries)
            .field("delay", &self.delay)
            .field("backoff", &self.backoff)
            .field("max_delay", &self.max_delay)
            .field("condition", &self.condition.is_some())
            .finish()
    }
}

impl CommandCache {
    /// Returns the cached outputs by their keys, e.g. to persist them.
    pub fn entries(&self) -> Vec<(String, CommandOutput)> {
        self.outputs()
            .iter()
            .map(|(key, output)| (key.clone(), output.clone()))
            .collect()
    }

    /// Returns the key of `run`: the directory, the program and the
    /// arguments, the environment variables set or removed by the command
    /// and those changed on the platform (unless
    /// [`CommandOptions::clear_env`]), the options changing the output and
    /// the input, each quoted or escaped so that different runs have
    /// different keys.
    ///
    /// The variables are named with whether they are set, their values are
    /// only hashed together (FNV-1a, the same on every Rust version), as
    /// they may hold secrets, e.g. tokens, that the persisted
    /// [`entries`](Self::entries) must not show.
    pub fn key(run: &CommandRun) -> String {
        let options = run.options();
        let vars: &[(String, Option<String>)] = if options.clear_env {
            &[]
        } else {
            run.var_changes()
        };
        fn names<'a>(vars: impl Iterator<Item = (&'a OsStr, bool)>) -> Vec<(&'a OsStr, &'a str)> {
            vars.map(|(name, set)| (name, if set { "set" } else { "removed" }))
                .collect()
        }

        let values = crate::utils::fnv1a(
            run.envs()
                .iter()
                .map(|(_, value)| value.as_deref().map(OsStr::as_encoded_bytes))
                .chain(
                    vars.iter()
                        .map(|(_, value)| value.as_deref().map(str::as_bytes)),
                ),
        );
        let mut key = format!(
            "dir: {:?}\ncommand: {:?}\nenvs: {:?}\nclear_env: {}\nsilent: {}",
            run.current_dir().unwrap_or(Path::new("")),
            std::iter::once(&run.program)
                .chain(&run.args)
                .collect::<Vec<_>>(),
            names(
                run.envs()
                    .iter()
                    .map(|(name, value)| (name.as_os_str(), value.is_some()))
            ),
            options.clear_env,
            options.silent,
        );

        if !options.clear_env {
            key.push_str(&format!(
                "\nvars: {:?}",
                names(
                    vars.iter()
                        .map(|(name, value)| (OsStr::new(name), value.is_some()))
                )
            ));
        }

        key.push_str(&format!("\nvalues: {values:016x}"));

        if let Some(input) = &options.input {
            key.push_str(&format!("\ninput: {}", input.escape_ascii()));
        }

        key
    }

    /// Adds the `entries`, e.g. persisted from [`Self::entries`], replacing
    /// those with the same keys.
    pub fn load(&self, entries: impl IntoIterator<Item = (String, CommandOutput)>) {
        self.outputs().extend(entries);
    }

    fn outputs(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, CommandOutput>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sets the cached output of `run`, if any, unless its output is set or
    /// it is interactive.
    fn hit(&self, run: &mut CommandRun) -> String {
        let key = Self::key(run);

        if !run.intercepted()
            && !run.options().interactive
            && let Some(output) = self.outputs().get(&key).cloned()
        {
            run.set_output(output);
        }

        key
    }

    /// Caches the successful output of `run` from `result` by `key`.
    fn store(&self, run: &CommandRun, key: String, result: &Result<CommandOutput>) {
        if let Ok(output) = result
            && output.success
            && !run.intercepted()
            && !run.has_sink()
            && !run.options().interactive
        {
            self.outputs().insert(key, output.clone());
        }
    }
}

impl std::fmt::Debug for CommandCache {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "CommandCache({})", self.outputs().len())
    }
}

/// Returns the type of the `interceptor` behind its [`Arc`].
///
/// WHY: the [`Any::type_id`] of the [`Arc`] itself would be its own.
fn type_of(interceptor: &Arc<dyn ProcessInterceptor>) -> TypeId {
    let interceptor: &dyn ProcessInterceptor = interceptor.as_ref();
    Any::type_id(interceptor)
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

impl Default for CommandOutput {
    fn default() -> Self {
        Self {
            success: false,
            status: None,
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

impl<A: Attempt + ?Sized> Execute for Executor<'_, '_, A> {
    fn execute(&mut self, run: &mut CommandRun) -> Result<CommandOutput> {
        begin(run)?;
        let run = &*run;
        let capture = self.sink.is_none();
        let (attempts, layers, mut sink) = (self.attempts, self.layers, self.sink.as_deref_mut());
        let outputs = Outputs::new(capture, move |stream, data| {
            emit(run, layers, &mut sink, stream, data)
        });

        match run.output.clone() {
            Some(output) => deliver(run, output, outputs),
            None => attempts.attempt(run, outputs),
        }
        .map(judge)
    }

    fn sleep(&self, delay: Duration) {
        self.attempts.sleep(delay);
    }
}

#[cfg(feature = "tokio")]
impl<A: Attempt + ?Sized> ExecuteAsync for Executor<'_, '_, A> {
    fn execute<'a>(&'a mut self, run: &'a mut CommandRun) -> BoxFuture<'a, Result<CommandOutput>> {
        Box::pin(async move {
            begin(run)?;
            let run = &*run;
            let capture = self.sink.is_none();
            let (attempts, layers, mut sink) =
                (self.attempts, self.layers, self.sink.as_deref_mut());
            let mut cancel = Cancel::new(run, layers);
            let outputs = Outputs::new(capture, move |stream, data| {
                emit(run, layers, &mut sink, stream, data)
            });
            let result = match run.output.clone() {
                Some(output) => deliver(run, output, outputs),
                None => attempts.attempt_async(run, outputs).await,
            };
            cancel.disarm();
            result.map(judge)
        })
    }

    fn delay(&self, delay: Duration) -> BoxFuture<'static, ()> {
        self.attempts.delay(delay)
    }

    fn layers(&self) -> &[Arc<dyn ProcessInterceptor>] {
        self.layers
    }
}

#[cfg(feature = "tokio")]
impl<'a> Cancel<'a> {
    fn new(run: &'a CommandRun, layers: &'a [Arc<dyn ProcessInterceptor>]) -> Self {
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

impl ProcessImpl {
    /// [internal]
    ///
    /// Creates a new `ProcessImpl` running child processes in
    /// `current_dir` with the inherited environment.
    #[cfg(test)]
    pub(crate) fn new(current_dir: PathBuf) -> Self {
        Self::with_env(SharedEnv::new(Vec::new(), current_dir))
    }

    /// [internal]
    ///
    /// Creates a new `ProcessImpl` running child processes in the current
    /// directory of `env` with its variable changes applied.
    pub(crate) fn with_env(env: SharedEnv) -> Self {
        let mut interceptors = CommandInterceptors::new();
        interceptors.push(StdStream);

        ProcessImpl {
            env,
            interceptors: SharedCell::new(interceptors),
        }
    }

    /// Returns a new command with the program, the arguments and the
    /// environment variables of the run resolved against the
    /// configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if the program is not found, see
    /// [`Self::program`].
    fn child_command(&self, run: &CommandRun) -> std::io::Result<Command> {
        let options = run.options();
        // WHY: only runs resolved against the env are run.
        let dir = run.current_dir().unwrap_or(Path::new(""));
        let changes = run.var_changes();
        #[cfg(unix)]
        let program = self.program(run, dir, changes)?;
        #[cfg(windows)]
        let program = self.program(run, dir);
        let mut child = Command::new(program.as_os_str());
        child.args(run.args()).current_dir(dir);

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
        } else {
            for (key, value) in changes {
                match value {
                    Some(value) => child.env(key, value),
                    None => child.env_remove(key),
                };
            }
        }

        for (key, value) in run.envs() {
            match value {
                Some(value) => child.env(key, value),
                None => child.env_remove(key),
            };
        }

        Ok(child)
    }

    /// Returns the program of `run` as the child process running in `dir`
    /// finds it.
    ///
    /// WHY: std resolves a relative path with a separator against the
    /// current directory of the current process on Windows. On Unix it
    /// looks a bare name up in the `PATH` of the child process, which
    /// [`CommandOptions::clear_env`] removes, and only by forking when it
    /// differs from the inherited one.
    #[cfg(unix)]
    fn program<'a>(
        &self,
        run: &'a CommandRun,
        dir: &Path,
        changes: &[(String, Option<String>)],
    ) -> std::io::Result<Cow<'a, Path>> {
        let program = Path::new(&run.program);

        if program
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .any(|byte| std::path::is_separator(char::from(*byte)))
        {
            Ok(Cow::Owned(dir.join(program)))
        } else {
            self.search_path(run, program, dir, changes)
        }
    }

    /// Returns the program of `run` as the child process running in `dir`
    /// finds it.
    ///
    /// WHY: std resolves a relative path with a separator against the
    /// current directory of the current process on Windows. A bare name is
    /// looked up by std from the child's `PATH`.
    #[cfg(windows)]
    fn program<'a>(&self, run: &'a CommandRun, dir: &Path) -> Cow<'a, Path> {
        let program = Path::new(&run.program);

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
    /// WHY: std on Windows falls back to the `PATH` of the current process.
    #[cfg(windows)]
    fn search_path<'a>(&self, program: &'a Path) -> Cow<'a, Path> {
        let _ = self;
        Cow::Borrowed(program)
    }

    /// Returns the bare `program` of `run` found in the `PATH` the child
    /// process would get without [`CommandOptions::clear_env`] when its
    /// `PATH` differs from the inherited one, otherwise the `program` for
    /// std to look up.
    #[cfg(unix)]
    fn search_path<'a>(
        &self,
        run: &CommandRun,
        program: &'a Path,
        dir: &Path,
        changes: &[(String, Option<String>)],
    ) -> std::io::Result<Cow<'a, Path>> {
        let inherited = std::env::var_os("PATH");
        let command_path = run
            .envs()
            .iter()
            .find(|(key, _)| key == "PATH")
            .map(|(_, value)| value.as_deref());
        let configured_path = changes
            .iter()
            .find(|(key, _)| key == "PATH")
            .map_or(inherited.as_deref(), |(_, value)| {
                value.as_deref().map(OsStr::new)
            });
        let path = command_path.unwrap_or(configured_path);
        let child_path = if run.options().clear_env {
            command_path.flatten()
        } else {
            path
        };

        match path.filter(|_| child_path != inherited.as_deref()) {
            Some(path) => unix::find_program(program, path, dir).map(Cow::Owned),
            None => Ok(Cow::Borrowed(program)),
        }
    }
}

impl Attempt for ProcessImpl {
    fn attempt(&self, run: &CommandRun, mut outputs: Outputs) -> Result<CommandOutput> {
        let options = run.options();
        let start = Instant::now();
        let deadline = options
            .timeout
            .and_then(|timeout| start.checked_add(timeout));
        let result = self
            .child_command(run)
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

        finish(run, start, result, outputs)
    }

    #[cfg(feature = "tokio")]
    fn attempt_async<'a>(
        &'a self,
        run: &'a CommandRun,
        outputs: Outputs<'a>,
    ) -> BoxFuture<'a, Result<CommandOutput>> {
        Box::pin(async move {
            let outputs = Mutex::new(outputs);
            let options = run.options();
            let group = !options.interactive;
            let start = Instant::now();
            let deadline = options
                .timeout
                .and_then(|timeout| start.checked_add(timeout));
            let result = match self.child_command(run).and_then(|mut child| {
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

            finish(
                run,
                start,
                result,
                outputs.into_inner().unwrap_or_else(PoisonError::into_inner),
            )
        })
    }

    fn sleep(&self, delay: Duration) {
        std::thread::sleep(delay);
    }

    #[cfg(feature = "tokio")]
    fn delay(&self, delay: Duration) -> BoxFuture<'static, ()> {
        Box::pin(tokio::time::sleep(delay))
    }
}

impl Process for ProcessImpl {
    fn run(&self, run: impl Into<CommandRun>) -> Result<CommandOutput> {
        run_chain(
            self,
            &self.interceptors,
            run.into().resolve(&self.env),
            None,
        )
    }

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
            .update(|interceptors| interceptors.0.clear());
    }

    fn add_interceptor(&self, interceptor: impl ProcessInterceptor) {
        self.interceptors
            .update(|interceptors| interceptors.push(interceptor));
    }
}

impl ProcessInterceptor for StdStream {
    fn output(&self, chunk: CommandChunk) -> Result {
        if chunk.run().options().stream {
            let _ = match chunk.stream() {
                OutputStream::Stdout => write_flushed(&mut std::io::stdout().lock(), chunk.data()),
                OutputStream::Stderr => write_flushed(&mut std::io::stderr().lock(), chunk.data()),
            };
        }

        chunk.next()
    }
}

impl ProcessInterceptor for CommandRetry {
    fn handle(&self, mut next: CommandNext) -> Result<CommandOutput> {
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
        mut next: AsyncCommandNext<'a>,
    ) -> BoxFuture<'a, Result<CommandOutput>> {
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

impl ProcessInterceptor for CommandDryRun {
    fn handle(&self, mut next: CommandNext) -> Result<CommandOutput> {
        dry_run(next.run_mut());
        next.next()
    }

    #[cfg(feature = "tokio")]
    fn handle_async<'a>(
        &'a self,
        mut next: AsyncCommandNext<'a>,
    ) -> BoxFuture<'a, Result<CommandOutput>> {
        dry_run(next.run_mut());
        Box::pin(async move { next.next().await })
    }
}

impl ProcessInterceptor for CommandCache {
    fn handle(&self, mut next: CommandNext) -> Result<CommandOutput> {
        let key = self.hit(next.run_mut());
        let result = next.next();
        self.store(next.run(), key, &result);
        result
    }

    #[cfg(feature = "tokio")]
    fn handle_async<'a>(
        &'a self,
        mut next: AsyncCommandNext<'a>,
    ) -> BoxFuture<'a, Result<CommandOutput>> {
        Box::pin(async move {
            let key = self.hit(next.run_mut());
            let result = next.next().await;
            self.store(next.run(), key, &result);
            result
        })
    }
}

/// Sets the dry run output of `run`, see [`CommandDryRun`].
fn dry_run(run: &mut CommandRun) {
    if !run.intercepted() {
        let output = run.get::<DryRunOutput<CommandOutput>>().map_or_else(
            || CommandOutput {
                status: Some(0),
                ..Default::default()
            },
            |output| output.0.clone(),
        );
        run.set_output(output);
    }
}

/// Writes `data` to `out` and flushes it.
fn write_flushed(out: &mut dyn Write, data: &[u8]) -> std::io::Result<()> {
    out.write_all(data)?;
    out.flush()
}

/// [internal]
///
/// Runs `run` by `attempts` through its interceptors, or those of the
/// `process`, passing its output to `sink`, if any.
pub(crate) fn run_chain<A: Attempt>(
    attempts: &A,
    process: &SharedCell<CommandInterceptors>,
    mut run: CommandRun,
    sink: Option<&mut OutputSink>,
) -> Result<CommandOutput> {
    run.id = crate::utils::next_run_id();
    let layers = run.layers(process);
    accept(&run, &layers)?;
    let result = CommandNext {
        run: &mut run,
        layers: &layers,
        inner: &mut Executor {
            attempts,
            layers: &layers,
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
pub(crate) async fn run_chain_async<A: Attempt>(
    attempts: &A,
    process: &SharedCell<CommandInterceptors>,
    mut run: CommandRun,
    sink: Option<&mut OutputSink<'_>>,
) -> Result<CommandOutput> {
    run.id = crate::utils::next_run_id();
    let layers = run.layers(process);
    accept(&run, &layers)?;
    let result = AsyncCommandNext {
        run: &mut run,
        layers: &layers,
        inner: &mut Executor {
            attempts,
            layers: &layers,
            sink,
        },
    }
    .next()
    .await;

    conclude(&run, result)
}

/// Rejects `run` with invalid options before it enters the chain, passing
/// the error to its interceptors `layers`.
fn accept(run: &CommandRun, layers: &[Arc<dyn ProcessInterceptor>]) -> Result<()> {
    validate(&run.program(), run.options(), run.sink)
        .inspect_err(|error| layers.iter().for_each(|layer| layer.invalid(run, error)))
}

/// Starts the attempt of `run`, rejecting the invalid options set by the
/// interceptors.
fn begin(run: &mut CommandRun) -> Result<()> {
    run.attempt = run.attempt.saturating_add(1);
    validate(&run.program(), run.options(), run.sink)
}

/// Passes the chunk of `data` of the output `stream` of `run` through its
/// interceptors `layers` to `sink`, if any.
fn emit(
    run: &CommandRun,
    layers: &[Arc<dyn ProcessInterceptor>],
    sink: &mut Option<&mut OutputSink>,
    stream: OutputStream,
    data: &[u8],
) -> Result {
    CommandChunk {
        run,
        stream,
        data: Cow::Borrowed(data),
        layers,
        sink: sink.as_mut().map(|sink| &mut **sink as &mut OutputSink),
    }
    .next()
}

/// Returns `output` with its success set from its status.
fn judge(output: CommandOutput) -> CommandOutput {
    CommandOutput {
        success: output.status == Some(0) && !output.timed_out,
        ..output
    }
}

/// Returns the `result` of `run` with an output that is not a success as an
/// error.
fn conclude(run: &CommandRun, result: Result<CommandOutput>) -> Result<CommandOutput> {
    match result {
        Ok(output) if !output.success => Err(failure(run, output)),
        result => result,
    }
}

/// [internal]
///
/// Returns the error of `run` failing with `output` as its source.
pub(crate) fn failure(run: &CommandRun, output: CommandOutput) -> crate::Error {
    let program = run.program();
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

/// [internal]
///
/// Rejects the combinations of `options` that cannot work.
pub(crate) fn validate(program: &str, options: &CommandOptions, sink: bool) -> Result<()> {
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

/// [internal]
///
/// Returns the output of `run` from the `output` set for it, e.g. by a test
/// or an interceptor, as if it ran: dropped if the run is silent or
/// interactive and passed through `outputs`.
pub(crate) fn deliver(
    run: &CommandRun,
    output: CommandOutput,
    mut outputs: Outputs,
) -> Result<CommandOutput> {
    let options = run.options();
    let output = CommandOutput {
        display_lines: options.display_lines,
        ..output
    };

    if !options.silent && !options.interactive {
        if !output.stdout.is_empty() {
            outputs.push(OutputStream::Stdout, &output.stdout);
        }

        if !outputs.failed() && !output.stderr.is_empty() {
            outputs.push(OutputStream::Stderr, &output.stderr);
        }
    }

    let (stdout, stderr) = outputs.finish()?;

    Ok(CommandOutput {
        stdout,
        stderr,
        ..output
    })
}

/// Returns the output of the attempt of `run` started at `start` from the
/// `result` of its child process.
fn finish(
    run: &CommandRun,
    start: Instant,
    result: std::io::Result<Option<std::process::ExitStatus>>,
    outputs: Outputs,
) -> Result<CommandOutput> {
    let program = run.program();
    let (stdout, stderr) = outputs.finish()?;
    let status = result.map_err(|error| io_error(&program, error))?;

    Ok(CommandOutput {
        success: false,
        status: status.and_then(|status| status.code()),
        signal: status.as_ref().and_then(exit_signal),
        stdout,
        stderr,
        duration: start.elapsed(),
        timed_out: status.is_none(),
        display_lines: run.options().display_lines,
    })
}

const DISPLAY_LINES: usize = 10;

/// How long the output of a child process is read at most after it exited,
/// see [`Process::run`].
pub(crate) const EXIT_GRACE: Duration = Duration::from_millis(100);

impl<'a> Outputs<'a> {
    /// Creates the outputs passing the chunks to `emit`, captured if
    /// `capture`.
    pub(crate) fn new(
        capture: bool,
        emit: impl FnMut(OutputStream, &[u8]) -> Result + Send + 'a,
    ) -> Self {
        Self {
            stdout: Vec::new(),
            stderr: Vec::new(),
            capture,
            emit: Box::new(emit),
            error: None,
        }
    }

    /// Takes the `data` of the output `stream` of the child process.
    pub(crate) fn push(&mut self, stream: OutputStream, data: &[u8]) {
        match (self.emit)(stream, data) {
            Err(error) => self.error = Some(error),
            Ok(()) if !self.capture => {}
            Ok(()) if stream == OutputStream::Stdout => self.stdout.extend_from_slice(data),
            Ok(()) => self.stderr.extend_from_slice(data),
        }
    }

    /// Returns `true` once passing a chunk on failed, which ends the attempt.
    pub(crate) fn failed(&self) -> bool {
        self.error.is_some()
    }

    /// Returns the standard output and error captured, failing with the
    /// error of passing a chunk on if it failed.
    pub(crate) fn finish(self) -> Result<(Vec<u8>, Vec<u8>)> {
        self.error.map_or(Ok((self.stdout, self.stderr)), Err)
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
    use crate::TestDir;
    use std::error::Error as _;

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

        impl ProcessInterceptor for ExitOnOutput {
            fn output(&self, _chunk: CommandChunk) -> Result {
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
    struct Recorder(Arc<Mutex<Recorded>>);

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

        fn started(&self, run: &CommandRun) {
            let mut recorded = self.recorded();
            recorded.events.push(format!(
                "started in {}",
                run.current_dir().unwrap().display()
            ));
            recorded.options.push(run.options().clone());
        }

        fn finished(&self, result: &Result<CommandOutput>) {
            self.recorded().events.push(match result {
                Ok(output) if output.success => "finished ok".to_string(),
                Ok(output) => format!("finished failed {:?}", output.status),
                Err(error) => format!("finished {error}"),
            });
        }
    }

    impl ProcessInterceptor for Recorder {
        fn handle(&self, mut next: CommandNext) -> Result<CommandOutput> {
            self.started(next.run());
            let result = next.next();
            self.finished(&result);
            result
        }

        #[cfg(feature = "tokio")]
        fn handle_async<'a>(
            &'a self,
            mut next: AsyncCommandNext<'a>,
        ) -> BoxFuture<'a, Result<CommandOutput>> {
            Box::pin(async move {
                self.started(next.run());
                let result = next.next().await;
                self.finished(&result);
                result
            })
        }

        fn output(&self, chunk: CommandChunk) -> Result {
            match chunk.stream() {
                OutputStream::Stdout => self.recorded().stdout.extend_from_slice(chunk.data()),
                OutputStream::Stderr => self.recorded().stderr.extend_from_slice(chunk.data()),
            }

            chunk.next()
        }

        #[cfg(feature = "tokio")]
        fn cancelled(&self, run: &CommandRun) {
            self.recorded()
                .events
                .push(format!("cancelled {}", run.program()));
        }

        fn invalid(&self, run: &CommandRun, error: &crate::Error) {
            let mut recorded = self.recorded();
            recorded.events.push(format!("invalid {error}"));
            recorded.options.push(run.options().clone());
        }
    }

    /// Calls the closure with the run before passing it on.
    struct Inspect<F>(F);

    impl<F: Fn(&mut CommandRun) + Send + Sync + 'static> ProcessInterceptor for Inspect<F> {
        fn handle(&self, mut next: CommandNext) -> Result<CommandOutput> {
            (self.0)(next.run_mut());
            next.next()
        }

        #[cfg(feature = "tokio")]
        fn handle_async<'a>(
            &'a self,
            mut next: AsyncCommandNext<'a>,
        ) -> BoxFuture<'a, Result<CommandOutput>> {
            (self.0)(next.run_mut());
            Box::pin(async move { next.next().await })
        }
    }

    /// Accepts the outputs with the exit code.
    struct Accept(i32);

    impl ProcessInterceptor for Accept {
        fn handle(&self, mut next: CommandNext) -> Result<CommandOutput> {
            next.next().map(|output| self.accept(output))
        }
    }

    impl Accept {
        fn accept(&self, output: CommandOutput) -> CommandOutput {
            CommandOutput {
                success: output.success || output.status == Some(self.0),
                ..output
            }
        }
    }

    /// An interceptor with the default methods only.
    struct Passing;

    impl ProcessInterceptor for Passing {}

    /// Changes of the env the process shares, like by its platform.
    impl ProcessImpl {
        fn set_var(&self, key: String, value: String) {
            self.env.set_var(key, value);
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
    fn retried(retry: CommandRetry, recorder: &Recorder) -> CommandInterceptors {
        let mut interceptors = CommandInterceptors::new();
        interceptors.push(retry);
        interceptors.push(recorder.clone());
        interceptors
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

    mod process_interceptor {
        use super::*;

        mod handle {
            use super::*;

            #[test]
            fn handle_should_run_rest_of_chain_by_default() {
                let (process, recorder) = observed();
                process.add_interceptor(Passing);

                let output = process.run(&shell("echo out")).unwrap();

                assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "out");
                assert_eq!(recorder.events().len(), 2);
            }

            #[test]
            fn handle_making_options_invalid_should_fail_attempt() {
                let (process, recorder) = observed();
                process.add_interceptor(Inspect(|run: &mut CommandRun| {
                    run.options_mut().interactive = true;
                    run.options_mut().silent = true;
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

        #[cfg(feature = "tokio")]
        mod handle_async {
            use super::*;

            #[test]
            fn handle_async_should_run_rest_of_chain_by_default() {
                let (process, recorder) = observed();
                process.add_interceptor(Passing);

                let output = block_on(process.run_async(&shell("echo out"))).unwrap();

                assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "out");
                assert_eq!(recorder.events().len(), 2);
            }

            #[test]
            fn handle_async_making_options_invalid_should_fail_attempt() {
                let (process, recorder) = observed();
                process.add_interceptor(Inspect(|run: &mut CommandRun| {
                    run.options_mut().interactive = true;
                    run.options_mut().silent = true;
                }));

                let error = block_on(process.run_async(&shell("exit 0"))).unwrap_err();

                assert!(
                    error
                        .description()
                        .ends_with("': interactive cannot be combined with silent")
                );
                assert_eq!(recorder.events()[1], format!("finished {error}"));
            }
        }

        mod output {
            use super::*;

            #[test]
            fn output_should_pass_chunk_on_by_default() {
                let process = process(std::env::temp_dir());
                process.clear_interceptors();
                process.add_interceptor(Passing);
                let mut sunk = (Vec::new(), Vec::new());

                process
                    .run_to(&shell("echo out"), &mut sink_into(&mut sunk))
                    .unwrap();

                assert_eq!(String::from_utf8_lossy(&sunk.0).trim(), "out");
            }
        }

        #[cfg(feature = "tokio")]
        mod cancelled {
            use super::*;

            #[test]
            fn cancelled_should_do_nothing_by_default() {
                let (process, recorder) = observed();
                process.add_interceptor(Passing);

                let result = block_on(async {
                    tokio::time::timeout(
                        Duration::from_millis(200),
                        process.run_async(&shell(SLEEP_THEN_ECHO)),
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

        mod invalid {
            use super::*;

            #[test]
            fn invalid_should_do_nothing_by_default() {
                let (process, recorder) = observed();
                process.add_interceptor(Passing);

                let error = process
                    .run(shell("exit 0").interactive().silent())
                    .unwrap_err();

                assert_eq!(recorder.events(), [format!("invalid {error}")]);
            }
        }
    }

    mod command_next {
        use super::*;

        mod run {
            use super::*;

            #[test]
            fn run_should_return_run_as_changed_before() {
                let (process, recorder) = observed();
                process.clear_interceptors();
                process.add_interceptor(Inspect(|run: &mut CommandRun| {
                    run.options_mut().display_lines = 3
                }));
                process.add_interceptor(recorder.clone());

                process.run(&shell("exit 0")).unwrap();

                assert_eq!(recorder.options().display_lines, 3);
            }
        }

        mod run_mut {
            use super::*;

            #[test]
            fn run_mut_should_change_run_of_rest_of_chain() {
                let process = process(std::env::temp_dir());
                process.add_interceptor(Inspect(|run: &mut CommandRun| {
                    run.set_env("AGPLATFORM_CHANGED", "changed");
                }));

                let output = process.run(&print_var("AGPLATFORM_CHANGED")).unwrap();

                assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "changed");
            }
        }

        mod next {
            use super::*;

            #[test]
            fn next_should_run_interceptors_in_order_then_command() {
                // GIVEN interceptors recording their order around the command
                let order = Arc::new(Mutex::new(Vec::new()));
                let process = process(std::env::temp_dir());
                process.clear_interceptors();
                for label in ["outer", "inner"] {
                    let order = order.clone();
                    process.add_interceptor(Inspect(move |_: &mut CommandRun| {
                        order.lock().unwrap().push(label);
                    }));
                }

                // WHEN running a command
                let output = process.run(&shell("echo out")).unwrap();

                // THEN the outer one runs first and the command last
                assert_eq!(*order.lock().unwrap(), ["outer", "inner"]);
                assert!(output.success);
            }

            #[test]
            fn next_failing_output_should_be_ok_inside_chain() {
                let (process, recorder) = observed();

                let error = process.run(&shell("exit 3")).unwrap_err();

                assert_eq!(recorder.events()[1], "finished failed Some(3)");
                assert_eq!(
                    error.description(),
                    format!("'{SHELL}' failed with exit code 3")
                );
            }

            #[test]
            fn next_called_again_should_run_command_again() {
                struct Twice;

                impl ProcessInterceptor for Twice {
                    fn handle(&self, mut next: CommandNext) -> Result<CommandOutput> {
                        let _ = next.next();
                        next.next()
                    }
                }

                let dir = TestDir::new();
                let process = process(dir.path().to_path_buf());
                process.add_interceptor(Twice);

                let output = process.run(&shell(FAIL_ONCE)).unwrap();

                assert!(output.success);
            }
        }

        mod sleep {
            use super::*;

            #[test]
            fn sleep_should_wait() {
                struct Sleeping;

                impl ProcessInterceptor for Sleeping {
                    fn handle(&self, mut next: CommandNext) -> Result<CommandOutput> {
                        next.sleep(Duration::from_millis(50));
                        next.next()
                    }
                }

                let process = process(std::env::temp_dir());
                process.add_interceptor(Sleeping);
                let start = Instant::now();

                process.run(&shell("exit 0")).unwrap();

                assert!(start.elapsed() >= Duration::from_millis(50));
            }
        }
    }

    #[cfg(feature = "tokio")]
    mod async_command_next {
        use super::*;

        /// Waits `0` before passing the run on.
        struct Sleeping(Duration);

        impl ProcessInterceptor for Sleeping {
            fn handle_async<'a>(
                &'a self,
                mut next: AsyncCommandNext<'a>,
            ) -> BoxFuture<'a, Result<CommandOutput>> {
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
                let (process, recorder) = observed();
                process.clear_interceptors();
                process.add_interceptor(Inspect(|run: &mut CommandRun| {
                    run.options_mut().display_lines = 3
                }));
                process.add_interceptor(recorder.clone());

                block_on(process.run_async(&shell("exit 0"))).unwrap();

                assert_eq!(recorder.options().display_lines, 3);
            }
        }

        mod run_mut {
            use super::*;

            #[test]
            fn run_mut_should_change_run_of_rest_of_chain() {
                let process = process(std::env::temp_dir());
                process.add_interceptor(Inspect(|run: &mut CommandRun| {
                    run.set_env("AGPLATFORM_CHANGED", "changed");
                }));

                let output = block_on(process.run_async(&print_var("AGPLATFORM_CHANGED"))).unwrap();

                assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "changed");
            }
        }

        mod next {
            use super::*;

            #[test]
            fn next_should_run_interceptors_in_order_then_command() {
                let order = Arc::new(Mutex::new(Vec::new()));
                let process = process(std::env::temp_dir());
                process.clear_interceptors();
                for label in ["outer", "inner"] {
                    let order = order.clone();
                    process.add_interceptor(Inspect(move |_: &mut CommandRun| {
                        order.lock().unwrap().push(label);
                    }));
                }

                let output = block_on(process.run_async(&shell("echo out"))).unwrap();

                assert_eq!(*order.lock().unwrap(), ["outer", "inner"]);
                assert!(output.success);
            }
        }

        mod sleep {
            use super::*;

            #[test]
            fn sleep_should_wait() {
                let process = process(std::env::temp_dir());
                process.add_interceptor(Sleeping(Duration::from_millis(50)));
                let start = Instant::now();

                block_on(process.run_async(&shell("exit 0"))).unwrap();

                assert!(start.elapsed() >= Duration::from_millis(50));
            }

            #[test]
            fn sleep_cancelled_should_tell_interceptors() {
                // GIVEN an interceptor waiting long before the run
                let (process, recorder) = observed();
                process.add_interceptor(Sleeping(Duration::from_secs(3600)));

                // WHEN the run is cancelled while it waits
                let result = block_on(async {
                    tokio::time::timeout(
                        Duration::from_millis(50),
                        process.run_async(&shell("exit 0")),
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

    mod command_chunk {
        use super::*;

        /// Replaces the data of the chunks with `0`.
        struct Replace(&'static [u8]);

        impl ProcessInterceptor for Replace {
            fn output(&self, mut chunk: CommandChunk) -> Result {
                chunk.set_data(self.0.to_vec());
                chunk.next()
            }
        }

        /// Drops the chunks.
        struct Dropping;

        impl ProcessInterceptor for Dropping {
            fn output(&self, _chunk: CommandChunk) -> Result {
                Ok(())
            }
        }

        mod data {
            use super::*;

            #[test]
            fn data_should_return_chunk_as_read() {
                let (process, recorder) = observed();

                process.run(&shell("echo out&& echo err 1>&2")).unwrap();

                assert_eq!(String::from_utf8_lossy(&recorder.stdout()).trim(), "out");
                assert_eq!(String::from_utf8_lossy(&recorder.stderr()).trim(), "err");
            }
        }

        mod set_data {
            use super::*;

            #[test]
            fn set_data_should_replace_data_passed_on_but_not_captured() {
                // GIVEN an interceptor replacing the chunks inside a recorder
                let (process, recorder) = observed();
                process.add_interceptor(Replace(b"***"));
                let mut sunk = (Vec::new(), Vec::new());

                // WHEN running commands capturing or sinking their output
                let output = process.run(&shell("echo secret")).unwrap();
                process
                    .run_to(&shell("echo secret"), &mut sink_into(&mut sunk))
                    .unwrap();

                // THEN the replaced data passes on, the captured one is kept
                assert_eq!(recorder.stdout(), b"******");
                assert_eq!(sunk.0, b"***");
                assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "secret");
            }
        }

        mod next {
            use super::*;

            #[test]
            fn next_not_called_should_drop_chunk() {
                let (process, recorder) = observed();
                process.add_interceptor(Dropping);
                let mut sunk = (Vec::new(), Vec::new());

                process
                    .run_to(&shell("echo out"), &mut sink_into(&mut sunk))
                    .unwrap();

                assert!(recorder.stdout().is_empty());
                assert!(sunk.0.is_empty());
            }

            #[test]
            fn next_failing_should_end_attempt_with_error() {
                struct Failing;

                impl ProcessInterceptor for Failing {
                    fn output(&self, _chunk: CommandChunk) -> Result {
                        Err(crate::Error::process("rejected"))
                    }
                }

                let process = process(std::env::temp_dir());
                process.add_interceptor(Failing);
                let start = Instant::now();

                let error = process.run(&shell(ECHO_THEN_SLEEP)).unwrap_err();

                assert_eq!(error.description(), "rejected");
                assert!(start.elapsed() < Duration::from_secs(5));
            }
        }

        mod run {
            use super::*;

            #[test]
            fn run_should_return_run_of_chunk() {
                struct Programs(Arc<Mutex<Vec<String>>>);

                impl ProcessInterceptor for Programs {
                    fn output(&self, chunk: CommandChunk) -> Result {
                        self.0
                            .lock()
                            .unwrap()
                            .push(chunk.run().program().into_owned());
                        chunk.next()
                    }
                }

                let programs = Arc::new(Mutex::new(Vec::new()));
                let process = process(std::env::temp_dir());
                process.add_interceptor(Programs(programs.clone()));

                process.run(&shell("echo out")).unwrap();

                assert_eq!(*programs.lock().unwrap(), [SHELL]);
            }
        }
    }

    mod command_interceptors {
        use super::*;

        mod insert {
            use super::*;

            #[test]
            fn insert_should_add_interceptor_at_index() {
                // GIVEN interceptors with one inserted before the other
                let order = Arc::new(Mutex::new(Vec::new()));
                let mut interceptors = CommandInterceptors::new();
                for (index, label) in [(0, "inner"), (0, "outer")] {
                    let order = order.clone();
                    interceptors.insert(
                        index,
                        Inspect(move |_: &mut CommandRun| order.lock().unwrap().push(label)),
                    );
                }

                // WHEN running a command with them
                shell("exit 0")
                    .interceptors(interceptors)
                    .run(&process(std::env::temp_dir()))
                    .unwrap();

                // THEN the inserted one is the outermost
                assert_eq!(*order.lock().unwrap(), ["outer", "inner"]);
            }
        }

        mod is_empty {
            use super::*;

            #[test]
            fn is_empty_should_tell_whether_there_are_interceptors() {
                let mut interceptors = CommandInterceptors::new();
                assert!(interceptors.is_empty());

                interceptors.push(Passing);

                assert!(!interceptors.is_empty());
            }
        }

        mod len {
            use super::*;

            #[test]
            fn len_should_count_interceptors() {
                let mut interceptors = CommandInterceptors::default();
                interceptors.push(Passing);
                interceptors.insert(1, Passing);

                assert_eq!(interceptors.len(), 2);
            }
        }

        mod std_fmt_debug {
            use super::*;

            #[test]
            fn fmt_should_show_count() {
                let mut interceptors = CommandInterceptors::new();
                interceptors.push(StdStream);

                assert_eq!(format!("{interceptors:?}"), "CommandInterceptors(1)");
            }
        }

        mod partial_eq {
            use super::*;

            #[test]
            fn eq_should_compare_instances() {
                let mut interceptors = CommandInterceptors::new();
                interceptors.push(StdStream);
                let mut other = CommandInterceptors::new();
                other.push(StdStream);

                assert_eq!(interceptors, interceptors.clone());
                assert_ne!(interceptors, other);
                assert_ne!(interceptors, CommandInterceptors::new());
            }
        }
    }

    mod std_stream {
        use super::*;

        mod process_interceptor {
            use super::*;

            mod output {
                use super::*;

                #[test]
                fn output_should_pass_streamed_and_other_chunks_on() {
                    let recorder = Recorder::default();
                    let process = process(std::env::temp_dir());
                    process.add_interceptor(recorder.clone());
                    let mut interceptors = CommandInterceptors::new();
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

    mod command_retry {
        use super::*;

        /// Returns the delays before retrying a failure of `git` by `retry`
        /// after 0, 1 and 2 retries.
        fn delays(retry: &CommandRetry) -> [Option<Duration>; 3] {
            let run = CommandRun::new(&Command::new("git"));
            [0, 1, 2].map(|retried| retry.retry(&run, &Ok(exit(1)), retried))
        }

        mod backoff {
            use super::*;

            #[test]
            fn backoff_should_multiply_delay_per_retry() {
                let retry = CommandRetry::new(5, Duration::from_millis(100)).backoff(2);

                assert_eq!(
                    delays(&retry),
                    [100, 200, 400].map(|millis| Some(Duration::from_millis(millis)))
                );
            }
        }

        mod max_delay {
            use super::*;

            #[test]
            fn max_delay_should_limit_backoff() {
                let retry = CommandRetry::new(5, Duration::from_secs(10))
                    .backoff(4)
                    .max_delay(Duration::from_secs(20));

                assert_eq!(
                    delays(&retry),
                    [10, 20, 20].map(|seconds| Some(Duration::from_secs(seconds)))
                );
            }
        }

        mod new {
            use super::*;

            #[test]
            fn new_should_retry_failures_until_success() {
                // GIVEN a command failing on its first attempt only
                let dir = TestDir::new();
                let (process, recorder) = observed();
                process.set_current_dir(dir.path().to_path_buf());

                // WHEN running it with retries
                let output = shell(FAIL_ONCE)
                    .interceptors(retried(
                        CommandRetry::new(2, Duration::from_millis(1)),
                        &recorder,
                    ))
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

            #[test]
            fn new_exhausting_retries_should_return_last_failure() {
                let (process, recorder) = observed();

                let error = shell("exit 3")
                    .interceptors(retried(CommandRetry::new(2, Duration::ZERO), &recorder))
                    .run(&process)
                    .unwrap_err();

                assert_eq!(error.command_output().unwrap().status, Some(3));
                assert_eq!(recorder.events().len(), 6);
            }

            #[test]
            fn new_should_retry_errors() {
                let attempts = Arc::new(Mutex::new(Vec::new()));
                let seen = attempts.clone();
                let mut interceptors = CommandInterceptors::new();
                interceptors.push(CommandRetry::new(1, Duration::ZERO));
                interceptors.push(Inspect(move |run: &mut CommandRun| {
                    seen.lock().unwrap().push(run.attempt());
                }));

                let error = Command::new("agplatform_missing_program")
                    .interceptors(interceptors)
                    .run(&process(std::env::temp_dir()))
                    .unwrap_err();

                assert!(error.source_as::<std::io::Error>().is_some());
                assert_eq!(*attempts.lock().unwrap(), [0, 1]);
            }
        }

        mod when {
            use super::*;

            #[test]
            fn when_should_retry_only_matching_failures() {
                fn exit_3(_: &CommandRun, result: &Result<CommandOutput>) -> bool {
                    result.as_ref().is_ok_and(|output| output.status == Some(3))
                }

                let (process, recorder) = observed();
                let interceptors =
                    retried(CommandRetry::new(1, Duration::ZERO).when(exit_3), &recorder);

                let _ = shell("exit 3")
                    .interceptors(interceptors.clone())
                    .run(&process);
                let _ = shell("exit 4").interceptors(interceptors).run(&process);

                assert_eq!(recorder.events().len(), 6);
            }
        }

        mod std_fmt_debug {
            use super::*;

            #[test]
            fn fmt_should_show_configuration() {
                let retry = CommandRetry::new(2, Duration::from_secs(1));

                assert_eq!(
                    format!("{retry:?}"),
                    "CommandRetry { retries: 2, delay: 1s, backoff: 1, max_delay: 60s, \
                     condition: false }"
                );
                assert_eq!(
                    format!("{:?}", retry.when(|_, _| true)),
                    "CommandRetry { retries: 2, delay: 1s, backoff: 1, max_delay: 60s, \
                     condition: true }"
                );
            }
        }

        mod process_interceptor {
            use super::*;

            mod handle {
                use super::*;

                #[test]
                fn handle_should_not_retry_accepted_output() {
                    let (process, recorder) = observed();
                    let mut interceptors = retried(CommandRetry::new(2, Duration::ZERO), &recorder);
                    interceptors.push(Accept(1));

                    let output = shell("exit 1")
                        .interceptors(interceptors)
                        .run(&process)
                        .unwrap();

                    assert_eq!(output.status, Some(1));
                    assert_eq!(recorder.events().len(), 2);
                }

                #[test]
                fn handle_intercepted_failure_should_not_retry() {
                    let (process, recorder) = observed();
                    let mut interceptors = retried(CommandRetry::new(2, Duration::ZERO), &recorder);
                    interceptors.push(Inspect(|run: &mut CommandRun| run.set_output(exit(1))));

                    let error = shell("exit 0")
                        .interceptors(interceptors)
                        .run(&process)
                        .unwrap_err();

                    assert_eq!(error.command_output().unwrap().status, Some(1));
                    assert_eq!(recorder.events().len(), 2);
                    assert_eq!(recorder.events()[1], "finished failed Some(1)");
                }

                #[test]
                fn handle_invalid_options_should_not_retry() {
                    let (process, recorder) = observed();

                    let error = shell("exit 0")
                        .interactive()
                        .timeout(Duration::from_secs(1))
                        .interceptors(retried(CommandRetry::new(2, Duration::ZERO), &recorder))
                        .run(&process)
                        .unwrap_err();

                    assert_eq!(recorder.events(), [format!("invalid {error}")]);
                }

                #[test]
                fn handle_after_sink_accepted_output_should_not_retry() {
                    let (process, recorder) = observed();
                    let mut sunk = (Vec::new(), Vec::new());

                    let error = shell("echo out&& exit 3")
                        .interceptors(retried(CommandRetry::new(2, Duration::ZERO), &recorder))
                        .run_to(&process, &mut sink_into(&mut sunk))
                        .unwrap_err();

                    assert_eq!(error.command_output().unwrap().status, Some(3));
                    assert_eq!(recorder.events().len(), 2);
                }

                #[test]
                fn handle_should_wait_delay_between_attempts() {
                    let start = Instant::now();

                    let _ = shell("exit 1")
                        .interceptors(retried(
                            CommandRetry::new(1, Duration::from_millis(50)),
                            &Recorder::default(),
                        ))
                        .run(&process(std::env::temp_dir()));

                    assert!(start.elapsed() >= Duration::from_millis(50));
                }
            }

            #[cfg(feature = "tokio")]
            mod handle_async {
                use super::*;

                #[test]
                fn handle_async_should_retry_failures_until_success() {
                    let dir = TestDir::new();
                    let (process, recorder) = observed();
                    process.set_current_dir(dir.path().to_path_buf());

                    let output = block_on(
                        shell(FAIL_ONCE)
                            .interceptors(retried(
                                CommandRetry::new(2, Duration::from_millis(1)),
                                &recorder,
                            ))
                            .run_async(&process),
                    )
                    .unwrap();

                    assert_eq!(output.status, Some(0));
                    assert_eq!(recorder.events().len(), 4);
                }

                #[test]
                fn handle_async_exhausting_retries_should_return_last_failure() {
                    let (process, recorder) = observed();

                    let error = block_on(
                        shell("exit 3")
                            .interceptors(retried(CommandRetry::new(1, Duration::ZERO), &recorder))
                            .run_async(&process),
                    )
                    .unwrap_err();

                    assert_eq!(error.command_output().unwrap().status, Some(3));
                    assert_eq!(recorder.events().len(), 4);
                }
            }
        }
    }

    mod command_dry_run {
        use super::*;

        mod process_interceptor {
            use super::*;

            mod handle {
                use super::*;

                #[test]
                fn handle_should_return_output_set_on_run_or_empty_success() {
                    // GIVEN a dry run where one command has its output set
                    let dir = TestDir::new();
                    let process = process(dir.path().to_path_buf());
                    process.add_interceptor(CommandDryRun);
                    let set = CommandOutput {
                        stdout: b"pods".to_vec(),
                        ..exit(0)
                    };

                    // WHEN running commands that would create a marker
                    let empty = process.run(&shell("touch marker")).unwrap();
                    let output = shell("touch marker")
                        .with(DryRunOutput(set.clone()))
                        .run(&process)
                        .unwrap();

                    // THEN they return the dry run outputs without running
                    assert_eq!(empty, exit(0));
                    assert_eq!(output, set);
                    assert!(!dir.path().join("marker").exists());
                }

                #[test]
                fn handle_should_keep_output_set_before() {
                    let process = process(std::env::temp_dir());
                    process
                        .add_interceptor(Inspect(|run: &mut CommandRun| run.set_output(exit(7))));
                    process.add_interceptor(CommandDryRun);

                    let error = process.run(&shell("exit 0")).unwrap_err();

                    assert_eq!(error.command_output().unwrap().status, Some(7));
                }
            }

            #[cfg(feature = "tokio")]
            mod handle_async {
                use super::*;

                #[test]
                fn handle_async_should_return_output_set_on_run() {
                    let process = process(std::env::temp_dir());
                    process.add_interceptor(CommandDryRun);

                    let output = block_on(
                        shell("exit 1")
                            .with(DryRunOutput(exit(0)))
                            .run_async(&process),
                    )
                    .unwrap();

                    assert_eq!(output, exit(0));
                }
            }
        }
    }

    mod command_cache {
        use super::*;

        /// Returns a process caching in `cache` the runs in `dir`.
        fn cached(dir: &TestDir, cache: &CommandCache) -> ProcessImpl {
            let process = process(dir.path().to_path_buf());
            process.add_interceptor(cache.clone());
            process
        }

        mod entries {
            use super::*;

            #[test]
            fn entries_should_return_cached_outputs_by_key() {
                let dir = TestDir::new();
                let cache = CommandCache::default();

                cached(&dir, &cache).run(&shell("echo out")).unwrap();

                let entries = cache.entries();
                assert_eq!(entries.len(), 1);
                assert!(entries[0].0.contains("\"echo out\""), "{entries:?}");
                assert_eq!(String::from_utf8_lossy(&entries[0].1.stdout).trim(), "out");
            }
        }

        mod key {
            use super::*;

            #[test]
            fn key_should_name_dir_command_vars_and_input() {
                let mut command = Command::new("git");
                command.arg("status").env("SET", "1").env_remove("REMOVED");
                let run = CommandRun::new(&command).input("in");

                assert_eq!(
                    CommandCache::key(&run),
                    format!(
                        "dir: \"\"\ncommand: [\"git\", \"status\"]\n\
                         envs: [(\"REMOVED\", \"removed\"), (\"SET\", \"set\")]\n\
                         clear_env: false\nsilent: false\nvars: []\nvalues: {:016x}\ninput: in",
                        crate::utils::fnv1a([None, Some(b"1".as_slice())])
                    )
                );
            }

            #[test]
            fn key_should_hash_values_of_vars() {
                let mut command = Command::new("git");
                command.env("TOKEN", "s3cr3t");
                let mut run = CommandRun::new(&command);
                run.var_changes = vec![("PLATFORM_TOKEN".to_string(), Some("t0k3n".to_string()))];

                let key = CommandCache::key(&run);

                assert!(!key.contains("s3cr3t") && !key.contains("t0k3n"), "{key}");
                assert!(
                    key.contains("vars: [(\"PLATFORM_TOKEN\", \"set\")]"),
                    "{key}"
                );
            }

            #[test]
            fn key_should_differ_for_runs_with_different_output() {
                let run = |args: &[&str]| {
                    let mut command = Command::new("printf");
                    command.args(args);
                    CommandRun::new(&command)
                };
                let mut configured = run(&["a"]);
                configured.var_changes = vec![("VAR".to_string(), None)];
                let mut set = run(&["a"]);
                set.var_changes = vec![("VAR".to_string(), Some("1".to_string()))];
                let mut changed = run(&["a"]);
                changed.var_changes = vec![("VAR".to_string(), Some("2".to_string()))];
                let runs = [
                    run(&["a b"]),
                    run(&["a", "b"]),
                    run(&["a"]),
                    run(&["a"]).silent(),
                    run(&["a"]).clear_env(),
                    configured,
                    set,
                    changed,
                    run(&["a"]).input(b"\xFF".to_vec()),
                    run(&["a"]).input(b"\xFE".to_vec()),
                ];

                let keys = runs
                    .iter()
                    .map(CommandCache::key)
                    .collect::<std::collections::BTreeSet<_>>();

                assert_eq!(keys.len(), runs.len(), "{keys:?}");
            }
        }

        mod load {
            use super::*;

            #[test]
            fn load_should_return_loaded_outputs_instead_of_running() {
                // GIVEN a cache loaded with the output of a command
                let dir = TestDir::new();
                let cache = CommandCache::default();
                let process = cached(&dir, &cache);
                let command = shell("touch marker");
                let key = CommandCache::key(&CommandRun::new(&command).resolve(&process.env));
                cache.load([(key, exit(0))]);

                // WHEN running it
                let output = process.run(&command).unwrap();

                // THEN the loaded output is returned without running it
                assert_eq!(output, exit(0));
                assert!(!dir.path().join("marker").exists());
            }

            #[test]
            fn load_longer_than_timeout_should_return_loaded_output() {
                let dir = TestDir::new();
                let cache = CommandCache::default();
                let process = cached(&dir, &cache);
                let run = || shell("exit 1").timeout(Duration::from_secs(1));
                let slow = CommandOutput {
                    duration: Duration::from_secs(3),
                    ..exit(0)
                };
                let key = CommandCache::key(&run().resolve(&process.env));
                cache.load([(key, slow.clone())]);

                assert_eq!(run().run(&process).unwrap(), slow);
            }

            #[test]
            fn load_should_not_answer_interactive_runs() {
                let dir = TestDir::new();
                let cache = CommandCache::default();
                let process = cached(&dir, &cache);
                let command = shell("touch marker");
                let key = CommandCache::key(&CommandRun::new(&command).resolve(&process.env));
                cache.load([(key, exit(0))]);

                command.interactive().run(&process).unwrap();

                assert!(dir.path().join("marker").exists());
            }
        }

        mod std_fmt_debug {
            use super::*;

            #[test]
            fn fmt_should_show_count() {
                let cache = CommandCache::default();
                cache.load([("key".to_string(), exit(0))]);

                assert_eq!(format!("{cache:?}"), "CommandCache(1)");
            }
        }

        mod process_interceptor {
            use super::*;

            mod handle {
                use super::*;

                #[test]
                fn handle_should_return_cached_output_of_same_command() {
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
                fn handle_should_not_cache_failed_intercepted_sunk_or_interactive_outputs() {
                    // GIVEN a cache after a dry run interceptor
                    let dir = TestDir::new();
                    let cache = CommandCache::default();
                    let process = cached(&dir, &cache);
                    let mut sunk = (Vec::new(), Vec::new());
                    let mut dry = CommandInterceptors::new();
                    dry.push(Inspect(|run: &mut CommandRun| run.set_output(exit(0))));
                    dry.push(cache.clone());

                    // WHEN running a failing, an intercepted, a sunk and an
                    // interactive command
                    let _ = process.run(&shell("exit 1"));
                    shell("exit 0").interceptors(dry).run(&process).unwrap();
                    process
                        .run_to(&shell("echo out"), &mut sink_into(&mut sunk))
                        .unwrap();
                    shell("exit 0").interactive().run(&process).unwrap();

                    // THEN none is cached
                    assert!(cache.entries().is_empty());
                }

                #[test]
                fn handle_should_keep_output_set_before() {
                    let dir = TestDir::new();
                    let cache = CommandCache::default();
                    let process = cached(&dir, &cache);
                    process.run(&shell("exit 0")).unwrap();
                    let mut interceptors = CommandInterceptors::new();
                    interceptors.push(Inspect(|run: &mut CommandRun| run.set_output(exit(7))));
                    interceptors.push(cache.clone());

                    let error = shell("exit 0")
                        .interceptors(interceptors)
                        .run(&process)
                        .unwrap_err();

                    assert_eq!(error.command_output().unwrap().status, Some(7));
                }
            }

            #[cfg(feature = "tokio")]
            mod handle_async {
                use super::*;

                #[test]
                fn handle_async_should_return_cached_output_of_same_command() {
                    let dir = TestDir::new();
                    let cache = CommandCache::default();
                    let process = cached(&dir, &cache);
                    let first = block_on(process.run_async(&shell("echo out"))).unwrap();

                    let second = block_on(process.run_async(&shell("echo out"))).unwrap();

                    assert_eq!(second, first);
                    assert_eq!(cache.entries().len(), 1);
                }
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
                        display_lines: 10,
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
                let mut interceptors = CommandInterceptors::new();
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
                let seen = Arc::new(Mutex::new(None));
                let process = process(std::env::temp_dir());
                let found = seen.clone();
                process.add_interceptor(Inspect(move |run: &mut CommandRun| {
                    *found.lock().unwrap() = run.get::<u32>().copied();
                }));

                shell("exit 0").with(7_u32).run(&process).unwrap();

                assert_eq!(*seen.lock().unwrap(), Some(7));
            }
        }
    }

    mod command_run {
        use super::*;

        /// Returns what `inspect` returns for the run of `run` by a process
        /// in the temporary directory, before the run is passed on.
        fn inspected<T: Send + 'static>(
            run: CommandRun,
            inspect: impl Fn(&mut CommandRun) -> T + Send + Sync + 'static,
        ) -> T {
            let seen = Arc::new(Mutex::new(None));
            let process = process(std::env::temp_dir());
            let found = seen.clone();
            process.add_interceptor(Inspect(move |run: &mut CommandRun| {
                *found.lock().unwrap() = Some(inspect(run));
            }));

            let _ = process.run(run);

            seen.lock().unwrap().take().unwrap()
        }

        #[test]
        fn command_run_should_combine_options() {
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
                    interceptors: None,
                    stream: true,
                    timeout: Some(Duration::from_secs(10))
                }
            );
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

        mod args {
            use super::*;

            #[test]
            fn args_should_return_args_of_command() {
                let mut command = Command::new("git");
                command.args(["commit", "-m", "message"]);

                assert_eq!(
                    CommandRun::new(&command).args(),
                    ["commit", "-m", "message"]
                );
            }
        }

        mod args_mut {
            use super::*;

            #[test]
            fn args_mut_should_change_args_of_command() {
                let mut run = CommandRun::new(&Command::new("git"));

                run.args_mut().push("status".into());

                assert_eq!(run.args(), ["status"]);
            }
        }

        mod attempt {
            use super::*;

            #[test]
            fn attempt_should_count_attempts_so_far() {
                let attempts = Arc::new(Mutex::new(Vec::new()));
                let seen = attempts.clone();
                let mut interceptors = CommandInterceptors::new();
                interceptors.push(CommandRetry::new(2, Duration::ZERO));
                interceptors.push(Inspect(move |run: &mut CommandRun| {
                    seen.lock().unwrap().push(run.attempt());
                }));

                let _ = shell("exit 1")
                    .interceptors(interceptors)
                    .run(&process(std::env::temp_dir()));

                assert_eq!(*attempts.lock().unwrap(), [0, 1, 2]);
            }
        }

        mod current_dir {
            use super::*;

            #[test]
            fn current_dir_should_be_resolved_dir_of_command_when_run() {
                let mut command = shell("exit 0");
                command.current_dir("sub");

                let dir = inspected(CommandRun::new(&command), |run| {
                    run.current_dir().map(Path::to_path_buf)
                });

                assert_eq!(CommandRun::new(&command).current_dir(), None);
                assert_eq!(dir, Some(std::env::temp_dir().join("sub")));
            }
        }

        mod envs {
            use super::*;

            #[test]
            fn envs_should_return_vars_set_and_removed_by_command() {
                let mut command = Command::new("git");
                command.env("SET", "1").env_remove("REMOVED");

                assert_eq!(
                    CommandRun::new(&command).envs(),
                    [
                        (OsString::from("REMOVED"), None),
                        (OsString::from("SET"), Some(OsString::from("1")))
                    ]
                );
            }
        }

        mod has_sink {
            use super::*;

            #[test]
            fn has_sink_should_tell_whether_output_goes_to_sink() {
                let seen = Arc::new(Mutex::new(Vec::new()));
                let process = process(std::env::temp_dir());
                let found = seen.clone();
                process.add_interceptor(Inspect(move |run: &mut CommandRun| {
                    found.lock().unwrap().push(run.has_sink());
                }));

                process.run(&shell("echo out")).unwrap();
                process
                    .run_to(&shell("echo out"), &mut |_, _| Ok(()))
                    .unwrap();

                assert_eq!(*seen.lock().unwrap(), [false, true]);
            }
        }

        mod id {
            use super::*;

            fn ids_of_two_runs(run: impl Fn(&ProcessImpl, CommandInterceptors)) -> Vec<u64> {
                let ids = Arc::new(Mutex::new(Vec::new()));
                let seen = ids.clone();
                let mut interceptors = CommandInterceptors::new();
                interceptors.push(CommandRetry::new(1, Duration::ZERO));
                interceptors.push(Inspect(move |run: &mut CommandRun| {
                    seen.lock().unwrap().push(run.id());
                }));
                let process = process(std::env::temp_dir());

                run(&process, interceptors.clone());
                run(&process, interceptors);

                ids.lock().unwrap().clone()
            }

            #[test]
            fn id_should_be_zero_before_run_starts() {
                assert_eq!(CommandRun::new(&shell("exit 1")).id(), 0);
            }

            #[test]
            fn id_should_be_same_for_attempts_and_unique_per_run() {
                let ids = ids_of_two_runs(|process, interceptors| {
                    let _ = shell("exit 1").interceptors(interceptors).run(process);
                });

                assert_ne!(ids[0], 0);
                assert_eq!(ids[..2], [ids[0]; 2]);
                assert_eq!(ids[2..], [ids[2]; 2]);
                assert_ne!(ids[0], ids[2]);
            }

            #[cfg(feature = "tokio")]
            #[test]
            fn id_async_should_be_same_for_attempts_and_unique_per_run() {
                let ids = ids_of_two_runs(|process, interceptors| {
                    let _ = block_on(
                        shell("exit 1")
                            .interceptors(interceptors)
                            .run_async(process),
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
                let mut run = CommandRun::new(&Command::new("git")).with(1_u32).with("a");

                run.insert(2_u32);

                assert_eq!(run.get::<u32>(), Some(&2));
                assert_eq!(run.get::<&str>(), Some(&"a"));
                assert_eq!(run.get::<u8>(), None);
            }
        }

        mod intercepted_by {
            use super::*;

            #[test]
            fn intercepted_by_should_name_interceptor_setting_output() {
                // GIVEN a dry run inside an interceptor seeing its result
                let seen = Arc::new(Mutex::new(Vec::new()));

                struct Seeing(Arc<Mutex<Vec<(bool, bool, bool)>>>);

                impl ProcessInterceptor for Seeing {
                    fn handle(&self, mut next: CommandNext) -> Result<CommandOutput> {
                        let result = next.next();
                        let run = next.run();
                        self.0.lock().unwrap().push((
                            run.intercepted(),
                            run.intercepted_by::<CommandDryRun>(),
                            run.intercepted_by::<Seeing>(),
                        ));
                        result
                    }
                }

                let process = process(std::env::temp_dir());
                process.add_interceptor(Seeing(seen.clone()));
                let mut dry = process.interceptors();
                dry.push(CommandDryRun);

                // WHEN running a command for real and in the dry run
                process.run(&shell("exit 0")).unwrap();
                shell("exit 0").interceptors(dry).run(&process).unwrap();

                // THEN only the dry run is intercepted, by the dry run
                assert_eq!(
                    *seen.lock().unwrap(),
                    [(false, false, false), (true, true, false)]
                );
            }
        }

        mod new {
            use super::*;

            #[test]
            fn new_should_use_default_options() {
                let (process, recorder) = observed();

                CommandRun::new(&shell("exit 0")).run(&process).unwrap();

                assert_eq!(recorder.options(), CommandOptions::default());
            }
        }

        mod options_mut {
            use super::*;

            #[test]
            fn options_mut_should_change_options_of_run() {
                let mut run = CommandRun::new(&Command::new("git"));

                run.options_mut().silent = true;

                assert!(run.options().silent);
            }
        }

        mod output {
            use super::*;

            #[test]
            fn output_should_return_output_set() {
                let mut run = CommandRun::new(&Command::new("git"));
                assert_eq!(run.output(), None);

                run.set_output(exit(0));

                assert_eq!(run.output(), Some(&exit(0)));
                assert!(run.intercepted());
            }
        }

        mod sunk {
            use super::*;

            #[test]
            fn sunk_should_tell_whether_sink_accepted_output() {
                struct Sunk(Arc<Mutex<Vec<bool>>>);

                impl ProcessInterceptor for Sunk {
                    fn handle(&self, mut next: CommandNext) -> Result<CommandOutput> {
                        let result = next.next();
                        self.0.lock().unwrap().push(next.run().sunk());
                        result
                    }
                }

                let seen = Arc::new(Mutex::new(Vec::new()));
                let process = process(std::env::temp_dir());
                process.add_interceptor(Sunk(seen.clone()));

                let mut sink = |_: OutputStream, _: &[u8]| Ok(());
                process.run_to(&shell("exit 0"), &mut sink).unwrap();
                process.run_to(&shell("echo out"), &mut sink).unwrap();

                assert_eq!(*seen.lock().unwrap(), [false, true]);
            }
        }

        mod remove_env {
            use super::*;

            #[test]
            fn remove_env_should_remove_var_for_command() {
                let process = process(std::env::temp_dir());
                process.add_interceptor(Inspect(|run: &mut CommandRun| {
                    run.remove_env("AGPLATFORM_REMOVED");
                }));

                let output = process
                    .run(print_var("AGPLATFORM_REMOVED").env("AGPLATFORM_REMOVED", "1"))
                    .unwrap();

                assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "unset");
            }
        }

        mod set_env {
            use super::*;

            #[test]
            fn set_env_should_replace_var_of_command() {
                let mut run = CommandRun::new(Command::new("git").env("A", "1").env("B", "1"));

                run.set_env("A", "2");

                assert_eq!(
                    run.envs(),
                    [
                        (OsString::from("B"), Some(OsString::from("1"))),
                        (OsString::from("A"), Some(OsString::from("2")))
                    ]
                );
            }
        }

        mod set_output {
            use super::*;

            #[test]
            fn set_output_should_return_output_instead_of_running() {
                // GIVEN an interceptor setting the output of a streamed run
                let dir = TestDir::new();
                let (process, recorder) = observed();
                process.set_current_dir(dir.path().to_path_buf());
                process.add_interceptor(Inspect(|run: &mut CommandRun| {
                    run.set_output(CommandOutput {
                        stdout: b"set".to_vec(),
                        ..exit(1)
                    });
                }));

                // WHEN running a command that would create a marker
                let error = shell("touch marker").stream().run(&process).unwrap_err();

                // THEN the output set is returned as if it ran
                assert_eq!(error.command_output().unwrap().stdout, b"set");
                assert_eq!(recorder.stdout(), b"set");
                assert!(!dir.path().join("marker").exists());
            }
        }

        mod set_program {
            use super::*;

            #[test]
            fn set_program_should_change_program_of_command() {
                let mut run = CommandRun::new(&Command::new("git"));

                run.set_program("hg");

                assert_eq!(run.program(), "hg");
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
                let command = shell("exit 0");

                process
                    .run(CommandRun::with_options(&command, options.clone()))
                    .unwrap();

                assert_eq!(recorder.options(), options);
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

                    assert_eq!(recorder.options(), CommandOptions::default());
                    assert_eq!(recorder.events().len(), 4);
                }
            }
        }

        mod program {
            use super::*;

            #[test]
            fn program_should_return_program_without_args() {
                let mut command = Command::new("git");
                command.args(["clone", "https://user:token@example.com/repo"]);

                assert_eq!(CommandRun::new(&command).program(), "git");
            }
        }

        mod command_line {
            use super::*;

            #[test]
            fn command_line_should_join_program_and_args() {
                let mut command = Command::new("git");
                command.args(["commit", "-m", "message"]);

                assert_eq!(
                    CommandRun::new(&command).command_line(),
                    "git commit -m message"
                );
            }
        }
    }

    mod command_output {
        use super::*;

        mod default {
            use super::*;

            #[test]
            fn default_should_be_empty_failure() {
                let output = CommandOutput::default();

                assert!(!output.success);
                assert_eq!(output.status, None);
                assert_eq!(output.display_lines, 10);
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
                    status: Some(0),
                    stdout: b"out".to_vec(),
                    ..Default::default()
                };

                assert_eq!(
                    output,
                    CommandOutput {
                        success: true,
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
                    "status: none\nduration: 0ns\nstdout:\n(2 lines omitted)\n3\n4\n5\n6\n7\n8\n9\n10\n11\n12\n\nstderr:\n"
                );
            }

            #[test]
            fn fmt_without_status_should_show_none() {
                let output = CommandOutput::default();

                assert_eq!(
                    output.to_string(),
                    "status: none\nduration: 0ns\nstdout:\n\nstderr:\n"
                );
            }

            #[test]
            fn fmt_with_signal_should_show_signal() {
                let output = CommandOutput {
                    signal: Some(9),
                    ..Default::default()
                };

                assert_eq!(
                    output.to_string(),
                    "status: signal 9\nduration: 0ns\nstdout:\n\nstderr:\n"
                );
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
                        .run(CommandRun::new(&shell("wc -c")).input(input))
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
                        interceptors: None,
                        stream: true,
                        timeout: None,
                    };

                    let output = process
                        .run(CommandRun::with_options(
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
                        .run(CommandRun::with_options(
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
                        .run(CommandRun::with_options(
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
                fn run_unchanged_vars_should_inherit_environment() {
                    let process = ProcessImpl::new(std::env::temp_dir());

                    let output = process.run(&print_var("PATH")).unwrap();

                    assert_ne!(output.stdout_str().unwrap().trim(), "unset");
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
                        .run(CommandRun::with_options(
                            &print_var("AGPLATFORM_CONFIGURED"),
                            options.clone(),
                        ))
                        .unwrap();
                    let own = process
                        .run(CommandRun::with_options(
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
                        .run(CommandRun::with_options(&echo_stdin(), options.clone()))
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
                        .run(CommandRun::with_options(&echo_stdin(), options.clone()))
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
                        .run(CommandRun::with_options(&shell("exit 0"), options.clone()))
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
                        .run(CommandRun::with_options(&shell("exit 3"), options.clone()))
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
                        .run(CommandRun::with_options(&shell("exit 0"), options.clone()))
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
                        interceptors: None,
                        stream: false,
                        timeout: Some(Duration::from_secs(10)),
                    };

                    let output = process
                        .run(CommandRun::with_options(
                            &shell("echo out"),
                            options.clone(),
                        ))
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
                            .run(CommandRun::with_options(&command, options.clone()))
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
                        interceptors: None,
                        stream: false,
                        timeout: Some(Duration::from_millis(200)),
                    };

                    // WHEN it exceeds the timeout
                    let error = process
                        .run(CommandRun::with_options(
                            &shell(SLEEP_THEN_ECHO),
                            options.clone(),
                        ))
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

                    let output = block_on(process.run_async(CommandRun::with_options(
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

                    let output = block_on(process.run_async(CommandRun::with_options(
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

                    let output = block_on(
                        process.run_async(CommandRun::with_options(&echo_stdin(), options.clone())),
                    )
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
                    let output = block_on(
                        process.run_async(CommandRun::with_options(&echo_stdin(), options.clone())),
                    )
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
                        process
                            .run_async(CommandRun::with_options(&shell("exit 0"), options.clone())),
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
                        process
                            .run_async(CommandRun::with_options(&shell("exit 3"), options.clone())),
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
                        process
                            .run_async(CommandRun::with_options(&shell("exit 0"), options.clone())),
                    )
                    .unwrap_err();

                    assert!(
                        error
                            .description()
                            .ends_with("': interactive cannot be combined with timeout")
                    );
                    assert_eq!(recorder.events(), [format!("invalid {error}")]);
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

                    let output = block_on(process.run_async(CommandRun::with_options(
                        &shell("echo out"),
                        options.clone(),
                    )))
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
                        let output = block_on(
                            process.run_async(CommandRun::with_options(&command, options.clone())),
                        )
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
                    let error = block_on(process.run_async(CommandRun::with_options(
                        &shell(ECHO_THEN_SLEEP),
                        options.clone(),
                    )))
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

                let error = failure(&CommandRun::new(&Command::new("git")), output.clone());

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

    mod deliver {
        use super::*;

        /// Returns the result of delivering `output` for `run` and the
        /// chunks passed on.
        fn delivered(
            run: &CommandRun,
            output: CommandOutput,
            fail: bool,
        ) -> (Result<CommandOutput>, Vec<(OutputStream, Vec<u8>)>) {
            let mut chunks = Vec::new();
            let outputs = Outputs::new(true, |stream, data| {
                chunks.push((stream, data.to_vec()));

                if fail {
                    Err(crate::Error::process("full"))
                } else {
                    Ok(())
                }
            });
            let result = deliver(run, output, outputs);
            (result, chunks)
        }

        #[test]
        fn deliver_should_pass_output_on_and_capture_it() {
            let run = CommandRun::new(&Command::new("git")).display_lines(3);
            let output = CommandOutput {
                stdout: b"out".to_vec(),
                stderr: b"err".to_vec(),
                ..exit(0)
            };

            let (result, chunks) = delivered(&run, output.clone(), false);

            let delivered = result.unwrap();
            assert_eq!(delivered, output);
            assert_eq!(delivered.display_lines, 3);
            assert_eq!(
                chunks,
                [
                    (OutputStream::Stdout, b"out".to_vec()),
                    (OutputStream::Stderr, b"err".to_vec())
                ]
            );
        }

        #[test]
        fn deliver_silent_or_interactive_should_drop_output() {
            for run in [
                CommandRun::new(&Command::new("git")).silent(),
                CommandRun::new(&Command::new("git")).interactive(),
            ] {
                let output = CommandOutput {
                    stdout: b"out".to_vec(),
                    ..exit(0)
                };

                let (result, chunks) = delivered(&run, output, false);

                assert!(result.unwrap().stdout.is_empty());
                assert!(chunks.is_empty());
            }
        }

        #[test]
        fn deliver_failing_to_pass_on_should_stop_with_error() {
            let output = CommandOutput {
                stdout: b"out".to_vec(),
                stderr: b"err".to_vec(),
                ..exit(0)
            };

            let (result, chunks) = delivered(&CommandRun::new(&Command::new("git")), output, true);

            assert_eq!(result.unwrap_err().description(), "full");
            assert_eq!(chunks.len(), 1);
        }

        #[test]
        fn deliver_longer_than_timeout_should_keep_output() {
            let run = CommandRun::new(&Command::new("git")).timeout(Duration::from_secs(1));
            let output = CommandOutput {
                stderr: b"err".to_vec(),
                duration: Duration::from_secs(2),
                ..exit(0)
            };

            let (result, _) = delivered(&run, output.clone(), false);

            assert_eq!(result.unwrap(), output);
        }
    }

    mod outputs {
        use super::*;

        mod push {
            use super::*;

            #[test]
            fn push_should_pass_chunks_on_and_capture_them() {
                let mut chunks = Vec::new();
                let mut outputs = Outputs::new(true, |stream, data| {
                    chunks.push((stream, data.to_vec()));
                    Ok(())
                });

                outputs.push(OutputStream::Stdout, b"out");
                outputs.push(OutputStream::Stderr, b"err");

                assert_eq!(
                    outputs.finish().unwrap(),
                    (b"out".to_vec(), b"err".to_vec())
                );
                assert_eq!(chunks.len(), 2);
            }

            #[test]
            fn push_not_capturing_should_only_pass_chunks_on() {
                let mut chunks = 0;
                let mut outputs = Outputs::new(false, |_, _| {
                    chunks += 1;
                    Ok(())
                });

                outputs.push(OutputStream::Stdout, b"out");
                outputs.push(OutputStream::Stderr, b"err");

                assert_eq!(outputs.finish().unwrap(), (Vec::new(), Vec::new()));
                assert_eq!(chunks, 2);
            }

            #[test]
            fn push_failing_to_pass_on_should_fail_outputs() {
                let mut outputs = Outputs::new(true, |_, _| Err(crate::Error::process("full")));

                outputs.push(OutputStream::Stdout, b"out");

                assert!(outputs.failed());
                assert_eq!(outputs.finish().unwrap_err().description(), "full");
            }
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
}
