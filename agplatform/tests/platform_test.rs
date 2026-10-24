use agplatform::CommandExt;
use agplatform::Env;
use agplatform::Platform;
use agplatform::Process;

#[cfg(feature = "testing")]
mod test_platform_test;

#[cfg(unix)]
fn print_var(name: &str) -> std::process::Command {
    let mut command = std::process::Command::new("sh");
    command.args(["-c", &format!("echo \"${{{name}-unset}}\"")]);
    command
}

#[cfg(windows)]
fn print_var(name: &str) -> std::process::Command {
    let mut command = std::process::Command::new("cmd");
    command.args([
        "/C",
        &format!("if defined {name} (echo %{name}%) else (echo unset)"),
    ]);
    command
}

mod platform {
    use super::*;

    #[test]
    fn platform_should_expose_public_api() {
        agplatform::Error::fs("fs error");
        agplatform::Error::env("env error");
        agplatform::Error::process("process error");
        agplatform::Error::request("request error");

        let mut platform = agplatform::platform();
        platform.env();
        platform.env_mut();
        platform.fs();
        platform.fs_mut();
        platform.process();
        platform.process_mut();
        platform.request();
    }

    #[test]
    fn platform_should_run_commands() {
        let platform = agplatform::platform();

        let output = std::process::Command::new(env!("CARGO"))
            .arg("--version")
            .timeout(std::time::Duration::from_secs(60))
            .run(platform.process())
            .unwrap();

        assert!(output.stdout_str().unwrap().starts_with("cargo "));
    }

    #[test]
    fn platform_should_pass_env_vars_to_processes() {
        // GIVEN a variable set and another one removed from the environment
        let mut platform = agplatform::platform();
        platform.env_mut().set_var("AGPLATFORM_SET", "value");
        platform.env_mut().set_var("AGPLATFORM_REMOVED", "value");
        platform.env_mut().remove_var("AGPLATFORM_REMOVED");

        // WHEN printing them from child processes
        let set = print_var("AGPLATFORM_SET").run(platform.process()).unwrap();
        let removed = print_var("AGPLATFORM_REMOVED")
            .run(platform.process())
            .unwrap();

        // THEN the child processes see the environment
        assert_eq!(set.stdout_str().unwrap().trim(), "value");
        assert_eq!(removed.stdout_str().unwrap().trim(), "unset");
    }

    #[test]
    fn platform_should_report_runs_to_process_observer() {
        // GIVEN an observer collecting failures on the platform
        #[derive(Default)]
        struct Failures(std::sync::Mutex<Vec<String>>);

        impl agplatform::ProcessObserver for Failures {
            fn finished(
                &self,
                _run: &agplatform::CommandRun,
                result: &agplatform::Result<agplatform::CommandOutput>,
            ) {
                if let Err(error) = result {
                    self.0.lock().unwrap().push(error.description().to_string());
                }
            }
        }

        let failures = std::sync::Arc::new(Failures::default());
        let mut platform = agplatform::platform();
        platform.process_mut().set_observer(failures.clone());

        // WHEN a command fails to start
        let _ = std::process::Command::new("agplatform_missing_program").run(platform.process());

        // THEN the observer sees the failure
        assert_eq!(failures.0.lock().unwrap().len(), 1);
        assert!(failures.0.lock().unwrap()[0].starts_with("'agplatform_missing_program': "));
    }
}
