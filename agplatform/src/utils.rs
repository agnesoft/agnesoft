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
}
