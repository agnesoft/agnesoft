use std::path::PathBuf;

use agplatform::DirEntry;
use agplatform::Env;
use agplatform::Fs;
use agplatform::Platform;

fn main() {}

#[allow(dead_code)]
fn sources(p: &impl Platform) -> Vec<PathBuf> {
    let mut sources = Vec::new();

    if let Some(file_or_dir) = p.env().args().nth(1) {
        let path = PathBuf::from(file_or_dir);
        if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            if p.fs().open_read(file_or_dir).is_ok() {
                sources.push(path);
            }
        } else if let Ok(entries) = p.fs().read_dir(file_or_dir) {
            sources = read_sources_in(entries, p);
        }
    } else {
        sources = read_sources_in(
            p.fs()
                .read_dir(".")
                .expect("reading current_dir should succeed"),
            p,
        );
    }

    sources
}

fn read_sources_in<T: IntoIterator<Item = DirEntry>>(dir: T, p: &impl Platform) -> Vec<PathBuf> {
    let mut entries = Vec::new();

    for entry in dir {
        if let Ok(read_dir) = p.fs().read_dir(entry.path()) {
            entries.extend(read_sources_in(read_dir, p));
        } else if entry.path().extension().and_then(|s| s.to_str()) == Some("rs") {
            entries.push(PathBuf::from(entry.path()));
        }
    }

    entries
}

#[cfg(test)]
mod tests {
    use agplatform::test_platform::test_platform;

    use super::*;

    #[test]
    fn it_works() {
        main();
    }

    mod sources_fn {
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
    }
}
