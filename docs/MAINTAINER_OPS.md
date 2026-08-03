# Maintainer ops — public repo + agents

How Will (owner) and coding agents run **wcasse/wscrpt** without thrash, tag lies, or scope creep.

**One-line rule:** Will freezes truth (STATUS, non-goals, tag policy). Agents move code and docs toward the next verify-green, reviewable change. Only Will pushes, tags, publishes Releases/crates.io, and signs the human iPad/Blink gate.

## Related docs

| Doc | Role |
| --- | --- |
| [STATUS.md](STATUS.md) | Live ship truth (install tag, what’s in the cut) |
| [OPEN_SOURCE_CHECKLIST.md](OPEN_SOURCE_CHECKLIST.md) | Launch scoreboard and deferrals |
| [LANES.md](LANES.md) | Dual-lane ownership when two agents work concurrently |
| [handoffs/](handoffs/) | Per-lane live status |
| [RELEASING.md](RELEASING.md) | Automated gate + checkpoint evidence |
| [PUBLISH_RUNBOOK.md](PUBLISH_RUNBOOK.md) | Tag / Release / post-install commands |
| [IPAD_BLINK_QA.md](IPAD_BLINK_QA.md) | Human remote-feel gate |
| [PUBLIC_SOURCE_AUDIT.md](PUBLIC_SOURCE_AUDIT.md) + `scripts/audit-public-source.sh` | No secrets / home paths in public tree or history |
| [../AGENTS.md](../AGENTS.md) | Standing agent instructions (model-agnostic) for any automated assistant |

---

## Split of labor

### Will always owns

1. **Product call** — ship vs defer, scope, feel (Escape, iPad/Blink QA).
2. **Public voice** — README claims, release-note tone, who this is for / not for.
3. **Secrets & identity** — `gh` auth, crates.io login, vulnerability reports, force-push decisions.
4. **Irreversible git** — `git push`, tags, history rewrite, force-with-lease. Agents do **not** push.
5. **Stranger trust** — merge external PRs after review; never auto-merge drive-by code.

### Agents default to

1. **Implement** on a branch (or clean working tree) behind `scripts/verify.sh`.
2. **Keep docs honest** — `CHANGELOG.md` `[Unreleased]`, regenerate `docs/COMMANDS.md` after keymap changes, update STATUS / handoffs when the cut changes.
3. **Triage** — reproduce issues, draft replies, propose minimal fixes, label candidates.
4. **Release prep** — version bump plan, notes draft, dry-run publish, audit scripts.
5. **CI babysit** — read fail logs → minimal fix PR; no product rewrites.

Rule of thumb: **agents draft and do; Will approves anything strangers will run or trust.**

---

## Truth files (start every session here)

| File | What it answers |
| --- | --- |
| `docs/STATUS.md` | What tag do strangers install? What is on the freeze tip? |
| `README.md` non-goals | What must not land without an explicit design decision? |
| `docs/LANES.md` + handoffs | Who owns which modules if two agents run? |
| `CHANGELOG.md` `[Unreleased]` | What user-visible work is not yet tagged? |

Session openers for agents should name the goal and end condition explicitly (see playbooks below). Do not invent a second status channel in chat only.

---

## Lane discipline

- **Two concurrent agents** only when exclusive ownership is clear ([LANES.md](LANES.md)).
- Shared hot files (`src/app.rs`, `src/render.rs`, keymap/command/session) follow the shared-file protocol in LANES — no “while I’m here” refactors.
- If it is not a dual-lane week: **one agent, one concern, one PR (or one reviewable commit series).**

---

## Default work unit

```text
branch (or clean tree) → implement → scripts/verify.sh → commit → Will push/PR → CI green → merge → STATUS if ship truth moved
```

- Agents stop after green verify + short STATUS/handoff note when relevant.
- Agents **never** finish with “ship later” on a dirty tree that claims readiness.
- **Install honesty:** public install pin (README) must match a **pushed, immutable tag**. `main` may lead the tag; README must not pretend strangers get untagged tip unless that is intentional and documented in STATUS.

---

## Release ceremony

Agents prepare; Will executes irreversible steps.

1. Freeze tip (STATUS names commit / intended tag).
2. `scripts/audit-public-source.sh` (and `--history` when publishing or rewriting risk is live).
3. `scripts/verify.sh`.
4. Package honesty: `Cargo.toml` version, `CHANGELOG.md`, README install pin, `docs/releases/vX.Y.Z.md`.
5. Will: push main if needed → annotated tag **once** → push tag. **Pushed tags never move.** Fixes after a public tag ship as a **new patch**.
6. Will: GitHub Release notes from `docs/releases/…`.
7. Optional: `cargo publish --dry-run --locked` then `cargo publish --locked`.
8. Will: reinstall from **git tag**, not `--path`:

   ```sh
   cargo install --git https://github.com/wcasse/wscrpt --tag vX.Y.Z --locked --force
   wscrpt --version && wscrpt --health
   ```

9. Update STATUS with the live install tag.

Human Blink/iPad pass ([IPAD_BLINK_QA.md](IPAD_BLINK_QA.md)) gates “confidently recommend to strangers.” Automation never approves remote typing feel, Escape delivery, reconnect, or terminal cleanup.

Exact ship commands: [PUBLISH_RUNBOOK.md](PUBLISH_RUNBOOK.md). Gate mechanics: [RELEASING.md](RELEASING.md).

---

## Issues and PRs (queue, not chat)

Keep labels few and mean:

| Label | Meaning |
| --- | --- |
| `bug` | Broken for real users or CI |
| `reliability` | Escape, reconnect, remote/iPad route |
| `docs` | Install honesty, host support, command reference |
| `wontfix` / out-of-scope | Protects the 0.2 boundary (see README non-goals) |
| `good first issue` | Only if Will would actually accept a stranger PR |

Security-sensitive paths (tasks, LSP launch, shell handoff, clipboard OSC 52, filesystem traversal): see [SECURITY.md](../SECURITY.md). Agents draft; Will replies on advisories.

---

## Cadence

| Cadence | Check |
| --- | --- |
| Daily (~15 min) | STATUS still true? `[Unreleased]` honest? Issues stale > ~5 days? |
| Per change | Agent implements; Will feels iPad/Blink path only when input/TTY/remote/session behavior changes |
| Before any public tag | Full ceremony above + human Blink pass for “recommend to strangers” |
| After tag | Clean reinstall from tag; version recorded in STATUS |

No need for daily releases. Ship a patch when install honesty or a real bug demands it.

---

## Agent playbooks (copy-paste)

### Implement a feature

```text
Lane: [AGENTS | STICKIES | none]
Read AGENTS.md, docs/CONTRIBUTOR_MAP.md, docs/STATUS.md, and the relevant handoff if any.
Implement only: <X>.
No new dependencies. No drive-by modularization of src/app.rs.
Respect mutation boundaries and README non-goals.
Update CHANGELOG.md under [Unreleased] for user-visible changes.
If keymap/COMMANDS registry changed: regenerate docs/COMMANDS.md.
Run scripts/verify.sh. Stop. Do not push. Summarize what changed and what Will should review.
```

### Triage / support

```text
Read SECURITY.md, README non-goals, and docs/STATUS.md.
Triage open issues/PRs. For each: reproducible?, in-scope?, minimal fix vs wontfix vs needs-info.
Draft maintainer replies in Will’s voice (direct, short, product-honest).
Flag security-sensitive items for Will only — do not publish unvetted detail.
No code unless Will marks it P0 or explicitly asks.
```

### Release prep

```text
Read docs/PUBLISH_RUNBOOK.md, docs/RELEASING.md, docs/STATUS.md, CHANGELOG.md.
Diff since last public tag. Draft docs/releases/vX.Y.Z.md and version-bump checklist.
Run scripts/audit-public-source.sh and scripts/verify.sh.
List exact commands Will must run (push / tag / gh release / cargo publish). Do not tag or push.
```

### CI babysit

```text
Watch CI on PR #<N> (or the current branch).
Fix failures only. No scope expansion, no refactors, no dependency adds.
Re-run the failing gate locally when possible. Stop when green or blocked on Will.
```

---

## Never fully delegate

| Area | Why |
| --- | --- |
| Escape / typing feel / Blink reconnect | Needs real fingers on the remote path |
| Scope creep (“add rename while we’re here”) | Breaks the 0.2 product story |
| Security advisories | Public trust surface |
| Rewriting git history | One wrong force-push scars the public repo |
| Accepting drive-by PRs | Review load + supply chain |
| Marketing claims in README | Will owns brand and honesty |

---

## Traps (first public project, agent-amplified)

1. **`main` ahead of the install tag without STATUS clarity** — strangers run old software; README looks like a lie. Pin install to tag; STATUS names tip vs tag.
2. **Moving tags** — never. New patch always.
3. **Personal email / home paths in history** — run the audit before every publish.
4. **Shipping agent output without PR-level review** — review as if a stranger wrote it.
5. **Two agents, one hot file** — use LANES or serialize.
6. **RC binaries / scratch / logs in the public tree** — gitignore and leave them out of releases.
7. **Over-promising roadmap in public docs** — STATUS is for maintainers; README stays short, boring, true.

---

## Quick commands (maintainer)

```sh
# full gate
scripts/verify.sh

# public tree hygiene
scripts/audit-public-source.sh
scripts/audit-public-source.sh --history

# reinstall public pin (example)
cargo install --git https://github.com/wcasse/wscrpt --tag v0.2.4 --locked --force
wscrpt --version && wscrpt --health
```

Identity for commits that hit public history: GitHub noreply for the owning account (see PUBLISH_RUNBOOK).
