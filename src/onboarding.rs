//! Lightweight first-run markers under the XDG state directory.
//!
//! These flags never launch tools and never write into the workspace. They only
//! remember whether the operator has already seen the built-in first-hour help.

use std::env;
use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const APPLICATION_DIRECTORY: &str = "wscrpt";
const ONBOARDING_FILENAME: &str = "onboarding.toml";

#[derive(Clone, Debug, Default, Deserialize, Serialize, Eq, PartialEq)]
#[serde(default)]
struct OnboardingFile {
    /// Operator has closed (or skipped) the first-run help overlay.
    first_run_help_seen: bool,
}

/// True when the first-run help should open automatically.
pub fn should_show_first_run_help() -> bool {
    // Integration harnesses and automation set this so Esc sequences reach the
    // buffer instead of dismissing the first-run overlay.
    if env::var_os("WSCRPT_SKIP_FIRST_RUN_HELP").is_some() {
        return false;
    }
    match load() {
        Ok(state) => !state.first_run_help_seen,
        // Missing state home or unreadable file: fail open so a broken XDG
        // layout cannot permanently hide the guide.
        Err(_) => true,
    }
}

/// Persist that first-run help was shown/dismissed.
pub fn mark_first_run_help_seen() {
    let mut state = load().unwrap_or_default();
    if state.first_run_help_seen {
        return;
    }
    state.first_run_help_seen = true;
    let _ = save(&state);
}

fn load() -> Result<OnboardingFile, String> {
    let path = onboarding_path()?;
    if !path.exists() {
        return Ok(OnboardingFile::default());
    }
    let source = fs::read_to_string(&path)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    toml::from_str(&source).map_err(|error| format!("invalid {}: {error}", path.display()))
}

fn save(state: &OnboardingFile) -> Result<(), String> {
    let path = onboarding_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    let encoded = toml::to_string_pretty(state)
        .map_err(|error| format!("could not encode onboarding state: {error}"))?;
    let temporary = path.with_extension("toml.tmp");
    fs::write(&temporary, encoded)
        .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
    fs::rename(&temporary, &path)
        .map_err(|error| format!("could not publish {}: {error}", path.display()))?;
    Ok(())
}

fn onboarding_path() -> Result<PathBuf, String> {
    if let Some(root) = env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        let root = PathBuf::from(root);
        if root.is_absolute() {
            return Ok(root.join(APPLICATION_DIRECTORY).join(ONBOARDING_FILENAME));
        }
    }
    let home = env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "HOME and XDG_STATE_HOME are unset".to_owned())?;
    Ok(home
        .join(".local/state")
        .join(APPLICATION_DIRECTORY)
        .join(ONBOARDING_FILENAME))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn missing_state_file_requests_first_run_help() {
        let _guard = ENV_LOCK.lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        // SAFETY: serialized by ENV_LOCK for this test module.
        unsafe {
            env::set_var("XDG_STATE_HOME", directory.path());
            env::remove_var("HOME");
        }
        assert!(should_show_first_run_help());
        mark_first_run_help_seen();
        assert!(!should_show_first_run_help());
        assert!(directory.path().join("wscrpt/onboarding.toml").is_file());
    }
}
