// input.rs — Slash command helpers and input mode types.

/// Check whether a string looks like a slash command (e.g. "/help").
pub fn is_slash_command(input: &str) -> bool {
    input.starts_with('/') && !input.starts_with("//")
}
/// Check if the input is a bang command (! prefix).
/// A single `!` is a bang command only when followed by at least one
/// non-whitespace character; `!!` (double bang) is NOT a bang command.
pub fn is_bang_command(input: &str) -> bool {
    input.starts_with('!') && !input.starts_with("!!") && input.len() > 1
}


/// Parse a slash command into `(command_name, args)`.
/// Returns `("", "")` if the input is not a slash command.
pub fn parse_slash_command(input: &str) -> (&str, &str) {
    if !is_slash_command(input) {
        return ("", "");
    }
    let without_slash = &input[1..];
    if let Some(space_idx) = without_slash.find(' ') {
        (
            &without_slash[..space_idx],
            without_slash[space_idx + 1..].trim(),
        )
    } else {
        (without_slash, "")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_command_detection() {
        assert!(is_slash_command("/help"));
        assert!(is_slash_command("/compact args"));
        assert!(!is_slash_command("//comment"));
        assert!(!is_slash_command("hello"));
        assert!(!is_slash_command(""));
    }

    #[test]
    fn parse_no_args() {
        let (cmd, args) = parse_slash_command("/help");
        assert_eq!(cmd, "help");
        assert_eq!(args, "");
    }

    #[test]
    fn parse_with_args() {
        let (cmd, args) = parse_slash_command("/compact  --force ");
        assert_eq!(cmd, "compact");
        assert_eq!(args, "--force");
    }

    #[test]
    fn parse_non_slash() {
        let (cmd, args) = parse_slash_command("hello world");
        assert_eq!(cmd, "");
        assert_eq!(args, "");
    }
}
