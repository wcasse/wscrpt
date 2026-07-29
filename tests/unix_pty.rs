//! Black-box terminal contracts for the `wscrpt` executable.
//!
//! These tests intentionally use a kernel pseudo-terminal rather than pipes:
//! the production binary rejects non-interactive stdio, and the distinction
//! between carriage return and newline matters for Blink/Magic Keyboard input.

#![cfg(any(
    target_os = "macos",
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]

use std::ffi::{c_int, c_ulong, c_void};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const ENTER_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049h";
const ENABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004h";
const DISABLE_BRACKETED_PASTE: &[u8] = b"\x1b[?2004l";
const LEAVE_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049l";
const SHOW_CURSOR: &[u8] = b"\x1b[?25h";

const READY_TIMEOUT: Duration = Duration::from_secs(5);
const EXIT_TIMEOUT: Duration = Duration::from_secs(8);
const READER_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const POLL_QUANTUM: Duration = Duration::from_millis(20);

#[repr(C)]
struct WindowSize {
    rows: u16,
    columns: u16,
    x_pixels: u16,
    y_pixels: u16,
}

// BSDs, macOS, and glibc expose openpty through libutil. Musl provides it
// directly from libc and commonly has no separate libutil to link.
#[cfg_attr(not(target_env = "musl"), link(name = "util"))]
unsafe extern "C" {
    fn openpty(
        master: *mut c_int,
        slave: *mut c_int,
        name: *mut u8,
        termios: *const c_void,
        window_size: *const WindowSize,
    ) -> c_int;
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
    fn kill(pid: c_int, signal: c_int) -> c_int;
}

#[cfg(target_os = "linux")]
const TIOCSWINSZ: c_ulong = 0x5414;
#[cfg(not(target_os = "linux"))]
const TIOCSWINSZ: c_ulong = 0x8008_7467;
const SIGWINCH: c_int = 28;

enum ReaderMessage {
    Bytes(Vec<u8>),
    Failed(String),
}

#[derive(Debug)]
enum LaunchError {
    PtyUnavailable(io::Error),
    Io(io::Error),
}

impl From<io::Error> for LaunchError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

struct PtyProcess {
    child: Child,
    input: File,
    output: Vec<u8>,
    messages: Receiver<ReaderMessage>,
    reader: Option<JoinHandle<()>>,
    reader_closed: bool,
}

impl PtyProcess {
    fn launch(workspace: &Path, file: &Path, columns: u16, rows: u16) -> Result<Self, LaunchError> {
        Self::launch_with_args(
            workspace,
            columns,
            rows,
            [
                "--project",
                workspace.to_str().expect("test workspace path is UTF-8"),
                "--no-mouse",
                "--no-osc52",
                "--no-session",
                file.to_str().expect("test file path is UTF-8"),
            ],
        )
    }

    fn launch_input_diagnostics(
        workspace: &Path,
        columns: u16,
        rows: u16,
    ) -> Result<Self, LaunchError> {
        Self::launch_with_args(workspace, columns, rows, ["--input-diagnostics"])
    }

    fn launch_workspace(workspace: &Path, columns: u16, rows: u16) -> Result<Self, LaunchError> {
        Self::launch_with_args(
            workspace,
            columns,
            rows,
            ["--no-mouse", "--no-osc52", "--no-session", "."],
        )
    }

    fn launch_with_args<const N: usize>(
        workspace: &Path,
        columns: u16,
        rows: u16,
        args: [&str; N],
    ) -> Result<Self, LaunchError> {
        let (master, slave) = open_pty(columns, rows).map_err(LaunchError::PtyUnavailable)?;
        let input = master.try_clone()?;
        let child_stdin = slave.try_clone()?;
        let child_stdout = slave.try_clone()?;

        let isolated_home = workspace.join("test-home");
        let config_home = workspace.join("test-config");
        let state_home = workspace.join("test-state");
        fs::create_dir_all(&isolated_home)?;
        fs::create_dir_all(&config_home)?;
        fs::create_dir_all(&state_home)?;

        let mut command = Command::new(editor_binary());
        command
            .args(args)
            .current_dir(workspace)
            .env_clear()
            .env("TERM", "xterm-256color")
            .env("LANG", "en_US.UTF-8")
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &isolated_home)
            .env("XDG_CONFIG_HOME", &config_home)
            .env("XDG_STATE_HOME", &state_home)
            .stdin(Stdio::from(child_stdin))
            .stdout(Stdio::from(child_stdout))
            .stderr(Stdio::from(slave));
        let child = command.spawn()?;

        let (sender, messages) = mpsc::channel();
        let reader = thread::spawn(move || read_master(master, sender));
        Ok(Self {
            child,
            input,
            output: Vec::new(),
            messages,
            reader: Some(reader),
            reader_closed: false,
        })
    }

    fn wait_until_ready(&mut self, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if contains_bytes(&self.output, ENTER_ALTERNATE_SCREEN)
                && contains_bytes(&self.output, ENABLE_BRACKETED_PASTE)
            {
                return Ok(());
            }
            if let Some(status) = self.child.try_wait()? {
                self.drain_reader(READER_DRAIN_TIMEOUT);
                return Err(io::Error::other(format!(
                    "editor exited before its PTY was ready ({status}); output: {}",
                    escaped_output(&self.output)
                )));
            }
            self.receive_until(deadline)?;
        }
    }

    fn write_input(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.input.write_all(bytes)?;
        self.input.flush()
    }

    fn resize(&mut self, columns: u16, rows: u16) -> io::Result<()> {
        let size = WindowSize {
            rows,
            columns,
            x_pixels: 0,
            y_pixels: 0,
        };
        // SAFETY: `input` owns a live PTY master and `size` is a valid
        // platform winsize layout for the duration of the ioctl call.
        let result = unsafe { ioctl(self.input.as_raw_fd(), TIOCSWINSZ, &size) };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
        let pid = c_int::try_from(self.child.id())
            .map_err(|_| io::Error::other("child PID does not fit c_int"))?;
        // Some PTY implementations notify only the foreground process group;
        // the test child has no separate session leader. Signal the editor
        // explicitly after updating the kernel winsize.
        // SAFETY: `pid` is the live child owned by this harness.
        if unsafe { kill(pid, SIGWINCH) } == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn wait_for_output_after(
        &mut self,
        needle: &[u8],
        start: usize,
        timeout: Duration,
    ) -> io::Result<usize> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(position) = find_bytes(&self.output, needle, start) {
                return Ok(position);
            }
            if let Some(status) = self.child.try_wait()? {
                self.drain_reader(READER_DRAIN_TIMEOUT);
                return Err(io::Error::other(format!(
                    "editor exited while waiting for PTY output ({status}); output: {}",
                    escaped_output(&self.output)
                )));
            }
            self.receive_until(deadline)?;
        }
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> io::Result<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait()? {
                self.drain_reader(READER_DRAIN_TIMEOUT);
                return Ok(status);
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                self.drain_reader(READER_DRAIN_TIMEOUT);
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "editor did not exit within {timeout:?}; output: {}",
                        escaped_output(&self.output)
                    ),
                ));
            }
            self.receive_until(deadline)?;
        }
    }

    fn receive_until(&mut self, deadline: Instant) -> io::Result<()> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out waiting for PTY output",
            ));
        }
        if self.reader_closed {
            // PTY EOF can become observable a few scheduler ticks before
            // waitpid reports the just-exited child. Keep polling the process
            // against the same hard deadline instead of treating that normal
            // ordering race as an editor failure.
            thread::park_timeout(remaining.min(POLL_QUANTUM));
            return Ok(());
        }
        match self.messages.recv_timeout(remaining.min(POLL_QUANTUM)) {
            Ok(ReaderMessage::Bytes(bytes)) => self.output.extend_from_slice(&bytes),
            Ok(ReaderMessage::Failed(message)) => {
                return Err(io::Error::other(format!(
                    "could not read editor PTY: {message}"
                )));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                self.reader_closed = true;
            }
        }
        Ok(())
    }

    fn drain_reader(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while !self.reader_closed {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self.messages.recv_timeout(remaining.min(POLL_QUANTUM)) {
                Ok(ReaderMessage::Bytes(bytes)) => self.output.extend_from_slice(&bytes),
                Ok(ReaderMessage::Failed(message)) => {
                    self.output.extend_from_slice(
                        format!("\n[PTY reader failed after exit: {message}]").as_bytes(),
                    );
                    break;
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    self.reader_closed = true;
                    break;
                }
            }
        }
        if self
            .reader
            .as_ref()
            .is_some_and(|reader| reader.is_finished())
        {
            let reader = self.reader.take().expect("reader was present");
            let _ = reader.join();
        }
    }
}

impl Drop for PtyProcess {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        self.drain_reader(READER_DRAIN_TIMEOUT);
    }
}

#[test]
fn magic_keyboard_unicode_enter_save_quit_and_terminal_cleanup() {
    let directory = tempfile::tempdir().expect("temp workspace");
    let file = directory.path().join("ipad-note.txt");
    fs::write(&file, []).expect("create empty document");

    let Some(mut editor) = launch_or_skip(directory.path(), &file, 84, 24) else {
        return;
    };
    editor
        .wait_until_ready(READY_TIMEOUT)
        .expect("editor should enter raw alternate-screen mode");

    // This is typed input, not a bracketed paste. U+000D models the physical
    // Return key emitted by a terminal keyboard; the ZWJ must survive as an
    // ordinary Unicode scalar between the two emoji code points.
    let first_line = "Magic Keyboard 👩\u{200d}💻";
    let second_line = "mosh-ready café 🐦";
    let pasted_line = "native bracketed paste 🪶";
    let mut input = first_line.as_bytes().to_vec();
    input.push(b'\r');
    input.extend_from_slice(second_line.as_bytes());
    input.push(b'\r');
    input.extend_from_slice(b"\x1b[200~");
    input.extend_from_slice(pasted_line.as_bytes());
    input.extend_from_slice(b"\x1b[201~");
    input.push(0x13); // Ctrl-S: save
    input.push(0x11); // Ctrl-Q: quit
    editor.write_input(&input).expect("write PTY input");

    let status = editor
        .wait_for_exit(EXIT_TIMEOUT)
        .expect("editor should save and quit before the deadline");
    assert!(
        status.success(),
        "editor exited with {status}; output: {}",
        escaped_output(&editor.output)
    );
    let expected = format!("{first_line}\n{second_line}\n{pasted_line}");
    assert_eq!(
        fs::read(&file).expect("read saved document"),
        expected.as_bytes(),
        "typed Unicode, physical Enter, or save bytes changed"
    );
    assert_normal_terminal_cleanup(&editor.output);
}

#[test]
fn tiny_real_pty_still_quits_and_restores_terminal_modes() {
    let directory = tempfile::tempdir().expect("temp workspace");
    let file = directory.path().join("tiny.txt");
    fs::write(&file, b"unchanged").expect("create document");

    let Some(mut editor) = launch_or_skip(directory.path(), &file, 12, 3) else {
        return;
    };
    editor
        .wait_until_ready(READY_TIMEOUT)
        .expect("editor should initialize in a tiny PTY");
    editor.write_input(&[0x11]).expect("send Ctrl-Q");
    let status = editor
        .wait_for_exit(EXIT_TIMEOUT)
        .expect("editor should quit from a tiny PTY");

    assert!(
        status.success(),
        "tiny-terminal editor exited with {status}; output: {}",
        escaped_output(&editor.output)
    );
    assert_eq!(fs::read(&file).unwrap(), b"unchanged");
    assert!(
        contains_bytes(&editor.output, b" too small "),
        "tiny-layout frame was not rendered: {}",
        escaped_output(&editor.output)
    );
    assert_normal_terminal_cleanup(&editor.output);
}

#[test]
fn workspace_directory_launch_renders_and_quits_on_a_real_pty() {
    let directory = tempfile::tempdir().expect("temp workspace");
    fs::write(directory.path().join("README.md"), b"workspace\n").unwrap();
    let Some(mut editor) = (match PtyProcess::launch_workspace(directory.path(), 84, 24) {
        Ok(process) => Some(process),
        Err(LaunchError::PtyUnavailable(error)) => {
            eprintln!("skipping PTY integration test: openpty is unavailable: {error}");
            None
        }
        Err(LaunchError::Io(error)) => panic!("could not launch workspace in PTY: {error}"),
    }) else {
        return;
    };

    editor.wait_until_ready(READY_TIMEOUT).unwrap();
    editor
        .wait_for_output_after(b"wscrpt", 0, READY_TIMEOUT)
        .expect("workspace launch should render its first frame");
    editor.write_input(&[0x11]).expect("send Ctrl-Q");
    let status = editor.wait_for_exit(EXIT_TIMEOUT).unwrap();
    assert!(status.success(), "workspace launch exited with {status}");
    assert_normal_terminal_cleanup(&editor.output);
}

#[test]
fn input_diagnostics_reports_keys_paste_resize_and_restores_modes() {
    let directory = tempfile::tempdir().expect("temp workspace");
    let Some(mut editor) = (match PtyProcess::launch_input_diagnostics(directory.path(), 84, 24) {
        Ok(process) => Some(process),
        Err(LaunchError::PtyUnavailable(error)) => {
            eprintln!("skipping PTY integration test: openpty is unavailable: {error}");
            None
        }
        Err(LaunchError::Io(error)) => panic!("could not launch input diagnostics in PTY: {error}"),
    }) else {
        return;
    };

    let header = editor
        .wait_for_output_after(b"wscrpt input diagnostics", 0, READY_TIMEOUT)
        .expect("diagnostic header should render");
    editor
        .wait_for_output_after(b"event_limit=512", header, READY_TIMEOUT)
        .expect("diagnostic instructions should render");

    editor
        .write_input(b"a\x0b")
        .expect("send printable key and Ctrl-K");
    let printable = editor
        .wait_for_output_after(b"key code=Char(a) modifiers=none", header, READY_TIMEOUT)
        .expect("printable key should be decoded");
    editor
        .wait_for_output_after(
            b"key code=Char(k) modifiers=CONTROL",
            printable,
            READY_TIMEOUT,
        )
        .expect("Ctrl-K should be decoded");

    editor
        .write_input(b"\x1b[200~hello\nworld\x1b[201~")
        .expect("send bracketed paste");
    editor
        .wait_for_output_after(
            b"paste bytes=11 chars=11 text=hello\\nworld",
            printable,
            READY_TIMEOUT,
        )
        .expect("bracketed paste should be decoded as one event");

    editor.resize(100, 30).expect("resize diagnostic PTY");
    editor
        .wait_for_output_after(b"resize cols=100 rows=30", printable, READY_TIMEOUT)
        .expect("resize should be reported");

    editor.write_input(&[0x07]).expect("send Ctrl-G");
    let status = editor
        .wait_for_exit(EXIT_TIMEOUT)
        .expect("input diagnostics should exit on Ctrl-G");
    assert!(
        status.success(),
        "input diagnostics exited with {status}; output: {}",
        escaped_output(&editor.output)
    );
    assert!(
        contains_bytes(&editor.output, ENABLE_BRACKETED_PASTE),
        "input diagnostics should enable bracketed paste: {}",
        escaped_output(&editor.output)
    );
    assert!(
        contains_bytes(&editor.output, DISABLE_BRACKETED_PASTE),
        "input diagnostics should disable bracketed paste: {}",
        escaped_output(&editor.output)
    );
    assert!(
        !contains_bytes(&editor.output, ENTER_ALTERNATE_SCREEN),
        "input diagnostics should not use alternate screen: {}",
        escaped_output(&editor.output)
    );
}

#[test]
fn narrow_pty_soft_wrap_moves_and_edits_a_visual_continuation() {
    let directory = tempfile::tempdir().expect("temp workspace");
    let file = directory.path().join("wrapped.txt");
    let original = "0123456789012345678901234567890123456789TAIL";
    fs::write(&file, original).expect("create wrapped document");

    let Some(mut editor) = launch_or_skip(directory.path(), &file, 40, 12) else {
        return;
    };
    editor
        .wait_until_ready(READY_TIMEOUT)
        .expect("editor should initialize in a narrow PTY");
    let before_toggle = editor.output.len();
    editor.write_input(b"\x1bz").expect("toggle soft wrap");
    editor
        .wait_for_output_after(b"WRAP", before_toggle, READY_TIMEOUT)
        .expect("wrapped status should be rendered");

    // At 40 columns the three-cell line-number gutter leaves 37 editor cells.
    // One visual Down must therefore land at document character 37, still on
    // the same logical line. Inserting there distinguishes real wrap-aware
    // movement from the old logical-line-only behavior.
    editor
        .write_input(b"\x1b[BX\x13\x11")
        .expect("move on the wrapped row, edit, save, and quit");
    let status = editor
        .wait_for_exit(EXIT_TIMEOUT)
        .expect("wrapped editor should save and quit");
    assert!(
        status.success(),
        "wrapped editor exited with {status}; output: {}",
        escaped_output(&editor.output)
    );

    let mut expected = original.to_owned();
    expected.insert(37, 'X');
    assert_eq!(fs::read_to_string(&file).unwrap(), expected);
    assert_normal_terminal_cleanup(&editor.output);
}

#[test]
fn pty_resize_reflows_wide_unicode_before_wrapped_movement() {
    let directory = tempfile::tempdir().expect("temp workspace");
    let file = directory.path().join("resize-wrap.txt");
    let original = format!("{}tail", "界".repeat(30));
    fs::write(&file, &original).expect("create wide document");

    let Some(mut editor) = launch_or_skip(directory.path(), &file, 40, 12) else {
        return;
    };
    editor.wait_until_ready(READY_TIMEOUT).unwrap();
    editor.write_input(b"\x1bz").unwrap();
    let toggled = editor
        .wait_for_output_after(b"WRAP", 0, READY_TIMEOUT)
        .expect("wrap indicator");

    editor.resize(50, 12).expect("widen PTY");
    let widened = editor
        .wait_for_output_after(b"WRAP", toggled + 1, READY_TIMEOUT)
        .expect("widened frame should redraw");
    editor.resize(40, 12).expect("narrow PTY again");
    editor
        .wait_for_output_after(b"WRAP", widened + 1, READY_TIMEOUT)
        .expect("narrowed frame should redraw");

    // Forty columns leave 37 content cells. Eighteen two-cell glyphs fit on
    // the first row, so visual Down at x=0 must land before glyph 19.
    editor.write_input(b"\x1b[BX\x13\x11").unwrap();
    let status = editor.wait_for_exit(EXIT_TIMEOUT).unwrap();
    assert!(status.success(), "resize-wrap editor exited with {status}");
    let mut expected = original;
    let insertion_byte = expected
        .char_indices()
        .nth(18)
        .map_or(expected.len(), |(byte, _)| byte);
    expected.insert(insertion_byte, 'X');
    assert_eq!(fs::read_to_string(&file).unwrap(), expected);
    assert_normal_terminal_cleanup(&editor.output);
}

#[test]
fn workspace_tree_action_renders_and_filters_inside_a_real_pty() {
    let directory = tempfile::tempdir().expect("temp workspace");
    let source = directory.path().join("src");
    fs::create_dir(&source).expect("create source directory");
    let active = source.join("main.rs");
    let other = source.join("other.rs");
    fs::write(&active, b"fn main() {}\n").expect("create active document");
    fs::write(&other, b"pub fn other() {}\n").expect("create second document");

    let Some(mut editor) = launch_or_skip(directory.path(), &active, 84, 24) else {
        return;
    };
    editor.wait_until_ready(READY_TIMEOUT).unwrap();
    editor
        .wait_for_output_after(b"Workspace indexed:", 0, READY_TIMEOUT)
        .expect("project snapshot should become ready before tree interaction");
    let before_tree = editor.output.len();
    editor
        .write_input(b"\x1bwt")
        .expect("open workspace tree through action namespace");
    let tree_frame = editor
        .wait_for_output_after(b"WORKSPACE", before_tree, READY_TIMEOUT)
        .expect("workspace overlay should render");
    editor
        .wait_for_output_after(b"main.rs", tree_frame, READY_TIMEOUT)
        .expect("active workspace file should render");
    editor
        .wait_for_output_after(b"filter files", tree_frame, READY_TIMEOUT)
        .expect("workspace navigation notice should finish the initial frame");

    let before_filter = editor.output.len();
    editor.write_input(b"other").expect("filter tree files");
    let filtered = editor
        .wait_for_output_after("⌕ src/other.rs".as_bytes(), before_filter, READY_TIMEOUT)
        .expect("filtered workspace file should render");
    editor
        .write_input(b"\r")
        .expect("open the filtered workspace file");
    editor
        .wait_for_output_after(b"other.rs", filtered + 1, READY_TIMEOUT)
        .expect("opened file should become visible in the editor frame");

    let refreshed = source.join("refreshed.rs");
    fs::write(&refreshed, b"REFRESHED_FILE_MARKER\n")
        .expect("create file after the initial workspace snapshot");
    let before_refresh = editor.output.len();
    editor
        .write_input(b"\x1bwR")
        .expect("refresh workspace snapshots through the action namespace");
    editor
        .wait_for_output_after(
            b"Workspace snapshot refreshed:",
            before_refresh,
            READY_TIMEOUT,
        )
        .expect("workspace refresh status should render");

    let before_refreshed_tree = editor.output.len();
    editor
        .write_input(b"\x1bwt")
        .expect("reopen the refreshed workspace tree");
    editor
        .wait_for_output_after(b"WORKSPACE", before_refreshed_tree, READY_TIMEOUT)
        .expect("refreshed workspace overlay should render");
    let before_refreshed_filter = editor.output.len();
    editor
        .write_input(b"refreshed")
        .expect("filter for the externally created file");
    let refreshed_filter = editor
        .wait_for_output_after(
            "⌕ src/refreshed.rs".as_bytes(),
            before_refreshed_filter,
            READY_TIMEOUT,
        )
        .expect("refreshed workspace file should render");
    editor
        .write_input(b"\r")
        .expect("open the externally created workspace file");
    editor
        .wait_for_output_after(b"3:refreshed.rs", refreshed_filter + 1, READY_TIMEOUT)
        .expect("refreshed file should become the active editor buffer");
    editor.write_input(&[0x11]).expect("send Ctrl-Q");
    let status = editor.wait_for_exit(EXIT_TIMEOUT).unwrap();

    assert!(status.success(), "workspace-tree PTY exited with {status}");
    assert_eq!(fs::read(&active).unwrap(), b"fn main() {}\n");
    assert_eq!(fs::read(&other).unwrap(), b"pub fn other() {}\n");
    assert_eq!(fs::read(&refreshed).unwrap(), b"REFRESHED_FILE_MARKER\n");
    assert_normal_terminal_cleanup(&editor.output);
}

#[test]
fn workspace_shell_handoff_restores_tty_and_returns_to_the_editor() {
    let directory = tempfile::tempdir().expect("temp workspace");
    let file = directory.path().join("shell.txt");
    fs::write(&file, b"unchanged").expect("create document");

    let Some(mut editor) = launch_or_skip(directory.path(), &file, 84, 24) else {
        return;
    };
    editor
        .wait_until_ready(READY_TIMEOUT)
        .expect("editor should enter raw alternate-screen mode");
    let initial_enter = find_bytes(&editor.output, ENTER_ALTERNATE_SCREEN, 0).unwrap();

    // Esc t t is the portable action-layer route. Wait for the editor to
    // restore its terminal modes before feeding commands to the real shell.
    editor
        .write_input(b"\x1btt")
        .expect("request workspace shell");
    let first_leave = editor
        .wait_for_output_after(LEAVE_ALTERNATE_SCREEN, initial_enter, READY_TIMEOUT)
        .expect("editor should release the alternate screen for the shell");
    editor
        .write_input(
            b"printf '%s\\n' \"$WSCRPT_WORKSPACE\" > shell-workspace.txt; pwd > shell-pwd.txt; exit\r",
        )
        .expect("run shell probes and exit");

    let second_enter = editor
        .wait_for_output_after(ENTER_ALTERNATE_SCREEN, first_leave + 1, READY_TIMEOUT)
        .expect("editor should re-enter its terminal UI after shell exit");
    editor.write_input(&[0x11]).expect("send Ctrl-Q");
    let status = editor
        .wait_for_exit(EXIT_TIMEOUT)
        .expect("editor should quit after returning from the shell");

    assert!(
        status.success(),
        "editor exited with {status}; output: {}",
        escaped_output(&editor.output)
    );
    let expected_root = directory.path().canonicalize().unwrap();
    let workspace_probe = fs::read_to_string(directory.path().join("shell-workspace.txt"))
        .expect("shell should receive WSCRPT_WORKSPACE");
    let pwd_probe = fs::read_to_string(directory.path().join("shell-pwd.txt"))
        .expect("shell should start in workspace");
    assert_eq!(
        Path::new(workspace_probe.trim()).canonicalize().unwrap(),
        expected_root
    );
    assert_eq!(
        Path::new(pwd_probe.trim()).canonicalize().unwrap(),
        expected_root
    );

    let final_disable = find_bytes(&editor.output, DISABLE_BRACKETED_PASTE, second_enter)
        .expect("final editor cleanup should disable bracketed paste");
    let final_leave = find_bytes(&editor.output, LEAVE_ALTERNATE_SCREEN, final_disable)
        .expect("final editor cleanup should leave alternate screen");
    assert!(
        find_bytes(&editor.output, SHOW_CURSOR, final_leave).is_some(),
        "cursor should be visible after the final editor exit: {}",
        escaped_output(&editor.output)
    );
    assert_eq!(fs::read(&file).unwrap(), b"unchanged");
}

#[test]
fn trusted_task_output_streams_through_the_real_editor_pty() {
    let directory = tempfile::tempdir().expect("temp workspace");
    let file = directory.path().join("task.txt");
    fs::write(&file, b"unchanged").expect("create document");
    fs::create_dir(directory.path().join(".wscrpt")).expect("create task directory");
    fs::write(
        directory.path().join(".wscrpt/tasks.toml"),
        "version = 1\n[tasks.check]\nargv = [\"printf\", \"TASK_OUTPUT_café_🪶\\n\"]\n",
    )
    .expect("write task configuration");

    let Some(mut editor) = launch_or_skip(directory.path(), &file, 84, 24) else {
        return;
    };
    editor.wait_until_ready(READY_TIMEOUT).unwrap();
    let before_trust = editor.output.len();
    editor
        .write_input(b"\x1btd")
        .expect("request the conventional default task");
    let trust = editor
        .wait_for_output_after(b"TRUST", before_trust, READY_TIMEOUT)
        .expect("task trust gate should render");
    editor.write_input(b"Y").expect("trust this run once");
    editor
        .write_input(b"\x1bto")
        .expect("open bounded task output");
    let output = editor
        .wait_for_output_after("TASK_OUTPUT_café_🪶".as_bytes(), trust, READY_TIMEOUT)
        .expect("Unicode task output should stream into its read-only view");
    editor
        .wait_for_output_after(b"task exited: 0", output, READY_TIMEOUT)
        .expect("task exit should be reported");
    editor.write_input(&[0x11]).expect("send Ctrl-Q");
    let status = editor.wait_for_exit(EXIT_TIMEOUT).unwrap();

    assert!(status.success(), "task-output PTY exited with {status}");
    assert_eq!(fs::read(&file).unwrap(), b"unchanged");
    assert_normal_terminal_cleanup(&editor.output);
}

fn launch_or_skip(workspace: &Path, file: &Path, columns: u16, rows: u16) -> Option<PtyProcess> {
    match PtyProcess::launch(workspace, file, columns, rows) {
        Ok(process) => Some(process),
        Err(LaunchError::PtyUnavailable(error)) => {
            eprintln!("skipping PTY integration test: openpty is unavailable: {error}");
            None
        }
        Err(LaunchError::Io(error)) => panic!("could not launch editor in PTY: {error}"),
    }
}

fn editor_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_wscrpt"))
}

fn open_pty(columns: u16, rows: u16) -> io::Result<(File, File)> {
    let mut master = -1;
    let mut slave = -1;
    let window_size = WindowSize {
        rows: rows.max(1),
        columns: columns.max(1),
        x_pixels: 0,
        y_pixels: 0,
    };
    // SAFETY: openpty receives valid writable pointers for both descriptors,
    // null optional name/termios pointers, and a correctly laid-out winsize.
    // Successful descriptors are immediately transferred into owned Files.
    let result = unsafe {
        openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &window_size,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openpty succeeded and returned two newly owned descriptors.
    let master = unsafe { File::from_raw_fd(master) };
    // SAFETY: as above, with distinct ownership of the slave descriptor.
    let slave = unsafe { File::from_raw_fd(slave) };
    Ok((master, slave))
}

fn read_master(mut master: File, sender: mpsc::Sender<ReaderMessage>) {
    let mut buffer = [0_u8; 8192];
    loop {
        match master.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => {
                if sender
                    .send(ReaderMessage::Bytes(buffer[..read].to_vec()))
                    .is_err()
                {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            // Linux PTY masters conventionally report EIO when the final
            // slave descriptor closes. Treat that as EOF, like macOS's read 0.
            Err(error) if error.raw_os_error() == Some(5) => break,
            Err(error) => {
                let _ = sender.send(ReaderMessage::Failed(error.to_string()));
                break;
            }
        }
    }
}

fn assert_normal_terminal_cleanup(output: &[u8]) {
    let entered = find_bytes(output, ENTER_ALTERNATE_SCREEN, 0)
        .unwrap_or_else(|| panic!("no alternate-screen entry in {}", escaped_output(output)));
    let paste_enabled = find_bytes(output, ENABLE_BRACKETED_PASTE, entered)
        .unwrap_or_else(|| panic!("no bracketed-paste enable in {}", escaped_output(output)));
    let paste_disabled = find_bytes(output, DISABLE_BRACKETED_PASTE, paste_enabled)
        .unwrap_or_else(|| panic!("no bracketed-paste cleanup in {}", escaped_output(output)));
    let left = find_bytes(output, LEAVE_ALTERNATE_SCREEN, paste_disabled)
        .unwrap_or_else(|| panic!("no alternate-screen cleanup in {}", escaped_output(output)));
    assert!(
        find_bytes(output, SHOW_CURSOR, left).is_some(),
        "cursor was not shown after leaving the alternate screen: {}",
        escaped_output(output)
    );
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    find_bytes(haystack, needle, 0).is_some()
}

fn find_bytes(haystack: &[u8], needle: &[u8], start: usize) -> Option<usize> {
    if needle.is_empty() {
        return Some(start.min(haystack.len()));
    }
    haystack
        .get(start..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|position| start + position)
}

fn escaped_output(output: &[u8]) -> String {
    let tail_start = output.len().saturating_sub(4096);
    output[tail_start..]
        .iter()
        .flat_map(|byte| std::ascii::escape_default(*byte))
        .map(char::from)
        .collect()
}
