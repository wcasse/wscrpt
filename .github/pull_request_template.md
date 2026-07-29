## What and why

<!-- One or two sentences. Link the issue if there is one. -->

## Checklist

- [ ] `scripts/verify.sh` passes locally (fmt, clippy `-D warnings`, tests, package, isolated install)
- [ ] `CHANGELOG.md` `[Unreleased]` mentions any user-visible or boundary change
- [ ] `docs/COMMANDS.md` regenerated if `src/keymap.rs::COMMANDS` changed
- [ ] No new dependencies without prior discussion
- [ ] No removed 0.2 surface reintroduced (embedded terminals, multi-file replace, LSP rename/code actions, in-editor Git mutation) without a design discussion

## Remote route notes

<!-- If this touches input, rendering, clipboard, terminal modes, or reconnect
     behavior: how was it exercised (local PTY tests, tmux smoke, real
     iPad/Blink/mosh route)? "Not applicable" is fine for pure internals. -->
