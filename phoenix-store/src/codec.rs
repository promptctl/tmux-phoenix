//! `Snapshot <-> bytes`, built entirely on `phoenix_core`'s public
//! constructors and accessors — decoding a corrupt file can produce a
//! [`StoreError`] but can never produce an invalid `Snapshot`, since
//! `phoenix_core` itself won't let one be constructed
//! (`[LAW:types-are-the-program]`, carried through from tmux-capture-4q0.1).
//!
//! `format_version` and `captured_at`/`checksum` live in the file header
//! (see [`crate::header`]), not here — this only encodes the body.

use phoenix_core::{
    CapturedProgram, Layout, NonEmpty, Pane, PaneContent, PaneIndex, Session, SessionName,
    Snapshot, TmuxVersion, Window, WindowIndex, WindowName,
};

use crate::binary::{Reader, Writer};
use crate::error::StoreError;

pub fn encode_body(snapshot: &Snapshot) -> Vec<u8> {
    let mut w = Writer::new();
    w.write_u32(snapshot.tmux_version.major);
    w.write_u32(snapshot.tmux_version.minor);
    w.write_i64(snapshot.captured_at.unix_timestamp());
    w.write_seq(
        snapshot.sessions.len(),
        snapshot.sessions.iter(),
        encode_session,
    );
    w.into_bytes()
}

fn encode_session(w: &mut Writer, session: &Session) {
    w.write_str(session.name().as_str());
    w.write_seq(
        session.windows().len(),
        session.windows().iter(),
        encode_window,
    );
    w.write_u32(session.active().0);
}

fn encode_window(w: &mut Writer, window: &Window) {
    w.write_u32(window.index().0);
    w.write_str(window.name().as_str());
    w.write_str(window.layout().as_str());
    w.write_seq(window.panes().len(), window.panes().iter(), encode_pane);
    w.write_u32(window.active().0);
}

fn encode_pane(w: &mut Writer, pane: &Pane) {
    w.write_u32(pane.index.0);
    w.write_str(pane.cwd.as_str());
    w.write_str(&pane.program.command);
    w.write_vec(&pane.program.argv, |w, arg| w.write_str(arg));
    w.write_option(&pane.content, |w, content| {
        w.write_vec(&content.lines, |w, line| w.write_str(line));
    });
}

/// Takes `format_version`/`captured_at` from the already-validated header
/// (see [`crate::header::Header`]) rather than re-decoding them from the
/// body — the header is the single source of truth for both.
pub fn decode_body(
    bytes: &[u8],
    format_version: phoenix_core::FormatVersion,
    captured_at: phoenix_core::OffsetDateTime,
) -> Result<Snapshot, StoreError> {
    let mut r = Reader::new(bytes);
    let tmux_version = TmuxVersion {
        major: r.read_u32()?,
        minor: r.read_u32()?,
    };
    let _captured_at_in_body = r.read_i64()?; // redundant with the header; kept for a stable body layout
    let sessions = r.read_vec(decode_session)?;
    let sessions = NonEmpty::from_vec(sessions).ok_or(StoreError::EmptyCollection)?;

    if !r.remaining().is_empty() {
        return Err(StoreError::Truncated);
    }

    Ok(Snapshot {
        format_version,
        tmux_version,
        captured_at,
        sessions,
    })
}

fn decode_session(r: &mut Reader) -> Result<Session, StoreError> {
    let name = SessionName::parse(r.read_str()?).ok_or(StoreError::InvalidName)?;
    let windows = r.read_vec(decode_window)?;
    let windows = NonEmpty::from_vec(windows).ok_or(StoreError::EmptyCollection)?;
    let active = WindowIndex(r.read_u32()?);
    Session::new(name, windows, active).map_err(StoreError::from)
}

fn decode_window(r: &mut Reader) -> Result<Window, StoreError> {
    let index = WindowIndex(r.read_u32()?);
    let name = WindowName::parse(r.read_str()?).ok_or(StoreError::InvalidName)?;
    let layout = Layout::parse(r.read_str()?).ok_or(StoreError::InvalidName)?;
    let panes = r.read_vec(decode_pane)?;
    let panes = NonEmpty::from_vec(panes).ok_or(StoreError::EmptyCollection)?;
    let active = PaneIndex(r.read_u32()?);
    Window::new(index, name, layout, panes, active).map_err(StoreError::from)
}

fn decode_pane(r: &mut Reader) -> Result<Pane, StoreError> {
    let index = PaneIndex(r.read_u32()?);
    let cwd = r.read_str()?.into();
    let command = r.read_str()?;
    let argv = r.read_vec(Reader::read_str)?;
    let content = r.read_option(|r| {
        let lines = r.read_vec(Reader::read_str)?;
        Ok(PaneContent::new(lines))
    })?;
    Ok(Pane {
        index,
        cwd,
        program: CapturedProgram::new(command, argv),
        content,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_core::{FormatVersion, OffsetDateTime};

    fn sample_snapshot() -> Snapshot {
        let pane0 = Pane {
            index: PaneIndex(0),
            cwd: "/home/user".into(),
            program: CapturedProgram::new("zsh", vec![]),
            content: None,
        };
        let pane1 = Pane {
            index: PaneIndex(1),
            cwd: "/home/user/proj".into(),
            program: CapturedProgram::new("vim", vec!["vim".to_string(), "DESIGN.md".to_string()]),
            content: Some(PaneContent::new(vec![
                "line one".to_string(),
                "line two".to_string(),
            ])),
        };
        let window = Window::new(
            WindowIndex(0),
            WindowName::parse("shell").unwrap(),
            Layout::parse("b25d,80x24,0,0,0").unwrap(),
            NonEmpty::new(pane0, vec![pane1]),
            PaneIndex(1),
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
    fn round_trips_a_full_tree() {
        let snapshot = sample_snapshot();
        let body = encode_body(&snapshot);
        let decoded = decode_body(&body, snapshot.format_version, snapshot.captured_at).unwrap();
        assert_eq!(decoded, snapshot);
    }

    #[test]
    fn rejects_truncated_bodies() {
        let snapshot = sample_snapshot();
        let mut body = encode_body(&snapshot);
        body.truncate(body.len() - 3);
        let err = decode_body(&body, snapshot.format_version, snapshot.captured_at).unwrap_err();
        assert!(matches!(err, StoreError::Truncated));
    }

    #[test]
    fn rejects_trailing_garbage_after_a_valid_body() {
        let snapshot = sample_snapshot();
        let mut body = encode_body(&snapshot);
        body.push(0xff);
        let err = decode_body(&body, snapshot.format_version, snapshot.captured_at).unwrap_err();
        assert!(matches!(err, StoreError::Truncated));
    }
}
