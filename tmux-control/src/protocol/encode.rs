//! Command encoding: the client-to-server half of the wire, and the mirror of
//! `decode`. tmux hands every line a control client sends to its config-file
//! parser (`control.c` → `cmd_parse_and_append`), so a command must be written
//! as tokens that parser reads back as exactly the original arguments, all on
//! one line.
//!
//! Ported from tmux's own `args_escape` (arguments.c), which tmux uses to print
//! a command its parser can read back, with two deviations. Each fixes an
//! argument tmux's form would not read back intact:
//!
//! - tmux single-quotes an argument whose only special character is `"`, yet
//!   still backslash-escapes inside those quotes, and single quotes do not
//!   process escapes. This encoder always double-quotes.
//! - tmux escapes `$` only before a variable-name character, a test its lexer
//!   makes with the server's locale-dependent `isalnum`. Under macOS's UTF-8
//!   locale the first byte of `é` passes it, so tmux reads `"$é"` as a variable
//!   and loses a byte (verified live on tmux 3.6a). This encoder cannot see the
//!   server's locale, so it escapes every `$`.

use std::fmt;

/// One tmux command encoded as a control-mode wire line: exactly one newline,
/// at the end. Only [`CommandLine::new`] and [`CommandLine::detach`] build one,
/// so a transport holding a `CommandLine` has nothing left to check
/// (`[LAW:parse-dont-validate]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandLine(String);

/// An argument held a NUL. tmux arguments are C strings, so no encoding can
/// carry one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NulInArgument {
    pub argument: String,
}

impl CommandLine {
    /// Encode the command `name` with `args`; tmux reads each argument back as
    /// exactly the string given. `name` is `'static` because a command name is
    /// never data.
    pub fn new(
        name: &'static str,
        args: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Result<Self, NulInArgument> {
        let tokens = std::iter::once(escape(name))
            .chain(args.into_iter().map(|arg| escape(arg.as_ref())))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self(tokens.join(" ") + "\n"))
    }

    /// The empty line, which tells tmux this client is detaching (SPEC §4.1).
    pub fn detach() -> Self {
        Self("\n".to_owned())
    }

    /// The command as written to tmux, without the line's newline: for printing.
    pub fn as_str(&self) -> &str {
        &self.0[..self.0.len() - 1]
    }

    /// The exact bytes to write to tmux, newline included.
    pub fn wire(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

/// Characters that make tmux's lexer end, split, or reinterpret a bare token:
/// `args_escape`'s set, plus `"` (see the module doc).
const NEEDS_QUOTES: [char; 9] = [' ', '#', '\'', ';', '$', '{', '}', '%', '"'];

/// `arg` as one token that tmux's lexer (`yylex_token`, cmd-parse.y) reads back
/// as `arg`. The lexer processes `\` escapes both bare and inside double quotes.
fn escape(arg: &str) -> Result<String, NulInArgument> {
    if arg.contains('\0') {
        return Err(NulInArgument {
            argument: arg.to_owned(),
        });
    }
    let quoted = arg.contains(NEEDS_QUOTES);
    let mut chars = arg.chars();
    Ok(match (chars.next(), chars.next()) {
        (None, _) => "''".to_owned(),
        // A lone special character is just its escape, as tmux prints it.
        (Some(c), None) if c != ' ' && (quoted || c == '~') => format!("\\{c}"),
        _ => {
            let quote = if quoted { "\"" } else { "" };
            // The lexer expands `~` only at the start of a token or quoted run.
            let tilde = if arg.starts_with('~') { "\\" } else { "" };
            let mut token = format!("{quote}{tilde}");
            arg.chars().for_each(|c| push_escaped(&mut token, c));
            token.push_str(quote);
            token
        }
    })
}

/// `c` as the lexer reads it back literally: tmux's `vis` with
/// `VIS_OCTAL|VIS_CSTYLE|VIS_TAB|VIS_NL|VIS_DQ`, except that every `$` is
/// escaped. Non-ASCII passes through raw, as `utf8_strvis` passes valid UTF-8,
/// which a `str` always is.
fn push_escaped(token: &mut String, c: char) {
    match c {
        '\\' => token.push_str("\\\\"),
        '"' => token.push_str("\\\""),
        '$' => token.push_str("\\$"),
        '\n' => token.push_str("\\n"),
        '\r' => token.push_str("\\r"),
        '\t' => token.push_str("\\t"),
        '\x07' => token.push_str("\\a"),
        '\x08' => token.push_str("\\b"),
        '\x0b' => token.push_str("\\v"),
        '\x0c' => token.push_str("\\f"),
        ' '..='~' => token.push(c),
        // The lexer reads exactly three octal digits, so a digit after the
        // escape stays a literal character.
        c if c.is_ascii() => token.push_str(&format!("\\{:03o}", u32::from(c))),
        c => token.push(c),
    }
}

impl fmt::Display for NulInArgument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "argument {:?} contains a NUL, which no tmux argument can hold",
            self.argument
        )
    }
}

impl std::error::Error for NulInArgument {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_each_argument_as_tmux_reads_it_back() {
        let cases = [
            ("plain", "plain"),
            ("-t", "-t"),
            ("a~b", "a~b"),
            ("héllo", "héllo"),
            ("", "''"),
            (" ", r#"" ""#),
            ("a b", r#""a b""#),
            (";", r"\;"),
            ("~", r"\~"),
            ("\"", r#"\""#),
            ("~/x", r"\~/x"),
            ("~/my dir", r#""\~/my dir""#),
            ("$HOME", r#""\$HOME""#),
            ("a$", r#""a\$""#),
            ("$é", r#""\$é""#),
            ("a\"b", r#""a\"b""#),
            ("it's", r#""it's""#),
            ("#{session_name}", r##""#{session_name}""##),
            ("a\\b", r"a\\b"),
            ("one\ntwo", r"one\ntwo"),
            ("\r\n", r"\r\n"),
            ("\t\x07\x08\x0b\x0c", r"\t\a\b\v\f"),
            ("\x017", r"\0017"),
            ("\x1b[0m", r"\033[0m"),
            ("\x7f", r"\177"),
        ];
        for (arg, token) in cases {
            let line = CommandLine::new("x", [arg]).unwrap();
            assert_eq!(line.as_str(), format!("x {token}"), "argument {arg:?}");
        }
    }

    #[test]
    fn a_command_is_its_tokens_joined_on_one_line() {
        let line = CommandLine::new("set-buffer", ["-b", "phx", "--", "a b"]).unwrap();
        assert_eq!(line.as_str(), r#"set-buffer -b phx -- "a b""#);
        assert_eq!(line.wire(), b"set-buffer -b phx -- \"a b\"\n");
    }

    #[test]
    fn no_ascii_character_puts_a_second_newline_on_the_wire() {
        let each: Vec<String> = (1u8..=0x7f).map(|b| char::from(b).to_string()).collect();
        let all: String = each.concat();
        for args in [each, vec![all]] {
            let line = CommandLine::new("x", &args).unwrap();
            let newlines = line.wire().iter().filter(|&&b| b == b'\n').count();
            assert_eq!(newlines, 1, "{:?}", line.as_str());
            assert_eq!(line.wire().last(), Some(&b'\n'));
        }
    }

    #[test]
    fn detach_is_the_empty_line() {
        let line = CommandLine::detach();
        assert_eq!(line.as_str(), "");
        assert_eq!(line.wire(), b"\n");
    }

    #[test]
    fn a_nul_fails_construction() {
        assert_eq!(
            CommandLine::new("set-buffer", ["ok", "a\0b"]),
            Err(NulInArgument {
                argument: "a\0b".to_owned()
            })
        );
    }
}
