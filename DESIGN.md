# tmux-phoenix — design

A single-binary Rust replacement for `tmux-resurrect` + `tmux-continuum`: capture the
full state of a running tmux server, persist it durably, and reconstruct it later.

The three priorities, in order, are **stability**, **speed**, and the **efficiency**
that underwrites both. Every decision below is justified against one of those three, or
it doesn't belong here.

Protocol authority for everything in §3 is the C-source-cited spec in
[`promptctl/tmux-control-mode-js/SPEC.md`](https://github.com/promptctl/tmux-control-mode-js/blob/master/SPEC.md) (tmux next-3.7, commit 5c30b145) and the tmux
man page — never a captured transcript. That TypeScript library is also the reference
architecture we port from; `IMPL §` citations point to its
[`IMPL.md`](https://github.com/promptctl/tmux-control-mode-js/blob/master/IMPL.md).

---

## 1. What the three priorities demand

| Priority | What it rules out | What phoenix does instead |
| --- | --- | --- |
| **Stable** | torn saves, parser panics on odd input, correctness that depends on timing, silent fallbacks | atomic `rename(2)` saves; a total codec with an `Unknown` arm that never panics; a typed connection state machine; loud failure everywhere |
| **Fast** | `fork/exec`-per-pane (resurrect spawns dozens), polling, streaming pane bytes we don't need | one persistent control-mode connection; event-driven change detection via subscriptions; `no-output` so tmux never sends us pane bytes |
| **Efficient** | re-interrogating the whole server on a timer, re-decoding, second copies of state | subscribe to *structure* changes only; `capture-pane` on demand; one source of truth per fact |

The efficiency line is the interesting one: resurrect and continuum are slow and
fragile for the *same* root reason — they shell out. continuum doesn't even have a
timer; it hijacks `status-right` redraws to decide when to save. A persistent
control-mode connection removes the per-operation process spawn entirely, which is
simultaneously the speed win and (because there are far fewer moving parts per
operation) a stability win.

---

## 2. Crate layout and dependency direction

Dependencies flow strictly downhill: a crate depends only on crates in rows below it
(`[LAW:one-way-deps]`). Two foundation crates at the bottom depend on nothing:

```
phoenix-cli
phoenix-daemon
phoenix-capture   phoenix-restore   phoenix-store
tmux-control      phoenix-core
```

- **`tmux-control`** — a complete, standalone Rust implementation of the tmux
  control-mode protocol. Depends on nothing phoenix-specific; it's a reusable crate
  in its own right (the expanded scope, §3). This is the substrate.
- **`phoenix-core`** — pure domain types (the `Snapshot` tree). No I/O.
- **`phoenix-capture`** — drives `tmux-control` to interrogate the server into a
  `Snapshot`.
- **`phoenix-store`** — atomic, versioned, generational persistence.
- **`phoenix-restore`** — pure `Snapshot -> RestorePlan`; the plan is executed by
  `tmux-control`.
- **`phoenix-daemon`** — keeps one tmux server's state alive across restarts.
- **`phoenix-cli`** — maps command lines onto the crates below.

Each crate's purpose is one conjunction-free sentence (`[LAW:decomposition]`). If
`phoenix-store` ever imports `tmux-control`, or `tmux-control` ever learns what a
`Snapshot` is, a joint has been mis-cut.

---

## 3. `tmux-control` — the complete protocol crate

Three layers, mirroring the reference library, each a clean seam
(`[LAW:effects-at-boundaries]`): a **pure codec**, an **effect transport**, and a
**correlation client**. The codec depends on neither of the others, so it is testable
against the spec's documented examples and recorded transcripts with zero I/O.

### 3.1 `protocol` — the pure codec (no I/O, no panics)

The wire is line-oriented; every server line starts with `%` (SPEC §1). The parser is
a state machine with exactly two states, because the protocol has exactly two
(SPEC §5–6):

- **Outside a guard block:** a `%begin` opens a block; any other `%`-line is a
  notification; anything else is unexpected and surfaced as `Unknown`, never dropped.
- **Inside a guard block:** every line is command output until `%end` (success) or
  `%error` (failure) closes it. **Notifications never appear inside a block** — the
  one invariant the whole design leans on (SPEC §6), so the state machine needs no
  lookahead.

```rust
fn feed(&mut self, bytes: &[u8]) -> Vec<ServerMessage>;   // pure, total, panic-free
```

`ServerMessage` is the **complete** notification set from SPEC §23 as a discriminated
union — every legal message representable, and an explicit catch-all so an unknown or
malformed line degrades to data instead of a crash (`[LAW:types-are-the-program]`,
`[LAW:no-silent-failure]`):

```rust
enum ServerMessage {
    GuardBegin(Guard), GuardEnd(Guard), GuardError(Guard),   // command-reply framing
    Output       { pane: PaneId, data: Vec<u8> },            // octal-decoded bytes
    ExtendedOutput { pane: PaneId, age_ms: u64, data: Vec<u8> },
    Pause { pane: PaneId }, Continue { pane: PaneId },
    PaneModeChanged { pane: PaneId },
    WindowAdd { window: WindowId }, WindowClose { window: WindowId },
    WindowRenamed { window: WindowId, name: String },
    WindowPaneChanged { window: WindowId, pane: PaneId },
    UnlinkedWindowAdd { window: WindowId }, /* …close, …renamed */
    LayoutChange { window: WindowId, layout: Layout, visible: Layout, flags: String },
    SessionChanged { session: SessionId, name: String },
    SessionRenamed { session: SessionId, name: String },     // §25: code sends id+name, man page lies
    SessionsChanged,
    SessionWindowChanged { session: SessionId, window: WindowId },
    ClientSessionChanged { client: String, session: SessionId, name: String },
    ClientDetached { client: String },
    PasteBufferChanged { name: String }, PasteBufferDeleted { name: String },
    SubscriptionChanged { name: String, session: Option<SessionId>, window: Option<WindowId>,
                          window_index: Option<u32>, pane: Option<PaneId>, value: String },
    Message { text: String },
    ConfigError { text: String },
    Exit { reason: Option<String> },
    Unknown(String),                                          // never panic, never drop
}
```

The implementation adds two more variants beyond this SPEC §23 catalogue, both
required for the guard-block state machine itself to stay total: `CommandOutput
{ command_number: u32, line: Vec<u8> }` for a line of command-response text
between `%begin` and `%end`/`%error` (SPEC §23 only catalogues `%`-prefixed
protocol lines — a command's actual output, e.g. a `list-panes` row, has no
wire type of its own and needs a home too), and `ProtocolError { command_number:
u32, line: Vec<u8> }` for a `%end`/`%error` that arrives positionally as the
open block's terminator but fails to parse — force-closing the block instead
of leaving it open forever, which would otherwise silently misroute every
subsequent line, notifications included, as output for a command that will
never settle.

Supporting pure pieces:

- **Typed IDs.** `SessionId`, `WindowId`, `PaneId` are newtypes that parse the
  `$`/`@`/`%` prefix once at the boundary (SPEC §3), so no code downstream ever
  re-parses a raw id string (`[LAW:parse-dont-validate]`).
- **`decode_octal`.** `%output`/`%extended-output` payloads are octal-escaped
  (`\NNN`, `\` → `\134`, SPEC §10). One decoder, tolerant of malformed/partial
  escapes: anything that isn't a valid `\000`–`\377` escape decodes to `?`.
- **`Layout`** stays an opaque newtype around tmux's own layout string. tmux owns
  window geometry; we transport it verbatim and never re-derive it
  (`[LAW:one-source-of-truth]`).
- **`CommandLine`.** The client-to-server half of the wire. tmux parses every line a
  control client sends with its config-file lexer, so a command is built from its
  argv by a port of tmux's own `args_escape`, and tmux reads each argument back as
  exactly the original string. A newline inside an argument is written as the `\n`
  escape, so a `CommandLine` is always one wire line and newlines in pane paths,
  option values, and relaunch commands survive. NUL is the one argument no encoding
  can carry (tmux arguments are C strings), so it fails construction.

### 3.2 `transport` — the effect edge

Spawns `tmux -C` (single `-C`; `-CC` needs a tty and adds only DCS framing we don't
want — IMPL §2.1), owns the child and its stdin/stdout pipes, reads bytes into the
codec, writes `CommandLine`s out. `send` takes the encoded type, not a string, so
the transport never frames or checks a command. Defined behind a `Transport` trait so the codec and
client are testable without a live tmux, and so an alternate transport (PTY, or a
remote socket) can be swapped in without touching the layers above
(`[LAW:locality-or-seam]`). This is the *only* place a process is spawned or a byte is
read from the OS.

### 3.3 `client` — correlation, dispatch, lifecycle

- **`execute(cmd) -> Result<CommandOutput, TmuxError>` is the single command-dispatch
  path** (`[LAW:single-enforcer]`). Every typed operation — `list_panes`,
  `capture_pane`, `subscribe`, `send_keys`, `split_window` — is a free function that
  encodes a command string and delegates to `execute`. Correlation is by FIFO: tmux
  processes commands in order and emits exactly one guard block each, so the head of
  the pending-command queue owns the next `%begin…%end/%error`. The command-number in
  the guard is informational, not the correlation key.
- **Connection state is owned, typed, and explicit** — no timing folklore
  (`[LAW:no-ambient-temporal-coupling]`):

  ```rust
  enum ConnectionState {
      Connecting,                    // consuming tmux's unsolicited startup greeting block
      Ready,                         // safe to correlate caller commands
      Reconnecting { attempt: u32 },
      Closed { reason: CloseReason },
  }
  ```

  The subtlety the reference calls out and we inherit: on attach tmux emits an
  **unsolicited** `%begin…%end` greeting that is *not* a reply to any command. It must
  be consumed during `Connecting` before any caller command may be correlated —
  correlating against it is a classic off-by-one that corrupts every subsequent reply.
- **Two teardowns, deliberately not merged** (IMPL §2.4): `detach()` writes a bare
  `\n` (tmux reads the empty line as client-exit, SPEC §4.1) and is the one wire write
  that is correctly *not* a command, because it carries no guard block to correlate;
  `close()` sends nothing: it drops the transport and reaps the child locally.
- **Notifications** dispatch as typed events to subscribers. **Pane output does not**
  — it routes through a separate byte/line sink path, because `%output` is
  high-volume and mixing it into the event stream is how you get head-of-line
  blocking. For phoenix this path is usually dark anyway (see §3.4).
- **Version gating**: a typed `UnsupportedTmuxVersion` error with a per-command floor,
  never a silently swallowed `%error` (IMPL §2.2). Protocol floor tmux 3.2.

### 3.4 How phoenix uses the crate (and why it's cheap)

phoenix sets the **`no-output`** client flag (SPEC §9) so tmux never sends pane bytes,
and registers **subscriptions** (SPEC §14) for the handful of structure formats that
mean "state changed" — session/window set, active window/pane, layout. The daemon then
holds one idle connection that costs almost nothing until tmux pushes a
`%subscription-changed`. Capturing a snapshot is a few `execute` round-trips
(`list-panes -a`, per-pane `capture-pane` only when content capture is on) over the
already-open connection — no tmux process spawns (argv recovery adds one `ps` pass, §5). This is the whole efficiency thesis in one
paragraph: **subscribe to structure, stream nothing, capture on demand.**

### 3.5 Stability properties this layer guarantees

- The codec is **total and panic-free**: every byte sequence maps to messages, with
  `Unknown` absorbing anything unrecognized. A malformed line can never crash the
  daemon.
- Reconnection is a **typed state**, not a retry loop bolted on: `Reconnecting`
  carries the attempt count and is itself the greeting-consuming phase of a
  reconnect, so the client re-consumes the greeting from `Reconnecting` and settles
  straight into `Ready`. It does not pass back through `Connecting`, which would
  overwrite the attempt count with a phase no caller can observe — `reconnect()` is
  synchronous, and `execute()` gates on `Ready` alone, so `Connecting` and
  `Reconnecting` refuse correlation alike.
- Backpressure can't wedge us: with `no-output` set we never enter the pause/`too far
  behind` machinery (SPEC §16) at all.

---

## 4. `phoenix-core` — the domain model

tmux state is a strict tree; encode it so a malformed snapshot can't be built
(`[LAW:types-are-the-program]`).

```rust
struct Snapshot { format_version: FormatVersion, tmux_version: TmuxVersion,
                  captured_at: OffsetDateTime, sessions: NonEmpty<Session> }
struct Session { name: SessionName, windows: NonEmpty<Window>, active: WindowIndex }
struct Window  { index: WindowIndex, name: WindowName, layout: Layout,
                 panes: NonEmpty<Pane>, active: PaneIndex }
struct Pane    { id: PaneId, index: PaneIndex, cwd: Option<Utf8PathBuf>,
                 program: CapturedProgram, content: Option<PaneContent> }
struct CapturedProgram { command: ProgramName, argv: Option<NonEmpty<String>> }
```

- `NonEmpty<T>` wherever a persisted snapshot needs ≥1, so restore never branches
  on the impossible-empty case (`[LAW:dataflow-not-control-flow]`). tmux guarantees
  it for windows and panes; for sessions, capture enforces it (§5).
- A single `active: WindowIndex` that must resolve to a member — not a per-child
  `bool` that could encode two-active-or-none (`[LAW:one-source-of-truth]`,
  validated at parse time). `active` is private on `Session`/`Window` behind a
  validating `new()`, which also refuses duplicate indices, so no caller can swap in
  an index that doesn't resolve.
- Absence is a type, not a sentinel. tmux reports a working directory it can't read
  as the empty string, which `Utf8PathBuf::parse` refuses, so the pane records `None`;
  a pane whose foreground argv `ps` couldn't recover has `argv: None`. Either one
  marks the save degraded (§9).
- `id` is tmux's own pane id, stable across saves while the pane lives. It correlates
  a pane with its previous capture for content dirty-tracking (§5); `index` is
  window-relative and shifts when a sibling pane opens or closes.
- Every type here is phoenix-core's own (`Layout` included, not `tmux-control`'s), so
  the crate depends on nothing; capture's fold parses tmux's strings into them.

**Implementation note:** the workspace uses no external crates, so `phoenix-core` is
std-only, like `tmux-control`. `OffsetDateTime` and `Utf8PathBuf` above are std-only
stand-ins for the `time` and `camino` types of the same name, written because the
environment the crates were built in could not reach crates.io.

---

## 5. Capture

`phoenix-capture` runs `list-panes -a` (one round-trip, all sessions/windows/panes
with layout, cwd, `pane_current_command`, `pane_pid`), plus one `ps` pass to recover
each pane's foreground argv (the format vars give only the command *name*), plus one
`capture-pane` per pane when content capture is on. A **pure** fold turns those text
blobs into a `Snapshot` — testable against fixtures with no tmux (`[LAW:effects-at-boundaries]`).

Structure capture is all-or-nothing (a torn tree is never persisted). A server with
zero sessions (possible under `exit-empty off`, or for an instant as the last session
closes) has no tree: capture returns a typed failure, the daemon logs it like any
failed save cycle, and nothing is saved, so an empty snapshot never becomes latest
(`[LAW:parse-dont-validate]`). Content capture
is best-effort per pane: an unresponsive pane degrades *that* pane's content to `None`
with a recorded warning and marks the save `degraded` — never nukes the snapshot, never
pretends (`[LAW:no-silent-failure]`).

**Content capture (verified live):** `#{history_size}`/`#{history_bytes}` are stable
while a pane is idle and move on any output, so one `list-panes -a -F` pull of
`#{pane_id} #{history_size} #{history_bytes}` is the free per-save dirty indicator. A
pane whose indicator matches what the previous capture recorded reuses that scrollback
unchanged; every other pane, including one never seen before, gets a fresh
`capture-pane -p -e -S -`. The small visible screen (`capture-pane -p -e`, no `-S`) is
always re-pulled, since an alt-screen TUI can redraw without touching scrollback.
`capture-pane` targets a bare `%N` pane id directly. `phoenix-capture` has no
persistence dependency, so it never loads the previous capture itself: the caller hands
in a `HashMap<pane_id, PreviousPaneContent>`. Only the daemon does that today (§8);
one-shot `phoenix save` captures structure only. Program output isn't guaranteed valid
UTF-8 the way structural fields are, so captured lines are decoded lossily (U+FFFD for
bad sequences) rather than failing the pane.

---

## 6. Restore — plan, then apply

`plan(&Snapshot) -> RestorePlan` is pure; `phoenix-restore`'s `apply` executes the
resulting ordered steps over a `tmux-control` client. Because the plan is data, `phoenix
restore --dry-run` prints what would run before it runs — a real safety property for a
tool that can `send-keys` into live shells. Every step prints as its exact tmux command
line except scrollback replay, which prints as a `#` summary: its command names a temp
file that only exists at apply time.

A restored pane comes back **at its captured cwd, running the program it was
running** — the captured argv is replayed into the pane, unconditionally, on every
restore path (interactive `phoenix restore` and the daemon's unattended boot restore
alike). Nothing is asked and nothing is configured: getting your programs back is the
whole point of restoring. A pane with no captured cwd sends no `-c`, so it opens in
tmux's default directory for the new session or window.

**Implementation notes (found by running the plan against a real tmux server):**
`new-session` has no flag to request a specific window index — the window lands wherever
the target server's `base-index` puts it — so `plan` always follows a session's
`new-session` with a `move-window` relocating it to the captured index. That move can
legitimately fail with tmux's "same index" error when the window already landed there;
the executor treats exactly that as success. Panes have no such fix-up at all —
`split-window` takes no index and there's no pane equivalent of `move-window` — so the
plan never targets a pane by index: each window's originally-active pane is always the
last one split (verified live that the most recently split pane stays active through a
following `select-layout`), so `select-pane` never appears in a plan.

**Content replay:** "the authoritative grid comes from capture-pane, not a re-emulated
stream" — a pane's captured `scrollback` is replayed by typing `cat <tempfile>; rm -f
<tempfile>` into the pane immediately after it's created, using the same current-pane
targeting `split-window` relies on. The temp file is created exclusively with mode 0600,
so another local user can neither read the scrollback nor plant the path first, and the
pane's own shell removes it once read, since only that shell knows when it has. This is
the one plan step that isn't a `TmuxCommand` (`PlanStep::ReplayContent`), because its
command line needs that temp file. A pane with `content: None` has nothing to replay.

**Program relaunch:** a pane's captured `argv` (§5's best-effort `ps` recovery) becomes a
`TmuxCommand::RelaunchProgram`, emitted right after that pane's content replay so
captured history is visible before the program that produced it restarts on top (the
order tmux-resurrect uses). The argv is shell-quoted element by element, so an argument
containing spaces round-trips as one shell word. Two pane shapes come back as a plain
shell at their cwd: one with `argv: None` (there is no command line to run), and one
that was idle at its prompt, which tmux reports as the pane's own shell being its
foreground process. The second is recognized by `pane_current_command` being an
interactive shell (`zsh`, `bash`, `fish`, …) invoked with no non-flag argument —
restore already creates every pane as a fresh shell, so re-running it would nest a
second shell, whereas `bash deploy.sh` is a real program and does come back.

**Connecting (`phoenix_restore::connect_and_apply`):** a control-mode client attaches to
a session, and restore creates sessions whose names the server may already use. Every
restore path goes through one function that first probes what the server holds. It
counts sessions with a plain `list-sessions` (bare `tmux -C` always creates a session, so
it can't be the check); only tmux's own no-server replies count as zero, and any other
failure stops the restore rather than guessing. A populated server is captured over a
short-lived connection and read as *bootstrap-only* when every session is one window
holding one pane idle at its shell — what a terminal starting `tmux` creates — and as
*built* otherwise. Idleness is `phoenix_core::Foreground`, the same reading restore's
program relaunch uses, and a pane whose argv wasn't recovered is `Unknown`, never idle.

The steps are the same for every server; only the scaffolding differs. An empty server
gets a created `phoenix-boot-N` session, a bootstrap-only server has every session renamed
to a free `phoenix-boot-N`, and a built server gets none. Each name is free of the
server's and the snapshot's names, so no snapshot collides with its own scaffolding. The
plan applies over the scaffolding, the client reattaches to a restored session (killing
the session a client is attached to ends that client's connection), every terminal still
on scaffolding is switched onto that session, and the scaffolding is killed. If the plan
doesn't apply, the scaffolding is put back instead: a created session is killed and a
renamed one gets its name back, so a failed restore never kills a login terminal's
session out from under it.

---

## 7. Persistence

`phoenix-store` owns the save dir (`${XDG_DATA_HOME}/tmux-phoenix/`) and is its single
writer (`[LAW:single-enforcer]`): `Store::save` takes an exclusive `flock` on
`${store_dir}/.lock` before choosing a generation id and holds it through prune, so a
manual `phoenix save` and the daemon's save run one at a time. How long a save waits
for the lock is a value its caller passes, not a mode: `phoenix save` waits up to 10 s,
then fails naming the contention; the daemon does not wait, so a contended cycle fails
without counting as a save and its next poll tries again. A generation's id is its
capture's Unix timestamp, raised above the newest existing id when it isn't already, so
ids rise in save order: `latest` always names the highest id. Retention is a count of
at least one from the `--keep` flag down (`--keep 0` is refused where it is parsed), so
pruning always keeps the generation the save just wrote. A snapshot in which every session
is a bootstrap session (§6) is refused with `StoreError::BootstrapOnly` before anything is
written: saving a server a login terminal just started would make `latest` name it in
place of the real state.
Serialize to a temp file, `fsync`, `rename(2)` onto the final name, then atomically
repoint `latest` — which therefore only ever names a fully-written snapshot; a crash
mid-save leaves the last good one untouched (`[LAW:one-source-of-truth]`). Keep the
last N generations. Body is MessagePack + zstd-compressed pane content by default
(fast, compact), with a `--format=json` option over the same `serde` schema for
inspection (`[LAW:one-type-per-behavior]`). A versioned header with a checksum; an
unknown `format_version` is refused loudly, never guessed (`[LAW:no-mode-explosion]`).

**Implementation note:** no `serde`/`rmp-serde`/`zstd`, for the same reason as §4's
note. The body is a hand-rolled, length-prefixed little-endian binary encoding built
directly on `phoenix-core`'s public constructors, so a corrupt file can produce a decode
error but never an invalid `Snapshot`, with an FNV-1a-64 checksum in place of a
cryptographic one — it only needs to catch local corruption, not tampering.
`--format=json` is a hand-rolled, encode-only JSON writer over the same tree.
`captured_at`/`format_version` live in a fixed 32-byte header ahead of the body, so
`list` reads a per-generation summary without decoding the tree.

**Content-addressed blob store:** a pane's `scrollback`/`visible` text is stored as a
blob under its own hash in `${store_dir}/blobs/`, and the generation file holds only the
hash. Two saves with byte-identical pane content (the common case for an unchanged pane,
since dirty-tracking carries the same scrollback forward) write the same blob, and the
second write is a no-op. Blobs use the same temp+fsync+rename atomicity as generation
files; writers racing on the same blob need no lock, since same hash means same bytes.
With no crypto-hash crate available, and a checksum-grade hash not enough (two blobs
colliding would return the wrong pane's content), the hash is 128 bits from two
independent, differently-salted FNV-1a-64 passes: adequate for a single-user,
non-adversarial store, not a cryptographic guarantee. Pruning blobs no surviving
generation references is not implemented yet.

---

## 8. The daemon

`phoenix-daemon` holds one `tmux-control` connection with `no-output` + structure
subscriptions, and owns *when to save* as explicit state
(`[LAW:no-ambient-temporal-coupling]`) — the thing continuum never had:

- **Event-driven save (primary).** On each `%subscription-changed`, reset a debounce
  timer; save once activity has been quiet for `debounce` seconds. Fresh right after
  real change, idle otherwise.
- **Interval ceiling (backstop).** A max-interval save so long steady sessions still
  checkpoint.
- **Boot restore.** On start, if the server holds nothing the user built — no sessions, or
  only bootstrap sessions (§6) — apply `latest` in their place; if it holds a session the
  user built, log which one and stay in save mode — never clobber a live server.
- **Supervision.** `launchd` user agent (macOS) / `systemd --user` unit (Linux);
  `phoenix daemon` runs it foreground for debugging. Independent of the tmux server —
  if tmux isn't running the connection sits in `Closed`/`Reconnecting` and the daemon
  idles.

**Implementation notes:** the structure subscription is one `refresh-client -B
phoenix-structure:@*:#{window_layout}` — verified live to fire on pane split, close and
resize and on window add and remove. It observes only the attached session's windows, so
a change in another session reaches the daemon through the max-interval backstop, not
the debounce path. A client's notification sink is fixed when the client is built, so
the daemon creates its activity flag (`StructureActivity`) first and builds the client
around its sink, including the client boot restore hands back. The debounce decision
(`DebounceState`) is pure, driven by timestamps the caller supplies, so its tests need
no sleeping. The loop has no wait-with-timeout primitive: once per short poll interval a
cheap heartbeat (`display-message -p ""`) makes `execute` read and dispatch whatever
notifications arrived, then the loop reads the flag and decides. One capture-or-save
failure is logged and the loop continues, except the store declining a bootstrap-only
capture, which ends the run so the daemon boots again; only failing to set `no-output` or
subscribe is fatal to a connection. The daemon carries each save's per-pane content forward
(seeded from `latest` on start), which is why it is the one path with content capture
on.

Boot restore probes the server first (§6). With a session the user built, it names that
session in its log, attaches, and never restores. Holding nothing the user built and with
a saved `latest`, it restores through `connect_and_apply`. With no sessions and nothing
saved, it attaches to nothing and waits: starting a server nobody asked for is not the
daemon's call. With only bootstrap sessions and nothing saved, it attaches and stays in
save mode.

The probe reads idleness from the process table at one instant, and a login shell's
prompt briefly runs programs of its own: measured live, `git` held the foreground about
160 ms into startup, in its own process group, indistinguishable from a program the user
started. So a boot probe can take a bootstrap session for a built one. The daemon doesn't
leave that to timing: the store refuses every bootstrap-only capture, and `run` returns on
that refusal, so `run_resilient` boots again and restores `latest` at the next save
cycle. The same instant-reading applies to a save: a capture that lands while a lone idle
shell redraws its prompt reads as built and is saved, but the generations before it are
kept and restorable with `restore --file`. `run_resilient` wraps all of this in a reconnect loop, so a
reconnect after tmux comes back is itself a boot restore; `TmuxError::{Send, Read,
TransportClosed, NotReady}` mark the connection dead, and every other error is one
command failing. `phoenix install` writes the launchd plist or systemd unit with the
binary's absolute path (from `current_exe`, since `$PATH` is minimal under both
supervisors) and prints the activation command, but never runs it.

---

## 9. CLI, failure philosophy, milestones

**CLI** (exit codes are a contract; stdout parseable, stderr human):
`save` (0 ok / 3 degraded / 1 fail), `restore [--dry-run|--file]`, `list`, `daemon`,
`status`, `install`.

**Failure philosophy.** Every external call — each `execute`, each `ps`, each file op —
is checked (exit zero? output non-empty? parses? sane?) and aborts the current
operation with a located message, leaving the last good state intact
(`[LAW:no-silent-failure]`). A failed save never touches `latest`. No `2>/dev/null`
anywhere.

**Milestones**, each with a machine-checkable done-shape phoenix runs itself
(`[LAW:verifiable-goals]`):

0. **`tmux-control` crate.** Codec + transport + `execute` + connection state machine +
   subscriptions. Done-shape: parse the spec's documented example blocks and recorded
   transcripts into the exact `ServerMessage` values; drive a live server through
   `execute` and assert replies.
1. **Capture → serialize → inspect** (`phoenix-core` + `phoenix-capture` +
   `phoenix-store` + `save`/`list`). Done-shape: round-trip this machine's live
   sessions; diff the parsed tree against the raw `list-panes` dump.
2. **Restore (shell + cwd)** (`phoenix-restore` + `--dry-run`). Done-shape: kill a test
   session, rebuild it, assert `list-panes` geometry matches the snapshot.
3. **Content capture + replay.**
4. **Daemon: subscription-driven debounced save + boot restore + service install.**
