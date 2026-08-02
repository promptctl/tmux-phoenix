//! Single-quote escaping for command arguments that go into a tmux command
//! string (`[LAW:single-enforcer]`: every free function in this module that
//! embeds a caller-supplied string routes through here).

/// Wrap `arg` in single quotes, escaping any single quote it contains as
/// `'\''` (close the quote, an escaped literal quote, reopen) — standard
/// POSIX-shell-style quoting, which tmux's own command parser also uses.
pub fn tmux_escape(arg: &str) -> String {
    let mut escaped = String::with_capacity(arg.len() + 2);
    escaped.push('\'');
    for ch in arg.chars() {
        if ch == '\'' {
            escaped.push_str("'\\''");
        } else {
            escaped.push(ch);
        }
    }
    escaped.push('\'');
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_plain_text_in_single_quotes() {
        assert_eq!(tmux_escape("hello"), "'hello'");
    }

    #[test]
    fn escapes_embedded_single_quotes() {
        assert_eq!(tmux_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn empty_string_becomes_empty_quotes() {
        assert_eq!(tmux_escape(""), "''");
    }

    #[test]
    fn passes_through_other_special_characters_untouched() {
        // Single quotes are the only thing single-quoting can't itself
        // protect against; everything else is literal inside them.
        assert_eq!(tmux_escape("a:b*c$d"), "'a:b*c$d'");
    }
}
