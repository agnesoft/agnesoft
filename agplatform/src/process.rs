use std::borrow::Cow;
#[cfg(unix)]
use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use crate::Result;
use crate::platform::platform_env::CurrentDirSetter;
use crate::platform::platform_env::EnvVarSetter;
use crate::utils::Tail;

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

/// The output of a child process a chunk passed to a sink is from, see
/// [`Process::run_to`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputStream {
    /// The standard output.
    Stdout,
    /// The standard error.
    Stderr,
}

/// [internal]
///
/// The output of an attempt of `run` as it arrives: captured unless passed
/// to the `sink`, and reported to the observer of a streamed run. The first
/// error of the sink ends the attempt.
pub(crate) struct Outputs<'a, 'b, 'c> {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    observer: Option<&'a dyn ProcessObserver>,
    run: &'a CommandRun<'b>,
    sink: Option<&'a mut OutputSink<'c>>,
    sink_error: Option<crate::Error>,
}

/// Observes the runs of commands by a [`Process`], e.g. to log them.
///
/// Every run is reported once to [`started`](Self::started) and
/// [`finished`](Self::finished), including the runs that fail to start.
/// An async run cancelled by dropping its future is finished with an error
/// whose source is `CommandCancelled` (with the `tokio` feature). The end
/// of a run that panics is not reported. Each
/// failed attempt that is retried (see [`CommandOptions::retries`]) is reported
/// to [`retrying`](Self::retrying), the attempt being available from
/// [`CommandRun::attempt`]. The output of each attempt of a run with
/// [`CommandOptions::stream`] is reported to [`stdout`](Self::stdout) and
/// [`stderr`](Self::stderr) as it arrives.
///
/// Installed with [`Process::set_observer`], e.g. on
/// [`Platform::process_mut`](crate::Platform::process_mut).
///
/// # Examples
///
/// ```rust
/// use agplatform::{CommandExt, CommandOutput, Process, ProcessObserver, Result, CommandRun};
/// use std::process::Command;
/// use std::sync::{Arc, Mutex};
///
/// #[derive(Default)]
/// struct Failures(Mutex<Vec<String>>);
///
/// impl ProcessObserver for Failures {
///     fn finished(&self, run: &CommandRun, result: &Result<CommandOutput>) {
///         if let Err(error) = result {
///             self.0.lock().unwrap().push(error.to_string());
///         }
///     }
/// }
///
/// let failures = Arc::new(Failures::default());
/// let mut process = agplatform::TestProcess::new();
/// process.set_observer(failures.clone());
/// process.set_output("git", CommandOutput { status: Some(1), ..Default::default() });
///
/// let _ = Command::new("git").arg("push").run(&process);
///
/// assert_eq!(*failures.0.lock().unwrap(), ["[Process] 'git' failed with exit code 1"]);
/// ```
#[allow(unused_variables)]
pub trait ProcessObserver: Send + Sync {
    /// Called before the command is run.
    fn started(&self, run: &CommandRun) {}

    /// Called with each chunk of the standard output of a streamed run as
    /// it arrives, as read rather than split into lines. Writes it to the
    /// standard output of the current process by default.
    fn stdout(&self, run: &CommandRun, data: &[u8]) {
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(data);
        let _ = stdout.flush();
    }

    /// Called with each chunk of the standard error of a streamed run as it
    /// arrives, as read rather than split into lines. Writes it to the
    /// standard error of the current process by default.
    fn stderr(&self, run: &CommandRun, data: &[u8]) {
        let mut stderr = std::io::stderr().lock();
        let _ = stderr.write_all(data);
        let _ = stderr.flush();
    }

    /// Called with the failed `result` of an attempt before the command is
    /// run again after [`CommandOptions::retry_delay`].
    fn retrying(&self, run: &CommandRun, result: &Result<CommandOutput>) {}

    /// Called with the `result` of the last attempt of the run, which is
    /// also returned by [`Process::run`].
    fn finished(&self, run: &CommandRun, result: &Result<CommandOutput>) {}
}

/// [internal]
///
/// The [`ProcessObserver`] used when none is set, which only streams the
/// output.
pub(crate) struct StdObserver;

/// Abstract process interface.
///
/// Runs child processes described by [`std::process::Command`] to
/// completion. Child processes run in the configured current directory
/// unless the command sets an absolute one, a relative one is resolved
/// against it. They get the configured environment variables, updated by
/// the variables the command sets or removes.
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
    /// hold secrets. A [`ProcessObserver`] gets the whole command, e.g. to
    /// log it redacted.
    ///
    /// Returns an error if:
    /// - [`CommandOptions::interactive`] is combined with
    ///   [`CommandOptions::input`] or [`CommandOptions::timeout`].
    /// - The process cannot be started.
    /// - The process exits with a non-zero code, is terminated without an
    ///   exit code or times out. Its [`CommandOutput`] is returned by
    ///   [`Error::command_output`](crate::Error::command_output).
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
    /// let options = CommandOptions { retries: 3, ..Default::default() };
    /// process.run(CommandRun::with_options(&fetch, &options)).unwrap();
    /// ```
    fn run<'a>(&self, run: impl Into<CommandRun<'a>>) -> Result<CommandOutput>;

    /// Runs `run` like [`Self::run`], passing the standard output and error
    /// to `sink` as they arrive, each chunk with the [`OutputStream`] it is
    /// from, instead of capturing them, e.g. for output too large to hold
    /// in memory. The [`CommandOutput`] then has no output, nor does the
    /// error of a failed run show any. A run with [`CommandOptions::stream`]
    /// still reports the output to the [`ProcessObserver`].
    ///
    /// The `sink` gets the output whether the run succeeds or not, which is
    /// known only once it ends. The chunks are as read, not split into
    /// lines. An attempt failing after `sink` accepted a chunk is not
    /// retried, even when [`CommandOptions::retry_if`] accepts the failure,
    /// as `sink` would get the output again from its start.
    ///
    /// # Errors
    ///
    /// As [`Self::run`], or the error returned by `sink`, which ends the
    /// attempt, killing the child process like [`CommandOptions::timeout`].
    /// It is retried only if `sink` accepted no chunk before and
    /// [`CommandOptions::retry_if`] accepts it. Fails as well if the run
    /// is [`CommandOptions::interactive`] or [`CommandOptions::silent`],
    /// which leave no output to pass.
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
    fn run_to<'a>(
        &self,
        run: impl Into<CommandRun<'a>>,
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
    /// killed. The run is reported to [`ProcessObserver::finished`] as
    /// failed with [`CommandCancelled`].
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
    /// use agplatform::{CommandExt, CommandOutput, Platform, Process};
    /// use std::process::Command;
    /// use std::time::Duration;
    ///
    /// # tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
    /// let platform = agplatform::platform();
    /// let process = platform.process();
    /// # let process = agplatform::TestProcess::new();
    /// # process.set_output("git", CommandOutput { status: Some(0), stdout: b"git version 2.0".to_vec(), ..Default::default() });
    ///
    /// let output = process.run_async(Command::new("git").arg("--version")).await.unwrap();
    /// assert!(output.stdout_str().unwrap().starts_with("git version"));
    ///
    /// let mut fetch = Command::new("git");
    /// fetch.arg("fetch");
    /// process.run_async(fetch.retry(3, Duration::from_secs(1))).await.unwrap();
    /// # });
    /// ```
    #[cfg(feature = "tokio")]
    fn run_async<'a>(
        &self,
        run: impl Into<CommandRun<'a>>,
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
    fn run_async_to<'a>(
        &self,
        run: impl Into<CommandRun<'a>>,
        sink: &mut (dyn FnMut(OutputStream, &[u8]) -> Result + Send),
    ) -> impl Future<Output = Result<CommandOutput>> + Send;

    /// Sets the observer of the runs, replacing the previous one.
    ///
    /// See [`ProcessObserver`].
    fn set_observer(&mut self, observer: Arc<dyn ProcessObserver>);
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
    fn clear_env(&self) -> CommandRun<'_>;

    /// Returns a [`CommandRun`] of the command whose output shows its last
    /// `lines` when displayed.
    ///
    /// See [`CommandOptions::display_lines`].
    fn display_lines(&self, lines: usize) -> CommandRun<'_>;

    /// Returns a [`CommandRun`] of the command whose output is discarded.
    ///
    /// See [`CommandOptions::silent`].
    fn silent(&self) -> CommandRun<'_>;

    /// Returns a [`CommandRun`] of the command reading `input`.
    ///
    /// See [`CommandOptions::input`].
    fn input<T: Into<Vec<u8>>>(&self, input: T) -> CommandRun<'_>;

    /// Returns a [`CommandRun`] of the command connected to the terminal.
    ///
    /// See [`CommandOptions::interactive`].
    fn interactive(&self) -> CommandRun<'_>;

    /// Returns a [`CommandRun`] of the command that streams its output.
    ///
    /// See [`CommandOptions::stream`].
    fn stream(&self) -> CommandRun<'_>;

    /// Returns a [`CommandRun`] of the command retried up to `retries` times after
    /// `delay` while it fails.
    ///
    /// See [`CommandOptions::retries`].
    fn retry(&self, retries: u32, delay: Duration) -> CommandRun<'_>;

    /// Returns a [`CommandRun`] of the command limited to `timeout`.
    ///
    /// See [`CommandOptions::timeout`].
    fn timeout(&self, timeout: Duration) -> CommandRun<'_>;
}

/// A command to be run with [`CommandOptions`].
///
/// Usually obtained from [`CommandExt`]. Also reported to a
/// [`ProcessObserver`], then with the directory the command runs in.
pub struct CommandRun<'a> {
    command: &'a Command,
    options: Cow<'a, CommandOptions>,
    current_dir: Option<PathBuf>,
    attempt: u32,
    id: u64,
    sink: bool,
    output_sunk: AtomicBool,
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
    /// capturing it, see [`ProcessObserver::stdout`].
    pub stream: bool,

    /// Runs the command again up to this many times while it fails, i.e.
    /// [`Process::run`] would return an error, waiting
    /// [`CommandOptions::retry_delay`] before each retry. Each attempt gets the
    /// [`CommandOptions::input`] and is limited by [`CommandOptions::timeout`] on
    /// its own.
    pub retries: u32,

    /// The delay before each retry, see [`CommandOptions::retries`].
    pub retry_delay: Duration,

    /// Retries only the failed attempts for which the predicate returns
    /// `true`, see [`CommandOptions::retries`]. All are retried when `None`.
    pub retry_if: Option<RetryIf>,

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

/// A predicate deciding whether a failed attempt is retried, see
/// [`CommandOptions::retry_if`].
///
/// Two predicates are equal when they are the same closure.
#[derive(Clone)]
pub struct RetryIf(Arc<RetryPredicate>);

type RetryPredicate = dyn Fn(&CommandRun, &Result<CommandOutput>) -> bool + Send + Sync;

/// The output of a finished child process.
///
/// Returned from [`Process::run`] on success and by
/// [`Error::command_output`](crate::Error::command_output) of its error
/// otherwise.
///
/// Outputs are equal when they differ at most in their
/// [`display_lines`](Self::display_lines), which only affects how they are
/// displayed.
#[derive(Clone, Debug)]
pub struct CommandOutput {
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

/// Enabled by the `tokio` feature flag.
///
/// The source of the error an async run cancelled by dropping its future
/// is reported with to [`ProcessObserver::finished`].
///
/// # Examples
///
/// ```rust
/// use agplatform::{CommandCancelled, CommandOutput, CommandRun, ProcessObserver, Result};
///
/// struct Log;
///
/// impl ProcessObserver for Log {
///     fn finished(&self, run: &CommandRun, result: &Result<CommandOutput>) {
///         match result {
///             Err(error) if error.source_as::<CommandCancelled>().is_some() => {
///                 println!("{} cancelled", run.program())
///             }
///             _ => {}
///         }
///     }
/// }
/// ```
#[cfg(feature = "tokio")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandCancelled;

/// [internal]
///
/// The attempts of an async run, see [`run_attempts_async`].
#[cfg(feature = "tokio")]
pub(crate) trait AsyncAttempts: Send {
    /// Runs the attempt of `run`.
    fn attempt(&mut self, run: &CommandRun) -> impl Future<Output = Result<CommandOutput>> + Send;

    /// Waits `delay` before a retry.
    fn delay(&self, delay: Duration) -> impl Future<Output = ()> + Send;
}

/// [internal]
///
/// The attempts of an async run by `process` passing the output to `sink`,
/// if any.
#[cfg(feature = "tokio")]
pub(crate) struct SinkAttempts<'a, 's, P> {
    pub(crate) process: &'a P,
    pub(crate) sink: Option<&'a mut OutputSink<'s>>,
}

/// [internal]
///
/// A run reported to the observer as started, and as finished once, as
/// cancelled when dropped before, unless panicking.
#[cfg(feature = "tokio")]
struct ObservedRun<'a, 'b> {
    observer: &'a dyn ProcessObserver,
    run: CommandRun<'b>,
    finished: bool,
}

/// [`Process`] implementation backed by [`std::process`].
///
/// Child processes run in the configured current directory with the
/// configured environment variables.
///
/// Normally obtained via [`platform()`](crate::platform) rather than
/// constructed directly.
pub struct ProcessImpl {
    pub(crate) current_dir: PathBuf,
    pub(crate) var_changes: Vec<(String, Option<String>)>,
    observer: Arc<dyn ProcessObserver>,
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

    fn clear_env(&self) -> CommandRun<'_> {
        CommandRun::new(self).clear_env()
    }

    fn display_lines(&self, lines: usize) -> CommandRun<'_> {
        CommandRun::new(self).display_lines(lines)
    }

    fn silent(&self) -> CommandRun<'_> {
        CommandRun::new(self).silent()
    }

    fn input<T: Into<Vec<u8>>>(&self, input: T) -> CommandRun<'_> {
        CommandRun::new(self).input(input)
    }

    fn interactive(&self) -> CommandRun<'_> {
        CommandRun::new(self).interactive()
    }

    fn stream(&self) -> CommandRun<'_> {
        CommandRun::new(self).stream()
    }

    fn retry(&self, retries: u32, delay: Duration) -> CommandRun<'_> {
        CommandRun::new(self).retry(retries, delay)
    }

    fn timeout(&self, timeout: Duration) -> CommandRun<'_> {
        CommandRun::new(self).timeout(timeout)
    }
}

impl<'a> CommandRun<'a> {
    /// Creates a run of `command` with the default [`CommandOptions`].
    pub fn new(command: &'a Command) -> Self {
        Self {
            command,
            options: Cow::Owned(CommandOptions::default()),
            current_dir: None,
            attempt: 1,
            id: 0,
            sink: false,
            output_sunk: AtomicBool::new(false),
        }
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
    /// let options = CommandOptions { retries: 3, ..Default::default() };
    /// let command = Command::new("git");
    /// let run = CommandRun::with_options(&command, &options);
    ///
    /// assert_eq!(run.options().retries, 3);
    /// ```
    pub fn with_options(command: &'a Command, options: &'a CommandOptions) -> Self {
        Self {
            command,
            options: Cow::Borrowed(options),
            current_dir: None,
            attempt: 1,
            id: 0,
            sink: false,
            output_sunk: AtomicBool::new(false),
        }
    }

    /// Returns the attempt of the run, starting at `1`, see
    /// [`CommandOptions::retries`].
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Returns the id of the run, unique within the process among the runs
    /// of commands and requests and the same for all of its attempts, e.g.
    /// to tell concurrent runs apart in a [`ProcessObserver`]. It is
    /// assigned when the run starts, `0` before.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Returns the command.
    pub fn command(&self) -> &'a Command {
        self.command
    }

    /// Returns the program and the arguments of `command` separated by spaces.
    pub fn command_line(&self) -> String {
        std::iter::once(self.command.get_program())
            .chain(self.command.get_args())
            .map(std::ffi::OsStr::to_string_lossy)
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Returns the directory the command runs in, i.e. its own resolved
    /// against the current directory of the process, when reported to a
    /// [`ProcessObserver`]. `None` otherwise.
    pub fn current_dir(&self) -> Option<&Path> {
        self.current_dir.as_deref()
    }

    /// Returns the options.
    pub fn options(&self) -> &CommandOptions {
        &self.options
    }

    /// Returns the program of `command`, lossily converted to UTF-8.
    pub fn program(&self) -> Cow<'_, str> {
        self.command.get_program().to_string_lossy()
    }

    /// Passes only the environment variables set by the command.
    ///
    /// See [`CommandOptions::clear_env`].
    pub fn clear_env(mut self) -> Self {
        self.options.to_mut().clear_env = true;
        self
    }

    /// Shows the last `lines` of the output when displayed.
    ///
    /// See [`CommandOptions::display_lines`].
    pub fn display_lines(mut self, lines: usize) -> Self {
        self.options.to_mut().display_lines = lines;
        self
    }

    /// Discards the output of the command.
    ///
    /// See [`CommandOptions::silent`].
    pub fn silent(mut self) -> Self {
        self.options.to_mut().silent = true;
        self
    }

    /// Writes `input` to the standard input of the command.
    ///
    /// See [`CommandOptions::input`].
    pub fn input<T: Into<Vec<u8>>>(mut self, input: T) -> Self {
        self.options.to_mut().input = Some(input.into());
        self
    }

    /// Connects the command to the terminal.
    ///
    /// See [`CommandOptions::interactive`].
    pub fn interactive(mut self) -> Self {
        self.options.to_mut().interactive = true;
        self
    }

    /// Streams the output of the command.
    ///
    /// See [`CommandOptions::stream`].
    pub fn stream(mut self) -> Self {
        self.options.to_mut().stream = true;
        self
    }

    /// Retries the command up to `retries` times after `delay` while it
    /// fails.
    ///
    /// See [`CommandOptions::retries`].
    pub fn retry(mut self, retries: u32, delay: Duration) -> Self {
        let options = self.options.to_mut();
        options.retries = retries;
        options.retry_delay = delay;
        self
    }

    /// Retries only the failed attempts for which `predicate` returns
    /// `true`, see [`Self::retry`].
    ///
    /// See [`CommandOptions::retry_if`].
    pub fn retry_if<F>(mut self, predicate: F) -> Self
    where
        F: Fn(&CommandRun, &Result<CommandOutput>) -> bool + Send + Sync + 'static,
    {
        self.options.to_mut().retry_if = Some(RetryIf::new(predicate));
        self
    }

    /// Limits the command to `timeout`.
    ///
    /// See [`CommandOptions::timeout`].
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.options.to_mut().timeout = Some(timeout);
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
    /// Returns the run to be reported to a [`ProcessObserver`], in the
    /// directory of its command resolved against `current_dir`.
    pub(crate) fn resolve(mut self, current_dir: &Path) -> Self {
        self.current_dir = Some(
            self.command
                .get_current_dir()
                .map_or(current_dir.to_path_buf(), |dir| current_dir.join(dir)),
        );
        self
    }
}

impl<'a> From<&'a Command> for CommandRun<'a> {
    fn from(command: &'a Command) -> Self {
        Self::new(command)
    }
}

impl<'a> From<&'a mut Command> for CommandRun<'a> {
    fn from(command: &'a mut Command) -> Self {
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
            retries: 0,
            retry_delay: Duration::ZERO,
            retry_if: None,
            silent: false,
            stream: false,
            timeout: None,
        }
    }
}

impl RetryIf {
    /// Creates a predicate deciding whether a failed attempt is retried.
    pub fn new<F>(predicate: F) -> Self
    where
        F: Fn(&CommandRun, &Result<CommandOutput>) -> bool + Send + Sync + 'static,
    {
        Self(Arc::new(predicate))
    }
}

impl std::fmt::Debug for RetryIf {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "RetryIf")
    }
}

impl PartialEq for RetryIf {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for RetryIf {}

impl CommandOutput {
    /// Returns `true` if the process exited with code `0` in time.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::CommandOutput;
    ///
    /// assert!(CommandOutput { status: Some(0), ..Default::default() }.success());
    /// assert!(!CommandOutput { status: Some(1), ..Default::default() }.success());
    /// ```
    pub fn success(&self) -> bool {
        self.status == Some(0) && !self.timed_out
    }

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

#[cfg(feature = "tokio")]
impl std::fmt::Display for CommandCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "cancelled")
    }
}

#[cfg(feature = "tokio")]
impl std::error::Error for CommandCancelled {}

impl ProcessImpl {
    /// Creates a new `ProcessImpl` running child processes in the given
    /// directory.
    ///
    /// Child processes inherit the environment of the current process with
    /// the variables set or removed since applied on top.
    pub fn new(current_dir: PathBuf) -> Self {
        ProcessImpl {
            current_dir,
            var_changes: Vec::new(),
            observer: Arc::new(StdObserver),
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
        let (command, options) = (run.command, run.options());
        let dir = run.current_dir().unwrap_or(&self.current_dir);
        #[cfg(unix)]
        let program = self.program(run, dir)?;
        #[cfg(windows)]
        let program = self.program(run, dir);
        let mut child = Command::new(program.as_os_str());
        child.args(command.get_args()).current_dir(dir);

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
            for (key, value) in &self.var_changes {
                match value {
                    Some(value) => child.env(key, value),
                    None => child.env_remove(key),
                };
            }
        }

        for (key, value) in command.get_envs() {
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
    fn program<'a>(&self, run: &CommandRun<'a>, dir: &Path) -> std::io::Result<Cow<'a, Path>> {
        let program = Path::new(run.command.get_program());

        if program
            .as_os_str()
            .as_encoded_bytes()
            .iter()
            .any(|byte| std::path::is_separator(char::from(*byte)))
        {
            Ok(Cow::Owned(dir.join(program)))
        } else {
            self.search_path(run, program, dir)
        }
    }

    /// Returns the program of `run` as the child process running in `dir`
    /// finds it.
    ///
    /// WHY: std resolves a relative path with a separator against the
    /// current directory of the current process on Windows. A bare name is
    /// looked up by std from the child's `PATH`.
    #[cfg(windows)]
    fn program<'a>(&self, run: &CommandRun<'a>, dir: &Path) -> Cow<'a, Path> {
        let program = Path::new(run.command.get_program());

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
    ) -> std::io::Result<Cow<'a, Path>> {
        let inherited = std::env::var_os("PATH");
        let command_path = run
            .command
            .get_envs()
            .find(|(key, _)| *key == "PATH")
            .map(|(_, value)| value);
        let configured_path = self
            .var_changes
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

    /// Runs the attempt of `run`, passing the output to `sink`, if any.
    fn attempt(&self, run: &CommandRun, sink: Option<&mut OutputSink>) -> Result<CommandOutput> {
        let mut outputs = Outputs::new(self.observer.as_ref(), run, sink);
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

    /// Runs the attempt of `run` like [`Self::attempt`] asynchronously.
    #[cfg(feature = "tokio")]
    async fn attempt_async(
        &self,
        run: &CommandRun<'_>,
        sink: Option<&mut OutputSink<'_>>,
    ) -> Result<CommandOutput> {
        let outputs = std::sync::Mutex::new(Outputs::new(self.observer.as_ref(), run, sink));
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
            outputs
                .into_inner()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }
}

impl CurrentDirSetter for ProcessImpl {
    fn set_current_dir(&mut self, path: PathBuf) {
        self.current_dir = path;
    }
}

impl EnvVarSetter for ProcessImpl {
    fn set_var(&mut self, key: String, value: String) {
        change_var(&mut self.var_changes, key, Some(value));
    }

    fn remove_var(&mut self, key: &str) {
        change_var(&mut self.var_changes, key.to_string(), None);
    }
}

impl Process for ProcessImpl {
    fn run<'a>(&self, run: impl Into<CommandRun<'a>>) -> Result<CommandOutput> {
        run_attempts(
            self.observer.as_ref(),
            run.into().resolve(&self.current_dir),
            &mut |run| self.attempt(run, None),
            std::thread::sleep,
        )
    }

    fn run_to<'a>(
        &self,
        run: impl Into<CommandRun<'a>>,
        sink: &mut (dyn FnMut(OutputStream, &[u8]) -> Result + Send),
    ) -> Result<CommandOutput> {
        run_attempts(
            self.observer.as_ref(),
            run.into().resolve(&self.current_dir).with_sink(),
            &mut |run| self.attempt(run, Some(&mut *sink)),
            std::thread::sleep,
        )
    }

    #[cfg(feature = "tokio")]
    fn run_async<'a>(
        &self,
        run: impl Into<CommandRun<'a>>,
    ) -> impl Future<Output = Result<CommandOutput>> + Send {
        let run = run.into().resolve(&self.current_dir);

        async move {
            run_attempts_async(
                self.observer.as_ref(),
                run,
                &mut SinkAttempts {
                    process: self,
                    sink: None,
                },
            )
            .await
        }
    }

    #[cfg(feature = "tokio")]
    fn run_async_to<'a>(
        &self,
        run: impl Into<CommandRun<'a>>,
        sink: &mut (dyn FnMut(OutputStream, &[u8]) -> Result + Send),
    ) -> impl Future<Output = Result<CommandOutput>> + Send {
        let run = run.into().resolve(&self.current_dir).with_sink();

        async move {
            run_attempts_async(
                self.observer.as_ref(),
                run,
                &mut SinkAttempts {
                    process: self,
                    sink: Some(sink),
                },
            )
            .await
        }
    }

    fn set_observer(&mut self, observer: Arc<dyn ProcessObserver>) {
        self.observer = observer;
    }
}

#[cfg(feature = "tokio")]
impl AsyncAttempts for SinkAttempts<'_, '_, ProcessImpl> {
    fn attempt(&mut self, run: &CommandRun) -> impl Future<Output = Result<CommandOutput>> + Send {
        self.process.attempt_async(run, self.sink.as_deref_mut())
    }

    fn delay(&self, delay: Duration) -> impl Future<Output = ()> + Send {
        tokio::time::sleep(delay)
    }
}

#[cfg(feature = "tokio")]
impl<'a, 'b> ObservedRun<'a, 'b> {
    fn new(observer: &'a dyn ProcessObserver, run: CommandRun<'b>) -> Self {
        observer.started(&run);

        Self {
            observer,
            run,
            finished: false,
        }
    }

    fn finish(mut self, result: Result<CommandOutput>) -> Result<CommandOutput> {
        self.finished = true;
        self.observer.finished(&self.run, &result);
        result
    }
}

#[cfg(feature = "tokio")]
impl Drop for ObservedRun<'_, '_> {
    fn drop(&mut self) {
        if !self.finished && !std::thread::panicking() {
            let cancelled = Err(
                process_error!("'{}' {CommandCancelled}", self.run.program())
                    .with_source(CommandCancelled),
            );
            self.observer.finished(&self.run, &cancelled);
        }
    }
}

impl ProcessObserver for StdObserver {}

/// [internal]
///
/// Returns `Ok(output)` if the process succeeded, otherwise an error with
/// the `output` as its source.
pub(crate) fn check(program: &str, output: CommandOutput) -> Result<CommandOutput> {
    match (output.timed_out, output.status) {
        (false, Some(0)) => Ok(output),
        (true, _) => Err(
            process_error!("'{program}' timed out after {:?}", output.duration).with_source(output),
        ),
        (false, Some(code)) => {
            Err(process_error!("'{program}' failed with exit code {code}").with_source(output))
        }
        (false, None) => match output.signal {
            Some(signal) => Err(
                process_error!("'{program}' was terminated by signal {signal}").with_source(output),
            ),
            None => Err(
                process_error!("'{program}' was terminated without an exit code")
                    .with_source(output),
            ),
        },
    }
}

/// [internal]
///
/// Returns the error of `program` failing to be started or waited for with
/// the I/O `error` as its source.
pub(crate) fn io_error(program: &str, error: std::io::Error) -> crate::Error {
    process_error!("'{program}': {error}").with_source(error)
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
/// Validates the options of `run` and runs its attempts with `attempt`
/// until one succeeds or no retries are left, waiting with `sleep` between
/// them, and reports them to `observer`.
pub(crate) fn run_attempts(
    observer: &dyn ProcessObserver,
    mut run: CommandRun,
    attempt: &mut dyn FnMut(&CommandRun) -> Result<CommandOutput>,
    sleep: fn(Duration),
) -> Result<CommandOutput> {
    run.id = crate::utils::next_run_id();
    observer.started(&run);

    let result = validate(&run.program(), run.options(), run.sink).and_then(|()| {
        loop {
            let result = attempt(&run);

            if !retry(&run, &result) {
                break result;
            }

            observer.retrying(&run, &result);
            sleep(run.options().retry_delay);
            run.attempt += 1;
        }
    });

    observer.finished(&run, &result);
    result
}

/// [internal]
///
/// Runs the async `attempts` of `run` like [`run_attempts`], reporting the
/// run as cancelled when the future is dropped before it finishes.
#[cfg(feature = "tokio")]
pub(crate) async fn run_attempts_async(
    observer: &dyn ProcessObserver,
    mut run: CommandRun<'_>,
    attempts: &mut impl AsyncAttempts,
) -> Result<CommandOutput> {
    run.id = crate::utils::next_run_id();
    let mut observed = ObservedRun::new(observer, run);

    let result = match validate(
        &observed.run.program(),
        observed.run.options(),
        observed.run.sink,
    ) {
        Ok(()) => loop {
            let result = attempts.attempt(&observed.run).await;

            if !retry(&observed.run, &result) {
                break result;
            }

            observer.retrying(&observed.run, &result);
            attempts.delay(observed.run.options().retry_delay).await;
            observed.run.attempt += 1;
        },
        Err(error) => Err(error),
    };

    observed.finish(result)
}

/// Returns whether the failed `result` of the attempt of `run` is retried.
/// A run whose output was accepted by its sink is never retried, as the
/// sink cannot be reset.
fn retry(run: &CommandRun, result: &Result<CommandOutput>) -> bool {
    result.is_err()
        && !run.output_sunk.load(Ordering::Relaxed)
        && run.attempt <= run.options().retries
        && run.attempt < u32::MAX
        && run
            .options()
            .retry_if
            .as_ref()
            .is_none_or(|retry_if| (retry_if.0)(run, result))
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

    check(
        &program,
        CommandOutput {
            status: status.and_then(|status| status.code()),
            signal: status.as_ref().and_then(exit_signal),
            stdout,
            stderr,
            duration: start.elapsed(),
            timed_out: status.is_none(),
            display_lines: run.options().display_lines,
        },
    )
}

const DISPLAY_LINES: usize = 10;

/// How long the output of a child process is read at most after it exited,
/// see [`Process::run`].
pub(crate) const EXIT_GRACE: Duration = Duration::from_millis(100);

/// [internal]
///
/// Records the change of the variable `key` in `changes`, replacing an
/// earlier one.
///
/// WHY: the keys are compared like the platform does, case-insensitively
/// on Windows.
pub(crate) fn change_var(
    changes: &mut Vec<(String, Option<String>)>,
    key: String,
    value: Option<String>,
) {
    #[cfg(windows)]
    changes.retain(|(k, _)| !k.eq_ignore_ascii_case(&key));
    #[cfg(not(windows))]
    changes.retain(|(k, _)| *k != key);
    changes.push((key, value));
}

impl<'a, 'b, 'c> Outputs<'a, 'b, 'c> {
    pub(crate) fn new(
        observer: &'a dyn ProcessObserver,
        run: &'a CommandRun<'b>,
        sink: Option<&'a mut OutputSink<'c>>,
    ) -> Self {
        Self {
            stdout: Vec::new(),
            stderr: Vec::new(),
            observer: run.options().stream.then_some(observer),
            run,
            sink,
            sink_error: None,
        }
    }

    /// Takes the `data` of the output `stream` of the child process.
    pub(crate) fn push(&mut self, stream: OutputStream, data: &[u8]) {
        if let Some(observer) = self.observer {
            match stream {
                OutputStream::Stdout => observer.stdout(self.run, data),
                OutputStream::Stderr => observer.stderr(self.run, data),
            }
        }

        match (&mut self.sink, stream) {
            (Some(sink), _) => match sink(stream, data) {
                Ok(()) => self.run.output_sunk.store(true, Ordering::Relaxed),
                Err(error) => self.sink_error = Some(error),
            },
            (None, OutputStream::Stdout) => self.stdout.extend_from_slice(data),
            (None, OutputStream::Stderr) => self.stderr.extend_from_slice(data),
        }
    }

    /// Returns `true` once the sink failed, which ends the attempt.
    pub(crate) fn failed(&self) -> bool {
        self.sink_error.is_some()
    }

    /// Returns the standard output and error captured, failing with the
    /// error of the sink if it failed.
    pub(crate) fn finish(self) -> Result<(Vec<u8>, Vec<u8>)> {
        self.sink_error.map_or(Ok((self.stdout, self.stderr)), Err)
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
    use std::sync::Mutex;

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

        impl ProcessObserver for ExitOnOutput {
            fn stdout(&self, _run: &CommandRun, _data: &[u8]) {
                std::process::exit(0);
            }
        }

        let mut process = process(dir);
        process.set_observer(Arc::new(ExitOnOutput));
        let _ = shell("exec 3> fifo; echo ready; exec sleep 10")
            .stream()
            .timeout(Duration::from_secs(10))
            .run(&process);
        std::process::exit(1)
    }

    fn process(current_dir: PathBuf) -> ProcessImpl {
        ProcessImpl::new(current_dir)
    }

    #[derive(Default)]
    struct RecordingObserver {
        events: Mutex<Vec<String>>,
        options: Mutex<Vec<CommandOptions>>,
        stdout: Mutex<Vec<u8>>,
        stderr: Mutex<Vec<u8>>,
    }

    impl RecordingObserver {
        fn events(&self) -> Vec<String> {
            self.events.lock().unwrap().clone()
        }

        fn options(&self) -> CommandOptions {
            self.options.lock().unwrap()[0].clone()
        }
    }

    impl ProcessObserver for RecordingObserver {
        fn started(&self, run: &CommandRun) {
            self.events.lock().unwrap().push(format!(
                "started in {}",
                run.current_dir().unwrap().display()
            ));
            self.options.lock().unwrap().push(run.options().clone());
        }

        fn stdout(&self, _run: &CommandRun, data: &[u8]) {
            self.stdout.lock().unwrap().extend_from_slice(data);
        }

        fn stderr(&self, _run: &CommandRun, data: &[u8]) {
            self.stderr.lock().unwrap().extend_from_slice(data);
        }

        fn retrying(&self, run: &CommandRun, _result: &Result<CommandOutput>) {
            self.events
                .lock()
                .unwrap()
                .push(format!("retrying attempt {}", run.attempt()));
        }

        fn finished(&self, _run: &CommandRun, result: &Result<CommandOutput>) {
            self.events.lock().unwrap().push(format!(
                "finished {}",
                result
                    .as_ref()
                    .map_or_else(|error| error.to_string(), |_| "ok".to_string())
            ));
        }
    }

    fn observed() -> (ProcessImpl, Arc<RecordingObserver>) {
        let observer = Arc::new(RecordingObserver::default());
        let mut process = process(std::env::temp_dir());
        process.set_observer(observer.clone());
        (process, observer)
    }

    #[cfg(feature = "tokio")]
    fn block_on<F: Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    mod command_ext {
        use super::*;

        mod run {
            use super::*;

            #[test]
            fn run_should_run_command_with_default_options() {
                let (process, observer) = observed();

                shell("exit 0").run(&process).unwrap();

                assert_eq!(observer.options(), CommandOptions::default());
            }
        }

        mod run_to {
            use super::*;

            #[test]
            fn run_to_should_run_command_with_default_options_to_sink() {
                let (process, observer) = observed();
                let mut sunk = (Vec::new(), Vec::new());

                shell("echo out")
                    .run_to(&process, &mut sink_into(&mut sunk))
                    .unwrap();

                assert_eq!(observer.options(), CommandOptions::default());
                assert_eq!(String::from_utf8_lossy(&sunk.0).trim(), "out");
            }
        }

        #[cfg(feature = "tokio")]
        mod run_async {
            use super::*;

            #[test]
            fn run_async_should_run_command_with_default_options() {
                let (process, observer) = observed();

                block_on(shell("exit 0").run_async(&process)).unwrap();

                assert_eq!(observer.options(), CommandOptions::default());
            }
        }

        #[cfg(feature = "tokio")]
        mod run_async_to {
            use super::*;

            #[test]
            fn run_async_to_should_run_command_with_default_options_to_sink() {
                let (process, observer) = observed();
                let mut sunk = (Vec::new(), Vec::new());

                block_on(shell("echo out").run_async_to(&process, &mut sink_into(&mut sunk)))
                    .unwrap();

                assert_eq!(observer.options(), CommandOptions::default());
                assert_eq!(String::from_utf8_lossy(&sunk.0).trim(), "out");
            }
        }

        mod clear_env {
            use super::*;

            #[test]
            fn clear_env_should_run_command_without_configured_vars() {
                let (process, observer) = observed();

                shell("exit 0").clear_env().run(&process).unwrap();

                assert_eq!(
                    observer.options(),
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
                let (process, observer) = observed();

                let error = shell("echo 1&& echo 2&& echo 3&& exit 1")
                    .display_lines(1)
                    .run(&process)
                    .unwrap_err();

                assert_eq!(observer.options().display_lines, 1);
                let output = error.command_output().unwrap();
                assert_eq!(output.display_lines, 1);
                assert!(output.to_string().contains("stdout:\n(2 lines omitted)\n3"));
            }
        }

        mod input {
            use super::*;

            #[test]
            fn input_should_run_command_with_input() {
                let (process, observer) = observed();

                shell("exit 0").input("b\na").run(&process).unwrap();

                assert_eq!(
                    observer.options(),
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
                let (process, observer) = observed();

                shell("exit 0").interactive().run(&process).unwrap();

                assert_eq!(
                    observer.options(),
                    CommandOptions {
                        interactive: true,
                        ..Default::default()
                    }
                );
            }
        }

        mod retry {
            use super::*;

            #[test]
            fn retry_should_run_command_with_retries() {
                let (process, observer) = observed();

                shell("exit 0")
                    .retry(2, Duration::from_millis(1))
                    .run(&process)
                    .unwrap();

                assert_eq!(
                    observer.options(),
                    CommandOptions {
                        retries: 2,
                        retry_delay: Duration::from_millis(1),
                        ..Default::default()
                    }
                );
            }
        }

        mod silent {
            use super::*;

            #[test]
            fn silent_should_run_command_discarding_output() {
                let (process, observer) = observed();

                shell("exit 0").silent().run(&process).unwrap();

                assert_eq!(
                    observer.options(),
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
                let (process, observer) = observed();

                shell("exit 0").stream().run(&process).unwrap();

                assert_eq!(
                    observer.options(),
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
                let (process, observer) = observed();

                shell("exit 0")
                    .timeout(Duration::from_secs(10))
                    .run(&process)
                    .unwrap();

                assert_eq!(
                    observer.options(),
                    CommandOptions {
                        timeout: Some(Duration::from_secs(10)),
                        ..Default::default()
                    }
                );
            }
        }
    }

    mod command_run {
        use super::*;

        #[test]
        fn command_run_should_combine_options() {
            let (process, observer) = observed();

            let _ = shell("exit 0")
                .timeout(Duration::from_secs(10))
                .stream()
                .clear_env()
                .silent()
                .display_lines(3)
                .retry(1, Duration::from_millis(1))
                .input("in")
                .interactive()
                .run(&process);

            assert_eq!(
                observer.options(),
                CommandOptions {
                    display_lines: 3,
                    clear_env: true,
                    silent: true,
                    input: Some(b"in".to_vec()),
                    interactive: true,
                    retries: 1,
                    retry_delay: Duration::from_millis(1),
                    retry_if: None,
                    stream: true,
                    timeout: Some(Duration::from_secs(10))
                }
            );
        }

        mod run_to {
            use super::*;

            #[test]
            fn run_to_should_run_with_options_to_sink() {
                let (process, observer) = observed();
                let command = shell("echo out");
                let mut sunk = (Vec::new(), Vec::new());

                command
                    .display_lines(3)
                    .run_to(&process, &mut sink_into(&mut sunk))
                    .unwrap();

                assert_eq!(observer.options().display_lines, 3);
                assert_eq!(String::from_utf8_lossy(&sunk.0).trim(), "out");
            }
        }

        #[cfg(feature = "tokio")]
        mod run_async_to {
            use super::*;

            #[test]
            fn run_async_to_should_run_with_options_to_sink() {
                let (process, observer) = observed();
                let command = shell("echo out");
                let mut sunk = (Vec::new(), Vec::new());

                block_on(
                    command
                        .display_lines(3)
                        .run_async_to(&process, &mut sink_into(&mut sunk)),
                )
                .unwrap();

                assert_eq!(observer.options().display_lines, 3);
                assert_eq!(String::from_utf8_lossy(&sunk.0).trim(), "out");
            }
        }

        mod retry_if {
            use super::*;

            #[test]
            fn retry_if_should_set_retry_predicate() {
                let (process, observer) = observed();

                shell("exit 0")
                    .retry(2, Duration::from_millis(1))
                    .retry_if(|_, _| true)
                    .run(&process)
                    .unwrap();

                let options = observer.options();
                assert_eq!(options.retries, 2);
                assert_eq!(options.retry_delay, Duration::from_millis(1));
                assert!(options.retry_if.is_some());
            }
        }

        mod id {
            use super::*;

            #[derive(Default)]
            struct Ids(Mutex<Vec<u64>>);

            impl ProcessObserver for Ids {
                fn started(&self, run: &CommandRun) {
                    self.0.lock().unwrap().push(run.id());
                }

                fn retrying(&self, run: &CommandRun, _result: &Result<CommandOutput>) {
                    self.0.lock().unwrap().push(run.id());
                }

                fn finished(&self, run: &CommandRun, result: &Result<CommandOutput>) {
                    assert!(result.is_err());
                    self.0.lock().unwrap().push(run.id());
                }
            }

            fn ids_of_two_runs(run: impl Fn(&ProcessImpl)) -> Vec<u64> {
                let ids = Arc::new(Ids::default());
                let mut process = process(std::env::temp_dir());
                process.set_observer(ids.clone());

                run(&process);
                run(&process);

                ids.0.lock().unwrap().clone()
            }

            #[test]
            fn id_should_be_zero_before_run_starts() {
                assert_eq!(CommandRun::new(&shell("exit 1")).id(), 0);
            }

            #[test]
            fn id_should_be_same_for_attempts_and_unique_per_run() {
                let ids = ids_of_two_runs(|process| {
                    let _ = shell("exit 1").retry(1, Duration::ZERO).run(process);
                });

                assert_ne!(ids[0], 0);
                assert_eq!(ids[..3], [ids[0]; 3]);
                assert_eq!(ids[3..], [ids[3]; 3]);
                assert_ne!(ids[0], ids[3]);
            }

            #[cfg(feature = "tokio")]
            #[test]
            fn id_async_should_be_same_for_attempts_and_unique_per_run() {
                let ids = ids_of_two_runs(|process| {
                    let _ = block_on(shell("exit 1").retry(1, Duration::ZERO).run_async(process));
                });

                assert_ne!(ids[0], 0);
                assert_eq!(ids[..3], [ids[0]; 3]);
                assert_eq!(ids[3..], [ids[3]; 3]);
                assert_ne!(ids[0], ids[3]);
            }
        }

        mod new {
            use super::*;

            #[test]
            fn new_should_use_default_options() {
                let (process, observer) = observed();

                CommandRun::new(&shell("exit 0")).run(&process).unwrap();

                assert_eq!(observer.options(), CommandOptions::default());
            }
        }

        mod with_options {
            use super::*;

            #[test]
            fn with_options_should_run_command_with_options() {
                let (process, observer) = observed();
                let options = CommandOptions {
                    display_lines: 3,
                    ..Default::default()
                };
                let command = shell("exit 0");

                process
                    .run(CommandRun::with_options(&command, &options))
                    .unwrap();

                assert_eq!(observer.options(), options);
            }
        }

        mod from {
            use super::*;

            #[allow(clippy::module_inception)]
            mod from {
                use super::*;

                #[test]
                fn from_command_should_run_it_with_default_options() {
                    let (process, observer) = observed();
                    let mut command = shell("exit 0");

                    process.run(&command).unwrap();
                    process.run(&mut command).unwrap();

                    assert_eq!(observer.options(), CommandOptions::default());
                    assert_eq!(observer.events().len(), 4);
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

    #[cfg(feature = "tokio")]
    mod command_cancelled {
        use super::*;

        mod std_fmt_display {
            use super::*;

            #[test]
            fn fmt_should_display_cancelled() {
                assert_eq!(CommandCancelled.to_string(), "cancelled");
            }
        }
    }

    mod retry_if {
        use super::*;

        mod std_fmt_debug {
            use super::*;

            #[test]
            fn fmt_should_name_type() {
                assert_eq!(format!("{:?}", RetryIf::new(|_, _| true)), "RetryIf");
            }
        }

        mod partial_eq {
            use super::*;

            #[test]
            fn eq_should_compare_closure_identity() {
                let retry_if = RetryIf::new(|_, _| true);

                assert_eq!(retry_if, retry_if.clone());
                assert_ne!(retry_if, RetryIf::new(|_, _| true));
            }
        }
    }

    mod command_output {
        use super::*;

        mod success {
            use super::*;

            #[test]
            fn success_should_require_zero_exit_code_in_time() {
                for (status, timed_out, expected) in [
                    (Some(0), false, true),
                    (Some(1), false, false),
                    (None, false, false),
                    (Some(0), true, false),
                ] {
                    let output = CommandOutput {
                        status,
                        timed_out,
                        ..Default::default()
                    };

                    assert_eq!(
                        output.success(),
                        expected,
                        "status: {status:?}, timed_out: {timed_out}"
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
            fn eq_should_ignore_display_lines() {
                let output = CommandOutput {
                    status: Some(0),
                    stdout: b"out".to_vec(),
                    ..Default::default()
                };

                assert_eq!(
                    output,
                    CommandOutput {
                        display_lines: 3,
                        ..output.clone()
                    }
                );
            }

            #[test]
            fn eq_should_compare_data() {
                let output = CommandOutput {
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

        mod current_dir_setter {
            use super::*;

            #[test]
            fn set_current_dir_should_update_current_directory() {
                let mut process = ProcessImpl::new(PathBuf::new());
                let new_dir = PathBuf::from("/tmp");
                process.set_current_dir(new_dir.clone());
                assert_eq!(process.current_dir, new_dir);
            }
        }

        mod env_var_setter {
            use super::*;

            mod set_var {
                use super::*;

                #[test]
                fn set_var_should_record_change_replacing_earlier_one() {
                    let mut process = ProcessImpl::new(PathBuf::new());
                    process.remove_var("A");

                    process.set_var("A".to_string(), "2".to_string());
                    process.set_var("B".to_string(), "3".to_string());

                    assert_eq!(
                        process.var_changes,
                        [
                            ("A".to_string(), Some("2".to_string())),
                            ("B".to_string(), Some("3".to_string()))
                        ]
                    );
                }

                #[cfg(windows)]
                #[test]
                fn set_var_should_replace_earlier_change_ignoring_case() {
                    let mut process = ProcessImpl::new(PathBuf::new());
                    process.set_var("Path".to_string(), "1".to_string());

                    process.set_var("PATH".to_string(), "2".to_string());

                    assert_eq!(
                        process.var_changes,
                        [("PATH".to_string(), Some("2".to_string()))]
                    );
                }
            }

            mod remove_var {
                use super::*;

                #[test]
                fn remove_var_should_record_removal_replacing_earlier_change() {
                    let mut process = ProcessImpl::new(PathBuf::new());
                    process.set_var("A".to_string(), "1".to_string());

                    process.remove_var("A");

                    assert_eq!(process.var_changes, [("A".to_string(), None)]);
                }
            }
        }

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
                        retries: 0,
                        retry_delay: Duration::ZERO,
                        retry_if: None,
                        stream: true,
                        timeout: None,
                    };

                    let output = process
                        .run(CommandRun::with_options(
                            &shell("echo out&& echo err 1>&2"),
                            &options,
                        ))
                        .unwrap();

                    assert_eq!(output.stdout_str().unwrap().trim(), "out");
                    assert_eq!(output.stderr_str().unwrap().trim(), "err");
                }

                #[test]
                fn run_streaming_should_report_output_to_observer() {
                    let (process, observer) = observed();
                    let options = CommandOptions {
                        stream: true,
                        ..Default::default()
                    };

                    let output = process
                        .run(CommandRun::with_options(
                            &shell("echo out&& echo err 1>&2"),
                            &options,
                        ))
                        .unwrap();

                    assert_eq!(*observer.stdout.lock().unwrap(), output.stdout);
                    assert_eq!(*observer.stderr.lock().unwrap(), output.stderr);
                    assert!(!output.stdout.is_empty());
                }

                #[test]
                fn run_silent_should_neither_capture_nor_stream_output() {
                    let (process, observer) = observed();
                    let options = CommandOptions {
                        silent: true,
                        stream: true,
                        ..Default::default()
                    };

                    let output = process
                        .run(CommandRun::with_options(
                            &shell("echo out&& echo err 1>&2"),
                            &options,
                        ))
                        .unwrap();

                    assert_eq!(output.status, Some(0));
                    assert!(output.stdout.is_empty());
                    assert!(output.stderr.is_empty());
                    assert!(observer.stdout.lock().unwrap().is_empty());
                    assert!(observer.stderr.lock().unwrap().is_empty());
                }

                #[test]
                fn run_with_retries_should_retry_until_success() {
                    // GIVEN a command failing on its first attempt only
                    let dir = TestDir::new();
                    let (mut process, observer) = observed();
                    process.set_current_dir(dir.path().to_path_buf());
                    let options = CommandOptions {
                        retries: 2,
                        retry_delay: Duration::from_millis(1),
                        ..Default::default()
                    };

                    // WHEN running it with retries
                    let output = process
                        .run(CommandRun::with_options(&shell(FAIL_ONCE), &options))
                        .unwrap();

                    // THEN the second attempt succeeds
                    assert_eq!(output.status, Some(0));
                    assert_eq!(
                        observer.events()[1..],
                        ["retrying attempt 1", "finished ok"]
                    );
                }

                #[test]
                fn run_exhausting_retries_should_return_last_failure() {
                    let (process, observer) = observed();
                    let options = CommandOptions {
                        retries: 2,
                        retry_delay: Duration::from_millis(1),
                        ..Default::default()
                    };

                    let error = process
                        .run(CommandRun::with_options(&shell("exit 3"), &options))
                        .unwrap_err();

                    assert_eq!(error.command_output().unwrap().status, Some(3));
                    assert_eq!(
                        observer.events()[1..3],
                        ["retrying attempt 1", "retrying attempt 2"]
                    );
                    assert_eq!(observer.events().len(), 4);
                }

                #[test]
                fn run_retry_if_should_retry_only_matching_failures() {
                    fn retry_exit_3(_: &CommandRun, result: &Result<CommandOutput>) -> bool {
                        result
                            .as_ref()
                            .err()
                            .and_then(crate::Error::command_output)
                            .and_then(|output| output.status)
                            == Some(3)
                    }

                    let (process, observer) = observed();
                    let options = CommandOptions {
                        retries: 1,
                        retry_delay: Duration::from_millis(1),
                        retry_if: Some(RetryIf::new(retry_exit_3)),
                        ..Default::default()
                    };

                    let _ = process.run(CommandRun::with_options(&shell("exit 3"), &options));
                    let _ = process.run(CommandRun::with_options(&shell("exit 4"), &options));

                    let events = observer.events();
                    assert_eq!(events[1], "retrying attempt 1");
                    assert!(events[3].starts_with("started"));
                    assert!(events[4].starts_with("finished"));
                    assert_eq!(events.len(), 5);
                }

                #[test]
                fn run_invalid_options_should_not_be_retried() {
                    let (process, observer) = observed();
                    let options = CommandOptions {
                        interactive: true,
                        input: Some(Vec::new()),
                        retries: 3,
                        retry_delay: Duration::from_secs(3600),
                        ..Default::default()
                    };

                    let error = process
                        .run(CommandRun::with_options(&shell("exit 0"), &options))
                        .unwrap_err();

                    assert!(
                        error
                            .description()
                            .ends_with("': interactive cannot be combined with input")
                    );
                    assert_eq!(observer.events().len(), 2);
                }

                #[test]
                fn run_retries_without_observer_should_return_last_failure() {
                    let process = process(std::env::temp_dir());
                    let options = CommandOptions {
                        retries: 1,
                        retry_delay: Duration::from_millis(1),
                        ..Default::default()
                    };

                    let error = process
                        .run(CommandRun::with_options(&shell("exit 3"), &options))
                        .unwrap_err();

                    assert_eq!(error.command_output().unwrap().status, Some(3));
                }

                #[test]
                fn run_should_report_start_and_result_to_observer() {
                    let (process, observer) = observed();

                    process.run(&shell("echo out")).unwrap();
                    let _ = process.run(&shell("exit 3"));

                    let events = observer.events();
                    let dir = std::env::temp_dir();
                    assert_eq!(events[0], format!("started in {}", dir.display()));
                    assert_eq!(events[1], "finished ok");
                    assert_eq!(events[2], format!("started in {}", dir.display()));
                    assert!(events[3].ends_with("' failed with exit code 3"));
                    assert!(observer.stdout.lock().unwrap().is_empty());
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
                    let mut process = process(std::env::temp_dir());
                    process.remove_var(INHERITED_VAR);

                    let output = process.run(&print_var(INHERITED_VAR)).unwrap();

                    assert_eq!(output.stdout_str().unwrap().trim(), "unset");
                }

                #[test]
                fn run_should_pass_vars() {
                    let mut process = process(std::env::temp_dir());
                    process.set_var("AGPLATFORM_VAR".to_string(), "value".to_string());

                    let output = process.run(&print_var("AGPLATFORM_VAR")).unwrap();

                    assert_eq!(output.stdout_str().unwrap().trim(), "value");
                }

                #[test]
                fn run_command_vars_should_override_vars() {
                    // GIVEN configured variables
                    let mut process = process(std::env::temp_dir());
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
                    let mut process = process(std::env::temp_dir());
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
                            &options,
                        ))
                        .unwrap();
                    let own = process
                        .run(CommandRun::with_options(
                            print_var("AGPLATFORM_OWN").env("AGPLATFORM_OWN", "2"),
                            &options,
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
                    let mut configured = process(std::env::temp_dir());
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
                    let mut process = process(std::env::temp_dir());
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
                        .run(CommandRun::with_options(&echo_stdin(), &options))
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
                        .run(CommandRun::with_options(&echo_stdin(), &options))
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
                        .run(CommandRun::with_options(&shell("exit 0"), &options))
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
                        .run(CommandRun::with_options(&shell("exit 3"), &options))
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
                        .run(CommandRun::with_options(&shell("exit 0"), &options))
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
                        retries: 0,
                        retry_delay: Duration::ZERO,
                        retry_if: None,
                        stream: false,
                        timeout: Some(Duration::from_secs(10)),
                    };

                    let output = process
                        .run(CommandRun::with_options(&shell("echo out"), &options))
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
                            .run(CommandRun::with_options(&command, &options))
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
                        retries: 0,
                        retry_delay: Duration::ZERO,
                        retry_if: None,
                        stream: false,
                        timeout: Some(Duration::from_millis(200)),
                    };

                    // WHEN it exceeds the timeout
                    let error = process
                        .run(CommandRun::with_options(&shell(SLEEP_THEN_ECHO), &options))
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
                fn run_async_streaming_should_report_output_to_observer() {
                    let (process, observer) = observed();
                    let options = CommandOptions {
                        stream: true,
                        ..Default::default()
                    };

                    let output = block_on(process.run_async(CommandRun::with_options(
                        &shell("echo out&& echo err 1>&2"),
                        &options,
                    )))
                    .unwrap();

                    assert_eq!(*observer.stdout.lock().unwrap(), output.stdout);
                    assert_eq!(*observer.stderr.lock().unwrap(), output.stderr);
                    assert_eq!(output.stdout_str().unwrap().trim(), "out");
                }

                #[test]
                fn run_async_silent_should_neither_capture_nor_stream_output() {
                    let (process, observer) = observed();
                    let options = CommandOptions {
                        silent: true,
                        stream: true,
                        ..Default::default()
                    };

                    let output = block_on(process.run_async(CommandRun::with_options(
                        &shell("echo out&& echo err 1>&2"),
                        &options,
                    )))
                    .unwrap();

                    assert!(output.stdout.is_empty());
                    assert!(output.stderr.is_empty());
                    assert!(observer.stdout.lock().unwrap().is_empty());
                }

                #[test]
                fn run_async_with_retries_should_retry_until_success() {
                    // GIVEN a command failing on its first attempt only
                    let dir = TestDir::new();
                    let (mut process, observer) = observed();
                    process.set_current_dir(dir.path().to_path_buf());
                    let options = CommandOptions {
                        retries: 2,
                        retry_delay: Duration::from_millis(1),
                        ..Default::default()
                    };

                    // WHEN running it with retries
                    let output = block_on(
                        process.run_async(CommandRun::with_options(&shell(FAIL_ONCE), &options)),
                    )
                    .unwrap();

                    // THEN the second attempt succeeds
                    assert_eq!(output.status, Some(0));
                    assert_eq!(
                        observer.events()[1..],
                        ["retrying attempt 1", "finished ok"]
                    );
                }

                #[test]
                fn run_async_exhausting_retries_should_return_last_failure() {
                    let (process, observer) = observed();
                    let options = CommandOptions {
                        retries: 1,
                        retry_delay: Duration::from_millis(1),
                        ..Default::default()
                    };

                    let error = block_on(
                        process.run_async(CommandRun::with_options(&shell("exit 3"), &options)),
                    )
                    .unwrap_err();

                    assert_eq!(error.command_output().unwrap().status, Some(3));
                    assert_eq!(observer.events().len(), 3);
                    assert_eq!(observer.events()[1], "retrying attempt 1");
                }

                #[test]
                fn run_async_input_should_write_input_to_stdin() {
                    let process = process(std::env::temp_dir());
                    let options = CommandOptions {
                        input: Some(b"b\na\n".to_vec()),
                        ..Default::default()
                    };

                    let output = block_on(
                        process.run_async(CommandRun::with_options(&echo_stdin(), &options)),
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
                        process.run_async(CommandRun::with_options(&echo_stdin(), &options)),
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
                        process.run_async(CommandRun::with_options(&shell("exit 0"), &options)),
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
                        process.run_async(CommandRun::with_options(&shell("exit 3"), &options)),
                    )
                    .unwrap_err();

                    let output = error.command_output().unwrap();
                    assert_eq!(output.status, Some(3));
                    assert!(output.stdout.is_empty());
                }

                #[test]
                fn run_async_invalid_options_should_return_error() {
                    let (process, observer) = observed();
                    let options = CommandOptions {
                        interactive: true,
                        timeout: Some(Duration::from_secs(1)),
                        ..Default::default()
                    };

                    let error = block_on(
                        process.run_async(CommandRun::with_options(&shell("exit 0"), &options)),
                    )
                    .unwrap_err();

                    assert!(
                        error
                            .description()
                            .ends_with("': interactive cannot be combined with timeout")
                    );
                    assert_eq!(observer.events().len(), 2);
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
                        process.run_async(CommandRun::with_options(&shell("echo out"), &options)),
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
                        let output = block_on(
                            process.run_async(CommandRun::with_options(&command, &options)),
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
                    let error = block_on(
                        process
                            .run_async(CommandRun::with_options(&shell(ECHO_THEN_SLEEP), &options)),
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
                fn run_async_cancelled_should_report_finished_cancelled() {
                    let (process, observer) = observed();
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
                        observer.events().last().unwrap(),
                        &format!(
                            "finished [Process] '{}' cancelled",
                            command.get_program().to_string_lossy()
                        )
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
                fn run_to_streamed_should_report_output_to_observer_too() {
                    let (process, observer) = observed();
                    let mut sunk = (Vec::new(), Vec::new());

                    process
                        .run_to(shell("echo out").stream(), &mut sink_into(&mut sunk))
                        .unwrap();

                    assert_eq!(*observer.stdout.lock().unwrap(), sunk.0);
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
                fn run_to_failed_after_sink_accepted_output_should_not_retry() {
                    let (process, observer) = observed();
                    let mut sunk = (Vec::new(), Vec::new());

                    let error = process
                        .run_to(
                            shell("echo out&& exit 3").retry(2, Duration::ZERO),
                            &mut sink_into(&mut sunk),
                        )
                        .unwrap_err();

                    assert_eq!(error.command_output().unwrap().status, Some(3));
                    assert!(
                        !observer
                            .events()
                            .iter()
                            .any(|event| event.starts_with("retrying"))
                    );
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

            mod set_observer {
                use super::*;

                #[test]
                fn set_observer_should_replace_observer() {
                    // GIVEN an observer replaced by another one
                    let (mut process, replaced) = observed();
                    let observer = Arc::new(RecordingObserver::default());
                    process.set_observer(observer.clone());

                    // WHEN running a command
                    process.run(&shell("exit 0")).unwrap();

                    // THEN only the new observer sees it
                    assert!(replaced.events().is_empty());
                    assert_eq!(observer.events().len(), 2);
                }
            }
        }
    }

    mod check {
        use super::*;

        #[test]
        fn check_success_should_return_output() {
            let output = CommandOutput {
                status: Some(0),
                ..Default::default()
            };

            assert_eq!(check("git", output.clone()).unwrap(), output);
        }

        #[test]
        fn check_failure_should_return_error_with_output() {
            for (status, signal, timed_out, description) in [
                (Some(1), None, false, "'git' failed with exit code 1"),
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

                let error = check("git", output.clone()).unwrap_err();

                assert_eq!(error.description(), description);
                assert_eq!(error.command_output(), Some(&output), "{description}");
            }
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

    mod retry {
        use super::*;

        #[test]
        fn retry_last_countable_attempt_should_not_retry() {
            let command = Command::new("git");
            let options = CommandOptions {
                retries: u32::MAX,
                ..Default::default()
            };
            let mut run = CommandRun::with_options(&command, &options);
            let result = Err(crate::Error::process("failed"));

            run.attempt = u32::MAX - 1;
            assert!(retry(&run, &result));
            run.attempt = u32::MAX;
            assert!(!retry(&run, &result));
        }
    }

    mod outputs {
        use super::*;

        mod push {
            use super::*;

            #[test]
            fn push_should_capture_streams_and_report_them_when_streamed() {
                // GIVEN the outputs of a streamed run recording them
                let (process, observer) = observed();
                let command = Command::new("sh");
                let options = CommandOptions {
                    stream: true,
                    ..Default::default()
                };
                let run = CommandRun::with_options(&command, &options);
                let mut outputs = Outputs::new(process.observer.as_ref(), &run, None);

                // WHEN pushing chunks of both streams
                outputs.push(OutputStream::Stdout, b"out");
                outputs.push(OutputStream::Stderr, b"err");

                // THEN they are captured and reported
                assert_eq!(
                    outputs.finish().unwrap(),
                    (b"out".to_vec(), b"err".to_vec())
                );
                assert_eq!(*observer.stdout.lock().unwrap(), b"out");
                assert_eq!(*observer.stderr.lock().unwrap(), b"err");
            }

            #[test]
            fn push_with_sink_should_pass_chunks_instead_of_capturing() {
                let command = Command::new("sh");
                let run = CommandRun::new(&command);
                let mut sunk = Vec::new();
                let mut sink = |stream: OutputStream, data: &[u8]| {
                    sunk.push((stream, data.to_vec()));
                    Ok(())
                };
                let mut outputs = Outputs::new(&StdObserver, &run, Some(&mut sink));

                outputs.push(OutputStream::Stdout, b"out");
                outputs.push(OutputStream::Stderr, b"err");

                assert_eq!(outputs.finish().unwrap(), (Vec::new(), Vec::new()));
                assert!(run.output_sunk.load(Ordering::Relaxed));
                assert_eq!(
                    sunk,
                    [
                        (OutputStream::Stdout, b"out".to_vec()),
                        (OutputStream::Stderr, b"err".to_vec())
                    ]
                );
            }

            #[test]
            fn push_to_failing_sink_should_fail_outputs() {
                let command = Command::new("sh");
                let run = CommandRun::new(&command);
                let mut sink = |_: OutputStream, _: &[u8]| Err(crate::Error::process("full"));
                let mut outputs = Outputs::new(&StdObserver, &run, Some(&mut sink));

                outputs.push(OutputStream::Stdout, b"out");

                assert!(outputs.failed());
                assert!(!run.output_sunk.load(Ordering::Relaxed));
                assert_eq!(outputs.finish().unwrap_err().description(), "full");
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
