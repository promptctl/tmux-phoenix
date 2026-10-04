//! Parses one line of `list-panes -a -F` and one line of `list-clients -F`
//! output into typed rows (ARCHITECTURE.md §6). Pure — no I/O — so it's
//! testable against captured text with no tmux running. Every field is
//! parsed into its domain type here, once (`[LAW:parse-dont-validate]`):
//! the fold never sees a raw string.
//!
//! Fields are joined with `\x1f` (ASCII Unit Separator), not a printable
//! character like `|` or a tab: session/window names and pane cwds are
//! free-form text that could legitimately contain either, verified live
//! against a real tmux server before picking this delimiter.
//!
//! `#{@phoenix-window}` reads the window option of ARCHITECTURE.md §3. A
//! tmux format resolves a `@` option through the server's options first,
//! then the pane's, then the window's (verified live on 3.7b); phoenix is the
//! only writer of that name and writes it at window scope, so the value a
//! format reports is the window's.

use std::fmt;

use phoenix_core::{
    Client, ClientName, Cwd, GroupName, HistoryIndicator, Layout, Made, MalformedWindowMark,
    PaneId, PaneIndex, SessionName, WindowId, WindowIndex, WindowName,
};

pub const FIELD_DELIMITER: char = '\u{1f}';

const PANE_FIELDS: [&str; 17] = [
    "session_name",
    "session_group",
    "window_index",
    "window_id",
    "window_name",
    "window_layout",
    "window_active",
    "window_last_flag",
    "window_zoomed_flag",
    "@phoenix-window",
    "pane_index",
    "pane_id",
    "pane_pid",
    "pane_current_path",
    "pane_active",
    "history_size",
    "history_bytes",
];

const CLIENT_FIELDS: [&str; 2] = ["client_name", "session_name"];

fn join_format(fields: &[&str]) -> String {
    fields
        .iter()
        .map(|f| format!("#{{{f}}}"))
        .collect::<Vec<_>>()
        .join(&FIELD_DELIMITER.to_string())
}

/// The `-F` format string [`parse_pane_row`] is paired with. One
/// `list-panes -a` round-trip with this format is the whole of structure
/// capture: every winlink (a shared window appears once per session that
/// links it, verified live on 3.7b) and every pane.
pub fn pane_format() -> String {
    join_format(&PANE_FIELDS)
}

/// The `-F` format string [`parse_client_row`] is paired with.
pub fn client_format() -> String {
    join_format(&CLIENT_FIELDS)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneRow {
    pub session: SessionName,
    pub group: Option<GroupName>,
    pub window_index: WindowIndex,
    pub window_id: WindowId,
    pub window_name: WindowName,
    pub window_layout: Layout,
    pub window_active: bool,
    pub window_last: bool,
    pub window_zoomed: bool,
    pub made: Made,
    pub pane_index: PaneIndex,
    pub pane_id: PaneId,
    pub pane_pid: u32,
    pub cwd: Cwd,
    pub pane_active: bool,
    pub indicator: HistoryIndicator,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowParseError {
    WrongFieldCount { expected: usize, found: usize },
    InvalidNumber { field: &'static str, value: String },
    InvalidBool { field: &'static str, value: String },
    InvalidId { field: &'static str, value: String },
    EmptyName { field: &'static str },
    WindowMark(MalformedWindowMark),
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
            RowParseError::InvalidId { field, value } => {
                write!(f, "field {field}: {value:?} is not a valid tmux id")
            }
            RowParseError::EmptyName { field } => write!(f, "field {field} was empty"),
            RowParseError::WindowMark(e) => write!(f, "{e}"),
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

fn parse_number<N: std::str::FromStr>(
    field: &'static str,
    value: &str,
) -> Result<N, RowParseError> {
    value.parse().map_err(|_| RowParseError::InvalidNumber {
        field,
        value: value.to_string(),
    })
}

fn parse_id<T>(
    field: &'static str,
    value: &str,
    parse: impl FnOnce(&str) -> Option<T>,
) -> Result<T, RowParseError> {
    parse(value).ok_or_else(|| RowParseError::InvalidId {
        field,
        value: value.to_string(),
    })
}

fn parse_name<T>(
    field: &'static str,
    value: &str,
    parse: impl FnOnce(&str) -> Option<T>,
) -> Result<T, RowParseError> {
    parse(value).ok_or(RowParseError::EmptyName { field })
}

fn split_fields<const N: usize>(line: &str) -> Result<[&str; N], RowParseError> {
    let fields: Vec<&str> = line.split(FIELD_DELIMITER).collect();
    fields
        .try_into()
        .map_err(|fields: Vec<&str>| RowParseError::WrongFieldCount {
            expected: N,
            found: fields.len(),
        })
}

pub fn parse_pane_row(line: &str) -> Result<PaneRow, RowParseError> {
    let [session, group, window_index, window_id, window_name, window_layout, window_active, window_last, window_zoomed, mark, pane_index, pane_id, pane_pid, cwd, pane_active, history_size, history_bytes] =
        split_fields::<17>(line)?;
    Ok(PaneRow {
        session: parse_name("session_name", session, |s| SessionName::parse(s))?,
        // Empty means ungrouped: tmux's own spelling of "no group".
        group: GroupName::parse(group),
        window_index: WindowIndex(parse_number("window_index", window_index)?),
        window_id: parse_id("window_id", window_id, WindowId::parse)?,
        window_name: parse_name("window_name", window_name, |s| WindowName::parse(s))?,
        window_layout: parse_name("window_layout", window_layout, |s| Layout::parse(s))?,
        window_active: parse_bool("window_active", window_active)?,
        window_last: parse_bool("window_last_flag", window_last)?,
        window_zoomed: parse_bool("window_zoomed_flag", window_zoomed)?,
        made: Made::parse_option(mark).map_err(RowParseError::WindowMark)?,
        pane_index: PaneIndex(parse_number("pane_index", pane_index)?),
        pane_id: parse_id("pane_id", pane_id, PaneId::parse)?,
        pane_pid: parse_number("pane_pid", pane_pid)?,
        cwd: Cwd::parse(cwd),
        pane_active: parse_bool("pane_active", pane_active)?,
        indicator: HistoryIndicator {
            history_size: parse_number("history_size", history_size)?,
            history_bytes: parse_number("history_bytes", history_bytes)?,
        },
    })
}

pub fn parse_client_row(line: &str) -> Result<Client, RowParseError> {
    let [name, session] = split_fields::<2>(line)?;
    Ok(Client {
        name: parse_name("client_name", name, |s| ClientName::parse(s))?,
        session: parse_name("session_name", session, |s| SessionName::parse(s))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_core::GenerationId;

    #[test]
    fn formats_have_one_hash_brace_per_field_joined_by_unit_separator() {
        let fmt = pane_format();
        assert_eq!(fmt.matches("#{").count(), PANE_FIELDS.len());
        assert!(fmt.contains(FIELD_DELIMITER));
        assert_eq!(client_format().matches("#{").count(), CLIENT_FIELDS.len());
    }

    fn row(fields: [&str; 17]) -> String {
        fields.join(&FIELD_DELIMITER.to_string())
    }

    const WELL_FORMED: [&str; 17] = [
        "main",
        "",
        "1",
        "@4",
        "shell",
        "b25d,80x24,0,0,0",
        "1",
        "0",
        "0",
        "",
        "0",
        "%7",
        "12345",
        "/Users/bmf/code",
        "1",
        "42",
        "4096",
    ];

    #[test]
    fn parses_a_well_formed_row() {
        let parsed = parse_pane_row(&row(WELL_FORMED)).unwrap();
        assert_eq!(parsed.session.as_str(), "main");
        assert_eq!(parsed.group, None);
        assert_eq!(parsed.window_index, WindowIndex(1));
        assert_eq!(parsed.window_id, WindowId(4));
        assert_eq!(parsed.window_name.as_str(), "shell");
        assert!(parsed.window_active);
        assert!(!parsed.window_last);
        assert!(!parsed.window_zoomed);
        assert_eq!(parsed.made, Made::NotByPhoenix);
        assert_eq!(parsed.pane_index, PaneIndex(0));
        assert_eq!(parsed.pane_id, PaneId(7));
        assert_eq!(parsed.cwd.known().unwrap().as_str(), "/Users/bmf/code");
        assert!(parsed.pane_active);
        assert_eq!(parsed.pane_pid, 12345);
        assert_eq!(
            parsed.indicator,
            HistoryIndicator {
                history_size: 42,
                history_bytes: 4096
            }
        );
    }

    #[test]
    fn a_group_and_a_window_mark_parse_into_their_types() {
        let mut fields = WELL_FORMED;
        fields[1] = "main";
        fields[9] = "7:@4";
        let parsed = parse_pane_row(&row(fields)).unwrap();
        assert_eq!(parsed.group.unwrap().as_str(), "main");
        assert_eq!(
            parsed.made,
            Made::ByPhoenix {
                generation: GenerationId(7),
                saved: WindowId(4)
            }
        );
    }

    #[test]
    fn an_empty_cwd_is_unreadable() {
        let mut fields = WELL_FORMED;
        fields[13] = "";
        assert_eq!(parse_pane_row(&row(fields)).unwrap().cwd, Cwd::Unreadable);
    }

    #[test]
    fn rejects_wrong_field_count() {
        assert_eq!(
            parse_pane_row("main\u{1f}1").unwrap_err(),
            RowParseError::WrongFieldCount {
                expected: 17,
                found: 2
            }
        );
    }

    #[test]
    fn rejects_invalid_bool_number_id_and_empty_name() {
        let mut fields = WELL_FORMED;
        fields[6] = "maybe";
        assert_eq!(
            parse_pane_row(&row(fields)).unwrap_err(),
            RowParseError::InvalidBool {
                field: "window_active",
                value: "maybe".to_string()
            }
        );
        let mut fields = WELL_FORMED;
        fields[2] = "one";
        assert_eq!(
            parse_pane_row(&row(fields)).unwrap_err(),
            RowParseError::InvalidNumber {
                field: "window_index",
                value: "one".to_string()
            }
        );
        let mut fields = WELL_FORMED;
        fields[11] = "7";
        assert_eq!(
            parse_pane_row(&row(fields)).unwrap_err(),
            RowParseError::InvalidId {
                field: "pane_id",
                value: "7".to_string()
            }
        );
        let mut fields = WELL_FORMED;
        fields[4] = "";
        assert_eq!(
            parse_pane_row(&row(fields)).unwrap_err(),
            RowParseError::EmptyName {
                field: "window_name"
            }
        );
    }

    #[test]
    fn a_malformed_window_mark_fails_the_row() {
        let mut fields = WELL_FORMED;
        fields[9] = "garbage";
        assert!(matches!(
            parse_pane_row(&row(fields)).unwrap_err(),
            RowParseError::WindowMark(_)
        ));
    }

    #[test]
    fn parses_a_client_row() {
        let client = parse_client_row("/dev/ttys003\u{1f}main").unwrap();
        assert_eq!(client.name.as_str(), "/dev/ttys003");
        assert_eq!(client.session.as_str(), "main");
        assert_eq!(
            parse_client_row("/dev/ttys003").unwrap_err(),
            RowParseError::WrongFieldCount {
                expected: 2,
                found: 1
            }
        );
    }
}
