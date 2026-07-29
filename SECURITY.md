# Security policy

## Supported versions

Security fixes target the latest released version on the default branch. Pre-1.0 releases may require upgrading rather than backports.

## What wscrpt trusts (and what it does not)

wscrpt runs on a development host and can edit files, run configured tasks, and launch language servers. Treat an open workspace like any other local dev tool.

| Surface | Policy |
| --- | --- |
| Language servers | Launched only from the user-owned global config (`~/.config/wscrpt/config.toml`). Project files cannot silently enable executables. |
| Tasks | Defined in `.wscrpt/tasks.toml` as argument vectors (no shell string). Every run requires an explicit trust confirmation. |
| Git | In-editor Git is read-only. Mutation happens in the full-screen workspace shell you start yourself. |
| Clipboard | Optional OSC 52 attempts can be disabled (`--no-osc52` or config). |
| Search / index | Filesystem traversal and result sizes are bounded; partial results are labeled. |

Opening an untrusted repository should not auto-start untrusted language servers. It may still contain task definitions—do not approve trust for tasks you have not reviewed.

## Reporting a vulnerability

Please **do not** open a public issue for exploitable vulnerabilities.

**Preferred:** GitHub private security advisories for this repo:

https://github.com/wcasse/wscrpt/security/advisories/new

If you cannot use that form, email the maintainer listed in `Cargo.toml` / git history with a clear subject like `wscrpt security`.

Include:

- wscrpt version (`wscrpt --version`)
- host OS/arch
- whether the route is local, SSH, or mosh/tmux
- minimal reproduction steps
- impact (data loss, code execution, terminal escape, etc.)

You should receive an acknowledgment when the report is reviewed.

## Hardening tips for operators

- Keep language server `argv` minimal and absolute when possible.
- Review `.wscrpt/tasks.toml` before first trust approval in a new clone.
- Prefer full-screen shell (`Esc t t`) for interactive or privileged work.
- Run `wscrpt --health` on the real remote route before relying on clipboard or LSP.
