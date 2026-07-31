//! The handful of ANSI styles this crate's reports use.
//!
//! Copied rather than shared: it is four colours, and depending on a styling crate to say "this
//! line is a problem" would be a heavier coupling than the duplication. The escapes match the
//! ones an embedding CLI is likely to use, so output looks the same either way.

/// Ends a styled span.
const RESET: &str = "\x1b[0m";

/// Wrap `text` in `sgr`, opening with a reset so an enclosing style can't bleed into it.
fn styled(sgr: &str, text: &str) -> String {
    format!("{RESET}\x1b[{sgr}m{text}{RESET}")
}

/// A section heading — bold blue.
pub(crate) fn header(text: &str) -> String {
    styled("1;34", text)
}

/// Something wrong: a failed download, an unreachable server — bold red.
pub(crate) fn problematic(text: &str) -> String {
    styled("1;31", text)
}

/// Something confirmed good — bold green.
pub(crate) fn approved(text: &str) -> String {
    styled("1;32", text)
}

/// A flag or argument name in help output — plain green, so it reads as a token to type.
pub(crate) fn argname(text: &str) -> String {
    styled("32", text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_style_opens_clean_and_closes_reset() {
        // The leading reset is what stops an enclosing colour from bleeding in; the trailing one
        // is what stops this colour leaking out into whatever prints next.
        for styled in [header("x"), problematic("x"), approved("x"), argname("x")] {
            assert!(styled.starts_with(RESET), "must open with a reset: {styled:?}");
            assert!(styled.ends_with(RESET), "must close with a reset: {styled:?}");
            assert!(styled.contains('x'), "the text survives: {styled:?}");
        }
        assert_ne!(problematic("x"), approved("x"), "the statuses must be distinguishable");
    }
}
