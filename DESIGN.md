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
  carries the attempt count; the client re-enters `Connecting` and re-consumes the
  greeting on reconnect.
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
struct Pane    { index: PaneIndex, cwd: Utf8PathBuf,
                 program: CapturedProgram, content: Option<PaneContent> }
```

- `NonEmpty<T>` wherever a persisted snapshot needs ≥1, so restore never branches
  on the impossible-empty case (`[LAW:dataflow-not-control-flow]`). tmux guarantees
  it for windows and panes; for sessions, capture enforces it (§5).
- A single `active: WindowIndex` that must resolve to a member — not a per-child
  `bool` that could encode two-active-or-none (`[LAW:one-source-of-truth]`,
  validated at parse time). Enforced by keeping `active` private on `Session`/
  `Window` behind a validating `new()`, not just at construction time — public
  fields would let a caller swap in a bad index afterward.
- Every type here is phoenix-core's own (`Layout` included, not `tmux-control`'s), so
  the crate depends on nothing; capture's fold parses tmux's strings into them.

**Implementation note:** this environment has no network access to fetch
external crates (`cargo add` against crates.io stalls and times out), so
`phoenix-core` is std-only, same as `tmux-control`. `OffsetDateTime` and
`Utf8PathBuf` above are hand-rolled std-only stand-ins for `time`'s and
`camino`'s types of the same name, not the external crates themselves.
Revisit the stand-ins for the real crates if network access becomes available.

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

**Content capture implementation notes (tmux-content-dos.1, verified live):**
`#{history_size}`/`#{history_bytes}` are stable while a pane is idle and move on any
output, so one `list-panes -a -F` pull of `#{pane_id} #{history_size} #{history_bytes}`
is the free per-save dirty indicator. A pane whose indicator matches what the *previous*
capture recorded reuses that scrollback unchanged; every other pane (including one never
seen before) gets a fresh `capture-pane -p -e -S -`. The small visible screen
(`capture-pane -p -e`, no `-S`) is *always* re-pulled regardless, since an alt-screen TUI
can redraw without ever touching scrollback. `capture-pane` targets a bare `%N` pane id
directly — no `session:window.pane` needed. The correlator across saves is tmux's own
pane id (`phoenix_core::PaneId`, living on `PaneContent`, not `Pane` — window-relative
`PaneIndex` shifts if a sibling pane is added or removed, but a pane id doesn't).
`phoenix-capture` has no persistence dependency of its own, so it never loads "the
previous capture" itself — the caller (ultimately whatever loaded the last `Snapshot`)
hands in a `HashMap<pane_id, PreviousPaneContent>`. Program output isn't guaranteed valid
UTF-8 the way structural fields are, so captured lines are lossily decoded (U+FFFD for
bad sequences) rather than failing the pane.

---

## 6. Restore — plan, then apply

`plan(&Snapshot, &RestorePolicy) -> RestorePlan` is pure; `tmux-control` executes the
resulting ordered `Vec<TmuxCommand>`. Because the plan is data, `phoenix restore
--dry-run` prints exactly what would run before it runs — a real safety property for a
tool that can `send-keys` into live shells.

Program relaunch defaults to **cwd + shell only**; it never blind-replays captured
argv. Which programs may relaunch is a learned, consent-gated ruleset (a matcher +
verdict, resolved by a pure function into `Restore`/`Skip`/`Ask`): interactively an
`Ask` prompts the user; non-interactively (boot restore) an `Ask` falls to the safe
default and is logged, so a boot never blocks and never escalates. *(Full permission
model deferred to a later milestone; the default-safe behavior ships first.)*

**Implementation notes (found by running the plan against a real tmux server, not
assumed):** `new-session` has no flag to request a specific window index — the
window lands wherever the target server's `base-index` config puts it — so `plan`
always follows a session's `new-session` with a `move-window` relocating it to the
captured index (bare `-s <session>` as source resolves to the session's current,
and right after creation only, window). That move can legitimately fail with tmux's
"same index" error when the window already happened to land there; the executor
must treat exactly that as success. Panes have no such fix-up available at all —
`split-window` takes no index and there's no pane equivalent of `move-window` — so
the plan never targets a pane by index: instead each window's originally-active
pane is always the *last* one `split-window`'d (verified live that the most
recently split pane stays active through a following `select-layout`), which means
`select-pane` never actually appears in a plan despite being one of the tmux
primitives this milestone's ticket named.

**Content replay (tmux-content-dos.3):** "the authoritative grid comes from
capture-pane, not a re-emulated stream" — a pane's captured `scrollback` (not the raw
bytes that produced it, which were never captured) is replayed by sending
`cat <tempfile>` into the pane immediately after it's created, using the same
current-pane targeting `SplitWindow` relies on (a pane still has no addressable
index — see above). This is the one `TmuxCommand` variant that isn't a literal tmux
command line: it needs a temp file that only exists at apply time, so `plan` (pure)
can't render it verbatim the way every other variant can, and `apply` special-cases
it. No separate policy toggle — a pane with captured content always gets replayed;
one with `content: None` is simply left as a fresh idle shell, unchanged from the
"cwd + shell only" default.

---

## 7. Persistence

`phoenix-store` owns the save dir (`${XDG_DATA_HOME}/tmux-phoenix/`) and is its single
writer, enforced by an exclusive lock on the dir held from temp write through prune
(`[LAW:single-enforcer]`).
Serialize to a temp file, `fsync`, `rename(2)` onto the final name, then atomically
repoint `latest` — which therefore only ever names a fully-written snapshot; a crash
mid-save leaves the last good one untouched (`[LAW:one-source-of-truth]`). Keep the
last N generations. Body is MessagePack + zstd-compressed pane content by default
(fast, compact), with a `--format=json` option over the same `serde` schema for
inspection (`[LAW:one-type-per-behavior]`). A versioned header with a checksum; an
unknown `format_version` is refused loudly, never guessed (`[LAW:no-mode-explosion]`).

**Implementation note:** no `serde`/`rmp-serde`/`zstd` — same network constraint as
§4. The default body format is a hand-rolled, length-prefixed little-endian binary
encoding built directly on `phoenix-core`'s public constructors (so a corrupt file
can produce a decode error but never an invalid `Snapshot`), with an FNV-1a-64
checksum in place of a cryptographic one — this only needs to catch local
corruption, not defend against tampering. `--format=json` is a hand-rolled,
one-way (encode-only) JSON writer over the same tree; nothing reads JSON back in.
`captured_at`/`format_version` live in a fixed 32-byte header ahead of the body, so
`list` can read a quick per-generation summary without decoding the whole tree.
Revisit with real `serde`+`rmp-serde`+`zstd` if network access becomes available.

**Content-addressed blob store (tmux-content-dos.2):** a pane's `scrollback`/`visible`
text is stored as a blob under its own hash in `${store_dir}/blobs/`, not inline in the
generation file — the generation file just holds the 16-byte hash. Two saves whose pane
content is byte-identical (the common case for an unchanged pane, since
`phoenix-capture`'s dirty-tracking carries the same scrollback bytes forward verbatim)
write the same blob file twice, and the second write is a no-op: deduplication and
"unchanged panes become pointer copies" both fall out of content-addressing for free.
Blobs are written with the same temp+fsync+rename atomicity as generation files;
concurrent writers racing to store the *same* blob need no locking, since same hash
implies same content and whichever writer's rename lands last still leaves the correct
bytes at that path. No crypto-hash crate is available (same network constraint as
above), and a checksum-grade hash isn't enough here — two different blobs colliding
would silently return the wrong pane content for one of them — so the hash is a
128-bit combination of two independent, differently-salted FNV-1a-64 passes: adequate
collision resistance for a single-user, non-adversarial content store, not a
cryptographic guarantee. Blob garbage collection (pruning blobs no surviving generation
still references) is deferred to a later ticket.

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
- **Boot restore.** On start, if the server has only the default empty session, apply
  `latest`; if sessions already exist, log and stay in save mode — never clobber a live
  server.
- **Supervision.** `launchd` user agent (macOS) / `systemd --user` unit (Linux);
  `phoenix daemon` runs it foreground for debugging. Independent of the tmux server —
  if tmux isn't running the connection sits in `Closed`/`Reconnecting` and the daemon
  idles.

**Implementation notes (tmux-daemon-b0h.1):** the structure subscription is one
`refresh-client -B <name>:@*:#{window_layout}` (all windows, tracking each one's
layout string) — verified live that it fires on pane split/close/resize and window
add/remove. The debounce/max-interval *decision* (`phoenix_daemon::DebounceState`)
is pure, driven by explicit timestamps the caller supplies, not the wall clock
directly — testable with synthetic time, no real sleeping in the unit tests. The
run loop itself has no separate "wait with a timeout" primitive: `tmux-control`'s
`Client::execute` already reads and dispatches a full batch of arrived notifications
before returning (the same batch-draining behavior `tmux-control-mode-1ju` built), so
a cheap heartbeat command (`display-message -p ""`) issued once per short poll
interval both drains pending `%subscription-changed` events and gives the loop a
natural, bounded wake-up cadence — no raw `poll(2)`/threading needed. A single
capture-or-save failure is logged and the loop continues; only a failure setting up
`no-output`/the subscription at startup is fatal. This closes the "content capture
isn't wired into `save`" gap `tmux-content-dos.2` left open: the daemon holds the
previous capture's per-pane content in memory across its own save cycles (seeded
from `latest` on startup), which `phoenix save` (one-shot, no natural "previous" to
hold) doesn't have a clean way to do.

**Note (limitation found while building boot restore, `tmux-daemon-b0h.2`):** the
structure subscription above is scoped to the client's *attached* session only —
verified live that a change in a second, non-attached session never fires it. A
multi-session server therefore relies more on the max-interval backstop than the
debounce path for sessions other than the one the daemon happens to be attached to.
Not fixed here (out of scope for daemon-core); worth revisiting if that gap matters
in practice.

**Boot restore implementation notes (tmux-daemon-b0h.2, verified live — each one
surprising enough that the one-sentence spec text above doesn't capture it):** bare
`tmux -C` (no session target) does *not* attach to an existing session when one is
already there — it unconditionally creates a brand new one every time, so "does the
server already have sessions" has to be checked with a plain, non-control-mode
`list-sessions` *before* opening any control-mode connection at all. Killing the
session a control-mode client is attached to ends that client's connection
(`%exit`) — there's no way to survive your own session's death — so the throwaway
bootstrap session boot restore creates (only when the server had zero sessions) is
torn down by reconnecting onto a real restored session first (closing the old
transport just *detaches*; the bootstrap session survives that, unattended), then
killing the now-unattended bootstrap session from a fresh connection.

**Resilience and install implementation notes (tmux-daemon-b0h.3):** `phoenix
daemon` never exits just because tmux went away or was never up in the first
place — `phoenix_daemon::run_resilient` wraps the one-connection `run` loop above
in an outer reconnect loop (`connect_and_boot` again, so a reconnect after tmux
comes back is itself a boot restore), sleeping `reconnect_interval` between
attempts. A connection is distinguished from a mere command failure by
`is_connection_dead` (`TmuxError::{Send,Read,TransportClosed,NotReady}`), so a
one-off command error doesn't get treated as the server dying. `phoenix install`
(`phoenix-cli/src/install.rs`) writes the launchd plist / systemd unit but
deliberately never runs `launchctl load`/`systemctl --user enable` itself — it
prints the exact command and leaves activating a persistent, reboot-surviving
background process to the user. The generated unit's `ExecStart`/`ProgramArguments`
embeds `std::env::current_exe()`'s absolute path rather than relying on `phoenix`
being on `$PATH` inside the constrained launchd/systemd environment. Plan
construction (which file, what content) is kept pure and unit-tested; only the
final `fs::write` is effectful, verified live on this (macOS) machine with
`plutil -lint` against the actual generated plist.

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
5. **Learned permission model for program relaunch.**
