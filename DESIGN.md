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
    SubscriptionChanged { name: String, session: SessionId, window: Option<WindowId>,
                          window_index: Option<u32>, pane: Option<PaneId>, value: String },
    Message { text: String },
    ConfigError { text: String },
    Exit { reason: Option<String> },
    Unknown(String),                                          // never panic, never drop
}
```

Supporting pure pieces:

- **Typed IDs.** `SessionId`, `WindowId`, `PaneId` are newtypes that parse the
  `$`/`@`/`%` prefix once at the boundary (SPEC §3), so no code downstream ever
  re-parses a raw id string (`[LAW:parse-dont-validate]`).
- **`decode_octal`.** `%output`/`%extended-output` payloads are octal-escaped
  (`\NNN`, `\` → `\134`, SPEC §10). One decoder, tolerant of malformed/partial
  escapes (pass a stray `\` through rather than panicking) — the reference documents
  exactly this tolerance.
- **`Layout`** stays an opaque newtype around tmux's own layout string. tmux owns
  window geometry; we transport it verbatim and never re-derive it
  (`[LAW:one-source-of-truth]`).

### 3.2 `transport` — the effect edge

Spawns `tmux -C` (single `-C`; `-CC` needs a tty and adds only DCS framing we don't
want — IMPL §2.1), owns the child and its stdin/stdout pipes, reads bytes into the
codec, writes command lines out. Defined behind a `Transport` trait so the codec and
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
  validated at parse time).
- Every type here is phoenix-core's own (`Layout` included, not `tmux-control`'s), so
  the crate depends on nothing; capture's fold parses tmux's strings into them.

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
