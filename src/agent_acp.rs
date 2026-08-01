//! Minimal ACP (Agent Client Protocol) stdio client for W2 process agents.
//!
//! Speaks newline-delimited JSON-RPC 2.0 with a host-configured argv
//! (typical: `grok agent stdio`). Maps a subset of `session/update` traffic
//! into bounded [`AgentEvent`] receipts for the existing coordinator.
//!
//! This is intentionally thin: no full ACP SDK, no filesystem/terminal
//! delegation yet, no permission UI (session is created without yolo by
//! default so a future Needs You path can host approvals).

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use crate::agent_contract::{
    AgentEvent, AgentEventKind, AgentRunState, MAX_SUMMARY_BYTES, unix_now_ms,
};
use crate::agent_runtime::{AgentEventPort, AgentJob, AgentJobEvent, EVENT_CAPACITY};
use crate::lsp_discover::resolve_executable;

const READ_IDLE: Duration = Duration::from_millis(40);
const MAX_STDOUT_LINE_BYTES: usize = 256 * 1024;
const MAX_EMITTED_EVENTS: u64 = 200;

/// Spawn a real ACP agent process and drive one goal turn over stdio.
pub fn spawn_acp_agent(
    workspace_id: u64,
    host_session_id: impl Into<String>,
    generation: u64,
    cwd: impl Into<PathBuf>,
    argv: &[String],
    goal: impl Into<String>,
) -> Result<(AgentJob, AgentEventPort), String> {
    let host_session_id = host_session_id.into();
    let cwd = cwd.into();
    let goal = goal.into();
    if argv.is_empty() {
        return Err("agent.argv is empty — set e.g. [\"grok\", \"agent\", \"stdio\"]".to_owned());
    }
    let program = resolve_executable(&argv[0]).ok_or_else(|| {
        format!(
            "agent binary {:?} not found on PATH — install the CLI or fix agent.argv",
            argv[0]
        )
    })?;
    let args: Vec<String> = argv[1..].to_vec();
    let (sender, receiver) = std::sync::mpsc::sync_channel(EVENT_CAPACITY);
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_flag = Arc::clone(&cancel);
    let child_pid = Arc::new(AtomicU32::new(0));
    let child_pid_for_thread = Arc::clone(&child_pid);

    let handle = thread::Builder::new()
        .name("wscrpt-agent-acp".to_owned())
        .spawn(move || {
            run_acp_session(
                workspace_id,
                host_session_id,
                generation,
                cwd,
                program,
                args,
                goal,
                sender,
                cancel_flag,
                child_pid_for_thread,
            );
        })
        .map_err(|error| format!("spawn ACP worker thread: {error}"))?;

    Ok((
        AgentJob::new(cancel, Some(child_pid), handle),
        AgentEventPort::new(receiver),
    ))
}

#[allow(clippy::too_many_arguments)]
fn run_acp_session(
    workspace_id: u64,
    host_session_id: String,
    generation: u64,
    cwd: PathBuf,
    program: PathBuf,
    args: Vec<String>,
    goal: String,
    sender: SyncSender<AgentJobEvent>,
    cancel: Arc<AtomicBool>,
    child_pid: Arc<AtomicU32>,
) {
    let mut sequence = 0u64;
    let emit = |sequence: &mut u64,
                sender: &SyncSender<AgentJobEvent>,
                kind: AgentEventKind,
                summary: String,
                run_state: Option<AgentRunState>| {
        if *sequence >= MAX_EMITTED_EVENTS {
            return false;
        }
        *sequence = sequence.saturating_add(1);
        let event = AgentEvent {
            workspace_id,
            session_id: host_session_id.clone(),
            generation,
            sequence: *sequence,
            timestamp_unix_ms: unix_now_ms(),
            kind,
            summary: truncate_summary(summary),
            path: None,
            git_object: None,
            artifact_ref: None,
            check_ok: None,
            run_state,
            sensitive: false,
        };
        sender.send(AgentJobEvent::Event(event)).is_ok()
    };

    if !emit(
        &mut sequence,
        &sender,
        AgentEventKind::State,
        format!("ACP starting · {}", display_argv(&program, &args)),
        Some(AgentRunState::Brief),
    ) {
        return;
    }

    let mut child = match spawn_process(&program, &args, &cwd) {
        Ok(child) => child,
        Err(error) => {
            let _ = sender.send(AgentJobEvent::Finished {
                cancelled: false,
                error: Some(error),
            });
            return;
        }
    };
    child_pid.store(child.id(), Ordering::Release);

    if cancel.load(Ordering::Acquire) {
        terminate_child(&mut child);
        child_pid.store(0, Ordering::Release);
        let _ = sender.send(AgentJobEvent::Finished {
            cancelled: true,
            error: None,
        });
        return;
    }

    let mut stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            terminate_child(&mut child);
            child_pid.store(0, Ordering::Release);
            let _ = sender.send(AgentJobEvent::Finished {
                cancelled: false,
                error: Some("ACP process has no stdin".to_owned()),
            });
            return;
        }
    };
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            terminate_child(&mut child);
            child_pid.store(0, Ordering::Release);
            let _ = sender.send(AgentJobEvent::Finished {
                cancelled: false,
                error: Some("ACP process has no stdout".to_owned()),
            });
            return;
        }
    };
    let lines = spawn_stdout_reader(stdout);

    let mut next_id: u64 = 1;
    let initialize_id = next_id;
    next_id += 1;
    if let Err(error) = write_request(
        &mut stdin,
        initialize_id,
        "initialize",
        json!({
            "protocolVersion": 1,
            "clientInfo": {
                "name": "wscrpt",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "clientCapabilities": {
                // No FS/terminal delegation yet — agent uses its own host tools.
            },
        }),
    ) {
        finish_with_error(
            &mut child,
            &child_pid,
            &sender,
            cancel.load(Ordering::Acquire),
            error,
        );
        return;
    }

    let init_result = match wait_for_response(
        &lines,
        &mut child,
        &cancel,
        initialize_id,
        Duration::from_secs(20),
        &sender,
        &mut sequence,
        workspace_id,
        &host_session_id,
        generation,
    ) {
        Ok(value) => value,
        Err(WaitError::Cancelled) => {
            terminate_child(&mut child);
            child_pid.store(0, Ordering::Release);
            let _ = sender.send(AgentJobEvent::Finished {
                cancelled: true,
                error: None,
            });
            return;
        }
        Err(WaitError::Failed(error)) => {
            finish_with_error(&mut child, &child_pid, &sender, false, error);
            return;
        }
    };
    let protocol_version = init_result
        .get("protocolVersion")
        .and_then(Value::as_u64)
        .unwrap_or(1);
    if !emit(
        &mut sequence,
        &sender,
        AgentEventKind::State,
        format!("ACP initialize ok · protocol {protocol_version}"),
        Some(AgentRunState::Working),
    ) {
        terminate_child(&mut child);
        child_pid.store(0, Ordering::Release);
        return;
    }

    let session_new_id = next_id;
    next_id += 1;
    if let Err(error) = write_request(
        &mut stdin,
        session_new_id,
        "session/new",
        json!({
            "cwd": cwd.to_string_lossy(),
            "mcpServers": [],
        }),
    ) {
        finish_with_error(
            &mut child,
            &child_pid,
            &sender,
            cancel.load(Ordering::Acquire),
            error,
        );
        return;
    }

    let session_result = match wait_for_response(
        &lines,
        &mut child,
        &cancel,
        session_new_id,
        Duration::from_secs(30),
        &sender,
        &mut sequence,
        workspace_id,
        &host_session_id,
        generation,
    ) {
        Ok(value) => value,
        Err(WaitError::Cancelled) => {
            terminate_child(&mut child);
            child_pid.store(0, Ordering::Release);
            let _ = sender.send(AgentJobEvent::Finished {
                cancelled: true,
                error: None,
            });
            return;
        }
        Err(WaitError::Failed(error)) => {
            finish_with_error(&mut child, &child_pid, &sender, false, error);
            return;
        }
    };

    let acp_session_id = session_result
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    if acp_session_id.is_empty() {
        finish_with_error(
            &mut child,
            &child_pid,
            &sender,
            false,
            "session/new returned no sessionId".to_owned(),
        );
        return;
    }
    if !emit(
        &mut sequence,
        &sender,
        AgentEventKind::State,
        format!("ACP session {acp_session_id}"),
        Some(AgentRunState::Working),
    ) {
        terminate_child(&mut child);
        child_pid.store(0, Ordering::Release);
        return;
    }

    let prompt_id = next_id;
    if let Err(error) = write_request(
        &mut stdin,
        prompt_id,
        "session/prompt",
        json!({
            "sessionId": acp_session_id,
            "prompt": [{ "type": "text", "text": goal }],
        }),
    ) {
        finish_with_error(
            &mut child,
            &child_pid,
            &sender,
            cancel.load(Ordering::Acquire),
            error,
        );
        return;
    }
    let _ = sender.send(AgentJobEvent::Notice(
        "ACP prompt sent — live updates on Agents dashboard".to_owned(),
    ));

    // Drain until prompt response, cancel, or process exit.
    loop {
        if cancel.load(Ordering::Acquire) {
            terminate_child(&mut child);
            child_pid.store(0, Ordering::Release);
            let _ = sender.send(AgentJobEvent::Finished {
                cancelled: true,
                error: None,
            });
            return;
        }
        match lines.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => match parse_line(&line) {
                Ok(value) => {
                    if let Some(method) = value.get("method").and_then(Value::as_str) {
                        handle_notification(
                            method,
                            value.get("params"),
                            &mut sequence,
                            &sender,
                            workspace_id,
                            &host_session_id,
                            generation,
                        );
                        continue;
                    }
                    if value.get("id").and_then(Value::as_u64) == Some(prompt_id) {
                        if let Some(error) = value.get("error") {
                            let message = error
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("session/prompt failed");
                            finish_with_error(
                                &mut child,
                                &child_pid,
                                &sender,
                                false,
                                format!("ACP prompt error: {message}"),
                            );
                            return;
                        }
                        let stop = value
                            .pointer("/result/stopReason")
                            .and_then(Value::as_str)
                            .unwrap_or("end_turn");
                        let _ = emit(
                            &mut sequence,
                            &sender,
                            AgentEventKind::ReviewReady,
                            format!("ACP turn complete · {stop}"),
                            Some(AgentRunState::Review),
                        );
                        drop(stdin);
                        let _ = child.wait();
                        child_pid.store(0, Ordering::Release);
                        let _ = sender.send(AgentJobEvent::Finished {
                            cancelled: false,
                            error: None,
                        });
                        return;
                    }
                }
                Err(error) => {
                    finish_with_error(&mut child, &child_pid, &sender, false, error);
                    return;
                }
            },
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if let Ok(Some(status)) = child.try_wait() {
                    child_pid.store(0, Ordering::Release);
                    let _ = sender.send(AgentJobEvent::Finished {
                        cancelled: false,
                        error: Some(format!("ACP process exited early ({status})")),
                    });
                    return;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                child_pid.store(0, Ordering::Release);
                let _ = sender.send(AgentJobEvent::Finished {
                    cancelled: false,
                    error: Some("ACP process closed stdout before prompt finished".to_owned()),
                });
                return;
            }
        }
    }
}

fn spawn_stdout_reader(stdout: impl std::io::Read + Send + 'static) -> Receiver<String> {
    let (tx, rx) = std::sync::mpsc::sync_channel(EVENT_CAPACITY);
    thread::Builder::new()
        .name("wscrpt-agent-acp-stdout".to_owned())
        .spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        if line.len() > MAX_STDOUT_LINE_BYTES {
                            break;
                        }
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .expect("spawn ACP stdout reader");
    rx
}

fn parse_line(line: &str) -> Result<Value, String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err("empty ACP line".to_owned());
    }
    serde_json::from_str(trimmed).map_err(|error| format!("ACP JSON parse: {error}"))
}

fn handle_notification(
    method: &str,
    params: Option<&Value>,
    sequence: &mut u64,
    sender: &SyncSender<AgentJobEvent>,
    workspace_id: u64,
    host_session_id: &str,
    generation: u64,
) {
    if method != "session/update" && method != "x.ai/session/update" {
        return;
    }
    let update = params
        .and_then(|params| params.get("update").or_else(|| params.get("sessionUpdate")))
        .cloned()
        .or_else(|| params.cloned())
        .unwrap_or(Value::Null);

    // Grok emits sessionUpdate on the update object; also accept flat shape.
    let kind = update
        .get("sessionUpdate")
        .or_else(|| update.get("session_update"))
        .and_then(Value::as_str)
        .unwrap_or("update");

    let (event_kind, run_state, summary) = match kind {
        "plan" => {
            let text = extract_text(&update).unwrap_or_else(|| "plan update".to_owned());
            (
                AgentEventKind::Plan,
                Some(AgentRunState::Working),
                format!("plan: {text}"),
            )
        }
        "tool_call" => {
            let title = update
                .get("title")
                .and_then(Value::as_str)
                .or_else(|| update.get("kind").and_then(Value::as_str))
                .unwrap_or("tool");
            (
                AgentEventKind::Notice,
                Some(AgentRunState::Working),
                format!("tool: {title}"),
            )
        }
        "tool_call_update" => {
            let status = update
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("update");
            (
                AgentEventKind::Notice,
                Some(AgentRunState::Working),
                format!("tool {status}"),
            )
        }
        "agent_message_chunk" | "agent_thought_chunk" => {
            // Coalesce-only: emit occasional progress, not every token.
            // Skip pure chunks to keep receipt bounded; status line gets notices
            // from higher-level tools. Emit a lightweight working heartbeat rarely.
            return;
        }
        other => {
            let text = extract_text(&update).unwrap_or_else(|| other.to_owned());
            (
                AgentEventKind::Notice,
                Some(AgentRunState::Working),
                format!("acp: {text}"),
            )
        }
    };

    if *sequence >= MAX_EMITTED_EVENTS {
        return;
    }
    *sequence = sequence.saturating_add(1);
    let event = AgentEvent {
        workspace_id,
        session_id: host_session_id.to_owned(),
        generation,
        sequence: *sequence,
        timestamp_unix_ms: unix_now_ms(),
        kind: event_kind,
        summary: truncate_summary(summary),
        path: None,
        git_object: None,
        artifact_ref: None,
        check_ok: None,
        run_state,
        sensitive: false,
    };
    let _ = sender.send(AgentJobEvent::Event(event));
}

fn extract_text(value: &Value) -> Option<String> {
    if let Some(text) = value.get("content").and_then(Value::as_str) {
        return Some(text.to_owned());
    }
    if let Some(text) = value.pointer("/content/text").and_then(Value::as_str) {
        return Some(text.to_owned());
    }
    if let Some(entries) = value.get("content").and_then(Value::as_array) {
        let mut out = String::new();
        for entry in entries {
            if let Some(text) = entry.get("text").and_then(Value::as_str) {
                if !out.is_empty() {
                    out.push(' ');
                }
                out.push_str(text);
            }
        }
        if !out.is_empty() {
            return Some(out);
        }
    }
    value
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn truncate_summary(summary: String) -> String {
    if summary.len() <= MAX_SUMMARY_BYTES {
        return summary;
    }
    let mut end = MAX_SUMMARY_BYTES.saturating_sub(1);
    while end > 0 && !summary.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &summary[..end])
}

fn display_argv(program: &Path, args: &[String]) -> String {
    let mut parts = vec![
        program
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| program.display().to_string()),
    ];
    parts.extend(args.iter().cloned());
    parts.join(" ")
}

fn spawn_process(program: &Path, args: &[String], cwd: &Path) -> Result<Child, String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    configure_process_group(&mut command);
    command
        .spawn()
        .map_err(|error| format!("failed to spawn {}: {error}", program.display()))
}

fn write_request(
    stdin: &mut impl Write,
    id: u64,
    method: &str,
    params: Value,
) -> Result<(), String> {
    let payload = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    let mut line = payload.to_string();
    line.push('\n');
    stdin
        .write_all(line.as_bytes())
        .map_err(|error| format!("ACP write {method}: {error}"))?;
    stdin
        .flush()
        .map_err(|error| format!("ACP flush {method}: {error}"))
}

enum WaitError {
    Cancelled,
    Failed(String),
}

#[allow(clippy::too_many_arguments)]
fn wait_for_response(
    lines: &Receiver<String>,
    child: &mut Child,
    cancel: &AtomicBool,
    expect_id: u64,
    timeout: Duration,
    sender: &SyncSender<AgentJobEvent>,
    sequence: &mut u64,
    workspace_id: u64,
    host_session_id: &str,
    generation: u64,
) -> Result<Value, WaitError> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(WaitError::Cancelled);
        }
        if std::time::Instant::now() > deadline {
            return Err(WaitError::Failed(format!(
                "timed out waiting for ACP response id={expect_id}"
            )));
        }
        match lines.recv_timeout(READ_IDLE) {
            Ok(line) => {
                let value = parse_line(&line).map_err(WaitError::Failed)?;
                if let Some(method) = value.get("method").and_then(Value::as_str) {
                    handle_notification(
                        method,
                        value.get("params"),
                        sequence,
                        sender,
                        workspace_id,
                        host_session_id,
                        generation,
                    );
                    continue;
                }
                if value.get("id").and_then(Value::as_u64) == Some(expect_id) {
                    if let Some(error) = value.get("error") {
                        let message = error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("request failed");
                        return Err(WaitError::Failed(message.to_owned()));
                    }
                    return Ok(value.get("result").cloned().unwrap_or(Value::Null));
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if let Ok(Some(status)) = child.try_wait() {
                    return Err(WaitError::Failed(format!(
                        "ACP process exited while waiting ({status})"
                    )));
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(WaitError::Failed(
                    "ACP process closed stdout while waiting for response".to_owned(),
                ));
            }
        }
    }
}

fn finish_with_error(
    child: &mut Child,
    child_pid: &AtomicU32,
    sender: &SyncSender<AgentJobEvent>,
    cancelled: bool,
    error: String,
) {
    terminate_child(child);
    child_pid.store(0, Ordering::Release);
    let _ = sender.send(AgentJobEvent::Finished {
        cancelled,
        error: if cancelled { None } else { Some(error) },
    });
}

fn terminate_child(child: &mut Child) {
    let pid = child.id();
    let _ = signal_process_group(pid, libc::SIGTERM);
    thread::sleep(Duration::from_millis(50));
    match child.try_wait() {
        Ok(Some(_)) => {}
        _ => {
            let _ = signal_process_group(pid, libc::SIGKILL);
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: libc::c_int) -> std::io::Result<()> {
    let result = unsafe { libc::killpg(pid as libc::pid_t, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn signal_process_group(_pid: u32, _signal: i32) -> std::io::Result<()> {
    Ok(())
}

/// Kill helper used by [`AgentJob::cancel`] when a process is live.
pub(crate) fn kill_agent_process_group(pid: u32) {
    if pid == 0 {
        return;
    }
    let _ = signal_process_group(pid, libc::SIGTERM);
    thread::sleep(Duration::from_millis(30));
    let _ = signal_process_group(pid, libc::SIGKILL);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentCoordinator;
    use crate::agent_runtime::{AgentJobEvent, new_session_id, work_packet_for_goal};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::mpsc::TryRecvError;
    use std::time::Duration;

    fn write_fake_acp_script(dir: &Path) -> PathBuf {
        let path = dir.join("fake-acp.py");
        // Minimal NDJSON ACP server: initialize, session/new, session/prompt + updates.
        let script = r#"#!/usr/bin/env python3
import json, sys

def read():
    line = sys.stdin.readline()
    if not line:
        return None
    return json.loads(line)

def write(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

while True:
    msg = read()
    if msg is None:
        break
    method = msg.get("method")
    mid = msg.get("id")
    if method == "initialize":
        write({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":1,"agentCapabilities":{}}})
    elif method == "session/new":
        write({"jsonrpc":"2.0","id":mid,"result":{"sessionId":"acp-test-session"}})
    elif method == "session/prompt":
        write({"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"plan","content":"1. inspect 2. edit"}}})
        write({"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"tool_call","title":"read Cargo.toml"}}})
        write({"jsonrpc":"2.0","id":mid,"result":{"stopReason":"end_turn"}})
        break
    else:
        write({"jsonrpc":"2.0","id":mid,"error":{"code":-32601,"message":"unknown method"}})
"#;
        fs::write(&path, script).unwrap();
        let mut perms = fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&path, perms).unwrap();
        path
    }

    #[test]
    fn acp_fake_process_reaches_review() {
        let temp = tempfile::tempdir().unwrap();
        let script = write_fake_acp_script(temp.path());
        let mut coordinator = AgentCoordinator::new(99);
        let session = new_session_id();
        let packet = work_packet_for_goal(99, temp.path(), "demo goal").unwrap();
        let generation = coordinator
            .start_run(session.clone(), packet, false)
            .unwrap();
        let argv = vec![script.to_string_lossy().into_owned()];
        let (job, port) =
            spawn_acp_agent(99, session, generation, temp.path(), &argv, "demo goal").unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match port.try_recv() {
                Ok(AgentJobEvent::Event(event)) => {
                    coordinator.admit(event).unwrap();
                }
                Ok(AgentJobEvent::Finished { cancelled, error }) => {
                    assert!(!cancelled, "error={error:?}");
                    assert!(error.is_none(), "{error:?}");
                    break;
                }
                Ok(AgentJobEvent::Notice(_)) => {}
                Err(TryRecvError::Empty) => {
                    if std::time::Instant::now() > deadline {
                        panic!(
                            "timeout waiting for fake ACP; state={:?}",
                            coordinator.run_state()
                        );
                    }
                    thread::sleep(Duration::from_millis(10));
                }
                Err(TryRecvError::Disconnected) => panic!("disconnected"),
            }
        }
        drop(job);
        assert_eq!(coordinator.run_state(), AgentRunState::Review);
        assert!(coordinator.receipt().len() >= 3);
    }
}
