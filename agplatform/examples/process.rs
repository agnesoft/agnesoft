//! A release helper running `git` and `cargo` as child processes.
//!
//! The helper takes `&impl Process`, so its tests run against a
//! `TestProcess` that never spawns anything. Each test sets the output of
//! the commands it expects and then checks which commands ran, with which
//! arguments, input and options. That covers retries, timeouts and missing
//! programs, all deterministic and without waiting.
//!
//! ```bash
//! cargo run --example process --features process -- branch
//! cargo run --example process --features process -- build
//! ```

use std::io::Write;
use std::process::Command;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use agplatform::CommandExt;
use agplatform::CommandOutput;
use agplatform::CommandRun;
use agplatform::Env;
use agplatform::Platform;
use agplatform::Process;
use agplatform::ProcessObserver;

/// Calls the process directly, which is equivalent to running the command
/// with `CommandExt::run` like the other functions.
fn current_branch(process: &impl Process) -> agplatform::Result<String> {
    let output = process.run(Command::new("git").args(["rev-parse", "--abbrev-ref", "HEAD"]))?;
    Ok(output.stdout_str()?.trim().to_string())
}

/// Commits the staged changes, passing `message` on the standard input so
/// that it may hold any characters.
fn commit(process: &impl Process, message: &str) -> agplatform::Result<()> {
    Command::new("git")
        .args(["commit", "--file", "-"])
        .input(message)
        .run(process)?;
    Ok(())
}

/// Pushes the current branch, retrying only the failures caused by the
/// network, not e.g. a rejected push. Each attempt is limited to a minute.
fn push(process: &impl Process) -> agplatform::Result<()> {
    Command::new("git")
        .arg("push")
        .timeout(Duration::from_secs(60))
        .retry(3, Duration::from_secs(5))
        .retry_if(|_, result| {
            result
                .as_ref()
                .err()
                .and_then(agplatform::Error::command_output)
                .is_some_and(|output| output.stderr_lossy().contains("Could not resolve host"))
        })
        .run(process)?;
    Ok(())
}

/// Returns whether `program` can be started, i.e. is installed.
fn installed(process: &impl Process, program: &str) -> bool {
    match Command::new(program).arg("--version").silent().run(process) {
        Ok(_) => true,
        Err(error) => error.command_output().is_some(),
    }
}

/// Builds the project, showing its output live through the observer of
/// `process`.
fn build(process: &impl Process) -> agplatform::Result<CommandOutput> {
    Command::new("cargo")
        .args(["build", "--release"])
        .stream()
        .run(process)
}

/// Writes the streamed output of the commands to `W` with each line
/// prefixed by the program.
struct Prefixed<W>(Mutex<W>);

impl<W: Write + Send> ProcessObserver for Prefixed<W> {
    fn stdout(&self, run: &CommandRun, data: &[u8]) {
        let mut out = self.0.lock().unwrap();
        for line in String::from_utf8_lossy(data).lines() {
            let _ = writeln!(out, "[{}] {line}", run.program());
        }
    }

    fn stderr(&self, run: &CommandRun, data: &[u8]) {
        self.stdout(run, data);
    }
}

fn main() -> agplatform::Result<()> {
    let mut platform = agplatform::platform();
    platform
        .process_mut()
        .set_observer(Arc::new(Prefixed(Mutex::new(std::io::stdout()))));
    let process = platform.process();
    let args: Vec<&str> = platform.env().args().skip(1).map(String::as_str).collect();

    match args.as_slice() {
        ["branch"] => println!("{}", current_branch(process)?),
        ["commit", message] => commit(process, message)?,
        ["push"] => push(process)?,
        ["build"] => println!("built in {:?}", build(process)?.duration),
        _ => println!(
            "usage: process branch|commit <message>|push|build (git installed: {})",
            installed(process, "git")
        ),
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agplatform::TestProcess;

    fn exit(status: i32, stderr: &str) -> CommandOutput {
        CommandOutput {
            status: Some(status),
            stderr: stderr.as_bytes().to_vec(),
            ..Default::default()
        }
    }

    mod current_branch {
        use super::*;

        #[test]
        fn current_branch_should_return_trimmed_stdout_of_git() {
            let process = TestProcess::new();
            process.set_output(
                "git rev-parse",
                CommandOutput {
                    status: Some(0),
                    stdout: b"main\n".to_vec(),
                    ..Default::default()
                },
            );

            let branch = current_branch(&process).unwrap();

            assert_eq!(branch, "main");
            assert_eq!(process.calls(), ["git rev-parse --abbrev-ref HEAD"]);
        }

        #[test]
        fn current_branch_outside_repository_should_fail_with_git_output() {
            let process = TestProcess::new();
            process.set_output("git", exit(128, "fatal: not a git repository"));

            let error = current_branch(&process).unwrap_err();

            let output = error.command_output().unwrap();
            assert_eq!(output.status, Some(128));
            assert_eq!(output.stderr_str().unwrap(), "fatal: not a git repository");
        }
    }

    mod commit {
        use super::*;

        #[test]
        fn commit_should_pass_message_on_stdin() {
            let process = TestProcess::new();
            process.set_output("git commit", exit(0, ""));

            commit(&process, "Fix \"quotes\" and $vars").unwrap();

            let call = &process.call_details()[0];
            assert_eq!(call.command_line, "git commit --file -");
            assert_eq!(
                call.options.input.as_deref(),
                Some(b"Fix \"quotes\" and $vars".as_slice())
            );
        }
    }

    mod push {
        use super::*;

        #[test]
        fn push_should_retry_network_failures() {
            // GIVEN a push failing twice to reach the remote
            let process = TestProcess::new();
            let offline = exit(128, "fatal: Could not resolve host: github.com");
            process.set_outputs("git push", [offline.clone(), offline, exit(0, "")]);

            // WHEN pushing
            let result = push(&process);

            // THEN it succeeds on the third attempt, without waiting for the
            // retry delays
            assert!(result.is_ok());
            assert_eq!(process.calls(), ["git push", "git push", "git push"]);
        }

        #[test]
        fn push_rejected_should_fail_without_retrying() {
            let process = TestProcess::new();
            process.set_output(
                "git push",
                exit(1, "! [rejected] main -> main (fetch first)"),
            );

            let error = push(&process).unwrap_err();

            assert!(
                error
                    .command_output()
                    .unwrap()
                    .stderr_lossy()
                    .contains("rejected")
            );
            assert_eq!(process.calls().len(), 1);
        }

        #[test]
        fn push_hanging_should_time_out() {
            // GIVEN a push that would take longer than its timeout
            let process = TestProcess::new();
            process.set_output(
                "git push",
                CommandOutput {
                    status: Some(0),
                    duration: Duration::from_secs(3600),
                    ..Default::default()
                },
            );

            // WHEN pushing
            let error = push(&process).unwrap_err();

            // THEN it times out at once without being retried
            assert!(error.command_output().unwrap().timed_out);
            assert_eq!(
                process.call_details()[0].options.timeout,
                Some(Duration::from_secs(60))
            );
            assert_eq!(process.calls().len(), 1);
        }
    }

    mod installed {
        use super::*;

        #[test]
        fn installed_should_tell_whether_program_starts() {
            let process = TestProcess::new();
            process.set_output("git", exit(0, ""));
            process.set_output("hg", exit(1, ""));
            process.set_io_error("svn", std::io::ErrorKind::NotFound);

            for (program, expected) in [("git", true), ("hg", true), ("svn", false)] {
                assert_eq!(installed(&process, program), expected, "program: {program}");
            }
        }
    }

    mod build {
        use super::*;

        #[test]
        fn build_should_stream_output() {
            let process = TestProcess::new();
            process.set_output(
                "cargo build",
                CommandOutput {
                    status: Some(0),
                    stderr: b"Compiling app\nFinished".to_vec(),
                    ..Default::default()
                },
            );

            build(&process).unwrap();

            assert!(process.call_details()[0].options.stream);
        }
    }

    mod prefixed {
        use super::*;

        mod process_observer {
            use super::*;

            mod stdout {
                use super::*;

                #[test]
                fn stdout_should_prefix_lines_with_program() {
                    // GIVEN the observer installed on the process
                    let observer = Arc::new(Prefixed(Mutex::new(Vec::new())));
                    let mut process = TestProcess::new();
                    process.set_observer(observer.clone());
                    process.set_output(
                        "cargo",
                        CommandOutput {
                            status: Some(0),
                            stdout: b"Compiling app\nFinished".to_vec(),
                            ..Default::default()
                        },
                    );

                    // WHEN a command streams its output
                    Command::new("cargo").stream().run(&process).unwrap();

                    // THEN each line is written with the program
                    assert_eq!(
                        String::from_utf8(observer.0.lock().unwrap().clone()).unwrap(),
                        "[cargo] Compiling app\n[cargo] Finished\n"
                    );
                }
            }

            mod stderr {
                use super::*;

                #[test]
                fn stderr_should_prefix_lines_with_program() {
                    let observer = Prefixed(Mutex::new(Vec::new()));
                    let command = Command::new("cargo");

                    observer.stderr(&CommandRun::new(&command), b"warning: unused");

                    assert_eq!(
                        String::from_utf8(observer.0.lock().unwrap().clone()).unwrap(),
                        "[cargo] warning: unused\n"
                    );
                }
            }
        }
    }
}
