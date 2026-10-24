use std::collections::VecDeque;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use crate::CommandOptions;
use crate::CommandOutput;
use crate::CommandRun;
use crate::Process;
use crate::ProcessObserver;
use crate::Result;
use crate::platform::platform_env::CurrentDirSetter;
use crate::platform::platform_env::EnvVarSetter;
use crate::process::StdObserver;
use crate::process::change_var;
use crate::process::check;
use crate::process::command_line;
use crate::process::io_error;
use crate::process::program;
use crate::process::run_attempts;
use crate::test_platform::test_fs::ROOT;

/// Enabled by the `testing` feature flag.
///
/// A memory-only [`crate::Process`] implementation for tests. No real
/// process is ever spawned. Commands are matched by their command line
/// (the program and the arguments separated by spaces) against the
/// patterns set with [`TestProcess::set_output`] and
/// [`TestProcess::set_io_error`].
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
    pub(crate) current_dir: PathBuf,
    var_changes: Vec<(String, Option<String>)>,
    data: Mutex<TestProcessData>,
    observer: Arc<dyn ProcessObserver>,
}

/// Enabled by the `testing` feature flag.
///
/// A command run by [`TestProcess`], see [`TestProcess::call_details`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestCall {
    /// The program and the arguments separated by spaces.
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
    responses: Vec<(
        String,
        VecDeque<std::result::Result<CommandOutput, std::io::ErrorKind>>,
    )>,
    calls: Vec<TestCall>,
}

impl TestProcess {
    /// Creates a new test process interface whose current directory is
    /// the [`ROOT`] of [`TestFs`](crate::TestFs).
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
        Self {
            current_dir: PathBuf::from(ROOT),
            var_changes: Vec::new(),
            data: Mutex::new(TestProcessData::default()),
            observer: Arc::new(StdObserver),
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

    /// Sets the `output` of the commands whose command line starts with
    /// `pattern`. When more patterns match, the most recently set one
    /// (including by [`Self::set_io_error`]) wins, so an empty pattern
    /// set first acts as a default.
    ///
    /// A [`CommandOutput::duration`] longer than the
    /// [`CommandOptions::timeout`] of a command makes it time out, without
    /// waiting. The output with non-zero status will return an error with
    /// the output attached to it that can be retrieved via [`Error::command_output`].
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
        self.data()
            .responses
            .push((pattern.into(), VecDeque::from([Ok(output)])));
    }

    /// Sets the `outputs` of the commands whose command line starts with
    /// `pattern`, returned one per run in order with the last one repeated,
    /// e.g. to make a command fail before it succeeds on a retry. Patterns
    /// match as in [`Self::set_output`]. No `outputs` set nothing.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use agplatform::{CommandExt, CommandOutput, TestProcess};
    /// use std::process::Command;
    /// use std::time::Duration;
    ///
    /// let process = TestProcess::new();
    /// let failure = CommandOutput { status: Some(1), ..Default::default() };
    /// let success = CommandOutput { status: Some(0), ..Default::default() };
    /// process.set_outputs("curl", [failure, success]);
    ///
    /// let output = Command::new("curl").retry(3, Duration::from_secs(1)).run(&process).unwrap();
    ///
    /// assert_eq!(output.status, Some(0));
    /// assert_eq!(process.calls(), ["curl", "curl"]);
    /// ```
    pub fn set_outputs<T: Into<String>, I: IntoIterator<Item = CommandOutput>>(
        &self,
        pattern: T,
        outputs: I,
    ) {
        let outputs: VecDeque<_> = outputs.into_iter().map(Ok).collect();

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
        self.data()
            .responses
            .push((pattern.into(), VecDeque::from([Err(kind)])));
    }

    fn respond(&self, run: &CommandRun) -> Result<CommandOutput> {
        let (command, options) = (run.command(), run.options());
        let command_line = command_line(command);
        let program = program(command);
        let mut vars = if options.clear_env {
            Vec::new()
        } else {
            self.var_changes.clone()
        };

        for (key, value) in command.get_envs() {
            change_var(
                &mut vars,
                key.to_string_lossy().into_owned(),
                value.map(|value| value.to_string_lossy().into_owned()),
            );
        }

        let mut data = self.data();
        data.calls.push(TestCall {
            command_line: command_line.clone(),
            current_dir: run.current_dir().unwrap_or(&self.current_dir).to_path_buf(),
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
            .ok_or_else(|| process_error!("'{command_line}': no output set"))?;
        drop(data);

        let output = response.map_err(|kind| io_error(&program, kind.into()))?;
        let output = CommandOutput {
            display_lines: options.display_lines,
            ..output
        };
        let output = if options.silent || options.interactive {
            CommandOutput {
                stdout: Vec::new(),
                stderr: Vec::new(),
                ..output
            }
        } else {
            output
        };

        if options.stream && !output.stdout.is_empty() {
            self.observer.stdout(run, &output.stdout);
        }

        if options.stream && !output.stderr.is_empty() {
            self.observer.stderr(run, &output.stderr);
        }

        match options.timeout {
            Some(timeout) if output.duration > timeout => check(
                &program,
                CommandOutput {
                    status: None,
                    duration: timeout,
                    timed_out: true,
                    ..output
                },
            ),
            _ => check(&program, output),
        }
    }

    fn data(&self) -> std::sync::MutexGuard<'_, TestProcessData> {
        self.data.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl CurrentDirSetter for TestProcess {
    fn set_current_dir(&mut self, path: PathBuf) {
        self.current_dir = path;
    }
}

impl EnvVarSetter for TestProcess {
    fn set_var(&mut self, key: String, value: String) {
        change_var(&mut self.var_changes, key, Some(value));
    }

    fn remove_var(&mut self, key: &str) {
        change_var(&mut self.var_changes, key.to_string(), None);
    }
}

impl Process for TestProcess {
    /// Records the command and returns the output set for it, without
    /// reading the input. With [`CommandOptions::stream`] the non-empty output
    /// set is reported to the observer at once. With
    /// [`CommandOptions::silent`] or [`CommandOptions::interactive`] the
    /// output set is dropped, as no output would be captured.
    ///
    /// See [`crate::Process::run`].
    ///
    /// # Errors
    ///
    /// Returns an error if no output was set for the command, an I/O error
    /// was set for it, or as [`crate::Process::run`] for the options and
    /// the output set.
    fn run(&self, command: &Command, options: &CommandOptions) -> Result<CommandOutput> {
        // WHY: retries are not waited for, like timeouts.
        run_attempts(
            self.observer.as_ref(),
            CommandRun::observed(command, options, &self.current_dir),
            &mut |run| self.respond(run),
            |_| {},
        )
    }

    /// Sets the observer of the runs, replacing the previous one.
    ///
    /// See [`crate::Process::set_observer`].
    fn set_observer(&mut self, observer: Arc<dyn ProcessObserver>) {
        self.observer = observer;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CommandExt;
    use std::time::Duration;

    fn exit(status: i32) -> CommandOutput {
        CommandOutput {
            status: Some(status),
            ..Default::default()
        }
    }

    #[derive(Default)]
    struct RecordingObserver(Mutex<Vec<String>>);

    impl ProcessObserver for RecordingObserver {
        fn started(&self, run: &CommandRun) {
            self.0.lock().unwrap().push(format!(
                "started {} in {}",
                command_line(run.command()),
                run.current_dir().unwrap().display()
            ));
        }

        fn stdout(&self, _run: &CommandRun, data: &[u8]) {
            self.0
                .lock()
                .unwrap()
                .push(format!("Stdout {}", String::from_utf8_lossy(data)));
        }

        fn stderr(&self, _run: &CommandRun, data: &[u8]) {
            self.0
                .lock()
                .unwrap()
                .push(format!("Stderr {}", String::from_utf8_lossy(data)));
        }

        fn finished(&self, _run: &CommandRun, result: &Result<CommandOutput>) {
            self.0
                .lock()
                .unwrap()
                .push(format!("finished {}", result.is_ok()));
        }
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
                let mut process = TestProcess::new();
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
                let mut process = TestProcess::new();
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
                let output = Command::new("curl")
                    .retry(2, Duration::from_secs(3600))
                    .run(&process)
                    .unwrap();

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

        mod current_dir_setter {
            use super::*;

            #[test]
            fn set_current_dir_should_update_current_directory() {
                let mut process = TestProcess::new();

                process.set_current_dir(PathBuf::from("/tmp"));

                assert_eq!(process.current_dir, PathBuf::from("/tmp"));
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
                    // GIVEN an observer and an output with both streams
                    let observer = Arc::new(RecordingObserver::default());
                    let mut process = TestProcess::new();
                    process.set_observer(observer.clone());
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
                        *observer.0.lock().unwrap(),
                        [
                            format!("started git in {ROOT}"),
                            "finished true".to_string()
                        ]
                    );
                }

                #[test]
                fn run_streaming_should_report_output_set_to_observer() {
                    // GIVEN an observer and an output with both streams
                    let observer = Arc::new(RecordingObserver::default());
                    let mut process = TestProcess::new();
                    process.set_observer(observer.clone());
                    process.set_output(
                        "git",
                        CommandOutput {
                            stdout: b"out".to_vec(),
                            stderr: b"err".to_vec(),
                            ..exit(0)
                        },
                    );

                    // WHEN streaming a matching command
                    Command::new("git").stream().run(&process).unwrap();

                    // THEN the observer gets the output between start and end
                    assert_eq!(
                        *observer.0.lock().unwrap(),
                        [
                            format!("started git in {ROOT}"),
                            "Stdout out".to_string(),
                            "Stderr err".to_string(),
                            "finished true".to_string()
                        ]
                    );
                }
            }

            mod set_observer {
                use super::*;

                #[test]
                fn set_observer_should_report_runs_in_resolved_dir() {
                    let observer = Arc::new(RecordingObserver::default());
                    let mut process = TestProcess::new();
                    process.set_observer(observer.clone());

                    let _ = Command::new("git").current_dir("repo").run(&process);

                    assert_eq!(
                        *observer.0.lock().unwrap(),
                        [
                            format!(
                                "started git in {}",
                                PathBuf::from(ROOT).join("repo").display()
                            ),
                            "finished false".to_string()
                        ]
                    );
                }
            }
        }
    }
}
