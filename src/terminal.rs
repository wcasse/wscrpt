//! Terminal lifecycle management for the interactive editor.
//!
//! [`TerminalSession`] owns every terminal mode it enables and restores those
//! modes on normal return, early return, and unwinding. Only one session may be
//! active in a process at a time; nested terminal sessions cannot be restored
//! correctly because terminal modes are process-global.

use std::io::{self, Stdout, Write};
use std::panic;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use crossterm::ExecutableCommand;
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::style::{Attribute, ResetColor, SetAttribute};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};

const RAW_MODE: u8 = 1 << 0;
const ALTERNATE_SCREEN: u8 = 1 << 1;
const CURSOR_HIDDEN: u8 = 1 << 2;
const BRACKETED_PASTE: u8 = 1 << 3;
const MOUSE_CAPTURE: u8 = 1 << 4;

static SESSION_CLAIMED: AtomicBool = AtomicBool::new(false);
static PANIC_STATE: AtomicU8 = AtomicU8::new(0);
static INSTALL_PANIC_HOOK: Once = Once::new();

/// Configuration applied while entering a [`TerminalSession`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalOptions {
    /// Whether the editor should receive terminal mouse events.
    ///
    /// Mouse capture is disabled by default so Blink can retain its native
    /// touch selection behavior. The caller may opt in from user config.
    pub mouse_capture: bool,
    /// Whether pasted text should arrive as a single bracketed-paste event.
    pub bracketed_paste: bool,
}

impl TerminalOptions {
    pub const fn new(mouse_capture: bool) -> Self {
        Self {
            mouse_capture,
            bracketed_paste: true,
        }
    }
}

impl Default for TerminalOptions {
    fn default() -> Self {
        Self::new(false)
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct SessionState {
    claimed: bool,
    raw_mode: bool,
    alternate_screen: bool,
    cursor_hidden: bool,
    bracketed_paste: bool,
    mouse_capture: bool,
}

impl SessionState {
    const fn panic_bits(self) -> u8 {
        (if self.raw_mode { RAW_MODE } else { 0 })
            | (if self.alternate_screen {
                ALTERNATE_SCREEN
            } else {
                0
            })
            | (if self.cursor_hidden { CURSOR_HIDDEN } else { 0 })
            | (if self.bracketed_paste {
                BRACKETED_PASTE
            } else {
                0
            })
            | (if self.mouse_capture { MOUSE_CAPTURE } else { 0 })
    }

    const fn needs_restoration(self) -> bool {
        self.panic_bits() != 0
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct RestorationPlan {
    raw_mode: bool,
    alternate_screen: bool,
    cursor_hidden: bool,
    bracketed_paste: bool,
    mouse_capture: bool,
}

impl RestorationPlan {
    const fn from_bits(bits: u8) -> Self {
        Self {
            raw_mode: bits & RAW_MODE != 0,
            alternate_screen: bits & ALTERNATE_SCREEN != 0,
            cursor_hidden: bits & CURSOR_HIDDEN != 0,
            bracketed_paste: bits & BRACKETED_PASTE != 0,
            mouse_capture: bits & MOUSE_CAPTURE != 0,
        }
    }
}

/// RAII owner for the editor's process-global terminal modes.
///
/// `TerminalSession` implements [`Write`], so it can be passed directly to
/// Crossterm's `queue!`/`execute!` macros or used by the renderer. Prefer the
/// methods on this type when changing a tracked terminal mode so restoration
/// remains exact.
pub struct TerminalSession {
    stdout: Stdout,
    state: SessionState,
}

impl TerminalSession {
    /// Enter an interactive terminal session with bracketed paste enabled.
    pub fn new(mouse_capture: bool) -> io::Result<Self> {
        Self::with_options(TerminalOptions::new(mouse_capture))
    }

    /// Enter an interactive terminal session using explicit options.
    pub fn with_options(options: TerminalOptions) -> io::Result<Self> {
        if SESSION_CLAIMED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a terminal session is already active",
            ));
        }

        install_panic_restoration_hook();

        let mut session = Self {
            stdout: io::stdout(),
            state: SessionState {
                claimed: true,
                ..SessionState::default()
            },
        };

        // Respect raw mode owned by an outer caller. We only undo modes that
        // this guard actually enabled.
        if !terminal::is_raw_mode_enabled()? {
            session.state.raw_mode = true;
            session.publish_panic_state();
            terminal::enable_raw_mode()?;
        }

        session.state.alternate_screen = true;
        session.publish_panic_state();
        session.stdout.execute(EnterAlternateScreen)?;

        session.hide_cursor()?;

        if options.bracketed_paste {
            session.set_bracketed_paste(true)?;
        }
        if options.mouse_capture {
            session.set_mouse_capture(true)?;
        }

        session.clear()?;
        Ok(session)
    }

    /// Whether this guard still owns an entered terminal session.
    pub const fn is_active(&self) -> bool {
        self.state.claimed
    }

    pub const fn mouse_capture_enabled(&self) -> bool {
        self.state.mouse_capture
    }

    pub const fn bracketed_paste_enabled(&self) -> bool {
        self.state.bracketed_paste
    }

    /// Current terminal dimensions as `(columns, rows)`.
    pub fn size(&self) -> io::Result<(u16, u16)> {
        terminal::size()
    }

    /// Clear the alternate screen, home the cursor, and flush immediately.
    pub fn clear(&mut self) -> io::Result<()> {
        self.ensure_active()?;
        self.stdout.execute(Clear(ClearType::All))?;
        self.stdout.execute(MoveTo(0, 0))?;
        Ok(())
    }

    /// Request a complete repaint by clearing and homing the terminal.
    pub fn redraw(&mut self) -> io::Result<()> {
        self.clear()
    }

    pub fn hide_cursor(&mut self) -> io::Result<()> {
        self.ensure_active()?;
        if self.state.cursor_hidden {
            return Ok(());
        }

        // Mark first: if output was written but flushing reports an error,
        // Drop still makes a conservative attempt to show the cursor.
        self.state.cursor_hidden = true;
        self.publish_panic_state();
        self.stdout.execute(Hide)?;
        Ok(())
    }

    pub fn show_cursor(&mut self) -> io::Result<()> {
        self.ensure_active()?;
        if !self.state.cursor_hidden {
            return Ok(());
        }

        self.stdout.execute(Show)?;
        self.state.cursor_hidden = false;
        self.publish_panic_state();
        Ok(())
    }

    pub fn set_bracketed_paste(&mut self, enabled: bool) -> io::Result<()> {
        self.ensure_active()?;
        if enabled == self.state.bracketed_paste {
            return Ok(());
        }

        if enabled {
            self.state.bracketed_paste = true;
            self.publish_panic_state();
            self.stdout.execute(EnableBracketedPaste)?;
        } else {
            self.stdout.execute(DisableBracketedPaste)?;
            self.state.bracketed_paste = false;
            self.publish_panic_state();
        }
        Ok(())
    }

    pub fn set_mouse_capture(&mut self, enabled: bool) -> io::Result<()> {
        self.ensure_active()?;
        if enabled == self.state.mouse_capture {
            return Ok(());
        }

        if enabled {
            self.state.mouse_capture = true;
            self.publish_panic_state();
            self.stdout.execute(EnableMouseCapture)?;
        } else {
            self.stdout.execute(DisableMouseCapture)?;
            self.state.mouse_capture = false;
            self.publish_panic_state();
        }
        Ok(())
    }

    /// Restore all terminal modes owned by this guard.
    ///
    /// This operation is idempotent. If a cleanup step fails, its ownership
    /// flag is retained so a later call (or Drop) can retry it.
    pub fn restore(&mut self) -> io::Result<()> {
        if !self.state.claimed {
            return Ok(());
        }

        let mut first_error = None;

        if self.state.mouse_capture {
            match self.stdout.execute(DisableMouseCapture) {
                Ok(_) => self.state.mouse_capture = false,
                Err(error) => record_first_error(&mut first_error, error),
            }
            self.publish_panic_state();
        }

        if self.state.bracketed_paste {
            match self.stdout.execute(DisableBracketedPaste) {
                Ok(_) => self.state.bracketed_paste = false,
                Err(error) => record_first_error(&mut first_error, error),
            }
            self.publish_panic_state();
        }

        // Style state is not queried by Crossterm, but resetting it prevents a
        // failed render from leaking colors or attributes into the shell.
        if let Err(error) = self.stdout.execute(SetAttribute(Attribute::Reset)) {
            record_first_error(&mut first_error, error);
        }
        if let Err(error) = self.stdout.execute(ResetColor) {
            record_first_error(&mut first_error, error);
        }

        if self.state.alternate_screen {
            match self.stdout.execute(LeaveAlternateScreen) {
                Ok(_) => self.state.alternate_screen = false,
                Err(error) => record_first_error(&mut first_error, error),
            }
            self.publish_panic_state();
        }

        // Ensure the user's shell gets a visible cursor even when the editor
        // happened to have made it visible for its final frame.
        match self.stdout.execute(Show) {
            Ok(_) => self.state.cursor_hidden = false,
            Err(error) => {
                self.state.cursor_hidden = true;
                record_first_error(&mut first_error, error);
            }
        }
        self.publish_panic_state();

        if self.state.raw_mode {
            match terminal::disable_raw_mode() {
                Ok(()) => self.state.raw_mode = false,
                Err(error) => record_first_error(&mut first_error, error),
            }
            self.publish_panic_state();
        }

        if !self.state.needs_restoration() {
            self.release_claim();
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn ensure_active(&self) -> io::Result<()> {
        if self.state.claimed {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "terminal session has already been restored",
            ))
        }
    }

    fn publish_panic_state(&self) {
        PANIC_STATE.store(self.state.panic_bits(), Ordering::Release);
    }

    fn release_claim(&mut self) {
        self.state.claimed = false;
        PANIC_STATE.store(0, Ordering::Release);
        SESSION_CLAIMED.store(false, Ordering::Release);
    }
}

impl Write for TerminalSession {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.stdout.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stdout.flush()
    }

    fn write_all(&mut self, buffer: &[u8]) -> io::Result<()> {
        self.stdout.write_all(buffer)
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        if !self.state.claimed {
            return;
        }

        let _ = self.restore();

        if self.state.claimed {
            // Crossterm cleanup can itself fail (for example after stdout has
            // gone away). Make one final ANSI/raw-mode attempt, then release
            // the process-global claim so caught panics cannot poison tests or
            // a later editor session.
            best_effort_restore(RestorationPlan::from_bits(self.state.panic_bits()));
            self.state = SessionState::default();
            PANIC_STATE.store(0, Ordering::Release);
            SESSION_CLAIMED.store(false, Ordering::Release);
        }
    }
}

fn install_panic_restoration_hook() {
    INSTALL_PANIC_HOOK.call_once(|| {
        let previous_hook = panic::take_hook();
        panic::set_hook(Box::new(move |panic_info| {
            let bits = PANIC_STATE.swap(0, Ordering::AcqRel);
            if bits != 0 {
                best_effort_restore(RestorationPlan::from_bits(bits));
            }
            previous_hook(panic_info);
        }));
    });
}

fn best_effort_restore(plan: RestorationPlan) {
    let mut stdout = io::stdout();
    let _ = write_ansi_restoration(&mut stdout, plan);
    if plan.raw_mode {
        let _ = terminal::disable_raw_mode();
    }
}

fn write_ansi_restoration(writer: &mut impl Write, plan: RestorationPlan) -> io::Result<()> {
    if plan.mouse_capture {
        writer.write_all(b"\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l")?;
    }
    if plan.bracketed_paste {
        writer.write_all(b"\x1b[?2004l")?;
    }
    writer.write_all(b"\x1b[0m")?;
    if plan.alternate_screen {
        writer.write_all(b"\x1b[?1049l")?;
    }
    if plan.cursor_hidden {
        writer.write_all(b"\x1b[?25h")?;
    }
    writer.flush()
}

fn record_first_error(slot: &mut Option<io::Error>, error: io::Error) {
    if slot.is_none() {
        *slot = Some(error);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_enable_bracketed_paste_but_not_mouse_by_default() {
        assert_eq!(
            TerminalOptions::default(),
            TerminalOptions {
                mouse_capture: false,
                bracketed_paste: true,
            }
        );
        assert!(TerminalOptions::new(true).mouse_capture);
    }

    #[test]
    fn session_state_only_publishes_modes_it_owns() {
        let state = SessionState {
            claimed: true,
            raw_mode: true,
            alternate_screen: true,
            cursor_hidden: false,
            bracketed_paste: true,
            mouse_capture: false,
        };

        assert_eq!(
            RestorationPlan::from_bits(state.panic_bits()),
            RestorationPlan {
                raw_mode: true,
                alternate_screen: true,
                cursor_hidden: false,
                bracketed_paste: true,
                mouse_capture: false,
            }
        );
        assert!(state.needs_restoration());
        assert!(!SessionState::default().needs_restoration());
    }

    #[test]
    fn ansi_restoration_is_ordered_and_feature_scoped() {
        let mut output = Vec::new();
        write_ansi_restoration(
            &mut output,
            RestorationPlan {
                raw_mode: true,
                alternate_screen: true,
                cursor_hidden: true,
                bracketed_paste: true,
                mouse_capture: true,
            },
        )
        .unwrap();

        assert_eq!(
            output,
            b"\x1b[?1006l\x1b[?1015l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?2004l\x1b[0m\x1b[?1049l\x1b[?25h"
        );

        let mut minimal = Vec::new();
        write_ansi_restoration(
            &mut minimal,
            RestorationPlan {
                cursor_hidden: true,
                ..RestorationPlan::default()
            },
        )
        .unwrap();
        assert_eq!(minimal, b"\x1b[0m\x1b[?25h");
    }
}
