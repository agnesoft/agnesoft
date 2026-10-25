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

/// Returns the last `lines` of `text`, preceded by the number of the lines
/// omitted if any.
#[cfg(any(feature = "process", feature = "request"))]
pub(crate) fn tail(text: &str, lines: usize) -> String {
    match text.lines().count().checked_sub(lines) {
        Some(omitted @ 1..) => {
            let start: usize = text.split_inclusive('\n').take(omitted).map(str::len).sum();
            format!("({omitted} lines omitted)\n{}", &text[start..])
        }
        _ => text.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
