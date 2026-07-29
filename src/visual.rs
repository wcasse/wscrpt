//! Viewport-local mapping between document offsets and terminal rows.
//!
//! `Document` and `Editor` keep logical Unicode-scalar offsets as their source
//! of truth. This module layers screen-row navigation on top without flattening
//! an entire document into one global wrap table: each logical line receives a
//! short-lived [`WrapMap`] only when an operation reaches that line.

use std::ops::Range;

use crate::Document;
use crate::wrap::{WrapError, WrapLimits, WrapMap, WrapSegment};

/// Display geometry used to map logical text onto terminal rows.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VisualMetrics {
    /// Editor cells available after subtracting the line-number gutter.
    pub content_width: usize,
    pub tab_width: usize,
    pub soft_wrap: bool,
    /// Explicit safety limits applied to every per-logical-line map and to a
    /// requested visible-row result.
    pub limits: WrapLimits,
}

impl VisualMetrics {
    pub fn new(content_width: usize, tab_width: usize, soft_wrap: bool) -> Self {
        Self {
            content_width,
            tab_width,
            soft_wrap,
            limits: WrapLimits::default(),
        }
    }

    pub const fn with_limits(mut self, limits: WrapLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Canonicalize a possibly stale viewport anchor after editing or reflow.
    ///
    /// Lines clamp to the document. Character offsets clamp to the line and
    /// snap first to a grapheme boundary, then to the start of the visual row
    /// containing that boundary. At a wrap boundary, affinity is to the later
    /// row, matching cursor placement in [`WrapMap`].
    pub fn normalize_anchor(
        &self,
        document: &Document,
        anchor: VisualAnchor,
    ) -> Result<VisualAnchor, WrapError> {
        let line = self.clamp_line(document, anchor.line);
        let (_, map) = self.map_for_line(document, line)?;
        let position = map
            .visual_position(line, anchor.char_in_line)
            .expect("a map built from one document line contains that line");
        let segment = map
            .segment_at(position.visual_row)
            .expect("a visual position names an existing segment");
        Ok(Self::anchor_for_segment(segment))
    }

    /// Build at most `count` consecutive screen rows beginning at `top`.
    ///
    /// The returned ranges are global document character offsets. `count` is
    /// also checked against `limits.max_visual_rows`, preventing an accidental
    /// unbounded allocation when this API is used outside a terminal viewport.
    pub fn visible_rows(
        &self,
        document: &Document,
        top: VisualAnchor,
        count: usize,
    ) -> Result<Vec<VisualRow>, WrapError> {
        if count == 0 {
            return Ok(Vec::new());
        }
        if count > self.limits.max_visual_rows {
            return Err(WrapError::TooManyVisualRows {
                limit: self.limits.max_visual_rows,
            });
        }

        let top = self.normalize_anchor(document, top)?;
        let last_line = document.line_count().saturating_sub(1);
        let mut rows = Vec::with_capacity(count);
        let mut line = top.line;
        let mut first_line = true;

        loop {
            let (line_start, map) = self.map_for_line(document, line)?;
            let segments = map
                .segments_for_line(line)
                .expect("a per-line map contains its requested logical line");
            let first_segment = if first_line {
                map.visual_position(line, top.char_in_line)
                    .expect("a per-line map contains its requested logical line")
                    .segment_index
            } else {
                0
            };

            for segment in &segments[first_segment..] {
                rows.push(Self::global_row(line_start, segment)?);
                if rows.len() == count {
                    return Ok(rows);
                }
            }

            if line == last_line {
                return Ok(rows);
            }
            line += 1;
            first_line = false;
        }
    }

    /// Locate a global document cursor on its canonical visual row.
    pub fn point_for_cursor(
        &self,
        document: &Document,
        cursor: usize,
    ) -> Result<VisualPoint, WrapError> {
        let requested = cursor;
        let clamped = cursor.min(document.len_chars());
        let line = document.char_to_line(clamped);
        let (line_start, map) = self.map_for_line(document, line)?;
        let local_cursor = clamped.saturating_sub(line_start);
        let position = map
            .visual_position(line, local_cursor)
            .expect("a map built from one document line contains that line");
        let segment = map
            .segment_at(position.visual_row)
            .expect("a visual position names an existing segment");
        let char_offset = line_start
            .checked_add(position.char_offset)
            .ok_or(WrapError::ArithmeticOverflow { logical_line: line })?;

        Ok(VisualPoint {
            row: Self::anchor_for_segment(segment),
            column: position.x,
            char_offset,
            snapped: position.snapped || char_offset != requested,
        })
    }

    /// Hit-test a cell in a previously generated visual row.
    ///
    /// Coordinates inside a tab or wide grapheme snap to its leading text
    /// boundary; coordinates beyond row content clamp to its end.
    pub fn cursor_for_point(
        &self,
        document: &Document,
        row: &VisualRow,
        x: usize,
    ) -> Result<usize, WrapError> {
        let anchor = self.normalize_anchor(document, row.anchor)?;
        let (line_start, map) = self.map_for_line(document, anchor.line)?;
        let position = map
            .visual_position(anchor.line, anchor.char_in_line)
            .expect("a map built from one document line contains that line");
        let hit = map
            .hit_test(position.visual_row, x)
            .expect("a visual position names a hit-testable segment");
        line_start
            .checked_add(hit.char_offset)
            .ok_or(WrapError::ArithmeticOverflow {
                logical_line: anchor.line,
            })
    }

    /// Move a viewport row anchor by signed screen rows, clamping at the first
    /// or final visual row of the document.
    pub fn advance_rows(
        &self,
        document: &Document,
        anchor: VisualAnchor,
        delta: isize,
    ) -> Result<VisualAnchor, WrapError> {
        let current = self.normalize_anchor(document, anchor)?;
        if delta == 0 {
            return Ok(current);
        }

        if delta > 0 {
            self.advance_forward(document, current, delta.unsigned_abs())
        } else {
            self.advance_backward(document, current, delta.unsigned_abs())
        }
    }

    /// Move a cursor by screen rows while preserving a preferred screen x.
    ///
    /// Passing `None` starts a new vertical movement group at the cursor's
    /// current x. The returned second value is the preferred x callers should
    /// feed back into subsequent vertical moves.
    pub fn move_cursor_rows(
        &self,
        document: &Document,
        cursor: usize,
        delta: isize,
        preferred_x: Option<usize>,
    ) -> Result<(usize, usize), WrapError> {
        let current = self.point_for_cursor(document, cursor)?;
        let desired_x = preferred_x.unwrap_or(current.column);
        if delta == 0 {
            return Ok((current.char_offset, desired_x));
        }
        let target = self.advance_rows(document, current.row, delta)?;
        let row = self.row_for_anchor(document, target)?;
        let cursor = self.cursor_for_point(document, &row, desired_x)?;
        Ok((cursor, desired_x))
    }

    fn advance_forward(
        &self,
        document: &Document,
        mut anchor: VisualAnchor,
        mut remaining: usize,
    ) -> Result<VisualAnchor, WrapError> {
        let last_line = document.line_count().saturating_sub(1);
        loop {
            let (_, map) = self.map_for_line(document, anchor.line)?;
            let segments = map
                .segments_for_line(anchor.line)
                .expect("a per-line map contains its requested logical line");
            let index = map
                .visual_position(anchor.line, anchor.char_in_line)
                .expect("a per-line map contains its requested logical line")
                .segment_index;
            let rows_after = segments.len().saturating_sub(index + 1);
            if remaining <= rows_after {
                return Ok(Self::anchor_for_segment(&segments[index + remaining]));
            }
            if anchor.line == last_line {
                return Ok(Self::anchor_for_segment(
                    segments
                        .last()
                        .expect("every logical line has a visual row"),
                ));
            }

            remaining -= rows_after + 1;
            anchor = VisualAnchor {
                line: anchor.line + 1,
                char_in_line: 0,
            };
            if remaining == 0 {
                return self.normalize_anchor(document, anchor);
            }
        }
    }

    fn advance_backward(
        &self,
        document: &Document,
        mut anchor: VisualAnchor,
        mut remaining: usize,
    ) -> Result<VisualAnchor, WrapError> {
        loop {
            let (_, map) = self.map_for_line(document, anchor.line)?;
            let segments = map
                .segments_for_line(anchor.line)
                .expect("a per-line map contains its requested logical line");
            let index = map
                .visual_position(anchor.line, anchor.char_in_line)
                .expect("a per-line map contains its requested logical line")
                .segment_index;
            if remaining <= index {
                return Ok(Self::anchor_for_segment(&segments[index - remaining]));
            }
            if anchor.line == 0 {
                return Ok(Self::anchor_for_segment(
                    segments
                        .first()
                        .expect("every logical line has a visual row"),
                ));
            }

            remaining -= index + 1;
            let previous_line = anchor.line - 1;
            let (_, previous_map) = self.map_for_line(document, previous_line)?;
            let previous_segments = previous_map
                .segments_for_line(previous_line)
                .expect("a per-line map contains its requested logical line");
            anchor = Self::anchor_for_segment(
                previous_segments
                    .last()
                    .expect("every logical line has a visual row"),
            );
            if remaining == 0 {
                return Ok(anchor);
            }
        }
    }

    fn row_for_anchor(
        &self,
        document: &Document,
        anchor: VisualAnchor,
    ) -> Result<VisualRow, WrapError> {
        let anchor = self.normalize_anchor(document, anchor)?;
        let (line_start, map) = self.map_for_line(document, anchor.line)?;
        let position = map
            .visual_position(anchor.line, anchor.char_in_line)
            .expect("a per-line map contains its requested logical line");
        let segment = map
            .segment_at(position.visual_row)
            .expect("a visual position names an existing segment");
        Self::global_row(line_start, segment)
    }

    fn map_for_line(
        &self,
        document: &Document,
        requested_line: usize,
    ) -> Result<(usize, WrapMap), WrapError> {
        let line = self.clamp_line(document, requested_line);
        let line_start = document.line_start_char(line);
        let line_end = document.line_end_char(line);
        let text = document.slice(line_start..line_end);
        let width = if self.soft_wrap {
            self.content_width
        } else {
            usize::MAX
        };
        let map = WrapMap::build_from_line(line, [text], width, self.tab_width, self.limits)?;
        Ok((line_start, map))
    }

    fn clamp_line(&self, document: &Document, line: usize) -> usize {
        line.min(document.line_count().saturating_sub(1))
    }

    fn anchor_for_segment(segment: &WrapSegment) -> VisualAnchor {
        VisualAnchor {
            line: segment.logical_line,
            char_in_line: segment.char_start,
        }
    }

    fn global_row(line_start: usize, segment: &WrapSegment) -> Result<VisualRow, WrapError> {
        let range_start =
            line_start
                .checked_add(segment.char_start)
                .ok_or(WrapError::ArithmeticOverflow {
                    logical_line: segment.logical_line,
                })?;
        let range_end =
            line_start
                .checked_add(segment.char_end)
                .ok_or(WrapError::ArithmeticOverflow {
                    logical_line: segment.logical_line,
                })?;
        Ok(VisualRow {
            anchor: Self::anchor_for_segment(segment),
            logical_line: segment.logical_line,
            segment_index: segment.segment_index,
            continuation: segment.segment_index > 0,
            char_range: range_start..range_end,
            logical_visual_start: segment.logical_column_start,
            cell_width: segment.cell_width,
            overflows_width: segment.overflows_width,
        })
    }
}

impl Default for VisualMetrics {
    fn default() -> Self {
        Self::new(80, 4, false)
    }
}

/// Stable document-relative identity of a screen row.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VisualAnchor {
    pub line: usize,
    /// Unicode-scalar offset from the logical line's start.
    pub char_in_line: usize,
}

/// One visible terminal row. Text ranges use global document character
/// offsets so selections, search matches, syntax, and diagnostics can reuse
/// their existing coordinate system.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VisualRow {
    pub anchor: VisualAnchor,
    pub logical_line: usize,
    pub segment_index: usize,
    pub continuation: bool,
    pub char_range: Range<usize>,
    pub logical_visual_start: usize,
    pub cell_width: usize,
    pub overflows_width: bool,
}

/// Canonical visual location of a global document cursor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VisualPoint {
    pub row: VisualAnchor,
    pub column: usize,
    pub char_offset: usize,
    pub snapped: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrapped(width: usize) -> VisualMetrics {
        VisualMetrics::new(width, 4, true)
    }

    #[test]
    fn visible_rows_are_bounded_and_expose_global_ranges() {
        let document = Document::from_text("abcdef\nxy\n");
        let rows = wrapped(3)
            .visible_rows(
                &document,
                VisualAnchor {
                    line: 0,
                    char_in_line: 3,
                },
                3,
            )
            .unwrap();

        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].char_range, 3..6);
        assert_eq!(rows[1].char_range, 6..6);
        assert_eq!(rows[2].char_range, 7..9);
        assert!(rows[0].continuation);
        assert!(rows[1].continuation);
        assert!(!rows[2].continuation);
    }

    #[test]
    fn zero_requested_rows_does_not_touch_even_a_zero_limit_map() {
        let document = Document::from_text("too long for zero limits");
        let metrics = wrapped(3).with_limits(WrapLimits {
            max_logical_lines: 0,
            max_chars_per_line: 0,
            max_total_chars: 0,
            max_graphemes: 0,
            max_visual_rows: 0,
        });
        assert_eq!(
            metrics
                .visible_rows(&document, VisualAnchor::default(), 0)
                .unwrap(),
            Vec::<VisualRow>::new()
        );
    }

    #[test]
    fn cursor_boundary_affinity_includes_the_exact_width_caret_row() {
        let document = Document::from_text("abcdef");
        let metrics = wrapped(3);

        assert_eq!(
            metrics.point_for_cursor(&document, 3).unwrap(),
            VisualPoint {
                row: VisualAnchor {
                    line: 0,
                    char_in_line: 3,
                },
                column: 0,
                char_offset: 3,
                snapped: false,
            }
        );
        assert_eq!(
            metrics.point_for_cursor(&document, 6).unwrap(),
            VisualPoint {
                row: VisualAnchor {
                    line: 0,
                    char_in_line: 6,
                },
                column: 0,
                char_offset: 6,
                snapped: false,
            }
        );
    }

    #[test]
    fn click_mapping_agrees_for_tabs_and_wide_graphemes() {
        let document = Document::from_text("a\t界b");
        let rows = wrapped(4)
            .visible_rows(&document, VisualAnchor::default(), 4)
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].char_range, 0..2);
        assert_eq!(rows[1].char_range, 2..4);

        let metrics = wrapped(4);
        assert_eq!(metrics.cursor_for_point(&document, &rows[0], 2).unwrap(), 1);
        assert_eq!(metrics.cursor_for_point(&document, &rows[0], 4).unwrap(), 2);
        assert_eq!(metrics.cursor_for_point(&document, &rows[1], 1).unwrap(), 2);
        assert_eq!(metrics.cursor_for_point(&document, &rows[1], 2).unwrap(), 3);
    }

    #[test]
    fn movement_crosses_wrapped_segments_exact_caret_rows_and_lines() {
        let document = Document::from_text("abcdef\nxy\n123456");
        let metrics = wrapped(3);

        assert_eq!(
            metrics.move_cursor_rows(&document, 1, 1, None).unwrap(),
            (4, 1)
        );
        // Row 0 + 3 reaches logical line 1 after segment 1 and the exact-width
        // end-caret row of logical line 0.
        assert_eq!(
            metrics.move_cursor_rows(&document, 1, 3, None).unwrap(),
            (8, 1)
        );
        assert_eq!(
            metrics.move_cursor_rows(&document, 8, -3, None).unwrap(),
            (1, 1)
        );
    }

    #[test]
    fn desired_column_survives_a_short_intermediate_line() {
        let document = Document::from_text("abcde\nx\nABCDE");
        let metrics = wrapped(4);
        let (short_end, desired) = metrics.move_cursor_rows(&document, 2, 2, None).unwrap();
        assert_eq!((short_end, desired), (7, 2));
        assert_eq!(
            metrics
                .move_cursor_rows(&document, short_end, 1, Some(desired))
                .unwrap(),
            (10, 2)
        );
    }

    #[test]
    fn page_advance_clamps_at_both_document_edges() {
        let document = Document::from_text("abcdef\nxy\n123456");
        let metrics = wrapped(3);
        let start = VisualAnchor::default();
        let page = metrics.advance_rows(&document, start, 4).unwrap();
        assert_eq!(
            page,
            VisualAnchor {
                line: 2,
                char_in_line: 0,
            }
        );
        assert_eq!(metrics.advance_rows(&document, page, -4).unwrap(), start);
        assert_eq!(
            metrics.advance_rows(&document, start, isize::MIN).unwrap(),
            start
        );
        assert_eq!(
            metrics.advance_rows(&document, start, isize::MAX).unwrap(),
            VisualAnchor {
                line: 2,
                char_in_line: 6,
            }
        );
    }

    #[test]
    fn resize_normalizes_anchor_to_the_new_containing_segment() {
        let document = Document::from_text("abcdefgh");
        let old_anchor = wrapped(4)
            .normalize_anchor(
                &document,
                VisualAnchor {
                    line: 0,
                    char_in_line: 4,
                },
            )
            .unwrap();
        assert_eq!(old_anchor.char_in_line, 4);
        assert_eq!(
            wrapped(3).normalize_anchor(&document, old_anchor).unwrap(),
            VisualAnchor {
                line: 0,
                char_in_line: 3,
            }
        );
    }

    #[test]
    fn unicode_cursor_inside_grapheme_snaps_without_splitting_it() {
        let emoji = "👩‍💻";
        let document = Document::from_text(&format!("a{emoji}b"));
        let point = wrapped(2).point_for_cursor(&document, 2).unwrap();
        assert_eq!(point.char_offset, 1);
        assert!(point.snapped);
        assert_eq!(point.column, 0);
        assert_eq!(point.row.char_in_line, 1);
    }

    #[test]
    fn unwrapped_metrics_keep_one_visual_row_per_logical_line() {
        let document = Document::from_text("abcdef\nxy");
        let metrics = VisualMetrics::new(2, 4, false);
        let rows = metrics
            .visible_rows(&document, VisualAnchor::default(), 8)
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].char_range, 0..6);
        assert_eq!(rows[1].char_range, 7..9);
        assert_eq!(metrics.point_for_cursor(&document, 6).unwrap().column, 6);
    }

    #[test]
    fn empty_and_trailing_lines_are_real_navigable_rows() {
        let document = Document::from_text("\n");
        let metrics = wrapped(8);
        let rows = metrics
            .visible_rows(&document, VisualAnchor::default(), 8)
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].char_range, 0..0);
        assert_eq!(rows[1].char_range, 1..1);
        assert_eq!(
            metrics.advance_rows(&document, rows[0].anchor, 1).unwrap(),
            rows[1].anchor
        );
    }

    #[test]
    fn wrap_errors_propagate_without_partial_results() {
        let document = Document::from_text("abc");
        let limits = WrapLimits {
            max_logical_lines: 1,
            max_chars_per_line: 2,
            max_total_chars: 2,
            max_graphemes: 2,
            max_visual_rows: 2,
        };
        let metrics = wrapped(1).with_limits(limits);
        assert_eq!(
            metrics.normalize_anchor(&document, VisualAnchor::default()),
            Err(WrapError::LineTooLong {
                logical_line: 0,
                limit: 2,
            })
        );
        assert_eq!(
            metrics.visible_rows(&Document::from_text("a"), VisualAnchor::default(), 3),
            Err(WrapError::TooManyVisualRows { limit: 2 })
        );
    }
}
