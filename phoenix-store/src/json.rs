//! A hand-rolled, inspection-only JSON encoder (DESIGN.md §7: "a
//! `--format=json` option ... for inspection"). One-way — this crate never
//! reads JSON back in; `latest` and every generation on disk are always the
//! binary format from [`crate::header`]. No external crate available (see
//! DESIGN.md §4's implementation note), so this hand-rolls a small JSON
//! value tree + renderer rather than pulling in `serde_json`.

use phoenix_core::Snapshot;

enum Value {
    Null,
    Number(i64),
    Str(String),
    Array(Vec<Value>),
    Object(Vec<(&'static str, Value)>),
}

fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn push_indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn render(value: &Value, depth: usize, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::Str(s) => out.push_str(&escape(s)),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                push_indent(out, depth + 1);
                render(item, depth + 1, out);
                if i + 1 < items.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            push_indent(out, depth);
            out.push(']');
        }
        Value::Object(fields) => {
            if fields.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str("{\n");
            for (i, (key, val)) in fields.iter().enumerate() {
                push_indent(out, depth + 1);
                out.push_str(&escape(key));
                out.push_str(": ");
                render(val, depth + 1, out);
                if i + 1 < fields.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            push_indent(out, depth);
            out.push('}');
        }
    }
}

fn snapshot_value(s: &Snapshot) -> Value {
    Value::Object(vec![
        (
            "format_version",
            Value::Number(i64::from(s.format_version.0)),
        ),
        (
            "tmux_version",
            Value::Str(format!("{}.{}", s.tmux_version.major, s.tmux_version.minor)),
        ),
        (
            "captured_at_unix",
            Value::Number(s.captured_at.unix_timestamp()),
        ),
        (
            "sessions",
            Value::Array(s.sessions.iter().map(session_value).collect()),
        ),
    ])
}

fn session_value(session: &phoenix_core::Session) -> Value {
    Value::Object(vec![
        ("name", Value::Str(session.name().as_str().to_string())),
        (
            "active_window",
            Value::Number(i64::from(session.active().0)),
        ),
        (
            "windows",
            Value::Array(session.windows().iter().map(window_value).collect()),
        ),
    ])
}

fn window_value(window: &phoenix_core::Window) -> Value {
    Value::Object(vec![
        ("index", Value::Number(i64::from(window.index().0))),
        ("name", Value::Str(window.name().as_str().to_string())),
        ("layout", Value::Str(window.layout().as_str().to_string())),
        ("active_pane", Value::Number(i64::from(window.active().0))),
        (
            "panes",
            Value::Array(window.panes().iter().map(pane_value).collect()),
        ),
    ])
}

fn pane_value(pane: &phoenix_core::Pane) -> Value {
    Value::Object(vec![
        ("id", Value::Number(i64::from(pane.id.0))),
        ("index", Value::Number(i64::from(pane.index.0))),
        (
            "cwd",
            match &pane.cwd {
                None => Value::Null,
                Some(cwd) => Value::Str(cwd.as_str().to_string()),
            },
        ),
        (
            "command",
            Value::Str(pane.program.command.as_str().to_string()),
        ),
        (
            "argv",
            match &pane.program.argv {
                None => Value::Null,
                Some(argv) => Value::Array(argv.iter().cloned().map(Value::Str).collect()),
            },
        ),
        (
            "content",
            match &pane.content {
                None => Value::Null,
                Some(content) => Value::Object(vec![
                    ("history_size", Value::Number(content.history_size as i64)),
                    ("history_bytes", Value::Number(content.history_bytes as i64)),
                    (
                        "scrollback_lines",
                        Value::Number(content.scrollback.len() as i64),
                    ),
                    ("visible_lines", Value::Number(content.visible.len() as i64)),
                ]),
            },
        ),
    ])
}

pub fn to_json(snapshot: &Snapshot) -> String {
    let mut out = String::new();
    render(&snapshot_value(snapshot), 0, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_core::{
        CapturedProgram, FormatVersion, Layout, NonEmpty, OffsetDateTime, Pane, PaneId, PaneIndex,
        ProgramName, Session, SessionName, TmuxVersion, Utf8PathBuf, Window, WindowIndex,
        WindowName,
    };

    fn sample() -> Snapshot {
        let pane = Pane {
            id: PaneId(0),
            index: PaneIndex(0),
            cwd: Utf8PathBuf::parse("/home/user"),
            program: CapturedProgram {
                command: ProgramName::parse("zsh").unwrap(),
                argv: None,
            },
            content: None,
        };
        let window = Window::new(
            WindowIndex(0),
            WindowName::parse("shell").unwrap(),
            Layout::parse("b25d,80x24,0,0,0").unwrap(),
            NonEmpty::singleton(pane),
            PaneIndex(0),
        )
        .unwrap();
        let session = Session::new(
            SessionName::parse("main").unwrap(),
            NonEmpty::singleton(window),
            WindowIndex(0),
        )
        .unwrap();
        Snapshot {
            format_version: FormatVersion::CURRENT,
            tmux_version: TmuxVersion { major: 3, minor: 5 },
            captured_at: OffsetDateTime::from_unix_timestamp(1_700_000_000),
            sessions: NonEmpty::singleton(session),
        }
    }

    #[test]
    fn produces_balanced_braces_and_brackets() {
        let json = to_json(&sample());
        let opens = json.matches('{').count() + json.matches('[').count();
        let closes = json.matches('}').count() + json.matches(']').count();
        assert_eq!(opens, closes);
    }

    #[test]
    fn contains_expected_field_values() {
        let json = to_json(&sample());
        assert!(json.contains("\"name\": \"main\""));
        assert!(json.contains("\"command\": \"zsh\""));
        assert!(json.contains("\"tmux_version\": \"3.5\""));
        assert!(json.contains("\"captured_at_unix\": 1700000000"));
    }

    #[test]
    fn escapes_quotes_and_backslashes_in_strings() {
        assert_eq!(escape("a\"b\\c"), r#""a\"b\\c""#);
    }

    #[test]
    fn escapes_control_characters() {
        assert_eq!(escape("a\nb\tc"), r#""a\nb\tc""#);
        assert_eq!(escape("\u{1}"), "\"\\u0001\"");
    }
}
