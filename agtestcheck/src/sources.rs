use std::path::Path;
use std::path::PathBuf;

use agplatform::DirEntry;
use agplatform::Env;
use agplatform::Fs;
use agplatform::Platform;

pub(crate) fn sources(p: &impl Platform) -> Vec<PathBuf> {
    let mut sources = Vec::new();
    let mut args = p.env().args().skip(1).peekable();
    args.next_if(|arg| *arg == "test-check");

    if let Some(path) = args.next() {
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

    mod sources {
        use super::*;
        use agplatform::TestEnv;

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
        fn sources_as_cargo_subcommand_should_return_all_sources() {
            let mut platform =
                test_platform().with_env(TestEnv::new().with_args(vec!["program", "test-check"]));
            platform.fs_mut().create_dir_all("src").unwrap();
            platform
                .fs_mut()
                .write("src/main.rs", b"fn main() {}")
                .unwrap();

            let result = sources(&platform);
            assert_eq!(result, vec![PathBuf::from("src/main.rs")]);
        }

        #[test]
        fn sources_as_cargo_subcommand_with_arg_should_return_specific_source() {
            let mut platform = test_platform().with_env(TestEnv::new().with_args(vec![
                "program",
                "test-check",
                "src/main.rs",
            ]));
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
