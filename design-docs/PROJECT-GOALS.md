# tmux-phoenix — project goals

**What this document is.** The product-level view of tmux-phoenix: what it's for, what it
does today, how it measures against the tools it replaces, and where it's going. `DESIGN.md`
is the engineering spec (crate boundaries, wire protocol, on-disk formats); this document is
the feature-level view above that — what a user gets, not how it's built. Update this
alongside `DESIGN.md` as scope moves; this file should always describe the intended product,
not just the milestones already closed.

---

## 1. What tmux-phoenix is

tmux-phoenix is a single Rust binary that captures the full state of a running tmux server —
every session, window, and pane, their layout, working directories, and (optionally) their
scrollback — persists it durably, and reconstructs it later. It replaces two widely-used
tmux plugins, [`tmux-resurrect`](https://github.com/tmux-plugins/tmux-resurrect) (manual
save/restore) and [`tmux-continuum`](https://github.com/tmux-plugins/tmux-continuum)
(continuous autosave + restore-on-boot on top of resurrect), with one tool built around a
persistent tmux control-mode connection instead of their shell/`fork`-`exec`-per-pane
approach.

**This is a first draft, not a finished product.** The first six milestones (M0–M5, closed)
established a working core loop — capture, persist, restore, daemonize — but a from-scratch
rewrite's first pass naturally lands short of a 10-year-old, widely-deployed tool's edge
cases. Section 3 states plainly where that's true. The floor for "done" is real, full parity
with what tmux-resurrect + tmux-continuum support today (Section 3); above that floor, the
goal is a genuinely more capable, more modern session-management toolbox (Section 4), not a
like-for-like clone.

**Guiding priorities, in order: stability, speed, then the efficiency that underwrites
both.** Concretely:

- **Stable** — a save is atomic (a crash mid-write never corrupts `latest`); the protocol
  codec can't panic on malformed input; restore is safe to preview (`--dry-run`) before it
  touches a live session; a daemon that loses its tmux connection reconnects rather than
  exiting.
- **Fast** — one persistent control-mode connection to tmux replaces resurrect's pattern of
  spawning a new process per pane/window during save and restore.
- **Efficient** — the daemon subscribes to *structural* changes only and asks tmux to never
  stream pane output at all (`no-output`), so an idle session costs the daemon almost
  nothing; scrollback content is fetched on demand and deduplicated by content hash across
  saves, so an unchanged pane doesn't re-write its content on every save.

---

## 2. Current state — what's shipped

Everything below is implemented, tested, and (where noted) verified against a live tmux
server, not just unit-tested in isolation. Milestones M0–M5 are closed; see `DESIGN.md` for
the engineering detail behind each item.

**Capture.** `phoenix save` interrogates a running tmux server over one control-mode
connection and builds a complete snapshot: every session, window, and pane; each pane's
working directory, layout, and foreground command; and, on the daemon's save path (see
below), each pane's visible screen and scrollback. Structure capture is all-or-nothing (a
torn tree is never persisted); if a single pane's content can't be captured, that pane's
content is dropped and the save is marked degraded rather than failing outright.

**Persistence.** Saves are atomic (write-then-`rename`) and versioned, with the last N
generations kept (`--keep`, default 10) and a `latest` pointer that only ever names a
fully-written snapshot. Pane scrollback is stored content-addressed — two saves whose pane
content is byte-identical (the common case for an untouched pane) store it once, not twice.
`phoenix list` shows saved generations; `phoenix restore --file <path>` restores a specific
one, not just the newest — a direct point-in-time-restore command where tmux-resurrect
requires manually re-pointing a symlink first.

**Restore.** `phoenix restore` rebuilds a snapshot's session/window/pane tree — exact
layout, working directories, active window — into a new tmux session. `--dry-run` prints the
exact tmux commands a real restore would run without running any of them, a safety property
tmux-resurrect doesn't have (it has no preview mode at all). Captured scrollback is replayed
into each pane immediately after it's created, so restored panes aren't just structurally
correct but show their prior history.

**Program relaunch.** Restore defaults to *shell + working directory only* — it never
blindly re-executes whatever was running in a pane. Relaunching a pane's captured program is
consent-gated: interactively, each candidate is a prompt (once / always for this exact
command / always for this program / no); a granted decision is remembered
(`${XDG_CONFIG_HOME}/tmux-phoenix/relaunch.rules`) so the same prompt doesn't repeat. This is
a different mechanism from tmux-resurrect's static `@resurrect-processes` allowlist string —
phoenix's version is interactive and self-training rather than something you hand-edit in
`.tmux.conf` — but it does not yet cover resurrect's per-program *strategies* (see §3).

**Continuous daemon.** `phoenix daemon` holds one open connection and subscribes to
structural change notifications; it saves once activity has been quiet for a debounce window
(default 10s) with a max-interval backstop (default 300s) so long-idle sessions still
checkpoint. This is event-driven, not a poll loop, and — unlike tmux-continuum — it does not
depend on the tmux status line being on or on no other plugin having clobbered
`status-right`. On start, if the target server has no sessions at all, it restores the latest
snapshot automatically ("boot restore"); if sessions already exist, it logs and stays in save
mode, never overwriting a live server. The daemon reconnects automatically if tmux isn't
running yet or goes away mid-run — it never just exits.

**Installation.** `phoenix install` writes a real `launchd` user-agent plist (macOS) or
`systemd --user` unit (Linux) that runs `phoenix daemon`, and prints the exact command to
activate it — it never activates the service itself. This is a step further than
tmux-continuum's boot integration, which opens an actual terminal-emulator window on macOS
and, by its own docs, has only thin/incomplete Linux support.

**Protocol foundation.** All of the above sits on `tmux-control`, a complete, independent
Rust implementation of the tmux control-mode wire protocol (not a phoenix-internal detail —
it's a reusable crate in its own right), including full notification decoding, subscriptions,
and a typed connection-state machine that survives reconnects and never panics on malformed
input.

---

## 3. Feature floor: parity with tmux-resurrect + tmux-continuum

This is the accountability section. Every row is checked against the two plugins' actual
current documentation (tmux-plugins/tmux-resurrect and tmux-plugins/tmux-continuum on
GitHub), not memory of what they do.

### Already at or ahead of parity

| Capability | tmux-resurrect / continuum | tmux-phoenix |
| --- | --- | --- |
| Session/window/pane tree, layout, cwd | Yes | Yes |
| Active window per session, active pane per window | Yes | Yes |
| Scrollback capture | Opt-in (`@resurrect-capture-pane-contents`), documented to break under certain `default-command` configs | Yes, on the daemon path; dirty-tracked so unchanged panes are cheap to re-save |
| Restore preview before touching a live session | None | `--dry-run` |
| Restoring a specific past generation | Manual: re-point the `last` symlink yourself, then restore | `phoenix restore --file <path>` directly |
| Autosave mechanism | Hijacks `status-right` redraws; silently stops if the status line is off or another plugin overwrites `status-right` | Event-driven off tmux's own change subscriptions; no status-line dependency |
| Autosave failure mode | Fragile, undetectable when broken | Debounce + max-interval backstop, logged failures, connection auto-recovers |
| Boot-time service integration | macOS: opens a real terminal window and runs `tmux` in it. Linux: starts only the bare tmux server, docs call this "incomplete," help wanted | Generates a real `launchd`/`systemd --user` unit that runs the daemon headless on both platforms |
| Program relaunch consent | Static allowlist string in `.tmux.conf`, user must hand-edit `~`/`->`/`*` syntax | Interactive prompt at restore time, remembered per-program or per-exact-command |
| Multi-server disambiguation | Only the first-started server gets autosave/autorestore; later servers get neither | `--socket` explicitly targets any server by name or path; each daemon instance is scoped to the socket it's given |

### Real gaps — not yet at parity

These are documented tmux-resurrect/continuum behaviors that tmux-phoenix does not
implement. None are architecture blockers; they're scoped work.

- **Zoomed-pane layout.** tmux-resurrect explicitly preserves a pane's zoomed state through
  restore. tmux-phoenix's snapshot doesn't currently capture or restore zoom.
- **Alternate session/window.** tmux keeps a "last used" pointer per session and per client
  (what `prefix + l` jumps back to). tmux-resurrect restores it; tmux-phoenix's domain model
  only tracks the single *active* window/pane, not the alternate one.
- **Grouped sessions** (multiple sessions sharing one set of windows, tmux-resurrect's
  multi-monitor use case) aren't represented in tmux-phoenix's snapshot at all.
- **Idempotent restore.** tmux-resurrect skips any session/window/pane that already exists
  on the target server rather than erroring — the one deliberate exception being a lone
  bootstrap pane, which it overwrites. `phoenix restore`'s plan always emits a bare
  `new-session -d -s <name>`; if a session with that name already exists on the target
  server, the restore fails outright instead of merging or skipping.
- **Per-program resume strategies.** tmux-resurrect ships a real strategy system beyond
  "relaunch the same command line" — `vim`/`nvim` restore via a `Session.vim` file (actual
  editor state, not just re-opening the binary), and a dedicated `mosh-client` strategy that
  correctly re-extracts and replays the original Mosh connection arguments. tmux-phoenix's
  consent-gated relaunch is a better *safety* model than resurrect's static config, but it
  has no equivalent of "restore this program *well*," only "restore this program's command
  line, or don't."
- **One-shot `save` doesn't capture content.** `phoenix save` (the plain CLI command, not the
  daemon) always runs with content capture off — it has no clean way to hold "the previous
  save's content" for dirty-tracking the way the daemon does in memory across cycles. A user
  running bare `phoenix save` gets structure only, no scrollback, silently — the daemon is
  currently the only path that actually delivers on the "save my scrollback" promise.
- **Restoring onto a from-scratch server.** `phoenix restore` (the CLI command) connects via
  `attach-session`, which requires an existing session on the target — it cannot restore
  directly onto a completely empty or not-yet-running tmux server. (`phoenix daemon`'s boot
  restore handles exactly this case already, via a throwaway bootstrap session; the CLI
  command doesn't share that logic.) This surfaced during manual verification as a
  cryptic `transport closed before the command's reply arrived` error rather than a
  clear message, let alone working. **This is explicitly in scope to close as real
  functionality**, not to leave as a documented limitation — see §4.
- **Status-line integration.** `#{continuum_status}` lets a user's tmux status bar show the
  current autosave interval or `off`. tmux-phoenix has no equivalent format-string output at
  all today — there's no way for a user's own tmux config to show "daemon running, last
  saved 2m ago" without shelling out themselves.
- **Not a tmux plugin.** tmux-resurrect and tmux-continuum are TPM-installable plugins with
  default keybindings (`prefix + Ctrl-s` / `Ctrl-r`) out of the box. tmux-phoenix today is a
  bare binary invoked from a shell or a background service — there's no tmux-side
  integration a user drops into `.tmux.conf` to get keybindings or status-line output for
  free.
- **No hooks.** tmux-resurrect exposes pre/post save and restore hooks (shell commands run
  at defined points, e.g. to capture/restore X11 window geometry alongside the tmux state).
  tmux-phoenix has no extensibility point like this.

---

## 4. Roadmap

### 4.1 Close the floor

Bring tmux-phoenix to genuine, no-caveats parity with tmux-resurrect + tmux-continuum, and
fix the rough edges found while doing so — with working functionality, not documentation
explaining why something is out of scope.

- Capture and restore zoomed-pane state.
- Capture and restore each session's alternate window and each client's alternate session.
- Support grouped sessions (multiple sessions attached to one shared window set).
- Make restore idempotent: skip a session/window/pane that already exists on the target
  server instead of failing the whole restore, matching tmux-resurrect's behavior (including
  its single-bootstrap-pane overwrite case).
- Unify `phoenix restore`'s connection strategy with the daemon's boot-restore path so it
  works uniformly whether the target server is empty, freshly started, or already populated
  — no more "restore doesn't work if nothing's running yet."
- Wire content capture into the one-shot `phoenix save` path (needs a way to load the
  previous save's per-pane content outside the daemon's in-memory state, so dirty-tracking
  works for a single invocation too).
- Add per-program resume strategies on top of the existing consent-gated relaunch model:
  vim/neovim session-file restore, a Mosh-aware strategy, and an extensible way to add more
  — consent-gating stays as the safety layer; strategies decide *how* to relaunch what's
  already been approved, not whether to.
- Ship a real tmux plugin wrapper (TPM-installable) with default keybindings for save/restore
  and a `#{phoenix_status}`-style format string for the status line, so tmux-phoenix doesn't
  require leaving tmux to use.
- Add a pre/post save and restore hook mechanism.

### 4.2 Beyond parity — a modern toolbox

Once the floor is solid, the differentiated goal: things tmux-resurrect and tmux-continuum,
as shell-script-era tools, were never positioned to do.

- **Named, tagged snapshots.** `phoenix save --tag before-migration` alongside the automatic
  generations, so a deliberate checkpoint is easy to find and never gets pruned by `--keep`.
- **Selective/partial restore.** Restore one session or one window out of a snapshot instead
  of the whole tree — useful when only one project's layout needs recreating, not the entire
  captured server.
- **A generation picker**, not just `--file <path>` — an interactive fuzzy-searchable list of
  past saves (timestamp, session names, degraded/clean status) to restore from, replacing
  resurrect's manual symlink dance entirely rather than just improving on it.
- **Full-text search across saved scrollback.** Since content capture already exists and is
  content-addressed, "which past session had this command or output in it" becomes a real,
  answerable query instead of something a user has to remember and manually grep for.
- **A snapshot diff/preview beyond `--dry-run`'s command list** — a readable summary of what
  changed between the live server and the snapshot being restored (sessions/windows/panes
  added, removed, or altered), not just the raw tmux commands that would run.
- **A daemon that supervises every active tmux server on a machine**, not one instance per
  socket started by hand — matching or exceeding continuum's (fragile) first-server-only
  behavior with something that scales to however many servers are actually running.
- **Cross-machine snapshot portability** — a snapshot directory that can live in a synced
  location (and eventually a defined transfer/import path), so "resume this session on
  another machine" becomes a real, supported workflow rather than something a user
  improvises with `rsync` and hope.
- **Declarative session templates** — define a named layout (sessions/windows/commands) up
  front and materialize it on demand, in the spirit of tools like tmuxinator/teamocil, but
  sharing tmux-phoenix's own snapshot/restore machinery instead of being a separate tool.
- **Security-conscious content handling.** Captured scrollback can contain secrets typed or
  echoed into a shell. Given this project's stated priority of improving on the old tools'
  security posture where possible, this deserves real design work: at minimum, a documented
  way to exclude specific panes/sessions from content capture; further out, encryption of
  stored scrollback at rest and/or redaction heuristics.

---

## 5. Deliberate divergences — keep these

Not every difference from tmux-resurrect/continuum is a gap to close. These are intentional
and should stay even as the floor gets filled in:

- **One persistent control-mode connection**, not a process spawned per pane/window/command.
  This is the root of tmux-phoenix's speed and stability advantage over both old tools, which
  are fundamentally shell scripts that `fork`/`exec` tmux repeatedly.
- **Event-driven, subscription-based save timing**, not a status-line-redraw hack or a bare
  polling timer. This is strictly more robust than tmux-continuum's mechanism and has no
  equivalent failure mode.
- **Consent-gated program relaunch as the default safety layer.** Even as per-program resume
  strategies (§4.1) get added, restore should never silently re-execute an arbitrary captured
  command line the way `':all:'` in tmux-resurrect's config can. Strategies extend *how* an
  approved relaunch happens, not a way around asking.
- **Content-addressed, deduplicated scrollback storage**, rather than a new flat copy of pane
  content on every save. This is already more storage-efficient than anything either old tool
  does.

---

## 6. Open questions

Things worth a real decision before or during the relevant roadmap work, not yet resolved:

- How should per-program resume strategies (§4.1) and the consent-gated ruleset compose in
  the config/rules file format — is a strategy a property of a rule, or a separate
  registration a rule can reference?
- Should cross-machine snapshot sync (§4.2) be a built-in transfer mechanism, or should
  tmux-phoenix only guarantee that its storage format is safe to sync externally (e.g. via a
  user's own `rsync`/Syncthing/cloud-drive setup) and stop there?
- What's the right default for excluding sensitive content from capture (§4.2) — an opt-out
  per pane/session, an opt-in-only model (matching resurrect's own default-off stance on
  content capture), or a heuristic redaction pass? This has real security implications and
  shouldn't be decided as a side effect of implementing something else.
