# iPad + Blink release acceptance

This matrix is the human release gate for the exact remote route. Host tests, Unix PTYs, and tmux automation are prerequisites, not substitutes for feel or key-delivery approval.

## Agent vs human split

Most of the busywork can be done by an agent or script on the host. Only the **real Blink → mosh/ssh → tmux → wscrpt** feel/key-delivery pass requires you on the iPad.

| Who | What |
| --- | --- |
| **Agent / host script** | Build/install binary identity; fill host fields of the route record; create the disposable Git fixture; snapshot hashes/`git status`; map which matrix rows are already covered by `scripts/verify.sh` / CI; draft the short human checklist |
| **Human on iPad** | Delayed Escape, typing latency, reconnect, clipboard attempt, shell handoff feel, clean exit — the short list in `HUMAN_PASS.md` after prep |

**Fast path (~15–25 min human):**

```sh
scripts/ipad-matrix-prep.sh
# open the printed HUMAN_PASS.md on the iPad session and walk that table only
```

The full matrix below remains the authoritative long form when something fails or for a major release.

## Route record

Complete every field before testing:

| Field | Value |
| --- | --- |
| Device / keyboard | |
| iPadOS | |
| Blink version | |
| Transport (`ssh` or `mosh`) | |
| Remote host / OS / architecture | |
| tmux version, socket/session, and active state | |
| `TERM`, `COLORTERM`, locale, `SSH_*`, and `MOSH_*` | |
| Source revision / checkout cleanliness | |
| Installed binary path | |
| Installed binary SHA-256 | |
| `wscrpt --version` | |
| `wscrpt --health` | |
| Date / reviewer | |

Confirm the launched binary is the installed `wscrpt` editor (not `/usr/bin/w` or any other tool).

## Fixture

Use a disposable Git workspace containing:

- at least three UTF-8 text files in nested directories;
- long wrapped lines, tabs, combining marks, emoji, and CRLF text;
- one ignored directory and one binary file;
- a `.wscrpt/tasks.toml` task that prints Unicode and one parseable error location;
- an explicitly configured language server;
- committed, modified, staged, and untracked Git paths;
- a second shell connected to the same host for external-change, reconnect, and cleanup checks.

Record before/after file hashes and `git status --porcelain=v2`. Git inspection in `wscrpt` must not change either the index or worktree.

## Acceptance matrix

Each row needs a human result and evidence location. `PASS` means the behavior is both correct and usable on the recorded route.

| ID | Exercise | Required observation | Result / evidence |
| --- | --- | --- | --- |
| A01 | Launch `wscrpt .` inside the recorded tmux/mosh route. | A usable first frame appears promptly while Indexing, Git loading, or Recovery scanning may still be visible. No startup freeze occurs. | |
| A02 | Run `wscrpt --input-diagnostics`; deliver delayed `Esc` then `s`, rapid `Esc`+`s`, `Ctrl-K`, `Ctrl-G`, arrows, Shift-arrows, Option/Alt variants, a multiline native paste, and a resize. | The transcript matches the actual route. Escape/action input is reliable, reserved keys are understood, paste is one event, and terminal modes restore on exit. | |
| B01 | Type continuously in two source files, including fast repeats and backspace. | Typing latency and redraw cadence feel acceptable; no dropped, duplicated, or reordered text. | |
| B02 | Enter `Esc`, wait several seconds, then complete actions and nested prefixes. Repeat with `Ctrl-K`. | The action layer never times out and command characters never leak into the buffer. | |
| B03 | Paste and edit ASCII, accented text, combining marks, CJK, and emoji. Undo/redo. | Unicode editing, grapheme movement/deletion, bracketed paste, selection, and undo boundaries are correct. | |
| B04 | Toggle soft wrap and navigate/edit long mixed-width lines before and after terminal resize. | Wrapped vertical movement, cursor placement, page movement, selection, and resize reflow remain coherent. | |
| C01 | Use Quick Open, Workspace Tree, sidebar, Open Path, Recent Files, dirty-buffer navigation, and Reopen Closed Buffer. | Navigation is responsive and opens the intended path without losing dirty buffers. Pending index state is explicit rather than blocking. | |
| C02 | Run project search, filter results, navigate a result, edit externally, then use `Esc w R`. | Search is usable, partial/pending state is visible, and explicit refresh replaces the old snapshot without stale results winning. | |
| C03 | Use in-buffer literal and regex find/replace, including empty replacement and undo. | Only the active buffer changes, the edit is explicit and undoable, and saving remains explicit. | |
| D01 | Trigger LSP completion, hover, definition, references, document/workspace symbols, diagnostics, and Problems. | Completion latency and navigation feel acceptable; stale results never reopen UI or move to the wrong document. | |
| D02 | Format one document with unsaved text, then undo. | Formatting changes only that document as one editor edit, does not auto-save, and refuses stale responses. | |
| E01 | Inspect the task catalog/details, request a task, refuse trust, then request again and approve. | No process starts before approval. The approved task runs with bounded Unicode output and its problem location is navigable. | |
| E02 | Run, rerun, and stop a task containing a foreground child. | Output remains responsive and stop cleans up the owned Unix process group/descendants. | |
| F01 | Use all retained Git views and pickers, then compare captured Git status and file hashes. | Status/diff/log/commit/history/HEAD/blame/branch data is useful and read-only; index and worktree are unchanged. | |
| F02 | Press `Esc t t`, run terminal programs and Git mutation, then exit the shell. Repeat via `:terminal`. | The TUI releases cleanly, the shell starts in the workspace, job control/Unicode/resize work, exit returns to the same editor state, and a full redraw is clean. | |
| G01 | Copy a line, a Unicode selection, a file location, and a problem. Test with OSC 52 enabled and disabled. | The internal register always works. The clipboard attempt succeeds or fails visibly without corrupting terminal output. | |
| H01 | Leave multiple buffers/cursors/bookmarks/sidebar/soft-wrap state, disconnect mosh, reconnect, and continue. | tmux/mosh continuity is usable and no key/redraw storm occurs after reconnect. | |
| H02 | Exit cleanly, launch pathless, and inspect restored state. | Supported paths, active buffer, cursors, recent files, bookmarks, sidebar, Problems flag, and wrap state restore. No terminal process/output is restored. | |
| H03 | Create an unsaved edit, force an abnormal process end, relaunch, inspect/recover/discard the journal. | Recovery scanning does not block the first frame; the journal is complete, scoped to this workspace, and recovery/discard is explicit. | |
| H04 | Change an open clean file externally, then edit/save from `wscrpt`; test Save As to an existing target. | External-change and create-new protections refuse unsafe overwrites without losing text. | |
| I01 | Quit with clean buffers, with dirty buffers then cancel, and finally after explicit save/discard. | Dirty protection is clear and the chosen outcome is honored. | |
| I02 | After every diagnostics/shell/reconnect/quit path, type in the outer shell and inspect `stty -a`. | Echo, canonical input, cursor, alternate screen, paste mode, and mouse state are restored; no child task or shell remains. | |

## Required verdicts

Record an explicit human `PASS` or `FAIL` for each of these release decisions:

- typing latency;
- Escape/action delivery;
- Unicode editing;
- wrapped navigation;
- Quick Open;
- project search;
- completion;
- task execution and stop;
- full-screen shell handoff and return;
- clipboard attempt;
- reconnect;
- recovery;
- clean exit and terminal cleanup.

A failure or blank verdict blocks release. Attach the diagnostics transcript, before/after hashes, tmux evidence, and any screen recording or notes used for the decision.
