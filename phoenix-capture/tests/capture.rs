//! Live integration tests: `capture()` driven against a real, isolated tmux
//! server (never the developer's own sessions). These are the ticket's
//! acceptance criteria for capture, run against tmux itself.

mod support;
use support::{line, IsolatedTmux, NO_ARGS};

use phoenix_capture::{capture, Previous};
use phoenix_core::{
    Foreground, GenerationId, Made, NonEmpty, Origin, Shells, Snapshot, Touched, WindowId,
};
use tmux_control::{Client, SpawnOptions, SpawnTransport};

fn connect(harness: &IsolatedTmux) -> Client<SpawnTransport> {
    let transport = SpawnTransport::spawn(
        &["attach-session", "-t", &harness.session],
        &SpawnOptions {
            socket: Some(harness.socket.clone()),
            ..Default::default()
        },
    )
    .expect("failed to spawn tmux -C");
    Client::connect(transport, drop, |_, _| {}).expect("handshake failed against real tmux")
}

fn snapshot(client: &mut Client<SpawnTransport>) -> Snapshot {
    capture(client, &Previous::default(), &Shells::default()).expect("capture failed")
}

/// Captures until `accept` holds for the active pane's foreground, or gives
/// up: a shell needs a moment to exec the program `send-keys` typed.
fn active_foreground_once(
    client: &mut Client<SpawnTransport>,
    accept: impl Fn(&Foreground) -> bool,
) -> Foreground {
    let mut last = None;
    for _ in 0..50 {
        let snapshot = snapshot(client);
        let session = snapshot.sessions().first();
        let foreground = snapshot
            .active_window(session)
            .active_pane()
            .foreground
            .clone();
        if accept(&foreground) {
            return foreground;
        }
        last = Some(foreground);
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("the active pane never reached the expected foreground; last saw {last:?}");
}

fn argv(foreground: &Foreground) -> Option<Vec<String>> {
    match foreground {
        Foreground::Program { argv } => Some(argv.iter().cloned().collect()),
        _ => None,
    }
}

#[test]
fn live_capture_round_trips_a_single_session_window_pane() {
    let harness = IsolatedTmux::new("capture-simple");
    let mut client = connect(&harness);

    let snapshot = snapshot(&mut client);

    assert_eq!(snapshot.sessions().len(), 1);
    let session = snapshot.sessions().first();
    assert_eq!(session.name().as_str(), harness.session);
    assert_eq!(session.windows().len(), 1);
    assert_eq!(snapshot.windows().len(), 1);

    let pane = snapshot.active_window(session).active_pane();
    assert_eq!(
        pane.cwd.known().unwrap().as_str(),
        std::env::current_dir().unwrap().to_str().unwrap()
    );
    assert!(matches!(snapshot.origin, Origin::Recorded(_)));
    assert_eq!(snapshot.touched, Touched::Never);
    // The control client itself is a client (verified live): it sits on a
    // session like any terminal, and a plan that kills that session must
    // move it first.
    let [control] = snapshot.clients() else {
        panic!(
            "expected exactly the control client, got {:?}",
            snapshot.clients()
        );
    };
    assert_eq!(control.session.as_str(), harness.session);

    client.close();
}

#[test]
fn live_capture_tracks_the_real_active_and_last_window_and_pane() {
    let harness = IsolatedTmux::new("capture-active");
    let mut client = connect(&harness);

    client
        .execute(&line("new-window", ["-n", "second"]))
        .unwrap();
    client
        .execute(&line("new-window", ["-n", "third"]))
        .unwrap();
    client
        .execute(&line("split-window", ["-h", "-t", "third"]))
        .unwrap();
    let real = client
        .execute(&line(
            "display-message",
            ["-p", "#{window_index}:#{pane_index}"],
        ))
        .unwrap();
    let real = String::from_utf8(real.lines[0].clone()).unwrap();
    let (real_window_index, real_pane_index) = real.split_once(':').unwrap();
    let real_window_index: u32 = real_window_index.parse().unwrap();
    let real_pane_index: u32 = real_pane_index.parse().unwrap();

    let snapshot = snapshot(&mut client);
    let session = snapshot.sessions().first();
    assert_eq!(session.windows().len(), 3);
    assert_eq!(session.active().0, real_window_index);
    // "second" was current right before "third" was created.
    assert_eq!(session.last().map(|w| w.0), Some(real_window_index - 1));
    let window = snapshot.active_window(session);
    assert_eq!(window.panes().len(), 2);
    assert_eq!(window.active().0, real_pane_index);

    client.close();
}

#[test]
fn live_capture_reads_a_jobs_exact_argv_with_a_spaced_argument_intact() {
    let harness = IsolatedTmux::new("capture-argv");
    let mut client = connect(&harness);

    // `-` reads stdin, so grep stays in the foreground; the quoted argument
    // is exactly what a whitespace split of `ps` output would break.
    client
        .execute(&line("send-keys", ["grep \"hello world\" -", "Enter"]))
        .unwrap();
    let foreground =
        active_foreground_once(&mut client, |f| argv(f).is_some_and(|a| a[0] == "grep"));
    assert_eq!(
        argv(&foreground).unwrap(),
        vec![
            "grep".to_string(),
            "hello world".to_string(),
            "-".to_string()
        ]
    );

    client.execute(&line("send-keys", ["C-c"])).ok();
    client.close();
}

#[test]
fn live_capture_reads_an_idle_shell_as_shell_and_a_script_as_program() {
    let harness = IsolatedTmux::new("capture-shell");
    let mut client = connect(&harness);

    let idle = active_foreground_once(&mut client, |f| *f == Foreground::Shell);
    assert_eq!(idle, Foreground::Shell);

    let dir = std::env::temp_dir().join(format!("phoenix-capture-watch-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("watch.sh"), "sleep 300\n").unwrap();
    client
        .execute(&line(
            "new-window",
            ["-c", dir.to_str().unwrap(), "bash ./watch.sh"],
        ))
        .unwrap();
    // tmux runs a window's command through `$SHELL -c` (verified live), so
    // the pane's own process starts as the shell with that exact command
    // line — argv[0] is a shell but not every remaining argument is a flag,
    // the rule that keeps a script from reading as an idle shell — and
    // becomes `bash ./watch.sh` once the shell execs its last command. Both
    // are the pane's own process read exactly; neither is a `Shell`.
    let script = active_foreground_once(&mut client, |f| argv(f).is_some());
    let script = argv(&script).unwrap();
    assert!(
        script.as_slice() == ["bash", "./watch.sh"]
            || script.ends_with(&["-c".to_string(), "bash ./watch.sh".to_string()]),
        "{script:?}"
    );

    client.execute(&line("kill-window", NO_ARGS)).ok();
    let _ = std::fs::remove_dir_all(&dir);
    client.close();
}

#[test]
fn live_capture_structure_matches_the_raw_list_panes_pane_count() {
    let harness = IsolatedTmux::new("capture-count");
    let mut client = connect(&harness);

    client.execute(&line("new-window", NO_ARGS)).unwrap();
    client.execute(&line("split-window", NO_ARGS)).unwrap();
    client.execute(&line("split-window", ["-h"])).unwrap();

    let raw = client
        .execute(&line("list-panes", ["-a", "-F", "#{pane_id}"]))
        .unwrap();
    let raw_pane_count = raw.lines.len();

    let snapshot = snapshot(&mut client);
    assert_eq!(snapshot.panes().count(), raw_pane_count);

    client.close();
}

#[test]
fn live_capture_of_a_server_with_no_sessions_is_an_error() {
    let harness = IsolatedTmux::new("capture-empty");
    let mut client = connect(&harness);

    // exit-empty off keeps the server alive after its last session is gone.
    client
        .execute(&line("set-option", ["-g", "exit-empty", "off"]))
        .unwrap();
    client
        .execute(&line("kill-session", ["-t", harness.session.as_str()]))
        .unwrap();

    assert!(capture(&mut client, &Previous::default(), &Shells::default()).is_err());

    client.close();
}

#[test]
fn live_capture_holds_a_grouped_sessions_shared_window_once() {
    let harness = IsolatedTmux::new("capture-grouped");
    let mut client = connect(&harness);

    client
        .execute(&line("new-window", ["-n", "second"]))
        .unwrap();
    let grouped = format!("{}-twin", harness.session);
    client
        .execute(&line(
            "new-session",
            ["-d", "-s", &grouped, "-t", &harness.session],
        ))
        .unwrap();

    let snapshot = snapshot(&mut client);
    assert_eq!(snapshot.sessions().len(), 2);
    assert_eq!(
        snapshot.windows().len(),
        2,
        "each shared window is held once"
    );
    let ids_of = |name: &str| -> Vec<WindowId> {
        let session = snapshot
            .sessions()
            .iter()
            .find(|s| s.name().as_str() == name)
            .unwrap();
        assert_eq!(session.group().unwrap().as_str(), harness.session);
        session.windows().iter().map(|l| l.window).collect()
    };
    assert_eq!(ids_of(&harness.session), ids_of(&grouped));

    client
        .execute(&line("kill-session", ["-t", grouped.as_str()]))
        .unwrap();
    client.close();
}

#[test]
fn live_capture_reads_the_server_and_window_marks() {
    let harness = IsolatedTmux::new("capture-marks");
    let mut client = connect(&harness);

    client
        .execute(&line("set-option", ["-s", "@phoenix-generation", "7"]))
        .unwrap();
    let window_id = client
        .execute(&line("display-message", ["-p", "#{window_id}"]))
        .unwrap();
    let window_id = String::from_utf8(window_id.lines[0].clone()).unwrap();
    let saved = WindowId::parse(&window_id).unwrap();
    client
        .execute(&line(
            "set-option",
            ["-w", "-t", &window_id, "@phoenix-window", "7:@3"],
        ))
        .unwrap();

    let snapshot = snapshot(&mut client);
    assert_eq!(snapshot.touched, Touched::By(GenerationId(7)));
    assert_eq!(
        snapshot.window(saved).made(),
        Made::ByPhoenix {
            generation: GenerationId(7),
            saved: WindowId(3),
        }
    );

    client
        .execute(&line("set-option", ["-s", "@phoenix-generation", "seven"]))
        .unwrap();
    assert!(
        capture(&mut client, &Previous::default(), &Shells::default()).is_err(),
        "a set-but-malformed server mark is refused, not read as untouched"
    );

    client.close();
}

#[test]
fn live_capture_reads_every_pane_once_for_a_shared_window() {
    let harness = IsolatedTmux::new("capture-shared-panes");
    let mut client = connect(&harness);
    client.execute(&line("split-window", NO_ARGS)).unwrap();
    let grouped = format!("{}-twin", harness.session);
    client
        .execute(&line(
            "new-session",
            ["-d", "-s", &grouped, "-t", &harness.session],
        ))
        .unwrap();

    let snapshot = snapshot(&mut client);
    let ids: Vec<_> = snapshot.panes().map(|p| p.id).collect();
    let distinct: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(ids.len(), 2);
    assert_eq!(distinct.len(), ids.len());
    let _: NonEmpty<_> = snapshot.windows().clone();

    client
        .execute(&line("kill-session", ["-t", grouped.as_str()]))
        .unwrap();
    client.close();
}
