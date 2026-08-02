//! Minimal Agent Client Protocol (ACP) process bridge.
//!
//! Speaks NDJSON JSON-RPC 2.0 over stdio (as used by `grok agent stdio` and the
//! public ACP spec). Scope for 0.2.4:
//! - `initialize` → `session/new` → `session/prompt`
//! - map `session/update` into bounded coordinator receipts
//! - auto-cancel `session/request_permission` (Needs You notice; no silent writes)
//! - cancel via `session/cancel` + process-group kill
//!
//! Full tool mediation (fs/terminal client methods) is intentionally deferred.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

use crate::agent::sticky_artifact_ref;
use crate::agent_contract::{
    AgentEvent, AgentEventKind, AgentRunState, MAX_SUMMARY_BYTES, unix_now_ms,
};
use crate::agent_runtime::{AgentEventPort, AgentJob, AgentJobEvent};

const EVENT_CAPACITY: usize = 64;
const MAX_RECEIPT_NOTICES: usize = 48;

/// Inputs for one ACP process run.
#[derive(Clone, Debug)]
pub struct AcpRunRequest {
    pub workspace_id: u64,
    pub session_id: String,
    pub generation: u64,
    pub cwd: PathBuf,
    pub argv: Vec<String>,
    pub goal: String,
    pub sticky_brief: Option<String>,
    pub sticky_id: Option<String>,
}

/// Spawn an ACP agent subprocess and stream bounded events to the TUI.
pub fn spawn_acp_agent(request: AcpRunRequest) -> Result<(AgentJob, AgentEventPort), String> {
    if request.argv.is_empty() {
        return Err("agent.argv is empty".to_owned());
    }
    let program = request.argv[0].clone();
    let args = request.argv[1..].to_vec();
    let cwd = canonicalize_dir(&request.cwd)?;

    let mut command = Command::new(&program);
    command
        .args(&args)
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_process_group(&mut command);

    let mut child = command.spawn().map_err(|source| {
        format!(
            "failed to spawn agent process `{}`: {source} (install/login the host CLI · docs/AGENT_AUTH.md)",
            request.argv.join(" ")
        )
    })?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "agent process has no stdin pipe".to_owned())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "agent process has no stdout pipe".to_owned())?;
    // Drain stderr so a chatty agent cannot fill the pipe and stall.
    if let Some(stderr) = child.stderr.take() {
        thread::Builder::new()
            .name("wscrpt-agent-acp-stderr".to_owned())
            .spawn(move || {
                let reader = BufReader::new(stderr);
                for _line in reader.lines() {
                    // Intentionally discard: secrets must not land in receipts.
                }
            })
            .ok();
    }

    let (sender, receiver) = std::sync::mpsc::sync_channel(EVENT_CAPACITY);
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_flag = Arc::clone(&cancel);
    let handle = thread::Builder::new()
        .name("wscrpt-agent-acp".to_owned())
        .spawn(move || {
            run_acp_session(request, child, stdin, stdout, sender, cancel_flag);
        })
        .map_err(|source| format!("failed to start ACP worker thread: {source}"))?;

    Ok((
        AgentJob::from_parts(cancel, handle),
        AgentEventPort::from_receiver(receiver),
    ))
}

fn run_acp_session(
    request: AcpRunRequest,
    mut child: Child,
    mut stdin: impl Write,
    stdout: impl std::io::Read + Send + 'static,
    sender: SyncSender<AgentJobEvent>,
    cancel: Arc<AtomicBool>,
) {
    let mut seq = 0u64;
    let mut emit = |kind: AgentEventKind,
                    summary: String,
                    run_state: Option<AgentRunState>,
                    check_ok: Option<bool>,
                    path: Option<PathBuf>,
                    artifact_ref: Option<String>| {
        seq = seq.saturating_add(1);
        let mut summary = summary;
        if summary.trim().is_empty() {
            summary = kind.as_str().to_owned();
        }
        if summary.len() > MAX_SUMMARY_BYTES {
            summary.truncate(MAX_SUMMARY_BYTES.saturating_sub(1));
            summary.push('…');
        }
        let event = AgentEvent {
            workspace_id: request.workspace_id,
            session_id: request.session_id.clone(),
            generation: request.generation,
            sequence: seq,
            timestamp_unix_ms: unix_now_ms(),
            kind,
            summary,
            path,
            git_object: None,
            artifact_ref,
            check_ok,
            run_state,
            sensitive: false,
        };
        sender.send(AgentJobEvent::Event(event)).is_ok()
    };

    let _ = emit(
        AgentEventKind::State,
        format!("ACP process: {}", request.argv.join(" ")),
        Some(AgentRunState::Working),
        None,
        None,
        None,
    );

    if let Some(title_brief) = request.sticky_brief.as_ref() {
        let title = title_brief.lines().next().unwrap_or("sticky").to_owned();
        let artifact = request
            .sticky_id
            .as_deref()
            .map(sticky_artifact_ref);
        let _ = emit(
            AgentEventKind::Notice,
            format!("sticky brief: {title}"),
            None,
            None,
            None,
            artifact,
        );
    }

    let mut next_rpc_id: u64 = 1;
    let init_id = match write_rpc(
        &mut stdin,
        &mut next_rpc_id,
        "initialize",
        json!({
            "protocolVersion": 1,
            "clientCapabilities": {},
            "clientInfo": {
                "name": "wscrpt",
                "title": "wscrpt",
                "version": env!("CARGO_PKG_VERSION"),
            }
        }),
    ) {
        Ok(id) => id,
        Err(error) => {
            finish_error(&sender, &mut child, &cancel, error);
            return;
        }
    };

    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();
    let mut acp_session_id: Option<String> = None;
    let mut prompt_id: Option<u64> = None;
    let mut notices = 0usize;
    let mut agent_text = String::new();
    let mut saw_prompt_result = false;

    // Drive initialize → session/new → session/prompt, then stream until done.
    loop {
        if cancel.load(Ordering::Acquire) {
            if let Some(sid) = acp_session_id.as_ref() {
                let _ = writeln!(
                    stdin,
                    "{}",
                    json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":sid}})
                );
                let _ = stdin.flush();
            }
            terminate_child(&mut child);
            let _ = sender.send(AgentJobEvent::Finished {
                cancelled: true,
                error: None,
            });
            return;
        }

        let line = match lines.next() {
            Some(Ok(line)) => line,
            Some(Err(error)) => {
                finish_error(
                    &sender,
                    &mut child,
                    &cancel,
                    format!("read agent stdout: {error}"),
                );
                return;
            }
            None => {
                // EOF
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // JSON-RPC responses
        if let Some(id) = msg.get("id").and_then(Value::as_u64) {
            if let Some(error) = msg.get("error") {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("agent JSON-RPC error");
                finish_error(&sender, &mut child, &cancel, message.to_owned());
                return;
            }
            let result = msg.get("result");

            if id == init_id {
                let cwd = match request.cwd.canonicalize() {
                    Ok(p) => p,
                    Err(_) => request.cwd.clone(),
                };
                let cwd_str = cwd.to_string_lossy().into_owned();
                match write_rpc(
                    &mut stdin,
                    &mut next_rpc_id,
                    "session/new",
                    json!({
                        "cwd": cwd_str,
                        "mcpServers": []
                    }),
                ) {
                    Ok(_) => {}
                    Err(error) => {
                        finish_error(&sender, &mut child, &cancel, error);
                        return;
                    }
                }
                continue;
            }

            if acp_session_id.is_none()
                && let Some(sid) = result
                    .and_then(|r| r.get("sessionId"))
                    .and_then(Value::as_str)
            {
                acp_session_id = Some(sid.to_owned());
                let mut prompt_text = request.goal.clone();
                if let Some(brief) = &request.sticky_brief {
                    prompt_text.push_str("\n\n---\nSticky brief:\n");
                    prompt_text.push_str(brief);
                }
                match write_rpc(
                    &mut stdin,
                    &mut next_rpc_id,
                    "session/prompt",
                    json!({
                        "sessionId": sid,
                        "prompt": [{ "type": "text", "text": prompt_text }]
                    }),
                ) {
                    Ok(pid) => {
                        prompt_id = Some(pid);
                        let _ = emit(
                            AgentEventKind::Plan,
                            format!("prompt: {}", truncate(&request.goal, 200)),
                            None,
                            None,
                            None,
                            None,
                        );
                    }
                    Err(error) => {
                        finish_error(&sender, &mut child, &cancel, error);
                        return;
                    }
                }
                continue;
            }

            if prompt_id == Some(id) {
                saw_prompt_result = true;
                let stop = result
                    .and_then(|r| r.get("stopReason"))
                    .and_then(Value::as_str)
                    .unwrap_or("end_turn");
                if !agent_text.is_empty() {
                    let _ = emit(
                        AgentEventKind::Notice,
                        truncate(&agent_text, MAX_SUMMARY_BYTES),
                        None,
                        None,
                        None,
                        request.sticky_id.as_deref().map(sticky_artifact_ref),
                    );
                }
                let ok = stop == "end_turn" || stop == "max_tokens";
                let _ = emit(
                    AgentEventKind::CheckResult,
                    format!("ACP stop: {stop}"),
                    None,
                    Some(ok),
                    None,
                    None,
                );
                if let Some(id) = &request.sticky_id {
                    let _ = emit(
                        AgentEventKind::Artifact,
                        "receipt ready for sticky write-back — Esc w A".to_owned(),
                        None,
                        None,
                        None,
                        Some(sticky_artifact_ref(id)),
                    );
                }
                let _ = emit(
                    AgentEventKind::ReviewReady,
                    format!("ACP run finished ({stop}) — review in dashboard"),
                    Some(AgentRunState::Review),
                    None,
                    None,
                    None,
                );
                break;
            }
            continue;
        }

        // Notifications / server requests
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        if method == "session/request_permission" {
            if let Some(id) = msg.get("id") {
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": id.clone(),
                    "result": { "outcome": { "outcome": "cancelled" } }
                });
                if let Ok(line) = serde_json::to_string(&response) {
                    let _ = writeln!(stdin, "{line}");
                    let _ = stdin.flush();
                }
            }
            if notices < MAX_RECEIPT_NOTICES {
                notices += 1;
                let _ = emit(
                    AgentEventKind::Approval,
                    "agent requested permission — cancelled (Needs You; no silent grant)".to_owned(),
                    Some(AgentRunState::NeedsYou),
                    None,
                    None,
                    None,
                );
                let _ = emit(
                    AgentEventKind::State,
                    "resuming after permission cancel".to_owned(),
                    Some(AgentRunState::Working),
                    None,
                    None,
                    None,
                );
            }
            continue;
        }

        if method == "session/update" {
            let update = msg
                .pointer("/params/update")
                .cloned()
                .unwrap_or(Value::Null);
            let kind = update
                .get("sessionUpdate")
                .and_then(Value::as_str)
                .unwrap_or("");
            match kind {
                "plan" => {
                    let plan = format_plan(&update);
                    if !plan.is_empty() {
                        let _ = emit(AgentEventKind::Plan, plan, None, None, None, None);
                    }
                }
                "agent_message_chunk" => {
                    if let Some(text) = update
                        .pointer("/content/text")
                        .and_then(Value::as_str)
                    {
                        agent_text.push_str(text);
                        if agent_text.len() > 8 * 1024 {
                            agent_text.truncate(8 * 1024);
                        }
                    }
                }
                "tool_call" | "tool_call_update" if notices < MAX_RECEIPT_NOTICES => {
                    notices += 1;
                    let title = update
                        .get("title")
                        .and_then(Value::as_str)
                        .or_else(|| update.get("kind").and_then(Value::as_str))
                        .unwrap_or("tool");
                    let status = update
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let summary = if status.is_empty() {
                        format!("tool: {title}")
                    } else {
                        format!("tool {status}: {title}")
                    };
                    let path = update
                        .get("locations")
                        .and_then(Value::as_array)
                        .and_then(|arr| arr.first())
                        .and_then(|loc| loc.get("path"))
                        .and_then(Value::as_str)
                        .map(PathBuf::from);
                    // Only emit path_touched for completed tools with a relative path.
                    if kind == "tool_call_update"
                        && status == "completed"
                        && path.as_ref().is_some_and(|p| p.is_relative())
                    {
                        let _ = emit(
                            AgentEventKind::PathTouched,
                            summary,
                            None,
                            None,
                            path,
                            None,
                        );
                    } else {
                        let _ = emit(AgentEventKind::Notice, summary, None, None, None, None);
                    }
                }
                "available_commands_update" | "session_info_update" | "usage_update"
                | "agent_thought_chunk" | "user_message_chunk" => {}
                other if !other.is_empty() && notices < MAX_RECEIPT_NOTICES => {
                    notices += 1;
                    let _ = emit(
                        AgentEventKind::Notice,
                        format!("session update: {other}"),
                        None,
                        None,
                        None,
                        None,
                    );
                }
                _ => {}
            }
            continue;
        }

        // Ignore vendor extensions (_x.ai/...).
        let _ = method;
    }

    if !saw_prompt_result && !cancel.load(Ordering::Acquire) {
        if !agent_text.is_empty() {
            let _ = emit(
                AgentEventKind::Notice,
                truncate(&agent_text, MAX_SUMMARY_BYTES),
                None,
                None,
                None,
                None,
            );
        }
        let _ = emit(
            AgentEventKind::ReviewReady,
            "ACP stream ended — review partial receipt".to_owned(),
            Some(AgentRunState::Review),
            None,
            None,
            None,
        );
    }

    // Best-effort teardown.
    let _ = child.kill();
    let _ = child.wait();
    let _ = sender.send(AgentJobEvent::Finished {
        cancelled: cancel.load(Ordering::Acquire),
        error: None,
    });
}

fn write_rpc(
    stdin: &mut impl Write,
    next_rpc_id: &mut u64,
    method: &str,
    params: Value,
) -> Result<u64, String> {
    let id = *next_rpc_id;
    *next_rpc_id = next_rpc_id.saturating_add(1);
    let msg = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params,
    });
    let line = serde_json::to_string(&msg).map_err(|e| e.to_string())?;
    writeln!(stdin, "{line}").map_err(|e| format!("write stdin: {e}"))?;
    stdin.flush().map_err(|e| format!("flush stdin: {e}"))?;
    Ok(id)
}

fn finish_error(
    sender: &SyncSender<AgentJobEvent>,
    child: &mut Child,
    cancel: &AtomicBool,
    error: String,
) {
    terminate_child(child);
    let _ = sender.send(AgentJobEvent::Notice(error.clone()));
    let _ = sender.send(AgentJobEvent::Finished {
        cancelled: cancel.load(Ordering::Acquire),
        error: Some(error),
    });
}

fn terminate_child(child: &mut Child) {
    #[cfg(unix)]
    {
        let pid = child.id();
        let _ = signal_process_group(pid, 15);
        thread::sleep(Duration::from_millis(100));
        let _ = signal_process_group(pid, 9);
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn format_plan(update: &Value) -> String {
    let Some(entries) = update.get("entries").and_then(Value::as_array) else {
        return String::new();
    };
    let mut lines = Vec::new();
    for (i, entry) in entries.iter().take(8).enumerate() {
        let content = entry
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("step");
        lines.push(format!("{}. {content}", i + 1));
    }
    lines.join("\n")
}

fn truncate(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.len() <= max {
        return trimmed.to_owned();
    }
    let keep = max.saturating_sub(1);
    let mut end = keep.min(trimmed.len());
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = trimmed[..end].to_owned();
    out.push('…');
    out
}

fn canonicalize_dir(path: &Path) -> Result<PathBuf, String> {
    let cwd = path
        .canonicalize()
        .map_err(|e| format!("agent cwd {}: {e}", path.display()))?;
    if !cwd.is_dir() {
        return Err(format!("agent cwd is not a directory: {}", cwd.display()));
    }
    Ok(cwd)
}

#[cfg(unix)]
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: i32) -> std::io::Result<()> {
    let pid = i32::try_from(pid)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "pid"))?;
    // SAFETY: kill with negated PID targets the process group leader.
    let result = unsafe { libc::kill(-pid, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_char_boundary() {
        let s = truncate("hello🚀world", 8);
        assert!(s.ends_with('…') || s.len() <= 8);
    }
}
