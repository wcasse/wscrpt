//! Optional black-box smoke test for the recommended tmux persistence layer.
//!
//! Hosts without tmux skip this test unless `WSCRPT_REQUIRE_TMUX=1` is set.
//! Required mode also turns an unusable server into a hard failure, so CI
//! cannot report success after silently skipping its terminal proof.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const READY_TIMEOUT: Duration = Duration::from_secs(8);
const EXIT_TIMEOUT: Duration = Duration::from_secs(8);

struct TmuxServer {
    socket: PathBuf,
}

impl TmuxServer {
    fn command(&self) -> Command {
        let mut command = Command::new("tmux");
        command.arg("-S").arg(&self.socket);
        command
    }

    fn output(&self, arguments: &[&str]) -> Output {
        self.command()
            .args(arguments)
            .output()
            .expect("run isolated tmux command")
    }

    fn send_literal(&self, text: &str) {
        let status = self
            .command()
            .args(["send-keys", "-t", "wscrpt:0.0", "-l", "--", text])
            .status()
            .expect("send literal tmux input");
        assert!(status.success(), "tmux literal send failed: {status}");
    }

    fn send_keys(&self, keys: &[&str]) {
        let status = self
            .command()
            .args(["send-keys", "-t", "wscrpt:0.0"])
            .args(keys)
            .status()
            .expect("send named tmux keys");
        assert!(status.success(), "tmux key send failed: {status}");
    }

    fn has_session(&self) -> bool {
        self.command()
            .args(["has-session", "-t", "wscrpt"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}

impl Drop for TmuxServer {
    fn drop(&mut self) {
        let _ = self
            .command()
            .arg("kill-server")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = fs::remove_file(&self.socket);
    }
}

#[test]
fn unicode_edit_save_and_exit_inside_isolated_tmux() {
    let tmux_required = std::env::var_os("WSCRPT_REQUIRE_TMUX").is_some_and(|value| value == "1");
    let tmux_available = Command::new("tmux")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !tmux_available {
        assert!(!tmux_required, "tmux is required but unavailable");
        eprintln!("skipping tmux smoke test: tmux is unavailable");
        return;
    }

    let directory = tempfile::tempdir().expect("temp workspace");
    let file = directory.path().join("tmux-note.txt");
    fs::write(&file, []).expect("create empty document");
    let isolated_home = directory.path().join("home");
    let config_home = directory.path().join("config");
    let state_home = directory.path().join("state");
    fs::create_dir_all(&isolated_home).unwrap();
    fs::create_dir_all(&config_home).unwrap();
    fs::create_dir_all(&state_home).unwrap();

    let socket = PathBuf::from("/tmp").join(format!(
        "wscrpt-tmux-test-{}-{}.sock",
        std::process::id(),
        unique_suffix()
    ));
    let server = TmuxServer { socket };
    let launch = server
        .command()
        .args([
            "new-session",
            "-d",
            "-x",
            "84",
            "-y",
            "24",
            "-s",
            "wscrpt",
            "-c",
        ])
        .arg(directory.path())
        .arg(editor_binary())
        .arg("--project")
        .arg(directory.path())
        .args(["--no-mouse", "--no-osc52", "--no-session"])
        .arg(&file)
        .env("HOME", &isolated_home)
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_STATE_HOME", &state_home)
        .env("LANG", "en_US.UTF-8")
        .output()
        .expect("start isolated tmux editor session");
    if !launch.status.success() || !server.has_session() {
        assert!(
            !tmux_required,
            "tmux is required but the isolated server could not start: {}",
            String::from_utf8_lossy(&launch.stderr).trim()
        );
        eprintln!(
            "skipping tmux smoke test: isolated server unavailable: {}",
            String::from_utf8_lossy(&launch.stderr).trim()
        );
        return;
    }

    let ready_deadline = Instant::now() + READY_TIMEOUT;
    loop {
        assert!(
            server.has_session(),
            "editor tmux session exited before ready"
        );
        let capture = server.output(&["capture-pane", "-p", "-t", "wscrpt:0.0"]);
        if capture.status.success() && String::from_utf8_lossy(&capture.stdout).contains("wscrpt") {
            break;
        }
        assert!(
            Instant::now() < ready_deadline,
            "editor did not render inside tmux before deadline: {}",
            String::from_utf8_lossy(&capture.stderr)
        );
        thread::sleep(Duration::from_millis(25));
    }

    server.send_literal("tmux Magic Keyboard café 🪶");
    server.send_keys(&["Enter"]);
    server.send_literal("persistent session");
    server.send_keys(&["C-s", "C-q"]);

    let exit_deadline = Instant::now() + EXIT_TIMEOUT;
    while server.has_session() && Instant::now() < exit_deadline {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(!server.has_session(), "editor did not exit its tmux pane");
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "tmux Magic Keyboard café 🪶\npersistent session"
    );
}

fn editor_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_wscrpt"))
}

fn unique_suffix() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}
