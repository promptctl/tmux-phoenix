# tmux-phoenix

A single Rust binary that captures the full state of a running tmux server — every session, window and pane, their layout, working directories and optionally their scrollback — persists it, and rebuilds it later. It is a replacement for the [tmux-resurrect](https://github.com/tmux-plugins/tmux-resurrect) + [tmux-continuum](https://github.com/tmux-plugins/tmux-continuum) pair, built around one persistent tmux control-mode connection instead of shell scripts that `fork`/`exec` tmux once per pane.

## Status: you cannot use this yet

Nothing is released. There is no published crate, no binary to download, and no install command — if you came here looking for a tmux-resurrect alternative you can run today, use tmux-resurrect and tmux-continuum; come back later.

What is on GitHub right now is the design document, the license, and one open pull request carrying the first code slice (the control-mode codec and transport). The rest of the implementation is written and tested but unmerged, waiting behind that review. No dates are promised here because none are known.

Read on if you want to evaluate the approach, follow the work, or contribute to it.

## What it does differently

tmux-resurrect and tmux-continuum are shell scripts, so every pane they save or restore costs a process spawn. tmux-phoenix opens one control-mode connection to the tmux server and keeps it: capture is a handful of round-trips over a connection that is already open, and a single `list-panes -a` pulls every session, window and pane at once. That is the root of the speed difference and, because there are far fewer moving parts per operation, of the stability difference too (DESIGN.md §1).

Autosave timing is the second difference. tmux-continuum has no timer at all — it hijacks `status-right` redraws to decide when to save, so it silently stops working if your status line is off or another plugin overwrites `status-right`. The tmux-phoenix daemon instead subscribes to tmux's own structural-change notifications, saves once activity has been quiet for a debounce window, and keeps a max-interval save as a backstop so long-idle sessions still checkpoint. It also asks tmux never to stream pane output to it (`no-output`), so an idle connection costs almost nothing until something actually changes (DESIGN.md §3.4, §8).

Three smaller things follow from being a program rather than a script. Restore is planned as data before it runs, so `phoenix restore --dry-run` prints the exact tmux commands a real restore would execute — worth having in a tool that can `send-keys` into live shells, and something tmux-resurrect has no equivalent of. `phoenix restore --file <path>` restores a specific past generation directly, where tmux-resurrect wants you to re-point a symlink by hand first. And pane scrollback is stored content-addressed, so an unchanged pane's content is written once and pointed at again rather than copied on every save.

## What it does not do yet

These are documented tmux-resurrect/continuum behaviors that tmux-phoenix does not have, and they are the project's own accounting, not an outside audit:

- **Zoomed panes, alternate window/session, and grouped sessions** are not represented in the snapshot at all.
- **Restore is not idempotent.** tmux-resurrect skips a session that already exists on the target server; tmux-phoenix's plan emits a bare `new-session` and the restore fails outright instead.
- **No per-program resume strategies.** Restore replays a pane's captured command line and nothing more — there is no equivalent of tmux-resurrect restoring `vim` through its session file, or of its Mosh strategy.
- **One-shot `phoenix save` captures structure only, not scrollback.** Only the daemon's save path currently delivers content capture.
- **No tmux-side integration.** It is not a TPM-installable plugin, there are no default keybindings, there is no `#{continuum_status}`-style format string for your status line, and there are no pre/post save and restore hooks.

## Design

[`DESIGN.md`](DESIGN.md) is the engineering spec and the place to argue with the approach: the wire protocol, the crate boundaries, the on-disk format, and the reasoning behind each. It is on `main` and readable now. The product-level view above it — the full parity table both sections above are drawn from, and the roadmap past parity — lives in `design-docs/PROJECT-GOALS.md`, which arrives with the slice that introduces it.

The three stated priorities, in order, are stability, speed, and the efficiency that underwrites both; DESIGN.md §1 tabulates what each one rules out. Two consequences shape most of the code. The protocol codec is pure, total and panic-free, with an `Unknown` arm that absorbs anything unrecognized, so a malformed line degrades to data instead of crashing the daemon (§3.1). And a save is written to a temp file and `rename(2)`d into place, so a crash mid-save leaves the last good snapshot untouched (§7).

## Crate layout

Seven crates, with dependencies flowing strictly downhill — a crate depends only on crates in rows below it (DESIGN.md §2):

```
phoenix-cli
phoenix-daemon
phoenix-capture   phoenix-restore   phoenix-store
tmux-control      phoenix-core
```

- **`tmux-control`** — a complete, standalone Rust implementation of the tmux control-mode protocol: a pure codec, an effect transport that owns the `tmux -C` child, and a client that correlates commands to replies through a typed connection state machine. It knows nothing about tmux-phoenix and is a reusable crate in its own right.
- **`phoenix-core`** — the domain model: the `Snapshot` tree, pure types, no I/O.
- **`phoenix-capture`** — drives `tmux-control` to interrogate a live server into a `Snapshot`.
- **`phoenix-store`** — atomic, versioned, generational persistence.
- **`phoenix-restore`** — the pure `Snapshot -> RestorePlan` planner; `tmux-control` executes the plan.
- **`phoenix-daemon`** — keeps one tmux server's state alive across restarts.
- **`phoenix-cli`** — maps command lines onto the crates beneath it; builds the `phoenix` binary.

## Where the work happens

`main` holds the design document and the license; there is no code on it yet. The implementation is split into a chain of review slices on `slice/*` branches that merge into `main` one at a time, and each becomes visible here when it is pushed for review — so far that is `slice/02-protocol-codec`, the `tmux-control` codec and transport, open as PR #2. The later slices exist but are not pushed yet, which is why the crates listed above are not all browsable on GitHub today.

## Building

There is nothing to build on `main`. On a slice branch that carries the workspace, `cargo build` and `cargo test` at the repository root build the workspace and run its tests; every crate depends only on its siblings by path, so there is nothing to fetch from crates.io.

## License

MIT — see `LICENSE`.
