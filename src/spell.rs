//! Prose-only spelling helpers. No new crates: a bundled word set plus a
//! small typo table. Optional `spell_argv` stays in config for a host aspell.

use std::collections::HashSet;
use std::sync::OnceLock;

/// Common typos → replacement. Only these are auto-corrected.
pub const TYPOS: &[(&str, &str)] = &[
    ("teh", "the"),
    ("adn", "and"),
    ("nad", "and"),
    ("taht", "that"),
    ("thier", "their"),
    ("recieve", "receive"),
    ("occured", "occurred"),
    ("seperate", "separate"),
    ("definately", "definitely"),
    ("untill", "until"),
    ("wich", "which"),
    ("whcih", "which"),
    ("becuase", "because"),
    ("becasue", "because"),
    ("langauge", "language"),
    ("lenght", "length"),
    ("widht", "width"),
    ("heigth", "height"),
    ("frist", "first"),
    ("jsut", "just"),
    ("tempalte", "template"),
    ("fucntion", "function"),
    ("funtion", "function"),
    ("retrun", "return"),
    ("reutrn", "return"),
    ("improvment", "improvement"),
    ("enviroment", "environment"),
    ("accomodate", "accommodate"),
    ("adress", "address"),
    ("tommorrow", "tomorrow"),
    ("yesterady", "yesterday"),
    ("intergrate", "integrate"),
    ("intergration", "integration"),
    ("independant", "independent"),
    ("occassion", "occasion"),
    ("publically", "publicly"),
    ("refered", "referred"),
    ("succesful", "successful"),
    ("successfull", "successful"),
    ("tahn", "than"),
    ("hte", "the"),
    ("fo", "of"),
    ("ot", "to"),
    ("nto", "not"),
    ("dont", "don't"),
    ("wont", "won't"),
    ("cant", "can't"),
    ("its'", "its"),
    ("simeltaneously", "simultaneously"),
    ("oficially", "officially"),
    ("interperatibility", "interpretability"),
    ("anhytime", "anytime"),
    ("brigher", "brighter"),
];

const COMMON_WORDS: &str = include_str!("spell_words.txt");

fn dictionary() -> &'static HashSet<String> {
    static DICT: OnceLock<HashSet<String>> = OnceLock::new();
    DICT.get_or_init(|| {
        let mut set = HashSet::with_capacity(4096);
        for word in COMMON_WORDS.split_whitespace() {
            set.insert(word.to_ascii_lowercase());
        }
        for (typo, fix) in TYPOS {
            set.insert((*fix).to_ascii_lowercase());
            let _ = typo;
        }
        set
    })
}

pub fn is_checkable_token(token: &str) -> bool {
    let mut letters = 0usize;
    for ch in token.chars() {
        if ch == '\'' {
            continue;
        }
        if !ch.is_ascii_alphabetic() {
            return false;
        }
        letters += 1;
    }
    letters >= 2 && !token.chars().any(|ch| ch == '_')
}

pub fn is_known_word(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    dictionary().contains(&lower)
}

pub fn correction_for(token: &str) -> Option<&'static str> {
    let lower = token.to_ascii_lowercase();
    TYPOS
        .iter()
        .find(|(typo, _)| *typo == lower)
        .map(|(_, fix)| *fix)
}

/// Line-local character ranges of unknown checkable tokens.
pub fn misspelled_ranges(line: &str) -> Vec<std::ops::Range<usize>> {
    let chars: Vec<char> = line.chars().collect();
    let mut ranges = Vec::new();
    let mut index = 0usize;
    while index < chars.len() {
        if is_word_char(chars[index]) {
            let start = index;
            index += 1;
            while index < chars.len() && is_word_char(chars[index]) {
                index += 1;
            }
            let token: String = chars[start..index].iter().collect();
            if is_checkable_token(&token) && !is_known_word(&token) {
                ranges.push(start..index);
            }
        } else {
            index += 1;
        }
    }
    ranges
}

/// Word immediately before `cursor` (Unicode scalar index) in `text`.
pub fn word_before(text: &str, cursor: usize) -> Option<(std::ops::Range<usize>, String)> {
    let chars: Vec<char> = text.chars().collect();
    if cursor == 0 || cursor > chars.len() {
        return None;
    }
    let mut end = cursor;
    if end > 0 && !is_word_char(chars[end - 1]) {
        end -= 1;
    }
    if end == 0 || !is_word_char(chars[end - 1]) {
        return None;
    }
    let mut start = end;
    while start > 0 && is_word_char(chars[start - 1]) {
        start -= 1;
    }
    let token: String = chars[start..end].iter().collect();
    Some((start..end, token))
}

fn is_word_char(ch: char) -> bool {
    ch.is_ascii_alphabetic() || ch == '\''
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_unknown_words_and_skips_known() {
        let ranges = misspelled_ranges("the qzzx fox");
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0], 4..8);
    }

    #[test]
    fn corrects_common_typos() {
        assert_eq!(correction_for("teh"), Some("the"));
        assert_eq!(correction_for("TEH"), Some("the"));
        assert_eq!(correction_for("hello"), None);
    }

    #[test]
    fn skips_identifiers_and_digits() {
        assert!(!is_checkable_token("snake_case"));
        assert!(!is_checkable_token("v2"));
        assert!(is_checkable_token("hello"));
    }
}
