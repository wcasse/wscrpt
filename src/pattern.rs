//! Shared find/replace pattern parsing.
//!
//! Default mode remains literal. Prefix a pattern with `re:` for a regular
//! expression, or `re:i:` for a case-insensitive regular expression.
//!
//! Examples:
//! - `foo` — case-sensitive or case-insensitive literal (caller chooses default)
//! - `re:foo.*bar` — case-sensitive regex
//! - `re:i:todo|fixme` — case-insensitive regex
//!
//! Regex compilation is size-bounded. Replacement templates may use `$0`, `$1`,
//! or named capture syntax supported by the `regex` crate.

use std::ops::Range;
use std::sync::Arc;

use regex::{Regex, RegexBuilder};
use thiserror::Error;

/// Largest pattern source accepted for compilation.
pub const MAX_PATTERN_BYTES: usize = 4 * 1024;
/// Largest compiled DFA size the regex engine may allocate.
pub const MAX_REGEX_SIZE_LIMIT: usize = 1024 * 1024;
/// Largest DFA state set size.
pub const MAX_REGEX_DFA_SIZE_LIMIT: usize = 1024 * 1024;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PatternError {
    #[error("search pattern is empty")]
    Empty,
    #[error("search pattern is {bytes} bytes; limit is {limit} bytes")]
    TooLarge { bytes: usize, limit: usize },
    #[error("invalid regular expression: {message}")]
    InvalidRegex { message: String },
}

/// A compiled find pattern used by buffer search, project search, and replace.
#[derive(Clone, Debug)]
pub enum Pattern {
    Literal {
        text: Arc<str>,
        case_sensitive: bool,
    },
    Regex(Arc<Regex>),
}

impl Pattern {
    /// Parse a user pattern.
    ///
    /// `default_case_sensitive` applies only to literal patterns. Regex
    /// case-sensitivity is controlled by the `re:` / `re:i:` prefix.
    pub fn parse(input: &str, default_case_sensitive: bool) -> Result<Self, PatternError> {
        if input.is_empty() {
            return Err(PatternError::Empty);
        }
        if input.len() > MAX_PATTERN_BYTES {
            return Err(PatternError::TooLarge {
                bytes: input.len(),
                limit: MAX_PATTERN_BYTES,
            });
        }
        if let Some(rest) = input.strip_prefix("re:i:") {
            return Self::compile_regex(rest, false);
        }
        if let Some(rest) = input.strip_prefix("re:") {
            return Self::compile_regex(rest, true);
        }
        Ok(Self::Literal {
            text: Arc::from(input),
            case_sensitive: default_case_sensitive,
        })
    }

    fn compile_regex(source: &str, case_sensitive: bool) -> Result<Self, PatternError> {
        if source.is_empty() {
            return Err(PatternError::Empty);
        }
        let mut builder = RegexBuilder::new(source);
        builder
            .case_insensitive(!case_sensitive)
            .size_limit(MAX_REGEX_SIZE_LIMIT)
            .dfa_size_limit(MAX_REGEX_DFA_SIZE_LIMIT)
            .unicode(true)
            .multi_line(true);
        match builder.build() {
            Ok(regex) => Ok(Self::Regex(Arc::new(regex))),
            Err(error) => Err(PatternError::InvalidRegex {
                message: error.to_string(),
            }),
        }
    }

    pub fn is_regex(&self) -> bool {
        matches!(self, Self::Regex(_))
    }

    pub fn mode_label(&self) -> &'static str {
        match self {
            Self::Literal { .. } => "literal",
            Self::Regex(_) => "regex",
        }
    }

    /// Find the next non-overlapping match at or after `from_byte`.
    pub fn find_from(&self, haystack: &str, from_byte: usize) -> Option<Range<usize>> {
        let from = from_byte.min(haystack.len());
        let slice = &haystack[from..];
        match self {
            Self::Literal {
                text,
                case_sensitive: true,
            } => slice.find(text.as_ref()).map(|offset| {
                let start = from + offset;
                start..start + text.len()
            }),
            Self::Literal {
                text,
                case_sensitive: false,
            } => find_case_insensitive(slice, text.as_ref()).map(|range| {
                let start = from + range.start;
                start..from + range.end
            }),
            Self::Regex(regex) => regex.find(slice).map(|matched| {
                let start = from + matched.start();
                start..from + matched.end()
            }),
        }
    }

    /// Find the last non-overlapping match that ends at or before `before_byte`.
    pub fn find_previous(&self, haystack: &str, before_byte: usize) -> Option<Range<usize>> {
        let before = before_byte.min(haystack.len());
        let slice = &haystack[..before];
        match self {
            Self::Literal {
                text,
                case_sensitive: true,
            } => slice
                .rfind(text.as_ref())
                .map(|start| start..start + text.len()),
            Self::Literal {
                text,
                case_sensitive: false,
            } => {
                let mut last = None;
                let mut offset = 0;
                while let Some(range) = find_case_insensitive(&slice[offset..], text.as_ref()) {
                    let start = offset + range.start;
                    let end = offset + range.end;
                    if end > before {
                        break;
                    }
                    last = Some(start..end);
                    offset = end.max(offset + 1);
                }
                last
            }
            Self::Regex(regex) => {
                let mut last = None;
                for matched in regex.find_iter(slice) {
                    last = Some(matched.start()..matched.end());
                }
                last
            }
        }
    }

    /// Collect non-overlapping match ranges, stopping at `limit`.
    pub fn find_all(
        &self,
        haystack: &str,
        limit: usize,
        mut cancelled: impl FnMut() -> bool,
    ) -> Option<Vec<Range<usize>>> {
        if limit == 0 {
            return Some(Vec::new());
        }
        let mut ranges = Vec::new();
        let mut offset = 0;
        while offset <= haystack.len() && ranges.len() < limit {
            if cancelled() {
                return None;
            }
            let Some(range) = self.find_from(haystack, offset) else {
                break;
            };
            // Zero-width matches must advance to avoid infinite loops.
            let next = if range.start == range.end {
                next_char_boundary(haystack, range.end)
            } else {
                range.end
            };
            ranges.push(range);
            if next <= offset {
                break;
            }
            offset = next;
        }
        Some(ranges)
    }

    /// Replace every non-overlapping match. Returns `(updated, count)`.
    ///
    /// For regex patterns, `replacement` may contain `$n` / `${name}` templates.
    /// Aggregate inserted replacement bytes are bounded by `max_result_bytes`.
    pub fn replace_all(
        &self,
        haystack: &str,
        replacement: &str,
        max_result_bytes: usize,
    ) -> Result<(String, usize), PatternError> {
        match self {
            Self::Literal {
                text,
                case_sensitive: true,
            } => {
                let count = haystack.matches(text.as_ref()).count();
                if count == 0 {
                    return Ok((haystack.to_owned(), 0));
                }
                let updated = haystack.replace(text.as_ref(), replacement);
                if updated.len() > max_result_bytes {
                    return Err(PatternError::InvalidRegex {
                        message: format!(
                            "replacement result is {} bytes; limit is {max_result_bytes} bytes",
                            updated.len()
                        ),
                    });
                }
                Ok((updated, count))
            }
            Self::Literal {
                text: _,
                case_sensitive: false,
            } => {
                let ranges = self
                    .find_all(haystack, usize::MAX, || false)
                    .unwrap_or_default();
                apply_literal_ranges(haystack, &ranges, replacement, max_result_bytes)
            }
            Self::Regex(regex) => {
                let mut count = 0_usize;
                let mut last_end = 0_usize;
                let mut updated = String::new();
                for captures in regex.captures_iter(haystack) {
                    let matched = captures.get(0).expect("group 0 always exists");
                    updated.push_str(&haystack[last_end..matched.start()]);
                    captures.expand(replacement, &mut updated);
                    if updated.len() > max_result_bytes {
                        return Err(PatternError::InvalidRegex {
                            message: format!(
                                "replacement result is {} bytes; limit is {max_result_bytes} bytes",
                                updated.len()
                            ),
                        });
                    }
                    count += 1;
                    last_end = matched.end();
                    if matched.start() == matched.end() {
                        // Advance past zero-width matches.
                        if last_end >= haystack.len() {
                            break;
                        }
                        let next = next_char_boundary(haystack, last_end);
                        if next == last_end {
                            break;
                        }
                    }
                }
                if count == 0 {
                    return Ok((haystack.to_owned(), 0));
                }
                updated.push_str(&haystack[last_end..]);
                if updated.len() > max_result_bytes {
                    return Err(PatternError::InvalidRegex {
                        message: format!(
                            "replacement result is {} bytes; limit is {max_result_bytes} bytes",
                            updated.len()
                        ),
                    });
                }
                Ok((updated, count))
            }
        }
    }
}

fn apply_literal_ranges(
    haystack: &str,
    ranges: &[Range<usize>],
    replacement: &str,
    max_result_bytes: usize,
) -> Result<(String, usize), PatternError> {
    if ranges.is_empty() {
        return Ok((haystack.to_owned(), 0));
    }
    let mut updated = String::new();
    let mut offset = 0;
    for range in ranges {
        updated.push_str(&haystack[offset..range.start]);
        updated.push_str(replacement);
        if updated.len() > max_result_bytes {
            return Err(PatternError::InvalidRegex {
                message: format!(
                    "replacement result is {} bytes; limit is {max_result_bytes} bytes",
                    updated.len()
                ),
            });
        }
        offset = range.end;
    }
    updated.push_str(&haystack[offset..]);
    if updated.len() > max_result_bytes {
        return Err(PatternError::InvalidRegex {
            message: format!(
                "replacement result is {} bytes; limit is {max_result_bytes} bytes",
                updated.len()
            ),
        });
    }
    Ok((updated, ranges.len()))
}

fn find_case_insensitive(haystack: &str, needle: &str) -> Option<Range<usize>> {
    if needle.is_empty() {
        return None;
    }
    let needle_chars: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
    if needle_chars.is_empty() {
        return None;
    }
    let hay: Vec<(usize, char)> = haystack
        .char_indices()
        .flat_map(|(index, character)| {
            character
                .to_lowercase()
                .map(move |folded| (index, folded))
                .collect::<Vec<_>>()
        })
        .collect();
    if hay.len() < needle_chars.len() {
        return None;
    }
    'outer: for start in 0..=(hay.len() - needle_chars.len()) {
        for (offset, expected) in needle_chars.iter().enumerate() {
            if hay[start + offset].1 != *expected {
                continue 'outer;
            }
        }
        let byte_start = hay[start].0;
        let end_index = start + needle_chars.len() - 1;
        let end_char = haystack[hay[end_index].0..]
            .chars()
            .next()
            .map(|character| character.len_utf8())
            .unwrap_or(0);
        let byte_end = hay[end_index].0 + end_char;
        return Some(byte_start..byte_end);
    }
    None
}

fn next_char_boundary(text: &str, index: usize) -> usize {
    if index >= text.len() {
        return text.len();
    }
    let mut end = index + 1;
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_default_and_regex_prefix() {
        let literal = Pattern::parse("bird", true).unwrap();
        assert!(!literal.is_regex());
        assert_eq!(literal.find_from("a bird flies", 0), Some(2..6));

        let regex = Pattern::parse(r"re:b\w+d", true).unwrap();
        assert!(regex.is_regex());
        assert_eq!(regex.find_from("a bird flies", 0), Some(2..6));

        let insensitive = Pattern::parse("re:i:BIRD", true).unwrap();
        assert_eq!(insensitive.find_from("a bird flies", 0), Some(2..6));
    }

    #[test]
    fn regex_replace_supports_captures() {
        let pattern = Pattern::parse(r"re:(\w+)-(\w+)", true).unwrap();
        let (updated, count) = pattern
            .replace_all("foo-bar and baz-qux", "$2/$1", 64 * 1024)
            .unwrap();
        assert_eq!(count, 2);
        assert_eq!(updated, "bar/foo and qux/baz");
    }

    #[test]
    fn empty_and_oversized_patterns_are_refused() {
        assert!(matches!(Pattern::parse("", true), Err(PatternError::Empty)));
        let huge = "a".repeat(MAX_PATTERN_BYTES + 1);
        assert!(matches!(
            Pattern::parse(&huge, true),
            Err(PatternError::TooLarge { .. })
        ));
    }
}
