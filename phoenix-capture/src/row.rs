//! Parses one line of `list-panes -a -F` output (DESIGN.md §5). Pure — no
//! I/O — so it's testable against captured text with no tmux running.
//!
//! Fields are joined with `\x1f` (ASCII Unit Separator), not a printable
//! character like `|` or a tab: session/window names and pane cwds are
//! free-form text that could legitimately contain either, verified live
//! against a real tmux server before picking this delimiter.

use std::fmt;

pub const FIELD_DELIMITER: char = '\u{1f}';

const FIELD_NAMES: [&str; 11] = [
    "session_name",
    "window_index",
    "window_name",
    "window_layout",
    "window_active",
    "pane_index",
    "pane_current_path",
    "pane_current_command",
    "pane_active",
    "pane_pid",
    "pane_id",
];

/// The `-F` format string this parser is paired with. One `list-panes -a`
/// round-trip with this format is the entirety of DESIGN.md §5's structure
/// capture.
pub fn format_string() -> String {
    FIELD_NAMES
        .iter()
        .map(|f| format!("#{{{f}}}"))
        .collect::<Vec<_>>()
        .join(&FIELD_DELIMITER.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneRow {
    pub session: String,
    pub window_index: u32,
    pub window_name: String,
    pub window_layout: String,
    pub window_active: bool,
    pub pane_index: u32,
    pub pane_cwd: String,
    pub pane_command: String,
    pub pane_active: bool,
    pub pane_pid: u32,
    /// tmux's `%N` pane identifier, stable for the pane's whole lifetime —
    /// the correlator content-capture's dirty-tracking needs (unlike
    /// `pane_index`, which is a window-relative position that shifts if a
    /// sibling pane is added or removed). The leading `%` is stripped here,
    /// at the parse boundary — nothing downstream re-parses the raw form.
    pub pane_id: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowParseError {
    WrongFieldCount { expected: usize, found: usize },
    InvalidNumber { field: &'static str, value: String },
    InvalidBool { field: &'static str, value: String },
    InvalidPaneId { value: String },
}

impl fmt::Display for RowParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RowParseError::WrongFieldCount { expected, found } => {
                write!(f, "expected {expected} fields, found {found}")
            }
            RowParseError::InvalidNumber { field, value } => {
                write!(f, "field {field}: {value:?} is not a valid number")
            }
            RowParseError::InvalidBool { field, value } => {
                write!(f, "field {field}: {value:?} is not \"0\" or \"1\"")
            }
            RowParseError::InvalidPaneId { value } => {
                write!(f, "field pane_id: {value:?} is not a valid \"%N\" pane id")
            }
        }
    }
}

impl std::error::Error for RowParseError {}

fn parse_bool(field: &'static str, value: &str) -> Result<bool, RowParseError> {
    match value {
        "0" => Ok(false),
        "1" => Ok(true),
        other => Err(RowParseError::InvalidBool {
            field,
            value: other.to_string(),
        }),
    }
}

fn parse_u32(field: &'static str, value: &str) -> Result<u32, RowParseError> {
    value.parse().map_err(|_| RowParseError::InvalidNumber {
        field,
        value: value.to_string(),
    })
}

fn parse_pane_id(value: &str) -> Result<u32, RowParseError> {
    value
        .strip_prefix('%')
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| RowParseError::InvalidPaneId {
            value: value.to_string(),
        })
}

pub fn parse_row(line: &str) -> Result<PaneRow, RowParseError> {
    let fields: Vec<&str> = line.split(FIELD_DELIMITER).collect();
    if fields.len() != FIELD_NAMES.len() {
        return Err(RowParseError::WrongFieldCount {
            expected: FIELD_NAMES.len(),
            found: fields.len(),
        });
    }
    Ok(PaneRow {
        session: fields[0].to_string(),
        window_index: parse_u32("window_index", fields[1])?,
        window_name: fields[2].to_string(),
        window_layout: fields[3].to_string(),
        window_active: parse_bool("window_active", fields[4])?,
        pane_index: parse_u32("pane_index", fields[5])?,
        pane_cwd: fields[6].to_string(),
        pane_command: fields[7].to_string(),
        pane_active: parse_bool("pane_active", fields[8])?,
        pane_pid: parse_u32("pane_pid", fields[9])?,
        pane_id: parse_pane_id(fields[10])?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_string_has_one_hash_brace_per_field_joined_by_unit_separator() {
        let fmt = format_string();
        assert_eq!(fmt.matches("#{").count(), FIELD_NAMES.len());
        assert!(fmt.contains(FIELD_DELIMITER));
    }

    fn row(fields: [&str; 11]) -> String {
        fields.join(&FIELD_DELIMITER.to_string())
    }

    #[test]
    fn parses_a_well_formed_row() {
        let line = row([
            "main",
            "1",
            "shell",
            "b25d,80x24,0,0,0",
            "1",
            "0",
            "/Users/bmf/code",
            "zsh",
            "1",
            "12345",
            "%7",
        ]);
        let parsed = parse_row(&line).unwrap();
        assert_eq!(parsed.session, "main");
        assert_eq!(parsed.window_index, 1);
        assert_eq!(parsed.window_name, "shell");
        assert!(parsed.window_active);
        assert_eq!(parsed.pane_index, 0);
        assert_eq!(parsed.pane_cwd, "/Users/bmf/code");
        assert_eq!(parsed.pane_command, "zsh");
        assert!(parsed.pane_active);
        assert_eq!(parsed.pane_pid, 12345);
        assert_eq!(parsed.pane_id, 7);
    }

    #[test]
    fn rejects_wrong_field_count() {
        let err = parse_row("main\u{1f}1").unwrap_err();
        assert_eq!(
            err,
            RowParseError::WrongFieldCount {
                expected: 11,
                found: 2
            }
        );
    }

    #[test]
    fn rejects_invalid_bool() {
        let line = row([
            "main", "1", "shell", "layout", "maybe", "0", "/cwd", "zsh", "1", "1", "%1",
        ]);
        let err = parse_row(&line).unwrap_err();
        assert_eq!(
            err,
            RowParseError::InvalidBool {
                field: "window_active",
                value: "maybe".to_string()
            }
        );
    }

    #[test]
    fn rejects_invalid_number() {
        let line = row([
            "main", "one", "shell", "layout", "1", "0", "/cwd", "zsh", "1", "1", "%1",
        ]);
        let err = parse_row(&line).unwrap_err();
        assert_eq!(
            err,
            RowParseError::InvalidNumber {
                field: "window_index",
                value: "one".to_string()
            }
        );
    }

    #[test]
    fn rejects_a_pane_id_missing_the_percent_prefix() {
        let line = row([
            "main", "1", "shell", "layout", "1", "0", "/cwd", "zsh", "1", "1", "7",
        ]);
        let err = parse_row(&line).unwrap_err();
        assert_eq!(
            err,
            RowParseError::InvalidPaneId {
                value: "7".to_string()
            }
        );
    }
}
