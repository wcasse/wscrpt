//! Pi Coding Agent RPC bridge (`pi --mode rpc`).
//!
//! Speaks Pi's JSONL command/event protocol over stdio (not ACP JSON-RPC).
//! Always loads the in-repo permission gate via `--extension` so tool calls can
//! surface as `extension_ui_request` → Needs You. Phase 2 answers pending
//! confirms fail-closed (deny) until the approve chord lands in Phase 4.
//!
//! Reference: https://pi.dev/ docs for RPC mode.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::agent::sticky_artifact_ref;
use crate::agent_auth::{default_pi_argv, pi_argv_with_permission_gate, pi_permission_gate_path};
use crate::agent_contract::{
    AgentEvent, AgentEventKind, AgentRunState, MAX_SUMMARY_BYTES, unix_now_ms,
};
use crate::agent_runtime::{AgentEventPort, AgentJob, AgentJobEvent};

const EVENT_CAPACITY: usize = 64;
const MAX_RECEIPT_NOTICES: usize = 64;
const MAX_ASSISTANT_CHARS: usize = 2 * 1024;
const RUN_WALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const READ_IDLE_TIMEOUT: Duration = Duration::from_millis(200);

/// Inputs for one Pi RPC process run.
#[derive(Clone, Debug)]
pub struct PiRunRequest {
    pub workspace_id: u64,
    pub session_id: String,
    pub generation: u64,
    pub cwd: PathBuf,
    /// Base argv (typically `pi --mode rpc`). Gate `--extension` is injected.
    pub argv: Vec<String>,
    pub goal: String,
    pub sticky_brief: Option<String>,
    pub sticky_id: Option<String>,
    /// Optional `provider/modelId` applied via `set_model` before prompt.
    pub model: Option<String>,
}

/// True when config should use the Pi RPC bridge (not ACP).
pub fn is_pi_profile(profile: &str, argv: &[String]) -> bool {
    let profile = profile.to_ascii_lowercase();
    if profile == "pi" || profile == "pi-rpc" {
        return true;
    }
    // argv: pi --mode rpc  (or npx … pi-coding-agent --mode rpc)
    let joined = argv.join(" ").to_ascii_lowercase();
    joined.contains("--mode rpc") || joined.contains("--mode=rpc")
}

/// Resolve argv for a Pi run: defaults + permission gate injection.
pub fn resolve_pi_argv(profile: &str, configured: &[String]) -> Result<Vec<String>, String> {
    let base = if configured.is_empty() && is_pi_profile(profile, configured) {
        default_pi_argv()
    } else if configured.is_empty() {
        return Err("agent.argv is empty".to_owned());
    } else {
        configured.to_vec()
    };
    let argv = pi_argv_with_permission_gate(&base);
    if !argv.iter().any(|a| a == "--extension" || a == "-e") {
        return Err(format!(
            "Pi permission gate not found (expected {rel}). Set WSCRPT_PI_PERMISSION_GATE or run from the wscrpt tree.",
            rel = crate::agent_auth::PI_PERMISSION_GATE_REL
        ));
    }
    Ok(argv)
}

/// Spawn Pi RPC and stream bounded events into the Agents dashboard.
pub fn spawn_pi_agent(request: PiRunRequest) -> Result<(AgentJob, AgentEventPort), String> {
    let argv = if request.argv.iter().any(|a| a == "--extension" || a == "-e") {
        request.argv.clone()
    } else {
        resolve_pi_argv("pi", &request.argv)?
    };
    if argv.is_empty() {
        return Err("agent.argv is empty".to_owned());
    }

    let program = argv[0].clone();
    let args = argv[1..].to_vec();
    let cwd = canonicalize_dir(&request.cwd)?;

    let mut command = Command::new(&program);
    command
        .args(&args)
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Avoid interactive trust prompts hanging the TUI when possible.
    command.env("PI_SKIP_VERSION_CHECK", "1");
    configure_process_group(&mut command);

    let mut child = command.spawn().map_err(|source| {
        format!(
            "failed to spawn Pi `{}`: {source} (install Pi · docs/AGENT_AUTH.md)",
            argv.join(" ")
        )
    })?;

    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| "Pi process has no stdin pipe".to_owned())?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Pi process has no stdout pipe".to_owned())?;
    if let Some(stderr) = child.stderr.take() {
        thread::Builder::new()
            .name("wscrpt-agent-pi-stderr".to_owned())
            .spawn(move || {
                let reader = BufReader::new(stderr);
                for _line in reader.lines() {
                    // Discard: provider noise / secrets must not enter receipts.
                }
            })
            .ok();
    }

    let (sender, receiver) = std::sync::mpsc::sync_channel(EVENT_CAPACITY);
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_flag = Arc::clone(&cancel);
    let mut request = request;
    request.argv = argv;
    let handle = thread::Builder::new()
        .name("wscrpt-agent-pi".to_owned())
        .spawn(move || {
            run_pi_session(request, child, stdin, stdout, sender, cancel_flag);
        })
        .map_err(|source| format!("failed to start Pi worker thread: {source}"))?;

    Ok((
        AgentJob::from_parts(cancel, handle),
        AgentEventPort::from_receiver(receiver),
    ))
}

fn run_pi_session(
    request: PiRunRequest,
    mut child: Child,
    mut stdin: impl Write,
    stdout: impl std::io::Read + Send + 'static,
    sender: SyncSender<AgentJobEvent>,
    cancel: Arc<AtomicBool>,
) {
    let mut seq = 0u64;
    let mut notices = 0usize;
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

    let gate_note = pi_permission_gate_path()
        .map(|p| format!("gate {}", p.display()))
        .unwrap_or_else(|| "gate unresolved".to_owned());
    let _ = emit(
        AgentEventKind::State,
        format!("Pi RPC: {} · {gate_note}", request.argv.join(" ")),
        Some(AgentRunState::Working),
        None,
        None,
        None,
    );

    if let Some(title_brief) = request.sticky_brief.as_ref() {
        let title = title_brief.lines().next().unwrap_or("sticky").to_owned();
        let artifact = request.sticky_id.as_deref().map(sticky_artifact_ref);
        let _ = emit(
            AgentEventKind::Notice,
            format!("sticky brief: {title}"),
            None,
            None,
            None,
            artifact,
        );
    }

    // Optional model select before the prompt.
    if let Some(model_spec) = request.model.as_ref()
        && let Some((provider, model_id)) = split_provider_model(model_spec)
        && let Err(error) = write_pi_command(
            &mut stdin,
            json!({
                "type": "set_model",
                "provider": provider,
                "modelId": model_id,
            }),
        )
    {
        finish_error(&sender, &mut child, &cancel, error);
        return;
    }

    let prompt = build_prompt(&request.goal, request.sticky_brief.as_deref());
    if let Err(error) = write_pi_command(
        &mut stdin,
        json!({
            "type": "prompt",
            "message": prompt,
        }),
    ) {
        finish_error(&sender, &mut child, &cancel, error);
        return;
    }

    // Read stdout on a helper thread into a channel so we can poll cancel.
    let (line_tx, line_rx) = std::sync::mpsc::sync_channel::<Result<String, String>>(256);
    thread::Builder::new()
        .name("wscrpt-agent-pi-stdout".to_owned())
        .spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(text) => {
                        if line_tx.send(Ok(text)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        let _ = line_tx.send(Err(error.to_string()));
                        break;
                    }
                }
            }
        })
        .ok();

    let started = Instant::now();
    let mut assistant = String::new();
    let mut settled = false;
    let mut paths_touched: Vec<PathBuf> = Vec::new();

    loop {
        if cancel.load(Ordering::Acquire) {
            let _ = write_pi_command(&mut stdin, json!({"type": "abort"}));
            terminate_child(&mut child);
            let _ = sender.send(AgentJobEvent::Finished {
                cancelled: true,
                error: None,
            });
            return;
        }
        if started.elapsed() > RUN_WALL_TIMEOUT {
            let _ = write_pi_command(&mut stdin, json!({"type": "abort"}));
            finish_error(
                &sender,
                &mut child,
                &cancel,
                "Pi run exceeded wall-clock limit (30m)".to_owned(),
            );
            return;
        }

        match line_rx.recv_timeout(READ_IDLE_TIMEOUT) {
            Ok(Ok(line)) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let value: Value = match serde_json::from_str(line) {
                    Ok(v) => v,
                    Err(_) => {
                        // Non-JSON noise — ignore.
                        continue;
                    }
                };
                if !handle_pi_event(
                    &value,
                    &mut emit,
                    &mut stdin,
                    &mut notices,
                    &mut assistant,
                    &mut paths_touched,
                    &mut settled,
                ) {
                    // Channel closed to App.
                    terminate_child(&mut child);
                    return;
                }
                if settled {
                    break;
                }
            }
            Ok(Err(error)) => {
                finish_error(&sender, &mut child, &cancel, format!("Pi stdout: {error}"));
                return;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // Check whether the child exited without a clean settle.
                match child.try_wait() {
                    Ok(Some(status)) => {
                        if !settled {
                            if status.success() {
                                let _ = emit(
                                    AgentEventKind::ReviewReady,
                                    "Pi process exited — review host changes".to_owned(),
                                    Some(AgentRunState::Review),
                                    None,
                                    None,
                                    None,
                                );
                            } else {
                                finish_error(
                                    &sender,
                                    &mut child,
                                    &cancel,
                                    format!("Pi exited with {status}"),
                                );
                                return;
                            }
                        }
                        break;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        finish_error(
                            &sender,
                            &mut child,
                            &cancel,
                            format!("Pi wait failed: {error}"),
                        );
                        return;
                    }
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if !settled {
                    let _ = emit(
                        AgentEventKind::ReviewReady,
                        "Pi stream ended — review host changes".to_owned(),
                        Some(AgentRunState::Review),
                        None,
                        None,
                        None,
                    );
                }
                break;
            }
        }
    }

    if !assistant.is_empty() && notices < MAX_RECEIPT_NOTICES {
        let _ = emit(
            AgentEventKind::Notice,
            format!("assistant: {}", truncate(&assistant, 240)),
            None,
            None,
            None,
            None,
        );
    }

    // Best-effort teardown.
    let _ = write_pi_command(&mut stdin, json!({"type": "abort"}));
    terminate_child(&mut child);
    let _ = sender.send(AgentJobEvent::Finished {
        cancelled: cancel.load(Ordering::Acquire),
        error: None,
    });
}

/// Map one Pi JSONL object into coordinator events / UI replies.
///
/// Returns false if the App event channel is closed.
fn handle_pi_event(
    value: &Value,
    emit: &mut impl FnMut(
        AgentEventKind,
        String,
        Option<AgentRunState>,
        Option<bool>,
        Option<PathBuf>,
        Option<String>,
    ) -> bool,
    stdin: &mut impl Write,
    notices: &mut usize,
    assistant: &mut String,
    paths_touched: &mut Vec<PathBuf>,
    settled: &mut bool,
) -> bool {
    let kind = value.get("type").and_then(Value::as_str).unwrap_or("");

    match kind {
        "response" => {
            // Command ack: surface failures only.
            if value.get("success").and_then(Value::as_bool) == Some(false) {
                let cmd = value
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or("command");
                let error = value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("failed");
                if *notices < MAX_RECEIPT_NOTICES {
                    *notices += 1;
                    return emit(
                        AgentEventKind::Notice,
                        format!("Pi {cmd}: {error}"),
                        None,
                        None,
                        None,
                        None,
                    );
                }
            }
            true
        }
        "agent_start" => emit(
            AgentEventKind::State,
            "Pi agent working".to_owned(),
            Some(AgentRunState::Working),
            None,
            None,
            None,
        ),
        "agent_settled" => {
            *settled = true;
            emit(
                AgentEventKind::ReviewReady,
                "Pi settled — review changes".to_owned(),
                Some(AgentRunState::Review),
                None,
                None,
                None,
            )
        }
        "agent_end" => {
            // May still retry; only mark review if willRetry is false.
            let will_retry = value
                .get("willRetry")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !will_retry {
                *settled = true;
                emit(
                    AgentEventKind::ReviewReady,
                    "Pi run complete — review changes".to_owned(),
                    Some(AgentRunState::Review),
                    None,
                    None,
                    None,
                )
            } else {
                true
            }
        }
        "tool_execution_start" => {
            let name = value
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let args = value.get("args").cloned().unwrap_or(Value::Null);
            let summary = format!(
                "tool start: {} · {}",
                name,
                summarize_tool_args(name, &args)
            );
            if *notices < MAX_RECEIPT_NOTICES {
                *notices += 1;
                emit(AgentEventKind::Notice, summary, None, None, None, None)
            } else {
                true
            }
        }
        "tool_execution_end" => {
            let name = value
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let is_error = value
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let args = value.get("args").cloned().unwrap_or(Value::Null);
            if let Some(path) = path_from_tool(name, &args) {
                if !paths_touched.iter().any(|p| p == &path) {
                    paths_touched.push(path.clone());
                }
                if !emit(
                    AgentEventKind::PathTouched,
                    format!("touched {}", path.display()),
                    None,
                    None,
                    Some(path),
                    None,
                ) {
                    return false;
                }
            }
            if *notices < MAX_RECEIPT_NOTICES {
                *notices += 1;
                let label = if is_error { "tool error" } else { "tool done" };
                emit(
                    AgentEventKind::Notice,
                    format!("{label}: {name}"),
                    None,
                    Some(!is_error),
                    None,
                    None,
                )
            } else {
                true
            }
        }
        "message_update" => {
            if let Some(delta) = value
                .pointer("/assistantMessageEvent/delta")
                .and_then(Value::as_str)
            {
                let event_type = value
                    .pointer("/assistantMessageEvent/type")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let is_text = event_type == "text_delta"
                    || (event_type.ends_with("delta") && !event_type.contains("thinking"));
                if is_text && assistant.len() < MAX_ASSISTANT_CHARS {
                    let room = MAX_ASSISTANT_CHARS.saturating_sub(assistant.len());
                    assistant.push_str(&delta.chars().take(room).collect::<String>());
                }
            }
            true
        }
        "message_end" => {
            if let Some(text) = extract_assistant_text(value)
                && assistant.is_empty()
            {
                *assistant = truncate(&text, MAX_ASSISTANT_CHARS);
            }
            true
        }
        "extension_ui_request" => {
            // Phase 2: fail-closed deny until Esc w A approve lands (Phase 4).
            let id = value.get("id").cloned().unwrap_or(Value::Null);
            let method = value
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("confirm");
            let title = value
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("Permission");
            let detail = value
                .get("message")
                .or_else(|| value.get("detail"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let summary = if detail.is_empty() {
                format!("Needs You: {title} (auto-deny until approve chord)")
            } else {
                format!(
                    "Needs You: {title} — {} (auto-deny until approve chord)",
                    truncate(detail, 120)
                )
            };
            if !emit(
                AgentEventKind::Approval,
                summary,
                Some(AgentRunState::NeedsYou),
                None,
                None,
                None,
            ) {
                return false;
            }
            // Answer immediately fail-closed so the agent can continue or adjust.
            let response = match method {
                "confirm" => json!({
                    "type": "extension_ui_response",
                    "id": id,
                    "confirmed": false,
                }),
                "select" | "input" | "editor" => json!({
                    "type": "extension_ui_response",
                    "id": id,
                    "cancelled": true,
                }),
                // Fire-and-forget methods need no response.
                _ => return true,
            };
            if write_pi_command(stdin, response).is_err() {
                return false;
            }
            true
        }
        "extension_error" => {
            let error = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("extension error");
            if *notices < MAX_RECEIPT_NOTICES {
                *notices += 1;
                emit(
                    AgentEventKind::Notice,
                    format!("extension: {error}"),
                    None,
                    None,
                    None,
                    None,
                )
            } else {
                true
            }
        }
        "auto_retry_start" if *notices < MAX_RECEIPT_NOTICES => {
            *notices += 1;
            let attempt = value.get("attempt").and_then(Value::as_u64).unwrap_or(0);
            emit(
                AgentEventKind::Notice,
                format!("Pi retrying (attempt {attempt})"),
                Some(AgentRunState::Working),
                None,
                None,
                None,
            )
        }
        "auto_retry_start" => true,
        "compaction_start"
        | "compaction_end"
        | "turn_start"
        | "turn_end"
        | "tool_execution_update"
        | "message_start"
        | "queue_update"
        | "bash_execution_update"
        | "auto_retry_end" => true,
        other if !other.is_empty() && *notices < MAX_RECEIPT_NOTICES => {
            *notices += 1;
            emit(
                AgentEventKind::Notice,
                format!("Pi event: {other}"),
                None,
                None,
                None,
                None,
            )
        }
        _ => true,
    }
}

fn build_prompt(goal: &str, sticky_brief: Option<&str>) -> String {
    let goal = goal.trim();
    match sticky_brief {
        Some(brief) if !brief.trim().is_empty() => {
            format!(
                "{goal}\n\n---\nSticky brief (user-attached working notes):\n{}\n",
                brief.trim()
            )
        }
        _ => goal.to_owned(),
    }
}

fn write_pi_command(stdin: &mut impl Write, value: Value) -> Result<(), String> {
    let line = serde_json::to_string(&value).map_err(|e| e.to_string())?;
    writeln!(stdin, "{line}").map_err(|e| format!("write Pi stdin: {e}"))?;
    stdin.flush().map_err(|e| format!("flush Pi stdin: {e}"))?;
    Ok(())
}

fn split_provider_model(spec: &str) -> Option<(&str, &str)> {
    let spec = spec.trim();
    let (provider, model) = spec.split_once('/')?;
    if provider.is_empty() || model.is_empty() {
        return None;
    }
    Some((provider, model))
}

fn summarize_tool_args(tool_name: &str, args: &Value) -> String {
    match tool_name {
        "bash" => {
            let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
            truncate(cmd, 100)
        }
        "write" | "edit" | "read" | "remove" | "move" | "rename" => path_from_tool(tool_name, args)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| truncate(&args.to_string(), 80)),
        _ => truncate(&args.to_string(), 80),
    }
}

fn path_from_tool(tool_name: &str, args: &Value) -> Option<PathBuf> {
    let key = match tool_name {
        "write" | "edit" | "read" | "remove" => "path",
        "move" | "rename" => "to",
        _ => return None,
    };
    let raw = args
        .get(key)
        .or_else(|| args.get("file_path"))
        .or_else(|| args.get("path"))
        .and_then(Value::as_str)?;
    let path = PathBuf::from(raw);
    // Prefer relative paths for the coordinator; skip absolute-out-of-tree later at admit.
    Some(path)
}

fn extract_assistant_text(value: &Value) -> Option<String> {
    let message = value.get("message")?;
    if message.get("role").and_then(Value::as_str) != Some("assistant") {
        return None;
    }
    let content = message.get("content")?;
    if let Some(s) = content.as_str() {
        return Some(s.to_owned());
    }
    let arr = content.as_array()?;
    let mut out = String::new();
    for block in arr {
        if block.get("type").and_then(Value::as_str) == Some("text")
            && let Some(text) = block.get("text").and_then(Value::as_str)
        {
            out.push_str(text);
        }
    }
    if out.is_empty() { None } else { Some(out) }
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

fn truncate(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max {
        return trimmed.to_owned();
    }
    let mut out: String = trimmed.chars().take(max.saturating_sub(1)).collect();
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
    use std::sync::Mutex;

    #[test]
    fn is_pi_profile_detects_label_and_argv() {
        assert!(is_pi_profile("pi", &[]));
        assert!(is_pi_profile("PI-RPC", &[]));
        assert!(is_pi_profile(
            "custom",
            &["pi".into(), "--mode".into(), "rpc".into()]
        ));
        assert!(!is_pi_profile(
            "grok",
            &["grok".into(), "agent".into(), "stdio".into()]
        ));
    }

    #[test]
    fn resolve_pi_argv_injects_gate() {
        let argv = resolve_pi_argv("pi", &[]).expect("gate in package tree");
        assert_eq!(argv[0], "pi");
        assert!(argv.windows(2).any(|w| w[0] == "--extension"));
        assert!(argv.iter().any(|a| a.contains("wscrpt-permission-gate.ts")));
    }

    #[test]
    fn build_prompt_includes_sticky() {
        let p = build_prompt("ship it", Some("title: notes\nbody"));
        assert!(p.contains("ship it"));
        assert!(p.contains("Sticky brief"));
        assert!(p.contains("body"));
    }

    #[test]
    fn handle_tool_end_emits_path_touched() {
        let events = Mutex::new(Vec::new());
        let mut notices = 0usize;
        let mut assistant = String::new();
        let mut paths = Vec::new();
        let mut settled = false;
        let mut stdin = Vec::new();
        let mut emit = |kind: AgentEventKind,
                        summary: String,
                        run_state: Option<AgentRunState>,
                        _check: Option<bool>,
                        path: Option<PathBuf>,
                        _art: Option<String>| {
            events
                .lock()
                .unwrap()
                .push((kind, summary, run_state, path));
            true
        };
        let value = json!({
            "type": "tool_execution_end",
            "toolName": "write",
            "args": {"path": "src/lib.rs"},
            "isError": false
        });
        assert!(handle_pi_event(
            &value,
            &mut emit,
            &mut stdin,
            &mut notices,
            &mut assistant,
            &mut paths,
            &mut settled,
        ));
        let locked = events.lock().unwrap();
        assert!(
            locked
                .iter()
                .any(|(k, _, _, p)| *k == AgentEventKind::PathTouched
                    && p.as_ref().is_some_and(|p| p.ends_with("src/lib.rs")))
        );
    }

    #[test]
    fn extension_ui_confirm_is_denied_fail_closed() {
        let events = Mutex::new(Vec::new());
        let mut notices = 0usize;
        let mut assistant = String::new();
        let mut paths = Vec::new();
        let mut settled = false;
        let mut stdin: Vec<u8> = Vec::new();
        let mut emit = |kind: AgentEventKind,
                        summary: String,
                        run_state: Option<AgentRunState>,
                        _check: Option<bool>,
                        path: Option<PathBuf>,
                        _art: Option<String>| {
            events
                .lock()
                .unwrap()
                .push((kind, summary, run_state, path));
            true
        };
        let value = json!({
            "type": "extension_ui_request",
            "id": "uuid-1",
            "method": "confirm",
            "title": "Allow tool?",
            "message": "bash: cargo test"
        });
        assert!(handle_pi_event(
            &value,
            &mut emit,
            &mut stdin,
            &mut notices,
            &mut assistant,
            &mut paths,
            &mut settled,
        ));
        let locked = events.lock().unwrap();
        assert!(
            locked
                .iter()
                .any(|(k, s, st, _)| *k == AgentEventKind::Approval
                    && st == &Some(AgentRunState::NeedsYou)
                    && s.contains("Allow tool"))
        );
        let written = String::from_utf8(stdin).unwrap();
        assert!(written.contains("extension_ui_response"));
        assert!(written.contains("\"confirmed\":false"));
    }

    #[test]
    fn agent_settled_marks_review() {
        let events = Mutex::new(Vec::new());
        let mut notices = 0usize;
        let mut assistant = String::new();
        let mut paths = Vec::new();
        let mut settled = false;
        let mut stdin = Vec::new();
        let mut emit = |kind: AgentEventKind,
                        summary: String,
                        run_state: Option<AgentRunState>,
                        _check: Option<bool>,
                        path: Option<PathBuf>,
                        _art: Option<String>| {
            events
                .lock()
                .unwrap()
                .push((kind, summary, run_state, path));
            true
        };
        assert!(handle_pi_event(
            &json!({"type": "agent_settled"}),
            &mut emit,
            &mut stdin,
            &mut notices,
            &mut assistant,
            &mut paths,
            &mut settled,
        ));
        assert!(settled);
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|(k, _, st, _)| *k == AgentEventKind::ReviewReady
                    && st == &Some(AgentRunState::Review))
        );
    }

    #[test]
    fn split_provider_model_works() {
        assert_eq!(
            split_provider_model("anthropic/claude-sonnet-4"),
            Some(("anthropic", "claude-sonnet-4"))
        );
        assert_eq!(split_provider_model("nope"), None);
    }
}
