//! `plan` (ARCHITECTURE.md §8): the difference between a saved snapshot and
//! a server, over recorded identity, as an ordered list of [`Step`]s. Pure —
//! no I/O — so every case below is a unit test with no tmux running.
//!
//! A saved window is *present* when a live window is that window: one this
//! generation's restore built and stamped ([`Made::ByPhoenix`]), or — on the
//! server incarnation the snapshot was saved from — the very window, by its
//! tmux id. Never when something merely sits at the same index or bears the
//! same name. A saved session is present when a live session has its name,
//! because tmux keeps names unique. Everything absent is built, everything
//! present is left alone, and nothing live is removed.
//!
//! The order tmux imposes — a session before its windows, every pane before
//! the layout, the layout before content is printed into it — is encoded
//! here once, as the sequence of the steps, and nowhere as a contract
//! between functions (`[LAW:no-ambient-temporal-coupling]`). What varies
//! between one restore and the next is this list, never which code runs
//! (`[LAW:dataflow-not-control-flow]`).

use std::collections::{BTreeSet, HashMap};
use std::fmt;

use phoenix_core::{
    Content, Foreground, GenerationId, Layout, Made, Origin, Session, Snapshot, Touched, WinLink,
    Window, WindowId, WindowIndex,
};
use tmux_control::{Opened, SessionName, UnaddressableSessionName};

use crate::step::{LinkSource, OptionScope, PaneRef, Step, WindowRef};

/// The session option a restore sets on every session it finishes. Its
/// reader is outside phoenix: an integration waiting for "restore finished"
/// waits on this value.
const RESTORED_OPTION: &str = "@phoenix-restored";

/// What a restore lands on: the three states a caller can be in, and no
/// fourth — a scratch session with no account of who made it, or an attached
/// connection with no capture, cannot be written (`[LAW:types-are-the-program]`).
#[derive(Debug, Clone, Copy)]
pub enum Onto<'a> {
    /// The connection attached to a session the server already held; `live`
    /// is a capture taken over it.
    Server { live: &'a Snapshot },
    /// No session existed, so the connection made `scratch` to attach with;
    /// `live` is a capture taken over it. The plan removes `scratch` as
    /// soon as a restored session exists to hold its clients.
    Scratch {
        scratch: &'a SessionName,
        live: &'a Snapshot,
    },
    /// No session exists and nothing has been opened: what a dry run finds,
    /// previewing the restore that would make `scratch`.
    NoServer { scratch: &'a SessionName },
}

impl<'a> Onto<'a> {
    /// What [`tmux_control::Connection::open`] reported, with the capture
    /// taken over that connection.
    pub fn opened(opened: &'a Opened, live: &'a Snapshot) -> Self {
        match opened {
            Opened::Attached => Onto::Server { live },
            Opened::Created(scratch) => Onto::Scratch { scratch, live },
        }
    }

    fn live(self) -> Option<&'a Snapshot> {
        match self {
            Onto::Server { live } | Onto::Scratch { live, .. } => Some(live),
            Onto::NoServer { .. } => None,
        }
    }

    fn scratch(self) -> Option<&'a SessionName> {
        match self {
            Onto::Server { .. } => None,
            Onto::Scratch { scratch, .. } | Onto::NoServer { scratch } => Some(scratch),
        }
    }
}

/// Something the plan left alone or did differently from the snapshot, for
/// the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// The server already holds a session of this name; what it lacks is
    /// added into it.
    SessionPresent { session: SessionName },
    /// The window saved at `index` is already on the server.
    WindowPresent {
        session: SessionName,
        index: WindowIndex,
    },
    /// The saved index holds another window, so this one lands at `landed`.
    Relocated {
        session: SessionName,
        saved: WindowIndex,
        landed: WindowIndex,
    },
    /// A live window in a saved session that is not one of the snapshot's.
    /// It stays.
    NotFromSnapshot {
        session: SessionName,
        index: WindowIndex,
    },
    /// tmux cannot be told this session's name as a target, so nothing of
    /// it is restored.
    Unaddressable(UnaddressableSessionName),
}

impl fmt::Display for Note {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Note::SessionPresent { session } => write!(
                f,
                "session {session} is already on the server; what it lacks is added into it"
            ),
            Note::WindowPresent { session, index } => write!(
                f,
                "session {session}: the window saved at index {} is already on the server",
                index.0
            ),
            Note::Relocated {
                session,
                saved,
                landed,
            } => write!(
                f,
                "session {session}: index {} is taken, so the window saved there lands at {}",
                saved.0, landed.0
            ),
            Note::NotFromSnapshot { session, index } => write!(
                f,
                "session {session}: the window at index {} is not from this snapshot and is left alone",
                index.0
            ),
            Note::Unaddressable(name) => write!(f, "{name}, so nothing of it is restored"),
        }
    }
}

/// An ordered list of steps in which every reference is defined by an
/// earlier step. Only [`plan`] builds one, so `apply` relies on that rather
/// than checking it (`[LAW:types-are-the-program]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    steps: Vec<Step>,
    notes: Vec<Note>,
}

impl Plan {
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }

    pub fn notes(&self) -> &[Note] {
        &self.notes
    }
}

/// A session name no session in `snapshot` has, for the session a restore
/// makes in order to attach to a server holding none. Chosen against the
/// snapshot so the plan can never take the scratch for a session to keep.
pub fn scratch_name(snapshot: &Snapshot) -> SessionName {
    let taken = |name: &str| {
        snapshot
            .sessions()
            .iter()
            .any(|s| s.name().as_str() == name)
    };
    (0u64..)
        .map(|n| format!("phoenix-scratch-{n}"))
        .find(|name| !taken(name))
        .map(|name| SessionName::parse(name).expect("the scratch prefix is addressable"))
        .expect("an unbounded range always holds a free name")
}

/// The steps that make `onto` hold everything `snapshot` holds, where
/// `generation` is the store's id for `snapshot` — the identity the windows
/// it builds are stamped with.
pub fn plan(generation: GenerationId, snapshot: &Snapshot, onto: Onto<'_>) -> Plan {
    let live = onto.live();
    let mut draft = Draft {
        generation,
        snapshot,
        live,
        // The server mark is the last step of a restore that finished, so a
        // server not carrying this generation's mark has not been finished.
        unfinished: live.map_or(Touched::Never, |live| live.touched) != Touched::By(generation),
        scratch: onto.scratch(),
        steps: Vec::new(),
        notes: Vec::new(),
        built: HashMap::new(),
        refs: 0,
    };

    snapshot
        .sessions()
        .iter()
        .for_each(|session| draft.session(session));
    if draft.unfinished {
        draft.steps.push(Step::SetOption {
            scope: OptionScope::Server,
            key: Touched::OPTION,
            value: generation.to_string(),
        });
    }

    Plan {
        steps: draft.steps,
        notes: draft.notes,
    }
}

struct Draft<'a> {
    generation: GenerationId,
    snapshot: &'a Snapshot,
    live: Option<&'a Snapshot>,
    unfinished: bool,
    /// The session the connection made to attach with, until a step removes
    /// it.
    scratch: Option<&'a SessionName>,
    steps: Vec<Step>,
    notes: Vec<Note>,
    /// Saved windows this plan has built so far.
    built: HashMap<WindowId, WindowRef>,
    refs: u32,
}

/// The lowest index at or above `from` that `taken` lacks, taken.
fn free_from(from: WindowIndex, taken: &mut BTreeSet<WindowIndex>) -> WindowIndex {
    let free = (from.0..)
        .map(WindowIndex)
        .find(|index| !taken.contains(index))
        .expect("a session's windows never exhaust u32");
    taken.insert(free);
    free
}

/// Removes the first link `pick` accepts, saying whether there was one.
fn claim(links: &mut Vec<WinLink>, pick: impl Fn(&WinLink) -> bool) -> bool {
    links
        .iter()
        .position(pick)
        .map(|at| links.remove(at))
        .is_some()
}

impl<'a> Draft<'a> {
    fn fresh(&mut self) -> (WindowRef, PaneRef) {
        let window = WindowRef(self.refs);
        (window, self.fresh_pane())
    }

    /// Windows and panes draw from one counter, so a dry run never shows a
    /// `w3` beside an unrelated `p3`.
    fn fresh_pane(&mut self) -> PaneRef {
        let pane = PaneRef(self.refs);
        self.refs += 1;
        pane
    }

    /// The live window that is `saved`, if the server holds it.
    fn present(&self, saved: &Window) -> Option<WindowId> {
        let live = self.live?;
        let stamp = Made::ByPhoenix {
            generation: self.generation,
            saved: saved.id(),
        };
        let same_server = matches!(
            (self.snapshot.origin, live.origin),
            (Origin::Recorded(saved_on), Origin::Recorded(live_on)) if saved_on == live_on
        );
        live.windows()
            .iter()
            .find(|w| w.made() == stamp || (same_server && w.id() == saved.id()))
            .map(Window::id)
    }

    /// Where `saved` can be linked from, if it exists yet.
    fn existing(&self, saved: &Window) -> Option<LinkSource> {
        self.built
            .get(&saved.id())
            .map(|window| LinkSource::Built(*window))
            .or_else(|| self.present(saved).map(LinkSource::Live))
    }

    /// What `saved` lacks on the server: each of its links no live link
    /// answers, with the index it lands at.
    fn lacking(
        &mut self,
        name: &SessionName,
        saved: &'a Session,
        live_links: &[WinLink],
    ) -> Vec<(WindowIndex, &'a Window)> {
        // Each saved link is paired with a live link to the same window when
        // the session has one: at the saved index first, then at any index —
        // a window an earlier restore had to place elsewhere is still linked.
        let wanted: Vec<(&WinLink, &Window)> = self.snapshot.windows_of(saved).collect();
        let mut unpaired = live_links.to_vec();
        let mut linked = vec![false; wanted.len()];
        for exact in [true, false] {
            for (at, (link, window)) in wanted.iter().enumerate() {
                let live_id = self.present(window);
                linked[at] = linked[at]
                    || claim(&mut unpaired, |l| {
                        Some(l.window) == live_id && (!exact || l.index == link.index)
                    });
            }
        }
        self.notes.extend(
            wanted
                .iter()
                .zip(&linked)
                .filter(|(_, linked)| **linked)
                .map(|((link, _), _)| Note::WindowPresent {
                    session: name.clone(),
                    index: link.index,
                }),
        );
        self.notes
            .extend(unpaired.iter().map(|link| Note::NotFromSnapshot {
                session: name.clone(),
                index: link.index,
            }));

        // A missing link lands at its saved index unless a live window holds
        // it; then at the next index no live window holds and no saved link
        // wants.
        let live_indices: BTreeSet<WindowIndex> = live_links.iter().map(|l| l.index).collect();
        let mut taken: BTreeSet<WindowIndex> = live_indices
            .iter()
            .copied()
            .chain(wanted.iter().map(|(link, _)| link.index))
            .collect();
        let mut missing = Vec::new();
        for ((link, window), _) in wanted.iter().zip(&linked).filter(|(_, linked)| !**linked) {
            let index = if live_indices.contains(&link.index) {
                let landed = free_from(link.index, &mut taken);
                self.notes.push(Note::Relocated {
                    session: name.clone(),
                    saved: link.index,
                    landed,
                });
                landed
            } else {
                link.index
            };
            missing.push((index, *window));
        }
        missing
    }

    /// Moves the scratch's clients onto `to` and removes it. It goes with
    /// the first restored session rather than at the plan's end: a later
    /// plan attaches to a server that already has sessions, so it has no
    /// account of who made the scratch, and one a restore cut short left
    /// behind would stay — and be saved — as if the user had built it. A
    /// snapshot with no restorable session leaves it standing.
    fn close_scratch(&mut self, to: &SessionName) {
        let closing = self.scratch.take().map(|scratch| {
            [
                Step::SwitchClients {
                    from: scratch.clone(),
                    to: to.clone(),
                },
                Step::KillSession {
                    name: scratch.clone(),
                },
            ]
        });
        self.steps.extend(closing.into_iter().flatten());
    }

    /// Every step for one saved session.
    fn session(&mut self, saved: &'a Session) {
        // [LAW:parse-dont-validate] the one crossing from a saved name to a
        // name tmux can target; every step below holds the proven type.
        let name = match SessionName::parse(saved.name().as_str()) {
            Ok(name) => name,
            Err(unaddressable) => {
                self.notes.push(Note::Unaddressable(unaddressable));
                return;
            }
        };
        let first_step = self.steps.len();

        let live_session = self
            .live
            .and_then(|live| live.sessions().iter().find(|s| s.name() == saved.name()));
        let live_links: Vec<WinLink> = live_session
            .iter()
            .flat_map(|session| session.windows().iter().copied())
            .collect();
        if live_session.is_some() {
            self.notes.push(Note::SessionPresent {
                session: name.clone(),
            });
        }
        let mut missing = self.lacking(&name, saved, &live_links);

        // tmux makes a session with one window. For a session the server
        // lacks, that window is the first one here that has to be built —
        // or, when every window it links exists already, one made only so
        // the session can exist, put where the first link goes for that link
        // to replace.
        let made = live_session.is_none().then(|| {
            let (window, pane) = self.fresh();
            let seed = missing
                .iter()
                .position(|(_, saved)| self.existing(saved).is_none())
                .map(|at| missing.remove(at));
            self.steps.push(Step::CreateSession {
                name: name.clone(),
                window_name: seed.map(|(_, saved)| saved.name().clone()),
                cwd: seed.and_then(|(_, saved)| saved.panes().first().cwd.known().cloned()),
                window,
                pane,
            });
            self.steps.push(Step::MoveWindow {
                window,
                session: name.clone(),
                to: seed
                    .or(missing.first().copied())
                    .map(|(index, _)| index)
                    .expect("a session links at least one window, and this one has none yet"),
            });
            (window, pane, seed)
        });
        self.close_scratch(&name);
        let mut placeholder = None;
        if let Some((window, pane, seed)) = made {
            match seed {
                Some((_, saved)) => self.furnish(saved, window, pane),
                None => placeholder = Some(window),
            }
        }

        for (index, saved) in missing {
            match self.existing(saved) {
                Some(source) => self.steps.push(Step::LinkWindow {
                    source,
                    into: name.clone(),
                    index,
                    replacing: placeholder.take(),
                }),
                None => {
                    let (window, pane) = self.fresh();
                    self.steps.push(Step::NewWindow {
                        session: name.clone(),
                        index,
                        name: saved.name().clone(),
                        cwd: saved.panes().first().cwd.known().cloned(),
                        window,
                        pane,
                    });
                    self.furnish(saved, window, pane);
                }
            }
        }

        // Which window a live session shows is its user's; only a session
        // this plan made is pointed at its saved active window.
        if live_session.is_none() {
            self.steps.push(Step::SelectWindow {
                session: name.clone(),
                index: saved.active(),
            });
        }
        if self.unfinished || self.steps.len() > first_step {
            self.steps.push(Step::SetOption {
                scope: OptionScope::Session(name.clone()),
                key: RESTORED_OPTION,
                value: self.generation.to_string(),
            });
        }
    }

    /// Everything inside a window just created with its first pane: the
    /// other panes, the layout, each pane's content and program, the active
    /// pane — and last the stamp that makes it count as built, so a restore
    /// dropped anywhere before it leaves a window the next plan rebuilds
    /// whole.
    fn furnish(&mut self, saved: &Window, window: WindowRef, first: PaneRef) {
        let mut panes = vec![first];
        for pane in saved.panes().iter().skip(1) {
            let from = *panes.last().expect("starts with the first pane");
            let made = self.fresh_pane();
            self.steps.push(Step::SplitPane {
                from,
                cwd: pane.cwd.known().cloned(),
                pane: made,
            });
            panes.push(made);
            // A split halves the pane it splits, and tmux refuses one with
            // no room left to halve — the fifth pane of a detached 80x24
            // window. Spreading the panes after each split keeps room for
            // the next; the saved layout below is what they end up in.
            self.steps.push(Step::SelectLayout {
                window,
                layout: Layout::parse("tiled").expect("a layout name is not empty"),
            });
        }
        self.steps.push(Step::SelectLayout {
            window,
            layout: saved.layout().clone(),
        });
        for (pane, at) in saved.panes().iter().zip(&panes) {
            // Scrollback first, so it is history by the time the program
            // that produced it starts again on top of it.
            match &pane.content {
                Content::Captured { scrollback, .. } => self.steps.push(Step::ReplayContent {
                    pane: *at,
                    lines: scrollback.clone(),
                }),
                Content::NotCaptured { .. } => {}
            }
            // An idle shell is what every new pane already is, and an
            // unrecovered foreground has no command line to run.
            match &pane.foreground {
                Foreground::Program { argv } => self.steps.push(Step::Relaunch {
                    pane: *at,
                    argv: argv.clone(),
                }),
                Foreground::Shell | Foreground::Unrecovered { .. } => {}
            }
        }
        let active = saved
            .panes()
            .iter()
            .zip(&panes)
            .find(|(pane, _)| pane.index == saved.active())
            .map(|(_, at)| *at)
            .expect("Window::new validated that `active` resolves to a member");
        self.steps.push(Step::SelectPane { pane: active });
        self.steps.push(Step::SetOption {
            scope: OptionScope::Window(window),
            key: Made::OPTION,
            value: Made::option_value(self.generation, saved.id()),
        });
        self.built.insert(saved.id(), window);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoenix_core::{
        ContentFailure, Cwd, HistoryIndicator, Layout, NonEmpty, OffsetDateTime, Pane, PaneId,
        PaneIndex, ServerId, TmuxVersion, Utf8PathBuf, WindowName,
    };

    const GEN: GenerationId = GenerationId(7);

    fn name(raw: &str) -> SessionName {
        SessionName::parse(raw).unwrap()
    }

    fn pane(index: u32) -> Pane {
        Pane {
            id: PaneId(index),
            index: PaneIndex(index),
            cwd: Cwd::parse(format!("/p{index}")),
            foreground: Foreground::Shell,
            content: Content::NotCaptured {
                reason: ContentFailure::NotRecorded,
            },
        }
    }

    fn window_of(id: u32, made: Made, panes: Vec<Pane>, active: u32) -> Window {
        Window::new(
            WindowId(id),
            made,
            WindowName::parse(format!("win{id}")).unwrap(),
            Layout::parse(format!("layout{id}")).unwrap(),
            false,
            NonEmpty::from_vec(panes).unwrap(),
            PaneIndex(active),
        )
        .unwrap()
    }

    /// A one-pane window nobody stamped.
    fn window(id: u32) -> Window {
        window_of(id, Made::NotByPhoenix, vec![pane(0)], 0)
    }

    /// A live window an earlier restore of [`GEN`] built from saved `saved`.
    fn built(id: u32, saved: u32) -> Window {
        let made = Made::ByPhoenix {
            generation: GEN,
            saved: WindowId(saved),
        };
        window_of(id, made, vec![pane(0)], 0)
    }

    /// `links` are `(index, window id)`; the first is the active one.
    fn session(session: &str, links: &[(u32, u32)]) -> Session {
        let links: Vec<WinLink> = links
            .iter()
            .map(|(index, window)| WinLink {
                index: WindowIndex(*index),
                window: WindowId(*window),
            })
            .collect();
        let active = links[0].index;
        Session::new(
            phoenix_core::SessionName::parse(session).unwrap(),
            None,
            NonEmpty::from_vec(links).unwrap(),
            active,
            None,
        )
        .unwrap()
    }

    fn server(pid: u32) -> Origin {
        Origin::Recorded(ServerId { pid, start_time: 1 })
    }

    fn snapshot_on(
        origin: Origin,
        touched: Touched,
        windows: Vec<Window>,
        sessions: Vec<Session>,
    ) -> Snapshot {
        Snapshot::new(
            origin,
            touched,
            OffsetDateTime::from_unix_timestamp(1_700_000_000),
            TmuxVersion { major: 3, minor: 7 },
            NonEmpty::from_vec(windows).unwrap(),
            NonEmpty::from_vec(sessions).unwrap(),
            vec![],
        )
        .unwrap()
    }

    /// A saved snapshot, from a server no test's live server is.
    fn saved(windows: Vec<Window>, sessions: Vec<Session>) -> Snapshot {
        snapshot_on(server(1), Touched::Never, windows, sessions)
    }

    /// A live server other than the one the snapshot came from.
    fn live(touched: Touched, windows: Vec<Window>, sessions: Vec<Session>) -> Snapshot {
        snapshot_on(server(2), touched, windows, sessions)
    }

    fn stamp(window: u32, saved: u32) -> Step {
        Step::SetOption {
            scope: OptionScope::Window(WindowRef(window)),
            key: "@phoenix-window",
            value: format!("7:@{saved}"),
        }
    }

    fn restored(session: &str) -> Step {
        Step::SetOption {
            scope: OptionScope::Session(name(session)),
            key: "@phoenix-restored",
            value: "7".to_owned(),
        }
    }

    fn server_mark() -> Step {
        Step::SetOption {
            scope: OptionScope::Server,
            key: "@phoenix-generation",
            value: "7".to_owned(),
        }
    }

    fn layout(window: u32, saved: u32) -> Step {
        Step::SelectLayout {
            window: WindowRef(window),
            layout: Layout::parse(format!("layout{saved}")).unwrap(),
        }
    }

    fn cwd(index: u32) -> Option<Utf8PathBuf> {
        Utf8PathBuf::parse(format!("/p{index}"))
    }

    fn window_name(saved: u32) -> WindowName {
        WindowName::parse(format!("win{saved}")).unwrap()
    }

    /// The steps that fill a one-pane window made as reference `at` from
    /// saved window `saved`.
    fn furnished(at: u32, saved: u32) -> Vec<Step> {
        vec![
            layout(at, saved),
            Step::SelectPane { pane: PaneRef(at) },
            stamp(at, saved),
        ]
    }

    fn new_window(session: &str, index: u32, at: u32, saved: u32) -> Vec<Step> {
        let mut steps = vec![Step::NewWindow {
            session: name(session),
            index: WindowIndex(index),
            name: window_name(saved),
            cwd: cwd(0),
            window: WindowRef(at),
            pane: PaneRef(at),
        }];
        steps.extend(furnished(at, saved));
        steps
    }

    /// A session created around saved window `saved`, placed at `index`.
    fn new_session(session: &str, index: u32, at: u32, saved: u32) -> Vec<Step> {
        let mut steps = vec![
            Step::CreateSession {
                name: name(session),
                window_name: Some(window_name(saved)),
                cwd: cwd(0),
                window: WindowRef(at),
                pane: PaneRef(at),
            },
            Step::MoveWindow {
                window: WindowRef(at),
                session: name(session),
                to: WindowIndex(index),
            },
        ];
        steps.extend(furnished(at, saved));
        steps
    }

    fn select_window(session: &str, index: u32) -> Step {
        Step::SelectWindow {
            session: name(session),
            index: WindowIndex(index),
        }
    }

    #[test]
    fn onto_no_server_the_scratch_goes_once_a_session_exists_and_the_mark_comes_last() {
        let snapshot = saved(
            vec![window(3), window(4)],
            vec![session("main", &[(4, 3)]), session("side", &[(0, 4)])],
        );
        let scratch = name("phoenix-scratch-0");
        let plan = plan(GEN, &snapshot, Onto::NoServer { scratch: &scratch });

        // Nothing that can fail in the middle of a restore runs while the
        // scratch still stands, bar making the first session.
        let mut expected = new_session("main", 4, 0, 3);
        expected.splice(
            2..2,
            [
                Step::SwitchClients {
                    from: scratch.clone(),
                    to: name("main"),
                },
                Step::KillSession { name: scratch },
            ],
        );
        expected.extend([select_window("main", 4), restored("main")]);
        expected.extend(new_session("side", 0, 1, 4));
        expected.extend([select_window("side", 0), restored("side"), server_mark()]);
        assert_eq!(plan.steps(), expected);
        assert_eq!(plan.notes(), []);
    }

    #[test]
    fn the_scratch_is_removed_only_when_the_connection_made_one() {
        let snapshot = saved(vec![window(3)], vec![session("main", &[(0, 3)])]);
        let other = live(
            Touched::Never,
            vec![window(0)],
            vec![session("mine", &[(0, 0)])],
        );
        let scratch = name("phoenix-scratch-0");
        let closes = |onto| {
            plan(GEN, &snapshot, onto)
                .steps()
                .iter()
                .filter(|s| matches!(s, Step::SwitchClients { .. } | Step::KillSession { .. }))
                .count()
        };

        assert_eq!(closes(Onto::opened(&Opened::Attached, &other)), 0);
        assert_eq!(
            closes(Onto::opened(&Opened::Created(scratch.clone()), &other)),
            2
        );
    }

    #[test]
    fn panes_are_split_in_saved_order_and_the_active_one_is_selected_by_reference() {
        let mut panes = vec![pane(0), pane(1), pane(2)];
        panes[0].content = Content::Captured {
            indicator: HistoryIndicator {
                history_size: 1,
                history_bytes: 1,
            },
            scrollback: vec!["history".to_owned()],
            visible: vec![],
        };
        let argv = NonEmpty::new("vim".to_owned(), vec!["a b".to_owned()]);
        panes[2].foreground = Foreground::Program { argv: argv.clone() };
        let snapshot = saved(
            vec![window_of(3, Made::NotByPhoenix, panes, 1)],
            vec![session("main", &[(0, 3)])],
        );
        let other = live(
            Touched::Never,
            vec![window(0)],
            vec![session("mine", &[(0, 0)])],
        );
        let plan = plan(GEN, &snapshot, Onto::Server { live: &other });

        let spread = Step::SelectLayout {
            window: WindowRef(0),
            layout: Layout::parse("tiled").unwrap(),
        };
        // The panes are spread after each split so the next has room. Every
        // pane exists before the saved layout, the layout before anything is
        // printed into a pane, and the stamp after all of it.
        assert_eq!(
            plan.steps()[2..11],
            [
                Step::SplitPane {
                    from: PaneRef(0),
                    cwd: cwd(1),
                    pane: PaneRef(1),
                },
                spread.clone(),
                Step::SplitPane {
                    from: PaneRef(1),
                    cwd: cwd(2),
                    pane: PaneRef(2),
                },
                spread,
                layout(0, 3),
                Step::ReplayContent {
                    pane: PaneRef(0),
                    lines: vec!["history".to_owned()],
                },
                Step::Relaunch {
                    pane: PaneRef(2),
                    argv,
                },
                Step::SelectPane { pane: PaneRef(1) },
                stamp(0, 3),
            ]
        );
    }

    #[test]
    fn a_snapshot_already_restored_plans_nothing() {
        let snapshot = saved(
            vec![window(3), window(4)],
            vec![
                session("main", &[(0, 3), (1, 4)]),
                session("side", &[(0, 4)]),
            ],
        );
        let after = live(
            Touched::By(GEN),
            vec![built(10, 3), built(11, 4)],
            vec![
                session("main", &[(0, 10), (1, 11)]),
                session("side", &[(0, 11)]),
            ],
        );
        let plan = plan(GEN, &snapshot, Onto::Server { live: &after });

        assert_eq!(plan.steps(), []);
    }

    #[test]
    fn a_window_is_itself_on_the_server_it_was_saved_from() {
        let windows = vec![window(3)];
        let sessions = vec![session("main", &[(0, 3)])];
        let snapshot = snapshot_on(
            server(1),
            Touched::By(GEN),
            windows.clone(),
            sessions.clone(),
        );
        let same = snapshot_on(
            server(1),
            Touched::By(GEN),
            windows.clone(),
            sessions.clone(),
        );
        let restarted = snapshot_on(server(2), Touched::By(GEN), windows, sessions);

        assert_eq!(
            plan(GEN, &snapshot, Onto::Server { live: &same }).steps(),
            []
        );
        // The same id on another incarnation is another window.
        assert_eq!(
            plan(GEN, &snapshot, Onto::Server { live: &restarted }).steps()[0],
            new_window("main", 1, 0, 3)[0]
        );
    }

    #[test]
    fn an_unstamped_window_is_not_counted_and_the_saved_one_is_built_whole_beside_it() {
        // What a restore dropped before the stamp leaves behind: the session,
        // and a window at the saved index that nothing vouches for.
        let snapshot = saved(vec![window(3)], vec![session("main", &[(0, 3)])]);
        let dropped = live(
            Touched::Never,
            vec![window(9)],
            vec![session("main", &[(0, 9)])],
        );
        let plan = plan(GEN, &snapshot, Onto::Server { live: &dropped });

        let mut expected = new_window("main", 1, 0, 3);
        expected.extend([restored("main"), server_mark()]);
        assert_eq!(plan.steps(), expected);
        assert_eq!(
            plan.notes(),
            [
                Note::SessionPresent {
                    session: name("main")
                },
                Note::NotFromSnapshot {
                    session: name("main"),
                    index: WindowIndex(0),
                },
                Note::Relocated {
                    session: name("main"),
                    saved: WindowIndex(0),
                    landed: WindowIndex(1),
                },
            ]
        );
    }

    #[test]
    fn a_taken_index_moves_one_window_past_every_index_the_snapshot_wants() {
        let windows: Vec<Window> = (10..15).map(window).collect();
        let links: Vec<(u32, u32)> = (0..5).map(|i| (i, 10 + i)).collect();
        let snapshot = saved(windows, vec![session("0", &links)]);
        let fresh = live(
            Touched::Never,
            vec![window(0)],
            vec![session("0", &[(0, 0)])],
        );
        let plan = plan(GEN, &snapshot, Onto::Server { live: &fresh });

        let landed: Vec<u32> = plan
            .steps()
            .iter()
            .filter_map(|step| match step {
                Step::NewWindow { index, .. } => Some(index.0),
                _ => None,
            })
            .collect();
        assert_eq!(landed, [5, 1, 2, 3, 4]);
        // The terminal's session keeps showing what it showed.
        assert!(!plan
            .steps()
            .iter()
            .any(|step| matches!(step, Step::SelectWindow { .. })));
    }

    #[test]
    fn a_window_an_earlier_restore_placed_elsewhere_is_still_linked() {
        let snapshot = saved(
            vec![window(10), window(11)],
            vec![session("0", &[(0, 10), (1, 11)])],
        );
        let after = live(
            Touched::By(GEN),
            vec![window(0), built(20, 11), built(21, 10)],
            vec![session("0", &[(0, 0), (1, 20), (2, 21)])],
        );

        assert_eq!(
            plan(GEN, &snapshot, Onto::Server { live: &after }).steps(),
            []
        );
    }

    #[test]
    fn a_shared_window_is_built_once_and_linked_into_the_other_session() {
        let snapshot = saved(
            vec![window(3), window(4)],
            vec![session("a", &[(0, 3)]), session("b", &[(0, 3), (1, 4)])],
        );
        let scratch = name("phoenix-scratch-0");
        let plan = plan(GEN, &snapshot, Onto::NoServer { scratch: &scratch });

        // `b` is created around the one window only it holds.
        let mut b = new_session("b", 1, 1, 4);
        b.extend([
            Step::LinkWindow {
                source: LinkSource::Built(WindowRef(0)),
                into: name("b"),
                index: WindowIndex(0),
                replacing: None,
            },
            select_window("b", 0),
            restored("b"),
        ]);
        // `a`, the scratch's removal inside it, and its two closing steps.
        let a_len = new_session("a", 0, 0, 3).len() + 2 + 2;
        assert_eq!(plan.steps()[a_len..a_len + b.len()], b);
    }

    #[test]
    fn a_session_whose_every_window_exists_is_made_with_a_window_its_first_link_replaces() {
        let snapshot = saved(
            vec![window(3), window(4)],
            vec![
                session("a", &[(0, 3), (1, 4)]),
                session("b", &[(5, 3), (2, 4)]),
            ],
        );
        let scratch = name("phoenix-scratch-0");
        let plan = plan(GEN, &snapshot, Onto::NoServer { scratch: &scratch });

        // `a`, the scratch's removal inside it, and its two closing steps.
        let a_len = new_session("a", 0, 0, 3).len() + new_window("a", 1, 1, 4).len() + 2 + 2;
        assert_eq!(
            plan.steps()[a_len..a_len + 6],
            [
                Step::CreateSession {
                    name: name("b"),
                    window_name: None,
                    cwd: None,
                    window: WindowRef(2),
                    pane: PaneRef(2),
                },
                // Where the first link goes, for that link to take its place.
                Step::MoveWindow {
                    window: WindowRef(2),
                    session: name("b"),
                    to: WindowIndex(5),
                },
                Step::LinkWindow {
                    source: LinkSource::Built(WindowRef(0)),
                    into: name("b"),
                    index: WindowIndex(5),
                    replacing: Some(WindowRef(2)),
                },
                Step::LinkWindow {
                    source: LinkSource::Built(WindowRef(1)),
                    into: name("b"),
                    index: WindowIndex(2),
                    replacing: None,
                },
                select_window("b", 5),
                restored("b"),
            ]
        );
    }

    #[test]
    fn a_window_already_on_the_server_is_linked_by_its_live_id() {
        let snapshot = saved(
            vec![window(3)],
            vec![session("a", &[(0, 3)]), session("b", &[(2, 3)])],
        );
        let half = live(
            Touched::Never,
            vec![built(40, 3)],
            vec![session("a", &[(0, 40)])],
        );
        let plan = plan(GEN, &snapshot, Onto::Server { live: &half });

        assert!(plan.steps().contains(&Step::LinkWindow {
            source: LinkSource::Live(WindowId(40)),
            into: name("b"),
            index: WindowIndex(2),
            replacing: Some(WindowRef(0)),
        }));
        assert!(!plan
            .steps()
            .iter()
            .any(|step| matches!(step, Step::NewWindow { .. })));
    }

    #[test]
    fn a_session_tmux_cannot_target_is_reported_and_the_rest_restored() {
        let snapshot = saved(
            vec![window(3), window(4)],
            vec![session("$odd", &[(0, 3)]), session("main", &[(0, 4)])],
        );
        let scratch = name("phoenix-scratch-0");
        let plan = plan(GEN, &snapshot, Onto::NoServer { scratch: &scratch });

        assert_eq!(
            plan.notes(),
            [Note::Unaddressable(SessionName::parse("$odd").unwrap_err())]
        );
        assert_eq!(plan.steps()[0], new_session("main", 0, 0, 4)[0]);
        assert!(plan.steps().contains(&Step::SwitchClients {
            from: scratch,
            to: name("main"),
        }));
    }

    #[test]
    fn the_scratch_stays_when_there_is_no_restored_session_to_move_its_clients_onto() {
        let snapshot = saved(vec![window(3)], vec![session("$odd", &[(0, 3)])]);
        let scratch = name("phoenix-scratch-0");
        let plan = plan(GEN, &snapshot, Onto::NoServer { scratch: &scratch });

        assert_eq!(plan.steps(), [server_mark()]);
    }

    #[test]
    fn the_scratch_name_is_free_of_every_saved_session_name() {
        let snapshot = saved(
            vec![window(3)],
            vec![
                session("phoenix-scratch-0", &[(0, 3)]),
                session("phoenix-scratch-1", &[(0, 3)]),
            ],
        );
        assert_eq!(scratch_name(&snapshot).as_str(), "phoenix-scratch-2");
    }

    #[test]
    fn a_dry_run_reads_as_tmux_commands_over_references() {
        let mut panes = vec![pane(0), pane(1)];
        panes[1].foreground = Foreground::Program {
            argv: NonEmpty::new("vim".to_owned(), vec!["it's".to_owned()]),
        };
        let snapshot = saved(
            vec![window_of(3, Made::NotByPhoenix, panes, 1)],
            vec![session("main", &[(2, 3)])],
        );
        let scratch = name("phoenix-scratch-0");
        let plan = plan(GEN, &snapshot, Onto::NoServer { scratch: &scratch });

        let printed: Vec<String> = plan.steps().iter().map(Step::to_string).collect();
        assert_eq!(
            printed,
            [
                "w0 p0 = new-session -s main -n win3 -c /p0",
                "move-window w0 to main:2",
                "switch-client every client on phoenix-scratch-0 to main",
                "kill-session phoenix-scratch-0",
                "p1 = split-window p0 -c /p1",
                "select-layout w0 tiled",
                "select-layout w0 layout3",
                r"send-keys p1 'vim' 'it'\''s'",
                "select-pane p1",
                "set-option w0 @phoenix-window 7:@3",
                "select-window main:2",
                "set-option session main @phoenix-restored 7",
                "set-option server @phoenix-generation 7",
            ]
        );
    }
}
