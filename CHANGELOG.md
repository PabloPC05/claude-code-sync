# Changelog

## 0.8.0 (fork: a stale window reloads itself)

In Syncthing mode, typing in a window that is behind no longer asks you to
type `/resume <id>` and retype your message: the window closes and reopens
itself on the latest chat and sends what you typed.

### Features

* **guard:** `guard install` puts a shell function named `claude` in the shell profiles: `profile.ps1` of Windows PowerShell and of PowerShell 7 under the Documents folder (OneDrive's when it redirects it), or `~/.bashrc` and `~/.zshrc` for the shells that exist. It runs the real `claude` with `CLAUDE_GUARD_SHELL` set for that process only (not for `-p`/`--print`) and, when Claude returns, reopens it if the prompt hook asked (at most 5 times in a row). It sits between `# >>> claude-code-sync guard >>>` and `# <<< claude-code-sync guard <<<`: install replaces the block, `guard uninstall` removes it, the rest of the file is never touched (bytes, encoding, byte order mark and line endings are kept). `--no-shell` skips it; `guard status` shows where it is installed; `--dry-run` covers it. On Windows `install` reports PowerShell's effective execution policy and warns, with `Set-ExecutionPolicy -Scope CurrentUser RemoteSigned`, when it is `Restricted` or `AllSigned` (it does not change it)
* **guard:** when the prompt hook finds a window stale, the Claude is a direct child of the shell named by `CLAUDE_GUARD_SHELL` and it is alive, the hook saves `{session_id, prompt, cwd, created_at}` in `<config dir>/guard/reload/<shell id>.json`, ends that Claude (SIGTERM, SIGKILL after 1.5 s; `TerminateProcess` on Windows) and blocks the prompt with a note that the chat is reloading. A nested Claude inherits the variable but not the parentage, so it never closes anything. If any condition fails, or ending the window fails, the 0.7.0 `/resume <id>` message is used and the request is deleted
* **guard:** `guard take-reload --shell <id> [--ps-legacy] [--format json|nul|b64] -- <original args>` consumes the request and prints the arguments of the relaunch: a whitelist of the original options (`--model`, `--permission-mode`, `--dangerously-skip-permissions`, `--allow-dangerously-skip-permissions`, `--add-dir`, `--settings`, `--setting-sources`, `--strict-mcp-config`, `--mcp-config`, `--verbose`, `--agent`, `--ide`, `--chrome`, `--no-chrome`, also as `--flag=value`), then `--resume <id>` and the typed text. `-c`, `--continue`, `-r`/`--resume`, `--fork-session`, `--session-id` and positional prompts are dropped. A text over 8000 characters is saved in `<config dir>/guard/reload/last-prompt.txt` instead of the command line, and a note on stderr says where. `--ps-legacy` escapes every argument for Windows PowerShell's legacy native-argument passing (the `CommandLineToArgvW` rules) so `claude.exe` receives the exact text. A request nobody took within two minutes is ignored

### Tests

* the wrapper is run by real shells (bash, zsh, pwsh, and Windows PowerShell 5.1 and pwsh in legacy mode on Windows) against a fake `claude` that asks for a reload with a text full of quotes, backslashes, a newline, ñ and an emoji; the second run must receive it exactly

## 0.7.0 (fork: Syncthing mode)

For two PCs whose `~/.claude/projects` is carried by Syncthing instead of git.
Git mode is unchanged.

### Features

* **guard:** `claude-code-sync guard install|uninstall|status|sweep|hook`: a Syncthing mode that needs no git, no sync repository and no `init`. `install` adds exec-form hooks for `UserPromptSubmit` (`guard hook prompt`), `SessionStart` (`start`), `Stop` (`stop`) and `SessionEnd` (`end`), sets `cleanupPeriodDays` to 3650 (Claude Code's 30-day cleanup would propagate deletions through Syncthing), leaves other hooks alone and refuses while the 0.6.0 autosync hooks are installed; `autosync install` refuses while the guard's hooks are installed
* **guard:** each window keeps a record in `<config dir>/guard/windows/<pid>.json`: its transcript's file identity (device and inode; volume serial and file index on Windows), its leaf and the uuids it loaded. Syncthing replaces a file by renaming a temporary one over it, which changes the identity; Claude's own appends do not
* **guard:** the prompt hook blocks a prompt typed in a window whose file was replaced and holds messages it never loaded, naming the exact `/resume <id>`; a window's own appends (an Esc-interrupted turn included) and replacements that only add metadata never block; nothing changed means no parsing
* **guard:** when a window that was behind closes, what it appended off its old leaf is removed (`/exit`'s entries, which made `claude --continue` show the old branch) or, if real messages, moved to a `(rama paralela)` session; under another live window on the same PC only the `/exit` entries go
* **guard:** `*.sync-conflict-*` copies are settled by every hook and by `guard sweep`: session transcripts are reconciled like a pull does (take over, combine, or split into a parallel session; a chat open in a window is only appended to, and that window is then behind), other `.jsonl` files become the union of both, `history.jsonl` the union ordered by `timestamp`; other copies are left alone, and the result is the same on both PCs

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
