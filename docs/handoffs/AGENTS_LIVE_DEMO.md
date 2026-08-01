# Agents live demo — ACP smoke checklist

**Branch:** `agents/w2-lane` · worktree `../wscrpt-agents`  
**Host snapshot (2026-08-01, Groudon):** `grok 0.2.118` on PATH · `~/.grok/auth.json` **present** · default wscrpt config still **`agent.use_fake = true`**

This is a human smoke checklist for a real `grok agent stdio` run through wscrpt. It does not store secrets and does not require committing config.

---

## 0. Build this branch

```sh
cd "/Users/wcasse/WILL PROJECTS/wscrpt-agents"
cargo build --locked
# optional: install just this binary for the session
# cargo install --path . --locked --force
```

Use `./target/debug/wscrpt` below unless you installed.

---

## 1. Host prep (one-time)

```sh
which grok && grok --version
test -f ~/.grok/auth.json && echo "auth ok" || grok login
# or: export XAI_API_KEY=...
```

Edit **user-global** config only (`~/.config/wscrpt/config.toml` — never the project tree):

```toml
[agent]
use_fake = false
profile = "grok"
argv = ["grok", "agent", "stdio"]
# optional:
# auth_check_argv = ["grok", "--version"]
# required_env = ["XAI_API_KEY"]   # only if you rely solely on env keys
```

Probe (no secrets printed):

```sh
./target/debug/wscrpt --health | grep '^agent\.'
```

Expect something like:

- `agent.mode=process`
- `agent.profile=grok`
- `agent.argv=grok agent stdio`
- `agent.argv0=` path to grok
- `agent.ready=yes`
- `agent.auth_marker=present:...` (or env-based ready)

If `ready=no`, fix auth/PATH first — do not start a run.

---

## 2. Clean workspace for the gate

Dirty-tree rules:

| State | What happens |
| --- | --- |
| Unsaved dirty buffers | **Blocked** — save with `Esc S` first |
| Git has changed paths | Confirm: **Y** start anyway · **Esc** cancel |
| Clean / no repo | Starts immediately |

```sh
cd "/Users/wcasse/WILL PROJECTS/wscrpt-agents"   # or any throwaway git clone
git status -sb
# prefer a clean tree or be ready to press Y
./target/debug/wscrpt .
```

---

## 3. Key map (Agents surface only)

| Key / command | Action |
| --- | --- |
| `Esc w a` / `:agent` | Start goal prompt |
| `Esc w D` / `:agents` | Toggle bottom Agents dashboard |
| `Esc w x` | Cancel run (kills process group) |
| `Y` / `N` | Allow / deny when **Needs You** |
| `Esc w A` | Allow (Needs You) |
| `Esc w G` / `:agent-review` | Review handoff → Git status (+ single-path diff) |
| `Esc v s` / `Esc v D` | Existing Git status / diff picker |

Dashboard is the only activity surface — no Agent Activity popup.

---

## 4. Smoke script (human, ~5–10 min)

### A. Fake path (control — always works)

1. Set `use_fake = true` temporarily (or leave default).
2. `Esc w a` → type `demo goal` → Enter.
3. Expect dashboard auto-open, plan/work/review events, finish in **Review**.
4. Expect auto **Git handoff** if repo present (`Esc w G` re-runs).

### B. Real ACP path

1. Config: `use_fake = false`, argv as above, `--health` ready.
2. Save all buffers; note Git dirt (Y if prompted).
3. `Esc w D` if you want the strip visible before start.
4. `Esc w a` → goal, e.g.:

   ```text
   List files under src/ and summarize agent_acp.rs purpose in one sentence. Do not edit files.
   ```

5. Watch bottom **AGENTS** strip:
   - `ACP starting · grok agent stdio`
   - initialize / session lines
   - plan / tool / coalesced agent text
   - paths if tools touch files (`path_touched`)
6. If **NEED YOU** appears: `Y` allow or `N` deny (or `Esc w A`).
7. On **REVIEW**: Git status should open; if one path was touched, its diff may open too.
8. `Esc w G` to re-handoff; `Esc w x` mid-run to cancel (process dies).

### C. Cancel mid-flight

1. Start a longer goal.
2. As soon as tools run: `Esc w x`.
3. Expect status “Agent cancelled”, no stuck process (`pgrep -fl 'grok agent'` empty after a second).

### D. Dirty-tree

1. Type into a buffer without saving → `Esc w a` → expect **error**, no job.
2. Save; dirty `git status` → expect **confirm** → Esc aborts, Y continues.

---

## 5. Pass / fail

| Check | Pass |
| --- | --- |
| Health `agent.ready=yes` in process mode | ☐ |
| Fake run reaches Review + handoff | ☐ |
| Real run shows ACP lines on dashboard | ☐ |
| Needs You Y/N answers and continues | ☐ |
| Review opens Git status (in a repo) | ☐ |
| Cancel leaves no orphan `grok agent` | ☐ |
| Dirty buffer hard-block | ☐ |
| Dirty Git soft-confirm | ☐ |

---

## 6. Known limits (do not file as regressions)

- No FS/terminal **delegation** to wscrpt yet (agent uses its own host tools).
- Message chunks are **coalesced**, not a full chat transcript.
- Auto review handoff runs **once** per run; use `Esc w G` again.
- Real agent cost/latency depends on model and network; fake path is free.

---

## 7. After the demo

- Restore `use_fake = true` if you do not want accidental live runs.
- Demo notes can stay in this file; do not commit secrets or API keys.

**Related:** [AGENT_AUTH.md](../AGENT_AUTH.md) · [AGENTS_LANE.md](AGENTS_LANE.md) · [LANES.md](../LANES.md)
