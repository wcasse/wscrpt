//! Agent authentication readiness — host-owned secrets, never vaulted by wscrpt.
//!
//! wscrpt launches (or will launch) a user-configured agent process. Login and
//! API keys belong to that CLI / the operator shell:
//!
//! - Grok Build: `grok login` → `~/.grok/auth.json`, or `XAI_API_KEY`
//! - Other CLIs: their own stores / env vars
//!
//! This module only **probes** readiness for health and status surfaces. It
//! never writes credentials, never reads key material into logs, and never
//! accepts secrets from project files.

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::config::AgentConfig;
use crate::lsp_discover::resolve_executable;

const AUTH_CHECK_TIMEOUT: Duration = Duration::from_secs(3);

/// Snapshot of whether a configured agent looks ready to run on this host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentReadiness {
    pub use_fake: bool,
    pub profile: String,
    pub argv_summary: String,
    pub argv0_resolved: Option<PathBuf>,
    pub missing_env: Vec<String>,
    pub auth_marker: AuthMarkerStatus,
    pub auth_check: AuthCheckStatus,
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthMarkerStatus {
    /// Fake mode — no host agent credentials required.
    NotRequired,
    /// No known marker for this profile.
    Unknown,
    /// A known credential path exists (file present; contents not inspected).
    Present(PathBuf),
    /// A known credential path is missing.
    Missing(PathBuf),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthCheckStatus {
    Skipped,
    Ok,
    Failed(String),
    TimedOut,
    NotRunnable(String),
}

impl AgentReadiness {
    /// True when wscrpt believes the operator can attempt a real agent run.
    pub fn ready_for_real_agent(&self) -> bool {
        if self.use_fake {
            return true;
        }
        if self.argv0_resolved.is_none() || !self.missing_env.is_empty() {
            return false;
        }
        match &self.auth_check {
            AuthCheckStatus::Failed(_)
            | AuthCheckStatus::TimedOut
            | AuthCheckStatus::NotRunnable(_) => return false,
            AuthCheckStatus::Ok | AuthCheckStatus::Skipped => {}
        }
        match &self.auth_marker {
            AuthMarkerStatus::Missing(_) => {
                // Known credential file absent: only accept an explicit probe pass.
                matches!(self.auth_check, AuthCheckStatus::Ok)
            }
            AuthMarkerStatus::Present(_)
            | AuthMarkerStatus::NotRequired
            | AuthMarkerStatus::Unknown => true,
        }
    }

    /// Short multi-line report for `--health` and in-editor status.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push(format!(
            "agent.mode={}",
            if self.use_fake { "fake" } else { "process" }
        ));
        lines.push(format!("agent.profile={}", self.profile));
        lines.push(format!("agent.argv={}", self.argv_summary));
        lines.push(format!(
            "agent.argv0={}",
            self.argv0_resolved
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "missing on PATH".to_owned())
        ));
        if self.missing_env.is_empty() {
            lines.push("agent.required_env=ok".to_owned());
        } else {
            lines.push(format!(
                "agent.required_env=missing:{}",
                self.missing_env.join(",")
            ));
        }
        lines.push(format!(
            "agent.auth_marker={}",
            format_marker(&self.auth_marker)
        ));
        lines.push(format!(
            "agent.auth_check={}",
            format_check(&self.auth_check)
        ));
        for note in &self.notes {
            lines.push(format!("agent.note={note}"));
        }
        lines
    }

    /// One-line human summary for the status bar.
    pub fn summary(&self) -> String {
        if self.use_fake {
            return "agent: fake (no host auth)".to_owned();
        }
        if self.argv0_resolved.is_none() {
            return format!(
                "agent: {} binary missing — install CLI or fix agent.argv",
                self.profile
            );
        }
        if !self.missing_env.is_empty() {
            return format!(
                "agent: set env {} (never store secrets in wscrpt config)",
                self.missing_env.join(", ")
            );
        }
        match &self.auth_marker {
            AuthMarkerStatus::Missing(path) => {
                format!(
                    "agent: sign in to {} (missing {}) — see docs/AGENT_AUTH.md",
                    self.profile,
                    path.display()
                )
            }
            AuthMarkerStatus::Present(_) => {
                format!("agent: {} credentials present on host", self.profile)
            }
            AuthMarkerStatus::Unknown | AuthMarkerStatus::NotRequired => match &self.auth_check {
                AuthCheckStatus::Ok => format!("agent: {} auth_check ok", self.profile),
                AuthCheckStatus::Failed(msg) => format!("agent: auth_check failed ({msg})"),
                AuthCheckStatus::TimedOut => "agent: auth_check timed out".to_owned(),
                AuthCheckStatus::NotRunnable(msg) => format!("agent: auth_check unusable ({msg})"),
                AuthCheckStatus::Skipped => {
                    format!(
                        "agent: {} configured — auth is host CLI's job (docs/AGENT_AUTH.md)",
                        self.profile
                    )
                }
            },
        }
    }
}

fn format_marker(status: &AuthMarkerStatus) -> String {
    match status {
        AuthMarkerStatus::NotRequired => "not_required".to_owned(),
        AuthMarkerStatus::Unknown => "unknown_profile".to_owned(),
        AuthMarkerStatus::Present(path) => format!("present:{}", path.display()),
        AuthMarkerStatus::Missing(path) => format!("missing:{}", path.display()),
    }
}

fn format_check(status: &AuthCheckStatus) -> String {
    match status {
        AuthCheckStatus::Skipped => "skipped".to_owned(),
        AuthCheckStatus::Ok => "ok".to_owned(),
        AuthCheckStatus::Failed(msg) => format!("failed:{msg}"),
        AuthCheckStatus::TimedOut => "timeout".to_owned(),
        AuthCheckStatus::NotRunnable(msg) => format!("unusable:{msg}"),
    }
}

/// Probe agent readiness without launching a coding session.
pub fn probe_agent(config: &AgentConfig) -> AgentReadiness {
    let mut notes = Vec::new();
    if config.use_fake {
        notes.push("set agent.use_fake = false and agent.argv to use a real CLI".to_owned());
        return AgentReadiness {
            use_fake: true,
            profile: config.profile.clone(),
            argv_summary: "(fake)".to_owned(),
            argv0_resolved: None,
            missing_env: Vec::new(),
            auth_marker: AuthMarkerStatus::NotRequired,
            auth_check: AuthCheckStatus::Skipped,
            notes,
        };
    }

    let argv_summary = if config.argv.is_empty() {
        "(empty)".to_owned()
    } else {
        config.argv.join(" ")
    };
    let argv0_resolved = config
        .argv
        .first()
        .and_then(|name| resolve_executable(name));

    let missing_env = config
        .required_env
        .iter()
        .filter(|name| env::var_os(name).is_none())
        .cloned()
        .collect::<Vec<_>>();

    let auth_marker = auth_marker_for_profile(&config.profile);
    let auth_check = if config.auth_check_argv.is_empty() {
        AuthCheckStatus::Skipped
    } else {
        run_auth_check(&config.auth_check_argv)
    };

    match &config.profile.to_ascii_lowercase()[..] {
        "grok" => {
            notes.push("host auth: run `grok login` or export XAI_API_KEY".to_owned());
            notes.push("typical argv: [\"grok\", \"agent\", \"stdio\"]".to_owned());
        }
        "claude" => {
            notes.push(
                "host auth: use Claude Code / Anthropic CLI login or ANTHROPIC_API_KEY".to_owned(),
            );
        }
        "codex" => {
            notes.push("host auth: use Codex CLI login for this host account".to_owned());
        }
        "custom" => {
            notes.push(
                "custom profile: set required_env and/or auth_check_argv yourself".to_owned(),
            );
        }
        _ => {
            notes.push("unknown profile label — auth still belongs to the host CLI".to_owned());
        }
    }
    notes.push("wscrpt never stores API keys in config.toml or the workspace".to_owned());

    AgentReadiness {
        use_fake: false,
        profile: config.profile.clone(),
        argv_summary,
        argv0_resolved,
        missing_env,
        auth_marker,
        auth_check,
        notes,
    }
}

fn auth_marker_for_profile(profile: &str) -> AuthMarkerStatus {
    match profile.to_ascii_lowercase().as_str() {
        "fake" => AuthMarkerStatus::NotRequired,
        "grok" => marker_path(home_path().map(|home| home.join(".grok").join("auth.json"))),
        // Claude Code has varied locations; treat as unknown unless env is set.
        "claude" | "codex" | "custom" => AuthMarkerStatus::Unknown,
        _ => AuthMarkerStatus::Unknown,
    }
}

fn marker_path(path: Option<PathBuf>) -> AuthMarkerStatus {
    let Some(path) = path else {
        return AuthMarkerStatus::Unknown;
    };
    if path.is_file() {
        AuthMarkerStatus::Present(path)
    } else {
        AuthMarkerStatus::Missing(path)
    }
}

fn home_path() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

fn run_auth_check(argv: &[String]) -> AuthCheckStatus {
    let Some(program) = argv.first() else {
        return AuthCheckStatus::Skipped;
    };
    let resolved = match resolve_executable(program) {
        Some(path) => path,
        None => {
            return AuthCheckStatus::NotRunnable(format!("{program} not on PATH"));
        }
    };
    let mut command = Command::new(&resolved);
    command.args(&argv[1..]);
    command.stdin(Stdio::null());
    command.stdout(Stdio::null());
    command.stderr(Stdio::null());
    // Inherit env so API keys / login state are visible to the probe only.
    match command.spawn() {
        Ok(mut child) => {
            let start = std::time::Instant::now();
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        return if status.success() {
                            AuthCheckStatus::Ok
                        } else {
                            AuthCheckStatus::Failed(format!("exit {status}"))
                        };
                    }
                    Ok(None) => {
                        if start.elapsed() > AUTH_CHECK_TIMEOUT {
                            let _ = child.kill();
                            let _ = child.wait();
                            return AuthCheckStatus::TimedOut;
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(error) => {
                        return AuthCheckStatus::NotRunnable(error.to_string());
                    }
                }
            }
        }
        Err(error) => AuthCheckStatus::NotRunnable(error.to_string()),
    }
}

/// Suggested default config block for a known profile (comments only).
pub fn suggested_profile_snippet(profile: &str) -> String {
    match profile {
        "grok" => r#"
# Grok Build on this host (authenticate outside wscrpt):
#   grok login
#   # or: export XAI_API_KEY=...
[agent]
use_fake = false
profile = "grok"
argv = ["grok", "agent", "stdio"]
# optional: required_env = ["XAI_API_KEY"]
# optional: auth_check_argv = ["grok", "--version"]
"#
        .to_owned(),
        "claude" => r#"
[agent]
use_fake = false
profile = "claude"
argv = ["claude"]   # adjust to your ACP-capable CLI
# required_env = ["ANTHROPIC_API_KEY"]
"#
        .to_owned(),
        _ => String::new(),
    }
}

/// Expand `~` in a path for display only.
pub fn display_home_relative(path: &Path) -> String {
    if let Some(home) = home_path()
        && let Ok(stripped) = path.strip_prefix(&home)
    {
        return format!("~/{}", stripped.display());
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_profile_is_always_ready() {
        let config = AgentConfig::default();
        let readiness = probe_agent(&config);
        assert!(readiness.use_fake);
        assert!(readiness.ready_for_real_agent());
        assert!(readiness.summary().contains("fake"));
    }

    #[test]
    fn missing_binary_is_not_ready() {
        let config = AgentConfig {
            use_fake: false,
            profile: "custom".to_owned(),
            argv: vec!["wscrpt-definitely-not-a-real-agent-bin-xyz".to_owned()],
            auth_check_argv: Vec::new(),
            required_env: Vec::new(),
        };
        let readiness = probe_agent(&config);
        assert!(readiness.argv0_resolved.is_none());
        assert!(!readiness.ready_for_real_agent());
    }

    #[test]
    fn required_env_missing_is_reported() {
        let name = "WSCRPT_TEST_AGENT_ENV_THAT_SHOULD_NOT_EXIST";
        // SAFETY: test-only env name unique to this suite.
        unsafe {
            env::remove_var(name);
        }
        let config = AgentConfig {
            use_fake: false,
            profile: "custom".to_owned(),
            argv: vec!["true".to_owned()], // usually on PATH
            auth_check_argv: Vec::new(),
            required_env: vec![name.to_owned()],
        };
        let readiness = probe_agent(&config);
        assert_eq!(readiness.missing_env, vec![name.to_owned()]);
        assert!(!readiness.ready_for_real_agent());
    }
}
