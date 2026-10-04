# Laws audit — 2026-10-04

**What this document is.** A read-only pass over every crate at commit `af4f727`,
against the universal architectural laws, looking for the *root causes* that generate
violations rather than the violations themselves. Small things are listed only where
they are symptoms of a big one. The architecture that resolves these findings is
`ARCHITECTURE.md`, next to this file; the backlog epic that schedules the work is
`tmux-laws-*` in lit.

The verdict up front: the foundations are good. `tmux-control`'s codec, the
`Snapshot` constructors, the pure `plan`, the pure `DebounceState`, and the atomic
store are all laws-shaped. The rot is concentrated in one place and radiates from it:
**phoenix tries to answer a question it cannot represent, "did the user build this
session?", by reading the process table at one instant, and then enforces that guess
at three checkpoints.** Nearly every mode, every "when/and/only/except" paragraph, and
every reconnect dance in the daemon and restore crates descends from that one
under-constrained fact. Fix the representation and most of the code below it is
residue that deletes itself.

---

## Finding 1 — the bootstrap heuristic: an unrepresentable fact, inferred, then enforced three times

**Where.** `phoenix_core::Session::is_bootstrap`, `phoenix_core::Foreground`,
`phoenix_store::Store::save` (`StoreError::BootstrapOnly`),
`phoenix_restore::connect::{ServerState, Scaffold, probe, retire}`,
`phoenix_daemon::boot::{Boot, Decided, misread}`, `phoenix_daemon::daemon::Cycle`,
`run_resilient`'s "forget the decision and boot again" arm, and DESIGN.md §6/§8.

**What breaks.** "A session the user built" is not a fact tmux holds. Phoenix
derives it from *one window, one pane, foreground is an interactive shell with only
flag arguments*, where "foreground" is read from `ps` at a single instant. DESIGN.md
§8 documents the consequence honestly: a login shell's prompt runs `git` for ~160 ms
in its own process group, so the probe "can take a bootstrap session for a built
one." The code then compensates with a provisional decision (`Boot::Declined`), a
later re-check (`Boot::misread`), a store refusal that is "an answer about the server,
not a failed save" (`Cycle::Refused`), a run that ends itself so the outer loop can
boot again, and a `Decided { ServerId }` memory so a reconnect does not re-probe.
Restore carries its own copy: `ServerState::of` re-derives bootstrap-ness, renames
such sessions to `phoenix-boot-N` scaffolding, applies, reconnects to a restored
session, re-captures the server, and retires or puts back each scaffold by what it
holds *now*.

**Laws.**
- `[LAW:no-ambient-temporal-coupling]` — correctness depends on what the process
  table showed at the instant of the read. The 160 ms `git` window is a sleep-shaped
  bet by another name. The diagnostic fails outright: run everything twice as fast or
  twice as slow and the daemon makes a different boot decision.
- `[LAW:types-are-the-program]` / `[LAW:parse-dont-validate]` — `is_bootstrap()`
  returns a `bool` and throws the proof away; every consumer asks again
  (`ServerState::of`, `Scaffold::retire`, `Store::save`, `Boot::misread`). The pane's
  `Foreground` is likewise re-derived from `command + argv` through a `SHELLS` list on
  every read instead of being a discriminator the pane carries.
- `[LAW:single-enforcer]` — one invariant ("never replace the user's live state with,
  or save over `latest`, a near-empty server") is enforced in the store, in restore's
  probe, and in the daemon's boot. They already disagree: the store refuses any
  bootstrap-only *snapshot*, while restore treats a bootstrap-only *server* as
  replaceable, and the daemon layers a third opinion (`Declined`/`Settled`) on top.
- `[LAW:no-mode-explosion]` — `ServerState{3} × Boot{2} × Cycle{2} × returning{2}`
  is the daemon's state space, none of it a domain concept. The `connect_and_boot`
  match has six arms; DESIGN.md §8's explanation is 40 lines of "when … and … only …
  except".
- `[LAW:dataflow-not-control-flow]` — the design sentence for boot restore contains
  "if", "only", "when", "except" and "unless" in every clause. That is the tell firing
  on the mechanics, not the consequences.
- `[LAW:decomposition]` / `[LAW:one-way-deps]` — `phoenix-store` now knows what a
  bootstrap session is; its one-sentence purpose ("atomic, versioned persistence")
  acquired an "and refuses snapshots that look like a login terminal". And
  `phoenix-restore` now depends on `phoenix-capture` (to probe), an edge DESIGN.md §2's
  graph does not have. Restore needed "what does the server hold?" and reached
  sideways for the only reader there was.

**Root cause.** The fact that matters is *provenance*: which server incarnation a
snapshot came from, and which session phoenix itself created in order to attach. Both
are knowable at the moment they happen — tmux's `#{pid}:#{start_time}` (already used
as `ServerId`) identifies a server incarnation, and the command that creates a session
knows it did. Phoenix records neither in the places that decide, so it infers an
adjacent, weaker fact from timing-sensitive evidence instead. Once a generation carries the
`ServerId` it was captured from, the daemon's entire boot question collapses to a
lookup: *have I ever saved from this server?* No process-table read, no provisional
decision, no store refusal, no re-boot. See `ARCHITECTURE.md` §3.

**Blast radius.** `boot.rs`, half of `daemon.rs`, `connect.rs` (800 lines), the
`BootstrapOnly` store arm, `Foreground`, `is_bootstrap`, and 1,384 lines of tests in
`phoenix-daemon/tests/{boot,resilience}.rs` (473 + 674) and
`phoenix-restore/tests/bootstrap.rs` (237) that exist to pin down the heuristic's edge
cases. All of it deletes.

---

## Finding 2 — two channels to one tmux server, and the transport is not the only process spawner

**Where.** `phoenix_restore::connect::run_plain` (spawns plain `tmux` for
`list-sessions`, `rename-session`, `kill-session`, `list-clients`, `switch-client`,
`new-session`), `phoenix_capture::argv` (spawns `ps`), versus
`tmux_control::transport::SpawnTransport` which DESIGN.md §3.2 calls "the *only*
place a process is spawned or a byte is read from the OS."

**What breaks.** Restore talks to the same server over two unrelated channels in one
operation: control mode for the plan, a fresh `tmux` subprocess per scaffolding step.
`run_plain` has its own error type, its own stderr classifier (`reports_no_server`),
and its own notion of "no server" that the control-mode side cannot see. The reason
given is real — `tmux -C attach-session` fails on an empty server — but it was solved
by growing a second channel in the wrong crate rather than by letting the control
client open with a different command. `SpawnTransport::spawn` already takes any argv;
the callers in restore and the daemon chose `attach-session` and then worked around
its failure outside the connection. Verified live on tmux 3.7b (2026-10-04): `tmux -C
new-session -s <name>` opens a control connection *on an empty server*, and a control
client can `switch-client` onto another session and survive the kill of the one it
left. The whole reconnect-after-restore dance exists because this was not known.

**Laws.** `[LAW:one-source-of-truth]` (two ways to say "run this against the server",
with two error vocabularies); `[LAW:effects-at-boundaries]` (process spawning leaked
out of the transport into restore and capture); `[LAW:locality-or-seam]` (the missing
seam is "a connection that can be opened on any server, empty or not", so the lack of
it rippled into 300 lines of scaffolding).

---

## Finding 3 — `Snapshot` is "what `list-panes -F` gave us", with degradation smeared across three `Option`s

**Where.** `phoenix_core::{Pane, CapturedProgram, Foreground}`,
`phoenix_cli::commands::is_degraded`, the daemon (which never reports degradation at
all), `phoenix_capture::fold`.

**What breaks.** `Pane { cwd: Option, program: { command, argv: Option }, content:
Option }`. Three independent optionals, each `None` for a different reason
(tmux could not read the cwd; `ps` failed or the process died; `capture-pane` failed
or content capture was simply off). "Degraded" is then a predicate the CLI derives by
scanning for `None`s, and the daemon doesn't derive it. `Foreground` — the one
classification the whole restore path and the whole bootstrap question hinge on — is
computed on every read from a shell-name list plus an "all remaining args start with
`-`" rule. And the structure itself is a strict tree `Session → Window → Pane`, which
is *not* tmux's model: tmux sessions hold *winlinks* to windows that may be shared
(grouped sessions, `link-window`). The parity backlog (`tmux-parity-ure.4/5/6`:
zoom, alternate pointers, grouped sessions) and the LLM epic (`tmux-llm-34m`: agent
identity per pane) will each have to bolt fields onto this shape; grouped sessions
cannot be expressed in it at all.

**Laws.** `[LAW:types-are-the-program]` (bag of optionals; the real structure lives in
folklore comments — "None means recovery failed for *this* pane"); `[LAW:parse-dont-validate]`
(`foreground()` is a validator run at every use, not a parse run once at capture);
`[LAW:one-source-of-truth]` (a shared window would be serialized once per session
that links it); `[LAW:composability]` (the next disparate requirement — grouped
sessions — needs a redesign, not a data fill).

---

## Finding 4 — "save" has two implementations, and content capture is a mode that one of them leaves off

**Where.** `phoenix_cli::commands::run_save` vs `phoenix_daemon::daemon::capture_and_save`;
`phoenix_capture::ContentCapture::{Off, On { previous }}`;
`phoenix_daemon::previous_content_from_snapshot`.

**What breaks.** The CLI's `save` runs with content `Off` and prints nothing about it
(PROJECT-GOALS §3 lists this as a gap; ticket `tmux-parity-ure.3`). The daemon runs
`On` and carries `previous` forward in memory, seeded from `latest` by a bridge
function only it has. Both callers compose capture + store differently; neither is
"the" save. The mode exists because dirty-tracking needs the previous indicators,
capture must not depend on the store (correct), and so the composition was pushed up
into each caller instead of into one place above both.

**Laws.** `[LAW:single-enforcer]` (two save paths); `[LAW:no-mode-explosion]` (`Off`
is a switch nobody plans to delete — and a default that silently discards the feature
phoenix is adopted for); `[LAW:no-silent-failure]` (structure-only save with no
notice); `[LAW:decomposition]` (there is no unit whose purpose is "the save operation",
so CLI and daemon each improvise one). Same shape for restore: the CLI and boot each
call `connect_and_apply` but handle `Reattach` differently.

---

## Finding 5 — the daemon is a 1-second poll wearing an event-driven coat, and the plan relies on execution order to find panes

**Where.** `tmux_control::Client` ("a sink only fires while some blocking call is
actively reading"), `phoenix_daemon::run`'s `display-message -p ""` heartbeat and
`thread::sleep(poll_interval)`; `phoenix_restore::plan`'s `panes_active_last` and
the "must be issued immediately after the pane it targets is created, before any later
split shifts *current* away" contract on `ReplayContent`/`RelaunchProgram`.

**What breaks.** The client has no reader of its own, so notifications arrive only as
a side effect of executing something; the daemon therefore executes a no-op every
second to drain them. The subscription covers only the attached session's windows, so
a change in another session is seen only by the max-interval backstop. On the restore
side, every pane-targeting step addresses "the current pane of `session:window`" and
is correct only because of where it sits in the sequence — the plan's doc comments
spend paragraphs defending the ordering. Verified live: `split-window -P -F
'#{pane_id}'` returns the created pane's id, and an attached-session subscription on
`#{S:#{W:#{window_layout}}}` fires for a change in *any* session. Neither is used.

**Laws.** `[LAW:no-ambient-temporal-coupling]` (ordering folklore in the plan;
drain-by-heartbeat in the daemon); `[LAW:dataflow-not-control-flow]` (a pane target
should be a *value* the step carries, bound when the split returns it);
`[LAW:effects-at-boundaries]` (the daemon loop interleaves clock reads, sleeps, tmux
I/O, store I/O and the decision in one function; only `DebounceState` is pure).

---

## Finding 6 — second clocks

Smaller, but each is a map that can drift from its territory
(`[FRAMING:representation]`, `[LAW:one-source-of-truth]`):

- **`latest` symlink vs. highest generation id.** `Store` asserts "latest always names
  the highest id" and maintains both. One is derivable from the other; the symlink is
  the second clock, and `tmux-store-i22` (readers racing a prune see `NoLatest` while
  generations exist) is exactly the drift it predicts.
- **`captured_at` written twice** per file: in the 32-byte header *and* in the body
  (`codec::encode_body` line 3). Decode reads the header's and ignores the body's.
- **DESIGN.md §6 and §8** have become transcripts of the implementation at the code's
  own altitude — "`run` returns and `run_resilient` boots again", "`TmuxError::{Send,
  Read, TransportClosed, NotReady}` mark the connection dead". That is a comment doing
  a type's job (`[LAW:comments-carry-meaning]`), and it drifts the moment the code
  moves; the §2 dependency graph has already drifted (Finding 2).
- **argv recovery** runs one system-wide `ps`, walks from each pane's shell down the
  children carrying the `+` (foreground process group) flag, and splits the winner's
  space-joined command string; it silently returns an empty map on any `ps` failure
  (`recover_argv`), so a broken `ps` reads as "every pane unknown" with no error
  anywhere (`[LAW:no-silent-failure]`), and a space inside one argument is
  indistinguishable from two arguments. Both facts `ps` is standing in for are
  readable directly, per pid: the terminal's foreground process group (`tpgid` in
  `/proc/<pid>/stat`; `e_tpgid` from `sysctl KERN_PROC_PID` on macOS) and that
  process's exact argv (`/proc/<pid>/cmdline`; `KERN_PROCARGS2`). The approximation
  is a choice, not a constraint.

---

## Finding 7 — the backlog encodes the wrong order

The parity epic is a strict chain (each child "blocked: earlier sibling"), with
`tmux-parity-ure.9` (hooks) at the top carrying an assignee from a session that never
started it. Five of the next six tickets — idempotent restore (.2), one-shot content
(.3), zoom (.4), alternate pointers (.5), grouped sessions (.6) — would be built *on*
the shapes Findings 1, 3 and 4 identify as the thing to replace: more arms in
`connect_and_apply`'s scaffolding match, more `Option`s on `Pane`, a third caller of
`ContentCapture`. The hooks ticket itself (.9) and per-program strategies (.7) land as
plan steps and a `Foreground -> Relaunch` function that only exist on the new shape, so
they wait too. Building parity
on the current foundation is paying carrying cost on a shape that is already scheduled
to go (`[LAW:carrying-cost]`: "no matter how far you've gone down the wrong road, turn
around"). The grooming that follows this audit puts the architecture epic ahead of
those tickets and leaves the ones that are independent of it (CI gates, test
flakiness, store reader lock) where they are.

---

## What is *not* a finding

To keep the signal honest, the things that were checked and are sound:

- `tmux-control`'s codec is total and pure; `ServerMessage` is a complete
  discriminated union with `Unknown`; `ConnectionState::closed` is a single enforcer.
- `Session::new`/`Window::new` make a dangling `active` index unrepresentable.
- `plan()` is pure and `--dry-run` prints the exact bytes `apply` sends.
- The store's temp+fsync+rename, exclusive save lock, non-zero retention from the flag
  down, and content-addressed blobs are all as DESIGN.md §7 claims.
- `DebounceState` is pure and clock-free.
- No `2>/dev/null`, no `|| true`, no silent fallback data sources anywhere.

These are the blocks the new architecture keeps.
