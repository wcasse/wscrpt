use std::ops::{Deref, DerefMut, Range};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::document::{Document, DocumentError, EditKind};
use crate::text::{
    char_for_visual_column, next_grapheme_end, previous_grapheme_start, visual_width,
};
use crate::visual::{VisualAnchor, VisualMetrics};
use crate::wrap::WrapError;

static EDITOR_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Viewport {
    pub top_line: usize,
    /// Line-local character offset anchoring the first soft-wrapped row.
    /// This survives terminal-width changes better than a segment number.
    pub top_wrap_char: usize,
    pub left_column: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CursorPosition {
    pub line: usize,
    pub char_column: usize,
    pub visual_column: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LineCommentToggle {
    Commented,
    Uncommented,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LineCommentToggleOutcome {
    pub mode: LineCommentToggle,
    pub lines_changed: usize,
}

#[derive(Clone, Debug)]
pub struct Editor {
    id: u64,
    state: EditorState,
}

/// Mutable editor contents, deliberately separated from the stable workspace
/// identity stored by [`Editor`]. Workspace mutation guards expose this state
/// so replacing or forgetting a guard cannot change an editor ID behind the
/// workspace index.
#[derive(Clone, Debug)]
pub struct EditorState {
    pub document: Document,
    pub cursor: usize,
    pub anchor: Option<usize>,
    pub viewport: Viewport,
    desired_visual_column: Option<usize>,
}

impl Editor {
    pub fn new(document: Document) -> Self {
        Self {
            id: EDITOR_ID.fetch_add(1, Ordering::Relaxed),
            state: EditorState {
                document,
                cursor: 0,
                anchor: None,
                viewport: Viewport::default(),
                desired_visual_column: None,
            },
        }
    }

    /// Stable workspace identity. The value is read-only so callers cannot
    /// invalidate Workspace's editor-ID index while mutating editor contents.
    ///
    /// ```compile_fail
    /// let mut editor = wscrpt::Editor::new(wscrpt::Document::new());
    /// editor.id = 7;
    /// ```
    pub fn id(&self) -> u64 {
        self.id
    }
}

impl Deref for Editor {
    type Target = EditorState;

    fn deref(&self) -> &Self::Target {
        &self.state
    }
}

impl DerefMut for Editor {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.state
    }
}

impl EditorState {
    pub fn position(&self, tab_width: usize) -> CursorPosition {
        let line = self.document.char_to_line(self.cursor);
        let start = self.document.line_start_char(line);
        let char_column = self.cursor.saturating_sub(start);
        let prefix = self.document.slice(start..self.cursor);
        CursorPosition {
            line,
            char_column,
            visual_column: visual_width(&prefix, tab_width),
        }
    }

    pub fn selection(&self) -> Option<Range<usize>> {
        self.anchor.and_then(|anchor| {
            let start = anchor.min(self.cursor);
            let end = anchor.max(self.cursor);
            (start != end).then_some(start..end)
        })
    }

    pub fn selected_text(&self) -> Option<String> {
        self.selection().map(|range| self.document.slice(range))
    }

    pub fn clear_selection(&mut self) {
        self.anchor = None;
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.document.len_chars();
        self.break_movement_group();
    }

    pub fn select_lines(&mut self) -> usize {
        let (start_line, end_line) = self.selected_line_range();
        let start = self.document.line_start_char(start_line);
        let end = if end_line + 1 < self.document.line_count() {
            self.document.line_start_char(end_line + 1)
        } else {
            self.document.len_chars()
        };
        self.anchor = Some(start);
        self.cursor = end;
        self.break_movement_group();
        end_line - start_line + 1
    }

    pub fn insert(&mut self, text: &str, kind: EditKind) -> Result<(), DocumentError> {
        let range = self.selection().unwrap_or(self.cursor..self.cursor);
        let before = self.cursor;
        let after = range.start + text.chars().count();
        self.document.edit(range, text, before, after, kind)?;
        self.cursor = after;
        self.anchor = None;
        self.desired_visual_column = None;
        Ok(())
    }

    /// Replace the complete buffer as one undoable service edit. Language
    /// servers express a formatting/refactor as several ranges against one
    /// immutable version; callers assemble and validate the result first, then
    /// publish it atomically through this method.
    pub fn replace_all_from_service(&mut self, text: &str) -> Result<(), DocumentError> {
        let before = self.cursor;
        let after = before.min(text.chars().count());
        self.document.edit(
            0..self.document.len_chars(),
            text,
            before,
            after,
            EditKind::Replace,
        )?;
        self.cursor = after;
        self.anchor = None;
        self.desired_visual_column = None;
        self.document.break_undo_group();
        Ok(())
    }

    /// Replaces every non-overlapping, case-sensitive literal occurrence as a
    /// single undo step. An empty needle and a no-match search are no-ops.
    pub fn replace_all_literal(
        &mut self,
        needle: &str,
        replacement: &str,
    ) -> Result<usize, DocumentError> {
        if needle.is_empty() {
            return Ok(0);
        }
        let original = self.document.text();
        let count = original.matches(needle).count();
        if count == 0 {
            return Ok(0);
        }
        let updated = original.replace(needle, replacement);
        self.replace_all_from_service(&updated)?;
        Ok(count)
    }

    /// Replace every match of a compiled pattern as one undo step.
    ///
    /// Accepts literal and `re:` / `re:i:` patterns via [`crate::pattern::Pattern`].
    /// Regex replacements may use `$n` capture templates.
    pub fn replace_all_pattern(
        &mut self,
        pattern: &crate::pattern::Pattern,
        replacement: &str,
    ) -> Result<usize, String> {
        let original = self.document.text();
        let (updated, count) = pattern
            .replace_all(
                &original,
                replacement,
                crate::document::MAX_DOCUMENT_BYTES as usize,
            )
            .map_err(|error| error.to_string())?;
        if count == 0 {
            return Ok(0);
        }
        self.replace_all_from_service(&updated)
            .map_err(|error| error.to_string())?;
        Ok(count)
    }

    pub fn toggle_line_comment(
        &mut self,
        marker: &str,
    ) -> Result<Option<LineCommentToggleOutcome>, DocumentError> {
        if marker.is_empty() {
            return Ok(None);
        }
        let (start_line, end_line) = self.selected_line_range();
        let mut line_infos = Vec::new();
        for line in start_line..=end_line {
            let text = self.document.line(line);
            let body = text.strip_suffix('\n').unwrap_or(&text);
            if body.trim().is_empty() {
                continue;
            }
            let indent_chars = body
                .chars()
                .take_while(|character| matches!(character, ' ' | '\t'))
                .count();
            let after_indent = &body[indent_chars..];
            line_infos.push((line, indent_chars, after_indent.starts_with(marker)));
        }
        if line_infos.is_empty() {
            return Ok(None);
        }

        let uncomment = line_infos.iter().all(|(_, _, has_marker)| *has_marker);
        let range_start = self.document.line_start_char(start_line);
        let range_end = if end_line + 1 < self.document.line_count() {
            self.document.line_start_char(end_line + 1)
        } else {
            self.document.len_chars()
        };
        let original = self.document.slice(range_start..range_end);
        let marker_chars = marker.chars().count();
        let mut replacement = String::new();
        let mut changes = Vec::new();
        let mut lines_changed = 0;

        for line in start_line..=end_line {
            let text = self.document.line(line);
            let has_newline = text.ends_with('\n');
            let body = text.strip_suffix('\n').unwrap_or(&text);
            if body.trim().is_empty() {
                replacement.push_str(&text);
                continue;
            }
            let indent_chars = body
                .chars()
                .take_while(|character| matches!(character, ' ' | '\t'))
                .count();
            let indent_bytes = indent_chars;
            if uncomment {
                let after_marker = &body[indent_bytes + marker.len()..];
                let remove_space = after_marker.starts_with(' ');
                let remove_bytes = marker.len() + usize::from(remove_space);
                let remove_chars = marker_chars + usize::from(remove_space);
                replacement.push_str(&body[..indent_bytes]);
                replacement.push_str(&body[indent_bytes + remove_bytes..]);
                changes.push(LineCommentChange {
                    line,
                    column: indent_chars,
                    delta: -(remove_chars as isize),
                });
            } else {
                replacement.push_str(&body[..indent_bytes]);
                replacement.push_str(marker);
                replacement.push(' ');
                replacement.push_str(&body[indent_bytes..]);
                changes.push(LineCommentChange {
                    line,
                    column: indent_chars,
                    delta: (marker_chars + 1) as isize,
                });
            }
            if has_newline {
                replacement.push('\n');
            }
            lines_changed += 1;
        }

        if replacement == original {
            return Ok(None);
        }
        let before = self.cursor;
        let after_cursor = self.adjust_after_line_comment_toggle(self.cursor, &changes);
        let after_anchor = self
            .anchor
            .map(|anchor| self.adjust_after_line_comment_toggle(anchor, &changes));
        self.document.edit(
            range_start..range_end,
            &replacement,
            before,
            after_cursor,
            EditKind::Replace,
        )?;
        self.cursor = after_cursor;
        self.anchor = after_anchor;
        self.desired_visual_column = None;
        self.document.break_undo_group();
        Ok(Some(LineCommentToggleOutcome {
            mode: if uncomment {
                LineCommentToggle::Uncommented
            } else {
                LineCommentToggle::Commented
            },
            lines_changed,
        }))
    }

    pub fn duplicate_lines(&mut self) -> Result<usize, DocumentError> {
        let had_selection = self.selection().is_some();
        let (start_line, end_line) = self.selected_line_range();
        let range_start = self.document.line_start_char(start_line);
        let range_end = if end_line + 1 < self.document.line_count() {
            self.document.line_start_char(end_line + 1)
        } else {
            self.document.len_chars()
        };
        let copied = self.document.slice(range_start..range_end);
        if copied.is_empty() {
            return Ok(0);
        }
        let needs_leading_newline =
            range_end == self.document.len_chars() && !copied.ends_with('\n');
        let insertion = if needs_leading_newline {
            format!("\n{copied}")
        } else {
            copied.clone()
        };
        let duplicate_start = range_end + usize::from(needs_leading_newline);
        let copied_chars = copied.chars().count();
        let cursor_after = if had_selection {
            duplicate_start + copied_chars
        } else {
            duplicate_start + self.cursor.saturating_sub(range_start)
        };
        let anchor_after = had_selection.then_some(duplicate_start);
        let before = self.cursor;
        self.document.edit(
            range_end..range_end,
            &insertion,
            before,
            cursor_after,
            EditKind::Replace,
        )?;
        self.cursor = cursor_after;
        self.anchor = anchor_after;
        self.desired_visual_column = None;
        self.document.break_undo_group();
        Ok(end_line - start_line + 1)
    }

    pub fn delete_lines(&mut self) -> Result<usize, DocumentError> {
        let (start_line, end_line) = self.selected_line_range();
        let range_start = self.document.line_start_char(start_line);
        let range_end = if end_line + 1 < self.document.line_count() {
            self.document.line_start_char(end_line + 1)
        } else {
            self.document.len_chars()
        };
        if range_start == range_end {
            return Ok(0);
        }
        let before = self.cursor;
        self.document.edit(
            range_start..range_end,
            "",
            before,
            range_start,
            EditKind::Replace,
        )?;
        self.cursor = range_start.min(self.document.len_chars());
        self.anchor = None;
        self.desired_visual_column = None;
        self.document.break_undo_group();
        Ok(end_line - start_line + 1)
    }

    pub fn move_lines_up(&mut self) -> Result<usize, DocumentError> {
        if self.document.is_read_only() {
            self.document.edit(
                self.cursor..self.cursor,
                "",
                self.cursor,
                self.cursor,
                EditKind::Replace,
            )?;
        }
        let (start_line, end_line) = self.selected_line_range();
        if start_line == 0 {
            return Ok(0);
        }
        let previous_start = self.document.line_start_char(start_line - 1);
        let block_start = self.document.line_start_char(start_line);
        let block_end = if end_line + 1 < self.document.line_count() {
            self.document.line_start_char(end_line + 1)
        } else {
            self.document.len_chars()
        };
        if block_start == block_end {
            return Ok(0);
        }
        let previous = self.document.slice(previous_start..block_start);
        let block = self.document.slice(block_start..block_end);
        let replacement = format!("{block}{previous}");
        let before_cursor = self.cursor;
        let before_anchor = self.anchor;
        let block_len = block.chars().count();
        let cursor_after = map_position_moving_lines_up(self.cursor, previous_start, block_start);
        let anchor_after = before_anchor
            .map(|anchor| map_position_moving_lines_up(anchor, previous_start, block_start));
        self.document.edit(
            previous_start..block_end,
            &replacement,
            before_cursor,
            cursor_after,
            EditKind::Replace,
        )?;
        self.cursor = cursor_after;
        self.anchor = anchor_after.filter(|anchor| *anchor != self.cursor);
        self.desired_visual_column = None;
        if self.anchor.is_some() {
            let moved_end = previous_start + block_len;
            if self.cursor == previous_start {
                self.anchor = Some(moved_end);
            } else if self.anchor == Some(previous_start) {
                self.cursor = moved_end;
            }
        }
        self.document.break_undo_group();
        Ok(end_line - start_line + 1)
    }

    pub fn move_lines_down(&mut self) -> Result<usize, DocumentError> {
        if self.document.is_read_only() {
            self.document.edit(
                self.cursor..self.cursor,
                "",
                self.cursor,
                self.cursor,
                EditKind::Replace,
            )?;
        }
        let (start_line, end_line) = self.selected_line_range();
        if end_line + 1 >= self.document.line_count() {
            return Ok(0);
        }
        let block_start = self.document.line_start_char(start_line);
        let block_end = self.document.line_start_char(end_line + 1);
        let next_end = if end_line + 2 < self.document.line_count() {
            self.document.line_start_char(end_line + 2)
        } else {
            self.document.len_chars()
        };
        if block_start == block_end || block_end == next_end {
            return Ok(0);
        }
        let block = self.document.slice(block_start..block_end);
        let next = self.document.slice(block_end..next_end);
        let replacement = format!("{next}{block}");
        let before_cursor = self.cursor;
        let before_anchor = self.anchor;
        let next_len = next.chars().count();
        let cursor_after =
            map_position_moving_lines_down(self.cursor, block_start, block_end, next_end);
        let anchor_after = before_anchor
            .map(|anchor| map_position_moving_lines_down(anchor, block_start, block_end, next_end));
        self.document.edit(
            block_start..next_end,
            &replacement,
            before_cursor,
            cursor_after,
            EditKind::Replace,
        )?;
        self.cursor = cursor_after;
        self.anchor = anchor_after.filter(|anchor| *anchor != self.cursor);
        self.desired_visual_column = None;
        if self.anchor.is_some() {
            let moved_start = block_start + next_len;
            if self.cursor == block_end {
                self.anchor = Some(moved_start);
            } else if self.anchor == Some(block_end) {
                self.cursor = moved_start;
            }
        }
        self.document.break_undo_group();
        Ok(end_line - start_line + 1)
    }

    pub fn indent_lines(
        &mut self,
        tab_width: usize,
        insert_spaces: bool,
    ) -> Result<usize, DocumentError> {
        if self.document.is_read_only() {
            self.document.edit(
                self.cursor..self.cursor,
                "",
                self.cursor,
                self.cursor,
                EditKind::Replace,
            )?;
        }
        let (start_line, end_line) = self.selected_line_range();
        let range_start = self.document.line_start_char(start_line);
        let range_end = if end_line + 1 < self.document.line_count() {
            self.document.line_start_char(end_line + 1)
        } else {
            self.document.len_chars()
        };
        let unit = if insert_spaces {
            " ".repeat(tab_width.max(1))
        } else {
            "\t".to_owned()
        };
        let unit_chars = unit.chars().count();
        let original = self.document.slice(range_start..range_end);
        let mut replacement = String::new();
        let mut changes = Vec::new();
        for line in start_line..=end_line {
            let text = self.document.line(line);
            replacement.push_str(&unit);
            replacement.push_str(&text);
            changes.push(LineIndentChange {
                line,
                column: 0,
                delta: unit_chars as isize,
            });
        }
        if replacement == original {
            return Ok(0);
        }
        let before = self.cursor;
        let after_cursor = self.adjust_after_line_indent_change(self.cursor, &changes);
        let after_anchor = self
            .anchor
            .map(|anchor| self.adjust_after_line_indent_change(anchor, &changes));
        self.document.edit(
            range_start..range_end,
            &replacement,
            before,
            after_cursor,
            EditKind::Replace,
        )?;
        self.cursor = after_cursor;
        self.anchor = after_anchor.filter(|anchor| *anchor != self.cursor);
        self.desired_visual_column = None;
        self.document.break_undo_group();
        Ok(end_line - start_line + 1)
    }

    pub fn outdent_lines(&mut self, tab_width: usize) -> Result<usize, DocumentError> {
        if self.document.is_read_only() {
            self.document.edit(
                self.cursor..self.cursor,
                "",
                self.cursor,
                self.cursor,
                EditKind::Replace,
            )?;
        }
        let (start_line, end_line) = self.selected_line_range();
        let range_start = self.document.line_start_char(start_line);
        let range_end = if end_line + 1 < self.document.line_count() {
            self.document.line_start_char(end_line + 1)
        } else {
            self.document.len_chars()
        };
        let original = self.document.slice(range_start..range_end);
        let mut replacement = String::new();
        let mut changes = Vec::new();
        let mut lines_changed = 0;

        for line in start_line..=end_line {
            let text = self.document.line(line);
            let has_newline = text.ends_with('\n');
            let body = text.strip_suffix('\n').unwrap_or(&text);
            let remove_chars = if body.starts_with('\t') {
                1
            } else {
                body.chars()
                    .take_while(|character| *character == ' ')
                    .take(tab_width.max(1))
                    .count()
            };
            if remove_chars == 0 {
                replacement.push_str(&text);
            } else {
                let remove_bytes = body
                    .char_indices()
                    .nth(remove_chars)
                    .map_or(body.len(), |(index, _)| index);
                replacement.push_str(&body[remove_bytes..]);
                if has_newline {
                    replacement.push('\n');
                }
                changes.push(LineIndentChange {
                    line,
                    column: 0,
                    delta: -(remove_chars as isize),
                });
                lines_changed += 1;
            }
        }

        if replacement == original {
            return Ok(0);
        }
        let before = self.cursor;
        let after_cursor = self.adjust_after_line_indent_change(self.cursor, &changes);
        let after_anchor = self
            .anchor
            .map(|anchor| self.adjust_after_line_indent_change(anchor, &changes));
        self.document.edit(
            range_start..range_end,
            &replacement,
            before,
            after_cursor,
            EditKind::Replace,
        )?;
        self.cursor = after_cursor;
        self.anchor = after_anchor.filter(|anchor| *anchor != self.cursor);
        self.desired_visual_column = None;
        self.document.break_undo_group();
        Ok(lines_changed)
    }

    pub fn insert_newline_with_indent(&mut self, tab_width: usize) -> Result<(), DocumentError> {
        let position = self.position(tab_width);
        let line = self.document.line(position.line);
        let leading: String = line
            .chars()
            .take_while(|character| matches!(character, ' ' | '\t'))
            .collect();
        let before_cursor = self
            .document
            .slice(self.document.line_start_char(position.line)..self.cursor);
        let after_cursor = self
            .document
            .slice(self.cursor..self.document.line_end_char(position.line));
        let extra_indent = before_cursor
            .trim_end()
            .chars()
            .last()
            .is_some_and(|character| matches!(character, '{' | '[' | '(' | ':'));
        let matching_closer = after_cursor
            .trim_start()
            .chars()
            .next()
            .is_some_and(|character| matches!(character, '}' | ']' | ')'));
        let unit = " ".repeat(tab_width.max(1));

        if extra_indent && matching_closer {
            let insertion = format!("\n{leading}{unit}\n{leading}");
            self.insert(&insertion, EditKind::Insert)?;
            self.cursor -= leading.chars().count() + 1;
        } else {
            let insertion = format!("\n{leading}{}", if extra_indent { &unit } else { "" });
            self.insert(&insertion, EditKind::Insert)?;
        }
        self.document.break_undo_group();
        Ok(())
    }

    pub fn insert_tab(
        &mut self,
        tab_width: usize,
        insert_spaces: bool,
    ) -> Result<(), DocumentError> {
        if insert_spaces {
            let column = self.position(tab_width).visual_column;
            let count = tab_width.max(1) - column % tab_width.max(1);
            self.insert(&" ".repeat(count), EditKind::Insert)
        } else {
            self.insert("\t", EditKind::Insert)
        }
    }

    pub fn backspace(&mut self) -> Result<(), DocumentError> {
        if self.selection().is_some() {
            return self.insert("", EditKind::Replace);
        }
        if self.cursor == 0 {
            return Ok(());
        }
        let line = self.document.char_to_line(self.cursor);
        let start = self.document.line_start_char(line);
        let prefix = self.document.slice(start..self.cursor);
        let previous = start + previous_grapheme_start(&prefix, prefix.chars().count());
        self.document.edit(
            previous..self.cursor,
            "",
            self.cursor,
            previous,
            EditKind::Backspace,
        )?;
        self.cursor = previous;
        self.desired_visual_column = None;
        Ok(())
    }

    pub fn delete_forward(&mut self) -> Result<(), DocumentError> {
        if self.selection().is_some() {
            return self.insert("", EditKind::Replace);
        }
        if self.cursor >= self.document.len_chars() {
            return Ok(());
        }
        let line = self.document.char_to_line(self.cursor);
        let end = self.document.line_end_char(line);
        let next = if self.cursor == end && self.cursor < self.document.len_chars() {
            self.cursor + 1
        } else {
            let suffix = self.document.slice(self.cursor..end);
            self.cursor + next_grapheme_end(&suffix, 0)
        };
        self.document.edit(
            self.cursor..next,
            "",
            self.cursor,
            self.cursor,
            EditKind::Delete,
        )?;
        self.desired_visual_column = None;
        Ok(())
    }

    pub fn move_left(&mut self, selecting: bool) {
        self.prepare_selection(selecting);
        if self.cursor > 0 {
            let line = self.document.char_to_line(self.cursor);
            let start = self.document.line_start_char(line);
            self.cursor = if self.cursor == start {
                self.cursor - 1
            } else {
                let prefix = self.document.slice(start..self.cursor);
                start + previous_grapheme_start(&prefix, prefix.chars().count())
            };
        }
        self.finish_movement(selecting, None);
    }

    pub fn move_right(&mut self, selecting: bool) {
        self.prepare_selection(selecting);
        if self.cursor < self.document.len_chars() {
            let line = self.document.char_to_line(self.cursor);
            let end = self.document.line_end_char(line);
            self.cursor = if self.cursor == end {
                self.cursor + 1
            } else {
                let suffix = self.document.slice(self.cursor..end);
                self.cursor + next_grapheme_end(&suffix, 0)
            };
        }
        self.finish_movement(selecting, None);
    }

    pub fn move_vertical(&mut self, delta: isize, selecting: bool, tab_width: usize) {
        self.prepare_selection(selecting);
        let position = self.position(tab_width);
        let desired = self.desired_visual_column.unwrap_or(position.visual_column);
        let target_line = position
            .line
            .saturating_add_signed(delta)
            .min(self.document.line_count().saturating_sub(1));
        let start = self.document.line_start_char(target_line);
        let end = self.document.line_end_char(target_line);
        let text = self.document.slice(start..end);
        self.cursor = start + char_for_visual_column(&text, desired, tab_width);
        self.finish_movement(selecting, Some(desired));
    }

    /// Move by terminal rows rather than logical lines. Document offsets stay
    /// authoritative; the visual mapper only decides the destination.
    pub fn move_wrapped_vertical(
        &mut self,
        delta: isize,
        selecting: bool,
        metrics: VisualMetrics,
    ) -> Result<(), WrapError> {
        let before = self.cursor;
        let (cursor, desired) =
            metrics.move_cursor_rows(&self.document, before, delta, self.desired_visual_column)?;
        self.prepare_selection(selecting);
        self.cursor = cursor;
        self.finish_movement(selecting, Some(desired));
        Ok(())
    }

    pub fn move_home(&mut self, selecting: bool) {
        self.prepare_selection(selecting);
        let line = self.document.char_to_line(self.cursor);
        let start = self.document.line_start_char(line);
        let end = self.document.line_end_char(line);
        let text = self.document.slice(start..end);
        let first_non_space = start
            + text
                .chars()
                .take_while(|character| matches!(character, ' ' | '\t'))
                .count();
        self.cursor = if self.cursor == first_non_space {
            start
        } else {
            first_non_space
        };
        self.finish_movement(selecting, None);
    }

    pub fn move_end(&mut self, selecting: bool) {
        self.prepare_selection(selecting);
        let line = self.document.char_to_line(self.cursor);
        self.cursor = self.document.line_end_char(line);
        self.finish_movement(selecting, None);
    }

    pub fn move_word_left(&mut self, selecting: bool) {
        self.prepare_selection(selecting);
        let mut index = self.cursor;
        while index > 0
            && self
                .document
                .char(index - 1)
                .is_some_and(char::is_whitespace)
        {
            index -= 1;
        }
        let word_class = index
            .checked_sub(1)
            .and_then(|i| self.document.char(i))
            .map(is_word_character);
        while index > 0
            && self
                .document
                .char(index - 1)
                .is_some_and(|character| Some(is_word_character(character)) == word_class)
        {
            index -= 1;
        }
        self.cursor = index;
        self.finish_movement(selecting, None);
    }

    pub fn move_word_right(&mut self, selecting: bool) {
        self.prepare_selection(selecting);
        let len = self.document.len_chars();
        let mut index = self.cursor;
        while index < len && self.document.char(index).is_some_and(char::is_whitespace) {
            index += 1;
        }
        let word_class = self.document.char(index).map(is_word_character);
        while index < len
            && self
                .document
                .char(index)
                .is_some_and(|character| Some(is_word_character(character)) == word_class)
        {
            index += 1;
        }
        self.cursor = index;
        self.finish_movement(selecting, None);
    }

    pub fn goto_line(&mut self, one_based_line: usize) {
        let line = one_based_line
            .saturating_sub(1)
            .min(self.document.line_count().saturating_sub(1));
        self.cursor = self.document.line_start_char(line);
        self.anchor = None;
        self.break_movement_group();
    }

    pub fn set_cursor(&mut self, cursor: usize, selecting: bool) {
        self.prepare_selection(selecting);
        self.cursor = cursor.min(self.document.len_chars());
        self.finish_movement(selecting, None);
    }

    pub fn undo(&mut self) {
        if let Some(cursor) = self.document.undo() {
            self.cursor = cursor.min(self.document.len_chars());
            self.anchor = None;
            self.desired_visual_column = None;
        }
    }

    pub fn redo(&mut self) {
        if let Some(cursor) = self.document.redo() {
            self.cursor = cursor.min(self.document.len_chars());
            self.anchor = None;
            self.desired_visual_column = None;
        }
    }

    pub fn find_from(&self, needle: &str, from: usize, wrap: bool) -> Option<Range<usize>> {
        let pattern = crate::pattern::Pattern::parse(needle, true).ok()?;
        self.find_pattern_from(&pattern, from, wrap)
    }

    pub fn find_previous(&self, needle: &str, before: usize, wrap: bool) -> Option<Range<usize>> {
        let pattern = crate::pattern::Pattern::parse(needle, true).ok()?;
        self.find_pattern_previous(&pattern, before, wrap)
    }

    pub fn find_pattern_from(
        &self,
        pattern: &crate::pattern::Pattern,
        from: usize,
        wrap: bool,
    ) -> Option<Range<usize>> {
        let text = self.document.text();
        let from_byte = char_to_byte(&text, from.min(self.document.len_chars()));
        let found = pattern.find_from(&text, from_byte).or_else(|| {
            wrap.then(|| pattern.find_from(&text[..from_byte], 0))
                .flatten()
        })?;
        Some(byte_range_to_char_range(&text, found))
    }

    pub fn find_pattern_previous(
        &self,
        pattern: &crate::pattern::Pattern,
        before: usize,
        wrap: bool,
    ) -> Option<Range<usize>> {
        let text = self.document.text();
        let before_byte = char_to_byte(&text, before.min(self.document.len_chars()));
        let found = pattern.find_previous(&text, before_byte).or_else(|| {
            wrap.then(|| pattern.find_previous(&text, text.len()))
                .flatten()
        })?;
        Some(byte_range_to_char_range(&text, found))
    }

    pub fn matching_bracket(&self) -> Option<usize> {
        let len = self.document.len_chars();
        if let Some(target) = self
            .document
            .char(self.cursor)
            .and_then(|character| self.matching_bracket_from(self.cursor, character))
        {
            return Some(target);
        }
        self.cursor
            .checked_sub(1)
            .and_then(|index| {
                self.document
                    .char(index)
                    .map(|character| (index, character))
            })
            .and_then(|(index, character)| {
                (index < len).then(|| self.matching_bracket_from(index, character))?
            })
    }

    fn matching_bracket_from(&self, index: usize, character: char) -> Option<usize> {
        let (open, close, forward) = match character {
            '(' => ('(', ')', true),
            '[' => ('[', ']', true),
            '{' => ('{', '}', true),
            ')' => ('(', ')', false),
            ']' => ('[', ']', false),
            '}' => ('{', '}', false),
            _ => return None,
        };
        if forward {
            let mut depth = 0usize;
            for cursor in index..self.document.len_chars() {
                match self.document.char(cursor) {
                    Some(found) if found == open => depth += 1,
                    Some(found) if found == close => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            return Some(cursor);
                        }
                    }
                    _ => {}
                }
            }
        } else {
            let mut depth = 0usize;
            for cursor in (0..=index).rev() {
                match self.document.char(cursor) {
                    Some(found) if found == close => depth += 1,
                    Some(found) if found == open => {
                        depth = depth.saturating_sub(1);
                        if depth == 0 {
                            return Some(cursor);
                        }
                    }
                    _ => {}
                }
            }
        }
        None
    }

    pub fn ensure_cursor_visible(
        &mut self,
        content_height: usize,
        content_width: usize,
        tab_width: usize,
        margin: usize,
    ) {
        let position = self.position(tab_width);
        let height = content_height.max(1);
        let width = content_width.max(1);
        let vertical_margin = margin.min(height.saturating_sub(1) / 2);
        if position.line < self.viewport.top_line + vertical_margin {
            self.viewport.top_line = position.line.saturating_sub(vertical_margin);
        } else if position.line >= self.viewport.top_line + height.saturating_sub(vertical_margin) {
            self.viewport.top_line = position
                .line
                .saturating_sub(height.saturating_sub(vertical_margin + 1));
        }
        if position.visual_column < self.viewport.left_column {
            self.viewport.left_column = position.visual_column;
        } else if position.visual_column >= self.viewport.left_column + width {
            self.viewport.left_column = position.visual_column.saturating_sub(width - 1);
        }
    }

    /// Normalize the resize-safe soft-wrap anchor and keep the cursor within
    /// the visible terminal rows using the same map used by movement/rendering.
    pub fn ensure_wrapped_cursor_visible(
        &mut self,
        metrics: VisualMetrics,
        content_height: usize,
        margin: usize,
    ) -> Result<(), WrapError> {
        let height = content_height.max(1);
        let vertical_margin = margin.min(height.saturating_sub(1) / 2);
        let cursor = metrics.point_for_cursor(&self.document, self.cursor)?;
        let top = metrics.normalize_anchor(
            &self.document,
            VisualAnchor {
                line: self.viewport.top_line,
                char_in_line: self.viewport.top_wrap_char,
            },
        )?;
        let rows = metrics.visible_rows(&self.document, top, height)?;
        let visible_index = rows.iter().position(|row| row.anchor == cursor.row);
        let cursor_before_top =
            (cursor.row.line, cursor.row.char_in_line) < (top.line, top.char_in_line);

        let next_top = match visible_index {
            Some(index) if index < vertical_margin => {
                metrics.advance_rows(&self.document, cursor.row, -(vertical_margin as isize))?
            }
            Some(index) if index >= height.saturating_sub(vertical_margin) => {
                let rows_above = height.saturating_sub(vertical_margin + 1);
                metrics.advance_rows(&self.document, cursor.row, -(rows_above as isize))?
            }
            Some(_) => top,
            None if cursor_before_top => {
                metrics.advance_rows(&self.document, cursor.row, -(vertical_margin as isize))?
            }
            None => {
                let rows_above = height.saturating_sub(vertical_margin + 1);
                metrics.advance_rows(&self.document, cursor.row, -(rows_above as isize))?
            }
        };
        self.set_wrap_anchor(next_top);
        Ok(())
    }

    pub fn scroll_wrapped_rows(
        &mut self,
        metrics: VisualMetrics,
        delta: isize,
    ) -> Result<(), WrapError> {
        let anchor = metrics.advance_rows(
            &self.document,
            VisualAnchor {
                line: self.viewport.top_line,
                char_in_line: self.viewport.top_wrap_char,
            },
            delta,
        )?;
        self.set_wrap_anchor(anchor);
        Ok(())
    }

    pub fn reset_vertical_goal(&mut self) {
        self.desired_visual_column = None;
    }

    fn set_wrap_anchor(&mut self, anchor: VisualAnchor) {
        self.viewport.top_line = anchor.line;
        self.viewport.top_wrap_char = anchor.char_in_line;
    }

    fn prepare_selection(&mut self, selecting: bool) {
        if selecting && self.anchor.is_none() {
            self.anchor = Some(self.cursor);
        } else if !selecting {
            self.anchor = None;
        }
        self.document.break_undo_group();
    }

    fn finish_movement(&mut self, selecting: bool, desired: Option<usize>) {
        if selecting && self.anchor == Some(self.cursor) {
            self.anchor = None;
        }
        self.desired_visual_column = desired;
    }

    fn break_movement_group(&mut self) {
        self.document.break_undo_group();
        self.desired_visual_column = None;
    }

    fn selected_line_range(&self) -> (usize, usize) {
        if let Some(range) = self.selection() {
            let start_line = self.document.char_to_line(range.start);
            let mut end_line = self.document.char_to_line(range.end);
            if range.end > range.start
                && end_line > start_line
                && range.end == self.document.line_start_char(end_line)
            {
                end_line -= 1;
            }
            (start_line, end_line)
        } else {
            let line = self.document.char_to_line(self.cursor);
            (line, line)
        }
    }

    fn adjust_after_line_comment_toggle(
        &self,
        position: usize,
        changes: &[LineCommentChange],
    ) -> usize {
        let line = self.document.char_to_line(position);
        let column = position.saturating_sub(self.document.line_start_char(line));
        let mut adjusted = position as isize;
        for change in changes {
            if change.line < line || (change.line == line && column >= change.column) {
                adjusted += change.delta;
            }
        }
        adjusted.max(0) as usize
    }

    fn adjust_after_line_indent_change(
        &self,
        position: usize,
        changes: &[LineIndentChange],
    ) -> usize {
        let line = self.document.char_to_line(position);
        let column = position.saturating_sub(self.document.line_start_char(line));
        let mut adjusted = position as isize;
        for change in changes {
            if change.line < line {
                adjusted += change.delta;
            } else if change.line == line {
                if change.delta >= 0 {
                    if column >= change.column {
                        adjusted += change.delta;
                    }
                } else {
                    let removed = (-change.delta) as usize;
                    if column > change.column + removed {
                        adjusted += change.delta;
                    } else if column > change.column {
                        adjusted -= (column - change.column) as isize;
                    }
                }
            }
        }
        adjusted.max(0) as usize
    }
}

fn map_position_moving_lines_up(
    position: usize,
    previous_start: usize,
    block_start: usize,
) -> usize {
    if position >= block_start {
        previous_start + position - block_start
    } else {
        position
    }
}

fn map_position_moving_lines_down(
    position: usize,
    block_start: usize,
    block_end: usize,
    next_end: usize,
) -> usize {
    if position < block_start {
        position
    } else if position <= block_end {
        position + next_end - block_end
    } else {
        position
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LineCommentChange {
    line: usize,
    column: usize,
    delta: isize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LineIndentChange {
    line: usize,
    column: usize,
    delta: isize,
}

fn is_word_character(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

fn char_to_byte(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map_or(text.len(), |(byte, _)| byte)
}

fn byte_range_to_char_range(text: &str, range: Range<usize>) -> Range<usize> {
    let start = text[..range.start.min(text.len())].chars().count();
    let end = text[..range.end.min(text.len())].chars().count();
    start..end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_replaces_selection_and_undo_restores_it() {
        let mut editor = Editor::new(Document::from_text("hello world"));
        editor.anchor = Some(6);
        editor.cursor = 11;
        editor.insert("iPad", EditKind::Replace).unwrap();
        assert_eq!(editor.document.text(), "hello iPad");
        editor.undo();
        assert_eq!(editor.document.text(), "hello world");
        assert_eq!(editor.cursor, 11);
    }

    #[test]
    fn vertical_movement_preserves_visual_column() {
        let mut editor = Editor::new(Document::from_text("12345\n短\n12345"));
        editor.cursor = 4;
        editor.move_vertical(1, false, 4);
        assert_eq!(editor.position(4).line, 1);
        editor.move_vertical(1, false, 4);
        assert_eq!(
            editor.position(4),
            CursorPosition {
                line: 2,
                char_column: 4,
                visual_column: 4
            }
        );
    }

    #[test]
    fn wrapped_vertical_movement_uses_screen_rows_and_keeps_x() {
        let mut editor = Editor::new(Document::from_text("abcdef\nxy"));
        let metrics = VisualMetrics::new(3, 4, true);
        editor.cursor = 1;

        editor.move_wrapped_vertical(1, false, metrics).unwrap();
        assert_eq!(editor.cursor, 4);
        editor.move_wrapped_vertical(1, false, metrics).unwrap();
        assert_eq!(editor.cursor, 6);
        editor.move_wrapped_vertical(1, false, metrics).unwrap();
        assert_eq!(editor.cursor, 8);
    }

    #[test]
    fn wrapped_shift_movement_selects_global_document_offsets() {
        let mut editor = Editor::new(Document::from_text("abcdef"));
        editor.cursor = 1;
        editor
            .move_wrapped_vertical(1, true, VisualMetrics::new(3, 4, true))
            .unwrap();
        assert_eq!(editor.cursor, 4);
        assert_eq!(editor.selection(), Some(1..4));
    }

    #[test]
    fn wrapped_visibility_uses_character_anchor_and_preserves_horizontal_scroll() {
        let mut editor = Editor::new(Document::from_text("abcdefghi"));
        editor.cursor = 7;
        editor.viewport.left_column = 11;
        editor
            .ensure_wrapped_cursor_visible(VisualMetrics::new(3, 4, true), 2, 0)
            .unwrap();
        assert_eq!(editor.viewport.top_line, 0);
        assert_eq!(editor.viewport.top_wrap_char, 3);
        assert_eq!(editor.viewport.left_column, 11);

        editor
            .scroll_wrapped_rows(VisualMetrics::new(3, 4, true), 1)
            .unwrap();
        assert_eq!(editor.viewport.top_wrap_char, 6);
    }

    #[test]
    fn grapheme_deletion_is_atomic() {
        let mut editor = Editor::new(Document::from_text("a👩‍💻b"));
        editor.cursor = "a👩‍💻".chars().count();
        editor.backspace().unwrap();
        assert_eq!(editor.document.text(), "ab");
    }

    #[test]
    fn search_wraps() {
        let editor = Editor::new(Document::from_text("one two one"));
        assert_eq!(editor.find_from("one", 1, true), Some(8..11));
        assert_eq!(editor.find_from("one", 9, true), Some(0..3));
    }

    #[test]
    fn smart_newline_indents_between_braces() {
        let mut editor = Editor::new(Document::from_text("{}"));
        editor.cursor = 1;
        editor.insert_newline_with_indent(4).unwrap();
        assert_eq!(editor.document.text(), "{\n    \n}");
        assert_eq!(editor.position(4).line, 1);
        assert_eq!(editor.position(4).visual_column, 4);
    }

    #[test]
    fn service_replacement_is_one_undo_step() {
        let mut editor = Editor::new(Document::from_text("fn  main() {}"));
        editor.cursor = 4;
        editor.replace_all_from_service("fn main() {\n}\n").unwrap();
        assert_eq!(editor.document.text(), "fn main() {\n}\n");
        editor.undo();
        assert_eq!(editor.document.text(), "fn  main() {}");
        assert_eq!(editor.cursor, 4);
    }

    #[test]
    fn literal_replace_all_is_unicode_safe_and_one_undo_step() {
        let mut editor = Editor::new(Document::from_text("café bird café\n"));
        editor.cursor = editor.document.len_chars();
        let count = editor.replace_all_literal("café", "🪶").unwrap();
        assert_eq!(count, 2);
        assert_eq!(editor.document.text(), "🪶 bird 🪶\n");
        assert!(editor.document.is_modified());
        editor.undo();
        assert_eq!(editor.document.text(), "café bird café\n");

        let state = editor.document.state_id();
        assert_eq!(editor.replace_all_literal("missing", "x").unwrap(), 0);
        assert_eq!(editor.document.state_id(), state);
    }

    #[test]
    fn line_comment_toggle_comments_uncomments_selection_as_one_undo_step() {
        let text = "fn main() {\n    alpha();\n\n    beta();\n}\n";
        let mut editor = Editor::new(Document::from_text(text));
        let anchor = text.find("alpha").unwrap();
        let cursor = text.find("}\n").unwrap();
        editor.cursor = anchor;
        editor.set_cursor(cursor, true);

        let outcome = editor.toggle_line_comment("//").unwrap().unwrap();

        assert_eq!(outcome.mode, LineCommentToggle::Commented);
        assert_eq!(outcome.lines_changed, 2);
        assert_eq!(
            editor.document.text(),
            "fn main() {\n    // alpha();\n\n    // beta();\n}\n"
        );
        assert!(editor.document.is_modified());

        let outcome = editor.toggle_line_comment("//").unwrap().unwrap();
        assert_eq!(outcome.mode, LineCommentToggle::Uncommented);
        assert_eq!(outcome.lines_changed, 2);
        assert_eq!(editor.document.text(), text);

        editor.undo();
        assert_eq!(
            editor.document.text(),
            "fn main() {\n    // alpha();\n\n    // beta();\n}\n"
        );
        editor.undo();
        assert_eq!(editor.document.text(), text);
    }

    #[test]
    fn line_comment_toggle_ignores_blank_lines_and_column_zero_selection_end() {
        let text = "first\nsecond\nthird\n";
        let mut editor = Editor::new(Document::from_text(text));
        editor.cursor = 0;
        editor.set_cursor(text.find("third").unwrap(), true);

        let outcome = editor.toggle_line_comment("#").unwrap().unwrap();

        assert_eq!(outcome.lines_changed, 2);
        assert_eq!(editor.document.text(), "# first\n# second\nthird\n");
    }

    #[test]
    fn duplicate_lines_copies_current_line_below_and_undoes_once() {
        let text = "alpha\nbeta\ngamma\n";
        let mut editor = Editor::new(Document::from_text(text));
        let beta_column = 2;
        editor.cursor = text.find("beta").unwrap() + beta_column;

        let lines = editor.duplicate_lines().unwrap();

        assert_eq!(lines, 1);
        assert_eq!(editor.document.text(), "alpha\nbeta\nbeta\ngamma\n");
        assert_eq!(editor.cursor, "alpha\nbeta\nbe".chars().count());
        editor.undo();
        assert_eq!(editor.document.text(), text);
        assert_eq!(editor.cursor, text.find("beta").unwrap() + beta_column);
    }

    #[test]
    fn duplicate_lines_copies_selected_line_range_and_handles_final_newline_absence() {
        let text = "one\ntwo\nthree";
        let mut editor = Editor::new(Document::from_text(text));
        editor.cursor = text.find("one").unwrap();
        editor.set_cursor(text.find("three").unwrap(), true);

        let lines = editor.duplicate_lines().unwrap();

        assert_eq!(lines, 2);
        assert_eq!(editor.document.text(), "one\ntwo\none\ntwo\nthree");
        assert_eq!(editor.selected_text().as_deref(), Some("one\ntwo\n"));

        let three = editor.document.text().find("three").unwrap();
        editor.set_cursor(three + "three".chars().count(), false);
        editor.duplicate_lines().unwrap();
        assert_eq!(editor.document.text(), "one\ntwo\none\ntwo\nthree\nthree");
    }

    #[test]
    fn delete_lines_removes_current_line_and_undoes_once() {
        let text = "alpha\nbeta\ngamma\n";
        let mut editor = Editor::new(Document::from_text(text));
        editor.cursor = text.find("beta").unwrap() + 2;

        let lines = editor.delete_lines().unwrap();

        assert_eq!(lines, 1);
        assert_eq!(editor.document.text(), "alpha\ngamma\n");
        assert_eq!(editor.cursor, "alpha\n".chars().count());
        editor.undo();
        assert_eq!(editor.document.text(), text);
        assert_eq!(editor.cursor, text.find("beta").unwrap() + 2);
    }

    #[test]
    fn delete_lines_removes_selected_range_and_whole_buffer() {
        let text = "one\ntwo\nthree";
        let mut editor = Editor::new(Document::from_text(text));
        editor.cursor = text.find("two").unwrap();
        editor.set_cursor(text.find("three").unwrap(), true);

        let lines = editor.delete_lines().unwrap();

        assert_eq!(lines, 1);
        assert_eq!(editor.document.text(), "one\nthree");
        assert_eq!(editor.anchor, None);

        editor.select_all();
        let lines = editor.delete_lines().unwrap();
        assert_eq!(lines, 2);
        assert_eq!(editor.document.text(), "");
        assert_eq!(editor.cursor, 0);
    }

    #[test]
    fn move_lines_up_reorders_current_line_selection_and_undoes_once() {
        let text = "alpha\nbeta\ngamma\ndelta\n";
        let mut editor = Editor::new(Document::from_text(text));
        let beta_column = 2;
        editor.cursor = text.find("beta").unwrap() + beta_column;

        let lines = editor.move_lines_up().unwrap();

        assert_eq!(lines, 1);
        assert_eq!(editor.document.text(), "beta\nalpha\ngamma\ndelta\n");
        assert_eq!(editor.cursor, beta_column);
        editor.undo();
        assert_eq!(editor.document.text(), text);
        assert_eq!(editor.cursor, text.find("beta").unwrap() + beta_column);

        let anchor = text.find("beta").unwrap();
        let cursor = text.find("delta").unwrap();
        editor.set_cursor(anchor, false);
        editor.set_cursor(cursor, true);

        let lines = editor.move_lines_up().unwrap();

        assert_eq!(lines, 2);
        assert_eq!(editor.document.text(), "beta\ngamma\nalpha\ndelta\n");
        assert_eq!(editor.selected_text().as_deref(), Some("beta\ngamma\n"));
    }

    #[test]
    fn move_lines_down_reorders_current_line_selection_and_stops_at_bottom() {
        let text = "alpha\nbeta\ngamma\ndelta\n";
        let mut editor = Editor::new(Document::from_text(text));
        let beta_column = 1;
        editor.cursor = text.find("beta").unwrap() + beta_column;

        let lines = editor.move_lines_down().unwrap();

        assert_eq!(lines, 1);
        assert_eq!(editor.document.text(), "alpha\ngamma\nbeta\ndelta\n");
        assert_eq!(editor.cursor, "alpha\ngamma\nb".chars().count());

        let anchor = "alpha\n".chars().count();
        let cursor = "alpha\ngamma\nbeta\n".chars().count();
        editor.set_cursor(anchor, false);
        editor.set_cursor(cursor, true);

        let lines = editor.move_lines_down().unwrap();

        assert_eq!(lines, 2);
        assert_eq!(editor.document.text(), "alpha\ndelta\ngamma\nbeta\n");
        assert_eq!(editor.selected_text().as_deref(), Some("gamma\nbeta\n"));
        let end = editor.document.len_chars();
        editor.set_cursor(end, false);
        assert_eq!(editor.move_lines_down().unwrap(), 0);
    }

    #[test]
    fn indent_lines_indents_current_line_selection_and_undoes_once() {
        let text = "alpha\nbeta\ngamma\n";
        let mut editor = Editor::new(Document::from_text(text));
        editor.cursor = text.find("beta").unwrap() + 2;

        let lines = editor.indent_lines(4, true).unwrap();

        assert_eq!(lines, 1);
        assert_eq!(editor.document.text(), "alpha\n    beta\ngamma\n");
        assert_eq!(editor.cursor, "alpha\n    be".chars().count());
        editor.undo();
        assert_eq!(editor.document.text(), text);
        assert_eq!(editor.cursor, text.find("beta").unwrap() + 2);

        editor.set_cursor(text.find("alpha").unwrap(), false);
        editor.set_cursor(text.find("gamma").unwrap(), true);

        let lines = editor.indent_lines(2, true).unwrap();

        assert_eq!(lines, 2);
        assert_eq!(editor.document.text(), "  alpha\n  beta\ngamma\n");
        assert_eq!(editor.selected_text().as_deref(), Some("alpha\n  beta\n"));
    }

    #[test]
    fn outdent_lines_removes_one_indent_unit_and_preserves_cursor() {
        let text = "    alpha\n  beta\n\tgamma\ndelta\n";
        let mut editor = Editor::new(Document::from_text(text));
        editor.cursor = text.find("alpha").unwrap() + 2;

        let lines = editor.outdent_lines(4).unwrap();

        assert_eq!(lines, 1);
        assert_eq!(editor.document.text(), "alpha\n  beta\n\tgamma\ndelta\n");
        assert_eq!(editor.cursor, 2);

        editor.set_cursor(0, false);
        let delta = editor.document.text().find("delta").unwrap();
        editor.set_cursor(delta, true);
        let lines = editor.outdent_lines(4).unwrap();

        assert_eq!(lines, 2);
        assert_eq!(editor.document.text(), "alpha\nbeta\ngamma\ndelta\n");
        assert_eq!(editor.outdent_lines(4).unwrap(), 0);
    }

    #[test]
    fn select_lines_selects_current_line_or_expands_existing_selection() {
        let text = "alpha\nbeta\ngamma";
        let mut editor = Editor::new(Document::from_text(text));
        editor.cursor = text.find("beta").unwrap() + 2;

        let lines = editor.select_lines();

        assert_eq!(lines, 1);
        assert_eq!(editor.selected_text().as_deref(), Some("beta\n"));

        let anchor = text.find("beta").unwrap() + 1;
        let cursor = text.find("gamma").unwrap();
        editor.set_cursor(anchor, false);
        editor.set_cursor(cursor, true);

        let lines = editor.select_lines();

        assert_eq!(lines, 1);
        assert_eq!(editor.selected_text().as_deref(), Some("beta\n"));

        editor.set_cursor(text.find("beta").unwrap(), false);
        editor.set_cursor(text.len(), true);

        let lines = editor.select_lines();

        assert_eq!(lines, 2);
        assert_eq!(editor.selected_text().as_deref(), Some("beta\ngamma"));
    }

    #[test]
    fn matching_bracket_finds_nested_pairs_from_current_or_previous_char() {
        let text = "fn main() {\n    call([1, {two: 2}]);\n}\n";
        let mut editor = Editor::new(Document::from_text(text));
        let open_paren = text.find("call(").unwrap() + "call".len();
        editor.cursor = open_paren;

        let close_paren = editor.matching_bracket().unwrap();

        assert_eq!(editor.document.char(close_paren), Some(')'));
        assert_eq!(close_paren, text.find("]);").unwrap() + 1);

        editor.cursor = close_paren + 1;
        assert_eq!(editor.matching_bracket(), Some(open_paren));

        editor.cursor = text.find("{two").unwrap();
        let close_brace = editor.matching_bracket().unwrap();
        assert_eq!(editor.document.char(close_brace), Some('}'));

        editor.cursor = text.find("main").unwrap();
        assert_eq!(editor.matching_bracket(), None);
    }
}
