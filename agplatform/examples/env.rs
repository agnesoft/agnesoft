//! Configuration of a command line tool read from its arguments and
//! environment.
//!
//! The tool reads them through `&impl Platform` instead of `std::env`, so
//! its tests give it a `TestEnv` with exactly the arguments, variables and
//! directories they need. They never touch the environment of the test
//! process, which is shared by all tests running in parallel.
//!
//! ```bash
//! cargo run --example env --features env -- --verbose
//! ```

use std::path::PathBuf;

use agplatform::Env;
use agplatform::Platform;

#[derive(Debug, PartialEq)]
struct Config {
    config_dir: PathBuf,
    verbose: bool,
    files: Vec<String>,
}

fn config(platform: &impl Platform) -> agplatform::Result<Config> {
    let env = platform.env();
    let config_dir = match env.var("XDG_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => env.home_dir().join(".config"),
    };
    let (flags, files): (Vec<_>, Vec<_>) = env
        .args()
        .skip(1)
        .cloned()
        .partition(|arg| arg.starts_with("--"));

    Ok(Config {
        config_dir: config_dir.join("tool"),
        verbose: flags.iter().any(|flag| flag == "--verbose") || env.var_bool("TOOL_VERBOSE")?,
        files,
    })
}

/// Makes `workspace` the current directory of the tool and exports it to
/// the processes it starts. Returns the previous current directory.
fn enter_workspace(platform: &impl Platform, workspace: PathBuf) -> PathBuf {
    let env = platform.env();
    env.set_var("TOOL_WORKSPACE", workspace.to_string_lossy());
    env.set_current_dir(workspace)
}

fn main() -> agplatform::Result<()> {
    let platform = agplatform::platform();
    let config = config(&platform)?;
    println!("{config:?}");

    let workspace = platform.env().tmp_dir().to_path_buf();
    let previous = enter_workspace(&platform, workspace);
    println!(
        "moved from {} to {}",
        previous.display(),
        platform.env().current_dir().display()
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agplatform::TestEnv;
    use agplatform::test_platform::test_platform;

    mod config {
        use super::*;

        #[test]
        fn config_should_split_arguments_into_flags_and_files() {
            let platform = test_platform().with_env(TestEnv::new().with_args(vec![
                "tool",
                "a.txt",
                "--verbose",
                "b.txt",
            ]));

            let config = config(&platform).unwrap();

            assert!(config.verbose);
            assert_eq!(config.files, ["a.txt", "b.txt"]);
        }

        #[test]
        fn config_should_prefer_xdg_config_home_over_home_dir() {
            for (vars, expected) in [
                (vec![], "/home/user/.config/tool"),
                (vec![("XDG_CONFIG_HOME", "")], "/home/user/.config/tool"),
                (vec![("XDG_CONFIG_HOME", "/xdg")], "/xdg/tool"),
            ] {
                let env = TestEnv::new()
                    .with_home_dir("/home/user")
                    .with_vars(vars.clone());
                let platform = test_platform().with_env(env);

                let config = config(&platform).unwrap();

                assert_eq!(config.config_dir, PathBuf::from(expected), "vars: {vars:?}");
            }
        }

        #[test]
        fn config_should_read_verbose_from_variable() {
            for (value, expected) in [("1", true), ("on", true), ("false", false)] {
                let env = TestEnv::new().with_vars(vec![("TOOL_VERBOSE", value)]);
                let platform = test_platform().with_env(env);

                let config = config(&platform).unwrap();

                assert_eq!(config.verbose, expected, "TOOL_VERBOSE: {value}");
            }
        }

        #[test]
        fn config_invalid_verbose_variable_should_fail() {
            let env = TestEnv::new().with_vars(vec![("TOOL_VERBOSE", "maybe")]);
            let platform = test_platform().with_env(env);

            let error = config(&platform).unwrap_err();

            assert_eq!(error.kind(), agplatform::ErrorKind::Env);
            assert!(error.description().contains("TOOL_VERBOSE"));
        }
    }

    mod enter_workspace {
        use super::*;

        #[test]
        fn enter_workspace_should_change_current_dir_and_export_it() {
            let platform = test_platform().with_env(TestEnv::new().with_current_dir("/home"));

            let previous = enter_workspace(&platform, PathBuf::from("/work"));

            assert_eq!(previous, PathBuf::from("/home"));
            assert_eq!(platform.env.current_dir(), PathBuf::from("/work"));
            assert_eq!(platform.env.var("TOOL_WORKSPACE").as_deref(), Some("/work"));
        }
    }
}
