use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

/// Returns `path` resolved against `current_dir` with the `.` and `..`
/// components removed lexically, never going above the root. The leading
/// `..` of a relative result are kept, like by the fs.
pub(crate) fn resolve_path(current_dir: &Path, path: &Path) -> PathBuf {
    let mut resolved = PathBuf::new();

    for component in current_dir.join(path).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if matches!(
                    resolved.components().next_back(),
                    None | Some(Component::ParentDir)
                ) {
                    resolved.push(component);
                } else {
                    resolved.pop();
                }
            }
            component => resolved.push(component),
        }
    }

    resolved
}

/// Enabled by the `tokio` feature flag.
///
/// The future returned by the async methods of the interceptors, e.g.
/// [`ProcessInterceptor::handle_async`](crate::ProcessInterceptor::handle_async).
#[cfg(all(feature = "tokio", any(feature = "process", feature = "request")))]
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Enabled by the `process` or `request` feature flag.
///
/// The output a dry-run interceptor returns for a run instead of running
/// it, set on the run with e.g.
/// [`CommandRun::with`](crate::CommandRun::with), see
/// [`CommandDryRun`](crate::CommandDryRun).
#[cfg(any(feature = "process", feature = "request"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DryRunOutput<T>(pub T);

/// [internal]
///
/// The interceptors of a process or request, changed through a shared
/// reference for the runs starting afterwards.
#[cfg(any(feature = "process", feature = "request"))]
pub(crate) struct SharedCell<T>(std::sync::RwLock<T>);

#[cfg(any(feature = "process", feature = "request"))]
impl<T: Clone> SharedCell<T> {
    pub(crate) fn new(value: T) -> Self {
        Self(std::sync::RwLock::new(value))
    }

    /// Returns a copy of the value, kept by a run until it finishes.
    pub(crate) fn get(&self) -> T {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Changes the value with `change`.
    pub(crate) fn update(&self, change: impl FnOnce(&mut T)) {
        change(
            &mut self
                .0
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    }
}

/// [internal]
///
/// Values of distinct types attached to a run, see
/// [`CommandRun::with`](crate::CommandRun::with).
#[cfg(any(feature = "process", feature = "request"))]
#[derive(Default)]
pub(crate) struct Extensions(Vec<Box<dyn std::any::Any + Send + Sync>>);

#[cfg(any(feature = "process", feature = "request"))]
impl Extensions {
    /// Returns the value of type `T`, if any.
    pub(crate) fn get<T: std::any::Any>(&self) -> Option<&T> {
        self.0.iter().find_map(|value| value.downcast_ref())
    }

    /// Sets the value of type `T`, replacing the previous one.
    pub(crate) fn insert<T: std::any::Any + Send + Sync>(&mut self, value: T) {
        self.0.retain(|value| !value.is::<T>());
        self.0.push(Box::new(value));
    }
}

/// [internal]
///
/// The longest delay of a retry interceptor by default, see e.g.
/// [`CommandRetry::max_delay`](crate::CommandRetry::max_delay).
#[cfg(any(feature = "process", feature = "request"))]
pub(crate) const MAX_DELAY: std::time::Duration = std::time::Duration::from_secs(60);

/// [internal]
///
/// Returns the delay before a retry after `retried` retries: `delay`
/// multiplied by `backoff` for each of them, at most `max`.
#[cfg(any(feature = "process", feature = "request"))]
pub(crate) fn backoff(
    delay: std::time::Duration,
    backoff: u32,
    retried: u32,
    max: std::time::Duration,
) -> std::time::Duration {
    delay
        .saturating_mul(backoff.saturating_pow(retried))
        .min(max)
}

/// [internal]
///
/// Returns the 64-bit FNV-1a hash of `values`, each written with whether
/// it is set and its length so that different lists hash differently, the
/// same on every platform and Rust version, unlike
/// [`std::hash::DefaultHasher`].
#[cfg(feature = "process")]
pub(crate) fn fnv1a<'a>(values: impl IntoIterator<Item = Option<&'a [u8]>>) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0100_0000_01b3;

    let hash = |hash: u64, bytes: &[u8]| {
        bytes.iter().fold(hash, |hash, &byte| {
            (hash ^ u64::from(byte)).wrapping_mul(PRIME)
        })
    };

    values.into_iter().fold(OFFSET, |state, value| match value {
        Some(value) => hash(
            hash(hash(state, &[1]), &(value.len() as u64).to_le_bytes()),
            value,
        ),
        None => hash(state, &[0]),
    })
}

/// [internal]
///
/// Returns `true` if the interceptors `a` and `b` are the same instances in
/// the same order.
#[cfg(any(feature = "process", feature = "request"))]
pub(crate) fn same_interceptors<T: ?Sized>(
    a: &[std::sync::Arc<T>],
    b: &[std::sync::Arc<T>],
) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| std::sync::Arc::ptr_eq(a, b))
}

/// Returns a new id of a command or request run, unique within the process
/// and starting at `1`.
#[cfg(any(feature = "process", feature = "request"))]
pub(crate) fn next_run_id() -> u64 {
    static NEXT_RUN_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT_RUN_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Removes surrounding single or double quotes from a string slice, if present.
/// Leading and trailing whitespace is trimmed before checking for quotes.
pub(crate) fn unquote<T: AsRef<str> + ?Sized>(str: &T) -> &str {
    let str_ref = str.as_ref().trim();
    let mut chars = str_ref.chars();

    if let Some(first) = chars.next()
        && let Some(last) = chars.last()
        && ((first == '\'' && last == '\'') || (first == '"' && last == '"'))
    {
        return &str_ref[1..str_ref.len() - 1];
    }

    str_ref
}

/// Displays the last `lines` of `text` lossily, at most `lines` KiB of
/// them, preceded by the number of the lines or the bytes omitted if any.
#[cfg(any(feature = "process", feature = "request"))]
pub(crate) struct Tail<'a> {
    pub(crate) text: &'a [u8],
    pub(crate) lines: usize,
}

#[cfg(any(feature = "process", feature = "request"))]
impl std::fmt::Display for Tail<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const LINE_BYTES: usize = 1024;

        // WHY: found from the end so that only the shown part is scanned
        // for lines, the omitted ones are only counted.
        let text = self.text;
        let lines_start = match self.lines.checked_sub(1) {
            Some(last) => text
                .strip_suffix(b"\n")
                .unwrap_or(text)
                .iter()
                .enumerate()
                .rev()
                .filter(|&(_, &byte)| byte == b'\n')
                .nth(last)
                .map_or(0, |(newline, _)| newline + 1),
            None => text.len(),
        };
        let max_bytes = self.lines.saturating_mul(LINE_BYTES);

        let start = if text.len() - lines_start > max_bytes {
            let mut start = text.len() - max_bytes;

            // WHY: a UTF-8 sequence cut at the start would be replaced.
            while text.get(start).is_some_and(|byte| byte & 0xC0 == 0x80) {
                start += 1;
            }

            writeln!(f, "({start} bytes omitted)")?;
            start
        } else {
            let omitted = text[..lines_start]
                .iter()
                .filter(|&&byte| byte == b'\n')
                .count()
                + usize::from(
                    lines_start == text.len() && !text.is_empty() && !text.ends_with(b"\n"),
                );

            if omitted > 0 {
                writeln!(f, "({omitted} lines omitted)")?;
            }

            lines_start
        };

        text[start..].utf8_chunks().try_for_each(|chunk| {
            f.write_str(chunk.valid())?;

            if chunk.invalid().is_empty() {
                Ok(())
            } else {
                f.write_str("\u{FFFD}")
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod resolve_path {
        use super::*;

        #[cfg(unix)]
        const ROOT: &str = "/";
        #[cfg(windows)]
        const ROOT: &str = "C:\\";

        #[test]
        fn resolve_path_should_resolve_relative_path_against_current_dir() {
            let current_dir = Path::new(ROOT).join("cwd");

            for (path, expected) in [
                ("a", "cwd/a"),
                ("a/./b", "cwd/a/b"),
                ("./a/../b/", "cwd/b"),
                ("..", ""),
                ("../../..", ""),
                ("../a", "a"),
            ] {
                assert_eq!(
                    resolve_path(&current_dir, Path::new(path)),
                    Path::new(ROOT).join(expected),
                    "path: {path}"
                );
            }
        }

        #[test]
        fn resolve_path_should_normalize_absolute_path() {
            let current_dir = Path::new(ROOT).join("cwd");
            let path = Path::new(ROOT).join("a/./b/../c");

            assert_eq!(
                resolve_path(&current_dir, &path),
                Path::new(ROOT).join("a/c")
            );
        }

        #[test]
        fn resolve_path_empty_current_dir_should_keep_path_relative() {
            for (path, expected) in [
                ("./a", "a"),
                ("a/./b/../c", "a/c"),
                ("../a", "../a"),
                ("../../a/..", "../.."),
            ] {
                assert_eq!(
                    resolve_path(Path::new(""), Path::new(path)),
                    Path::new(expected),
                    "path: {path}"
                );
            }
        }

        #[cfg(windows)]
        #[test]
        fn resolve_path_should_resolve_windows_prefixes() {
            assert_eq!(
                resolve_path(Path::new("C:\\cwd"), Path::new("\\a\\..\\b")),
                Path::new("C:\\b")
            );
            assert_eq!(
                resolve_path(Path::new("C:\\cwd"), Path::new("D:\\a")),
                Path::new("D:\\a")
            );
        }
    }

    #[cfg(feature = "process")]
    mod fnv1a {
        use super::*;

        #[test]
        fn fnv1a_should_hash_values_with_presence_and_length() {
            let hashes = [
                fnv1a([]),
                fnv1a([None]),
                fnv1a([Some(b"".as_slice())]),
                fnv1a([Some(b"ab".as_slice())]),
                fnv1a([Some(b"a".as_slice()), Some(b"b")]),
            ];

            assert_eq!(fnv1a([]), 0xcbf2_9ce4_8422_2325);
            assert_eq!(
                hashes
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len(),
                hashes.len(),
                "{hashes:x?}"
            );
        }
    }

    mod unquote {
        use super::*;

        #[test]
        fn unquote_should_strip_matching_quotes() {
            for (input, expected) in [
                ("'test'", "test"),
                ("\"test\"", "test"),
                ("'\"test\"'", "\"test\""),
                ("\"'test'\"", "'test'"),
            ] {
                assert_eq!(unquote(input), expected, "input: {input}");
            }
        }

        #[test]
        fn unquote_should_return_unquoted_input_unchanged() {
            for input in ["test", "'test", "test\""] {
                assert_eq!(unquote(input), input, "input: {input}");
            }
        }

        #[test]
        fn unquote_should_trim_whitespace_before_checking() {
            assert_eq!(unquote(" 'test' "), "test");
        }
    }

    #[cfg(any(feature = "process", feature = "request"))]
    mod tail {
        use super::*;

        fn tail(text: &str, lines: usize) -> String {
            Tail {
                text: text.as_bytes(),
                lines,
            }
            .to_string()
        }

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

        #[test]
        fn tail_should_count_lines_as_str_lines() {
            for (text, lines, expected) in [
                ("a\nb", 1, "(1 lines omitted)\nb"),
                ("a\n\nb\n", 2, "(1 lines omitted)\n\nb\n"),
                ("\n\n", 1, "(1 lines omitted)\n\n"),
                ("a\nb", 0, "(2 lines omitted)\n"),
                ("a\nb\n", 0, "(2 lines omitted)\n"),
                ("", 0, ""),
            ] {
                assert_eq!(
                    tail(text, lines),
                    expected,
                    "text: {text:?}, lines: {lines}"
                );
            }
        }

        #[test]
        fn tail_longer_than_lines_kib_should_keep_last_bytes() {
            let text = format!("a\n{}", "b".repeat(2048));

            assert_eq!(
                tail(&text, 1),
                format!("(1026 bytes omitted)\n{}", "b".repeat(1024))
            );
        }

        #[test]
        fn tail_cut_in_utf8_sequence_should_start_at_next_char() {
            let text = format!("{}a", "é".repeat(512));

            assert_eq!(
                tail(&text, 1),
                format!("(2 bytes omitted)\n{}a", "é".repeat(511))
            );
        }

        #[test]
        fn tail_failing_formatter_should_fail() {
            struct Writer {
                writes: usize,
                written: usize,
            }

            impl std::fmt::Write for Writer {
                fn write_str(&mut self, _: &str) -> std::fmt::Result {
                    self.written += 1;
                    (self.written <= self.writes)
                        .then_some(())
                        .ok_or(std::fmt::Error)
                }
            }

            for text in [
                [b"a\n".as_slice(), &b"b".repeat(2048), b"\xFF"].concat(),
                b"a\nb\xFF\n".to_vec(),
            ] {
                let tail = Tail {
                    text: &text,
                    lines: 1,
                };
                let mut counter = Writer {
                    writes: usize::MAX,
                    written: 0,
                };
                std::fmt::write(&mut counter, format_args!("{tail}")).unwrap();

                for writes in 0..counter.written {
                    let mut writer = Writer { writes, written: 0 };

                    assert!(
                        std::fmt::write(&mut writer, format_args!("{tail}")).is_err(),
                        "text: {text:?}, writes: {writes}"
                    );
                }
            }
        }

        #[test]
        fn tail_should_replace_invalid_utf8() {
            assert_eq!(
                Tail {
                    text: b"a\xFFb\xC3",
                    lines: 1,
                }
                .to_string(),
                "a\u{FFFD}b\u{FFFD}"
            );
        }
    }
}
