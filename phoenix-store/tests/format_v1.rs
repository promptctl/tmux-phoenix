//! A generation written by the format before this one stays readable. The
//! fixture is a real file: `phoenix save` from the last pre-bump build,
//! against an isolated tmux 3.7b server holding session `fixture` with a
//! window `first` (one pane, `sleep 987654` running in `/private/tmp`) and
//! a window `second` (two panes idle at `zsh -l`, in `/` and
//! `/private/tmp`). Content was not captured by that build's one-shot save.

use std::path::Path;

use phoenix_core::{
    Content, ContentFailure, Foreground, Made, NonEmpty, Origin, Touched, WindowIndex,
};
use phoenix_store::{FormatVersion, Store};

#[test]
fn a_pre_bump_generation_loads_with_origin_before_origin_was_recorded() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/format-v1.phnx");
    let scratch = std::env::temp_dir().join(format!("phoenix-store-v1-{}", std::process::id()));
    let store = Store::new(&scratch);

    let snapshot = store
        .load_file(&fixture)
        .expect("the v1 fixture must decode");

    assert_eq!(snapshot.origin, Origin::BeforeOriginWasRecorded);
    assert_eq!(snapshot.touched, Touched::Never);
    assert_eq!(snapshot.captured_at.unix_timestamp(), 1_791_110_788);
    assert!(snapshot.clients().is_empty());

    let session = snapshot.sessions().first();
    assert_eq!(session.name().as_str(), "fixture");
    assert_eq!(session.group(), None);
    assert_eq!(session.windows().len(), 2);
    assert_eq!(snapshot.windows().len(), 2);
    assert_eq!(session.active(), WindowIndex(2));

    let mut windows = snapshot.windows_of(session);
    let (first_link, first) = windows.next().unwrap();
    assert_eq!(first_link.index, WindowIndex(1));
    assert_eq!(first.name().as_str(), "first");
    assert_eq!(first.made(), Made::NotByPhoenix);
    assert!(!first.zoomed());
    let sleeping = first.panes().first();
    assert_eq!(sleeping.cwd.known().unwrap().as_str(), "/private/tmp");
    assert_eq!(
        sleeping.foreground,
        Foreground::Program {
            argv: NonEmpty::new("sleep".to_string(), vec!["987654".to_string()])
        }
    );
    assert_eq!(
        sleeping.content,
        Content::NotCaptured {
            reason: ContentFailure::NotRecorded
        }
    );

    let (_, second) = windows.next().unwrap();
    assert_eq!(second.name().as_str(), "second");
    assert_eq!(second.panes().len(), 2);
    for pane in second.panes().iter() {
        assert_eq!(pane.foreground, Foreground::Shell, "{pane:?}");
    }

    // Every pane's content is a reasoned absence, nothing else is degraded.
    assert_eq!(snapshot.degradations().len(), 3);

    let _ = std::fs::remove_dir_all(&scratch);
}

#[test]
fn the_fixture_lists_as_the_previous_format_with_no_origin_and_no_tag() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/format-v1.phnx");
    let scratch =
        std::env::temp_dir().join(format!("phoenix-store-v1-list-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).unwrap();
    std::fs::copy(&fixture, scratch.join("snapshot-1791110788.phnx")).unwrap();
    let store = Store::new(&scratch);

    let listed = store.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].format_version, FormatVersion::BEFORE_ORIGIN);
    assert_eq!(listed[0].origin, Origin::BeforeOriginWasRecorded);
    assert_eq!(listed[0].tag, None);
    assert_eq!(listed[0].captured_at.unix_timestamp(), 1_791_110_788);
    assert_eq!(
        store.load_latest().unwrap().origin,
        Origin::BeforeOriginWasRecorded
    );

    let _ = std::fs::remove_dir_all(&scratch);
}
