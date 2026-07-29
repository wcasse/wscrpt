//! Lightweight, bounded syntax highlighting for the terminal renderer.
//!
//! This is deliberately a lexical highlighter rather than a parser. It keeps
//! common source/configuration files readable without starting another
//! process, downloading grammars, or walking an entire document on every
//! keystroke. Language servers remain authoritative for semantic diagnostics
//! and edits.

use std::ops::Range;
use std::path::Path;

/// Maximum Unicode-scalar length inspected on one rendered line.
pub const MAX_HIGHLIGHT_CHARS: usize = 32 * 1024;
/// Maximum number of styled ranges emitted for one rendered line.
pub const MAX_HIGHLIGHT_SPANS: usize = 2_048;
/// Maximum lines inspected for a local document outline.
pub const MAX_OUTLINE_LINES: usize = 20_000;
/// Maximum outline rows retained for one document.
pub const MAX_OUTLINE_SYMBOLS: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyntaxKind {
    Keyword,
    Type,
    String,
    Comment,
    Number,
    Constant,
    Function,
    Property,
    Heading,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyntaxSpan {
    /// Unicode-scalar range within the logical line.
    pub range: Range<usize>,
    pub kind: SyntaxKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutlineSymbol {
    pub label: String,
    pub line: usize,
    pub char_column: usize,
}

pub fn identifier_at_line_char(line: &str, char_column: usize) -> Option<String> {
    let characters = line.chars().collect::<Vec<_>>();
    if characters.is_empty() {
        return None;
    }
    let cursor = char_column.min(characters.len());
    let character_index = if cursor < characters.len() && is_identifier_continue(characters[cursor])
    {
        cursor
    } else if cursor > 0 && is_identifier_continue(characters[cursor - 1]) {
        cursor - 1
    } else {
        return None;
    };
    let mut start = character_index;
    while start > 0 && is_identifier_continue(characters[start - 1]) {
        start -= 1;
    }
    if !is_identifier_start(characters[start]) {
        return None;
    }
    let end = consume_identifier(&characters, start);
    Some(characters[start..end].iter().collect())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyntaxLanguage {
    Rust,
    Python,
    JavaScript,
    Go,
    CLike,
    Swift,
    Ruby,
    Shell,
    Json,
    Toml,
    Yaml,
    Markdown,
    Markup,
    Css,
    Sql,
    Lua,
}

pub fn language_for_path(path: Option<&Path>) -> Option<SyntaxLanguage> {
    let path = path?;
    let filename = path.file_name()?.to_string_lossy();
    let lowercase_name = filename.to_ascii_lowercase();
    if matches!(
        lowercase_name.as_str(),
        "dockerfile" | "makefile" | "justfile"
    ) {
        return Some(SyntaxLanguage::Shell);
    }

    let extension = path.extension()?.to_string_lossy().to_ascii_lowercase();
    match extension.as_str() {
        "rs" => Some(SyntaxLanguage::Rust),
        "py" | "pyi" => Some(SyntaxLanguage::Python),
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" => Some(SyntaxLanguage::JavaScript),
        "go" => Some(SyntaxLanguage::Go),
        "c" | "h" | "cc" | "hh" | "cpp" | "hpp" | "cxx" | "hxx" | "java" | "kt" | "kts" => {
            Some(SyntaxLanguage::CLike)
        }
        "swift" => Some(SyntaxLanguage::Swift),
        "rb" | "rake" => Some(SyntaxLanguage::Ruby),
        "sh" | "bash" | "zsh" | "fish" => Some(SyntaxLanguage::Shell),
        "json" | "jsonc" => Some(SyntaxLanguage::Json),
        "toml" => Some(SyntaxLanguage::Toml),
        "yaml" | "yml" => Some(SyntaxLanguage::Yaml),
        "md" | "mdx" | "markdown" => Some(SyntaxLanguage::Markdown),
        "html" | "htm" | "xml" | "svg" | "vue" | "svelte" => Some(SyntaxLanguage::Markup),
        "css" | "scss" | "sass" | "less" => Some(SyntaxLanguage::Css),
        "sql" => Some(SyntaxLanguage::Sql),
        "lua" => Some(SyntaxLanguage::Lua),
        _ => None,
    }
}

pub fn line_comment_marker_for_path(path: Option<&Path>) -> Option<&'static str> {
    let language = language_for_path(path)?;
    match language {
        SyntaxLanguage::Rust
        | SyntaxLanguage::JavaScript
        | SyntaxLanguage::Go
        | SyntaxLanguage::CLike
        | SyntaxLanguage::Swift
        | SyntaxLanguage::Json
        | SyntaxLanguage::Css => Some("//"),
        SyntaxLanguage::Python
        | SyntaxLanguage::Ruby
        | SyntaxLanguage::Shell
        | SyntaxLanguage::Toml
        | SyntaxLanguage::Yaml => Some("#"),
        SyntaxLanguage::Sql | SyntaxLanguage::Lua => Some("--"),
        SyntaxLanguage::Markdown | SyntaxLanguage::Markup => None,
    }
}

/// Carry state for multiline comments and strings across logical lines.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HighlightState {
    #[default]
    Normal,
    BlockComment,
    String(char),
}

/// Highlights a single logical line. Returned ranges never exceed the bounded
/// prefix inspected by this function.
pub fn highlight_line(path: Option<&Path>, line: &str) -> Vec<SyntaxSpan> {
    highlight_line_with_state(path, line, HighlightState::Normal).0
}

/// Highlights a line and returns the state that should seed the next line.
pub fn highlight_line_with_state(
    path: Option<&Path>,
    line: &str,
    mut state: HighlightState,
) -> (Vec<SyntaxSpan>, HighlightState) {
    let Some(language) = language_for_path(path) else {
        return (Vec::new(), HighlightState::Normal);
    };
    let characters: Vec<char> = line.chars().take(MAX_HIGHLIGHT_CHARS).collect();
    if characters.is_empty() {
        return (Vec::new(), state);
    }

    if language == SyntaxLanguage::Markdown {
        return (highlight_markdown(&characters), HighlightState::Normal);
    }
    if language == SyntaxLanguage::Markup {
        return (highlight_markup(&characters), HighlightState::Normal);
    }

    let mut spans = Vec::new();
    let mut cursor = 0;

    if state == HighlightState::BlockComment {
        let end = find_pair_end(&characters, 0, '*', '/');
        push_span(&mut spans, 0..end, SyntaxKind::Comment);
        if end < characters.len()
            && end >= 2
            && characters[end - 2] == '*'
            && characters[end - 1] == '/'
        {
            state = HighlightState::Normal;
            cursor = end;
        } else {
            return (spans, HighlightState::BlockComment);
        }
    } else if let HighlightState::String(quote) = state {
        let end = consume_quoted_continuation(&characters, 0, quote);
        push_span(&mut spans, 0..end, SyntaxKind::String);
        if end < characters.len() && characters.get(end.saturating_sub(1)) == Some(&quote) {
            // consume_quoted_continuation returns index after closing quote when closed.
            state = HighlightState::Normal;
            cursor = end;
        } else if end >= characters.len() {
            return (spans, HighlightState::String(quote));
        } else {
            state = HighlightState::Normal;
            cursor = end;
        }
    }

    while cursor < characters.len() && spans.len() < MAX_HIGHLIGHT_SPANS {
        if let Some(prefix_len) = line_comment_prefix(language, &characters, cursor) {
            push_span(&mut spans, cursor..characters.len(), SyntaxKind::Comment);
            debug_assert!(prefix_len > 0);
            state = HighlightState::Normal;
            break;
        }

        if starts_with(&characters, cursor, &['/', '*']) {
            let end = find_pair_end(&characters, cursor + 2, '*', '/');
            push_span(&mut spans, cursor..end, SyntaxKind::Comment);
            if end >= characters.len()
                || !(end >= 2 && characters[end - 2] == '*' && characters[end - 1] == '/')
            {
                state = HighlightState::BlockComment;
                break;
            }
            cursor = end;
            continue;
        }

        let character = characters[cursor];
        if is_quote(language, character) {
            if language == SyntaxLanguage::Rust
                && character == '\''
                && looks_like_rust_lifetime(&characters, cursor)
            {
                let end = consume_identifier(&characters, cursor + 1);
                push_span(&mut spans, cursor..end, SyntaxKind::Constant);
                cursor = end;
                continue;
            }
            let end = consume_quoted(&characters, cursor, character);
            push_span(&mut spans, cursor..end, SyntaxKind::String);
            if end >= characters.len() || characters.get(end.saturating_sub(1)) != Some(&character)
            {
                // Unclosed string continues on the next line for languages that
                // allow it; still useful for accidental multiline strings.
                state = HighlightState::String(character);
                break;
            }
            cursor = end;
            continue;
        }

        if character.is_ascii_digit() {
            let end = consume_number(&characters, cursor);
            push_span(&mut spans, cursor..end, SyntaxKind::Number);
            cursor = end;
            continue;
        }

        if is_identifier_start(character) {
            let end = consume_identifier(&characters, cursor);
            let word: String = characters[cursor..end].iter().collect();
            let kind = classify_identifier(language, &word, &characters, cursor, end);
            if let Some(kind) = kind {
                push_span(&mut spans, cursor..end, kind);
            }
            cursor = end;
            continue;
        }

        cursor += 1;
    }
    (spans, state)
}

/// Advance highlight state across lines without retaining spans.
pub fn advance_highlight_state(
    path: Option<&Path>,
    line: &str,
    state: HighlightState,
) -> HighlightState {
    highlight_line_with_state(path, line, state).1
}

/// Fast multiline-only state advance for scroll/index builds.
///
/// Skips keyword/number classification and does not allocate a full character
/// vector. Only tracks block comments and string/char literals so a jump to
/// the bottom of a large file stays O(document) with a tiny constant factor.
pub fn advance_multiline_state_fast(
    path: Option<&Path>,
    line: &str,
    state: HighlightState,
) -> HighlightState {
    let Some(language) = language_for_path(path) else {
        return HighlightState::Normal;
    };
    if matches!(
        language,
        SyntaxLanguage::Markdown | SyntaxLanguage::Markup | SyntaxLanguage::Json
    ) {
        return HighlightState::Normal;
    }
    advance_multiline_state_chars(language, line.chars(), state)
}

fn advance_multiline_state_chars(
    language: SyntaxLanguage,
    characters: impl Iterator<Item = char>,
    mut state: HighlightState,
) -> HighlightState {
    let mut characters = characters.peekable();
    match state {
        HighlightState::BlockComment => {
            let mut previous_was_star = false;
            for character in characters.by_ref() {
                if previous_was_star && character == '/' {
                    state = HighlightState::Normal;
                    break;
                }
                previous_was_star = character == '*';
            }
            if state == HighlightState::BlockComment {
                return HighlightState::BlockComment;
            }
        }
        HighlightState::String(quote) => {
            let mut escaped = false;
            for character in characters.by_ref() {
                if escaped {
                    escaped = false;
                    continue;
                }
                if character == '\\' {
                    escaped = true;
                    continue;
                }
                if character == quote {
                    state = HighlightState::Normal;
                    break;
                }
            }
            if let HighlightState::String(_) = state {
                return state;
            }
        }
        HighlightState::Normal => {}
    }

    while let Some(character) = characters.next() {
        if state != HighlightState::Normal {
            break;
        }
        if line_comment_starts(language, character, characters.peek().copied()) {
            break;
        }
        if character == '/' && characters.peek() == Some(&'*') {
            characters.next();
            // Scan remainder of this line for */
            let mut closed = false;
            let mut previous_was_star = false;
            for inner in characters.by_ref() {
                if previous_was_star && inner == '/' {
                    closed = true;
                    break;
                }
                previous_was_star = inner == '*';
            }
            if !closed {
                return HighlightState::BlockComment;
            }
            continue;
        }
        if is_quote(language, character) {
            if language == SyntaxLanguage::Rust
                && character == '\''
                && looks_like_rust_lifetime_peek(character, characters.peek().copied())
            {
                // lifetime: skip identifier chars
                while matches!(characters.peek(), Some(c) if is_identifier_continue(*c)) {
                    characters.next();
                }
                continue;
            }
            let quote = character;
            let mut escaped = false;
            let mut closed = false;
            for inner in characters.by_ref() {
                if escaped {
                    escaped = false;
                    continue;
                }
                if inner == '\\' {
                    escaped = true;
                    continue;
                }
                if inner == quote {
                    closed = true;
                    break;
                }
            }
            if !closed {
                return HighlightState::String(quote);
            }
        }
    }
    HighlightState::Normal
}

fn line_comment_starts(language: SyntaxLanguage, first: char, second: Option<char>) -> bool {
    match language {
        SyntaxLanguage::Rust
        | SyntaxLanguage::JavaScript
        | SyntaxLanguage::Go
        | SyntaxLanguage::CLike
        | SyntaxLanguage::Swift
        | SyntaxLanguage::Css => first == '/' && second == Some('/'),
        SyntaxLanguage::Python
        | SyntaxLanguage::Ruby
        | SyntaxLanguage::Shell
        | SyntaxLanguage::Toml
        | SyntaxLanguage::Yaml => first == '#',
        SyntaxLanguage::Sql | SyntaxLanguage::Lua => first == '-' && second == Some('-'),
        SyntaxLanguage::Json | SyntaxLanguage::Markdown | SyntaxLanguage::Markup => false,
    }
}

fn looks_like_rust_lifetime_peek(_quote: char, next: Option<char>) -> bool {
    matches!(next, Some(c) if is_identifier_start(c) || c == '_')
}

/// Sampled multiline highlight states for a document revision.
///
/// Built lazily as the viewport moves so jumping to the bottom of a large file
/// pays one linear pass with the fast state machine (no per-line `String`
/// allocations), then subsequent scrolls are O(stride).
#[derive(Clone, Debug, Default)]
pub struct HighlightStateIndex {
    editor_id: u64,
    state_id: u64,
    /// `samples[i]` is the state at the start of line `i * STRIDE`.
    samples: Vec<HighlightState>,
    /// Exclusive end of the contiguous indexed line range from line 0.
    lines_indexed: usize,
}

impl HighlightStateIndex {
    pub const STRIDE: usize = 64;

    pub fn clear(&mut self) {
        self.editor_id = 0;
        self.state_id = 0;
        self.samples.clear();
        self.lines_indexed = 0;
    }

    /// State at the start of `line` using a rope-backed fast index.
    pub fn state_before_line(
        &mut self,
        editor_id: u64,
        state_id: u64,
        path: Option<&Path>,
        rope: &ropey::Rope,
        line: usize,
    ) -> HighlightState {
        let line_count = rope.len_lines();
        if line_count == 0 {
            return HighlightState::Normal;
        }
        // ropey counts a trailing empty line after a final newline; clamp.
        let line = line.min(line_count.saturating_sub(1));
        if self.editor_id != editor_id || self.state_id != state_id {
            self.editor_id = editor_id;
            self.state_id = state_id;
            self.samples.clear();
            self.samples.push(HighlightState::Normal);
            self.lines_indexed = 0;
        }
        self.ensure_indexed(path, rope, line.saturating_add(1));
        let sample_index = line / Self::STRIDE;
        let mut state = self
            .samples
            .get(sample_index)
            .copied()
            .unwrap_or(HighlightState::Normal);
        let from = sample_index * Self::STRIDE;
        for index in from..line {
            state = advance_line_from_rope(path, rope, index, state);
        }
        state
    }

    fn ensure_indexed(
        &mut self,
        path: Option<&Path>,
        rope: &ropey::Rope,
        through_line_exclusive: usize,
    ) {
        let line_count = rope.len_lines();
        let target = through_line_exclusive.min(line_count);
        if target <= self.lines_indexed {
            return;
        }
        let mut state = if self.lines_indexed == 0 {
            HighlightState::Normal
        } else {
            let sample_index = (self.lines_indexed - 1) / Self::STRIDE;
            let mut recovered = self
                .samples
                .get(sample_index)
                .copied()
                .unwrap_or(HighlightState::Normal);
            let from = sample_index * Self::STRIDE;
            for index in from..self.lines_indexed {
                recovered = advance_line_from_rope(path, rope, index, recovered);
            }
            recovered
        };
        for index in self.lines_indexed..target {
            if index % Self::STRIDE == 0 {
                let sample_slot = index / Self::STRIDE;
                if self.samples.len() <= sample_slot {
                    self.samples.resize(sample_slot + 1, HighlightState::Normal);
                }
                self.samples[sample_slot] = state;
            }
            state = advance_line_from_rope(path, rope, index, state);
        }
        self.lines_indexed = target;
        if self.lines_indexed < line_count && self.lines_indexed.is_multiple_of(Self::STRIDE) {
            let sample_slot = self.lines_indexed / Self::STRIDE;
            if self.samples.len() <= sample_slot {
                self.samples.resize(sample_slot + 1, HighlightState::Normal);
            }
            self.samples[sample_slot] = state;
        }
    }
}

fn advance_line_from_rope(
    path: Option<&Path>,
    rope: &ropey::Rope,
    line: usize,
    state: HighlightState,
) -> HighlightState {
    let slice = rope.line(line);
    // Avoid allocating a String: walk rope chunks as chars.
    advance_multiline_state_chars(
        language_for_path(path).unwrap_or(SyntaxLanguage::Markdown),
        slice
            .chars()
            .filter(|character| !matches!(character, '\n' | '\r')),
        state,
    )
}

#[cfg(test)]
mod index_tests {
    use super::*;
    use ropey::Rope;

    #[test]
    fn highlight_index_reaches_file_bottom_without_per_frame_full_rescan() {
        let mut body = String::new();
        body.push_str("/* open\n");
        for index in 0..2_000 {
            body.push_str(&format!("line {index} plain text\n"));
        }
        body.push_str("still comment */\nfn done() {}\n");
        let rope = Rope::from_str(&body);
        let path = Path::new("big.rs");
        let mut index = HighlightStateIndex::default();
        let last = rope.len_lines().saturating_sub(2);
        let state = index.state_before_line(1, 1, Some(path), &rope, last);
        // After the closing */ the final function line should start normal.
        assert_eq!(state, HighlightState::Normal);
        // Second query at the same bottom must reuse samples (still correct).
        let again = index.state_before_line(1, 1, Some(path), &rope, last);
        assert_eq!(again, HighlightState::Normal);
        assert!(index.samples.len() > 10);
    }

    #[test]
    fn fast_state_tracks_unclosed_block_comment() {
        let state = advance_multiline_state_fast(
            Some(Path::new("x.rs")),
            "/* start",
            HighlightState::Normal,
        );
        assert_eq!(state, HighlightState::BlockComment);
        let state = advance_multiline_state_fast(Some(Path::new("x.rs")), " middle", state);
        assert_eq!(state, HighlightState::BlockComment);
        let state = advance_multiline_state_fast(Some(Path::new("x.rs")), " end */ code", state);
        assert_eq!(state, HighlightState::Normal);
    }
}

pub fn outline_symbols(path: Option<&Path>, text: &str) -> Vec<OutlineSymbol> {
    let Some(language) = language_for_path(path) else {
        return Vec::new();
    };
    let mut symbols = Vec::new();
    let mut offset = 0;
    for (line_index, raw_line) in text.lines().take(MAX_OUTLINE_LINES).enumerate() {
        if symbols.len() >= MAX_OUTLINE_SYMBOLS {
            break;
        }
        if let Some(symbol) = outline_symbol_for_line(language, raw_line, line_index, offset) {
            symbols.push(symbol);
        }
        offset += raw_line.len() + 1;
    }
    symbols
}

fn outline_symbol_for_line(
    language: SyntaxLanguage,
    raw_line: &str,
    line_index: usize,
    _line_start_byte: usize,
) -> Option<OutlineSymbol> {
    let trimmed = raw_line.trim_start();
    if trimmed.is_empty() {
        return None;
    }
    let indent = raw_line.len().saturating_sub(trimmed.len());
    if is_outline_comment(language, trimmed) {
        return None;
    }
    match language {
        SyntaxLanguage::Markdown => markdown_outline_symbol(trimmed, line_index, indent),
        SyntaxLanguage::Rust => declaration_after_any(
            raw_line,
            line_index,
            &[
                "fn", "struct", "enum", "trait", "impl", "mod", "type", "const", "static",
            ],
        ),
        SyntaxLanguage::Python => declaration_after_any(raw_line, line_index, &["def", "class"]),
        SyntaxLanguage::JavaScript => declaration_after_any(
            raw_line,
            line_index,
            &[
                "function",
                "class",
                "interface",
                "type",
                "enum",
                "const",
                "let",
                "var",
            ],
        ),
        SyntaxLanguage::Go => go_outline_symbol(raw_line, line_index),
        _ => None,
    }
}

fn markdown_outline_symbol(
    trimmed: &str,
    line_index: usize,
    indent_chars: usize,
) -> Option<OutlineSymbol> {
    let hashes = trimmed
        .chars()
        .take_while(|character| *character == '#')
        .count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let title = trimmed[hashes..].trim();
    if title.is_empty() {
        return None;
    }
    Some(OutlineSymbol {
        label: format!("{}{}", "  ".repeat(hashes.saturating_sub(1)), title),
        line: line_index,
        char_column: indent_chars + hashes + trimmed[hashes..].len().saturating_sub(title.len()),
    })
}

fn go_outline_symbol(raw_line: &str, line_index: usize) -> Option<OutlineSymbol> {
    let byte = find_word(raw_line, "func")?;
    let after = raw_line[byte + "func".len()..].trim_start();
    let skipped = raw_line[byte + "func".len()..].len() - after.len();
    let name_byte = if after.starts_with('(') {
        let receiver_end = after.find(')')?;
        byte + "func".len() + skipped + receiver_end + 1 + after[receiver_end + 1..].len()
            - after[receiver_end + 1..].trim_start().len()
    } else {
        byte + "func".len() + skipped
    };
    identifier_at(raw_line, name_byte).map(|(name, start)| OutlineSymbol {
        label: format!("func {name}"),
        line: line_index,
        char_column: byte_to_line_char(raw_line, start),
    })
}

fn declaration_after_any(
    raw_line: &str,
    line_index: usize,
    keywords: &[&str],
) -> Option<OutlineSymbol> {
    for keyword in keywords {
        if let Some(byte) = find_word(raw_line, keyword) {
            let after_start = byte + keyword.len();
            let after = raw_line[after_start..].trim_start();
            let name_byte = after_start + raw_line[after_start..].len() - after.len();
            if let Some((name, start)) = identifier_at(raw_line, name_byte) {
                return Some(OutlineSymbol {
                    label: format!("{keyword} {name}"),
                    line: line_index,
                    char_column: byte_to_line_char(raw_line, start),
                });
            }
            if *keyword == "impl" {
                let impl_label = after
                    .split('{')
                    .next()
                    .unwrap_or(after)
                    .trim()
                    .trim_end_matches("where")
                    .trim();
                if !impl_label.is_empty() {
                    return Some(OutlineSymbol {
                        label: format!("impl {impl_label}"),
                        line: line_index,
                        char_column: byte_to_line_char(raw_line, name_byte),
                    });
                }
            }
        }
    }
    None
}

fn is_outline_comment(language: SyntaxLanguage, trimmed: &str) -> bool {
    match language {
        SyntaxLanguage::Python | SyntaxLanguage::Shell | SyntaxLanguage::Ruby => {
            trimmed.starts_with('#')
        }
        SyntaxLanguage::Markdown => false,
        _ => trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*'),
    }
}

fn find_word(line: &str, word: &str) -> Option<usize> {
    let mut cursor = 0;
    while cursor < line.len() {
        let Some(found) = line[cursor..].find(word) else {
            break;
        };
        let byte = cursor + found;
        let before_ok = line[..byte]
            .chars()
            .next_back()
            .is_none_or(|character| !is_identifier_continue(character));
        let after_byte = byte + word.len();
        let after_ok = line[after_byte..]
            .chars()
            .next()
            .is_none_or(|character| !is_identifier_continue(character));
        if before_ok && after_ok {
            return Some(byte);
        }
        cursor = after_byte;
    }
    None
}

fn identifier_at(line: &str, byte: usize) -> Option<(String, usize)> {
    let mut start = byte;
    while start < line.len() {
        let character = line[start..].chars().next()?;
        if is_identifier_start(character) {
            break;
        }
        if !character.is_whitespace() {
            return None;
        }
        start += character.len_utf8();
    }
    let mut end = start;
    while end < line.len() {
        let character = line[end..].chars().next()?;
        if !is_identifier_continue(character) {
            break;
        }
        end += character.len_utf8();
    }
    (start < end).then(|| (line[start..end].to_owned(), start))
}

fn byte_to_line_char(line: &str, byte: usize) -> usize {
    line[..byte.min(line.len())].chars().count()
}

fn highlight_markdown(characters: &[char]) -> Vec<SyntaxSpan> {
    let mut spans = Vec::new();
    let first = characters
        .iter()
        .position(|character| !character.is_whitespace())
        .unwrap_or(characters.len());
    let hashes = characters[first..]
        .iter()
        .take_while(|character| **character == '#')
        .count();
    if (1..=6).contains(&hashes)
        && characters
            .get(first + hashes)
            .is_some_and(|character| character.is_whitespace())
    {
        push_span(&mut spans, first..characters.len(), SyntaxKind::Heading);
        return spans;
    }

    let mut cursor = 0;
    while cursor < characters.len() && spans.len() < MAX_HIGHLIGHT_SPANS {
        if characters[cursor] == '`' {
            let end = consume_quoted(characters, cursor, '`');
            push_span(&mut spans, cursor..end, SyntaxKind::String);
            cursor = end;
        } else if starts_with(characters, cursor, &['[']) {
            let end = characters[cursor + 1..]
                .iter()
                .position(|character| *character == ']')
                .map_or(cursor + 1, |relative| cursor + relative + 2);
            if end > cursor + 1 {
                push_span(&mut spans, cursor..end, SyntaxKind::Property);
            }
            cursor = end.max(cursor + 1);
        } else {
            cursor += 1;
        }
    }
    spans
}

fn highlight_markup(characters: &[char]) -> Vec<SyntaxSpan> {
    let mut spans = Vec::new();
    let mut cursor = 0;
    while cursor < characters.len() && spans.len() < MAX_HIGHLIGHT_SPANS {
        if starts_with(characters, cursor, &['<', '!', '-', '-']) {
            let end = find_sequence_end(characters, cursor + 4, &['-', '-', '>']);
            push_span(&mut spans, cursor..end, SyntaxKind::Comment);
            cursor = end;
        } else if characters[cursor] == '<' {
            let end = characters[cursor..]
                .iter()
                .position(|character| *character == '>')
                .map_or(characters.len(), |relative| cursor + relative + 1);
            push_span(&mut spans, cursor..end, SyntaxKind::Keyword);
            cursor = end;
        } else {
            cursor += 1;
        }
    }
    spans
}

fn line_comment_prefix(
    language: SyntaxLanguage,
    characters: &[char],
    cursor: usize,
) -> Option<usize> {
    let slash = matches!(
        language,
        SyntaxLanguage::Rust
            | SyntaxLanguage::JavaScript
            | SyntaxLanguage::Go
            | SyntaxLanguage::CLike
            | SyntaxLanguage::Swift
            | SyntaxLanguage::Json
            | SyntaxLanguage::Css
    ) && starts_with(characters, cursor, &['/', '/']);
    let hash = matches!(
        language,
        SyntaxLanguage::Python
            | SyntaxLanguage::Ruby
            | SyntaxLanguage::Shell
            | SyntaxLanguage::Toml
            | SyntaxLanguage::Yaml
    ) && characters[cursor] == '#';
    let dash = matches!(language, SyntaxLanguage::Sql | SyntaxLanguage::Lua)
        && starts_with(characters, cursor, &['-', '-']);
    slash
        .then_some(2)
        .or_else(|| hash.then_some(1))
        .or_else(|| dash.then_some(2))
}

fn is_quote(language: SyntaxLanguage, character: char) -> bool {
    character == '"'
        || (character == '\'' && language != SyntaxLanguage::Json)
        || (character == '`'
            && matches!(language, SyntaxLanguage::JavaScript | SyntaxLanguage::Shell))
}

fn looks_like_rust_lifetime(characters: &[char], cursor: usize) -> bool {
    let Some(next) = characters.get(cursor + 1) else {
        return false;
    };
    if !is_identifier_start(*next) {
        return false;
    }
    let end = consume_identifier(characters, cursor + 1);
    characters.get(end) != Some(&'\'')
}

fn consume_quoted(characters: &[char], start: usize, quote: char) -> usize {
    let mut cursor = start + 1;
    let mut escaped = false;
    while cursor < characters.len() {
        let character = characters[cursor];
        cursor += 1;
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == quote {
            break;
        }
    }
    cursor
}

/// Like [`consume_quoted`], but the opening quote was already consumed on a
/// previous line. Starts at `start` inside the string body.
fn consume_quoted_continuation(characters: &[char], start: usize, quote: char) -> usize {
    let mut cursor = start;
    let mut escaped = false;
    while cursor < characters.len() {
        let character = characters[cursor];
        cursor += 1;
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == quote {
            break;
        }
    }
    cursor
}

fn consume_number(characters: &[char], start: usize) -> usize {
    let mut cursor = start + 1;
    while cursor < characters.len()
        && (characters[cursor].is_ascii_alphanumeric()
            || matches!(characters[cursor], '_' | '.' | '+' | '-'))
    {
        cursor += 1;
    }
    cursor
}

fn consume_identifier(characters: &[char], start: usize) -> usize {
    let mut cursor = start;
    while cursor < characters.len() && is_identifier_continue(characters[cursor]) {
        cursor += 1;
    }
    cursor
}

fn is_identifier_start(character: char) -> bool {
    character == '_' || character.is_alphabetic()
}

fn is_identifier_continue(character: char) -> bool {
    character == '_' || character.is_alphanumeric()
}

fn classify_identifier(
    language: SyntaxLanguage,
    word: &str,
    characters: &[char],
    start: usize,
    end: usize,
) -> Option<SyntaxKind> {
    if is_keyword(language, word) {
        return Some(SyntaxKind::Keyword);
    }
    if matches!(
        word,
        "true" | "false" | "null" | "nil" | "None" | "True" | "False"
    ) {
        return Some(SyntaxKind::Constant);
    }
    if previous_identifier(characters, start).is_some_and(|previous| {
        matches!(
            previous.as_str(),
            "def" | "fn" | "func" | "function" | "fun"
        )
    }) {
        return Some(SyntaxKind::Function);
    }
    if characters[..start]
        .iter()
        .rev()
        .find(|character| !character.is_whitespace())
        == Some(&'.')
    {
        return Some(SyntaxKind::Property);
    }
    let next = characters[end..]
        .iter()
        .find(|character| !character.is_whitespace());
    if next == Some(&'(') {
        return Some(SyntaxKind::Function);
    }
    if word.chars().next().is_some_and(char::is_uppercase) {
        return Some(SyntaxKind::Type);
    }
    None
}

fn previous_identifier(characters: &[char], before: usize) -> Option<String> {
    let end = characters[..before]
        .iter()
        .rposition(|character| !character.is_whitespace())?
        + 1;
    let start = characters[..end]
        .iter()
        .rposition(|character| !is_identifier_continue(*character))
        .map_or(0, |index| index + 1);
    (start < end).then(|| characters[start..end].iter().collect())
}

fn is_keyword(language: SyntaxLanguage, word: &str) -> bool {
    match language {
        SyntaxLanguage::Rust => matches!(
            word,
            "as" | "async"
                | "await"
                | "break"
                | "const"
                | "continue"
                | "crate"
                | "dyn"
                | "else"
                | "enum"
                | "extern"
                | "fn"
                | "for"
                | "if"
                | "impl"
                | "in"
                | "let"
                | "loop"
                | "match"
                | "mod"
                | "move"
                | "mut"
                | "pub"
                | "ref"
                | "return"
                | "self"
                | "Self"
                | "static"
                | "struct"
                | "super"
                | "trait"
                | "type"
                | "union"
                | "unsafe"
                | "use"
                | "where"
                | "while"
        ),
        SyntaxLanguage::Python => matches!(
            word,
            "and"
                | "as"
                | "assert"
                | "async"
                | "await"
                | "break"
                | "class"
                | "continue"
                | "def"
                | "del"
                | "elif"
                | "else"
                | "except"
                | "finally"
                | "for"
                | "from"
                | "global"
                | "if"
                | "import"
                | "in"
                | "is"
                | "lambda"
                | "nonlocal"
                | "not"
                | "or"
                | "pass"
                | "raise"
                | "return"
                | "try"
                | "while"
                | "with"
                | "yield"
        ),
        SyntaxLanguage::JavaScript => matches!(
            word,
            "as" | "async"
                | "await"
                | "break"
                | "case"
                | "catch"
                | "class"
                | "const"
                | "continue"
                | "debugger"
                | "default"
                | "delete"
                | "do"
                | "else"
                | "enum"
                | "export"
                | "extends"
                | "finally"
                | "for"
                | "from"
                | "function"
                | "get"
                | "if"
                | "implements"
                | "import"
                | "in"
                | "instanceof"
                | "interface"
                | "let"
                | "new"
                | "of"
                | "private"
                | "protected"
                | "public"
                | "readonly"
                | "return"
                | "set"
                | "static"
                | "super"
                | "switch"
                | "this"
                | "throw"
                | "try"
                | "type"
                | "typeof"
                | "var"
                | "void"
                | "while"
                | "with"
                | "yield"
        ),
        SyntaxLanguage::Go => matches!(
            word,
            "break"
                | "case"
                | "chan"
                | "const"
                | "continue"
                | "default"
                | "defer"
                | "else"
                | "fallthrough"
                | "for"
                | "func"
                | "go"
                | "goto"
                | "if"
                | "import"
                | "interface"
                | "map"
                | "package"
                | "range"
                | "return"
                | "select"
                | "struct"
                | "switch"
                | "type"
                | "var"
        ),
        SyntaxLanguage::CLike | SyntaxLanguage::Swift => matches!(
            word,
            "abstract"
                | "auto"
                | "break"
                | "case"
                | "catch"
                | "class"
                | "const"
                | "continue"
                | "default"
                | "defer"
                | "do"
                | "else"
                | "enum"
                | "extends"
                | "final"
                | "finally"
                | "for"
                | "fun"
                | "func"
                | "if"
                | "implements"
                | "import"
                | "in"
                | "interface"
                | "internal"
                | "namespace"
                | "new"
                | "open"
                | "operator"
                | "override"
                | "package"
                | "private"
                | "protected"
                | "protocol"
                | "public"
                | "return"
                | "static"
                | "struct"
                | "switch"
                | "template"
                | "this"
                | "throw"
                | "throws"
                | "try"
                | "typedef"
                | "typename"
                | "using"
                | "val"
                | "var"
                | "virtual"
                | "void"
                | "when"
                | "where"
                | "while"
        ),
        SyntaxLanguage::Ruby => matches!(
            word,
            "alias"
                | "begin"
                | "break"
                | "case"
                | "class"
                | "def"
                | "defined"
                | "do"
                | "else"
                | "elsif"
                | "end"
                | "ensure"
                | "for"
                | "if"
                | "in"
                | "module"
                | "next"
                | "redo"
                | "rescue"
                | "retry"
                | "return"
                | "self"
                | "super"
                | "then"
                | "undef"
                | "unless"
                | "until"
                | "when"
                | "while"
                | "yield"
        ),
        SyntaxLanguage::Shell => matches!(
            word,
            "case"
                | "coproc"
                | "do"
                | "done"
                | "elif"
                | "else"
                | "esac"
                | "export"
                | "fi"
                | "for"
                | "function"
                | "if"
                | "in"
                | "local"
                | "readonly"
                | "select"
                | "then"
                | "time"
                | "until"
                | "while"
        ),
        SyntaxLanguage::Json => false,
        SyntaxLanguage::Toml | SyntaxLanguage::Yaml => matches!(word, "true" | "false" | "null"),
        SyntaxLanguage::Css => matches!(
            word,
            "and" | "from" | "important" | "media" | "not" | "only" | "or" | "supports" | "to"
        ),
        SyntaxLanguage::Sql => matches!(
            word.to_ascii_uppercase().as_str(),
            "ALTER"
                | "AND"
                | "AS"
                | "ASC"
                | "BEGIN"
                | "BY"
                | "CASE"
                | "CREATE"
                | "DELETE"
                | "DESC"
                | "DISTINCT"
                | "DROP"
                | "ELSE"
                | "END"
                | "FROM"
                | "FULL"
                | "GROUP"
                | "HAVING"
                | "INNER"
                | "INSERT"
                | "INTO"
                | "JOIN"
                | "LEFT"
                | "LIMIT"
                | "NOT"
                | "NULL"
                | "ON"
                | "OR"
                | "ORDER"
                | "OUTER"
                | "RETURNING"
                | "RIGHT"
                | "SELECT"
                | "SET"
                | "TABLE"
                | "THEN"
                | "UNION"
                | "UPDATE"
                | "VALUES"
                | "WHEN"
                | "WHERE"
        ),
        SyntaxLanguage::Lua => matches!(
            word,
            "and"
                | "break"
                | "do"
                | "else"
                | "elseif"
                | "end"
                | "for"
                | "function"
                | "goto"
                | "if"
                | "in"
                | "local"
                | "not"
                | "or"
                | "repeat"
                | "return"
                | "then"
                | "until"
                | "while"
        ),
        SyntaxLanguage::Markdown | SyntaxLanguage::Markup => false,
    }
}

fn starts_with(characters: &[char], cursor: usize, needle: &[char]) -> bool {
    characters.get(cursor..cursor.saturating_add(needle.len())) == Some(needle)
}

fn find_pair_end(characters: &[char], mut cursor: usize, first: char, second: char) -> usize {
    while cursor + 1 < characters.len() {
        if characters[cursor] == first && characters[cursor + 1] == second {
            return cursor + 2;
        }
        cursor += 1;
    }
    characters.len()
}

fn find_sequence_end(characters: &[char], mut cursor: usize, needle: &[char]) -> usize {
    while cursor < characters.len() {
        if starts_with(characters, cursor, needle) {
            return cursor + needle.len();
        }
        cursor += 1;
    }
    characters.len()
}

fn push_span(spans: &mut Vec<SyntaxSpan>, range: Range<usize>, kind: SyntaxKind) {
    if range.start < range.end && spans.len() < MAX_HIGHLIGHT_SPANS {
        spans.push(SyntaxSpan { range, kind });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn text_for_span(line: &str, span: &SyntaxSpan) -> String {
        line.chars()
            .skip(span.range.start)
            .take(span.range.end - span.range.start)
            .collect()
    }

    #[test]
    fn detects_common_languages_and_special_filenames() {
        assert_eq!(
            language_for_path(Some(&PathBuf::from("src/main.rs"))),
            Some(SyntaxLanguage::Rust)
        );
        assert_eq!(
            language_for_path(Some(&PathBuf::from("Dockerfile"))),
            Some(SyntaxLanguage::Shell)
        );
        assert_eq!(language_for_path(Some(&PathBuf::from("image.bin"))), None);
    }

    #[test]
    fn rust_highlighting_uses_unicode_scalar_ranges() {
        let path = PathBuf::from("main.rs");
        let line = "pub fn café<'a>() -> &'a str { \"世界//\" } // note";
        let spans = highlight_line(Some(&path), line);
        let styled: Vec<_> = spans
            .iter()
            .map(|span| (text_for_span(line, span), span.kind))
            .collect();
        assert!(styled.contains(&("pub".to_owned(), SyntaxKind::Keyword)));
        assert!(styled.contains(&("fn".to_owned(), SyntaxKind::Keyword)));
        assert!(styled.contains(&("café".to_owned(), SyntaxKind::Function)));
        assert!(styled.contains(&("'a".to_owned(), SyntaxKind::Constant)));
        assert!(styled.contains(&("\"世界//\"".to_owned(), SyntaxKind::String)));
        assert!(styled.contains(&("// note".to_owned(), SyntaxKind::Comment)));
    }

    #[test]
    fn comment_markers_inside_python_strings_stay_strings() {
        let path = PathBuf::from("main.py");
        let line = "value = '# not comment' # comment";
        let spans = highlight_line(Some(&path), line);
        let styled: Vec<_> = spans
            .iter()
            .map(|span| (text_for_span(line, span), span.kind))
            .collect();
        assert!(styled.contains(&("'# not comment'".to_owned(), SyntaxKind::String)));
        assert!(styled.contains(&("# comment".to_owned(), SyntaxKind::Comment)));
    }

    #[test]
    fn headings_and_inline_code_are_distinct() {
        let path = PathBuf::from("README.md");
        let heading = highlight_line(Some(&path), "## Streamlined IDE");
        assert_eq!(heading[0].kind, SyntaxKind::Heading);
        let inline = highlight_line(Some(&path), "Run `cargo test` now");
        assert_eq!(
            text_for_span("Run `cargo test` now", &inline[0]),
            "`cargo test`"
        );
        assert_eq!(inline[0].kind, SyntaxKind::String);
    }

    #[test]
    fn local_outline_extracts_common_declarations_and_headings() {
        let rust = PathBuf::from("main.rs");
        let symbols = outline_symbols(
            Some(&rust),
            "use crate::x;\npub struct Bird;\nimpl Bird {\n    pub fn fly(&self) {}\n}\n",
        );
        assert_eq!(
            symbols
                .iter()
                .map(|symbol| (symbol.label.as_str(), symbol.line))
                .collect::<Vec<_>>(),
            [("struct Bird", 1), ("impl Bird", 2), ("fn fly", 3)]
        );

        let markdown = PathBuf::from("README.md");
        let symbols = outline_symbols(
            Some(&markdown),
            "# Title\ntext\n## Setup\n### Install\nnot a heading\n",
        );
        assert_eq!(
            symbols
                .iter()
                .map(|symbol| symbol.label.as_str())
                .collect::<Vec<_>>(),
            ["Title", "  Setup", "    Install"]
        );
    }

    #[test]
    fn identifier_at_line_char_handles_inside_and_after_identifier() {
        let line = "let café_call = value42();";
        assert_eq!(
            identifier_at_line_char(line, 5).as_deref(),
            Some("café_call")
        );
        assert_eq!(
            identifier_at_line_char(line, 13).as_deref(),
            Some("café_call")
        );
        assert_eq!(
            identifier_at_line_char(line, 22).as_deref(),
            Some("value42")
        );
        assert_eq!(identifier_at_line_char(line, 14), None);
        assert_eq!(identifier_at_line_char("let 42value = 1;", 5), None);
    }

    #[test]
    fn pathological_lines_are_bounded() {
        let path = PathBuf::from("main.rs");
        let line = "fn value() { 1 } ".repeat(MAX_HIGHLIGHT_SPANS * 4);
        let spans = highlight_line(Some(&path), &line);
        assert!(spans.len() <= MAX_HIGHLIGHT_SPANS);
        assert!(
            spans
                .iter()
                .all(|span| span.range.end <= MAX_HIGHLIGHT_CHARS)
        );
    }
}
