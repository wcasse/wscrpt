use std::env;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use crossterm::ExecutableCommand;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyModifiers,
};
use crossterm::terminal;

use wscrpt::Workspace;
use wscrpt::app::App;
use wscrpt::config::Config;
use wscrpt::render::Renderer;
use wscrpt::session::{Session, SessionStore};
use wscrpt::terminal::TerminalSession;

fn default_config_text() -> String {
    let mut text = String::from(
        r#"# ~/.config/wscrpt/config.toml
tab_width = 4
insert_spaces = true
line_numbers = true
mouse = false
scroll_margin = 3
osc52_copy = true
# When true, Save requests LSP document formatting before writing to disk.
format_on_save = false

# Language servers are launched only from this user-owned global config.
# `wscrpt --health` reports well-known servers found on PATH; it never auto-enables
# them. Uncomment an entry only after installing and choosing the executable.

"#,
    );
    let discovered = wscrpt::lsp_discover::discover_language_servers();
    text.push_str(&wscrpt::lsp_discover::suggested_config_snippets(
        &discovered,
    ));
    text
}
const MAX_INPUT_DIAGNOSTIC_EVENTS: usize = 512;

#[derive(Debug, Parser)]
#[command(
    name = "wscrpt",
    version,
    about = "wscrpt — a remote-first terminal editor and streamlined IDE"
)]
struct Cli {
    /// File or workspace directory to open.
    path: Option<PathBuf>,

    /// Override the discovered workspace root.
    #[arg(long, value_name = "DIR")]
    project: Option<PathBuf>,

    /// Enable terminal mouse reporting. Off by default for Blink touch selection.
    #[arg(long, conflicts_with = "no_mouse")]
    mouse: bool,

    /// Disable terminal mouse reporting even when enabled in config.
    #[arg(long)]
    no_mouse: bool,

    /// Keep copies in the editor register without attempting OSC 52.
    #[arg(long)]
    no_osc52: bool,

    /// Print a starter configuration and exit.
    #[arg(long)]
    print_default_config: bool,

    /// Print the generated Markdown command reference and exit.
    #[arg(long)]
    print_command_reference: bool,

    /// Print remote-terminal and tool diagnostics and exit.
    #[arg(long)]
    health: bool,

    /// Show decoded terminal input events for the current route and exit on Ctrl-G/Ctrl-C.
    #[arg(long)]
    input_diagnostics: bool,

    /// Do not restore or persist the last workspace session.
    #[arg(long)]
    no_session: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.print_default_config {
        print!("{}", default_config_text());
        return Ok(());
    }
    if cli.print_command_reference {
        print!("{}", wscrpt::keymap::command_reference_markdown());
        return Ok(());
    }
    if cli.health {
        print_health();
        return Ok(());
    }
    if cli.input_diagnostics {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            bail!("wscrpt --input-diagnostics needs an interactive terminal");
        }
        run_input_diagnostics().context("input diagnostics failed")?;
        return Ok(());
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!(
            "wscrpt needs an interactive terminal; use `wscrpt --health` for a non-interactive check"
        );
    }

    let (mut config, _) = Config::load().map_err(anyhow::Error::msg)?;
    if cli.mouse {
        config.mouse = true;
    }
    if cli.no_mouse {
        config.mouse = false;
    }
    if cli.no_osc52 {
        config.osc52_copy = false;
    }

    let explicit_launch = cli.path.is_some() || cli.project.is_some();
    let (file, root) = launch_paths(cli.path, cli.project)?;
    let mut restored_layout = None;
    let mut restored_recent_files = None;
    let mut restored_bookmarks = None;
    let mut startup_notice = None;
    let workspace = if !cli.no_session && !explicit_launch {
        match SessionStore::from_env().and_then(|store| store.load()) {
            Ok(Some(session)) => match restore_workspace(&session) {
                Ok((workspace, skipped)) => {
                    restored_layout = Some(session.layout);
                    restored_recent_files = Some(session.recent_files.clone());
                    restored_bookmarks = Some(session.bookmarks.clone());
                    startup_notice = Some((
                        if skipped == 0 {
                            format!("Restored workspace {}", session.root.display())
                        } else {
                            format!(
                                "Restored workspace {}; skipped {skipped} unavailable file{}",
                                session.root.display(),
                                if skipped == 1 { "" } else { "s" }
                            )
                        },
                        skipped > 0,
                    ));
                    workspace
                }
                Err(error) => {
                    startup_notice = Some((format!("Session restore skipped: {error}"), true));
                    Workspace::from_path(file, root)?
                }
            },
            Ok(None) => Workspace::from_path(file, root)?,
            Err(error) => {
                startup_notice = Some((format!("Session restore skipped: {error}"), true));
                Workspace::from_path(file, root)?
            }
        }
    } else {
        Workspace::from_path(file, root)?
    };
    let mut app = App::new(workspace, config.clone());
    if cli.no_session {
        app.disable_session_persistence();
    }
    if let Some(layout) = restored_layout {
        app.apply_session_layout(layout);
    }
    if let Some(recent_files) = restored_recent_files {
        app.apply_session_recent_files(recent_files);
    }
    if let Some(bookmarks) = restored_bookmarks {
        app.apply_session_bookmarks(bookmarks);
    }
    if let Some((notice, error)) = startup_notice {
        app.startup_notice(notice, error);
    }
    app.maybe_open_first_run_help();
    let mut terminal = TerminalSession::new(config.mouse).context("could not enter terminal UI")?;
    let mut renderer = Renderer::default();
    let mut size = terminal.size().context("could not read terminal size")?;
    app.set_screen_size(size);
    app.start_background_services();
    // Remote-first frame budget: coalesce input and avoid painting faster than
    // mosh/Blink can usefully display. Background services are throttled inside
    // App; this caps input-driven paints as well.
    const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(16); // ~60 fps ceiling
    const MAX_EVENTS_PER_WAKE: usize = 64;
    const IDLE_POLL: Duration = Duration::from_millis(100);
    const ACTIVE_POLL: Duration = Duration::from_millis(20);
    let mut redraw = true;
    let mut last_recovery = Instant::now();
    // The first frame must never wait behind the remote-input frame budget. In
    // particular, a queued Ctrl-Q must not let the process exit before a tiny
    // terminal receives its first usable frame.
    let mut last_frame = Instant::now() - MIN_FRAME_INTERVAL;
    let perf_enabled = env::var_os("WSCRPT_PERF").is_some();

    loop {
        if app.should_quit() {
            break;
        }
        if redraw && last_frame.elapsed() >= MIN_FRAME_INTERVAL {
            if app.take_full_redraw() {
                renderer.invalidate();
            }
            let frame_started = Instant::now();
            let paint = renderer
                .draw(&mut terminal, &mut app, size)
                .context("could not draw frame")?;
            let control_output = app.take_terminal_output();
            if !control_output.is_empty() {
                terminal.write_all(&control_output)?;
                terminal.flush()?;
            }
            if perf_enabled {
                app.set_perf_stats(frame_started.elapsed(), paint);
            }
            last_frame = Instant::now();
            redraw = false;
        }

        let poll_timeout = if app.wants_frequent_poll() {
            ACTIVE_POLL
        } else {
            IDLE_POLL
        };
        // If a frame is pending only because of the frame budget, wake sooner.
        let poll_timeout = if redraw {
            poll_timeout.min(MIN_FRAME_INTERVAL.saturating_sub(last_frame.elapsed()))
        } else {
            poll_timeout
        };

        if event::poll(poll_timeout).context("could not poll terminal input")? {
            // Coalesce a burst of keys/mouse/resizes into one logical wake so
            // mosh typing does not force one full frame per byte.
            let mut events = 0_usize;
            loop {
                let terminal_event = event::read().context("could not read terminal input")?;
                if let event::Event::Resize(width, height) = terminal_event {
                    size = (width, height);
                }
                app.handle_event(terminal_event);
                events += 1;
                if events >= MAX_EVENTS_PER_WAKE
                    || !event::poll(Duration::ZERO).context("could not drain terminal input")?
                {
                    break;
                }
            }
            redraw = true;
        }
        if last_recovery.elapsed() >= Duration::from_secs(2) {
            redraw |= app.checkpoint_recovery();
            redraw |= app.checkpoint_session();
            last_recovery = Instant::now();
        }
        redraw |= app.poll_ui_transients();
        redraw |= app.poll_services();

        if app.take_terminal_request() {
            let _ = app.checkpoint_recovery();
            let _ = app.checkpoint_session();
            terminal
                .restore()
                .context("could not release terminal for workspace shell")?;
            let outcome = run_workspace_shell(app.workspace_root());
            terminal = TerminalSession::new(config.mouse)
                .context("could not re-enter terminal UI after workspace shell")?;
            size = terminal
                .size()
                .context("could not read terminal size after workspace shell")?;
            app.set_screen_size(size);
            app.terminal_returned(outcome);
            renderer.invalidate();
            redraw = true;
        }
    }

    let _ = app.checkpoint_session();
    terminal.restore().context("could not restore terminal")?;
    Ok(())
}

fn restore_workspace(session: &Session) -> Result<(Workspace, usize)> {
    if !session.root.is_dir() {
        bail!(
            "workspace root no longer exists: {}",
            session.root.display()
        );
    }

    let mut workspace = Workspace::new(Some(session.root.clone()))
        .context("could not create restored workspace")?;
    let mut skipped = 0;
    let mut restored_active = None;
    for (session_index, state) in session.open_files.iter().enumerate() {
        if !state.path.is_file() {
            skipped += 1;
            continue;
        }
        let Ok(workspace_index) = workspace.open(&state.path) else {
            skipped += 1;
            continue;
        };
        let mut editor = workspace
            .editor_mut(workspace_index)
            .expect("the restored workspace buffer was just opened");
        editor.cursor = state.cursor.min(editor.document.len_chars());
        editor.anchor = state
            .anchor
            .filter(|anchor| *anchor <= editor.document.len_chars());
        editor.viewport.top_line = state
            .viewport
            .top_line
            .min(editor.document.line_count().saturating_sub(1));
        let viewport_line_start = editor.document.line_start_char(editor.viewport.top_line);
        let viewport_line_end = editor.document.line_end_char(editor.viewport.top_line);
        editor.viewport.top_wrap_char = state
            .viewport
            .top_wrap_char
            .min(viewport_line_end.saturating_sub(viewport_line_start));
        editor.viewport.left_column = state.viewport.left_column;
        if session_index == session.active_index {
            restored_active = Some(workspace_index);
        }
    }
    if let Some(index) = restored_active {
        workspace.activate(index);
    }
    Ok((workspace, skipped))
}

fn run_workspace_shell(root: &Path) -> Result<ExitStatus, String> {
    let shell = env::var_os("SHELL")
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| "/bin/sh".into());
    Command::new(&shell)
        .current_dir(root)
        .env("WSCRPT_WORKSPACE", root)
        .status()
        .map_err(|error| format!("{}: {error}", PathBuf::from(shell).display()))
}

fn launch_paths(
    path: Option<PathBuf>,
    explicit_root: Option<PathBuf>,
) -> Result<(Option<PathBuf>, Option<PathBuf>)> {
    match path {
        Some(path) if path.is_dir() => {
            if explicit_root.is_some() {
                bail!("pass either a workspace directory or --project, not both");
            }
            Ok((None, Some(path)))
        }
        path => Ok((path, explicit_root)),
    }
}

fn print_health() {
    let term = env::var("TERM").unwrap_or_else(|_| "(unset)".to_owned());
    let locale = env::var("LC_ALL")
        .or_else(|_| env::var("LC_CTYPE"))
        .or_else(|_| env::var("LANG"))
        .unwrap_or_else(|_| "(unset)".to_owned());
    let transport = if env::var_os("MOSH_IP").is_some() || env::var_os("MOSH_CONNECTION").is_some()
    {
        "mosh"
    } else if env::var_os("SSH_CONNECTION").is_some() {
        "ssh"
    } else {
        "local/unknown"
    };
    let tmux = env::var_os("TMUX").is_some();

    println!("wscrpt {}", env!("CARGO_PKG_VERSION"));
    println!("TERM={term}");
    println!("locale={locale}");
    println!("transport={transport}");
    println!("tmux={}", if tmux { "yes" } else { "no" });
    println!(
        "git={}",
        if command_exists("git") {
            "available"
        } else {
            "missing"
        }
    );
    println!(
        "rg={}",
        if command_exists("rg") {
            "available (optional)"
        } else {
            "missing (built-in project search remains available)"
        }
    );
    println!(
        "shell={}",
        env::var_os("SHELL")
            .filter(|shell| !shell.is_empty())
            .map(|shell| PathBuf::from(shell).display().to_string())
            .unwrap_or_else(|| "/bin/sh (fallback)".to_owned())
    );
    println!(
        "config={}",
        wscrpt::config::config_path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "unavailable (HOME and XDG_CONFIG_HOME unset)".to_owned())
    );
    println!(
        "session={}",
        SessionStore::from_env()
            .map(|store| store.path().display().to_string())
            .unwrap_or_else(|error| format!("unavailable ({error})"))
    );
    let discovered = wscrpt::lsp_discover::discover_language_servers();
    if discovered.is_empty() {
        println!("language_servers_on_path=none of the well-known candidates");
    } else {
        println!(
            "language_servers_on_path={}",
            discovered
                .iter()
                .map(|server| format!("{}={}", server.name, server.executable.display()))
                .collect::<Vec<_>>()
                .join(", ")
        );
        print!(
            "{}",
            wscrpt::lsp_discover::suggested_config_snippets(&discovered)
        );
    }
    if let Ok((config, source)) = Config::load() {
        if config.language_servers.is_empty() {
            println!(
                "language_servers_configured=0{}",
                source
                    .map(|path| format!(" ({path})"))
                    .unwrap_or_else(|| " (defaults)".to_owned())
            );
        } else {
            for server in &config.language_servers {
                let argv0 = server.argv.first().map(String::as_str).unwrap_or("");
                let resolved = wscrpt::lsp_discover::resolve_executable(argv0)
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "missing on PATH".to_owned());
                println!(
                    "language_server_configured={} argv0={argv0} resolved={resolved}",
                    server.name
                );
            }
        }
    }
    if !locale.to_ascii_uppercase().contains("UTF-8")
        && !locale.to_ascii_uppercase().contains("UTF8")
    {
        println!("warning=mosh and Unicode editing require a UTF-8 locale");
    }
    if tmux {
        println!(
            "clipboard=OSC 52 uses tmux DCS passthrough; tmux 3.3+ needs `allow-passthrough on`"
        );
    }
}

fn run_input_diagnostics() -> Result<()> {
    let _session = InputDiagnosticSession::enter()?;
    println!("wscrpt input diagnostics");
    println!(
        "route: TERM={} transport={} tmux={}",
        current_term(),
        current_transport(),
        env::var_os("TMUX").is_some()
    );
    println!("press keys to inspect decoded events; Ctrl-G or Ctrl-C exits");
    println!("event_limit={MAX_INPUT_DIAGNOSTIC_EVENTS}");
    std::io::stdout().flush()?;

    let mut events = 0_usize;
    while events < MAX_INPUT_DIAGNOSTIC_EVENTS {
        let event = event::read().context("could not read terminal input")?;
        println!("{:03} {}", events + 1, describe_event(&event));
        std::io::stdout().flush()?;
        events += 1;
        if is_input_diagnostic_exit(&event) {
            break;
        }
    }
    if events >= MAX_INPUT_DIAGNOSTIC_EVENTS {
        println!("event limit reached; exiting");
    }
    Ok(())
}

struct InputDiagnosticSession {
    raw_mode: bool,
    bracketed_paste: bool,
}

impl InputDiagnosticSession {
    fn enter() -> Result<Self> {
        let mut session = Self {
            raw_mode: false,
            bracketed_paste: false,
        };
        if !terminal::is_raw_mode_enabled()? {
            terminal::enable_raw_mode()?;
            session.raw_mode = true;
        }
        std::io::stdout().execute(EnableBracketedPaste)?;
        session.bracketed_paste = true;
        Ok(session)
    }
}

impl Drop for InputDiagnosticSession {
    fn drop(&mut self) {
        if self.bracketed_paste {
            let _ = std::io::stdout().execute(DisableBracketedPaste);
            self.bracketed_paste = false;
        }
        if self.raw_mode {
            let _ = terminal::disable_raw_mode();
            self.raw_mode = false;
        }
        let _ = std::io::stdout().flush();
    }
}

fn describe_event(event: &Event) -> String {
    match event {
        Event::Key(key) => describe_key_event(key),
        Event::Paste(text) => format!(
            "paste bytes={} chars={} text={}",
            text.len(),
            text.chars().count(),
            diagnostic_string(text)
        ),
        Event::Resize(width, height) => format!("resize cols={width} rows={height}"),
        Event::Mouse(mouse) => format!(
            "mouse kind={:?} column={} row={} modifiers={}",
            mouse.kind,
            mouse.column,
            mouse.row,
            describe_modifiers(mouse.modifiers)
        ),
        Event::FocusGained => "focus gained".to_owned(),
        Event::FocusLost => "focus lost".to_owned(),
    }
}

fn describe_key_event(key: &KeyEvent) -> String {
    format!(
        "key code={} modifiers={} kind={:?} state={:?}",
        describe_key_code(key.code),
        describe_modifiers(key.modifiers),
        key.kind,
        key.state
    )
}

fn describe_key_code(code: KeyCode) -> String {
    match code {
        KeyCode::Backspace => "Backspace".to_owned(),
        KeyCode::Enter => "Enter".to_owned(),
        KeyCode::Left => "Left".to_owned(),
        KeyCode::Right => "Right".to_owned(),
        KeyCode::Up => "Up".to_owned(),
        KeyCode::Down => "Down".to_owned(),
        KeyCode::Home => "Home".to_owned(),
        KeyCode::End => "End".to_owned(),
        KeyCode::PageUp => "PageUp".to_owned(),
        KeyCode::PageDown => "PageDown".to_owned(),
        KeyCode::Tab => "Tab".to_owned(),
        KeyCode::BackTab => "BackTab".to_owned(),
        KeyCode::Delete => "Delete".to_owned(),
        KeyCode::Insert => "Insert".to_owned(),
        KeyCode::F(number) => format!("F{number}"),
        KeyCode::Char(character) => format!("Char({})", diagnostic_char(character)),
        KeyCode::Null => "Null".to_owned(),
        KeyCode::Esc => "Esc".to_owned(),
        KeyCode::CapsLock => "CapsLock".to_owned(),
        KeyCode::ScrollLock => "ScrollLock".to_owned(),
        KeyCode::NumLock => "NumLock".to_owned(),
        KeyCode::PrintScreen => "PrintScreen".to_owned(),
        KeyCode::Pause => "Pause".to_owned(),
        KeyCode::Menu => "Menu".to_owned(),
        KeyCode::KeypadBegin => "KeypadBegin".to_owned(),
        KeyCode::Media(media) => format!("Media({media:?})"),
        KeyCode::Modifier(modifier) => format!("Modifier({modifier:?})"),
    }
}

fn describe_modifiers(modifiers: KeyModifiers) -> String {
    let mut labels = Vec::new();
    for (flag, label) in [
        (KeyModifiers::SHIFT, "SHIFT"),
        (KeyModifiers::CONTROL, "CONTROL"),
        (KeyModifiers::ALT, "ALT"),
        (KeyModifiers::SUPER, "SUPER"),
        (KeyModifiers::HYPER, "HYPER"),
        (KeyModifiers::META, "META"),
    ] {
        if modifiers.contains(flag) {
            labels.push(label);
        }
    }
    if labels.is_empty() {
        "none".to_owned()
    } else {
        labels.join("|")
    }
}

fn is_input_diagnostic_exit(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(KeyEvent {
            code: KeyCode::Char('g' | 'c'),
            modifiers,
            ..
        }) if modifiers.contains(KeyModifiers::CONTROL)
    )
}

fn current_term() -> String {
    env::var("TERM").unwrap_or_else(|_| "(unset)".to_owned())
}

fn current_transport() -> &'static str {
    if env::var_os("MOSH_IP").is_some() || env::var_os("MOSH_CONNECTION").is_some() {
        "mosh"
    } else if env::var_os("SSH_CONNECTION").is_some() {
        "ssh"
    } else {
        "local/unknown"
    }
}

fn diagnostic_string(input: &str) -> String {
    let mut output = String::new();
    for character in input.chars().take(80) {
        output.push_str(&diagnostic_char(character));
    }
    if input.chars().count() > 80 {
        output.push_str("...");
    }
    output
}

fn diagnostic_char(character: char) -> String {
    match character {
        '\n' => "\\n".to_owned(),
        '\r' => "\\r".to_owned(),
        '\t' => "\\t".to_owned(),
        '\\' => "\\\\".to_owned(),
        character if character.is_control() => format!("\\u{{{:04X}}}", character as u32),
        character => character.to_string(),
    }
}

fn command_exists(program: &str) -> bool {
    command_exists_in_path(program, env::var_os("PATH").as_deref())
}

fn command_exists_in_path(program: &str, path: Option<&std::ffi::OsStr>) -> bool {
    if program.is_empty() {
        return false;
    }
    let program_path = Path::new(program);
    if program_path.components().count() > 1 {
        return is_executable_file(program_path);
    }
    path.into_iter()
        .flat_map(env::split_paths)
        .map(|directory| directory.join(program))
        .any(|candidate| is_executable_file(&candidate))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use wscrpt::session::{OpenFileState, ViewportState};

    #[test]
    fn directory_becomes_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        let (file, root) = launch_paths(Some(dir.path().to_path_buf()), None).unwrap();
        assert!(file.is_none());
        assert_eq!(root.as_deref(), Some(dir.path()));
    }

    #[test]
    fn file_and_explicit_project_are_preserved() {
        let (file, root) = launch_paths(Some("src/main.rs".into()), Some(".".into())).unwrap();
        assert_eq!(file, Some("src/main.rs".into()));
        assert_eq!(root, Some(".".into()));
    }

    #[cfg(unix)]
    #[test]
    fn health_command_probe_is_path_only_and_requires_an_executable_file() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let tool = directory.path().join("probe-tool");
        fs::write(&tool, "not a runnable program").unwrap();
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o600)).unwrap();
        let path = directory.path().as_os_str().to_os_string();
        assert!(!command_exists_in_path("probe-tool", Some(&path)));

        fs::set_permissions(&tool, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(command_exists_in_path("probe-tool", Some(&path)));
        assert!(command_exists_in_path(
            tool.to_str().unwrap(),
            Some(std::ffi::OsStr::new(""))
        ));
    }

    #[test]
    fn input_diagnostics_describe_keys_paste_and_exit_keys() {
        let control_g = Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
        assert!(is_input_diagnostic_exit(&control_g));
        assert!(is_input_diagnostic_exit(&Event::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))));
        assert!(!is_input_diagnostic_exit(&Event::Key(KeyEvent::new(
            KeyCode::Esc,
            KeyModifiers::NONE,
        ))));

        assert_eq!(
            describe_event(&Event::Key(KeyEvent::new(
                KeyCode::Char('y'),
                KeyModifiers::CONTROL | KeyModifiers::ALT,
            ))),
            "key code=Char(y) modifiers=CONTROL|ALT kind=Press state=KeyEventState(0x0)"
        );
        assert_eq!(
            describe_event(&Event::Paste("a\n\t🛠".to_owned())),
            "paste bytes=7 chars=4 text=a\\n\\t🛠"
        );
        assert_eq!(
            describe_event(&Event::Resize(80, 24)),
            "resize cols=80 rows=24"
        );
    }

    #[test]
    fn session_restore_reopens_files_clamps_state_and_preserves_active_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.rs");
        let second = dir.path().join("second.rs");
        fs::write(&first, "one\n").unwrap();
        fs::write(&second, "αβ\nthird\n").unwrap();
        let mut session = Session::new(dir.path());
        session.open_files = vec![
            OpenFileState {
                path: first,
                cursor: 99,
                anchor: Some(100),
                viewport: ViewportState {
                    top_line: 99,
                    top_wrap_char: 3,
                    left_column: 7,
                },
            },
            OpenFileState {
                path: second.clone(),
                cursor: 2,
                anchor: Some(0),
                viewport: ViewportState::default(),
            },
        ];
        session.active_index = 1;

        let (workspace, skipped) = restore_workspace(&session).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!(workspace.len(), 2);
        assert_eq!(workspace.active().document.path(), Some(second.as_path()));
        assert_eq!(workspace.active().cursor, 2);
        assert_eq!(workspace.active().anchor, Some(0));
        assert_eq!(workspace.buffers()[0].cursor, 4);
        assert_eq!(workspace.buffers()[0].anchor, None);
        assert_eq!(workspace.buffers()[0].viewport.top_line, 1);
        assert_eq!(workspace.buffers()[0].viewport.top_wrap_char, 0);
        assert_eq!(workspace.buffers()[0].viewport.left_column, 7);
    }

    #[test]
    fn session_restore_skips_missing_files_without_creating_them() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("gone.rs");
        let mut session = Session::new(dir.path());
        session.open_files.push(OpenFileState {
            path: missing.clone(),
            cursor: 0,
            anchor: None,
            viewport: ViewportState::default(),
        });

        let (workspace, skipped) = restore_workspace(&session).unwrap();
        assert_eq!(skipped, 1);
        assert_eq!(workspace.len(), 1);
        assert!(workspace.active().document.path().is_none());
        assert!(!missing.exists());
    }
}
