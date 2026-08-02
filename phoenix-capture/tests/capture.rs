//! Live integration tests: `capture()` driven against a real, isolated tmux
//! server (never the developer's own sessions).

mod support;
use support::IsolatedTmux;

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
    Client::connect(transport).expect("handshake failed against real tmux")
}

#[test]
fn live_capture_round_trips_a_single_session_window_pane() {
    let harness = IsolatedTmux::new("capture-simple");
    let mut client = connect(&harness);

    let snapshot = phoenix_capture::capture(&mut client).expect("capture failed");

    assert_eq!(snapshot.sessions.len(), 1);
    let session = snapshot.sessions.first();
    assert_eq!(session.name().as_str(), harness.session);
    assert_eq!(session.windows().len(), 1);

    let window = session.active_window();
    let pane = window.active_pane();
    assert_eq!(
        pane.cwd.as_str(),
        std::env::current_dir().unwrap().to_str().unwrap()
    );

    client.close();
}

#[test]
fn live_capture_tracks_the_real_active_window_and_pane() {
    let harness = IsolatedTmux::new("capture-active");
    let mut client = connect(&harness);

    client.execute("new-window -n second").unwrap();
    client.execute("new-window -n third").unwrap();
    client.execute("split-window -h -t third").unwrap();
    // tmux focuses the newest window/pane on creation; "third" (index 2,
    // real tmux base-index may vary) and its right-hand split are active.
    let real_active = client
        .execute("display-message -p \"#{window_index}:#{pane_index}\"")
        .unwrap();
    let real_active = String::from_utf8(real_active.lines[0].clone()).unwrap();
    let (real_window_index, real_pane_index) = real_active.split_once(':').unwrap();
    let real_window_index: u32 = real_window_index.parse().unwrap();
    let real_pane_index: u32 = real_pane_index.parse().unwrap();

    let snapshot = phoenix_capture::capture(&mut client).expect("capture failed");
    let session = snapshot.sessions.first();
    assert_eq!(session.windows().len(), 3);
    assert_eq!(session.active().0, real_window_index);
    let window = session.active_window();
    assert_eq!(window.panes().len(), 2);
    assert_eq!(window.active().0, real_pane_index);

    client.close();
}

#[test]
fn live_capture_recovers_the_foreground_programs_argv() {
    let harness = IsolatedTmux::new("capture-argv");
    let mut client = connect(&harness);

    client.execute("send-keys 'sleep 987654' Enter").unwrap();
    // Give the shell a moment to actually exec `sleep` before we snapshot —
    // otherwise the pane's foreground process could still be the shell
    // itself mid-fork.
    let mut argv = Vec::new();
    for _ in 0..30 {
        let snapshot = phoenix_capture::capture(&mut client).expect("capture failed");
        let pane = snapshot.sessions.first().active_window().active_pane();
        if pane.program.command == "sleep" {
            argv = pane.program.argv.clone();
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(argv, vec!["sleep".to_string(), "987654".to_string()]);

    client.execute("send-keys C-c").ok();
    client.close();
}

#[test]
fn live_capture_structure_matches_the_raw_list_panes_pane_count() {
    let harness = IsolatedTmux::new("capture-count");
    let mut client = connect(&harness);

    client.execute("new-window").unwrap();
    client.execute("split-window").unwrap();
    client.execute("split-window -h").unwrap();

    let raw = client.execute("list-panes -a -F '#{pane_id}'").unwrap();
    let raw_pane_count = raw.lines.len();

    let snapshot = phoenix_capture::capture(&mut client).expect("capture failed");
    let captured_pane_count: usize = snapshot
        .sessions
        .iter()
        .flat_map(|s| s.windows().iter())
        .map(|w| w.panes().len())
        .sum();

    assert_eq!(captured_pane_count, raw_pane_count);

    client.close();
}
