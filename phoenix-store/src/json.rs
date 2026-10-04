//! A hand-rolled, inspection-only JSON encoder (`--format=json` for
//! inspection). One-way — this crate never reads JSON back in; every
//! generation on disk is always the binary format from [`crate::header`].
//! No external crate available (see DESIGN.md §4's implementation note), so
//! this hand-rolls a small JSON value tree + renderer rather than pulling
//! in `serde_json`.

use phoenix_core::{Content, Cwd, Foreground, Made, Origin, Session, Snapshot, Touched, Window};

enum Value {
    Null,
    Bool(bool),
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
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
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

fn string(s: impl ToString) -> Value {
    Value::Str(s.to_string())
}

fn snapshot_value(s: &Snapshot) -> Value {
    Value::Object(vec![
        (
            "origin",
            match s.origin {
                Origin::Recorded(id) => string(id),
                Origin::BeforeOriginWasRecorded => Value::Null,
            },
        ),
        (
            "touched",
            match s.touched {
                Touched::By(generation) => Value::Number(generation.0),
                Touched::Never => Value::Null,
            },
        ),
        (
            "tmux_version",
            string(format!("{}.{}", s.tmux_version.major, s.tmux_version.minor)),
        ),
        (
            "captured_at_unix",
            Value::Number(s.captured_at.unix_timestamp()),
        ),
        (
            "windows",
            Value::Array(s.windows().iter().map(window_value).collect()),
        ),
        (
            "sessions",
            Value::Array(s.sessions().iter().map(session_value).collect()),
        ),
        (
            "clients",
            Value::Array(
                s.clients()
                    .iter()
                    .map(|c| {
                        Value::Object(vec![
                            ("name", string(&c.name)),
                            ("session", string(&c.session)),
                        ])
                    })
                    .collect(),
            ),
        ),
    ])
}

fn session_value(session: &Session) -> Value {
    Value::Object(vec![
        ("name", string(session.name())),
        ("group", session.group().map(string).unwrap_or(Value::Null)),
        (
            "active_window",
            Value::Number(i64::from(session.active().0)),
        ),
        (
            "last_window",
            session
                .last()
                .map(|w| Value::Number(i64::from(w.0)))
                .unwrap_or(Value::Null),
        ),
        (
            "windows",
            Value::Array(
                session
                    .windows()
                    .iter()
                    .map(|link| {
                        Value::Object(vec![
                            ("index", Value::Number(i64::from(link.index.0))),
                            ("window", string(link.window)),
                        ])
                    })
                    .collect(),
            ),
        ),
    ])
}

fn window_value(window: &Window) -> Value {
    Value::Object(vec![
        ("id", string(window.id())),
        (
            "made",
            match window.made() {
                Made::ByPhoenix { generation, saved } => Value::Object(vec![
                    ("generation", Value::Number(generation.0)),
                    ("saved", string(saved)),
                ]),
                Made::NotByPhoenix => Value::Null,
            },
        ),
        ("name", string(window.name())),
        ("layout", string(window.layout())),
        ("zoomed", Value::Bool(window.zoomed())),
        ("active_pane", Value::Number(i64::from(window.active().0))),
        (
            "panes",
            Value::Array(window.panes().iter().map(pane_value).collect()),
        ),
    ])
}

fn pane_value(pane: &phoenix_core::Pane) -> Value {
    Value::Object(vec![
        ("id", string(pane.id)),
        ("index", Value::Number(i64::from(pane.index.0))),
        (
            "cwd",
            match &pane.cwd {
                Cwd::Known(path) => string(path),
                Cwd::Unreadable => Value::Null,
            },
        ),
        (
            "foreground",
            match &pane.foreground {
                Foreground::Shell => Value::Object(vec![("kind", string("shell"))]),
                Foreground::Program { argv } => Value::Object(vec![
                    ("kind", string("program")),
                    ("argv", Value::Array(argv.iter().map(string).collect())),
                ]),
                Foreground::Unrecovered { reason } => Value::Object(vec![
                    ("kind", string("unrecovered")),
                    ("reason", string(reason)),
                ]),
            },
        ),
        (
            "content",
            match &pane.content {
                Content::NotCaptured { reason } => {
                    Value::Object(vec![("not_captured", string(reason))])
                }
                Content::Captured {
                    indicator,
                    scrollback,
                    visible,
                } => Value::Object(vec![
                    ("history_size", Value::Number(indicator.history_size as i64)),
                    (
                        "history_bytes",
                        Value::Number(indicator.history_bytes as i64),
                    ),
                    ("scrollback_lines", Value::Number(scrollback.len() as i64)),
                    ("visible_lines", Value::Number(visible.len() as i64)),
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
    use crate::testing::sample_snapshot;

    #[test]
    fn produces_balanced_braces_and_brackets() {
        let json = to_json(&sample_snapshot(1_700_000_000));
        let opens = json.matches('{').count() + json.matches('[').count();
        let closes = json.matches('}').count() + json.matches(']').count();
        assert_eq!(opens, closes);
    }

    #[test]
    fn contains_expected_field_values() {
        let json = to_json(&sample_snapshot(1_700_000_000));
        assert!(json.contains("\"name\": \"main\""));
        assert!(json.contains("\"kind\": \"shell\""));
        assert!(json.contains("\"kind\": \"program\""));
        assert!(json.contains("\"tmux_version\": \"3.7\""));
        assert!(json.contains("\"captured_at_unix\": 1700000000"));
        assert!(json.contains("\"origin\": \"4242:1700000000\""));
    }

    #[test]
    fn escapes_quotes_backslashes_and_control_characters() {
        assert_eq!(escape("a\"b\\c"), r#""a\"b\\c""#);
        assert_eq!(escape("a\nb\tc"), r#""a\nb\tc""#);
        assert_eq!(escape("\u{1}"), "\"\\u0001\"");
    }
}
