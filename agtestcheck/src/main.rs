mod check;
mod source_map;

use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;

use agplatform::DirEntry;
use agplatform::Env;
use agplatform::Fs;
use agplatform::Platform;
use agplatform::platform;

use crate::check::check;
use crate::source_map::source_map;

fn main() -> ExitCode {
    run(&platform(), &mut std::io::stdout())
}

fn run(p: &impl Platform, out: &mut dyn Write) -> ExitCode {
    let issues = check(&source_map(p, &sources(p)));

    for issue in &issues {
        writeln!(
            out,
            "{}:{}: {}",
            issue.file.display(),
            issue.line,
            issue.kind
        )
        .expect("writing the issue should succeed");
    }

    if issues.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn sources(p: &impl Platform) -> Vec<PathBuf> {
    let mut sources = Vec::new();

    if let Some(path) = p.env().args().nth(1) {
        if is_rust_source(path) {
            if p.fs().open_read(path).is_ok() {
                sources.push(path.into());
            }
        } else if let Ok(entries) = p.fs().read_dir(path) {
            sources = read_sources_in(entries, p);
        }
    } else if let Ok(entries) = p.fs().read_dir(".") {
        sources = read_sources_in(entries, p);
    }

    sources
}

fn read_sources_in<T: IntoIterator<Item = DirEntry>>(dir: T, p: &impl Platform) -> Vec<PathBuf> {
    let mut entries = Vec::new();

    dir.into_iter()
        .for_each(|entry| entries.extend(read_dir_entry(entry, p)));

    entries
}

fn read_dir_entry(entry: DirEntry, p: &impl Platform) -> Vec<PathBuf> {
    match entry.file_type() {
        agplatform::FileType::File if is_rust_source(entry.path()) => {
            vec![PathBuf::from(entry.path())]
        }
        agplatform::FileType::Dir if is_integration_tests(&entry, p) => Vec::new(),
        agplatform::FileType::Dir => read_sources_in(
            p.fs()
                .read_dir(entry.path())
                .expect("reading subdir should succeed"),
            p,
        ),
        _ => Vec::new(),
    }
}

fn is_integration_tests(entry: &DirEntry, p: &impl Platform) -> bool {
    entry.file_name() == "tests"
        && entry
            .path()
            .parent()
            .is_some_and(|dir| p.fs().open_read(dir.join("Cargo.toml")).is_ok())
}

fn is_rust_source<P: AsRef<Path>>(path: P) -> bool {
    path.as_ref().extension().and_then(|s| s.to_str()) == Some("rs")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agplatform::test_platform::test_platform;

    mod main {
        use super::*;

        #[test]
        fn main_should_check_current_dir() {
            let _ = main();
        }
    }

    mod run {
        use super::*;

        #[test]
        fn run_without_issues_should_succeed_and_print_nothing() {
            let mut platform = test_platform();
            platform.fs_mut().create_dir_all("src").unwrap();
            platform
                .fs_mut()
                .write(
                    "src/lib.rs",
                    "fn f() {}\n#[cfg(test)]\nmod tests {\n    mod f {\n        #[test]\n        fn f_should_work() {}\n    }\n}\n",
                )
                .unwrap();
            let mut out = Vec::new();

            assert_eq!(run(&platform, &mut out), ExitCode::SUCCESS);
            assert_eq!(String::from_utf8(out).unwrap(), "");
        }

        #[test]
        fn run_with_issues_should_fail_and_print_them() {
            let mut platform = test_platform();
            platform.fs_mut().create_dir_all("src").unwrap();
            platform
                .fs_mut()
                .write(
                    "src/lib.rs",
                    "#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n",
                )
                .unwrap();
            let mut out = Vec::new();

            assert_eq!(run(&platform, &mut out), ExitCode::FAILURE);
            assert_eq!(
                String::from_utf8(out).unwrap(),
                "src/lib.rs:4: test `t` should be in the module of its type, trait, function or method\n"
            );
        }
    }

    mod sources {
        use agplatform::TestEnv;

        use super::*;

        #[test]
        fn sources_no_args_should_return_all_sources() {
            let mut platform = test_platform();
            platform.fs_mut().create_dir_all("src/check").unwrap();
            platform
                .fs_mut()
                .write("src/main.rs", b"fn main() {}")
                .unwrap();
            platform.fs_mut().write("src/check/check.rs", b"").unwrap();

            let mut result = sources(&platform);
            result.sort();
            assert_eq!(
                result,
                vec![
                    PathBuf::from("src/check/check.rs"),
                    PathBuf::from("src/main.rs"),
                ]
            );
        }

        #[test]
        fn sources_with_arg_should_return_specific_source() {
            let mut platform =
                test_platform().with_env(TestEnv::new().with_args(vec!["program", "src/main.rs"]));
            platform.fs_mut().create_dir_all("src/check").unwrap();
            platform
                .fs_mut()
                .write("src/main.rs", b"fn main() {}")
                .unwrap();
            platform.fs_mut().write("src/check/check.rs", b"").unwrap();

            let result = sources(&platform);
            assert_eq!(result, vec![PathBuf::from("src/main.rs")]);
        }

        #[test]
        fn sources_with_dir_arg_should_return_specific_source() {
            let mut platform =
                test_platform().with_env(TestEnv::new().with_args(vec!["program", "src/check"]));
            platform.fs_mut().create_dir_all("src/check").unwrap();
            platform
                .fs_mut()
                .write("src/main.rs", b"fn main() {}")
                .unwrap();
            platform.fs_mut().write("src/check/check.rs", b"").unwrap();

            let result = sources(&platform);
            assert_eq!(result, vec![PathBuf::from("src/check/check.rs")]);
        }

        #[test]
        fn sources_with_arg_should_ignore_dir_with_rs_suffix() {
            let mut platform =
                test_platform().with_env(TestEnv::new().with_args(vec!["program", "src/check.rs"]));
            platform.fs_mut().create_dir_all("src/check.rs").unwrap();
            platform
                .fs_mut()
                .write("src/main.rs", b"fn main() {}")
                .unwrap();
            let result = sources(&platform);
            assert_eq!(result, Vec::<PathBuf>::new());
        }

        #[test]
        fn sources_with_nonexistent_arg_should_return_no_sources() {
            let platform = test_platform()
                .with_env(TestEnv::new().with_args(vec!["program", "src/nonexistent"]));
            let result = sources(&platform);
            assert_eq!(result, Vec::<PathBuf>::new());
        }

        #[test]
        fn sources_should_return_no_sources_when_none_exist() {
            let mut platform = test_platform();
            platform.fs_mut().create_dir_all("src/check").unwrap();
            platform
                .fs_mut()
                .write("src/main.cpp", b"fn main() {}")
                .unwrap();
            platform.fs_mut().write("src/check/check.cpp", b"").unwrap();
            let result = sources(&platform);
            assert_eq!(result, Vec::<PathBuf>::new());
        }

        #[test]
        fn sources_should_ignore_dir_with_rs_suffix() {
            let mut platform = test_platform();
            platform.fs_mut().create_dir_all("src/check.rs").unwrap();
            platform
                .fs_mut()
                .write("src/main.rs", b"fn main() {}")
                .unwrap();
            let result = sources(&platform);
            assert_eq!(result, vec![PathBuf::from("src/main.rs")]);
        }

        #[test]
        fn sources_should_skip_integration_tests_of_crates() {
            let mut platform = test_platform();

            for dir in [
                "tests",
                "agplatform/tests",
                "agplatform/src",
                "src/parser/tests",
                "fixtures/tests",
            ] {
                platform.fs_mut().create_dir_all(dir).unwrap();
            }

            for file in [
                "Cargo.toml",
                "tests/root_test.rs",
                "agplatform/Cargo.toml",
                "agplatform/tests/platform_test.rs",
                "agplatform/src/lib.rs",
                "src/parser/tests/mod.rs",
                "fixtures/tests/x.rs",
            ] {
                platform.fs_mut().write(file, b"").unwrap();
            }

            let mut result = sources(&platform);
            result.sort();
            assert_eq!(
                result,
                vec![
                    PathBuf::from("agplatform/src/lib.rs"),
                    PathBuf::from("fixtures/tests/x.rs"),
                    PathBuf::from("src/parser/tests/mod.rs"),
                ]
            );
        }

        #[test]
        fn sources_no_args_should_ignore_missing_current_dir() {
            let mut platform = test_platform();
            platform.env_mut().set_current_dir("/tmp");
            let result = sources(&platform);
            assert_eq!(result, Vec::<PathBuf>::new());
        }
    }
}
