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

    #[test]
    fn test_unquote() {
        assert_eq!(unquote("test"), "test");
        assert_eq!(unquote("'test'"), "test");
        assert_eq!(unquote("\"test\""), "test");
        assert_eq!(unquote("'\"test\"'"), "\"test\"");
        assert_eq!(unquote("\"'test'\""), "'test'");
        assert_eq!(unquote("'test"), "'test");
        assert_eq!(unquote("test\""), "test\"");
        assert_eq!(unquote(" 'test' "), "test");
    }
}
