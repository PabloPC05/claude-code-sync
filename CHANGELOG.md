# Changelog

## 0.6.0 (fork: open windows, stale-window guard, per-turn push)

A chat left open on one PC while the other continues it no longer waits for the
window to close.

### Features

* **pull:** a session open on this machine receives the other PC's turns by appending: when the repository copy only added entries (or added messages while the local side added metadata), the missing entries are appended in one write in append mode, never renamed or rewritten; diverged chats, subagent transcripts and sidecar files stay deferred
* **stale:** a pull that appended to an open chat records the windows holding the old version in `<config dir>/stale/<session>.json`; `SessionStart` (also `/resume` inside a running window) clears the mark, and `claude_ancestors` finds the window from a hook's parent process (CreateToolhelp32Snapshot on Windows)
* **hook:** `claude-code-sync hook prompt|session-start|session-end|stop`: `prompt` blocks a prompt typed in a stale window and tells the person the exact `/resume <id>` that reloads it (the picker never lists the chat open in its own window); `session-start`, `session-end` and `stop` start a detached `autosync run`; every path exits 0 and does no network work
* **repair:** when a stale window closes, what it appended off its old leaf is taken out of the transcript (the `/exit` entries, which made `claude --continue` show the old branch), or, if it holds real messages, moved to a `(rama paralela)` session; the sweep at the start of every sync repairs windows that died without a `SessionEnd`; rewind and edit branches are never touched
* **autosync:** `install` adds exec-form hooks for `UserPromptSubmit`, `SessionStart`, `SessionEnd` and `Stop` (replacing the 0.5.0 `SessionEnd` hook); `uninstall` removes them, `--no-hooks` installs none, `status` lists them. Run `autosync install` again after upgrading
* **autosync:** a run that finds the sync lock taken leaves `sync.rerun`; the holder syncs again when done (at most 3 extra rounds), so a turn that finished during a sync is pushed promptly

### Bug Fixes

* **branches:** writing a transcript retries the rename a few times on Windows, where a scanner can hold the file for a moment

## 0.5.0 (fork: roots, parallel branches, autosync)

First release of the fork [PabloPC05/claude-code-sync](https://github.com/PabloPC05/claude-code-sync),
based on perfectra1n/claude-code-sync v0.4.1. The install scripts, `self-update`
and cargo-binstall now fetch this fork's releases.

### Features

* **project_map:** root map — `config --map-root <id>=<path>` / `--unmap-root <id>`; every project below a mapped folder syncs as `@<id>@<path below it>` and lands in each machine's own encoded directory; the home directory is the implicit root `home` (`auto_home_root`, `--auto-home-root`)
* **pull:** a session continued on two machines is split into the session and a deterministic parallel branch (`<title> (rama paralela)`) instead of being merged into one file; fast-forwards and metadata-only changes settle without a branch; live sessions are deferred; transcripts leave git's union merge so the sync repository never interleaves them; `pull -i` offers "parallel branch" first
* **autosync:** `autosync install|uninstall|status|run` — LaunchAgent, systemd user timer (or a crontab line) or a hidden Task Scheduler task, plus a `SessionEnd` hook; raises Claude Code's `cleanupPeriodDays` to 3650; one run at a time through a lock file; never prompts
* **config:** new configs allow transcripts up to 95 MiB, and an oversized transcript is reported in the log too

### Bug Fixes

* **project_map:** encode project paths longer than 200 characters the way Claude Code does (cut and hashed)
* **project_map:** a project only pulled so far is pushed back under its root name, not duplicated under the local encoded name
* **filter:** include/exclude patterns apply to a project's attachments, session sidecar directories and memory, in push, pull, status and purge; excluded projects are not reported for size
* **merge:** a smart-merged transcript ends with the `last-prompt` of its most recent branch, so resume continues the newest one
* **push:** a transcript whose repository copy holds entries this machine lacks (a session live here, or one not pulled yet) is held back instead of pushed over them
* **autosync:** the `SessionEnd` hook uses Claude Code's exec form (no shell), so it also works where hooks run under PowerShell
* **pull:** the summary counts only diverged transcripts as conflicts

## [0.4.1](https://github.com/perfectra1n/claude-code-sync/compare/v0.4.0...v0.4.1) (2026-10-10)


### Bug Fixes

* **pull:** never lose a local edit — three-way pull, combining [#105](https://github.com/perfectra1n/claude-code-sync/issues/105) and [#106](https://github.com/perfectra1n/claude-code-sync/issues/106) ([#107](https://github.com/perfectra1n/claude-code-sync/issues/107)) ([ac3a329](https://github.com/perfectra1n/claude-code-sync/commit/ac3a32972d4b54d7b75f64df1565d7760537d887))

## [0.4.0](https://github.com/perfectra1n/claude-code-sync/compare/v0.3.3...v0.4.0) (2026-09-24)


### Features

* one-line installers, package-manager channels, ARM64 builds and self-update ([#98](https://github.com/perfectra1n/claude-code-sync/issues/98)) ([0278e84](https://github.com/perfectra1n/claude-code-sync/commit/0278e84103025edfe8c0223186249cba341fba1e))

## [0.3.3](https://github.com/perfectra1n/claude-code-sync/compare/v0.3.2...v0.3.3) (2026-07-13)


### Miscellaneous Chores

* adopt release-please + mise-driven CI ([#92](https://github.com/perfectra1n/claude-code-sync/issues/92)) ([67fb73b](https://github.com/perfectra1n/claude-code-sync/commit/67fb73b94e9dabbc25f0707d16e792c31ef1dcbb))
* make chore/refactor/docs/style commits release-worthy ([#94](https://github.com/perfectra1n/claude-code-sync/issues/94)) ([6fb1467](https://github.com/perfectra1n/claude-code-sync/commit/6fb14673abc9e73c292542671c963f7f6799aae6))


### Code Refactoring

* reshape three oversized files into focused modules ([#93](https://github.com/perfectra1n/claude-code-sync/issues/93)) ([286b3b4](https://github.com/perfectra1n/claude-code-sync/commit/286b3b4fb5c833cd2654efafed98f27509e9dc11))
