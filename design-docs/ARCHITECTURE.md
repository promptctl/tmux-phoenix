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
| Whether **phoenix has touched this server** | the save that lands or the restore that finishes | a tmux server option (`@phoenix-generation`), read as `Touched` at connect | the entire boot-decision state machine |
| Which **server incarnation** a snapshot came from | the capture, from tmux's `#{pid}:#{start_time}` | the generation header (`origin: Origin`) | nothing yet — it is provenance for `list`, `status` and import |
| Which **live window is a saved window** | the plan step that finishes building it | a tmux window option (`@phoenix-window`), read into `live` | matching by name or index, which is a guess |
| Which **session phoenix made in order to attach** | `Connection::open`'s reply | `Opened::Created(name)`, a value the restore plan ends by removing | the bootstrap heuristic, scaffolding retire/put-back, the post-restore reconnect |
| What a **pane's foreground** is | the capture, from the OS: the pane terminal's foreground process group, then that process's exact argv | a closed `Foreground` enum on `Pane` | `Foreground` re-derived on every read via a shell-name list over `ps` output |
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

- **Every `Snapshot` carries `origin: Origin`,** `Recorded(ServerId)` at capture. Capture reads it over the same
  connection it reads everything else. The store writes it into the generation header
  next to `captured_at`, so `list` can show it without decoding a body.
- **The server is marked the moment phoenix touches it.** A tmux *server* user option
  `@phoenix-generation=<generation id>` is written by `save` once the generation has
  landed and by the restore plan as its final step. A server option lives exactly as
  long as the server incarnation and no longer (verified live on 3.7b: `set-option -s
  @…` round-trips through `show-options -s -v`), so it is the incarnation's own memory
  of having been phoenix's, whichever operation — CLI or daemon — made it so. Capture
  reads it into `Snapshot.touched: Touched = By(GenerationId) | Never`.
- **Every window the plan builds is stamped** with a window user option
  `@phoenix-window=<generation id>:<saved window id>` as the *last* step of that
  window's group, after its panes, layout, content and relaunches. A window option
  follows the window through `link-window` (verified live), so a shared window carries
  one stamp however many sessions link it. Capture reads it into `Window.made: Made =
  ByPhoenix { generation, saved: WindowId } | NotByPhoenix`.
- **Every session a restore finishes is stamped** `@phoenix-restored=<generation id>`
  by the same plan. Its reader is outside phoenix: a terminal integration that waits
  for "restore finished" waits on this value (§10), not on a file or a sleep.

With the server mark recorded, the daemon's boot question has a one-line answer:

> **Restore on boot if and only if the store holds a generation and the server is
> `Touched::Never`.**

A server phoenix has saved from or restored onto — including the one the daemon just
lost the connection to, and one the user restored onto by hand before starting the
daemon — is never restored over. Any other server receives the newest snapshot
*beside* whatever it already holds: `plan(snapshot, live)` is the difference over
recorded identity (§8), so nothing live is removed, nothing live is mistaken for a
saved thing by its name or index, and every saved window that is not already present
is created. A login terminal's fresh session `0` is handled without deciding whether it
is "untouched": the saved `0`'s windows are all added into it — at their saved index
when it is free, otherwise the next free one, which the report names — and the one
window the terminal's shell occupies stays, as does the terminal's client. The costs,
stated plainly: a daemon that was stopped while the user built sessions on a new
server will, when started, add the previous incarnation's sessions beside them; a user
who kills their server to start clean gets it restored, because keeping state alive
across a server's death is the product; and a terminal attached to a session whose
name the snapshot lacks stays on that session, with the restored ones one
`switch-client` away. These replace the README's "into a server holding nothing you
built" and "which it replaces while keeping that terminal attached" with "adds what the
server lacks and removes nothing", and step 6 of §12 rewrites those lines.

No process table is read; no decision is provisional; nothing is re-checked after the
first save; a reconnect is the same question with the same answer. The store's
`BootstrapOnly` refusal goes away with the reason it existed: a near-empty capture from
a fresh server is simply a generation from *that* server, and the generation
picker/`restore --file` keep every older one reachable. If "the newest generation from
a *previous* incarnation" is later wanted as the boot default instead of "the newest
generation", that is one filter over the header's `origin` — data, not a mode.

A server that was running before the upgrade carries no mark, so the first daemon boot
after upgrading takes the "any other server" arm: the newest generation, saved from
that very server by the old daemon, is planned against the live server, and the
difference is every session and every window closed since that save — which the old
daemon took promptly only for the attached session, and at the max-interval backstop
for the rest. That is the accepted one-time cost of the format bump, not a case the
state machine distinguishes.

The question "may I remove this session?" is no longer asked of tmux. The only session
phoenix ever removes is the one `Connection::open` reports it created in order to
attach (§4), and that fact travels as a value — `Opened::Created(name)` — into the plan,
which ends by moving the clients attached to it onto a restored session and killing it
by that name. A login terminal's session `0`, or anything else the user built, is never
phoenix's to kill; whether a stray disappears is the user's tmux configuration
(`destroy-unattached`), not phoenix's guess. The scaffolding `Created/Renamed`,
`retire`, `put_back`, and the post-restore re-capture all go.

---

## 4. `tmux-control` — one channel, owned reader, typed events

The codec, `ServerMessage`, `CommandLine`, `ConnectionState`, and `execute` are
unchanged by this work. Three additions make the layers above it pure:

**A connection opens on any server.** `Connection::open(socket, attach: Attach, events)
-> (Connection, Opened)` where `Attach::{Existing, OrCreate { name }}` is what the caller
permits and `Opened::{Attached, Created(SessionName)}` is what happened. `Existing`
runs `tmux -C attach-session` and fails with a typed `NoSessions` error — tmux presents
"no server on this socket" and "a server holding no sessions" identically (verified live
on 3.7b), and for every caller they mean the same thing: there is no session here.
`OrCreate` runs the same command and, on exactly that error, `tmux -C new-session -s
<name>`, which opens control mode and creates the session in one step (verified live
on tmux 3.7b). The failed attach *is* the signal; there is no prior "does this server
have sessions?" probe to race against, and no plain `tmux` is spawned anywhere. Two
callers, each with one reason: the daemon's idle loop passes `Existing`, because
whether to start a server is the state machine's decision (§9: only when the store
holds something to put on it), not the transport's; a restore passes `OrCreate`,
because that decision has been made. The two error vocabularies
collapse to `TmuxError`. Nothing above this crate spawns `tmux` again.

**The client owns its reader.** A reader thread (or task) owns the transport's read
side and delivers `Event::{Notification(ServerMessage), PaneOutput(PaneId, Vec<u8>),
Closed(CloseReason)}` to the sink the caller gave `open`; `execute` correlates replies
off the same stream. The sink is the caller's, so the crate holds no queue on anyone's
behalf: a caller that wants to react to notifications while idle forwards to a channel
of its own and *waits on that*, optionally with a deadline; a caller that only executes
passes `drop`. The sink is only ever called from the reader thread. The heartbeat command and the poll interval disappear
(`[LAW:no-ambient-temporal-coupling]`: the one owner of "when does a notification
arrive" is the reader).

**Typed targets.** `Target::{Session(SessionName), SessionId, Window(SessionName,
WindowIndex), WindowId, Pane(PaneId)}` renders to tmux's target syntax in one place,
exact-match (`=name:` and `=name:=index` — the trailing colon is what keeps a window- or
pane-taking command from prefix-matching the session, and the second `=` is what keeps an
absent index from matching a window by name) where tmux allows it. `Connection::abort_handle()`
ends the link from any thread, releasing a caller blocked in `execute` on a server that
stopped answering. `split-window`/`new-window` return the created id (`-P -F
'#{pane_id}'`, verified live) as a typed value.

---

## 5. `phoenix-core` — the strongest true theorem about a tmux server

tmux's structure is not a tree. A session holds an ordered set of *winlinks* to
windows; a window may be linked into several sessions (grouped sessions, `link-window`);
a window holds panes. The model says exactly that, so grouped sessions are a data fill
rather than a redesign (`[LAW:composability]`'s mirror-signal):

```
Snapshot   { origin: Origin, touched: Touched, captured_at, tmux_version,
             windows: NonEmpty<Window>, sessions: NonEmpty<Session>, clients: Vec<Client> }
Session    { name: SessionName, group: Option<GroupName>,
             windows: NonEmpty<WinLink>, active: WindowIndex, last: Option<WindowIndex> }
WinLink    { index: WindowIndex, window: WindowRef }          // index is per-session
Window     { id: WindowId, made: Made, name, layout: Layout, zoomed: Option<PaneIndex>,
             panes: NonEmpty<Pane>, active: PaneIndex }
Client     { name: ClientName, session: SessionName }         // from list-clients, same connection
Pane       { id: PaneId, index: PaneIndex, cwd: Cwd, foreground: Foreground, content: Content }
```

Constructors keep the invariants that exist today (every `active`/`last`/`zoomed`
resolves; indices unique) and add one: every `WindowRef` resolves to a window in
`Snapshot.windows`, and every window is linked by at least one session.

**Absence is a variant with a reason, never a bare `Option`:**

```
Origin     = Recorded(ServerId) | BeforeOriginWasRecorded   // the latter only from the old decoder
Touched    = By(GenerationId) | Never                        // the server option of §3
Made       = ByPhoenix { generation: GenerationId, saved: WindowId } | NotByPhoenix   // the window option of §3
Cwd        = Known(Utf8PathBuf) | Unreadable
Foreground = Shell                                  // idle at the pane's own shell
           | Program { argv: NonEmpty<String>, identity: Option<AgentSession> }
           | Unrecovered { reason: RecoveryFailure }
Content    = Captured { indicator: HistoryIndicator, scrollback: Blob, visible: Blob }
           | NotCaptured { reason: ContentFailure }
```

`Foreground` is decided **once, at capture**, in two OS reads keyed by the pane's pid
and nothing else. `pane_pid` is the pane's shell, so the first read asks the kernel
which process group holds the pane's terminal: `tpgid` from `/proc/<pane_pid>/stat` on
Linux, `kinfo_proc.kp_eproc.e_tpgid` from `sysctl KERN_PROC_PID` on macOS — the same
fact `ps` renders as its `+` flag, read directly instead of walked (verified live on an
isolated server, 2026-10-04: a pane running `sleep 300` reports the sleep's pid as the
shell's `tpgid`, and the shell's own pid once the sleep is interrupted). When it equals
the shell's own group the pane is at its own process; otherwise a job holds the
terminal. The second read is the group leader's exact argv (`/proc/<pid>/cmdline`,
`KERN_PROCARGS2` — NUL-separated, no whitespace splitting). A pane at its own process
is `Shell` when `argv[0]`, with a login shell's leading `-` removed, has its basename
in the shell set capture is given (`Shells`, a value with a default list, not a
constant inside core) *and* every remaining argument begins with `-` — the rule
`phoenix-core::program::foreground` applies today, kept because `bash ./watch.sh` is a
program and `bash -l` is not. Everything else — a job, or a pane whose own process is
`vim`, `ssh`, a script, a `default-command` — is `Program` with that argv. A group leader that exited while its pipeline lives (`make | less`) is
`Unrecovered { reason: LeaderGone }`, not a guess at which survivor to relaunch. The
result is the discriminator every consumer matches on exhaustively. `AgentSession` is the hook the LLM-resume epic needs; per-program
strategies (vim session files, mosh) are a pure function `Foreground -> Relaunch`
inside restore, keyed on `argv[0]` — one type, N instances (`[LAW:one-type-per-behavior]`).

**Degraded** is a derived view in one place: `Snapshot::degradations() ->
Vec<Degradation>` walks the variants and lists each pane's `Unreadable` /
`Unrecovered` / `NotCaptured` with its reason. The CLI's exit code 3 and the daemon's
log line both read it; neither reimplements it.

The two `PaneId`/`TmuxVersion` twins between `phoenix-core` and `tmux-control` are
unchanged by this work: the crates are independent foundations and capture converts at
the seam.

---

## 6. `phoenix-capture` — always the same operations, variability in the values

Capture is one fixed sequence: one `list-panes -a -F` for structure (now including
`window_id`, `window_zoomed_flag`, `session_group`, the last-window flag, and the
history indicator), the §5 foreground reads per pane, one `capture-pane` per pane, one
version/`ServerId` read. **Content capture is not a mode.** It always runs
(`[LAW:dataflow-not-control-flow]`); what varies is the `Previous: HashMap<PaneId,
HistoryIndicator + Blob>` the caller passes — empty on a cold start, the last
generation's on every other call — and a pane whose indicator matches reuses its blob.
`ContentCapture::Off` is deleted, and with it the one-shot `save` that silently
captured nothing.

Capture's effects are confined to its edge function; the fold from rows + argv +
content into a `Snapshot` stays pure. A failed process read — the pane's shell gone, a
group leader that exited between the two reads — is a loud, per-pane
`Unrecovered { reason }`, never an empty map.

---

## 7. `phoenix-store` — one clock, reads of one state

- **`latest` is derived, not stored.** The generation with the highest id *is* latest;
  the symlink was a second representation of that fact and is removed. `load_latest`
  is `list().first()`.
- **A read is of one state of the store, and takes no lock.** A save only publishes an
  immutable generation above every other id, or removes generations, so the id set
  names the store's state: a read lists the ids, reads, and lists again, and an
  unchanged set means no save landed in between; a changed one runs the read again.
  A listing is a consistent view and `NoLatest` can only mean "no generation exists"
  (closes `tmux-store-i22` by construction). The first build of this section had
  readers share the save lock; `flock` grants a shared lock whenever no exclusive one
  is held, so overlapping readers starved saves (seen live, 2026-10-04), and the lock
  was removed rather than repaired with a second one. A blob sweep must delete a
  generation's file before its blobs, so a read that loses a blob also sees the id
  set move.
- **The header is the generation's identity:** `format_version, header_len, origin:
  Origin, captured_at, tag: Option<Tag>, body_len, checksum`. The header carries its
  own length so `tag` can vary and `list` still reads headers without decoding a body.
  `captured_at` is written once. A `tag` exempts a generation from pruning — named
  snapshots are a header field, not a second store.
- **Retention is a value** (`Retention { keep_untagged: NonZeroUsize }`) the caller
  passes; blob pruning is a sweep over the union of surviving generations' references.
- The body encodes the §5 graph (windows once, winlinks by reference).

---

## 8. `phoenix-restore` — a plan is a small program; apply binds its variables

`plan(snapshot: &Snapshot, live: &Snapshot, opened: Opened) -> Plan` is pure and
takes the snapshot to restore, a capture of the target server, and §4's account of how
the connection was opened. The plan is the *difference over recorded identity*. A
saved window is present on `live` when a live window's `made` is `ByPhoenix` with that
generation and saved id — never when something merely sits at the same index or bears
the same name. A saved session is present when a live session has its name, because
tmux makes names unique. The difference is then mechanical: a saved session absent from
`live` is created with its first window; every saved window absent from `live` is
created once, in the first session that links it, at its saved index when free and
otherwise the next free index, which the report names; every saved winlink whose
window is present but not linked into that session at that index becomes a
`LinkWindow`, so a grouped window is built once and shared, exactly as the snapshot
holds it. A window the plan creates is stamped only as the last step of its group, so a
restore the connection dropped halfway leaves an unstamped partial window the next
plan does not count — it builds the saved window whole and the report lists the
partial one as live and not phoenix's. Idempotent restore falls out; so does completing
an interrupted one. The same function with a renderer instead of an executor is the
human-readable diff the toolbox roadmap asks for. Selective restore (one session, one
window) is a filter on `snapshot` before planning — data, not a mode.

Clients are moved by name, never by implication: `live.clients` says which client sits
on which session, and when `opened` is `Created(name)` the plan ends with a
`SwitchClient { client, to }` for each client attached to `name` and then `KillSession
{ name }`; when it is `Attached` no client is moved, because every client is on a
session the user chose. The ops layer chooses `name` free of every session name in the
snapshot before opening, so the difference can never mistake the scratch for a session
to keep.

Steps carry **symbolic references** bound at apply time, so no step depends on what
"current" happens to be:

```
Plan { steps: Vec<Step> }
Step = CreateSession { name, first_window, cwd }            -> binds PaneRef
     | NewWindow     { session, index, name, cwd }          -> binds PaneRef, WindowRef
     | LinkWindow    { window: WindowRef, into: SessionName, index }
     | SplitPane     { in: PaneRef, cwd }                   -> binds PaneRef
     | SelectLayout  { window: WindowRef, layout }
     | ReplayContent { pane: PaneRef, blob }
     | Relaunch      { pane: PaneRef, command: Relaunch }
     | SetOption     { target, key, value }                 // @phoenix-window last in each window's group,
                                                            // @phoenix-restored per session, @phoenix-generation last of all
     | SelectWindow  { session, index } | Zoom { pane: PaneRef } | …
     | SwitchClient  { client: ClientName, to: SessionName } | KillSession { name }   // only from Opened::Created
     | Hook          { point: HookPoint }                   // pre/post, user-configured
```

`apply` runs steps in order over one connection; `-P -F '#{pane_id} #{window_id}'`
replies bind `PaneRef`s and `WindowRef`s; `--dry-run` renders the same steps with
symbolic names. The plan is the
single place that knows the order constraints tmux imposes (`move-window` after
`new-session`, layout after all panes exist), and it encodes them as sequence in
*data*, not as a contract between functions.

**Connecting is not restore's job.** `connect_and_apply` is gone. Restore is handed a
connection and the `Opened` that came with it, captures `live`, plans, and applies —
all over that one connection, with no reconnect. Moving clients off and removing the
session phoenix created to attach with, and marking the server, are the plan's last
steps, so `--dry-run` shows them and nothing outside the plan ever kills a session.

---

## 9. `phoenix-daemon` — a pure state machine fed by events

```
Daemon::step(&mut self, event: Event, now: Instant) -> Vec<Action>

Event  = Connected { live: Snapshot, newest: Option<GenerationId> }
       | NoServer  { newest: Option<GenerationId> }
       | StructureChanged | Tick | Disconnected
Action = Restore(GenerationId) | Save | Log(String) | WaitThen(Duration)
```

The loop captures `live` and reads the store's newest header once per connection
attempt and puts both into the event, so `step` never touches the store or the server
and sees the evidence itself — `live.touched` is the server's own mark, not a flag
someone computed from it — and `Restore(g)` is only ever emitted with a `g` the event
carried. The boot arms are the whole of §3's rule plus one refusal: `Connected { live:
{ touched: Never, .. }, newest: Some(g) }` and `NoServer { newest: Some(g) }` both
yield `Restore(g)`; `NoServer { newest: None }` yields `WaitThen(backoff)`, because a
store with nothing in it gives the daemon no reason to start a server, while a store
with something in it is exactly the daemon's reason to start one (§4's `Existing` is
for the idle loop, which has not yet decided); every other `Connected` yields nothing
and the daemon is simply attached. The rest — debounce, max-interval backstop, reconnect backoff — is a
pure function of state and event, and `DebounceState` is already most of it. The loop
around it is a dozen lines: open a connection with `Attach::Existing` (§4), subscribe
once with an attached-session-scope format that loops every session
(`#{S:#{W:#{window_layout}}}` — verified live to fire for a change in another
session), then `select` on the event channel and a timer, feed `step`, perform the
actions through `phoenix-ops`. `Restore` runs over the connection the loop already
holds when the event was `Connected`; when it was `NoServer`, the loop opens one with
`phoenix-ops::connect` (§10) and keeps it. There is never a second control client on
the server. No heartbeat, no poll interval,
no `Boot`, no `Decided`, no `Cycle::Refused`, no "end the run so the outer loop boots
again".

The daemon's control client attaches wherever it attaches; because phoenix never kills
a session it did not create and switches rather than reconnects, which session that is
no longer matters.

---

## 10. `phoenix-ops` — one implementation per operation

```
connect(socket, &snapshot)                     -> (Connection, Opened)
save(conn, store, retention)                   -> SaveReport    { generation, degradations }
restore(conn, opened, &snapshot, scope)        -> RestoreReport { applied, skipped, degradations }
status(store, conn?)                           -> Status        { last_save, touched, daemon_state }
```

The caller loads the generation once and hands the same `Snapshot` to `connect` and
`restore`, so the scratch name and the plan are chosen against one snapshot and cannot
drift apart. `connect` is the one place a scratch name is chosen — free of every
session name in that snapshot — and passed to `Connection::open(socket,
Attach::OrCreate { name })`. `save` is: read the previous generation's indicators from
the store, capture with them, write, then set the server mark. `restore` is §8's
sequence over the connection it is given. The CLI calls `connect` then `restore` and drops the connection; the daemon
calls `restore` on the connection it already holds, or `connect` first when there was
no server (§9). The CLI prints a report; the daemon logs one; neither composes the
lower crates itself. The hooks ticket (`tmux-parity-ure.9`) lands here as
`Step::Hook` edges in the plan plus the `SetOption` stamp of §3
(`@phoenix-restored=<generation>`) that is the one writer of "restore finished" a
tmux-side integration can wait on — the completion signal is a value tmux holds, not
a file or a sleep.

---

## 11. How the roadmap lands on this shape

Every open feature becomes data on an existing seam rather than a new mode:

| Roadmap item | Lands as |
| --- | --- |
| Zoom, alternate window/session, grouped sessions | fields in §5's graph; two format vars in capture; `Zoom`, `SelectWindow` and `LinkWindow` steps in the plan |
| Idempotent restore, diff/preview, selective restore | `plan(snapshot, live)` + a filter |
| One-shot save with content | the only save there is |
| vim/mosh/LLM resume strategies | `Foreground -> Relaunch`, one function, keyed by `argv[0]` |
| Named/tagged snapshots, generation picker | a header field; `list()` with it |
| Hooks and the restore-finished signal | `Step::Hook`, `Step::SetOption` |
| Status-line format string, TPM plugin | `status()` rendered by a thin script |
| Multi-server daemon | N state machines, one loop, keyed by socket |
| Cross-machine portability | an `import` that reads a self-describing generation file and writes it as the next id here, so "highest id is latest" stays true; `origin` says where it came from |
| Secrets exclusion | a per-pane/session tmux option read in capture, producing `Content::NotCaptured { reason: Excluded }` |

---

## 12. Order of work

The dependency among the changes, which is also the order that keeps the tree
shippable at every step:

1. **`tmux-control`:** open-on-any-server, owned reader + event sink, typed
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
   of intent, `PROJECT-GOALS.md` §3's gap table updated, and the README's promises
   about restore and boot restore reworded as §3 states them.

Each step is its own ticket under the `tmux-laws` epic, each with the live-tmux
verification it needs, and each leaves less code than it found
(`[LAW:polishing-by-subtraction]`): the expected net is roughly 2,600 lines removed —
`connect.rs` (796), `boot.rs` (204), half of `daemon.rs` (~225), and the 1,384 lines
of tests that pinned the heuristic — against a few hundred added in `tmux-control`
and `phoenix-ops`.
