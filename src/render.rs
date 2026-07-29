use std::borrow::Cow;
use std::collections::VecDeque;
use std::io::{self, Write};
use std::ops::Range;

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::queue;
use crossterm::style::{
    Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{Clear, ClearType};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Prompt};
use crate::lsp_ui::DiagnosticSeverity;
use crate::syntax::{SyntaxKind, SyntaxSpan};
use crate::text::{grapheme_cell_width, visual_width};
use crate::visual::{VisualAnchor, VisualMetrics, VisualRow};

const MIN_WIDTH_FOR_PROJECT_SIDEBAR: usize = 72;
const MIN_EDITOR_WIDTH_WITH_PROJECT_SIDEBAR: usize = 48;
const MAX_PROJECT_SIDEBAR_WIDTH: usize = 30;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Style {
    fg: Color,
    bg: Color,
    bold: bool,
    underlined: bool,
}

impl Style {
    const fn new(fg: Color, bg: Color) -> Self {
        Self {
            fg,
            bg,
            bold: false,
            underlined: false,
        }
    }

    const fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    const fn underlined(mut self) -> Self {
        self.underlined = true;
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Span {
    text: String,
    style: Style,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Row {
    spans: Vec<Span>,
    width: usize,
}

impl Row {
    fn push(&mut self, text: impl Into<String>, style: Style) {
        let text = sanitize_display_text(&text.into());
        if text.is_empty() {
            return;
        }
        // Everything in a Row is eventually written to the user's terminal.
        // Paths, task labels, diagnostics, and server messages are untrusted
        // display text, so they must never be able to inject ANSI/control
        // sequences through otherwise ordinary UI chrome.
        self.width += UnicodeWidthStr::width(text.as_str());
        if self.spans.last().is_some_and(|span| span.style == style) {
            self.spans.last_mut().unwrap().text.push_str(&text);
        } else {
            self.spans.push(Span { text, style });
        }
    }

    /// Push text that is already sanitized and has a known width.
    /// This avoids redundant grapheme iteration and width calculation.
    fn push_clean(&mut self, text: &str, width: usize, style: Style) {
        if text.is_empty() {
            return;
        }
        debug_assert!(!text.chars().any(char::is_control));
        debug_assert_eq!(UnicodeWidthStr::width(text), width);
        self.width += width;
        if self.spans.last().is_some_and(|span| span.style == style) {
            self.spans.last_mut().unwrap().text.push_str(text);
        } else {
            self.spans.push(Span {
                text: text.to_owned(),
                style,
            });
        }
    }

    fn push_fitted(&mut self, text: &str, style: Style, max_width: usize) {
        if self.width >= max_width {
            return;
        }
        let available = max_width - self.width;
        let fitted = fit_text(text, available);
        self.push(fitted, style);
    }

    fn pad_to(&mut self, width: usize, style: Style) {
        if self.width < width {
            self.push(" ".repeat(width - self.width), style);
        }
    }

    fn append(&mut self, other: &Row) {
        for span in &other.spans {
            self.push(&span.text, span.style);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Layout {
    pub width: usize,
    pub height: usize,
    pub header_y: Option<usize>,
    pub content_y: usize,
    pub content_height: usize,
    pub gutter_width: usize,
    pub content_width: usize,
    pub status_y: usize,
    pub footer_y: usize,
    pub too_small: bool,
}

impl Layout {
    pub fn calculate(width: u16, height: u16, line_count: usize, line_numbers: bool) -> Self {
        let width = width as usize;
        let height = height as usize;
        let too_small = width < 40 || height < 6;
        let header_y = (height >= 8).then_some(0);
        let content_y = usize::from(header_y.is_some());
        let chrome = content_y + 2;
        let content_height = height.saturating_sub(chrome);
        let digits = line_count.max(1).ilog10() as usize + 1;
        let gutter_width = if line_numbers && width >= 24 {
            (digits + 2).min(width.saturating_sub(1))
        } else {
            0
        };
        let content_width = width.saturating_sub(gutter_width);
        Self {
            width,
            height,
            header_y,
            content_y,
            content_height,
            gutter_width,
            content_width,
            status_y: height.saturating_sub(2),
            footer_y: height.saturating_sub(1),
            too_small,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CandidateOverlayLayout {
    pub(crate) x: usize,
    pub(crate) y: usize,
    pub(crate) width: usize,
    pub(crate) shown: usize,
    pub(crate) window_start: usize,
    notice_rows: usize,
}

impl CandidateOverlayLayout {
    pub(crate) fn calculate(
        layout: Layout,
        item_count: usize,
        selected: usize,
        has_notice: bool,
    ) -> Option<Self> {
        let notice_rows = usize::from(has_notice);
        let available = layout
            .content_height
            .saturating_sub(1)
            .saturating_sub(notice_rows);
        let shown = item_count.min(available).min(12);
        if shown == 0 && !has_notice {
            return None;
        }
        let width = layout.width.saturating_sub(4).min(88);
        let x = (layout.width - width) / 2;
        let box_height = shown + 1 + notice_rows;
        let y = layout.content_y + layout.content_height.saturating_sub(box_height) / 2;
        let selected = selected.min(item_count.saturating_sub(1));
        let window_start = selected
            .saturating_add(1)
            .saturating_sub(shown)
            .min(item_count.saturating_sub(shown));
        Some(Self {
            x,
            y,
            width,
            shown,
            window_start,
            notice_rows,
        })
    }

    pub(crate) fn item_at(self, column: usize, row: usize) -> Option<usize> {
        if column < self.x || column >= self.x.saturating_add(self.width) {
            return None;
        }
        let visible_index = row.checked_sub(self.y.saturating_add(1))?;
        (visible_index < self.shown).then_some(self.window_start + visible_index)
    }

    fn notice_y(self) -> Option<usize> {
        (self.notice_rows != 0).then_some(self.y + self.shown + 1)
    }
}

pub struct Renderer {
    previous: Vec<Row>,
    previous_size: (u16, u16),
    invalidated: bool,
}

impl Default for Renderer {
    fn default() -> Self {
        Self {
            previous: Vec::new(),
            previous_size: (0, 0),
            invalidated: true,
        }
    }
}

/// Stats from one differential paint pass (for `WSCRPT_PERF`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PaintStats {
    pub rows_painted: usize,
    pub rows_total: usize,
}

impl Renderer {
    pub fn invalidate(&mut self) {
        self.invalidated = true;
    }

    pub fn draw(
        &mut self,
        output: &mut impl Write,
        app: &mut App,
        size: (u16, u16),
    ) -> io::Result<PaintStats> {
        let full_layout = Layout::calculate(
            size.0,
            size.1,
            app.workspace().active().document.line_count(),
            app.config().line_numbers,
        );
        let sidebar_width = project_sidebar_width(app, full_layout);
        let editor_width = full_layout.width.saturating_sub(sidebar_width);
        let editor_layout = Layout::calculate(
            editor_width as u16,
            size.1,
            app.workspace().active().document.line_count(),
            app.config().line_numbers,
        );
        app.prepare_viewport(editor_layout);
        let (rows, cursor) = build_frame(app, full_layout, editor_layout, sidebar_width);

        if self.previous_size != size || self.invalidated {
            queue!(
                output,
                Hide,
                SetAttribute(Attribute::Reset),
                ResetColor,
                Clear(ClearType::All)
            )?;
            self.previous.clear();
            self.previous_size = size;
            self.invalidated = false;
        } else {
            queue!(output, Hide)?;
        }

        let mut rows_painted = 0_usize;
        for (index, row) in rows.iter().enumerate() {
            if self.previous.get(index) != Some(row) {
                paint_row(output, index as u16, row)?;
                rows_painted += 1;
            }
        }
        if self.previous.len() > rows.len() {
            for index in rows.len()..self.previous.len() {
                queue!(
                    output,
                    MoveTo(0, index as u16),
                    Clear(ClearType::CurrentLine)
                )?;
                rows_painted += 1;
            }
        }
        let rows_total = rows.len();
        self.previous = rows;

        if let Some((x, y)) = cursor.filter(|_| !full_layout.too_small) {
            queue!(output, MoveTo(x, y), Show)?;
        } else {
            queue!(output, Hide)?;
        }
        output.flush()?;
        Ok(PaintStats {
            rows_painted,
            rows_total,
        })
    }
}

fn project_sidebar_width(app: &App, layout: Layout) -> usize {
    if !app.workspace_sidebar_visible()
        || layout.too_small
        || layout.width < MIN_WIDTH_FOR_PROJECT_SIDEBAR
    {
        return 0;
    }
    let width = layout.width / 4;
    width.clamp(20, MAX_PROJECT_SIDEBAR_WIDTH).min(
        layout
            .width
            .saturating_sub(MIN_EDITOR_WIDTH_WITH_PROJECT_SIDEBAR),
    )
}

fn build_frame(
    app: &mut App,
    layout: Layout,
    editor_layout: Layout,
    sidebar_width: usize,
) -> (Vec<Row>, Option<(u16, u16)>) {
    let mut rows = vec![Row::default(); layout.height];
    if layout.height == 0 || layout.width == 0 {
        return (rows, None);
    }
    if layout.too_small {
        return build_too_small(layout);
    }

    if let Some(header_y) = layout.header_y {
        rows[header_y] = build_header(app, layout.width);
    }
    let wrapped_rows = app.soft_wrap_enabled().then(|| {
        let editor = app.workspace().active();
        VisualMetrics::new(editor_layout.content_width, app.config().tab_width, true).visible_rows(
            &editor.document,
            VisualAnchor {
                line: editor.viewport.top_line,
                char_in_line: editor.viewport.top_wrap_char,
            },
            editor_layout.content_height,
        )
    });
    let wrapped_rows = wrapped_rows.and_then(Result::ok);
    if sidebar_width > 0 {
        build_content_with_project_sidebar(
            app,
            layout,
            editor_layout,
            sidebar_width,
            &mut rows,
            wrapped_rows.as_deref(),
        );
    } else {
        build_editor_rows(app, layout, &mut rows, wrapped_rows.as_deref());
    }
    rows[layout.status_y] = build_status(app, layout.width);
    let prompt_window = app
        .prompt()
        .map(|prompt| prompt_window(prompt, layout.width));
    rows[layout.footer_y] = build_footer(app, layout.width, prompt_window.as_ref());

    if app.is_help() {
        overlay_help(app, layout, &mut rows);
    } else if let Some(overlay) = app.overlay() {
        overlay_candidates(layout, &mut rows, overlay);
    }

    let cursor = if let Some(prompt_window) = prompt_window {
        Some((prompt_window.cursor_x as u16, layout.footer_y as u16))
    } else if app.is_help() || app.overlay().is_some() {
        None
    } else {
        editor_cursor(app, editor_layout, wrapped_rows.as_deref()).map(|(x, y)| {
            (
                x.saturating_add(u16::try_from(sidebar_width).unwrap_or(u16::MAX)),
                y,
            )
        })
    };
    (rows, cursor)
}

fn build_content_with_project_sidebar(
    app: &mut App,
    layout: Layout,
    editor_layout: Layout,
    sidebar_width: usize,
    rows: &mut [Row],
    wrapped_rows: Option<&[VisualRow]>,
) {
    let mut editor_rows = vec![Row::default(); layout.height];
    build_editor_rows(app, editor_layout, &mut editor_rows, wrapped_rows);
    let sidebar_rows = build_project_sidebar_rows(app, editor_layout, sidebar_width);
    let divider = Style::new(Color::AnsiValue(240), Color::AnsiValue(235));
    for screen_line in 0..editor_layout.content_height {
        let y = layout.content_y + screen_line;
        let mut row = sidebar_rows
            .get(screen_line)
            .cloned()
            .unwrap_or_else(Row::default);
        row.push("│", divider);
        row.append(&editor_rows[y]);
        row.pad_to(layout.width, Style::new(Color::Reset, Color::Reset));
        rows[y] = row;
    }
}

fn build_too_small(layout: Layout) -> (Vec<Row>, Option<(u16, u16)>) {
    let mut rows = vec![Row::default(); layout.height];
    let style = Style::new(Color::White, Color::AnsiValue(52));
    for row in &mut rows {
        row.pad_to(layout.width, style);
    }
    if layout.height > 0 {
        let message = if layout.width >= 16 {
            " wscrpt: terminal too small "
        } else {
            " too small "
        };
        let x = layout.width.saturating_sub(visual_width(message, 4)) / 2;
        let mut row = Row::default();
        row.push(" ".repeat(x), style);
        row.push_fitted(message, style.bold(), layout.width);
        row.pad_to(layout.width, style);
        rows[layout.height / 2] = row;
    }
    (rows, None)
}

fn build_project_sidebar_rows(app: &App, layout: Layout, sidebar_width: usize) -> Vec<Row> {
    let content_width = sidebar_width.saturating_sub(1);
    let base = Style::new(Color::AnsiValue(250), Color::AnsiValue(235));
    let header = Style::new(Color::White, Color::AnsiValue(60)).bold();
    let directory = Style::new(Color::AnsiValue(153), Color::AnsiValue(235)).bold();
    let file = Style::new(Color::AnsiValue(250), Color::AnsiValue(235));
    let active = Style::new(Color::Black, Color::AnsiValue(180)).bold();
    let muted = Style::new(Color::AnsiValue(244), Color::AnsiValue(235));
    let mut rows = Vec::with_capacity(layout.content_height);
    let view_rows = layout.content_height.saturating_sub(2);
    let view = app.project_sidebar_view(view_rows);

    let mut title = Row::default();
    title.push_fitted(" PROJECT ", header, content_width);
    title.pad_to(content_width, header);
    rows.push(title);

    for line in view.lines.iter().take(view_rows) {
        let mut row = Row::default();
        let indent_width = line
            .depth
            .saturating_mul(2)
            .min(content_width.saturating_sub(1));
        row.push(" ".repeat(indent_width), base);
        let style = if line.active {
            active
        } else if line.directory {
            directory
        } else {
            file
        };
        row.push_fitted(&line.text, style, content_width);
        row.pad_to(content_width, base);
        rows.push(row);
    }

    while rows.len() < layout.content_height.saturating_sub(1) {
        let mut row = Row::default();
        row.pad_to(content_width, base);
        rows.push(row);
    }

    if layout.content_height > 1 {
        let mut footer = Row::default();
        let label = if view.unavailable {
            " no index "
        } else if view.partial {
            " partial · Esc w t "
        } else {
            " Esc w t "
        };
        footer.push_fitted(label, muted, content_width);
        footer.pad_to(content_width, muted);
        rows.push(footer);
    }

    rows.truncate(layout.content_height);
    rows
}

fn build_header(app: &App, width: usize) -> Row {
    let base = Style::new(Color::AnsiValue(250), Color::AnsiValue(235));
    let active = Style::new(Color::White, Color::AnsiValue(24)).bold();
    let dirty = Style::new(Color::AnsiValue(223), Color::AnsiValue(235));
    let overflow = Style::new(Color::AnsiValue(223), Color::AnsiValue(235)).bold();
    let mut row = Row::default();
    row.push_fitted(
        " wscrpt ",
        Style::new(Color::White, Color::AnsiValue(60)).bold(),
        width,
    );
    let labels = app
        .workspace()
        .buffers()
        .iter()
        .enumerate()
        .map(|(index, editor)| {
            let marker = if editor.document.is_modified() {
                "*"
            } else {
                ""
            };
            sanitize_display_text(&format!(
                " {}:{}{} ",
                index + 1,
                editor.document.display_name(),
                marker
            ))
        })
        .collect::<Vec<_>>();
    let available = width.saturating_sub(row.width);
    let window = header_tab_window(
        &labels
            .iter()
            .map(|label| visual_width(label, 4))
            .collect::<Vec<_>>(),
        app.workspace().active_index(),
        available,
    );
    let hidden_right = window.end < labels.len();
    if window.start > 0 {
        row.push_fitted("‹", overflow, width);
    }
    let label_limit = width.saturating_sub(usize::from(hidden_right));
    for (index, label) in labels
        .iter()
        .enumerate()
        .skip(window.start)
        .take(window.end.saturating_sub(window.start))
    {
        let editor = &app.workspace().buffers()[index];
        let style = if index == app.workspace().active_index() {
            active
        } else if editor.document.is_modified() {
            dirty
        } else {
            base
        };
        row.push_fitted(label, style, label_limit);
    }
    if hidden_right {
        row.push_fitted("›", overflow, width);
    }
    row.pad_to(width, base);
    row
}

fn header_tab_window(label_widths: &[usize], active: usize, available: usize) -> Range<usize> {
    if label_widths.is_empty() || available == 0 {
        return 0..0;
    }
    let active = active.min(label_widths.len() - 1);
    let mut start = active;
    let mut end = active + 1;

    while start > 0 && header_window_width(label_widths, start - 1, end) <= available {
        start -= 1;
    }
    while end < label_widths.len() && header_window_width(label_widths, start, end + 1) <= available
    {
        end += 1;
    }
    start..end
}

fn header_window_width(label_widths: &[usize], start: usize, end: usize) -> usize {
    label_widths[start..end].iter().fold(
        usize::from(start > 0) + usize::from(end < label_widths.len()),
        |width, label| width.saturating_add(*label),
    )
}

fn build_editor_rows(
    app: &mut App,
    layout: Layout,
    rows: &mut [Row],
    wrapped_rows: Option<&[VisualRow]>,
) {
    if let Some(wrapped_rows) = wrapped_rows {
        build_wrapped_editor_rows(app, layout, rows, wrapped_rows);
        return;
    }
    let top_line = app.workspace().active().viewport.top_line;
    let left_column = app.workspace().active().viewport.left_column;
    let line_count = app.workspace().active().document.line_count();
    let selection = app.workspace().active().selection();
    let search_match = app.search_match();
    let diagnostics = app.diagnostic_highlights();
    let cursor_line = app
        .workspace()
        .active()
        .position(app.config().tab_width)
        .line;
    let gutter = Style::new(Color::AnsiValue(244), Color::Reset);
    let gutter_current = Style::new(pro_ansi_bright_yellow(), Color::AnsiValue(234)).bold();
    let blank = Style::new(Color::AnsiValue(238), Color::Reset);
    let visible_lines = (0..layout.content_height)
        .map(|screen_line| top_line + screen_line)
        .filter(|&line| line < line_count);
    let syntax_cache = syntax_spans_for_visible_lines(app, visible_lines);

    for screen_line in 0..layout.content_height {
        let y = layout.content_y + screen_line;
        let document_line = top_line + screen_line;
        let mut row = Row::default();
        if document_line >= line_count {
            if layout.gutter_width > 0 {
                row.push_fitted("~", blank, layout.gutter_width);
                row.pad_to(layout.gutter_width, blank);
            }
            row.pad_to(layout.width, Style::new(Color::Reset, Color::Reset));
            rows[y] = row;
            continue;
        }

        if layout.gutter_width > 0 {
            let style = if document_line == cursor_line {
                gutter_current
            } else {
                gutter
            };
            let number_width = layout.gutter_width.saturating_sub(2);
            row.push(format!("{:>number_width$} │", document_line + 1), style);
        }
        let line_start = app
            .workspace()
            .active()
            .document
            .line_start_char(document_line);
        let line_end = app
            .workspace()
            .active()
            .document
            .line_end_char(document_line);
        let syntax = syntax_cache
            .get(&document_line)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        append_document_range(
            &mut row,
            app,
            document_line,
            line_start..line_end,
            0,
            left_column,
            layout.content_width,
            syntax,
            LineDecorations {
                selection: selection.as_ref(),
                search_match: search_match.as_ref(),
                diagnostics: &diagnostics,
                is_current: document_line == cursor_line,
            },
        );
        row.pad_to(layout.width, Style::new(Color::Reset, Color::Reset));
        rows[y] = row;
    }
}

fn build_wrapped_editor_rows(
    app: &mut App,
    layout: Layout,
    rows: &mut [Row],
    visual_rows: &[VisualRow],
) {
    let selection = app.workspace().active().selection();
    let search_match = app.search_match();
    let diagnostics = app.diagnostic_highlights();
    let cursor_line = app
        .workspace()
        .active()
        .position(app.config().tab_width)
        .line;
    let gutter = Style::new(Color::AnsiValue(244), Color::Reset);
    let gutter_current = Style::new(pro_ansi_bright_yellow(), Color::AnsiValue(234)).bold();
    let blank = Style::new(Color::AnsiValue(238), Color::Reset);
    // Indexed preamble + one sequential walk over the visible logical range.
    let syntax_cache =
        syntax_spans_for_visible_lines(app, visual_rows.iter().map(|row| row.logical_line));

    for screen_line in 0..layout.content_height {
        let y = layout.content_y + screen_line;
        let mut row = Row::default();
        let Some(visual_row) = visual_rows.get(screen_line) else {
            if layout.gutter_width > 0 {
                row.push_fitted("~", blank, layout.gutter_width);
                row.pad_to(layout.gutter_width, blank);
            }
            row.pad_to(layout.width, Style::new(Color::Reset, Color::Reset));
            rows[y] = row;
            continue;
        };

        if layout.gutter_width > 0 {
            let style = if visual_row.logical_line == cursor_line {
                gutter_current
            } else {
                gutter
            };
            let number_width = layout.gutter_width.saturating_sub(2);
            if visual_row.continuation {
                row.push(format!("{:>number_width$} │", "↪"), style);
            } else {
                row.push(
                    format!("{:>number_width$} │", visual_row.logical_line + 1),
                    style,
                );
            }
        }
        let syntax = syntax_cache
            .get(&visual_row.logical_line)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        append_document_range(
            &mut row,
            app,
            visual_row.logical_line,
            visual_row.char_range.clone(),
            visual_row.logical_visual_start,
            visual_row.logical_visual_start,
            layout.content_width,
            syntax,
            LineDecorations {
                selection: selection.as_ref(),
                search_match: search_match.as_ref(),
                diagnostics: &diagnostics,
                is_current: visual_row.logical_line == cursor_line,
            },
        );
        rows[y] = row;
    }
}

struct LineDecorations<'a> {
    selection: Option<&'a std::ops::Range<usize>>,
    search_match: Option<&'a std::ops::Range<usize>>,
    diagnostics: &'a [(std::ops::Range<usize>, DiagnosticSeverity)],
    is_current: bool,
}

/// Highlight every logical line in `lines` with a single sequential state walk.
///
/// Preamble comes from the rope-backed sample index (fast jump to bottom of
/// large files). Visible lines still get full highlighter spans once.
fn syntax_spans_for_visible_lines(
    app: &mut App,
    lines: impl IntoIterator<Item = usize>,
) -> std::collections::HashMap<usize, Vec<SyntaxSpan>> {
    let mut needed: Vec<usize> = lines.into_iter().collect();
    if needed.is_empty() {
        return std::collections::HashMap::new();
    }
    needed.sort_unstable();
    needed.dedup();
    let first = needed[0];
    let last = *needed.last().unwrap_or(&first);
    let mut state = app.syntax_state_before_line(first);
    let path = app
        .workspace()
        .active()
        .document
        .path()
        .map(std::path::PathBuf::from);
    let mut cache = std::collections::HashMap::with_capacity(needed.len());
    let mut needed_index = 0;
    for line in first..=last {
        let editor = app.workspace().active();
        let line_start = editor.document.line_start_char(line);
        let line_end = editor.document.line_end_char(line);
        let text = editor.document.slice(line_start..line_end);
        let (spans, next_state) =
            crate::syntax::highlight_line_with_state(path.as_deref(), &text, state);
        if needed_index < needed.len() && needed[needed_index] == line {
            cache.insert(line, spans);
            needed_index += 1;
        }
        state = next_state;
        if needed_index >= needed.len() {
            break;
        }
    }
    cache
}

#[allow(clippy::too_many_arguments)]
fn append_document_range(
    row: &mut Row,
    app: &App,
    line_number: usize,
    global_range: std::ops::Range<usize>,
    logical_visual_start: usize,
    left: usize,
    width: usize,
    syntax: &[SyntaxSpan],
    decorations: LineDecorations<'_>,
) {
    let editor = app.workspace().active();
    let row_start_width = row.width;
    let line_start = editor.document.line_start_char(line_number);
    let line_end = editor.document.line_end_char(line_number);
    let range_start = global_range.start.clamp(line_start, line_end);
    let range_end = global_range.end.clamp(range_start, line_end);
    let text = editor.document.slice(range_start..range_end);
    let right = left.saturating_add(width);
    // Default body text: near-white on Pro-like near-black (or transparent bg so
    // the host Terminal "Pro" background shows through).
    let base = if decorations.is_current {
        Style::new(pro_fg_default(), pro_bg_current_line())
    } else {
        Style::new(pro_fg_default(), Color::Reset)
    };
    let selected = Style::new(Color::White, Color::AnsiValue(19)); // deep blue selection
    let matched = Style::new(Color::Black, Color::AnsiValue(226)).bold(); // Pro yellow hit
    let mut syntax_index = 0;
    let mut visual_column = logical_visual_start;
    let mut char_column = range_start.saturating_sub(line_start);

    for grapheme in text.graphemes(true) {
        let grapheme_chars = grapheme.chars().count();
        let local_range = char_column..char_column + grapheme_chars;
        let global = line_start + char_column;
        let char_range = global..global + grapheme_chars;
        let (rendered, cell_width) =
            display_grapheme(grapheme, visual_column, app.config().tab_width);
        let visual_end = visual_column + cell_width;
        char_column += grapheme_chars;

        if visual_end <= left {
            visual_column = visual_end;
            continue;
        }
        if visual_column >= right {
            break;
        }

        while syntax
            .get(syntax_index)
            .is_some_and(|span| span.range.end <= local_range.start)
        {
            syntax_index += 1;
        }
        let lexical = syntax
            .get(syntax_index)
            .filter(|span| ranges_overlap(&span.range, &local_range))
            .map(|span| syntax_style(span.kind, base.bg))
            .unwrap_or(base);

        let style = if decorations
            .selection
            .is_some_and(|range| ranges_overlap(range, &char_range))
        {
            selected
        } else if decorations
            .search_match
            .is_some_and(|range| ranges_overlap(range, &char_range))
        {
            matched
        } else if let Some((_, severity)) = decorations
            .diagnostics
            .iter()
            .find(|(range, _)| ranges_overlap(range, &char_range))
        {
            Style::new(
                match severity {
                    DiagnosticSeverity::Error => pro_ansi_bright_red(),
                    DiagnosticSeverity::Warning => pro_ansi_bright_yellow(),
                    DiagnosticSeverity::Information => pro_ansi_bright_cyan(),
                    DiagnosticSeverity::Hint => pro_ansi_bright_green(),
                    DiagnosticSeverity::Unknown(_) => pro_fg_default(),
                },
                base.bg,
            )
            .underlined()
        } else {
            lexical
        };
        let visible_start = left.saturating_sub(visual_column);
        let visible_end = cell_width.min(right.saturating_sub(visual_column));
        if visible_start == 0 && visible_end == cell_width {
            row.push_clean(&rendered, cell_width, style);
        } else {
            let spaces = " ".repeat(visible_end.saturating_sub(visible_start));
            row.push_clean(&spaces, visible_end.saturating_sub(visible_start), style);
        }
        visual_column = visual_end;
    }
    row.pad_to(row_start_width + width, base);
}

/// Terminal.app **Pro**-inspired syntax colors.
///
/// Pro’s rainbow comes from saturated ANSI bright reds/greens/yellows/blues/
/// magentas/cyans on a near-black field. We map lexical kinds onto that same
/// family so code reads like a colorful Pro session rather than a muted gray
/// IDE theme. 256-color indexes stay compatible with Blink/mosh; truecolor
/// hosts still render these as vivid primaries.
fn syntax_style(kind: SyntaxKind, background: Color) -> Style {
    match kind {
        // Magenta / hot pink — control flow & declarations
        SyntaxKind::Keyword => Style::new(pro_ansi_bright_magenta(), background).bold(),
        // Electric cyan — types & type-ish names
        SyntaxKind::Type => Style::new(pro_ansi_bright_cyan(), background).bold(),
        // Neon green — string literals
        SyntaxKind::String => Style::new(pro_ansi_bright_green(), background),
        // Dim gray-green — comments stay secondary
        SyntaxKind::Comment => Style::new(pro_comment(), background),
        // Bright yellow — numbers
        SyntaxKind::Number => Style::new(pro_ansi_bright_yellow(), background),
        // Orange / gold — true/false/null/const-ish
        SyntaxKind::Constant => Style::new(pro_constant(), background).bold(),
        // Azure blue — function / method names
        SyntaxKind::Function => Style::new(pro_ansi_bright_blue(), background).bold(),
        // Light cyan — properties / fields
        SyntaxKind::Property => Style::new(pro_property(), background),
        // Bright yellow-white — headings
        SyntaxKind::Heading => Style::new(pro_ansi_bright_yellow(), background).bold(),
    }
}

// --- Terminal Pro palette (256-color approximations of classic ANSI bright) ---

fn pro_fg_default() -> Color {
    // Near pure white — Pro text color
    Color::AnsiValue(255)
}

fn pro_bg_current_line() -> Color {
    // Slight lift off black for the active line (still Pro-dark)
    Color::AnsiValue(234)
}

fn pro_comment() -> Color {
    // Muted gray (bright black / Pro comment-like secondary)
    Color::AnsiValue(245)
}

fn pro_constant() -> Color {
    // Warm gold / bright yellow-orange
    Color::AnsiValue(220)
}

fn pro_property() -> Color {
    // Light cyan
    Color::AnsiValue(159)
}

fn pro_ansi_bright_red() -> Color {
    Color::AnsiValue(196) // #ff0000-ish
}

fn pro_ansi_bright_green() -> Color {
    Color::AnsiValue(46) // #00ff00-ish
}

fn pro_ansi_bright_yellow() -> Color {
    Color::AnsiValue(226) // #ffff00-ish
}

fn pro_ansi_bright_blue() -> Color {
    Color::AnsiValue(39) // vivid azure (readable on black)
}

fn pro_ansi_bright_magenta() -> Color {
    Color::AnsiValue(201) // #ff00ff-ish
}

fn pro_ansi_bright_cyan() -> Color {
    Color::AnsiValue(51) // #00ffff-ish
}

fn build_status(app: &App, width: usize) -> Row {
    let base = Style::new(Color::AnsiValue(252), Color::AnsiValue(237));
    let mode_style = match app.mode_label() {
        "EDIT" => Style::new(Color::Black, Color::AnsiValue(114)).bold(),
        "VIEW" => Style::new(Color::Black, Color::AnsiValue(153)).bold(),
        "ACTION" => Style::new(Color::Black, Color::AnsiValue(180)).bold(),
        "HELP" => Style::new(Color::White, Color::AnsiValue(61)).bold(),
        _ => Style::new(Color::White, Color::AnsiValue(31)).bold(),
    };
    let editor = app.workspace().active();
    let position = editor.position(app.config().tab_width);
    let dirty = if editor.document.is_modified() {
        "*"
    } else {
        ""
    };
    let left = format!(
        " {}/{} {}{} ",
        app.workspace().active_index() + 1,
        app.workspace().len(),
        editor.document.display_name(),
        dirty
    );
    let git = app
        .git_summary()
        .map(|summary| format!("  {summary}"))
        .unwrap_or_default();
    let lsp = app
        .lsp_summary()
        .map(|summary| format!("  {summary}"))
        .unwrap_or_default();
    let wrap = if app.soft_wrap_enabled() {
        "  WRAP"
    } else {
        ""
    };
    let right = format!(
        " Ln {}:{}  {}  UTF-8{}{}{} ",
        position.line + 1,
        position.visual_column + 1,
        editor.document.line_ending().label(),
        git,
        lsp,
        wrap
    );
    let mut row = Row::default();
    row.push(format!(" {} ", app.mode_label()), mode_style);
    let remaining = width.saturating_sub(row.width);
    let right_width = visual_width(&right, 4).min(remaining);
    let left_limit = remaining.saturating_sub(right_width);
    row.push_fitted(&left, base, row.width + left_limit);
    row.pad_to(width.saturating_sub(right_width), base);
    row.push_fitted(&right, base, width);
    row.pad_to(width, base);
    row
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PromptWindow {
    prefix: String,
    input: String,
    cursor_x: usize,
    hidden_left: bool,
    hidden_right: bool,
}

#[derive(Debug)]
struct PromptCell {
    text: String,
    width: usize,
}

fn prompt_window(prompt: &Prompt, width: usize) -> PromptWindow {
    let cursor_chars = prompt.before_cursor().chars().count();
    layout_prompt_window(prompt.prefix(), &prompt.input, cursor_chars, width)
}

fn layout_prompt_window(
    prefix: &str,
    input: &str,
    cursor_chars: usize,
    width: usize,
) -> PromptWindow {
    if width == 0 {
        return PromptWindow {
            prefix: String::new(),
            input: String::new(),
            cursor_x: 0,
            hidden_left: !input.is_empty(),
            hidden_right: !input.is_empty(),
        };
    }

    // Long descriptive prompts (notably recovery) may not consume the entire
    // footer: retain at least half the row for editable input.
    let prefix_limit = width.saturating_sub(1).min(width / 2);
    let prefix = fit_text(prefix, prefix_limit);
    let prefix_width = visual_width(&prefix, 4).min(width.saturating_sub(1));
    let available = width - prefix_width;
    let cursor_byte = grapheme_boundary_byte(input, cursor_chars);
    let mut logical_column = prefix_width;
    let mut before = VecDeque::new();
    let mut before_width = 0usize;
    let mut hidden_left = false;
    let max_before_width = available.saturating_sub(1);

    for grapheme in input[..cursor_byte].graphemes(true) {
        let (text, cell_width) = display_grapheme(grapheme, logical_column, 4);
        logical_column = logical_column.saturating_add(cell_width);
        before_width = before_width.saturating_add(cell_width);
        before.push_back(PromptCell {
            text: text.into_owned(),
            width: cell_width,
        });
        loop {
            let marker_width = usize::from(hidden_left && available > 1);
            if before_width.saturating_add(marker_width) <= max_before_width {
                break;
            }
            let Some(removed) = before.pop_front() else {
                break;
            };
            before_width = before_width.saturating_sub(removed.width);
            hidden_left = true;
        }
    }

    let show_left_marker = hidden_left && available > 1;
    let mut rendered_input = String::new();
    if show_left_marker {
        rendered_input.push('‹');
    }
    for cell in before {
        rendered_input.push_str(&cell.text);
    }
    let used_before = before_width + usize::from(show_left_marker);
    let cursor_x = prefix_width + used_before;
    let mut remaining = available.saturating_sub(used_before);
    let mut suffix = input[cursor_byte..].graphemes(true).peekable();
    let mut hidden_right = false;

    while let Some(grapheme) = suffix.next() {
        let (text, cell_width) = display_grapheme(grapheme, logical_column, 4);
        let reserve_marker = usize::from(suffix.peek().is_some());
        if cell_width.saturating_add(reserve_marker) > remaining {
            hidden_right = true;
            break;
        }
        rendered_input.push_str(&text);
        remaining -= cell_width;
        logical_column = logical_column.saturating_add(cell_width);
    }
    if hidden_right && remaining > 0 {
        rendered_input.push('›');
    }

    PromptWindow {
        prefix,
        input: rendered_input,
        cursor_x: cursor_x.min(width - 1),
        hidden_left,
        hidden_right,
    }
}

fn grapheme_boundary_byte(text: &str, requested_char: usize) -> usize {
    let mut consumed_chars = 0usize;
    let mut boundary_byte = 0usize;
    for (byte, grapheme) in text.grapheme_indices(true) {
        if consumed_chars >= requested_char {
            return byte;
        }
        let next_chars = consumed_chars.saturating_add(grapheme.chars().count());
        if next_chars > requested_char {
            return byte;
        }
        consumed_chars = next_chars;
        boundary_byte = byte + grapheme.len();
    }
    boundary_byte
}

fn build_footer(app: &App, width: usize, prompt_window: Option<&PromptWindow>) -> Row {
    let base = Style::new(Color::AnsiValue(250), Color::AnsiValue(234));
    let error = Style::new(Color::AnsiValue(224), Color::AnsiValue(52)).bold();
    let mut row = Row::default();
    if let Some(prompt) = prompt_window {
        row.push_fitted(
            &prompt.prefix,
            Style::new(Color::AnsiValue(223), Color::AnsiValue(234)).bold(),
            width,
        );
        row.push_fitted(&prompt.input, base, width);
    } else if let Some(message) = app.status_message() {
        row.push_fitted(
            &format!(" {message} "),
            if app.status_is_error() { error } else { base },
            width,
        );
    } else {
        row.push_fitted(&app.footer_hint(), base, width);
    }
    if let Some(perf) = app.perf_stats() {
        let muted = Style::new(Color::AnsiValue(244), Color::AnsiValue(234));
        row.push_fitted(&format!(" {perf} "), muted, width);
    }
    row.pad_to(width, base);
    row
}

fn overlay_help(app: &App, layout: Layout, rows: &mut [Row]) {
    let lines = app.help_lines();
    let box_width = layout.width.saturating_sub(4).min(76);
    let box_height = lines.len().saturating_add(2).min(layout.content_height);
    let x = (layout.width - box_width) / 2;
    let y = layout.content_y + layout.content_height.saturating_sub(box_height) / 2;
    let border = Style::new(Color::AnsiValue(223), Color::AnsiValue(236)).bold();
    let body = Style::new(Color::AnsiValue(252), Color::AnsiValue(236));

    for index in 0..box_height {
        let mut row = Row::default();
        row.push(" ".repeat(x), Style::new(Color::Reset, Color::Reset));
        if index == 0 || index + 1 == box_height {
            row.push("─".repeat(box_width), border);
        } else {
            row.push("│", border);
            row.push_fitted(&lines[index - 1], body, x + box_width - 1);
            row.pad_to(x + box_width - 1, body);
            row.push("│", border);
        }
        row.pad_to(layout.width, Style::new(Color::Reset, Color::Reset));
        rows[y + index] = row;
    }
}

fn overlay_candidates(layout: Layout, rows: &mut [Row], overlay: crate::app::OverlayView<'_>) {
    let Some(overlay_layout) = CandidateOverlayLayout::calculate(
        layout,
        overlay.items.len(),
        overlay.selected,
        overlay.notice.is_some(),
    ) else {
        return;
    };
    let x = overlay_layout.x;
    let y = overlay_layout.y;
    let box_width = overlay_layout.width;
    let shown = overlay_layout.shown;
    let header = Style::new(Color::AnsiValue(223), Color::AnsiValue(236)).bold();
    let body = Style::new(Color::AnsiValue(252), Color::AnsiValue(236));
    let active = Style::new(Color::White, Color::AnsiValue(25)).bold();
    let notice_style = Style::new(Color::AnsiValue(221), Color::AnsiValue(236));

    let mut title = Row::default();
    title.push(" ".repeat(x), Style::new(Color::Reset, Color::Reset));
    title.push_fitted(&format!(" {} ", overlay.title), header, x + box_width);
    title.pad_to(x + box_width, header);
    title.pad_to(layout.width, Style::new(Color::Reset, Color::Reset));
    rows[y] = title;

    let selected = overlay.selected.min(overlay.items.len().saturating_sub(1));
    let window_start = overlay_layout.window_start;
    for (visible_index, item) in overlay
        .items
        .iter()
        .skip(window_start)
        .take(shown)
        .enumerate()
    {
        let item_index = overlay_layout
            .item_at(x, y + visible_index + 1)
            .expect("each rendered candidate row maps to its source item");
        let style = if item_index == selected { active } else { body };
        let mut row = Row::default();
        row.push(" ".repeat(x), Style::new(Color::Reset, Color::Reset));
        row.push_fitted(&format!(" {item}"), style, x + box_width);
        row.pad_to(x + box_width, style);
        row.pad_to(layout.width, Style::new(Color::Reset, Color::Reset));
        rows[y + visible_index + 1] = row;
    }

    if let Some(notice) = overlay.notice {
        let mut row = Row::default();
        row.push(" ".repeat(x), Style::new(Color::Reset, Color::Reset));
        row.push_fitted(&format!(" ! {notice}"), notice_style, x + box_width);
        row.pad_to(x + box_width, notice_style);
        row.pad_to(layout.width, Style::new(Color::Reset, Color::Reset));
        rows[overlay_layout
            .notice_y()
            .expect("notice row exists when an overlay notice is rendered")] = row;
    }
}

fn editor_cursor(
    app: &App,
    layout: Layout,
    wrapped_rows: Option<&[VisualRow]>,
) -> Option<(u16, u16)> {
    let editor = app.workspace().active();
    if let Some(rows) = wrapped_rows {
        let point = VisualMetrics::new(layout.content_width, app.config().tab_width, true)
            .point_for_cursor(&editor.document, editor.cursor)
            .ok()?;
        let y = rows.iter().position(|row| row.anchor == point.row)?;
        if y >= layout.content_height || point.column >= layout.content_width {
            return None;
        }
        return Some((
            (layout.gutter_width + point.column) as u16,
            (layout.content_y + y) as u16,
        ));
    }
    let position = editor.position(app.config().tab_width);
    let y = position.line.checked_sub(editor.viewport.top_line)?;
    if y >= layout.content_height {
        return None;
    }
    let x = position
        .visual_column
        .saturating_sub(editor.viewport.left_column);
    if x >= layout.content_width {
        return None;
    }
    Some((
        (layout.gutter_width + x) as u16,
        (layout.content_y + y) as u16,
    ))
}

fn paint_row(output: &mut impl Write, y: u16, row: &Row) -> io::Result<()> {
    queue!(output, MoveTo(0, y))?;
    let mut previous_style: Option<Style> = None;
    for span in &row.spans {
        if let Some(previous) = previous_style {
            if previous.fg != span.style.fg {
                queue!(output, SetForegroundColor(span.style.fg))?;
            }
            if previous.bg != span.style.bg {
                queue!(output, SetBackgroundColor(span.style.bg))?;
            }
            if previous.bold != span.style.bold {
                queue!(
                    output,
                    SetAttribute(if span.style.bold {
                        Attribute::Bold
                    } else {
                        Attribute::NormalIntensity
                    })
                )?;
            }
            if previous.underlined != span.style.underlined {
                queue!(
                    output,
                    SetAttribute(if span.style.underlined {
                        Attribute::Underlined
                    } else {
                        Attribute::NoUnderline
                    })
                )?;
            }
        } else {
            queue!(
                output,
                SetForegroundColor(span.style.fg),
                SetBackgroundColor(span.style.bg),
                SetAttribute(if span.style.bold {
                    Attribute::Bold
                } else {
                    Attribute::NormalIntensity
                }),
                SetAttribute(if span.style.underlined {
                    Attribute::Underlined
                } else {
                    Attribute::NoUnderline
                })
            )?;
        }
        queue!(output, Print(&span.text))?;
        previous_style = Some(span.style);
    }
    queue!(
        output,
        ResetColor,
        SetAttribute(Attribute::Reset),
        Clear(ClearType::UntilNewLine)
    )
}

fn display_grapheme<'a>(
    grapheme: &'a str,
    column: usize,
    tab_width: usize,
) -> (Cow<'a, str>, usize) {
    let width = grapheme_cell_width(grapheme, column, tab_width);
    if grapheme == "\t" {
        return (Cow::Owned(" ".repeat(width)), width);
    }
    if grapheme.chars().any(char::is_control) {
        return (Cow::Borrowed("�"), 1);
    }
    if UnicodeWidthStr::width(grapheme) == 0 {
        (Cow::Borrowed("◌"), 1)
    } else {
        (Cow::Borrowed(grapheme), width)
    }
}

fn ranges_overlap(left: &std::ops::Range<usize>, right: &std::ops::Range<usize>) -> bool {
    left.start < right.end && right.start < left.end
}

fn fit_text(text: &str, max_width: usize) -> String {
    let text = sanitize_display_text(text);
    let text = text.as_str();
    if visual_width(text, 4) <= max_width {
        return text.to_owned();
    }
    if max_width == 0 {
        return String::new();
    }
    let mut output = String::new();
    let mut width = 0;
    let reserve_ellipsis = max_width > 1;
    let content_limit = max_width.saturating_sub(usize::from(reserve_ellipsis));
    for grapheme in text.graphemes(true) {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if width + grapheme_width > content_limit {
            break;
        }
        output.push_str(grapheme);
        width += grapheme_width;
    }
    if reserve_ellipsis {
        output.push('…');
    }
    output
}

fn sanitize_display_text(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for grapheme in text.graphemes(true) {
        if grapheme.chars().any(char::is_control) {
            output.push('�');
        } else if UnicodeWidthStr::width(grapheme) == 0 {
            output.push('◌');
        } else {
            output.push_str(grapheme);
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::session::LayoutFlags;
    use crate::{Document, Editor, Workspace};
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    fn app_with_text(text: &str) -> App {
        let mut workspace = Workspace::new(None).unwrap();
        let replaced = workspace
            .replace_editor(0, Editor::new(Document::from_text(text)))
            .expect("the initial test buffer exists");
        drop(replaced);
        App::new_ready_for_test(workspace, Config::default())
    }

    fn row_text(row: &Row) -> String {
        row.spans.iter().map(|span| span.text.as_str()).collect()
    }

    #[test]
    fn layout_handles_tiny_dimensions_without_underflow() {
        let layout = Layout::calculate(0, 0, 100, true);
        assert!(layout.too_small);
        assert_eq!(layout.content_height, 0);
        assert_eq!(layout.content_width, 0);
    }

    #[test]
    fn fitting_does_not_split_wide_graphemes() {
        assert_eq!(fit_text("ab界cd", 4), "ab…");
        assert_eq!(fit_text("abcdef", 4), "abc…");
        assert_eq!(fit_text("abc", 0), "");
    }

    #[test]
    fn control_graphemes_are_visible() {
        assert_eq!(display_grapheme("\0", 0, 4), (Cow::Borrowed("�"), 1));
    }

    #[test]
    fn paint_row_sets_initial_style_emits_only_changes_and_resets() {
        let first = Style::new(Color::AnsiValue(10), Color::AnsiValue(20))
            .bold()
            .underlined();
        let second = Style::new(Color::AnsiValue(11), Color::AnsiValue(20))
            .bold()
            .underlined();
        let row = Row {
            spans: vec![
                Span {
                    text: "left".to_owned(),
                    style: first,
                },
                Span {
                    text: "right".to_owned(),
                    style: second,
                },
            ],
            width: 9,
        };

        let mut actual = Vec::new();
        paint_row(&mut actual, 7, &row).unwrap();
        let mut expected = Vec::new();
        queue!(
            expected,
            MoveTo(0, 7),
            SetForegroundColor(first.fg),
            SetBackgroundColor(first.bg),
            SetAttribute(Attribute::Bold),
            SetAttribute(Attribute::Underlined),
            Print("left"),
            SetForegroundColor(second.fg),
            Print("right"),
            ResetColor,
            SetAttribute(Attribute::Reset),
            Clear(ClearType::UntilNewLine)
        )
        .unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn initial_draw_resets_terminal_style_before_painting() {
        let mut app = app_with_text("text");
        let mut renderer = Renderer::default();
        let mut actual = Vec::new();
        renderer.draw(&mut actual, &mut app, (80, 24)).unwrap();

        let mut expected_prefix = Vec::new();
        queue!(
            expected_prefix,
            Hide,
            SetAttribute(Attribute::Reset),
            ResetColor,
            Clear(ClearType::All)
        )
        .unwrap();
        assert!(actual.starts_with(&expected_prefix));
    }

    #[test]
    fn row_text_cannot_inject_terminal_controls() {
        let mut row = Row::default();
        row.push(
            "unsafe\x1b[2J\nname",
            Style::new(Color::White, Color::Black),
        );
        assert_eq!(row.spans[0].text, "unsafe�[2J�name");
        assert!(!row.spans[0].text.chars().any(char::is_control));
    }

    #[test]
    fn fitted_untrusted_controls_and_zero_width_text_stays_within_row() {
        let fitted = fit_text("a\0\u{200b}bcdef", 4);
        assert_eq!(fitted, "a�◌…");
        assert_eq!(UnicodeWidthStr::width(fitted.as_str()), 4);
    }

    #[test]
    fn header_keeps_a_late_active_buffer_visible_with_left_overflow_marker() {
        let mut app = app_with_text("");
        for index in 1..8 {
            app.workspace_mut()
                .open_virtual(format!("earlier-{index}.rs"), "");
        }
        app.workspace_mut().open_virtual("active.rs", "");

        let header = build_header(&app, 40);
        let rendered = row_text(&header);
        assert!(rendered.contains("9:active.rs"), "{rendered:?}");
        assert!(rendered.contains('‹'), "{rendered:?}");
        assert!(!rendered.contains("1:[untitled]"), "{rendered:?}");
        assert_eq!(header.width, 40);
        assert!(header.spans.iter().any(|span| {
            span.text.contains("9:active.rs") && span.style.bg == Color::AnsiValue(24)
        }));
    }

    #[test]
    fn header_window_never_excludes_the_active_index() {
        let widths = [10, 10, 10, 10, 10, 10];
        let window = header_tab_window(&widths, 4, 25);
        assert!(window.contains(&4));
        assert!(window.start > 0);
        assert!(window.end < widths.len());
        assert!(header_window_width(&widths, window.start, window.end) <= 25);
    }

    #[test]
    fn long_prompt_window_follows_end_and_middle_cursor_positions() {
        let input = "01234567890123456789👩‍💻abcdefghij";
        let at_end = layout_prompt_window(" : ", input, input.chars().count(), 20);
        assert!(at_end.hidden_left);
        assert!(!at_end.hidden_right);
        assert!(at_end.input.starts_with('‹'));
        assert!(at_end.input.contains("👩‍💻"));
        assert_eq!(at_end.cursor_x, 19);
        assert!(visual_width(&format!("{}{}", at_end.prefix, at_end.input), 4) <= 20);

        let in_middle = layout_prompt_window(" : ", &"x".repeat(60), 30, 20);
        assert!(in_middle.hidden_left);
        assert!(in_middle.hidden_right);
        assert!(in_middle.input.starts_with('‹'));
        assert!(in_middle.input.ends_with('›'));
        assert_eq!(in_middle.cursor_x, 19);
        assert!(visual_width(&format!("{}{}", in_middle.prefix, in_middle.input), 4) <= 20);
    }

    #[test]
    fn prompt_window_snaps_inside_grapheme_and_sanitizes_display_cells() {
        let input = "a👩‍💻\0\u{200b}界b";
        let inside_emoji = layout_prompt_window(" prompt › ", input, 2, 40);
        assert_eq!(
            inside_emoji.cursor_x,
            visual_width(&inside_emoji.prefix, 4) + 1
        );
        assert!(inside_emoji.input.contains("👩‍💻"));
        assert!(inside_emoji.input.contains('�'));
        assert!(inside_emoji.input.contains('◌'));
        assert!(!inside_emoji.input.chars().any(char::is_control));

        let long_prefix = layout_prompt_window(
            " recovery (Enter restore · V view · D discard) › ",
            "journal-name",
            "journal-name".chars().count(),
            32,
        );
        assert!(visual_width(&long_prefix.prefix, 4) <= 16);
        assert!(long_prefix.cursor_x < 32);
        assert!(long_prefix.input.contains("journal-name"));
    }

    #[test]
    fn frame_footer_and_prompt_cursor_share_one_window() {
        let mut app = app_with_text("");
        app.handle_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        app.handle_event(Event::Key(KeyEvent::new(
            KeyCode::Char(':'),
            KeyModifiers::NONE,
        )));
        app.handle_event(Event::Paste(
            "edit /a/very/long/path/that/continues/well/past/the/ipad/footer.rs".to_owned(),
        ));
        let layout = Layout::calculate(
            40,
            12,
            app.workspace().active().document.line_count(),
            app.config().line_numbers,
        );
        let expected = prompt_window(app.prompt().unwrap(), layout.width);
        let (rows, cursor) = build_frame(&mut app, layout, layout, 0);
        let footer = row_text(&rows[layout.footer_y]);

        assert_eq!(
            cursor,
            Some((expected.cursor_x as u16, layout.footer_y as u16))
        );
        assert!(footer.starts_with(&format!("{}{}", expected.prefix, expected.input)));
        assert!(expected.hidden_left);
        assert_eq!(rows[layout.footer_y].width, layout.width);
    }

    #[test]
    fn soft_wrap_renders_continuations_and_exact_width_caret_row() {
        let mut app = app_with_text(&"a".repeat(80));
        app.apply_session_layout(LayoutFlags {
            soft_wrap: true,
            ..LayoutFlags::default()
        });
        let layout = Layout::calculate(
            40,
            12,
            app.workspace().active().document.line_count(),
            app.config().line_numbers,
        );
        app.workspace_mut().active_mut().cursor = layout.content_width;
        app.prepare_viewport(layout);
        let (rows, cursor) = build_frame(&mut app, layout, layout, 0);

        assert!(row_text(&rows[layout.content_y]).contains(&"a".repeat(layout.content_width)));
        assert!(row_text(&rows[layout.content_y + 1]).contains('↪'));
        assert_eq!(
            cursor,
            Some((layout.gutter_width as u16, (layout.content_y + 1) as u16))
        );
        assert!(rows.iter().all(|row| row.width <= layout.width));
    }

    #[test]
    fn project_sidebar_renders_left_of_editor_and_offsets_cursor() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("src/nested")).unwrap();
        let active = directory.path().join("src/nested/main.rs");
        std::fs::write(&active, "abc\n").unwrap();
        std::fs::write(directory.path().join("src/lib.rs"), "pub fn lib() {}\n").unwrap();
        let workspace =
            Workspace::from_path(Some(active), Some(directory.path().to_path_buf())).unwrap();
        let mut app = App::new_ready_for_test(workspace, Config::default());
        app.apply_session_layout(LayoutFlags {
            workspace_tree_visible: true,
            ..LayoutFlags::default()
        });
        app.workspace_mut().active_mut().cursor = 1;

        let layout = Layout::calculate(
            100,
            14,
            app.workspace().active().document.line_count(),
            app.config().line_numbers,
        );
        let sidebar_width = project_sidebar_width(&app, layout);
        assert!(sidebar_width > 0);
        let editor_layout = Layout::calculate(
            (layout.width - sidebar_width) as u16,
            14,
            app.workspace().active().document.line_count(),
            app.config().line_numbers,
        );
        app.prepare_viewport(editor_layout);
        let (rows, cursor) = build_frame(&mut app, layout, editor_layout, sidebar_width);
        let content = row_text(&rows[layout.content_y]);

        assert!(content.contains("PROJECT"));
        assert!(content.contains("abc"));
        assert!(content.contains('│'));
        assert_eq!(
            cursor,
            Some((
                (sidebar_width + editor_layout.gutter_width + 1) as u16,
                layout.content_y as u16
            ))
        );
        assert!(rows.iter().all(|row| row.width <= layout.width));
    }

    #[test]
    fn an_empty_partial_result_overlay_still_renders_its_notice() {
        let layout = Layout::calculate(80, 24, 1, true);
        let mut rows = vec![Row::default(); layout.height];
        let items = Vec::new();
        overlay_candidates(
            layout,
            &mut rows,
            crate::app::OverlayView {
                title: "PROJECT SEARCH",
                items: &items,
                selected: 0,
                notice: Some("Partial results: project scan safety limit reached"),
            },
        );
        let rendered = rows
            .iter()
            .flat_map(|row| row.spans.iter())
            .map(|span| span.text.as_str())
            .collect::<String>();
        assert!(rendered.contains("PROJECT SEARCH"));
        assert!(rendered.contains("Partial results"));
    }

    #[test]
    fn candidate_overlay_hit_testing_accepts_only_visible_item_rows() {
        let layout = Layout::calculate(80, 10, 1, true);
        let overlay = CandidateOverlayLayout::calculate(layout, 8, 7, true).unwrap();

        assert_eq!(
            overlay,
            CandidateOverlayLayout {
                x: 2,
                y: 1,
                width: 76,
                shown: 5,
                window_start: 3,
                notice_rows: 1,
            }
        );
        assert_eq!(overlay.item_at(overlay.x, overlay.y + 1), Some(3));
        assert_eq!(
            overlay.item_at(overlay.x + overlay.width - 1, overlay.y + overlay.shown),
            Some(7)
        );
        assert_eq!(overlay.item_at(overlay.x, overlay.y), None);
        assert_eq!(
            overlay.item_at(overlay.x, overlay.notice_y().unwrap()),
            None
        );
        assert_eq!(overlay.item_at(overlay.x - 1, overlay.y + 1), None);
        assert_eq!(
            overlay.item_at(overlay.x + overlay.width, overlay.y + 1),
            None
        );
    }

    #[test]
    fn candidate_overlay_layout_handles_notice_only_and_absent_overlays() {
        let layout = Layout::calculate(80, 24, 1, true);
        let notice_only = CandidateOverlayLayout::calculate(layout, 0, 0, true).unwrap();

        assert_eq!(notice_only.shown, 0);
        assert_eq!(notice_only.window_start, 0);
        assert_eq!(notice_only.item_at(notice_only.x, notice_only.y + 1), None);
        assert_eq!(
            notice_only.notice_y(),
            Some(notice_only.y + notice_only.shown + 1)
        );
        assert_eq!(CandidateOverlayLayout::calculate(layout, 0, 0, false), None);
    }

    #[test]
    fn candidate_overlay_scrolls_to_keep_selection_visible() {
        let layout = Layout::calculate(80, 24, 1, true);
        let mut rows = vec![Row::default(); layout.height];
        let items = (0..20)
            .map(|index| format!("item-{index:02}"))
            .collect::<Vec<_>>();
        overlay_candidates(
            layout,
            &mut rows,
            crate::app::OverlayView {
                title: "QUICK OPEN",
                items: &items,
                selected: 17,
                notice: None,
            },
        );
        let rendered = rows.iter().map(row_text).collect::<String>();
        assert!(rendered.contains("item-17"));
        assert!(!rendered.contains("item-00"));
    }
}
