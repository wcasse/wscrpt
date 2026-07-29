//! Pure, bounded soft-wrap layout for logical text lines.
//!
//! The map uses Unicode scalar (`char`) offsets, matching `Document` and
//! `Editor`. Wrap boundaries are always extended-grapheme boundaries. Tabs use
//! stops measured from the start of the *logical* line, so wrapping does not
//! change their expansion.
//!
//! Rendering semantics intentionally match `render::display_grapheme`:
//! controls are displayed as a one-cell replacement, and an otherwise
//! zero-width grapheme is displayed as a one-cell dotted-circle placeholder.
//! This makes cursor and hit-test coordinates agree with what the editor puts
//! on screen.

use std::error::Error;
use std::fmt;
use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

use crate::text::grapheme_cell_width;

/// Conservative defaults for building a whole-document wrap map on a remote
/// device. Callers may choose smaller limits for viewport-local maps.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WrapLimits {
    /// Maximum number of logical lines accepted by one map.
    pub max_logical_lines: usize,
    /// Maximum Unicode scalar count in any one logical line.
    pub max_chars_per_line: usize,
    /// Maximum Unicode scalar count across all logical lines.
    pub max_total_chars: usize,
    /// Maximum number of stored extended grapheme clusters.
    pub max_graphemes: usize,
    /// Maximum number of generated visual rows.
    pub max_visual_rows: usize,
}

impl Default for WrapLimits {
    fn default() -> Self {
        Self {
            max_logical_lines: 1_000_000,
            max_chars_per_line: 1_048_576,
            max_total_chars: 8_388_608,
            max_graphemes: 2_097_152,
            max_visual_rows: 1_048_576,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WrapError {
    TooManyLogicalLines {
        limit: usize,
    },
    LineTooLong {
        logical_line: usize,
        limit: usize,
    },
    TooManyTotalChars {
        limit: usize,
    },
    TooManyGraphemes {
        limit: usize,
    },
    TooManyVisualRows {
        limit: usize,
    },
    EmbeddedLineFeed {
        logical_line: usize,
        char_offset: usize,
    },
    LogicalLineNumberOverflow,
    ArithmeticOverflow {
        logical_line: usize,
    },
}

impl fmt::Display for WrapError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyLogicalLines { limit } => {
                write!(formatter, "wrap map exceeds its {limit}-line limit")
            }
            Self::LineTooLong {
                logical_line,
                limit,
            } => write!(
                formatter,
                "logical line {logical_line} exceeds its {limit}-character wrap limit"
            ),
            Self::TooManyTotalChars { limit } => write!(
                formatter,
                "wrap map exceeds its {limit}-total-character limit"
            ),
            Self::TooManyGraphemes { limit } => {
                write!(formatter, "wrap map exceeds its {limit}-grapheme limit")
            }
            Self::TooManyVisualRows { limit } => {
                write!(formatter, "wrap map exceeds its {limit}-visual-row limit")
            }
            Self::EmbeddedLineFeed {
                logical_line,
                char_offset,
            } => write!(
                formatter,
                "logical line {logical_line} contains a line feed at character {char_offset}"
            ),
            Self::LogicalLineNumberOverflow => {
                formatter.write_str("logical line number overflow while building wrap map")
            }
            Self::ArithmeticOverflow { logical_line } => write!(
                formatter,
                "display-column arithmetic overflow on logical line {logical_line}"
            ),
        }
    }
}

impl Error for WrapError {}

/// Summary of one logical line in a [`WrapMap`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WrappedLine {
    pub logical_line: usize,
    pub first_visual_row: usize,
    pub visual_row_count: usize,
    pub char_len: usize,
    /// Display width of the unwrapped logical line.
    pub visual_width: usize,
    first_cell: usize,
    cell_count: usize,
}

impl WrappedLine {
    pub fn visual_rows(&self) -> Range<usize> {
        self.first_visual_row..self.first_visual_row + self.visual_row_count
    }
}

/// One visual row produced by wrapping a logical line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WrapSegment {
    pub logical_line: usize,
    pub segment_index: usize,
    pub visual_row: usize,
    /// Line-local Unicode scalar offsets.
    pub char_start: usize,
    pub char_end: usize,
    /// Display columns measured from the start of the unwrapped logical line.
    pub logical_column_start: usize,
    pub logical_column_end: usize,
    /// Display cells occupied by this visual row.
    pub cell_width: usize,
    /// True only when a single indivisible grapheme is wider than the wrap
    /// width. No grapheme is split merely to force this to false.
    pub overflows_width: bool,
    first_cell: usize,
    cell_count: usize,
}

impl WrapSegment {
    pub fn char_range(&self) -> Range<usize> {
        self.char_start..self.char_end
    }

    pub fn logical_column_range(&self) -> Range<usize> {
        self.logical_column_start..self.logical_column_end
    }
}

/// Canonical visual location of a line-local character offset.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VisualPosition {
    pub logical_line: usize,
    pub segment_index: usize,
    pub visual_row: usize,
    pub char_offset: usize,
    pub x: usize,
    /// The requested offset was past end-of-line or inside a grapheme and was
    /// snapped to the closest preceding valid grapheme boundary.
    pub snapped: bool,
}

/// A cursor position relative to a viewport's top visual row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RelativeCursor {
    pub x: usize,
    pub y: usize,
    pub visual_row: usize,
    pub char_offset: usize,
    pub snapped: bool,
}

/// Result of converting a cell x-coordinate in a visual row to a text
/// boundary. Coordinates inside a wide grapheme or tab snap to its start.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HitTest {
    pub logical_line: usize,
    pub segment_index: usize,
    pub visual_row: usize,
    pub char_offset: usize,
    /// Canonical cell coordinate of `char_offset` within this segment.
    pub x: usize,
    /// The requested x was inside a grapheme/tab or beyond segment end.
    pub snapped: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct GraphemeCell {
    char_end: usize,
    logical_column_end: usize,
}

/// Immutable mapping between logical lines and soft-wrapped visual rows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WrapMap {
    requested_width: usize,
    wrap_width: usize,
    tab_width: usize,
    first_logical_line: usize,
    lines: Vec<WrappedLine>,
    segments: Vec<WrapSegment>,
    cells: Vec<GraphemeCell>,
}

impl WrapMap {
    /// Build a map whose first input line has logical line number zero.
    ///
    /// Each item must contain exactly one logical line and must not include its
    /// terminating line feed. Empty input produces an empty map; an empty line
    /// produces one empty visual row.
    ///
    /// A requested width of zero is explicitly normalized to one cell. A tab
    /// width of zero is likewise normalized to one cell.
    pub fn build<I, S>(
        lines: I,
        width: usize,
        tab_width: usize,
        limits: WrapLimits,
    ) -> Result<Self, WrapError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self::build_from_line(0, lines, width, tab_width, limits)
    }

    /// Build a map for a logical-line window starting at
    /// `first_logical_line`. Visual row numbers remain local to this map and
    /// begin at zero.
    pub fn build_from_line<I, S>(
        first_logical_line: usize,
        lines: I,
        width: usize,
        tab_width: usize,
        limits: WrapLimits,
    ) -> Result<Self, WrapError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let wrap_width = width.max(1);
        let tab_width = tab_width.max(1);
        let mut map = Self {
            requested_width: width,
            wrap_width,
            tab_width,
            first_logical_line,
            lines: Vec::new(),
            segments: Vec::new(),
            cells: Vec::new(),
        };
        let mut total_chars = 0usize;

        for (relative_line, source) in lines.into_iter().enumerate() {
            if relative_line >= limits.max_logical_lines {
                return Err(WrapError::TooManyLogicalLines {
                    limit: limits.max_logical_lines,
                });
            }
            let logical_line = first_logical_line
                .checked_add(relative_line)
                .ok_or(WrapError::LogicalLineNumberOverflow)?;
            map.push_line(source.as_ref(), logical_line, &limits, &mut total_chars)?;
        }

        Ok(map)
    }

    pub fn requested_width(&self) -> usize {
        self.requested_width
    }

    pub fn wrap_width(&self) -> usize {
        self.wrap_width
    }

    pub fn tab_width(&self) -> usize {
        self.tab_width
    }

    pub fn first_logical_line(&self) -> usize {
        self.first_logical_line
    }

    pub fn logical_line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn visual_row_count(&self) -> usize {
        self.segments.len()
    }

    pub fn grapheme_count(&self) -> usize {
        self.cells.len()
    }

    pub fn line(&self, logical_line: usize) -> Option<&WrappedLine> {
        self.line_index(logical_line)
            .and_then(|index| self.lines.get(index))
    }

    pub fn segment_at(&self, visual_row: usize) -> Option<&WrapSegment> {
        self.segments.get(visual_row)
    }

    pub fn segments_for_line(&self, logical_line: usize) -> Option<&[WrapSegment]> {
        let line = self.line(logical_line)?;
        Some(&self.segments[line.first_visual_row..line.first_visual_row + line.visual_row_count])
    }

    /// Map a line-local Unicode scalar offset to a visual row and x.
    ///
    /// An offset at a wrap boundary belongs to the following segment. End of
    /// line belongs to the final segment. Offsets inside a grapheme snap to
    /// that grapheme's start.
    pub fn visual_position(
        &self,
        logical_line: usize,
        char_offset: usize,
    ) -> Option<VisualPosition> {
        let line = self.line(logical_line)?;
        let cells = self.line_cells(line);
        let requested = char_offset;
        let clamped = requested.min(line.char_len);
        let preceding_cells = cells.partition_point(|cell| cell.char_end <= clamped);
        let canonical_char = preceding_cells
            .checked_sub(1)
            .map_or(0, |index| cells[index].char_end);
        let logical_column = preceding_cells
            .checked_sub(1)
            .map_or(0, |index| cells[index].logical_column_end);
        let segments = self.segments_for_line(logical_line)?;
        let segment_offset = segments
            .partition_point(|segment| segment.char_start <= canonical_char)
            .saturating_sub(1);
        let segment = segments.get(segment_offset)?;

        Some(VisualPosition {
            logical_line,
            segment_index: segment.segment_index,
            visual_row: segment.visual_row,
            char_offset: canonical_char,
            x: logical_column.saturating_sub(segment.logical_column_start),
            snapped: canonical_char != requested,
        })
    }

    /// Map a cursor to coordinates below `top_visual_row`.
    ///
    /// `None` means either the logical line is absent from this map or the
    /// cursor is above the requested top row. The returned y is intentionally
    /// not height-clipped; callers can compare it with their viewport height.
    pub fn cursor_relative_to(
        &self,
        logical_line: usize,
        char_offset: usize,
        top_visual_row: usize,
    ) -> Option<RelativeCursor> {
        let position = self.visual_position(logical_line, char_offset)?;
        let y = position.visual_row.checked_sub(top_visual_row)?;
        Some(RelativeCursor {
            x: position.x,
            y,
            visual_row: position.visual_row,
            char_offset: position.char_offset,
            snapped: position.snapped,
        })
    }

    /// Hit-test x within a visual row.
    ///
    /// This follows `text::char_for_visual_column`: a coordinate strictly
    /// inside a tab or wide grapheme selects the boundary before it; its exact
    /// trailing edge selects the boundary after it. Values beyond the segment
    /// clamp to its end.
    pub fn hit_test(&self, visual_row: usize, x: usize) -> Option<HitTest> {
        let segment = self.segment_at(visual_row)?;
        let cells = &self.cells[segment.first_cell..segment.first_cell + segment.cell_count];
        let clamped_x = x.min(segment.cell_width);
        let target_column = segment
            .logical_column_start
            .checked_add(clamped_x)
            .unwrap_or(segment.logical_column_end)
            .min(segment.logical_column_end);
        let preceding_cells =
            cells.partition_point(|cell| cell.logical_column_end <= target_column);
        let (char_offset, logical_column) = preceding_cells.checked_sub(1).map_or_else(
            || (segment.char_start, segment.logical_column_start),
            |index| (cells[index].char_end, cells[index].logical_column_end),
        );
        let canonical_x = logical_column.saturating_sub(segment.logical_column_start);

        Some(HitTest {
            logical_line: segment.logical_line,
            segment_index: segment.segment_index,
            visual_row,
            char_offset,
            x: canonical_x,
            snapped: canonical_x != x,
        })
    }

    fn line_index(&self, logical_line: usize) -> Option<usize> {
        logical_line
            .checked_sub(self.first_logical_line)
            .filter(|index| *index < self.lines.len())
    }

    fn line_cells(&self, line: &WrappedLine) -> &[GraphemeCell] {
        &self.cells[line.first_cell..line.first_cell + line.cell_count]
    }

    fn push_line(
        &mut self,
        text: &str,
        logical_line: usize,
        limits: &WrapLimits,
        total_chars: &mut usize,
    ) -> Result<(), WrapError> {
        let first_visual_row = self.segments.len();
        let first_cell = self.cells.len();
        let mut segment_first_cell = first_cell;
        let mut segment_char_start = 0usize;
        let mut segment_column_start = 0usize;
        let mut line_chars = 0usize;
        let mut line_column = 0usize;

        for grapheme in text.graphemes(true) {
            if let Some(byte_offset) = grapheme.find('\n') {
                return Err(WrapError::EmbeddedLineFeed {
                    logical_line,
                    char_offset: line_chars + grapheme[..byte_offset].chars().count(),
                });
            }

            let grapheme_chars = grapheme.chars().count();
            let next_line_chars = line_chars
                .checked_add(grapheme_chars)
                .ok_or(WrapError::ArithmeticOverflow { logical_line })?;
            if next_line_chars > limits.max_chars_per_line {
                return Err(WrapError::LineTooLong {
                    logical_line,
                    limit: limits.max_chars_per_line,
                });
            }
            let next_total_chars = total_chars
                .checked_add(grapheme_chars)
                .ok_or(WrapError::ArithmeticOverflow { logical_line })?;
            if next_total_chars > limits.max_total_chars {
                return Err(WrapError::TooManyTotalChars {
                    limit: limits.max_total_chars,
                });
            }
            if self.cells.len() >= limits.max_graphemes {
                return Err(WrapError::TooManyGraphemes {
                    limit: limits.max_graphemes,
                });
            }

            let cell_width = grapheme_cell_width(grapheme, line_column, self.tab_width);
            let next_line_column = line_column
                .checked_add(cell_width)
                .ok_or(WrapError::ArithmeticOverflow { logical_line })?;
            let current_segment_width = line_column
                .checked_sub(segment_column_start)
                .ok_or(WrapError::ArithmeticOverflow { logical_line })?;
            let prospective_segment_width = current_segment_width
                .checked_add(cell_width)
                .ok_or(WrapError::ArithmeticOverflow { logical_line })?;

            if current_segment_width > 0 && prospective_segment_width > self.wrap_width {
                self.push_segment(
                    logical_line,
                    first_visual_row,
                    segment_char_start,
                    line_chars,
                    segment_column_start,
                    line_column,
                    segment_first_cell,
                    limits,
                )?;
                segment_first_cell = self.cells.len();
                segment_char_start = line_chars;
                segment_column_start = line_column;
            }

            self.cells.push(GraphemeCell {
                char_end: next_line_chars,
                logical_column_end: next_line_column,
            });
            line_chars = next_line_chars;
            line_column = next_line_column;
            *total_chars = next_total_chars;
        }

        self.push_segment(
            logical_line,
            first_visual_row,
            segment_char_start,
            line_chars,
            segment_column_start,
            line_column,
            segment_first_cell,
            limits,
        )?;
        // A terminal caret denotes an insertion boundary. When the last
        // content segment consumes the complete row (or contains one
        // indivisible over-wide grapheme), its end boundary cannot be shown at
        // x == wrap_width. Give that boundary an explicit empty continuation
        // row at x == 0 instead. Empty logical lines already have an empty row
        // and must not gain a second one.
        if line_chars > 0
            && self
                .segments
                .last()
                .is_some_and(|segment| segment.cell_width >= self.wrap_width)
        {
            self.push_segment(
                logical_line,
                first_visual_row,
                line_chars,
                line_chars,
                line_column,
                line_column,
                self.cells.len(),
                limits,
            )?;
        }
        let visual_row_count = self.segments.len() - first_visual_row;
        self.lines.push(WrappedLine {
            logical_line,
            first_visual_row,
            visual_row_count,
            char_len: line_chars,
            visual_width: line_column,
            first_cell,
            cell_count: self.cells.len() - first_cell,
        });
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn push_segment(
        &mut self,
        logical_line: usize,
        first_visual_row: usize,
        char_start: usize,
        char_end: usize,
        logical_column_start: usize,
        logical_column_end: usize,
        first_cell: usize,
        limits: &WrapLimits,
    ) -> Result<(), WrapError> {
        if self.segments.len() >= limits.max_visual_rows {
            return Err(WrapError::TooManyVisualRows {
                limit: limits.max_visual_rows,
            });
        }
        let cell_width = logical_column_end
            .checked_sub(logical_column_start)
            .ok_or(WrapError::ArithmeticOverflow { logical_line })?;
        self.segments.push(WrapSegment {
            logical_line,
            segment_index: self.segments.len() - first_visual_row,
            visual_row: self.segments.len(),
            char_start,
            char_end,
            logical_column_start,
            logical_column_end,
            cell_width,
            overflows_width: cell_width > self.wrap_width,
            first_cell,
            cell_count: self.cells.len() - first_cell,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(lines: &[&str], width: usize) -> WrapMap {
        WrapMap::build(lines.iter().copied(), width, 4, WrapLimits::default()).unwrap()
    }

    #[test]
    fn empty_input_has_no_rows() {
        let layout = map(&[], 8);
        assert_eq!(layout.logical_line_count(), 0);
        assert_eq!(layout.visual_row_count(), 0);
        assert_eq!(layout.grapheme_count(), 0);
        assert_eq!(layout.segment_at(0), None);
    }

    #[test]
    fn empty_logical_lines_each_have_one_empty_visual_row() {
        let layout = map(&["", ""], 8);
        assert_eq!(layout.visual_row_count(), 2);
        assert_eq!(
            layout.segment_at(0).unwrap().char_range(),
            Range { start: 0, end: 0 }
        );
        assert_eq!(layout.segment_at(1).unwrap().logical_line, 1);
        assert_eq!(
            layout.visual_position(1, 99),
            Some(VisualPosition {
                logical_line: 1,
                segment_index: 0,
                visual_row: 1,
                char_offset: 0,
                x: 0,
                snapped: true,
            })
        );
    }

    #[test]
    fn ascii_wraps_greedily_and_maps_both_directions() {
        let layout = map(&["abcdef"], 3);
        assert_eq!(layout.visual_row_count(), 3);
        assert_eq!(layout.segment_at(0).unwrap().char_range(), 0..3);
        assert_eq!(layout.segment_at(0).unwrap().cell_width, 3);
        assert_eq!(layout.segment_at(1).unwrap().char_range(), 3..6);
        assert_eq!(layout.segment_at(2).unwrap().char_range(), 6..6);
        assert_eq!(
            layout.visual_position(0, 2).unwrap(),
            VisualPosition {
                logical_line: 0,
                segment_index: 0,
                visual_row: 0,
                char_offset: 2,
                x: 2,
                snapped: false,
            }
        );
        assert_eq!(layout.visual_position(0, 3).unwrap().visual_row, 1);
        assert_eq!(layout.visual_position(0, 3).unwrap().x, 0);
        assert_eq!(layout.visual_position(0, 6).unwrap().visual_row, 2);
        assert_eq!(layout.visual_position(0, 6).unwrap().x, 0);
        assert_eq!(layout.hit_test(1, 1).unwrap().char_offset, 4);
    }

    #[test]
    fn exact_width_creates_an_empty_row_for_the_end_caret() {
        let layout = map(&["abc"], 3);
        assert_eq!(layout.visual_row_count(), 2);
        let continuation = layout.segment_at(1).unwrap();
        assert_eq!(continuation.char_range(), 3..3);
        assert_eq!(continuation.logical_column_range(), 3..3);
        assert_eq!(continuation.cell_width, 0);
        assert!(!continuation.overflows_width);
        let end = layout.visual_position(0, 3).unwrap();
        assert_eq!((end.visual_row, end.x), (1, 0));
        assert_eq!(layout.cursor_relative_to(0, 3, 0).unwrap().y, 1);
        assert_eq!(layout.hit_test(1, 0).unwrap().char_offset, 3);
        assert_eq!(layout.hit_test(1, 0).unwrap().x, 0);
        assert!(!layout.hit_test(1, 0).unwrap().snapped);
    }

    #[test]
    fn exact_width_caret_row_counts_toward_the_explicit_row_limit() {
        let limits = WrapLimits {
            max_visual_rows: 1,
            ..WrapLimits::default()
        };
        assert_eq!(
            WrapMap::build(["x"], 1, 4, limits),
            Err(WrapError::TooManyVisualRows { limit: 1 })
        );
    }

    #[test]
    fn nonzero_first_line_is_preserved_in_every_mapping() {
        let layout = WrapMap::build_from_line(41, ["abcd"], 2, 4, WrapLimits::default()).unwrap();
        assert_eq!(layout.first_logical_line(), 41);
        assert_eq!(layout.line(40), None);
        assert_eq!(layout.line(41).unwrap().logical_line, 41);
        assert_eq!(layout.segment_at(1).unwrap().logical_line, 41);
        assert_eq!(layout.hit_test(1, 0).unwrap().logical_line, 41);
    }

    #[test]
    fn grapheme_sequences_are_never_split() {
        let emoji = "👩‍💻";
        let emoji_chars = emoji.chars().count();
        let text = format!("a{emoji}b");
        let layout = WrapMap::build([text], 2, 4, WrapLimits::default()).unwrap();

        assert_eq!(layout.segment_at(0).unwrap().char_range(), 0..1);
        assert_eq!(
            layout.segment_at(1).unwrap().char_range(),
            1..1 + emoji_chars
        );
        assert_eq!(
            layout.segment_at(2).unwrap().char_range(),
            1 + emoji_chars..2 + emoji_chars
        );
        let inside = layout.visual_position(0, 2).unwrap();
        assert_eq!(inside.char_offset, 1);
        assert!(inside.snapped);
    }

    #[test]
    fn combining_sequence_is_one_cursor_cell_and_one_boundary() {
        let layout = map(&["a\u{301}b"], 1);
        assert_eq!(layout.grapheme_count(), 2);
        assert_eq!(layout.segment_at(0).unwrap().char_range(), 0..2);
        assert_eq!(layout.segment_at(1).unwrap().char_range(), 2..3);
        assert_eq!(layout.visual_position(0, 1).unwrap().char_offset, 0);
        assert_eq!(layout.hit_test(0, 1).unwrap().char_offset, 2);
    }

    #[test]
    fn standalone_zero_width_grapheme_matches_renderer_placeholder_width() {
        let layout = map(&["\u{200b}x"], 1);
        assert_eq!(layout.grapheme_count(), 2);
        assert_eq!(layout.segment_at(0).unwrap().cell_width, 1);
        assert_eq!(layout.segment_at(0).unwrap().char_range(), 0..1);
        assert_eq!(layout.segment_at(1).unwrap().char_range(), 1..2);
    }

    #[test]
    fn control_grapheme_matches_renderer_replacement_width() {
        let layout = map(&["\0x"], 1);
        assert_eq!(layout.visual_row_count(), 3);
        assert_eq!(layout.segment_at(0).unwrap().cell_width, 1);
        assert!(!layout.segment_at(0).unwrap().overflows_width);
    }

    #[test]
    fn indivisible_wide_grapheme_can_overflow_width_one() {
        let layout = map(&["界x"], 1);
        let wide = layout.segment_at(0).unwrap();
        assert_eq!(wide.char_range(), 0..1);
        assert_eq!(wide.cell_width, 2);
        assert!(wide.overflows_width);
        assert_eq!(layout.segment_at(1).unwrap().char_range(), 1..2);
        assert_eq!(layout.hit_test(0, 1).unwrap().char_offset, 0);
        assert_eq!(layout.hit_test(0, 2).unwrap().char_offset, 1);
    }

    #[test]
    fn tabs_keep_logical_line_tab_stops_across_wraps() {
        let layout = map(&["a\tb"], 2);
        let segments = layout.segments_for_line(0).unwrap();
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[0].char_range(), 0..1);
        assert_eq!(segments[0].logical_column_range(), 0..1);
        assert_eq!(segments[1].char_range(), 1..2);
        assert_eq!(segments[1].logical_column_range(), 1..4);
        assert_eq!(segments[1].cell_width, 3);
        assert!(segments[1].overflows_width);
        assert_eq!(segments[2].logical_column_range(), 4..5);

        assert_eq!(layout.hit_test(1, 0).unwrap().char_offset, 1);
        assert_eq!(layout.hit_test(1, 2).unwrap().char_offset, 1);
        assert_eq!(layout.hit_test(1, 3).unwrap().char_offset, 2);
    }

    #[test]
    fn width_zero_and_tab_zero_are_explicitly_normalized() {
        let layout = WrapMap::build(["\txy"], 0, 0, WrapLimits::default()).unwrap();
        assert_eq!(layout.requested_width(), 0);
        assert_eq!(layout.wrap_width(), 1);
        assert_eq!(layout.tab_width(), 1);
        assert_eq!(layout.visual_row_count(), 4);
        assert!(
            layout.segments[..3]
                .iter()
                .all(|segment| segment.cell_width == 1)
        );
        assert_eq!(layout.segment_at(3).unwrap().cell_width, 0);
    }

    #[test]
    fn visual_rows_resolve_across_multiple_logical_lines() {
        let layout = map(&["abcd", "e", ""], 2);
        assert_eq!(layout.visual_row_count(), 5);
        assert_eq!(layout.segment_at(0).unwrap().logical_line, 0);
        assert_eq!(layout.segment_at(1).unwrap().logical_line, 0);
        assert_eq!(layout.segment_at(2).unwrap().logical_line, 0);
        assert_eq!(layout.segment_at(3).unwrap().logical_line, 1);
        assert_eq!(layout.segment_at(4).unwrap().logical_line, 2);
        assert_eq!(layout.line(0).unwrap().visual_rows(), 0..3);
        assert_eq!(layout.line(2).unwrap().visual_rows(), 4..5);
    }

    #[test]
    fn cursor_coordinates_are_relative_to_top_visual_row() {
        let layout = map(&["abcdef"], 2);
        assert_eq!(
            layout.cursor_relative_to(0, 5, 1),
            Some(RelativeCursor {
                x: 1,
                y: 1,
                visual_row: 2,
                char_offset: 5,
                snapped: false,
            })
        );
        assert_eq!(layout.cursor_relative_to(0, 1, 1), None);
        assert_eq!(layout.cursor_relative_to(99, 0, 0), None);
    }

    #[test]
    fn hit_testing_snaps_inside_cells_and_clamps_beyond_end() {
        let layout = map(&["a界"], 8);
        assert_eq!(
            layout.hit_test(0, 2),
            Some(HitTest {
                logical_line: 0,
                segment_index: 0,
                visual_row: 0,
                char_offset: 1,
                x: 1,
                snapped: true,
            })
        );
        assert_eq!(layout.hit_test(0, 3).unwrap().char_offset, 2);
        let beyond = layout.hit_test(0, usize::MAX).unwrap();
        assert_eq!((beyond.char_offset, beyond.x), (2, 3));
        assert!(beyond.snapped);
        assert_eq!(layout.hit_test(1, 0), None);
    }

    #[test]
    fn offset_past_line_end_clamps_and_reports_snapping() {
        let layout = map(&["ab"], 8);
        assert_eq!(
            layout.visual_position(0, usize::MAX),
            Some(VisualPosition {
                logical_line: 0,
                segment_index: 0,
                visual_row: 0,
                char_offset: 2,
                x: 2,
                snapped: true,
            })
        );
    }

    #[test]
    fn logical_line_limit_is_enforced_before_extra_layout() {
        let limits = WrapLimits {
            max_logical_lines: 1,
            ..WrapLimits::default()
        };
        assert_eq!(
            WrapMap::build(["a", "b"], 8, 4, limits),
            Err(WrapError::TooManyLogicalLines { limit: 1 })
        );
    }

    #[test]
    fn per_line_character_limit_counts_unicode_scalars() {
        let limits = WrapLimits {
            max_chars_per_line: 2,
            ..WrapLimits::default()
        };
        assert_eq!(
            WrapMap::build(["a\u{301}"], 8, 4, limits),
            Ok(map(&["a\u{301}"], 8))
        );
        assert_eq!(
            WrapMap::build(["a\u{301}b"], 8, 4, limits),
            Err(WrapError::LineTooLong {
                logical_line: 0,
                limit: 2,
            })
        );
    }

    #[test]
    fn total_character_limit_is_enforced_across_lines() {
        let limits = WrapLimits {
            max_total_chars: 2,
            ..WrapLimits::default()
        };
        assert_eq!(
            WrapMap::build(["ab", "c"], 8, 4, limits),
            Err(WrapError::TooManyTotalChars { limit: 2 })
        );
    }

    #[test]
    fn grapheme_storage_limit_is_enforced() {
        let limits = WrapLimits {
            max_graphemes: 1,
            ..WrapLimits::default()
        };
        assert_eq!(
            WrapMap::build(["ab"], 8, 4, limits),
            Err(WrapError::TooManyGraphemes { limit: 1 })
        );
    }

    #[test]
    fn visual_row_limit_includes_empty_lines() {
        let limits = WrapLimits {
            max_visual_rows: 1,
            ..WrapLimits::default()
        };
        assert_eq!(
            WrapMap::build(["", ""], 8, 4, limits),
            Err(WrapError::TooManyVisualRows { limit: 1 })
        );
    }

    #[test]
    fn a_very_long_line_stops_at_the_explicit_bound() {
        let text = "x".repeat(10_000);
        let limits = WrapLimits {
            max_chars_per_line: 128,
            ..WrapLimits::default()
        };
        assert_eq!(
            WrapMap::build([text], 1, 4, limits),
            Err(WrapError::LineTooLong {
                logical_line: 0,
                limit: 128,
            })
        );
    }

    #[test]
    fn embedded_line_feed_is_rejected_with_its_char_offset() {
        assert_eq!(
            WrapMap::build(["ab\ncd"], 8, 4, WrapLimits::default()),
            Err(WrapError::EmbeddedLineFeed {
                logical_line: 0,
                char_offset: 2,
            })
        );
    }

    #[test]
    fn logical_line_number_overflow_is_reported() {
        assert_eq!(
            WrapMap::build_from_line(usize::MAX, ["a", "b"], 8, 4, WrapLimits::default()),
            Err(WrapError::LogicalLineNumberOverflow)
        );
    }

    #[test]
    fn zero_limits_allow_empty_input_but_not_an_empty_line() {
        let limits = WrapLimits {
            max_logical_lines: 0,
            max_chars_per_line: 0,
            max_total_chars: 0,
            max_graphemes: 0,
            max_visual_rows: 0,
        };
        assert!(WrapMap::build(std::iter::empty::<&str>(), 0, 0, limits).is_ok());
        assert_eq!(
            WrapMap::build([""], 0, 0, limits),
            Err(WrapError::TooManyLogicalLines { limit: 0 })
        );
    }
}
