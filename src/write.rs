//! Writer-facing buffer profile: prose vs code, wrap policy, hard wrap.
//!
//! Kept out of `app.rs` so the façade only toggles and paints.

use std::ops::Range;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::document::Document;
use crate::wrap::WrapPolicy;

/// How this buffer should behave for wrap, spelling, and idle save.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WriteProfile {
    /// Identifiers, no spellcheck, character wrap when soft wrap is on.
    #[default]
    Code,
    /// Narrative text: word wrap, optional spell/autocorrect, idle save.
    Prose,
}

impl WriteProfile {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::Prose => "prose",
        }
    }

    pub const fn wrap_policy(self) -> WrapPolicy {
        match self {
            Self::Code => WrapPolicy::Char,
            Self::Prose => WrapPolicy::Word,
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "code" | "src" | "source" => Some(Self::Code),
            "prose" | "text" | "write" | "notes" => Some(Self::Prose),
            _ => None,
        }
    }

    /// Infer from a disk path. Untitled and extensionless files default to
    /// prose so a scratch note is not treated as source.
    pub fn infer(path: Option<&Path>) -> Self {
        let Some(path) = path else {
            return Self::Prose;
        };
        let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
            return Self::Prose;
        };
        match ext.to_ascii_lowercase().as_str() {
            "md" | "markdown" | "mdown" | "txt" | "text" | "rst" | "adoc" | "asciidoc" => {
                Self::Prose
            }
            _ => Self::Code,
        }
    }

    pub fn other(self) -> Self {
        match self {
            Self::Code => Self::Prose,
            Self::Prose => Self::Code,
        }
    }
}

/// Inclusive-of-text, exclusive-of-trailing-newline range of the paragraph
/// containing `cursor`. A paragraph is a run of non-blank lines.
pub fn paragraph_char_range(document: &Document, cursor: usize) -> Range<usize> {
    if document.len_chars() == 0 {
        return 0..0;
    }
    let line = document.char_to_line(cursor.min(document.len_chars()));
    let mut start_line = line;
    while start_line > 0 && !line_is_blank(document, start_line - 1) {
        start_line -= 1;
    }
    let mut end_line = line;
    while end_line + 1 < document.line_count() && !line_is_blank(document, end_line + 1) {
        end_line += 1;
    }
    document.line_start_char(start_line)..document.line_end_char(end_line)
}

/// Reflow whitespace-separated words to `column` display cells (Unicode width
/// approximated as scalar count for ASCII-heavy prose; wide graphemes count as
/// one token that may exceed the column).
pub fn hard_wrap_text(text: &str, column: usize) -> String {
    let column = column.clamp(MIN_HARD_WRAP_COLUMN, MAX_HARD_WRAP_COLUMN);
    let mut out = String::new();
    let mut col = 0usize;
    let mut first = true;
    for word in text.split_whitespace() {
        let width = word.chars().count().max(1);
        if !first && col + 1 + width > column {
            out.push('\n');
            col = 0;
            first = true;
        }
        if !first {
            out.push(' ');
            col += 1;
        }
        out.push_str(word);
        col += width;
        first = false;
        if col >= column {
            out.push('\n');
            col = 0;
            first = true;
        }
    }
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

pub const MIN_HARD_WRAP_COLUMN: usize = 8;
pub const MAX_HARD_WRAP_COLUMN: usize = 240;
pub const DEFAULT_HARD_WRAP_COLUMN: usize = 72;

fn line_is_blank(document: &Document, line: usize) -> bool {
    let start = document.line_start_char(line);
    let end = document.line_end_char(line);
    document.slice(start..end).chars().all(char::is_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Document;

    #[test]
    fn infers_markdown_as_prose_and_rust_as_code() {
        assert_eq!(
            WriteProfile::infer(Some(Path::new("notes/ideas.md"))),
            WriteProfile::Prose
        );
        assert_eq!(
            WriteProfile::infer(Some(Path::new("src/main.rs"))),
            WriteProfile::Code
        );
        assert_eq!(WriteProfile::infer(None), WriteProfile::Prose);
        assert_eq!(
            WriteProfile::infer(Some(Path::new("wscrpt-ideas"))),
            WriteProfile::Prose
        );
    }

    #[test]
    fn hard_wrap_breaks_on_words() {
        let wrapped = hard_wrap_text("hello world from wscrpt tonight", 12);
        assert_eq!(wrapped, "hello world\nfrom wscrpt\ntonight");
    }

    #[test]
    fn paragraph_range_stops_at_blank_lines() {
        let document = Document::from_text("one\n\ntwo three\nfour\n\nfive\n");
        let range = paragraph_char_range(&document, document.line_start_char(2) + 1);
        assert_eq!(document.slice(range), "two three\nfour");
    }
}
