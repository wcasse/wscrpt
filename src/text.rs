use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Terminal cells occupied by one extended grapheme at `logical_column`.
///
/// Tabs advance to stops measured from the beginning of the logical line.
/// Other control graphemes render as the one-cell replacement character, and
/// otherwise zero-width graphemes render as the one-cell dotted-circle
/// placeholder. This is the shared width policy for cursor movement, wrapping,
/// hit-testing, and rendering.
pub fn grapheme_cell_width(grapheme: &str, logical_column: usize, tab_width: usize) -> usize {
    let tab_width = tab_width.max(1);
    if grapheme == "\t" {
        return tab_width - logical_column % tab_width;
    }
    if grapheme.chars().any(char::is_control) {
        return 1;
    }
    UnicodeWidthStr::width(grapheme).max(1)
}

pub fn previous_grapheme_start(text: &str, char_idx: usize) -> usize {
    let mut previous = 0;
    for (byte_idx, grapheme) in text.grapheme_indices(true) {
        let start = text[..byte_idx].chars().count();
        let end = start + grapheme.chars().count();
        if end >= char_idx {
            return start;
        }
        previous = start;
    }
    previous
}

pub fn next_grapheme_end(text: &str, char_idx: usize) -> usize {
    for (byte_idx, grapheme) in text.grapheme_indices(true) {
        let start = text[..byte_idx].chars().count();
        let end = start + grapheme.chars().count();
        if start >= char_idx || end > char_idx {
            return end;
        }
    }
    text.chars().count()
}

pub fn visual_width(text: &str, tab_width: usize) -> usize {
    visual_width_from(text, tab_width, 0)
}

pub fn visual_width_from(text: &str, tab_width: usize, initial_column: usize) -> usize {
    let mut column = initial_column;
    for grapheme in text.graphemes(true) {
        column = column.saturating_add(grapheme_cell_width(grapheme, column, tab_width));
    }
    column.saturating_sub(initial_column)
}

pub fn char_for_visual_column(text: &str, target: usize, tab_width: usize) -> usize {
    let mut column = 0;
    let mut chars = 0;
    for grapheme in text.graphemes(true) {
        let width = grapheme_cell_width(grapheme, column, tab_width);
        let next_column = column.saturating_add(width);
        if next_column > target {
            break;
        }
        column = next_column;
        chars += grapheme.chars().count();
    }
    chars
}

/// Clamp a zero-based Unicode-scalar column to an extended-grapheme boundary.
///
/// Scalar-oriented tools can legally point into a combining sequence. The
/// editor never places its cursor inside that sequence, so such coordinates
/// snap to the beginning of the containing grapheme.
pub fn char_for_scalar_column(text: &str, target: usize) -> usize {
    let line_chars = text.chars().count();
    let target = target.min(line_chars);
    if target == line_chars {
        return line_chars;
    }
    for (byte_idx, grapheme) in text.grapheme_indices(true) {
        let start = text[..byte_idx].chars().count();
        let end = start + grapheme.chars().count();
        if target < end {
            return start;
        }
    }
    line_chars
}

pub fn expand_tabs(text: &str, tab_width: usize) -> String {
    let mut output = String::with_capacity(text.len());
    let mut column = 0;
    for grapheme in text.graphemes(true) {
        let width = grapheme_cell_width(grapheme, column, tab_width);
        if grapheme == "\t" {
            output.extend(std::iter::repeat_n(' ', width));
        } else if grapheme.chars().any(char::is_control) {
            output.push('�');
        } else if UnicodeWidthStr::width(grapheme) == 0 {
            output.push('◌');
        } else {
            output.push_str(grapheme);
        }
        column = column.saturating_add(width);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grapheme_navigation_keeps_combining_sequences_together() {
        let text = "a\u{301}b";
        assert_eq!(next_grapheme_end(text, 0), 2);
        assert_eq!(previous_grapheme_start(text, 2), 0);
        assert_eq!(next_grapheme_end(text, 2), 3);
    }

    #[test]
    fn grapheme_navigation_keeps_emoji_sequences_together() {
        let text = "👩‍💻x";
        let emoji_chars = "👩‍💻".chars().count();
        assert_eq!(next_grapheme_end(text, 0), emoji_chars);
        assert_eq!(previous_grapheme_start(text, emoji_chars), 0);
    }

    #[test]
    fn tabs_follow_tab_stops() {
        assert_eq!(visual_width("a\tb", 4), 5);
        assert_eq!(expand_tabs("a\tb", 4), "a   b");
        assert_eq!(char_for_visual_column("a\tb", 3, 4), 1);
        assert_eq!(char_for_visual_column("a\tb", 4, 4), 2);
    }

    #[test]
    fn grapheme_cells_cover_tabs_wide_controls_and_zero_width() {
        assert_eq!(grapheme_cell_width("\t", 0, 4), 4);
        assert_eq!(grapheme_cell_width("\t", 3, 4), 1);
        assert_eq!(grapheme_cell_width("\t", 99, 0), 1);
        assert_eq!(grapheme_cell_width("界", 0, 4), 2);
        assert_eq!(grapheme_cell_width("\0", 0, 4), 1);
        assert_eq!(grapheme_cell_width("\u{200b}", 0, 4), 1);
        assert_eq!(grapheme_cell_width("a\u{301}", 0, 4), 1);
    }

    #[test]
    fn initial_column_controls_logical_line_tab_stops() {
        assert_eq!(visual_width_from("\tX", 4, 0), 5);
        assert_eq!(visual_width_from("\tX", 4, 3), 2);
        assert_eq!(visual_width_from("\tX", 0, 7), 2);
    }

    #[test]
    fn controls_and_zero_width_graphemes_are_visible_everywhere() {
        let text = "a\0\u{200b}b";
        assert_eq!(visual_width(text, 4), 4);
        assert_eq!(expand_tabs(text, 4), "a�◌b");

        assert_eq!(char_for_visual_column(text, 1, 4), 1);
        assert_eq!(char_for_visual_column(text, 2, 4), 2);
        assert_eq!(char_for_visual_column(text, 3, 4), 3);
        assert_eq!(char_for_visual_column(text, 4, 4), 4);
    }

    #[test]
    fn column_hit_testing_never_splits_wide_or_multiscalar_graphemes() {
        let text = "界a\u{301}b";
        assert_eq!(char_for_visual_column(text, 0, 4), 0);
        assert_eq!(char_for_visual_column(text, 1, 4), 0);
        assert_eq!(char_for_visual_column(text, 2, 4), 1);
        assert_eq!(char_for_visual_column(text, 3, 4), 3);
        assert_eq!(char_for_visual_column(text, 4, 4), 4);
    }

    #[test]
    fn scalar_columns_preserve_boundaries_and_snap_combining_sequences() {
        let text = "\tca\u{66}e\u{301}界";
        assert_eq!(char_for_scalar_column(text, 0), 0);
        assert_eq!(char_for_scalar_column(text, 1), 1);
        assert_eq!(char_for_scalar_column(text, 4), 4);
        assert_eq!(char_for_scalar_column(text, 5), 4);
        assert_eq!(char_for_scalar_column(text, 6), 6);
        assert_eq!(char_for_scalar_column(text, usize::MAX), 7);
    }

    #[test]
    fn expanded_text_has_the_same_terminal_width_as_source() {
        for text in ["a\tb", "界\tX", "a\0\u{200b}b", "👩‍💻\t界", "a\u{301}\t"] {
            let expanded = expand_tabs(text, 4);
            assert_eq!(visual_width(&expanded, 4), visual_width(text, 4));
            assert!(!expanded.contains('\t'));
            assert!(!expanded.chars().any(char::is_control));
        }
    }
}
