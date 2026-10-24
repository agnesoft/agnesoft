use std::borrow::Cow;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use crate::Result;
use crate::platform::platform_env::CurrentDirSetter;
use crate::platform::platform_env::EnvVarSetter;

macro_rules! process_error {
    ($($arg:tt)*) => {
        $crate::Error::process(format!($($arg)*))
    };
}

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix::{configure, wait_with_output};
#[cfg(windows)]
use windows::{configure, wait_with_output};

/// [internal]
///
/// Where the output of a child process is streamed to as it arrives.
pub(crate) type Sink<'a> = Option<&'a mut dyn FnMut(&[u8])>;

/// Observes the runs of commands by a [`Process`], e.g. to log them.
///
/// Every run is reported once to [`started`](Self::started) and
/// [`finished`](Self::finished), including the runs that fail to start. Each
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
    /// it arrives. Writes it to the standard output of the current process
    /// by default.
    fn stdout(&self, run: &CommandRun, data: &[u8]) {
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(data);
        let _ = stdout.flush();
    }

    /// Called with each chunk of the standard error of a streamed run as it
    /// arrives. Writes it to the standard error of the current process by
    /// default.
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
pub trait Process {
    /// Runs `command` to completion according to `options` and returns
    /// its output.
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
    /// use agplatform::{CommandOutput, Platform, Process, CommandOptions};
    /// use std::process::Command;
    ///
    /// let platform = agplatform::platform();
    /// let process = platform.process();
    /// # let process = agplatform::TestProcess::new();
    /// # process.set_output("git", CommandOutput { status: Some(0), stdout: b"git version 2.0".to_vec(), ..Default::default() });
    ///
    /// let output = process
    ///     .run(Command::new("git").arg("--version"), &CommandOptions::default())
    ///     .unwrap();
    /// assert!(output.stdout_str().unwrap().starts_with("git version"));
    /// ```
    fn run(&self, command: &Command, options: &CommandOptions) -> Result<CommandOutput>;

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
}

/// Options for [`Process::run`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandOptions {
    /// The number of the last lines of the standard output and error shown
    /// when displaying the [`CommandOutput`], e.g. as the source of an
    /// error. 10 by default.
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
    /// the terminal.
    pub interactive: bool,

    /// Writes the output of the child process to the standard output and
    /// error of the current process as it arrives, in addition to
    /// capturing it.
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
    /// Ctrl-C.
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

    /// The captured standard output.
    pub stdout: Vec<u8>,

    /// The captured standard error.
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
        process.run(self, &CommandOptions::default())
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
        }
    }

    /// Returns the attempt of the run, starting at `1`, see
    /// [`CommandOptions::retries`].
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Returns the command.
    pub fn command(&self) -> &'a Command {
        self.command
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
        process.run(self.command, &self.options)
    }

    /// [internal]
    ///
    /// Returns the run of `command` with `options` to be reported to a
    /// [`ProcessObserver`], in its directory resolved against
    /// `current_dir`.
    pub(crate) fn observed(
        command: &'a Command,
        options: &'a CommandOptions,
        current_dir: &Path,
    ) -> Self {
        Self {
            command,
            options: Cow::Borrowed(options),
            current_dir: Some(
                command
                    .get_current_dir()
                    .map_or(current_dir.to_path_buf(), |dir| current_dir.join(dir)),
            ),
            attempt: 1,
        }
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
            self.status
                .map_or("none".to_string(), |code| code.to_string()),
            self.duration,
            tail(&self.stdout_lossy(), self.display_lines),
            tail(&self.stderr_lossy(), self.display_lines)
        )
    }
}

impl std::error::Error for CommandOutput {}

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
    fn child_command(&self, run: &CommandRun) -> Command {
        let (command, options) = (run.command, run.options());
        let mut child = Command::new(command.get_program());
        child
            .args(command.get_args())
            .current_dir(run.current_dir().unwrap_or(&self.current_dir));

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

        child
    }

    fn attempt(&self, run: &CommandRun) -> Result<CommandOutput> {
        let mut stdout = |data: &[u8]| self.observer.stdout(run, data);
        let mut stderr = |data: &[u8]| self.observer.stderr(run, data);

        if run.options().stream {
            self.run_child(run, Some(&mut stdout), Some(&mut stderr))
        } else {
            self.run_child(run, None, None)
        }
    }

    fn run_child(
        &self,
        run: &CommandRun,
        stdout_sink: Sink,
        stderr_sink: Sink,
    ) -> Result<CommandOutput> {
        let options = run.options();
        let program = program(run.command);
        let mut child = self.child_command(run);
        configure(&mut child, options.timeout);

        let start = Instant::now();
        let deadline = options.timeout.map(|timeout| start + timeout);
        let (status, stdout, stderr) = child
            .spawn()
            .and_then(|mut child| {
                wait_with_output(
                    &mut child,
                    options.input.as_deref().unwrap_or_default(),
                    deadline,
                    stdout_sink,
                    stderr_sink,
                )
            })
            .map_err(|error| io_error(&program, error))?;

        check(
            &program,
            CommandOutput {
                status: status.and_then(|status| status.code()),
                stdout,
                stderr,
                duration: start.elapsed(),
                timed_out: status.is_none(),
                display_lines: options.display_lines,
            },
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
    fn run(&self, command: &Command, options: &CommandOptions) -> Result<CommandOutput> {
        run_attempts(
            self.observer.as_ref(),
            CommandRun::observed(command, options, &self.current_dir),
            &mut |run| self.attempt(run),
            std::thread::sleep,
        )
    }

    fn set_observer(&mut self, observer: Arc<dyn ProcessObserver>) {
        self.observer = observer;
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
        (false, None) => Err(
            process_error!("'{program}' was terminated without an exit code").with_source(output),
        ),
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
pub(crate) fn validate(program: &str, options: &CommandOptions) -> Result<()> {
    if options.interactive && options.input.is_some() {
        Err(process_error!(
            "'{program}': interactive cannot be combined with input"
        ))
    } else if options.interactive && options.timeout.is_some() {
        Err(process_error!(
            "'{program}': interactive cannot be combined with timeout"
        ))
    } else {
        Ok(())
    }
}

/// [internal]
///
/// Returns the last `lines` of `text`, preceded by the number of the lines
/// omitted if any.
pub(crate) fn tail(text: &str, lines: usize) -> String {
    match text.lines().count().checked_sub(lines) {
        Some(omitted @ 1..) => {
            let start: usize = text.split_inclusive('\n').take(omitted).map(str::len).sum();
            format!("({omitted} lines omitted)\n{}", &text[start..])
        }
        _ => text.to_string(),
    }
}

/// [internal]
///
/// Returns the program of `command`, lossily converted to UTF-8.
pub(crate) fn program(command: &Command) -> Cow<'_, str> {
    command.get_program().to_string_lossy()
}

/// [internal]
///
/// Returns the program and the arguments of `command` separated by spaces.
pub(crate) fn command_line(command: &Command) -> String {
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(std::ffi::OsStr::to_string_lossy)
        .collect::<Vec<_>>()
        .join(" ")
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
    observer.started(&run);

    let result = validate(&program(run.command), run.options()).and_then(|()| {
        loop {
            let result = attempt(&run);

            if result.is_ok()
                || run.attempt > run.options().retries
                || !run
                    .options()
                    .retry_if
                    .as_ref()
                    .is_none_or(|retry_if| (retry_if.0)(&run, &result))
            {
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

const DISPLAY_LINES: usize = 10;

/// [internal]
///
/// Records the change of the variable `key` in `changes`, replacing an
/// earlier one.
///
/// WHY: `std` applies the changes in order and compares the keys like the
/// platform does, e.g. case-insensitively on Windows.
pub(crate) fn change_var(
    changes: &mut Vec<(String, Option<String>)>,
    key: String,
    value: Option<String>,
) {
    changes.retain(|(k, _)| *k != key);
    changes.push((key, value));
}

/// [internal]
///
/// Appends `data` to `output` and passes it to `sink`.
pub(crate) fn capture(output: &mut Vec<u8>, sink: &mut Sink, data: &[u8]) {
    output.extend_from_slice(data);

    if let Some(sink) = sink {
        sink(data);
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
    const INHERITED_VAR: &str = "HOME";
    #[cfg(windows)]
    const INHERITED_VAR: &str = "USERPROFILE";

    #[cfg(unix)]
    const LEAVE_ORPHAN: &str = "(sleep 1; touch marker) & exit 0";
    #[cfg(windows)]
    const LEAVE_ORPHAN: &str =
        "start \"\" /b cmd /C \"ping -n 3 127.0.0.1 >NUL & type nul > marker\"";

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

        mod new {
            use super::*;

            #[test]
            fn new_should_use_default_options() {
                let (process, observer) = observed();

                CommandRun::new(&shell("exit 0")).run(&process).unwrap();

                assert_eq!(observer.options(), CommandOptions::default());
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

                    let output = process
                        .run(
                            &shell("echo out&& echo err 1>&2"),
                            &CommandOptions::default(),
                        )
                        .unwrap();

                    assert_eq!(output.status, Some(0));
                    assert_eq!(output.stdout_str().unwrap().trim(), "out");
                    assert_eq!(output.stderr_str().unwrap().trim(), "err");
                    assert!(!output.timed_out);
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
                        .run(&shell("echo out&& echo err 1>&2"), &options)
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
                        .run(&shell("echo out&& echo err 1>&2"), &options)
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
                        .run(&shell("echo out&& echo err 1>&2"), &options)
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
                    let output = process.run(&shell(FAIL_ONCE), &options).unwrap();

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

                    let error = process.run(&shell("exit 3"), &options).unwrap_err();

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

                    let _ = process.run(&shell("exit 3"), &options);
                    let _ = process.run(&shell("exit 4"), &options);

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

                    let error = process.run(&shell("exit 0"), &options).unwrap_err();

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

                    let error = process.run(&shell("exit 3"), &options).unwrap_err();

                    assert_eq!(error.command_output().unwrap().status, Some(3));
                }

                #[test]
                fn run_should_report_start_and_result_to_observer() {
                    let (process, observer) = observed();

                    process
                        .run(&shell("echo out"), &CommandOptions::default())
                        .unwrap();
                    let _ = process.run(&shell("exit 3"), &CommandOptions::default());

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

                    let output = process
                        .run(&shell(PRINT_DIR), &CommandOptions::default())
                        .unwrap();

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
                    let output = process
                        .run(
                            shell(PRINT_DIR).current_dir("sub"),
                            &CommandOptions::default(),
                        )
                        .unwrap();

                    // THEN the command runs in the sub directory
                    assert_eq!(
                        std::fs::canonicalize(output.stdout_str().unwrap().trim()).unwrap(),
                        std::fs::canonicalize(dir.path().join("sub")).unwrap()
                    );
                }

                #[test]
                fn run_unchanged_vars_should_inherit_environment() {
                    let process = ProcessImpl::new(std::env::temp_dir());

                    let output = process
                        .run(&print_var("PATH"), &CommandOptions::default())
                        .unwrap();

                    assert_ne!(output.stdout_str().unwrap().trim(), "unset");
                }

                #[test]
                fn run_should_not_pass_removed_inherited_var() {
                    let mut process = process(std::env::temp_dir());
                    process.remove_var(INHERITED_VAR);

                    let output = process
                        .run(&print_var(INHERITED_VAR), &CommandOptions::default())
                        .unwrap();

                    assert_eq!(output.stdout_str().unwrap().trim(), "unset");
                }

                #[test]
                fn run_should_pass_vars() {
                    let mut process = process(std::env::temp_dir());
                    process.set_var("AGPLATFORM_VAR".to_string(), "value".to_string());

                    let output = process
                        .run(&print_var("AGPLATFORM_VAR"), &CommandOptions::default())
                        .unwrap();

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
                        .run(
                            print_var("AGPLATFORM_SET").env("AGPLATFORM_SET", "2"),
                            &CommandOptions::default(),
                        )
                        .unwrap();
                    let removed = process
                        .run(
                            print_var("AGPLATFORM_REMOVED").env_remove("AGPLATFORM_REMOVED"),
                            &CommandOptions::default(),
                        )
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
                        .run(&print_var("AGPLATFORM_CONFIGURED"), &options)
                        .unwrap();
                    let own = process
                        .run(
                            print_var("AGPLATFORM_OWN").env("AGPLATFORM_OWN", "2"),
                            &options,
                        )
                        .unwrap();

                    // THEN only the variables of the commands are passed
                    assert_eq!(configured.stdout_str().unwrap().trim(), "unset");
                    assert_eq!(own.stdout_str().unwrap().trim(), "2");
                }

                #[test]
                fn run_input_should_write_input_to_stdin() {
                    let process = process(std::env::temp_dir());
                    let options = CommandOptions {
                        input: Some(b"b\na\n".to_vec()),
                        ..Default::default()
                    };

                    let output = process.run(&echo_stdin(), &options).unwrap();

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
                    let output = process.run(&echo_stdin(), &options).unwrap();

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

                    let output = process.run(&shell("exit 0"), &options).unwrap();

                    assert_eq!(output.status, Some(0));
                }

                #[test]
                fn run_interactive_should_not_capture_output() {
                    let process = process(std::env::temp_dir());
                    let options = CommandOptions {
                        interactive: true,
                        ..Default::default()
                    };

                    let error = process.run(&shell("exit 3"), &options).unwrap_err();

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

                    let error = process.run(&shell("exit 0"), &options).unwrap_err();

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

                    process.run(&command, &CommandOptions::default()).unwrap();

                    assert_eq!(command.get_current_dir(), None);
                    assert_eq!(command.get_envs().count(), 0);
                }

                #[test]
                fn run_non_zero_exit_should_return_error_with_output() {
                    let process = process(std::env::temp_dir());

                    let error = process
                        .run(&shell("echo out&& exit 3"), &CommandOptions::default())
                        .unwrap_err();

                    let output = error.command_output().unwrap();
                    assert_eq!(error.kind(), crate::ErrorKind::Process);
                    assert_eq!(output.status, Some(3));
                    assert_eq!(output.stdout_str().unwrap().trim(), "out");
                }

                #[test]
                fn run_missing_program_should_return_error_with_io_source() {
                    let process = process(std::env::temp_dir());

                    let error = process
                        .run(
                            &Command::new("agplatform_missing_program"),
                            &CommandOptions::default(),
                        )
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

                    let output = process.run(&shell("echo out"), &options).unwrap();

                    assert_eq!(output.stdout_str().unwrap().trim(), "out");
                    assert!(!output.timed_out);
                }

                #[test]
                fn run_exceeding_timeout_should_kill_descendants_of_exited_child() {
                    // GIVEN a child that exits leaving a descendant holding its
                    // output open, which would create a marker later
                    let dir = TestDir::new();
                    let process = process(dir.path().to_path_buf());
                    let options = CommandOptions {
                        timeout: Some(Duration::from_millis(200)),
                        ..Default::default()
                    };

                    // WHEN the timeout passes
                    let error = process.run(&shell(LEAVE_ORPHAN), &options).unwrap_err();
                    std::thread::sleep(Duration::from_millis(1500));

                    // THEN the descendant was killed before creating the marker
                    assert!(error.command_output().unwrap().timed_out);
                    assert!(!dir.path().join("marker").exists());
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
                    let error = process.run(&shell(SLEEP_THEN_ECHO), &options).unwrap_err();

                    // THEN both are killed so the output ends right away
                    let output = error.command_output().unwrap();
                    assert!(output.timed_out);
                    assert_eq!(output.status, None);
                    assert!(output.stdout.is_empty());
                    assert!(output.duration < Duration::from_secs(5));
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
                    process
                        .run(&shell("exit 0"), &CommandOptions::default())
                        .unwrap();

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
            for (status, timed_out, description) in [
                (Some(1), false, "'git' failed with exit code 1"),
                (None, false, "'git' was terminated without an exit code"),
                (None, true, "'git' timed out after 1s"),
            ] {
                let output = CommandOutput {
                    status,
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
                assert!(validate("git", &options).is_ok(), "{options:?}");
            }
        }

        #[test]
        fn validate_interactive_with_input_or_timeout_should_return_error() {
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
            ] {
                assert_eq!(
                    validate("git", &options).unwrap_err().description(),
                    description
                );
            }
        }
    }

    mod capture {
        use super::*;

        #[test]
        fn capture_should_append_data_and_pass_it_to_sink() {
            let mut output = b"a".to_vec();
            let mut sunk = Vec::new();
            let mut sink = |data: &[u8]| sunk.extend_from_slice(data);

            capture(&mut output, &mut Some(&mut sink), b"b");

            assert_eq!(output, b"ab");
            assert_eq!(sunk, b"b");
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

    mod program {
        use super::*;

        #[test]
        fn program_should_return_program_without_args() {
            let mut command = Command::new("git");
            command.args(["clone", "https://user:token@example.com/repo"]);

            assert_eq!(program(&command), "git");
        }
    }

    mod tail {
        use super::*;

        #[test]
        fn tail_should_keep_up_to_lines() {
            for text in ["", "a", "a\nb\n", &"x\n".repeat(10)] {
                assert_eq!(tail(text, 10), text, "text: {text:?}");
            }
        }

        #[test]
        fn tail_more_than_lines_should_keep_last_lines() {
            let text = (1..=11).map(|line| format!("{line}\n")).collect::<String>();

            assert_eq!(
                tail(&text, 10),
                "(1 lines omitted)\n2\n3\n4\n5\n6\n7\n8\n9\n10\n11\n"
            );
            assert_eq!(tail(&text, 2), "(9 lines omitted)\n10\n11\n");
        }
    }

    mod command_line {
        use super::*;

        #[test]
        fn command_line_should_join_program_and_args() {
            let mut command = Command::new("git");
            command.args(["commit", "-m", "message"]);

            assert_eq!(command_line(&command), "git commit -m message");
        }
    }
}
