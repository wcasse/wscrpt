//! Minimal ACP (Agent Client Protocol) stdio client for W2 process agents.
//!
//! Speaks newline-delimited JSON-RPC 2.0 with a host-configured argv
//! (typical: `grok agent stdio`). Maps a subset of `session/update` traffic
//! into bounded [`AgentEvent`] receipts for the existing coordinator.
//!
//! This is intentionally thin: no full ACP SDK, no filesystem/terminal
//! delegation yet. Tool permission prompts (`session/request_permission`)
//! surface as **Needs You** on the Agents dashboard (Y allow · N deny).

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
use crate::agent_runtime::{
    AgentEventPort, AgentJob, AgentJobEvent, EVENT_CAPACITY, PendingPermission, PermissionDecision,
    PermissionOption, send_job_event_auto,
};
use crate::lsp_discover::resolve_executable;

const READ_IDLE: Duration = Duration::from_millis(40);
const MAX_STDOUT_LINE_BYTES: usize = 256 * 1024;
const MAX_EMITTED_EVENTS: u64 = 200;
/// Flush coalesced agent text after this many UTF-8 bytes.
const CHUNK_FLUSH_BYTES: usize = 120;
/// Flush coalesced agent text after this idle gap.
const CHUNK_FLUSH_IDLE: Duration = Duration::from_millis(400);
/// Max locations to map into path_touched events per tool update.
const MAX_PATHS_PER_TOOL: usize = 8;

/// Backpressured deliver: soft events drop when the UI is behind; hard events wait.
fn deliver(sender: &SyncSender<AgentJobEvent>, event: AgentJobEvent, cancel: &AtomicBool) -> bool {
    send_job_event_auto(sender, event, Some(cancel))
}

/// Assign sequence only if the event is queued (soft drops do not burn sequence ids).
#[allow(clippy::too_many_arguments)]
fn deliver_sequenced(
    sequence: &mut u64,
    sender: &SyncSender<AgentJobEvent>,
    cancel: &AtomicBool,
    workspace_id: u64,
    host_session_id: &str,
    generation: u64,
    kind: AgentEventKind,
    summary: String,
    run_state: Option<AgentRunState>,
    path: Option<PathBuf>,
) -> bool {
    if *sequence >= MAX_EMITTED_EVENTS {
        return false;
    }
    let next = sequence.saturating_add(1);
    let event = AgentEvent {
        workspace_id,
        session_id: host_session_id.to_owned(),
        generation,
        sequence: next,
        timestamp_unix_ms: unix_now_ms(),
        kind,
        summary: truncate_summary(summary),
        path,
        git_object: None,
        artifact_ref: None,
        check_ok: None,
        run_state,
        sensitive: false,
    };
    if deliver(sender, AgentJobEvent::Event(event), cancel) {
        *sequence = next;
        true
    } else {
        false
    }
}

/// Coalesces streaming message/thought chunks into sparse receipt lines.
#[derive(Debug, Default)]
struct ChunkCoalesce {
    buffer: String,
    label: &'static str,
    last_push: Option<std::time::Instant>,
}

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
    let (permission_tx, permission_rx) = std::sync::mpsc::sync_channel(4);
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
                permission_rx,
            );
        })
        .map_err(|error| format!("spawn ACP worker thread: {error}"))?;

    Ok((
        AgentJob::new(cancel, Some(child_pid), Some(permission_tx), handle),
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
    permission_rx: Receiver<PermissionDecision>,
) {
    let mut sequence = 0u64;
    let mut coalesce = ChunkCoalesce::default();
    let cancel_for_emit = Arc::clone(&cancel);
    let emit = |sequence: &mut u64,
                sender: &SyncSender<AgentJobEvent>,
                kind: AgentEventKind,
                summary: String,
                run_state: Option<AgentRunState>,
                path: Option<PathBuf>| {
        deliver_sequenced(
            sequence,
            sender,
            cancel_for_emit.as_ref(),
            workspace_id,
            &host_session_id,
            generation,
            kind,
            summary,
            run_state,
            path,
        )
    };

    if !emit(
        &mut sequence,
        &sender,
        AgentEventKind::State,
        format!("ACP starting · {}", display_argv(&program, &args)),
        Some(AgentRunState::Brief),
        None,
    ) {
        return;
    }

    let mut child = match spawn_process(&program, &args, &cwd) {
        Ok(child) => child,
        Err(error) => {
            let _ = deliver(
                &sender,
                AgentJobEvent::Finished {
                    cancelled: false,
                    error: Some(error),
                },
                cancel.as_ref(),
            );
            return;
        }
    };
    child_pid.store(child.id(), Ordering::Release);

    if cancel.load(Ordering::Acquire) {
        terminate_child(&mut child);
        child_pid.store(0, Ordering::Release);
        let _ = deliver(
            &sender,
            AgentJobEvent::Finished {
                cancelled: true,
                error: None,
            },
            cancel.as_ref(),
        );
        return;
    }

    let mut stdin = match child.stdin.take() {
        Some(stdin) => stdin,
        None => {
            terminate_child(&mut child);
            child_pid.store(0, Ordering::Release);
            let _ = deliver(
                &sender,
                AgentJobEvent::Finished {
                    cancelled: false,
                    error: Some("ACP process has no stdin".to_owned()),
                },
                cancel.as_ref(),
            );
            return;
        }
    };
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            terminate_child(&mut child);
            child_pid.store(0, Ordering::Release);
            let _ = deliver(
                &sender,
                AgentJobEvent::Finished {
                    cancelled: false,
                    error: Some("ACP process has no stdout".to_owned()),
                },
                cancel.as_ref(),
            );
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
            cancel.as_ref(),
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
            let _ = deliver(
                &sender,
                AgentJobEvent::Finished {
                    cancelled: true,
                    error: None,
                },
                cancel.as_ref(),
            );
            return;
        }
        Err(WaitError::Failed(error)) => {
            finish_with_error(
                &mut child,
                &child_pid,
                &sender,
                cancel.as_ref(),
                false,
                error,
            );
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
        None,
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
            cancel.as_ref(),
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
            let _ = deliver(
                &sender,
                AgentJobEvent::Finished {
                    cancelled: true,
                    error: None,
                },
                cancel.as_ref(),
            );
            return;
        }
        Err(WaitError::Failed(error)) => {
            finish_with_error(
                &mut child,
                &child_pid,
                &sender,
                cancel.as_ref(),
                false,
                error,
            );
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
            cancel.as_ref(),
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
        None,
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
            cancel.as_ref(),
            cancel.load(Ordering::Acquire),
            error,
        );
        return;
    }
    let _ = deliver(
        &sender,
        AgentJobEvent::Notice("ACP prompt sent — live updates on Agents dashboard".to_owned()),
        cancel.as_ref(),
    );

    // Drain until prompt response, cancel, or process exit.
    loop {
        if cancel.load(Ordering::Acquire) {
            terminate_child(&mut child);
            child_pid.store(0, Ordering::Release);
            let _ = deliver(
                &sender,
                AgentJobEvent::Finished {
                    cancelled: true,
                    error: None,
                },
                cancel.as_ref(),
            );
            return;
        }
        match lines.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => match parse_line(&line) {
                Ok(value) => {
                    if let Some(method) = value.get("method").and_then(Value::as_str) {
                        // Agent → client request (has id): permission prompt.
                        if method == "session/request_permission"
                            && let Some(request_id) = value.get("id").and_then(Value::as_u64)
                        {
                            if let Err(error) = handle_permission_request(
                                request_id,
                                value.get("params"),
                                &mut stdin,
                                &permission_rx,
                                cancel.as_ref(),
                                &mut sequence,
                                &sender,
                                workspace_id,
                                &host_session_id,
                                generation,
                            ) {
                                if error == "cancelled" {
                                    terminate_child(&mut child);
                                    child_pid.store(0, Ordering::Release);
                                    let _ = deliver(
                                        &sender,
                                        AgentJobEvent::Finished {
                                            cancelled: true,
                                            error: None,
                                        },
                                        cancel.as_ref(),
                                    );
                                    return;
                                }
                                finish_with_error(
                                    &mut child,
                                    &child_pid,
                                    &sender,
                                    cancel.as_ref(),
                                    false,
                                    error,
                                );
                                return;
                            }
                            continue;
                        }
                        // Notifications (no id, or other methods).
                        if value.get("id").is_none() {
                            handle_notification(
                                method,
                                value.get("params"),
                                &cwd,
                                &mut sequence,
                                &mut coalesce,
                                &sender,
                                cancel.as_ref(),
                                workspace_id,
                                &host_session_id,
                                generation,
                            );
                        }
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
                                cancel.as_ref(),
                                false,
                                format!("ACP prompt error: {message}"),
                            );
                            return;
                        }
                        flush_coalesce(
                            &mut coalesce,
                            &mut sequence,
                            &sender,
                            cancel.as_ref(),
                            workspace_id,
                            &host_session_id,
                            generation,
                            true,
                        );
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
                            None,
                        );
                        drop(stdin);
                        let _ = child.wait();
                        child_pid.store(0, Ordering::Release);
                        let _ = deliver(
                            &sender,
                            AgentJobEvent::Finished {
                                cancelled: false,
                                error: None,
                            },
                            cancel.as_ref(),
                        );
                        return;
                    }
                }
                Err(error) => {
                    finish_with_error(
                        &mut child,
                        &child_pid,
                        &sender,
                        cancel.as_ref(),
                        false,
                        error,
                    );
                    return;
                }
            },
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                flush_coalesce(
                    &mut coalesce,
                    &mut sequence,
                    &sender,
                    cancel.as_ref(),
                    workspace_id,
                    &host_session_id,
                    generation,
                    false,
                );
                if let Ok(Some(status)) = child.try_wait() {
                    child_pid.store(0, Ordering::Release);
                    let _ = deliver(
                        &sender,
                        AgentJobEvent::Finished {
                            cancelled: false,
                            error: Some(format!("ACP process exited early ({status})")),
                        },
                        cancel.as_ref(),
                    );
                    return;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                child_pid.store(0, Ordering::Release);
                let _ = deliver(
                    &sender,
                    AgentJobEvent::Finished {
                        cancelled: false,
                        error: Some("ACP process closed stdout before prompt finished".to_owned()),
                    },
                    cancel.as_ref(),
                );
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

#[allow(clippy::too_many_arguments)]
fn handle_permission_request(
    request_id: u64,
    params: Option<&Value>,
    stdin: &mut impl Write,
    permission_rx: &Receiver<PermissionDecision>,
    cancel: &AtomicBool,
    sequence: &mut u64,
    sender: &SyncSender<AgentJobEvent>,
    workspace_id: u64,
    host_session_id: &str,
    generation: u64,
) -> Result<(), String> {
    let params = params.cloned().unwrap_or(Value::Null);
    let options = parse_permission_options(params.get("options"));
    if options.is_empty() {
        return Err("session/request_permission had no options".to_owned());
    }
    let tool_title = params
        .pointer("/toolCall/title")
        .or_else(|| params.pointer("/toolCall/fields/title"))
        .and_then(Value::as_str)
        .or_else(|| params.pointer("/toolCall/kind").and_then(Value::as_str))
        .unwrap_or("tool");
    let summary = format!("permission: {tool_title}");

    // Receipt + Needs You state for the dashboard.
    let _ = deliver_sequenced(
        sequence,
        sender,
        cancel,
        workspace_id,
        host_session_id,
        generation,
        AgentEventKind::Approval,
        summary.clone(),
        Some(AgentRunState::NeedsYou),
        None,
    );
    let _ = deliver(
        sender,
        AgentJobEvent::PermissionNeeded(PendingPermission {
            request_id,
            summary: truncate_summary(summary),
            options: options.clone(),
        }),
        cancel,
    );

    // Wait for the TUI (or cancel).
    let decision = loop {
        if cancel.load(Ordering::Acquire) {
            break PermissionDecision::Cancelled;
        }
        match permission_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(decision) => break decision,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                break PermissionDecision::Cancelled;
            }
        }
    };

    let selected_id = match &decision {
        PermissionDecision::Select { option_id } => Some(option_id.clone()),
        PermissionDecision::Cancelled => None,
    };
    let result = match &decision {
        PermissionDecision::Select { option_id } => json!({
            "outcome": {
                "outcome": "selected",
                "optionId": option_id,
            }
        }),
        PermissionDecision::Cancelled => json!({
            "outcome": { "outcome": "cancelled" }
        }),
    };
    write_response(stdin, request_id, result)?;

    if selected_id.is_none() && cancel.load(Ordering::Acquire) {
        return Err("cancelled".to_owned());
    }

    // After a selection, mark working again on the receipt.
    if let Some(option_id) = selected_id {
        let _ = deliver_sequenced(
            sequence,
            sender,
            cancel,
            workspace_id,
            host_session_id,
            generation,
            AgentEventKind::Notice,
            format!("permission answered · {option_id}"),
            Some(AgentRunState::Working),
            None,
        );
    }
    Ok(())
}

fn parse_permission_options(value: Option<&Value>) -> Vec<PermissionOption> {
    let Some(Value::Array(items)) = value else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for item in items {
        let option_id = item
            .get("optionId")
            .or_else(|| item.get("option_id"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        if option_id.is_empty() {
            continue;
        }
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(option_id.as_str())
            .to_owned();
        let kind = item
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("allow_once")
            .to_owned();
        out.push(PermissionOption {
            option_id,
            name,
            kind,
        });
    }
    out
}

fn write_response(stdin: &mut impl Write, id: u64, result: Value) -> Result<(), String> {
    let payload = json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    });
    let mut line = payload.to_string();
    line.push('\n');
    stdin
        .write_all(line.as_bytes())
        .map_err(|error| format!("ACP write response: {error}"))?;
    stdin
        .flush()
        .map_err(|error| format!("ACP flush response: {error}"))
}

#[allow(clippy::too_many_arguments)]
fn handle_notification(
    method: &str,
    params: Option<&Value>,
    cwd: &Path,
    sequence: &mut u64,
    coalesce: &mut ChunkCoalesce,
    sender: &SyncSender<AgentJobEvent>,
    cancel: &AtomicBool,
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

    match kind {
        "agent_message_chunk" => {
            push_chunk(
                coalesce,
                "agent",
                extract_text(&update).unwrap_or_default(),
                sequence,
                sender,
                cancel,
                workspace_id,
                host_session_id,
                generation,
            );
            return;
        }
        "agent_thought_chunk" => {
            push_chunk(
                coalesce,
                "thought",
                extract_text(&update).unwrap_or_default(),
                sequence,
                sender,
                cancel,
                workspace_id,
                host_session_id,
                generation,
            );
            return;
        }
        _ => {
            // Discrete events flush any streaming text first.
            flush_coalesce(
                coalesce,
                sequence,
                sender,
                cancel,
                workspace_id,
                host_session_id,
                generation,
                true,
            );
        }
    }

    match kind {
        "plan" => {
            let text = extract_text(&update).unwrap_or_else(|| "plan update".to_owned());
            emit_event(
                sequence,
                sender,
                cancel,
                workspace_id,
                host_session_id,
                generation,
                AgentEventKind::Plan,
                format!("plan: {text}"),
                Some(AgentRunState::Working),
                None,
            );
        }
        "tool_call" | "tool_call_update" => {
            emit_tool_update(
                &update,
                kind,
                cwd,
                sequence,
                sender,
                cancel,
                workspace_id,
                host_session_id,
                generation,
            );
        }
        other => {
            let text = extract_text(&update).unwrap_or_else(|| other.to_owned());
            emit_event(
                sequence,
                sender,
                cancel,
                workspace_id,
                host_session_id,
                generation,
                AgentEventKind::Notice,
                format!("acp: {text}"),
                Some(AgentRunState::Working),
                None,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn emit_tool_update(
    update: &Value,
    kind: &str,
    cwd: &Path,
    sequence: &mut u64,
    sender: &SyncSender<AgentJobEvent>,
    cancel: &AtomicBool,
    workspace_id: u64,
    host_session_id: &str,
    generation: u64,
) {
    let title = update
        .get("title")
        .and_then(Value::as_str)
        .or_else(|| update.pointer("/fields/title").and_then(Value::as_str))
        .or_else(|| update.get("kind").and_then(Value::as_str))
        .unwrap_or(if kind == "tool_call_update" {
            "tool update"
        } else {
            "tool"
        });
    let status = update.get("status").and_then(Value::as_str).unwrap_or("");
    let summary = if status.is_empty() {
        format!("tool: {title}")
    } else {
        format!("tool {status}: {title}")
    };
    emit_event(
        sequence,
        sender,
        cancel,
        workspace_id,
        host_session_id,
        generation,
        AgentEventKind::Notice,
        summary,
        Some(AgentRunState::Working),
        None,
    );

    let paths = extract_tool_paths(update, cwd);
    for path in paths.into_iter().take(MAX_PATHS_PER_TOOL) {
        let path_display = path.display().to_string();
        emit_event(
            sequence,
            sender,
            cancel,
            workspace_id,
            host_session_id,
            generation,
            AgentEventKind::PathTouched,
            format!("path: {path_display}"),
            Some(AgentRunState::Working),
            Some(path),
        );
    }
}

/// Collect workspace-relative paths from tool_call locations / raw_input.
fn extract_tool_paths(update: &Value, cwd: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut push = |raw: &str| {
        if let Some(relative) = relativize_workspace_path(raw, cwd)
            && !paths.iter().any(|existing| existing == &relative)
        {
            paths.push(relative);
        }
    };

    if let Some(locations) = update
        .get("locations")
        .or_else(|| update.pointer("/fields/locations"))
        .and_then(Value::as_array)
    {
        for location in locations {
            if let Some(path) = location.get("path").and_then(Value::as_str) {
                push(path);
            }
        }
    }

    // Common raw_input shapes: { "path": "..." } or { "file": "..." } or { "file_path": "..." }.
    for key in ["path", "file", "file_path", "filename", "target"] {
        if let Some(path) = update
            .pointer(&format!("/rawInput/{key}"))
            .or_else(|| update.pointer(&format!("/raw_input/{key}")))
            .or_else(|| update.pointer(&format!("/fields/rawInput/{key}")))
            .and_then(Value::as_str)
        {
            push(path);
        }
    }

    paths
}

/// Map an absolute or relative path into a workspace-relative path when possible.
fn relativize_workspace_path(raw: &str, cwd: &Path) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let without_uri = trimmed
        .strip_prefix("file://")
        .unwrap_or(trimmed)
        .to_owned();
    let path = PathBuf::from(&without_uri);
    let relative = if path.is_absolute() {
        path.strip_prefix(cwd).ok()?.to_path_buf()
    } else {
        path
    };
    if relative.as_os_str().is_empty() {
        return None;
    }
    // Coordinator rejects `..` escapes; drop those early.
    if relative
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return None;
    }
    Some(relative)
}

#[allow(clippy::too_many_arguments)]
fn push_chunk(
    coalesce: &mut ChunkCoalesce,
    label: &'static str,
    text: String,
    sequence: &mut u64,
    sender: &SyncSender<AgentJobEvent>,
    cancel: &AtomicBool,
    workspace_id: u64,
    host_session_id: &str,
    generation: u64,
) {
    if text.is_empty() {
        return;
    }
    if coalesce.label != label && !coalesce.buffer.is_empty() {
        flush_coalesce(
            coalesce,
            sequence,
            sender,
            cancel,
            workspace_id,
            host_session_id,
            generation,
            true,
        );
    }
    coalesce.label = label;
    coalesce.buffer.push_str(&text);
    coalesce.last_push = Some(std::time::Instant::now());
    if coalesce.buffer.len() >= CHUNK_FLUSH_BYTES {
        flush_coalesce(
            coalesce,
            sequence,
            sender,
            cancel,
            workspace_id,
            host_session_id,
            generation,
            true,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn flush_coalesce(
    coalesce: &mut ChunkCoalesce,
    sequence: &mut u64,
    sender: &SyncSender<AgentJobEvent>,
    cancel: &AtomicBool,
    workspace_id: u64,
    host_session_id: &str,
    generation: u64,
    force: bool,
) {
    if coalesce.buffer.is_empty() {
        return;
    }
    if !force {
        let Some(last) = coalesce.last_push else {
            return;
        };
        if last.elapsed() < CHUNK_FLUSH_IDLE {
            return;
        }
    }
    let text = std::mem::take(&mut coalesce.buffer);
    let label = coalesce.label;
    coalesce.last_push = None;
    let collapsed = collapse_whitespace(&text);
    if collapsed.is_empty() {
        return;
    }
    emit_event(
        sequence,
        sender,
        cancel,
        workspace_id,
        host_session_id,
        generation,
        AgentEventKind::Notice,
        format!("{label}: {collapsed}"),
        Some(AgentRunState::Working),
        None,
    );
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[allow(clippy::too_many_arguments)]
fn emit_event(
    sequence: &mut u64,
    sender: &SyncSender<AgentJobEvent>,
    cancel: &AtomicBool,
    workspace_id: u64,
    host_session_id: &str,
    generation: u64,
    kind: AgentEventKind,
    summary: String,
    run_state: Option<AgentRunState>,
    path: Option<PathBuf>,
) {
    let _ = deliver_sequenced(
        sequence,
        sender,
        cancel,
        workspace_id,
        host_session_id,
        generation,
        kind,
        summary,
        run_state,
        path,
    );
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
    // Handshake phase: ignore streaming coalesce (no cwd-bound tool paths yet).
    let mut coalesce = ChunkCoalesce::default();
    let cwd = PathBuf::from(".");
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
                        &cwd,
                        sequence,
                        &mut coalesce,
                        sender,
                        cancel,
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
    cancel: &AtomicBool,
    cancelled: bool,
    error: String,
) {
    terminate_child(child);
    child_pid.store(0, Ordering::Release);
    let _ = deliver(
        sender,
        AgentJobEvent::Finished {
            cancelled,
            error: if cancelled { None } else { Some(error) },
        },
        cancel,
    );
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
    use crate::agent_contract::AgentEventKind;
    use crate::agent_runtime::{
        AgentJobEvent, PermissionDecision, new_session_id, work_packet_for_goal,
    };
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
        write({"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"hello "}}}})
        write({"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"world"}}}})
        # Need-you: request permission and wait for client response before finishing.
        write({"jsonrpc":"2.0","id":9001,"method":"session/request_permission","params":{
            "sessionId":"acp-test-session",
            "toolCall":{"title":"edit src/lib.rs"},
            "options":[
                {"optionId":"allow-once","name":"Allow once","kind":"allow_once"},
                {"optionId":"reject-once","name":"Reject once","kind":"reject_once"}
            ]
        }})
        # block until permission response
        while True:
            resp = read()
            if resp is None:
                break
            if resp.get("id") == 9001:
                break
        write({"jsonrpc":"2.0","method":"session/update","params":{"update":{
            "sessionUpdate":"tool_call",
            "title":"edit src/lib.rs",
            "kind":"edit",
            "locations":[{"path":"src/lib.rs"}],
            "rawInput":{"path":"src/lib.rs"}
        }}})
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
        let mut saw_permission = false;
        loop {
            match port.try_recv() {
                Ok(AgentJobEvent::Event(event)) => {
                    coordinator.admit(event).unwrap();
                }
                Ok(AgentJobEvent::PermissionNeeded(pending)) => {
                    saw_permission = true;
                    assert!(pending.options.iter().any(|option| option.is_allow()));
                    job.reply_permission(PermissionDecision::Select {
                        option_id: "allow-once".to_owned(),
                    })
                    .unwrap();
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
        assert!(saw_permission, "expected session/request_permission");
        assert_eq!(coordinator.run_state(), AgentRunState::Review);
        assert!(coordinator.receipt().len() >= 3);
        let receipt = coordinator.receipt();
        assert!(
            receipt.iter().any(|event| {
                event.kind == AgentEventKind::PathTouched
                    && event
                        .path
                        .as_ref()
                        .is_some_and(|path| path.ends_with("src/lib.rs"))
            }),
            "expected path_touched for src/lib.rs; receipt={receipt:?}"
        );
        assert!(
            receipt.iter().any(|event| {
                event.kind == AgentEventKind::Notice && event.summary.contains("agent:")
            }),
            "expected coalesced agent message chunk; receipt={receipt:?}"
        );
    }

    #[test]
    fn relativize_strips_cwd_and_file_uri() {
        let cwd = PathBuf::from("/tmp/project");
        assert_eq!(
            relativize_workspace_path("src/main.rs", &cwd).unwrap(),
            PathBuf::from("src/main.rs")
        );
        assert_eq!(
            relativize_workspace_path("/tmp/project/src/lib.rs", &cwd).unwrap(),
            PathBuf::from("src/lib.rs")
        );
        assert_eq!(
            relativize_workspace_path("file:///tmp/project/foo.rs", &cwd).unwrap(),
            PathBuf::from("foo.rs")
        );
        assert!(relativize_workspace_path("/other/place.rs", &cwd).is_none());
        assert!(relativize_workspace_path("../escape.rs", &cwd).is_none());
    }

    #[test]
    fn extract_tool_paths_from_locations_and_raw_input() {
        let cwd = PathBuf::from("/ws");
        let update = json!({
            "title": "edit",
            "locations": [{"path": "/ws/src/a.rs"}, {"path": "src/b.rs"}],
            "rawInput": {"path": "src/c.rs"}
        });
        let paths = extract_tool_paths(&update, &cwd);
        assert!(paths.iter().any(|path| path == Path::new("src/a.rs")));
        assert!(paths.iter().any(|path| path == Path::new("src/b.rs")));
        assert!(paths.iter().any(|path| path == Path::new("src/c.rs")));
    }
}
