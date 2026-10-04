# tmux-phoenix — target architecture

**What this document is.** The laws-aligned shape of tmux-phoenix: what the parts are,
where the seams fall, what each seam is made of, and which facts live where. It is the
synthesis of `LAWS-AUDIT-2026-10-04.md`; read that for *why* the current shape is
wrong, this for *what* right looks like. It is deliberately big-picture — crate
purposes, types at the boundaries, dependency direction, ownership of facts — and it
considers how the shape will be implemented only enough to show the shape is buildable.
Implementation detail belongs in the tickets under the `tmux-laws` epic and, once
built, in `DESIGN.md`, which this document supersedes section by section as the work
lands.

The three product priorities are unchanged — **stability, speed, efficiency** — and so
are the deliberate divergences in `PROJECT-GOALS.md` §5: one persistent control-mode
connection, event-driven save timing, content-addressed scrollback. This architecture
is how to honor those with far less code.

---

## 1. The one idea

Every finding in the audit traces to a fact phoenix needed but did not represent, and
so inferred from timing-sensitive evidence and then guarded against everywhere. The
whole redesign is one move, applied repeatedly:

> **Record the fact where it is born; make it a value in a type; let everything
> downstream read the value.** Never infer it later, never re-check it inland.

Applied to the four facts that matter:

| Fact | Where it is born | Where it lives | What it replaces |
| --- | --- | --- | --- |
| Which **server incarnation** a snapshot came from | the capture, from tmux's `#{pid}:#{start_time}` | the generation header (`origin: ServerId`) | the entire boot-decision state machine |
| Which **sessions phoenix created** | the restore plan that creates them | a tmux session user option (`@phoenix-created`) | the bootstrap heuristic and scaffolding retire/put-back |
| What a **pane's foreground** is | the capture, from the OS's exact argv | a closed `Foreground` enum on `Pane` | `Foreground` re-derived on every read via a shell-name list |
| Where a **restored pane** is | the `split-window -P` reply at apply time | a bound `PaneRef` in the plan | "the current pane, if you issue this step immediately" |

Everything else in this document is consequence.

---

## 2. Parts and dependency direction

Dependencies flow strictly downhill (`[LAW:one-way-deps]`). Each crate's purpose is one
sentence with no conjunction (`[LAW:decomposition]`).

```
phoenix-cli                       maps command lines onto phoenix-ops
phoenix-daemon                    keeps one tmux server's state alive across restarts
phoenix-ops                       the one implementation of each user-visible operation
phoenix-capture   phoenix-restore   phoenix-store
tmux-control      phoenix-core
```

- **`tmux-control`** — a complete, standalone client for the tmux control-mode
  protocol. The only crate that spawns a process or reads a byte from a tmux server.
- **`phoenix-core`** — the pure domain model (`Snapshot` and everything in it). No I/O.
- **`phoenix-capture`** — reads a live server into a `Snapshot`.
- **`phoenix-store`** — atomic, versioned, generational persistence of snapshots.
- **`phoenix-restore`** — turns a `Snapshot` and a live `Snapshot` into a `Plan`, and
  applies a `Plan` over a connection.
- **`phoenix-ops`** *(new)* — composes the three middle crates into exactly one
  `save`, one `restore`, and one `status`, so the CLI and the daemon cannot drift.
- **`phoenix-daemon`** — a pure state machine over connection events and clock ticks,
  plus the thin loop that feeds it.
- **`phoenix-cli`** — argument parsing and exit codes.

Two edges in today's graph are gone: `phoenix-restore → phoenix-capture` (restore
needed "what does the server hold?" — in the new shape the *caller* captures the live
server and hands restore a `Snapshot`, so restore never reads anything) and the plain
`tmux` subprocess channel inside restore (§4). `phoenix-store` no longer knows what a
session is for; `phoenix-capture` no longer knows the store exists; neither ever did
by design, and the ops layer is where that composition legitimately lives.

---

## 3. Provenance — the fact that dissolves the daemon's state machine

A tmux server incarnation is identified by `ServerId = #{pid}:#{start_time}`. It
changes when the server restarts on the same socket and is the same across every
session of one server (verified live on tmux 3.7b, 2026-10-04). Today it is read but used only
to recognise a reconnect; it belongs in the data.

- **Every `Snapshot` carries `origin: ServerId`.** Capture reads it over the same
  connection it reads everything else. The store writes it into the generation header
  next to `captured_at`, so `list` can show it without decoding a body.
- **Every session phoenix creates is stamped** with a session user option
  `@phoenix-created=<generation id>` by the restore plan itself (a `SetOption` step —
  data in the plan, not a side channel). tmux stores user options durably for the
  session's life; verified live that a session option round-trips through
  `display-message -p '#{@…}'`.

With those two values, the daemon's boot question has a one-line answer:

> **Restore on boot if and only if the store holds no generation whose `origin` is
> this server.**

A server the daemon has never saved from is a fresh server; it gets the newest
snapshot. A server it has saved from — including the one it just lost the connection
to — is never restored over. No process table is read; no decision is provisional;
nothing is re-checked after the first save; a reconnect is the same question with the
same answer. The store's `BootstrapOnly` refusal goes away with the reason it existed:
a near-empty capture from a fresh server is simply a generation from *that* server,
and the generation picker/`restore --file` keep every older one reachable. If "the
newest generation from a *previous* incarnation" is later wanted as the boot default
instead of "the newest generation", that is one filter over the same header field —
data, not a mode.

The session stamp answers the only other question restore ever asks: *may I remove
this session?* **Phoenix removes only sessions it created.** A login terminal's
session `0` holding one idle shell is not phoenix's to kill; restore adds beside it
and moves its attached clients onto a restored session (`switch-client`, which a
control client can also do for itself). Whether the stray then disappears is the
user's tmux configuration (`destroy-unattached`), not phoenix's guess. The scaffolding
`Created/Renamed`, `retire`, `put_back`, and the post-restore re-capture all go.

---

## 4. `tmux-control` — one channel, owned reader, typed events

The codec, `ServerMessage`, `CommandLine`, `ConnectionState`, and `execute` stay as
they are. Three additions make the layers above it pure:

**A connection opens on any server.** `Connection::open(socket, attach: Attach)`
where `Attach::{Existing, OrCreate { name }}`. On a server with sessions it is `tmux
-C attach-session`; on an empty or absent server it is `tmux -C new-session -s
<name>`, which opens control mode and creates the session in one step (verified live
on tmux 3.7b). The probe that decides between them ("does this server have sessions?")
lives *here*, in the transport, as the one place a plain `tmux` is ever spawned. The
two error vocabularies collapse to `TmuxError`. Nothing above this crate spawns `tmux`
again.

**The client owns its reader.** A reader thread (or task) owns the transport's read
side and delivers `Event::{Notification(ServerMessage), PaneOutput(PaneId, Vec<u8>),
Closed(CloseReason)}` on a channel; `execute` correlates replies off the same stream.
A caller that wants to react to notifications while idle *waits on the channel*,
optionally with a deadline. The heartbeat command and the poll interval disappear
(`[LAW:no-ambient-temporal-coupling]`: the one owner of "when does a notification
arrive" is the reader).

**Typed targets.** `Target::{Session(SessionName), Window(SessionName, WindowIndex),
Pane(PaneId)}` renders to tmux's target syntax in one place, exact-match (`=name`)
where tmux allows it. `split-window`/`new-window` return the created id (`-P -F
'#{pane_id}'`, verified live) as a typed value.

---

## 5. `phoenix-core` — the strongest true theorem about a tmux server

tmux's structure is not a tree. A session holds an ordered set of *winlinks* to
windows; a window may be linked into several sessions (grouped sessions, `link-window`);
a window holds panes. The model says exactly that, so grouped sessions are a data fill
rather than a redesign (`[LAW:composability]`'s mirror-signal):

```
Snapshot   { origin: ServerId, captured_at, tmux_version, windows: NonEmpty<Window>,
             sessions: NonEmpty<Session> }
Session    { name: SessionName, group: Option<GroupName>,
             windows: NonEmpty<WinLink>, active: WindowIndex, last: Option<WindowIndex> }
WinLink    { index: WindowIndex, window: WindowRef }          // index is per-session
Window     { id: WindowId, name, layout: Layout, zoomed: Option<PaneIndex>,
             panes: NonEmpty<Pane>, active: PaneIndex }
Pane       { id: PaneId, index: PaneIndex, cwd: Cwd, foreground: Foreground, content: Content }
```

Constructors keep the invariants that exist today (every `active`/`last`/`zoomed`
resolves; indices unique) and add one: every `WindowRef` resolves to a window in
`Snapshot.windows`, and every window is linked by at least one session.

**Absence is a variant with a reason, never a bare `Option`:**

```
Cwd        = Known(Utf8PathBuf) | Unreadable
Foreground = Shell                                  // idle at the pane's own shell
           | Program { argv: NonEmpty<String>, identity: Option<AgentSession> }
           | Unrecovered { reason: RecoveryFailure }
Content    = Captured { indicator: HistoryIndicator, scrollback: Blob, visible: Blob }
           | NotCaptured { reason: ContentFailure }
```

`Foreground` is decided **once, at capture**, from the OS's exact argv
(`/proc/<pid>/cmdline` on Linux, `KERN_PROCARGS2` on macOS — NUL-separated, no
whitespace splitting), and is the discriminator every consumer matches on
exhaustively. `AgentSession` is the hook the LLM-resume epic needs; per-program
strategies (vim session files, mosh) are a pure function `Foreground -> Relaunch`
inside restore, keyed on `argv[0]` — one type, N instances (`[LAW:one-type-per-behavior]`).

**Degraded** is a derived view in one place: `Snapshot::degradations() ->
Vec<Degradation>` walks the variants and lists each pane's `Unreadable` /
`Unrecovered` / `NotCaptured` with its reason. The CLI's exit code 3 and the daemon's
log line both read it; neither reimplements it.

The two `PaneId`/`TmuxVersion` twins between `phoenix-core` and `tmux-control` stay:
the crates are independent foundations by design and capture converts at the seam.

---

## 6. `phoenix-capture` — always the same operations, variability in the values

Capture is one fixed sequence: one `list-panes -a -F` for structure (now including
`window_id`, `window_zoomed_flag`, `session_group`, the last-window flag, and the
history indicator), one OS argv read per pane pid, one `capture-pane` per pane, one
version/`ServerId` read. **Content capture is not a mode.** It always runs
(`[LAW:dataflow-not-control-flow]`); what varies is the `Previous: HashMap<PaneId,
HistoryIndicator + Blob>` the caller passes — empty on a cold start, the last
generation's on every other call — and a pane whose indicator matches reuses its blob.
`ContentCapture::Off` is deleted, and with it the one-shot `save` that silently
captured nothing.

Capture's effects are confined to its edge function; the fold from rows + argv +
content into a `Snapshot` stays pure. A failed `ps`/procfs read is a loud,
per-pane `Unrecovered { reason }`, never an empty map.

---

## 7. `phoenix-store` — one clock, readers inside the lock

- **`latest` is derived, not stored.** The generation with the highest id *is* latest;
  the symlink was a second representation of that fact and is removed. `load_latest`
  is `list().first()`.
- **Readers share the lock the writer holds exclusively**, so a listing is a
  consistent view and `NoLatest` can only mean "no generation exists" (closes
  `tmux-store-i22` by construction).
- **The header is the generation's identity:** `format_version, origin: ServerId,
  captured_at, tag: Option<Tag>, body_len, checksum`. `captured_at` is written once.
  A `tag` exempts a generation from pruning — named snapshots are a header field, not
  a second store.
- **Retention is a value** (`Retention { keep_untagged: NonZeroUsize }`) the caller
  passes; blob pruning is a sweep over the union of surviving generations' references.
- The body encodes the §5 graph (windows once, winlinks by reference).

---

## 8. `phoenix-restore` — a plan is a small program; apply binds its variables

`plan(snapshot: &Snapshot, live: &Snapshot) -> Plan` is pure and takes **both** the
snapshot to restore and a capture of the target server. The plan is the *difference*:
sessions that already exist on `live` are not created (idempotent restore falls out),
and the same function with a renderer instead of an executor is the human-readable
diff the toolbox roadmap asks for. Selective restore (one session, one window) is a
filter on `snapshot` before planning — data, not a mode.

Steps carry **symbolic references** bound at apply time, so no step depends on what
"current" happens to be:

```
Plan { steps: Vec<Step> }
Step = CreateSession { name, first_window, cwd }            -> binds PaneRef
     | NewWindow     { session, index, name, cwd }          -> binds PaneRef
     | SplitPane     { in: PaneRef, cwd }                   -> binds PaneRef
     | SelectLayout  { window, layout }
     | ReplayContent { pane: PaneRef, blob }
     | Relaunch      { pane: PaneRef, command: Relaunch }
     | SetOption     { target, key, value }                 // e.g. @phoenix-created
     | SelectWindow  { session, index } | Zoom { pane: PaneRef } | …
     | Hook          { point: HookPoint }                   // pre/post, user-configured
```

`apply` runs steps in order over one connection; `-P -F '#{pane_id}'` replies bind
`PaneRef`s; `--dry-run` renders the same steps with symbolic names. The plan is the
single place that knows the order constraints tmux imposes (`move-window` after
`new-session`, layout after all panes exist), and it encodes them as sequence in
*data*, not as a contract between functions.

**Connecting is not restore's job.** `connect_and_apply` is gone. The ops layer opens
a connection (§4's `Attach::OrCreate` on an empty server), captures `live`, plans,
applies, switches the connection onto a restored session, and removes only the session
phoenix created to attach with — all over one connection, with no reconnect.

---

## 9. `phoenix-daemon` — a pure state machine fed by events

```
Daemon::step(&mut self, event: Event, now: Instant) -> Vec<Action>

Event  = Connected { server: ServerId, has_generation_from_this_server: bool }
       | StructureChanged | Tick | Disconnected
Action = Restore(GenerationId) | Save | Log(String) | WaitThen(Duration)
```

The decision logic — boot-restore-or-not, debounce, max-interval backstop, reconnect
backoff — is a pure function of state and event, and `DebounceState` is already most
of it. The loop around it is a dozen lines: open a connection (§4), subscribe once with
an attached-session-scope format that loops every session
(`#{S:#{W:#{window_layout}}}` — verified live to fire for a change in another
session), then `select` on the event channel and a timer, feed `step`, perform the
actions through `phoenix-ops`. No heartbeat, no poll interval, no `Boot`, no
`Decided`, no `Cycle::Refused`, no "end the run so the outer loop boots again".

The daemon's control client attaches wherever it attaches; because phoenix never kills
a session it did not create and switches rather than reconnects, which session that is
no longer matters.

---

## 10. `phoenix-ops` — one implementation per operation

```
save(conn, store, retention)        -> SaveReport   { generation, degradations }
restore(conn, store, source, scope) -> RestoreReport{ applied, skipped, degradations }
status(store, conn?)                -> Status       { last_save, origin, daemon_state }
```

`save` is: read the previous generation's indicators from the store, capture with
them, write. `restore` is §8's sequence. The CLI prints a report; the daemon logs one;
neither composes the lower crates itself. The hooks ticket (`tmux-parity-ure.9`)
lands here as `Step::Hook` edges in the plan plus a `SetOption` stamp
(`@phoenix-restored=<generation>`) that is the one writer of "restore finished" a
tmux-side integration can wait on — the completion signal is a value tmux holds, not
a file or a sleep.

---

## 11. How the roadmap lands on this shape

Every open feature becomes data on an existing seam rather than a new mode:

| Roadmap item | Lands as |
| --- | --- |
| Zoom, alternate window/session, grouped sessions | fields in §5's graph; two format vars in capture; two step kinds in the plan |
| Idempotent restore, diff/preview, selective restore | `plan(snapshot, live)` + a filter |
| One-shot save with content | the only save there is |
| vim/mosh/LLM resume strategies | `Foreground -> Relaunch`, one function, keyed by `argv[0]` |
| Named/tagged snapshots, generation picker | a header field; `list()` with it |
| Hooks and the restore-finished signal | `Step::Hook`, `Step::SetOption` |
| Status-line format string, TPM plugin | `status()` rendered by a thin script |
| Multi-server daemon | N state machines, one loop, keyed by socket |
| Cross-machine portability | the store is already a directory of self-describing files; `origin` says where each came from |
| Secrets exclusion | a per-pane/session tmux option read in capture, producing `Content::NotCaptured { reason: Excluded }` |

---

## 12. Order of work

The dependency among the changes, which is also the order that keeps the tree
shippable at every step:

1. **`tmux-control`:** open-on-any-server, owned reader + event channel, typed
   targets, `-P` ids. Pure addition; nothing above breaks.
2. **`phoenix-core` + capture + store:** the §5 graph with reasoned absences,
   `origin` in the header, exact argv, content always on, `latest` derived, reader
   lock. One format-version bump; old generations read via the old decoder.
3. **`phoenix-restore`:** symbolic plan over `(snapshot, live)`, `apply` binding refs,
   no connecting, no scaffolding. Delete `connect.rs`.
4. **`phoenix-ops`**, then rewire the CLI onto it.
5. **`phoenix-daemon`:** the state machine; delete `boot.rs`, the heuristic tests, and
   `is_bootstrap`/`Foreground::IdleShell` from core once nothing reads them.
6. **`DESIGN.md`** rewritten section by section to describe the result at the altitude
   of intent, and `PROJECT-GOALS.md` §3's gap table updated.

Each step is its own ticket under the `tmux-laws` epic, each with the live-tmux
verification it needs, and each leaves less code than it found
(`[LAW:polishing-by-subtraction]`): the expected net is roughly 3,000 lines removed
across `connect.rs`, `boot.rs`, half of `daemon.rs`, and the tests that pinned the
heuristic, against a few hundred added in `tmux-control` and `phoenix-ops`.
