//! Typed, conservative adapters from raw LSP JSON into editor-facing values.
//!
//! The live client deliberately retains feature payloads as lossless JSON so
//! it can remain protocol-complete without a large public type graph. This
//! module is the UI boundary for diagnostics, navigation, completion, and
//! single-document formatting. It exposes no workspace-edit interface and
//! converts UTF-16 ranges against an immutable snapshot before an edit applies.

use std::path::PathBuf;

use ropey::Rope;

use crate::lsp::{CharOffset, DocumentSnapshot, DocumentVersion, LspPosition};
#[cfg(test)]
use crate::lsp_client::file_uri;
use crate::lsp_client::{JsonValue, LspRange};

const MAX_DOCUMENT_EDITS: usize = 16_384;

pub const MAX_DIAGNOSTICS_INSPECTED: usize = 4_096;
pub const MAX_DIAGNOSTICS_RETAINED: usize = 1_024;
pub const MAX_DIAGNOSTIC_URI_BYTES: usize = 16 * 1_024;
pub const MAX_DIAGNOSTIC_MESSAGE_BYTES: usize = 4 * 1_024;
pub const MAX_DIAGNOSTIC_SOURCE_BYTES: usize = 256;
pub const MAX_DIAGNOSTIC_RAW_BYTES: usize = 16 * 1_024;
pub const MAX_DIAGNOSTIC_RAW_NODES: usize = 4_096;
pub const MAX_DIAGNOSTIC_RAW_DEPTH: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DiagnosticSeverity {
    Error,
    Warning,
    Information,
    Hint,
    Unknown(i64),
}

impl DiagnosticSeverity {
    pub const fn marker(self) -> char {
        match self {
            Self::Error => 'E',
            Self::Warning => 'W',
            Self::Information => 'I',
            Self::Hint => 'H',
            Self::Unknown(_) => '?',
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Diagnostic {
    pub uri: String,
    pub range: LspRange,
    pub severity: DiagnosticSeverity,
    pub message: String,
    pub source: Option<String>,
    pub raw: JsonValue,
}

/// Bounded outcome of adapting one `publishDiagnostics` payload for the UI.
///
/// `truncated` covers either input entries beyond the inspection limit or
/// valid diagnostics beyond the retention limit. `fields_truncated` counts
/// individual retained message/source fields that were clipped, while
/// `raw_omitted` counts retained diagnostics whose original JSON exceeded the
/// raw byte, node, or depth ceiling and was replaced with `JsonValue::Null`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DiagnosticReport {
    pub diagnostics: Vec<Diagnostic>,
    pub inspected: usize,
    pub truncated: bool,
    pub skipped_invalid: usize,
    pub fields_truncated: usize,
    pub raw_omitted: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TextEdit {
    pub range: LspRange,
    pub new_text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionItem {
    pub label: String,
    pub detail: Option<String>,
    pub insert_text: String,
    pub text_edit: Option<TextEdit>,
    pub is_snippet: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Location {
    pub uri: String,
    pub range: LspRange,
}

/// Maximum number of server-provided workspace-symbol candidates inspected
/// for one request. Bounding inspected candidates as well as retained symbols
/// keeps a malformed response from turning picker construction into unbounded
/// work.
pub const MAX_WORKSPACE_SYMBOLS: usize = 4_096;

/// Maximum UTF-8 size retained for either a workspace symbol's name or its
/// optional container name.
pub const MAX_WORKSPACE_SYMBOL_FIELD_BYTES: usize = 512;

/// File URIs cannot be clipped without changing their target, so implausibly
/// large values are rejected instead. The allowance covers a maximally
/// percent-encoded path several times longer than common filesystem limits.
pub const MAX_WORKSPACE_SYMBOL_URI_BYTES: usize = 16 * 1_024;

/// LSP's `uinteger` wire type is limited to the non-negative signed 32-bit
/// range, even on clients whose native index type is wider.
const MAX_LSP_UINTEGER: u64 = i32::MAX as u64;

/// Standard LSP `SymbolKind` values, with an explicit forward-compatible
/// fallback for values introduced by a newer or non-conforming server.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceSymbolKind {
    File,
    Module,
    Namespace,
    Package,
    Class,
    Method,
    Property,
    Field,
    Constructor,
    Enum,
    Interface,
    Function,
    Variable,
    Constant,
    String,
    Number,
    Boolean,
    Array,
    Object,
    Key,
    Null,
    EnumMember,
    Struct,
    Event,
    Operator,
    TypeParameter,
    Unknown(i64),
}

impl WorkspaceSymbolKind {
    /// A short, control-free label suitable for the symbol picker.
    pub const fn label(self) -> &'static str {
        match self {
            Self::File => "File",
            Self::Module => "Module",
            Self::Namespace => "Namespace",
            Self::Package => "Package",
            Self::Class => "Class",
            Self::Method => "Method",
            Self::Property => "Property",
            Self::Field => "Field",
            Self::Constructor => "Constructor",
            Self::Enum => "Enum",
            Self::Interface => "Interface",
            Self::Function => "Function",
            Self::Variable => "Variable",
            Self::Constant => "Constant",
            Self::String => "String",
            Self::Number => "Number",
            Self::Boolean => "Boolean",
            Self::Array => "Array",
            Self::Object => "Object",
            Self::Key => "Key",
            Self::Null => "Null",
            Self::EnumMember => "Enum member",
            Self::Struct => "Struct",
            Self::Event => "Event",
            Self::Operator => "Operator",
            Self::TypeParameter => "Type parameter",
            Self::Unknown(_) => "Unknown",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceSymbol {
    pub name: String,
    pub container_name: Option<String>,
    pub kind: WorkspaceSymbolKind,
    pub location: Location,
}

/// Outcome of parsing one `workspace/symbol` response.
///
/// `truncated` is true when candidates beyond the inspection cap were ignored
/// or when a retained name/container had to be clipped. `skipped_invalid`
/// counts malformed candidates among the bounded set that was inspected.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkspaceSymbolReport {
    pub symbols: Vec<WorkspaceSymbol>,
    pub truncated: bool,
    pub skipped_invalid: usize,
}

pub fn parse_diagnostics(uri: &str, values: &[JsonValue]) -> DiagnosticReport {
    let inspected = values.len().min(MAX_DIAGNOSTICS_INSPECTED);
    let mut report = DiagnosticReport {
        inspected,
        truncated: values.len() > MAX_DIAGNOSTICS_INSPECTED,
        ..DiagnosticReport::default()
    };
    if !diagnostic_uri_is_valid(uri) {
        report.skipped_invalid = inspected;
        return report;
    }
    report
        .diagnostics
        .reserve(inspected.min(MAX_DIAGNOSTICS_RETAINED));

    for value in values.iter().take(MAX_DIAGNOSTICS_INSPECTED) {
        let Some(parsed) = parse_diagnostic_fields(value) else {
            report.skipped_invalid += 1;
            continue;
        };
        if report.diagnostics.len() >= MAX_DIAGNOSTICS_RETAINED {
            report.truncated = true;
            continue;
        }

        let (message, message_truncated) =
            clip_utf8_field(parsed.message, MAX_DIAGNOSTIC_MESSAGE_BYTES);
        report.fields_truncated += usize::from(message_truncated);
        let source = parsed.source.map(|source| {
            let (source, source_truncated) = clip_utf8_field(source, MAX_DIAGNOSTIC_SOURCE_BYTES);
            report.fields_truncated += usize::from(source_truncated);
            source.to_owned()
        });
        let raw = if diagnostic_raw_within_bounds(value) {
            value.clone()
        } else {
            report.raw_omitted += 1;
            JsonValue::Null
        };
        report.diagnostics.push(Diagnostic {
            uri: uri.to_owned(),
            range: parsed.range,
            severity: parsed.severity,
            message: message.to_owned(),
            source,
            raw,
        });
    }
    report
}

#[derive(Clone, Copy)]
struct ParsedDiagnosticFields<'a> {
    range: LspRange,
    severity: DiagnosticSeverity,
    message: &'a str,
    source: Option<&'a str>,
}

fn parse_diagnostic_fields(value: &JsonValue) -> Option<ParsedDiagnosticFields<'_>> {
    let range = parse_range(value.get("range")?)?;
    if (range.start.line, range.start.character) > (range.end.line, range.end.character) {
        return None;
    }
    let message = value.get("message")?.as_str()?;
    let severity = match value.get("severity").and_then(JsonValue::as_i64) {
        Some(1) => DiagnosticSeverity::Error,
        Some(2) => DiagnosticSeverity::Warning,
        Some(3) => DiagnosticSeverity::Information,
        Some(4) => DiagnosticSeverity::Hint,
        Some(other) => DiagnosticSeverity::Unknown(other),
        None => DiagnosticSeverity::Information,
    };
    let source = value.get("source").and_then(JsonValue::as_str);
    Some(ParsedDiagnosticFields {
        range,
        severity,
        message,
        source,
    })
}

fn diagnostic_uri_is_valid(uri: &str) -> bool {
    if uri.is_empty() || uri.len() > MAX_DIAGNOSTIC_URI_BYTES || uri.chars().any(char::is_control) {
        return false;
    }
    file_uri_to_path(uri).is_ok()
}

fn clip_utf8_field(value: &str, max_bytes: usize) -> (&str, bool) {
    if value.len() <= max_bytes {
        return (value, false);
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (&value[..end], true)
}

/// Checks exact compact-JSON byte size plus structural complexity without
/// recursively walking or allocating a serialized copy. The explicit stack is
/// itself bounded by `MAX_DIAGNOSTIC_RAW_NODES`.
fn diagnostic_raw_within_bounds(root: &JsonValue) -> bool {
    let mut bytes = 0_usize;
    let mut scheduled_nodes = 1_usize;
    let mut stack = vec![(root, 1_usize)];
    while let Some((value, depth)) = stack.pop() {
        if depth > MAX_DIAGNOSTIC_RAW_DEPTH {
            return false;
        }
        match value {
            JsonValue::Null => {
                if !add_raw_bytes(&mut bytes, 4) {
                    return false;
                }
            }
            JsonValue::Bool(value) => {
                if !add_raw_bytes(&mut bytes, if *value { 4 } else { 5 }) {
                    return false;
                }
            }
            JsonValue::Number(number) => {
                if !add_raw_bytes(&mut bytes, number.as_str().len()) {
                    return false;
                }
            }
            JsonValue::String(value) => {
                let Some(encoded) = bounded_json_string_len(value) else {
                    return false;
                };
                if !add_raw_bytes(&mut bytes, encoded) {
                    return false;
                }
            }
            JsonValue::Array(values) => {
                let Some(structural_bytes) = 2_usize.checked_add(values.len().saturating_sub(1))
                else {
                    return false;
                };
                if !add_raw_bytes(&mut bytes, structural_bytes)
                    || !schedule_raw_children(&mut scheduled_nodes, values.len())
                    || (!values.is_empty() && depth >= MAX_DIAGNOSTIC_RAW_DEPTH)
                {
                    return false;
                }
                stack.extend(values.iter().rev().map(|value| (value, depth + 1)));
            }
            JsonValue::Object(object) => {
                let Some(structural_bytes) = 2_usize
                    .checked_add(object.len().saturating_sub(1))
                    .and_then(|bytes| bytes.checked_add(object.len()))
                else {
                    return false;
                };
                if !add_raw_bytes(&mut bytes, structural_bytes)
                    || !schedule_raw_children(&mut scheduled_nodes, object.len())
                    || (!object.is_empty() && depth >= MAX_DIAGNOSTIC_RAW_DEPTH)
                {
                    return false;
                }
                for key in object.keys() {
                    let Some(encoded) = bounded_json_string_len(key) else {
                        return false;
                    };
                    if !add_raw_bytes(&mut bytes, encoded) {
                        return false;
                    }
                }
                stack.extend(object.values().rev().map(|value| (value, depth + 1)));
            }
        }
    }
    true
}

fn add_raw_bytes(total: &mut usize, amount: usize) -> bool {
    match total.checked_add(amount) {
        Some(updated) if updated <= MAX_DIAGNOSTIC_RAW_BYTES => {
            *total = updated;
            true
        }
        _ => false,
    }
}

fn schedule_raw_children(total: &mut usize, children: usize) -> bool {
    match total.checked_add(children) {
        Some(updated) if updated <= MAX_DIAGNOSTIC_RAW_NODES => {
            *total = updated;
            true
        }
        _ => false,
    }
}

fn bounded_json_string_len(value: &str) -> Option<usize> {
    let mut bytes = 2_usize;
    for character in value.chars() {
        let encoded = match character {
            '"' | '\\' | '\u{08}' | '\u{0c}' | '\n' | '\r' | '\t' => 2,
            character if character <= '\u{1f}' => 6,
            character => character.len_utf8(),
        };
        bytes = bytes.checked_add(encoded)?;
        if bytes > MAX_DIAGNOSTIC_RAW_BYTES {
            return None;
        }
    }
    Some(bytes)
}

pub fn parse_completion(result: &JsonValue) -> Vec<CompletionItem> {
    let values = match result {
        JsonValue::Array(values) => values.as_slice(),
        JsonValue::Object(_) => match result.get("items") {
            Some(JsonValue::Array(values)) => values.as_slice(),
            _ => return Vec::new(),
        },
        JsonValue::Null => return Vec::new(),
        _ => return Vec::new(),
    };
    values
        .iter()
        .take(1_024)
        .filter_map(parse_completion_item)
        .collect()
}

fn parse_completion_item(value: &JsonValue) -> Option<CompletionItem> {
    let label = value.get("label")?.as_str()?.to_owned();
    let detail = value
        .get("detail")
        .and_then(JsonValue::as_str)
        .map(str::to_owned);
    let text_edit = match value.get("textEdit") {
        None => None,
        Some(value) => Some(parse_completion_text_edit(value)?),
    };
    let explicit_insert_text = match value.get("insertText") {
        None => None,
        Some(value) => Some(value.as_str()?),
    };
    let insert_text = explicit_insert_text
        .or_else(|| text_edit.as_ref().map(|edit| edit.new_text.as_str()))
        .unwrap_or(&label)
        .to_owned();
    let is_snippet = match value.get("insertTextFormat") {
        None => false,
        Some(value) => match value.as_i64()? {
            1 => false,
            2 => true,
            _ => return None,
        },
    };
    Some(CompletionItem {
        label,
        detail,
        insert_text,
        text_edit,
        is_snippet,
    })
}

fn parse_completion_text_edit(value: &JsonValue) -> Option<TextEdit> {
    let new_text = value.get("newText")?.as_str()?.to_owned();
    let range = match (
        value.get("range"),
        value.get("insert"),
        value.get("replace"),
    ) {
        (Some(range), None, None) => parse_range(range)?,
        (None, Some(insert), Some(replace)) => {
            // The editor applies the replacement range, but both required
            // InsertReplaceEdit ranges and their prefix relationship must be
            // valid before the completion item is allowed into the picker.
            let insert = parse_range(insert)?;
            let replace = parse_range(replace)?;
            if insert.start != replace.start
                || !position_is_at_or_before(insert.start, insert.end)
                || !position_is_at_or_before(insert.end, replace.end)
            {
                return None;
            }
            replace
        }
        _ => return None,
    };
    Some(TextEdit { range, new_text })
}

fn position_is_at_or_before(left: LspPosition, right: LspPosition) -> bool {
    left.line < right.line || (left.line == right.line && left.character <= right.character)
}

pub fn parse_locations(result: &JsonValue) -> Vec<Location> {
    match result {
        JsonValue::Null => Vec::new(),
        JsonValue::Array(values) => values
            .iter()
            .take(4_096)
            .filter_map(parse_location)
            .collect(),
        JsonValue::Object(_) => parse_location(result).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Parse the two range-bearing result shapes allowed by LSP 3.17's
/// `workspace/symbol` request: `SymbolInformation[]` and `WorkspaceSymbol[]`.
///
/// Range-less `WorkspaceSymbolLocation` values are intentionally skipped. The
/// client does not advertise `resolveSupport`, so it cannot safely turn those
/// placeholders into navigable locations after the picker is shown.
pub fn parse_workspace_symbols(result: &JsonValue) -> WorkspaceSymbolReport {
    let values = match result {
        JsonValue::Null => return WorkspaceSymbolReport::default(),
        JsonValue::Array(values) => values,
        _ => {
            return WorkspaceSymbolReport {
                skipped_invalid: 1,
                ..WorkspaceSymbolReport::default()
            };
        }
    };

    let mut report = WorkspaceSymbolReport {
        truncated: values.len() > MAX_WORKSPACE_SYMBOLS,
        ..WorkspaceSymbolReport::default()
    };
    report
        .symbols
        .reserve(values.len().min(MAX_WORKSPACE_SYMBOLS));

    for value in values.iter().take(MAX_WORKSPACE_SYMBOLS) {
        match parse_workspace_symbol(value) {
            Some((symbol, fields_truncated)) => {
                report.truncated |= fields_truncated;
                report.symbols.push(symbol);
            }
            None => report.skipped_invalid += 1,
        }
    }
    report
}

fn parse_workspace_symbol(value: &JsonValue) -> Option<(WorkspaceSymbol, bool)> {
    let kind = parse_workspace_symbol_kind(value.get("kind")?.as_i64()?);
    let location = parse_workspace_symbol_location(value.get("location")?)?;

    let raw_name = value.get("name")?.as_str()?;
    if raw_name.is_empty() {
        return None;
    }
    let (name, name_truncated) = bounded_symbol_field(raw_name)?;

    let (container_name, container_truncated) = match value.get("containerName") {
        None => (None, false),
        Some(container) => {
            let (container, truncated) = bounded_symbol_field(container.as_str()?)?;
            ((!container.is_empty()).then_some(container), truncated)
        }
    };

    Some((
        WorkspaceSymbol {
            name,
            container_name,
            kind,
            location,
        },
        name_truncated || container_truncated,
    ))
}

fn parse_workspace_symbol_location(value: &JsonValue) -> Option<Location> {
    let uri = value.get("uri")?.as_str()?;
    if uri.len() > MAX_WORKSPACE_SYMBOL_URI_BYTES || uri.chars().any(char::is_control) {
        return None;
    }
    file_uri_to_path(uri).ok()?;

    let range = parse_range(value.get("range")?)?;
    let start = (range.start.line.get(), range.start.character.get());
    let end = (range.end.line.get(), range.end.character.get());
    if start > end {
        return None;
    }
    Some(Location {
        uri: uri.to_owned(),
        range,
    })
}

fn bounded_symbol_field(value: &str) -> Option<(String, bool)> {
    let end = if value.len() <= MAX_WORKSPACE_SYMBOL_FIELD_BYTES {
        value.len()
    } else {
        let mut end = MAX_WORKSPACE_SYMBOL_FIELD_BYTES;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        end
    };
    let retained = &value[..end];
    if retained.chars().any(char::is_control) {
        return None;
    }
    Some((retained.to_owned(), end != value.len()))
}

const fn parse_workspace_symbol_kind(value: i64) -> WorkspaceSymbolKind {
    match value {
        1 => WorkspaceSymbolKind::File,
        2 => WorkspaceSymbolKind::Module,
        3 => WorkspaceSymbolKind::Namespace,
        4 => WorkspaceSymbolKind::Package,
        5 => WorkspaceSymbolKind::Class,
        6 => WorkspaceSymbolKind::Method,
        7 => WorkspaceSymbolKind::Property,
        8 => WorkspaceSymbolKind::Field,
        9 => WorkspaceSymbolKind::Constructor,
        10 => WorkspaceSymbolKind::Enum,
        11 => WorkspaceSymbolKind::Interface,
        12 => WorkspaceSymbolKind::Function,
        13 => WorkspaceSymbolKind::Variable,
        14 => WorkspaceSymbolKind::Constant,
        15 => WorkspaceSymbolKind::String,
        16 => WorkspaceSymbolKind::Number,
        17 => WorkspaceSymbolKind::Boolean,
        18 => WorkspaceSymbolKind::Array,
        19 => WorkspaceSymbolKind::Object,
        20 => WorkspaceSymbolKind::Key,
        21 => WorkspaceSymbolKind::Null,
        22 => WorkspaceSymbolKind::EnumMember,
        23 => WorkspaceSymbolKind::Struct,
        24 => WorkspaceSymbolKind::Event,
        25 => WorkspaceSymbolKind::Operator,
        26 => WorkspaceSymbolKind::TypeParameter,
        unknown => WorkspaceSymbolKind::Unknown(unknown),
    }
}

pub fn parse_document_symbols(result: &JsonValue, document_uri: &str) -> Vec<(String, Location)> {
    let JsonValue::Array(values) = result else {
        return Vec::new();
    };
    let mut output = Vec::new();
    collect_document_symbols(values, document_uri, 0, &mut output);
    output
}

fn collect_document_symbols(
    values: &[JsonValue],
    document_uri: &str,
    depth: usize,
    output: &mut Vec<(String, Location)>,
) {
    if depth > 32 || output.len() >= 4_096 {
        return;
    }
    for value in values.iter().take(4_096 - output.len()) {
        let Some(name) = value.get("name").and_then(JsonValue::as_str) else {
            continue;
        };
        let location = if let Some(location) = value.get("location") {
            parse_location(location)
        } else {
            value
                .get("selectionRange")
                .or_else(|| value.get("range"))
                .and_then(parse_range)
                .map(|range| Location {
                    uri: document_uri.to_owned(),
                    range,
                })
        };
        if let Some(location) = location {
            output.push((format!("{}{}", "  ".repeat(depth), name), location));
        }
        if let Some(JsonValue::Array(children)) = value.get("children") {
            collect_document_symbols(children, document_uri, depth + 1, output);
        }
        if output.len() >= 4_096 {
            break;
        }
    }
}

fn parse_location(value: &JsonValue) -> Option<Location> {
    if let Some(uri) = value.get("uri").and_then(JsonValue::as_str) {
        return Some(Location {
            uri: uri.to_owned(),
            range: parse_range(value.get("range")?)?,
        });
    }
    Some(Location {
        uri: value.get("targetUri")?.as_str()?.to_owned(),
        range: parse_range(
            value
                .get("targetSelectionRange")
                .or_else(|| value.get("targetRange"))?,
        )?,
    })
}

pub fn render_hover(result: &JsonValue) -> Option<String> {
    if matches!(result, JsonValue::Null) {
        return None;
    }
    let contents = result.get("contents").unwrap_or(result);
    let mut lines = Vec::new();
    collect_hover_contents(contents, &mut lines);
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n\n"))
    }
}

fn collect_hover_contents(value: &JsonValue, lines: &mut Vec<String>) {
    match value {
        JsonValue::String(text) => lines.push(text.clone()),
        JsonValue::Array(values) => {
            for value in values {
                collect_hover_contents(value, lines);
            }
        }
        JsonValue::Object(_) => {
            if let Some(text) = value.get("value").and_then(JsonValue::as_str) {
                if let Some(language) = value.get("language").and_then(JsonValue::as_str) {
                    lines.push(format!("```{language}\n{text}\n```"));
                } else {
                    lines.push(text.to_owned());
                }
            }
        }
        JsonValue::Null | JsonValue::Bool(_) | JsonValue::Number(_) => {}
    }
}

fn parse_text_edit_array(values: &[JsonValue]) -> Option<Vec<TextEdit>> {
    if values.len() > MAX_DOCUMENT_EDITS {
        return None;
    }
    values
        .iter()
        .map(|value| {
            Some(TextEdit {
                range: parse_range(value.get("range")?)?,
                new_text: value.get("newText")?.as_str()?.to_owned(),
            })
        })
        .collect()
}

pub fn parse_text_edits(result: &JsonValue) -> Option<Vec<TextEdit>> {
    match result {
        JsonValue::Null => Some(Vec::new()),
        JsonValue::Array(values) => parse_text_edit_array(values),
        _ => None,
    }
}

pub fn parse_range(value: &JsonValue) -> Option<LspRange> {
    Some(LspRange::new(
        parse_position(value.get("start")?)?,
        parse_position(value.get("end")?)?,
    ))
}

fn parse_position(value: &JsonValue) -> Option<LspPosition> {
    let line = value.get("line")?.as_u64()?;
    let character = value.get("character")?.as_u64()?;
    if line > MAX_LSP_UINTEGER || character > MAX_LSP_UINTEGER {
        return None;
    }
    let line = usize::try_from(line).ok()?;
    let character = usize::try_from(character).ok()?;
    Some(LspPosition::new(line.into(), character.into()))
}

pub fn range_to_chars(text: &str, range: LspRange) -> Result<std::ops::Range<usize>, String> {
    let snapshot = DocumentSnapshot::from_text(text, DocumentVersion::INITIAL);
    let start = snapshot
        .position_to_char(range.start)
        .map_err(|error| error.to_string())?
        .get();
    let end = snapshot
        .position_to_char(range.end)
        .map_err(|error| error.to_string())?
        .get();
    if start > end {
        return Err("LSP edit range ends before it starts".to_owned());
    }
    Ok(start..end)
}

/// Apply an LSP edit batch to one immutable source version. Edits are checked
/// for overlap and then applied from the end, preserving all earlier offsets.
pub fn apply_text_edits(text: &str, edits: &[TextEdit]) -> Result<String, String> {
    let snapshot = DocumentSnapshot::from_text(text, DocumentVersion::INITIAL);
    let mut converted = Vec::with_capacity(edits.len());
    for edit in edits {
        let start = snapshot
            .position_to_char(edit.range.start)
            .map_err(|error| error.to_string())?
            .get();
        let end = snapshot
            .position_to_char(edit.range.end)
            .map_err(|error| error.to_string())?
            .get();
        if start > end {
            return Err("LSP edit range ends before it starts".to_owned());
        }
        converted.push((start, end, edit.new_text.as_str()));
    }
    converted.sort_by_key(|(start, end, _)| (*start, *end));
    for pair in converted.windows(2) {
        let (left_start, left_end, _) = pair[0];
        let (right_start, _, _) = pair[1];
        if left_end > right_start || (left_start == left_end && left_start == right_start) {
            return Err("LSP edit batch contains overlapping ranges".to_owned());
        }
    }

    let mut rope = Rope::from_str(text);
    for (start, end, replacement) in converted.into_iter().rev() {
        rope.remove(start..end);
        rope.insert(start, replacement);
    }
    Ok(rope.to_string())
}

pub fn file_uri_to_path(uri: &str) -> Result<PathBuf, String> {
    const FILE_SCHEME_PREFIX: &str = "file://";
    let Some(prefix) = uri.get(..FILE_SCHEME_PREFIX.len()) else {
        return Err("LSP location is not a file URI".to_owned());
    };
    if !prefix.eq_ignore_ascii_case(FILE_SCHEME_PREFIX) {
        return Err("LSP location is not a file URI".to_owned());
    }
    let encoded = &uri[FILE_SCHEME_PREFIX.len()..];
    if encoded.bytes().any(|byte| matches!(byte, b'?' | b'#')) {
        return Err("file URI queries and fragments are not supported".to_owned());
    }
    let encoded = if encoded.starts_with('/') {
        encoded.to_owned()
    } else {
        let (host, path) = encoded
            .split_once('/')
            .ok_or_else(|| "remote-host file URIs are not supported".to_owned())?;
        if !host.eq_ignore_ascii_case("localhost") {
            return Err("remote-host file URIs are not supported".to_owned());
        }
        format!("/{path}")
    };
    let mut bytes = Vec::with_capacity(encoded.len());
    let source = encoded.as_bytes();
    let mut index = 0;
    while index < source.len() {
        if source[index] == b'%' {
            let high = *source
                .get(index + 1)
                .ok_or_else(|| "truncated percent escape in file URI".to_owned())?;
            let low = *source
                .get(index + 2)
                .ok_or_else(|| "truncated percent escape in file URI".to_owned())?;
            bytes.push(
                hex(high)
                    .and_then(|high| hex(low).map(|low| high << 4 | low))
                    .ok_or_else(|| "invalid percent escape in file URI".to_owned())?,
            );
            index += 3;
        } else {
            bytes.push(source[index]);
            index += 1;
        }
    }
    if bytes.contains(&0) {
        return Err("file URI contains a NUL byte".to_owned());
    }
    if decoded_path_contains_control(&bytes) {
        return Err("file URI path contains a control character".to_owned());
    }
    decoded_file_uri_path(bytes)
}

/// Reject Unicode control characters in every valid UTF-8 run while leaving
/// otherwise-invalid bytes available as pathname identity on Unix. ASCII
/// controls are checked separately because they can occur inside an invalid
/// UTF-8 sequence and must never be admitted.
fn decoded_path_contains_control(mut bytes: &[u8]) -> bool {
    if bytes.iter().any(|byte| byte.is_ascii_control()) {
        return true;
    }

    while !bytes.is_empty() {
        match std::str::from_utf8(bytes) {
            Ok(text) => return text.chars().any(char::is_control),
            Err(error) => {
                let valid_up_to = error.valid_up_to();
                if valid_up_to > 0
                    && std::str::from_utf8(&bytes[..valid_up_to])
                        .is_ok_and(|text| text.chars().any(char::is_control))
                {
                    return true;
                }
                let invalid_bytes = error
                    .error_len()
                    .unwrap_or_else(|| bytes.len().saturating_sub(valid_up_to));
                bytes = &bytes[valid_up_to.saturating_add(invalid_bytes)..];
            }
        }
    }
    false
}

#[cfg(unix)]
fn decoded_file_uri_path(bytes: Vec<u8>) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStringExt as _;

    Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

#[cfg(not(unix))]
fn decoded_file_uri_path(bytes: Vec<u8>) -> Result<PathBuf, String> {
    let path = String::from_utf8(bytes).map_err(|_| "file URI path is not UTF-8".to_owned())?;
    Ok(PathBuf::from(path))
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

pub fn diagnostic_char_range(
    text: &str,
    diagnostic: &Diagnostic,
) -> Option<std::ops::Range<usize>> {
    range_to_chars(text, diagnostic.range).ok()
}

pub fn cursor_position(text: &str, cursor: usize) -> Result<LspPosition, String> {
    cursor_position_in_snapshot(
        &DocumentSnapshot::from_text(text, DocumentVersion::INITIAL),
        cursor,
    )
}

pub fn cursor_position_in_snapshot(
    snapshot: &DocumentSnapshot,
    cursor: usize,
) -> Result<LspPosition, String> {
    snapshot
        .char_to_position(CharOffset::new(cursor))
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(source: &str) -> JsonValue {
        JsonValue::parse(source).unwrap()
    }

    fn diagnostic_value(
        message: impl Into<String>,
        source: Option<String>,
        data: Option<JsonValue>,
    ) -> JsonValue {
        let JsonValue::Object(mut diagnostic) = json(
            r#"{
                "range":{"start":{"line":0,"character":1},"end":{"line":0,"character":3}},
                "severity":1,
                "message":"placeholder"
            }"#,
        ) else {
            unreachable!()
        };
        diagnostic.insert("message".to_owned(), JsonValue::String(message.into()));
        if let Some(source) = source {
            diagnostic.insert("source".to_owned(), JsonValue::String(source));
        }
        if let Some(data) = data {
            diagnostic.insert("data".to_owned(), data);
        }
        JsonValue::Object(diagnostic)
    }

    #[cfg(unix)]
    #[test]
    fn file_uri_round_trips_distinct_non_utf8_unix_basenames() {
        use std::os::unix::ffi::OsStringExt as _;

        use crate::lsp_client::file_uri;

        let first = PathBuf::from(std::ffi::OsString::from_vec(
            b"/tmp/non-utf8-\x80.rs".to_vec(),
        ));
        let second = PathBuf::from(std::ffi::OsString::from_vec(
            b"/tmp/non-utf8-\x81.rs".to_vec(),
        ));
        let first_uri = file_uri(&first);
        let second_uri = file_uri(&second);

        assert_ne!(first, second);
        assert_ne!(first_uri, second_uri);
        assert!(first_uri.ends_with("non-utf8-%80.rs"));
        assert!(second_uri.ends_with("non-utf8-%81.rs"));
        assert_eq!(file_uri_to_path(&first_uri).unwrap(), first);
        assert_eq!(file_uri_to_path(&second_uri).unwrap(), second);
    }

    #[test]
    fn file_uri_decoder_rejects_remote_hosts_bad_escapes_nul_and_controls() {
        for invalid_uri in [
            "https://example.test/a.rs",
            "file://remote/tmp/a.rs",
            "file:///tmp/truncated%",
            "file:///tmp/truncated%0",
            "file:///tmp/invalid%QQ.rs",
            "file:///tmp/raw\0.rs",
            "file:///tmp/encoded%00.rs",
            "file:///tmp/raw\u{1b}.rs",
            "file:///tmp/encoded%1B.rs",
            "file:///tmp/encoded%7F.rs",
            "file:///tmp/unicode%C2%85.rs",
        ] {
            assert!(file_uri_to_path(invalid_uri).is_err(), "{invalid_uri:?}");
        }
    }

    #[test]
    fn file_uri_decoder_rejects_raw_query_and_fragment_but_preserves_encoded_bytes() {
        for invalid_uri in [
            "file:///tmp/name.rs?query",
            "file:///tmp/name.rs#fragment",
            "file://LOCALHOST/tmp/name?query#fragment",
        ] {
            assert!(file_uri_to_path(invalid_uri).is_err(), "{invalid_uri:?}");
        }

        assert_eq!(
            file_uri_to_path("file:///tmp/name%3Fquery%23fragment.rs").unwrap(),
            PathBuf::from("/tmp/name?query#fragment.rs")
        );
        assert_eq!(
            file_uri_to_path("file://LOCALHOST/tmp/name%3fquery%23fragment.rs").unwrap(),
            PathBuf::from("/tmp/name?query#fragment.rs")
        );
    }

    #[test]
    fn file_uri_decoder_round_trips_ascii_case_insensitive_schemes() {
        for uri in [
            "FILE:///tmp/mixed%20scheme.rs",
            "FiLe://LOCALHOST/tmp/mixed%20scheme.rs",
            "fIlE://localhost/tmp/mixed%20scheme.rs",
        ] {
            let path = file_uri_to_path(uri).unwrap();
            assert_eq!(path, PathBuf::from("/tmp/mixed scheme.rs"));
            assert_eq!(file_uri(path), "file:///tmp/mixed%20scheme.rs");
        }

        for invalid_uri in [
            "HTTPS:///tmp/a.rs",
            "FILE://remote/tmp/a.rs",
            "FiLe:///tmp/a.rs?query",
            "fIlE:///tmp/a.rs#fragment",
            "FILE:///tmp/a%1B.rs",
        ] {
            assert!(file_uri_to_path(invalid_uri).is_err(), "{invalid_uri:?}");
        }
    }

    #[test]
    fn parses_diagnostics_and_unicode_ranges() {
        let values = match json(
            r#"[{"range":{"start":{"line":0,"character":1},"end":{"line":0,"character":3}},"severity":1,"source":"rustc","message":"bad"}]"#,
        ) {
            JsonValue::Array(values) => values,
            _ => unreachable!(),
        };
        let report = parse_diagnostics("file:///tmp/a.rs", &values);
        assert_eq!(report.inspected, 1);
        assert!(!report.truncated);
        assert_eq!(report.skipped_invalid, 0);
        assert_eq!(report.fields_truncated, 0);
        assert_eq!(report.raw_omitted, 0);
        assert_eq!(report.diagnostics[0].severity, DiagnosticSeverity::Error);
        assert_eq!(report.diagnostics[0].message, "bad");
        // UTF-16 column 3 is after the two-code-unit emoji.
        assert_eq!(
            diagnostic_char_range("a🪶b", &report.diagnostics[0]),
            Some(1..2)
        );
    }

    #[test]
    fn diagnostics_bound_inspection_retention_and_invalid_counts() {
        let valid = diagnostic_value("valid", Some("test".to_owned()), None);

        let exact = parse_diagnostics(
            "file:///tmp/a.rs",
            &vec![valid.clone(); MAX_DIAGNOSTICS_RETAINED],
        );
        assert_eq!(exact.inspected, MAX_DIAGNOSTICS_RETAINED);
        assert_eq!(exact.diagnostics.len(), MAX_DIAGNOSTICS_RETAINED);
        assert!(!exact.truncated);
        assert_eq!(exact.skipped_invalid, 0);

        let retained_overflow = parse_diagnostics(
            "file:///tmp/a.rs",
            &vec![valid; MAX_DIAGNOSTICS_RETAINED + 1],
        );
        assert_eq!(retained_overflow.inspected, MAX_DIAGNOSTICS_RETAINED + 1);
        assert_eq!(
            retained_overflow.diagnostics.len(),
            MAX_DIAGNOSTICS_RETAINED
        );
        assert!(retained_overflow.truncated);
        assert_eq!(retained_overflow.skipped_invalid, 0);

        let inspection_overflow = parse_diagnostics(
            "file:///tmp/a.rs",
            &vec![JsonValue::Null; MAX_DIAGNOSTICS_INSPECTED + 1],
        );
        assert_eq!(inspection_overflow.inspected, MAX_DIAGNOSTICS_INSPECTED);
        assert!(inspection_overflow.truncated);
        assert_eq!(
            inspection_overflow.skipped_invalid,
            MAX_DIAGNOSTICS_INSPECTED
        );
        assert!(inspection_overflow.diagnostics.is_empty());
    }

    #[test]
    fn diagnostics_reject_malformed_entries_but_preserve_severity_defaults() {
        let malformed = [
            JsonValue::Null,
            json(r#"{}"#),
            json(r#"{"message":"missing range"}"#),
            json(
                r#"{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":0}},"message":4}"#,
            ),
            json(
                r#"{"range":{"start":{"line":2147483648,"character":0},"end":{"line":2147483648,"character":0}},"message":"bad position"}"#,
            ),
        ];
        let mut unknown = diagnostic_value("unknown", None, None);
        let JsonValue::Object(fields) = &mut unknown else {
            unreachable!()
        };
        fields.insert("severity".to_owned(), JsonValue::from(99_i64));
        fields.insert("source".to_owned(), JsonValue::Null);
        let mut defaulted = diagnostic_value("defaulted", None, None);
        let JsonValue::Object(fields) = &mut defaulted else {
            unreachable!()
        };
        fields.remove("severity");

        let mut values = malformed.to_vec();
        values.extend([unknown, defaulted]);
        let report = parse_diagnostics("file:///tmp/a.rs", &values);
        assert_eq!(report.inspected, 7);
        assert_eq!(report.skipped_invalid, 5);
        assert_eq!(report.diagnostics.len(), 2);
        assert_eq!(
            report.diagnostics[0].severity,
            DiagnosticSeverity::Unknown(99)
        );
        assert_eq!(report.diagnostics[0].source, None);
        assert_eq!(
            report.diagnostics[1].severity,
            DiagnosticSeverity::Information
        );
    }

    #[test]
    fn diagnostics_reject_inverted_ranges() {
        let values = [
            json(
                r#"{"range":{"start":{"line":2,"character":4},"end":{"line":2,"character":3}},"message":"same-line inverted"}"#,
            ),
            json(
                r#"{"range":{"start":{"line":3,"character":0},"end":{"line":2,"character":99}},"message":"cross-line inverted"}"#,
            ),
            json(
                r#"{"range":{"start":{"line":2,"character":4},"end":{"line":2,"character":4}},"message":"empty but valid"}"#,
            ),
        ];

        let report = parse_diagnostics("file:///tmp/a.rs", &values);
        assert_eq!(report.inspected, 3);
        assert_eq!(report.skipped_invalid, 2);
        assert_eq!(report.diagnostics.len(), 1);
        assert_eq!(report.diagnostics[0].message, "empty but valid");
    }

    #[test]
    fn diagnostic_publication_uri_is_file_control_and_size_bounded() {
        let value = diagnostic_value("valid", None, None);
        let exact_uri = format!(
            "file:///{}",
            "a".repeat(MAX_DIAGNOSTIC_URI_BYTES - "file:///".len())
        );
        assert_eq!(exact_uri.len(), MAX_DIAGNOSTIC_URI_BYTES);
        assert_eq!(
            parse_diagnostics(&exact_uri, std::slice::from_ref(&value))
                .diagnostics
                .len(),
            1
        );

        for invalid_uri in [
            format!("{exact_uri}x"),
            "https://example.test/a.rs".to_owned(),
            "file://remote/tmp/a.rs".to_owned(),
            "file:///tmp/%QQ.rs".to_owned(),
            "file:///tmp/raw\u{1b}.rs".to_owned(),
            "file:///tmp/encoded%1B.rs".to_owned(),
        ] {
            let report = parse_diagnostics(&invalid_uri, std::slice::from_ref(&value));
            assert_eq!(report.inspected, 1, "{invalid_uri:?}");
            assert_eq!(report.skipped_invalid, 1, "{invalid_uri:?}");
            assert!(report.diagnostics.is_empty(), "{invalid_uri:?}");
        }
    }

    #[test]
    fn diagnostic_fields_clip_at_utf8_boundaries_and_accept_exact_limits() {
        let exact_message = format!("{}🪶", "m".repeat(MAX_DIAGNOSTIC_MESSAGE_BYTES - 4));
        let exact_source = format!("{}é", "s".repeat(MAX_DIAGNOSTIC_SOURCE_BYTES - 2));
        let exact = parse_diagnostics(
            "file:///tmp/a.rs",
            &[diagnostic_value(
                exact_message.clone(),
                Some(exact_source.clone()),
                None,
            )],
        );
        assert_eq!(exact.fields_truncated, 0);
        assert_eq!(exact.diagnostics[0].message, exact_message);
        assert_eq!(
            exact.diagnostics[0].source.as_deref(),
            Some(exact_source.as_str())
        );

        let long_message = format!(
            "{}🪶discarded",
            "m".repeat(MAX_DIAGNOSTIC_MESSAGE_BYTES - 1)
        );
        let long_source = format!("{}édiscarded", "s".repeat(MAX_DIAGNOSTIC_SOURCE_BYTES - 1));
        let clipped = parse_diagnostics(
            "file:///tmp/a.rs",
            &[diagnostic_value(long_message, Some(long_source), None)],
        );
        assert_eq!(clipped.fields_truncated, 2);
        assert_eq!(
            clipped.diagnostics[0].message,
            "m".repeat(MAX_DIAGNOSTIC_MESSAGE_BYTES - 1)
        );
        assert_eq!(
            clipped.diagnostics[0].source.as_deref(),
            Some("s".repeat(MAX_DIAGNOSTIC_SOURCE_BYTES - 1).as_str())
        );
    }

    #[test]
    fn diagnostic_raw_json_is_byte_node_and_depth_bounded_without_serializing() {
        let exact_bytes = JsonValue::String("x".repeat(MAX_DIAGNOSTIC_RAW_BYTES - 2));
        let oversized_bytes = JsonValue::String("x".repeat(MAX_DIAGNOSTIC_RAW_BYTES - 1));
        assert!(diagnostic_raw_within_bounds(&exact_bytes));
        assert!(!diagnostic_raw_within_bounds(&oversized_bytes));
        let exact_escaped = JsonValue::String("\"".repeat((MAX_DIAGNOSTIC_RAW_BYTES - 2) / 2));
        assert!(diagnostic_raw_within_bounds(&exact_escaped));

        let exact_nodes = JsonValue::Array(vec![
            JsonValue::Array(Vec::new());
            MAX_DIAGNOSTIC_RAW_NODES - 1
        ]);
        let excessive_nodes =
            JsonValue::Array(vec![JsonValue::Array(Vec::new()); MAX_DIAGNOSTIC_RAW_NODES]);
        assert!(diagnostic_raw_within_bounds(&exact_nodes));
        assert!(!diagnostic_raw_within_bounds(&excessive_nodes));

        let mut exact_depth = JsonValue::Null;
        for _ in 1..MAX_DIAGNOSTIC_RAW_DEPTH {
            exact_depth = JsonValue::Array(vec![exact_depth]);
        }
        assert!(diagnostic_raw_within_bounds(&exact_depth));
        let excessive_depth = JsonValue::Array(vec![exact_depth]);
        assert!(!diagnostic_raw_within_bounds(&excessive_depth));

        let report = parse_diagnostics(
            "file:///tmp/a.rs",
            &[diagnostic_value(
                "bounded message",
                None,
                Some(JsonValue::String("x".repeat(MAX_DIAGNOSTIC_RAW_BYTES))),
            )],
        );
        assert_eq!(report.raw_omitted, 1);
        assert_eq!(report.diagnostics[0].raw, JsonValue::Null);
    }

    #[test]
    fn applies_multiple_unicode_edits_from_the_end() {
        let edits = vec![
            TextEdit {
                range: parse_range(&json(
                    r#"{"start":{"line":0,"character":1},"end":{"line":0,"character":3}}"#,
                ))
                .unwrap(),
                new_text: "bird".to_owned(),
            },
            TextEdit {
                range: parse_range(&json(
                    r#"{"start":{"line":0,"character":4},"end":{"line":0,"character":5}}"#,
                ))
                .unwrap(),
                new_text: "!".to_owned(),
            },
        ];
        assert_eq!(apply_text_edits("a🪶bc", &edits).unwrap(), "abirdb!");
    }

    #[test]
    fn rejects_overlapping_and_out_of_bounds_edits() {
        let range = parse_range(&json(
            r#"{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}"#,
        ))
        .unwrap();
        let edits = vec![
            TextEdit {
                range,
                new_text: "x".to_owned(),
            },
            TextEdit {
                range,
                new_text: "y".to_owned(),
            },
        ];
        assert!(apply_text_edits("ab", &edits).is_err());
        let out_of_bounds = TextEdit {
            range: parse_range(&json(
                r#"{"start":{"line":99,"character":0},"end":{"line":99,"character":1}}"#,
            ))
            .unwrap(),
            new_text: "x".to_owned(),
        };
        assert!(apply_text_edits("ab", &[out_of_bounds]).is_err());
    }
    #[test]
    fn parses_symbol_information_and_range_bearing_workspace_symbols() {
        let report = parse_workspace_symbols(&json(
            r#"[
                {
                    "name": "LegacyThing",
                    "kind": 5,
                    "deprecated": false,
                    "containerName": "legacy::module",
                    "location": {
                        "uri": "file:///tmp/legacy.rs",
                        "range": {
                            "start": {"line": 3, "character": 4},
                            "end": {"line": 3, "character": 15}
                        }
                    }
                },
                {
                    "name": "modern_thing",
                    "kind": 12,
                    "tags": [1],
                    "containerName": "modern",
                    "location": {
                        "uri": "file:///tmp/modern.rs",
                        "range": {
                            "start": {"line": 8, "character": 2},
                            "end": {"line": 9, "character": 0}
                        }
                    },
                    "data": {"serverToken": 17}
                }
            ]"#,
        ));

        assert!(!report.truncated);
        assert_eq!(report.skipped_invalid, 0);
        assert_eq!(report.symbols.len(), 2);
        assert_eq!(report.symbols[0].name, "LegacyThing");
        assert_eq!(
            report.symbols[0].container_name.as_deref(),
            Some("legacy::module")
        );
        assert_eq!(report.symbols[0].kind, WorkspaceSymbolKind::Class);
        assert_eq!(report.symbols[0].kind.label(), "Class");
        assert_eq!(report.symbols[0].location.range.start.line.get(), 3);
        assert_eq!(report.symbols[1].name, "modern_thing");
        assert_eq!(report.symbols[1].kind, WorkspaceSymbolKind::Function);
        assert_eq!(report.symbols[1].location.range.end.line.get(), 9);
    }

    #[test]
    fn workspace_symbols_map_every_standard_kind_and_preserve_unknown_values() {
        let expected = [
            (WorkspaceSymbolKind::File, "File"),
            (WorkspaceSymbolKind::Module, "Module"),
            (WorkspaceSymbolKind::Namespace, "Namespace"),
            (WorkspaceSymbolKind::Package, "Package"),
            (WorkspaceSymbolKind::Class, "Class"),
            (WorkspaceSymbolKind::Method, "Method"),
            (WorkspaceSymbolKind::Property, "Property"),
            (WorkspaceSymbolKind::Field, "Field"),
            (WorkspaceSymbolKind::Constructor, "Constructor"),
            (WorkspaceSymbolKind::Enum, "Enum"),
            (WorkspaceSymbolKind::Interface, "Interface"),
            (WorkspaceSymbolKind::Function, "Function"),
            (WorkspaceSymbolKind::Variable, "Variable"),
            (WorkspaceSymbolKind::Constant, "Constant"),
            (WorkspaceSymbolKind::String, "String"),
            (WorkspaceSymbolKind::Number, "Number"),
            (WorkspaceSymbolKind::Boolean, "Boolean"),
            (WorkspaceSymbolKind::Array, "Array"),
            (WorkspaceSymbolKind::Object, "Object"),
            (WorkspaceSymbolKind::Key, "Key"),
            (WorkspaceSymbolKind::Null, "Null"),
            (WorkspaceSymbolKind::EnumMember, "Enum member"),
            (WorkspaceSymbolKind::Struct, "Struct"),
            (WorkspaceSymbolKind::Event, "Event"),
            (WorkspaceSymbolKind::Operator, "Operator"),
            (WorkspaceSymbolKind::TypeParameter, "Type parameter"),
        ];
        for (index, (kind, label)) in expected.into_iter().enumerate() {
            assert_eq!(parse_workspace_symbol_kind(index as i64 + 1), kind);
            assert_eq!(kind.label(), label);
        }

        let report = parse_workspace_symbols(&json(
            r#"[{
                "name": "future",
                "kind": 99,
                "location": {
                    "uri": "file:///tmp/future.rs",
                    "range": {
                        "start": {"line": 0, "character": 0},
                        "end": {"line": 0, "character": 1}
                    }
                }
            }]"#,
        ));
        assert_eq!(report.symbols[0].kind, WorkspaceSymbolKind::Unknown(99));
        assert_eq!(report.symbols[0].kind.label(), "Unknown");
    }

    #[test]
    fn workspace_symbols_treat_null_as_empty_and_report_invalid_top_level_data() {
        assert_eq!(
            parse_workspace_symbols(&JsonValue::Null),
            WorkspaceSymbolReport::default()
        );
        let invalid = parse_workspace_symbols(&json(r#"{"name":"not an array"}"#));
        assert!(invalid.symbols.is_empty());
        assert!(!invalid.truncated);
        assert_eq!(invalid.skipped_invalid, 1);
    }

    #[test]
    fn workspace_symbols_skip_malformed_range_less_non_file_and_control_data() {
        let report = parse_workspace_symbols(&json(
            r#"[
                42,
                {},
                {"name":"missing kind","location":{}},
                {"name":"wrong kind","kind":"12","location":{}},
                {"name":"missing location","kind":12},
                {
                    "name":"range-less",
                    "kind":12,
                    "location":{"uri":"file:///tmp/a.rs"}
                },
                {
                    "name":"web URI",
                    "kind":12,
                    "location":{"uri":"https://example.test/a.rs","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}
                },
                {
                    "name":"remote file URI",
                    "kind":12,
                    "location":{"uri":"file://remote/tmp/a.rs","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}
                },
                {
                    "name":"bad escape",
                    "kind":12,
                    "location":{"uri":"file:///tmp/%QQ.rs","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}
                },
                {
                    "name":"backward range",
                    "kind":12,
                    "location":{"uri":"file:///tmp/a.rs","range":{"start":{"line":2,"character":0},"end":{"line":1,"character":9}}}
                },
                {
                    "name":"bad\nname",
                    "kind":12,
                    "location":{"uri":"file:///tmp/a.rs","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}
                },
                {
                    "name":"bad container",
                    "containerName":"terminal\u001battack",
                    "kind":12,
                    "location":{"uri":"file:///tmp/a.rs","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}
                },
                {
                    "name":"null container",
                    "containerName":null,
                    "kind":12,
                    "location":{"uri":"file:///tmp/a.rs","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}
                },
                {
                    "name":"raw URI control",
                    "kind":12,
                    "location":{"uri":"file:///tmp/a\u001b.rs","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}
                },
                {
                    "name":"encoded URI control",
                    "kind":12,
                    "location":{"uri":"file:///tmp/a%1B.rs","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}
                },
                {
                    "name":"kept",
                    "kind":12,
                    "location":{"uri":"file:///tmp/kept.rs","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}
                }
            ]"#,
        ));

        assert_eq!(report.skipped_invalid, 15);
        assert!(!report.truncated);
        assert_eq!(report.symbols.len(), 1);
        assert_eq!(report.symbols[0].name, "kept");
        assert!(
            report.symbols[0]
                .name
                .chars()
                .all(|character| !character.is_control())
        );
    }

    #[test]
    fn workspace_symbols_decode_percent_uris_and_clip_fields_at_unicode_boundaries() {
        let name = format!("{}🪶discarded", "a".repeat(511));
        let container = format!("{}édiscarded", "b".repeat(510));
        let result = json(&format!(
            r#"[{{
                "name":"{name}",
                "containerName":"{container}",
                "kind":22,
                "location":{{
                    "uri":"file:///tmp/a%20b%23c.rs",
                    "range":{{
                        "start":{{"line":1,"character":2}},
                        "end":{{"line":1,"character":3}}
                    }}
                }}
            }}]"#
        ));
        let report = parse_workspace_symbols(&result);

        assert!(report.truncated);
        assert_eq!(report.skipped_invalid, 0);
        assert_eq!(report.symbols.len(), 1);
        assert_eq!(report.symbols[0].name.len(), 511);
        assert_eq!(
            report.symbols[0].container_name.as_ref().unwrap().len(),
            512
        );
        assert!(
            report.symbols[0]
                .container_name
                .as_ref()
                .unwrap()
                .ends_with('é')
        );
        assert_eq!(report.symbols[0].kind, WorkspaceSymbolKind::EnumMember);
        assert_eq!(
            file_uri_to_path(&report.symbols[0].location.uri).unwrap(),
            PathBuf::from("/tmp/a b#c.rs")
        );
    }

    #[test]
    fn workspace_symbols_reject_oversized_file_uris_instead_of_clipping_them() {
        let uri = format!(
            "file:///tmp/{}.rs",
            "a".repeat(MAX_WORKSPACE_SYMBOL_URI_BYTES)
        );
        let result = json(&format!(
            r#"[{{
                "name":"too_far",
                "kind":12,
                "location":{{
                    "uri":"{uri}",
                    "range":{{
                        "start":{{"line":0,"character":0}},
                        "end":{{"line":0,"character":1}}
                    }}
                }}
            }}]"#
        ));
        let report = parse_workspace_symbols(&result);

        assert!(report.symbols.is_empty());
        assert!(!report.truncated);
        assert_eq!(report.skipped_invalid, 1);
    }

    #[test]
    fn workspace_symbols_inspect_at_most_4096_candidates() {
        let template = match json(
            r#"[{
                "name":"bounded",
                "kind":13,
                "location":{
                    "uri":"file:///tmp/bounded.rs",
                    "range":{
                        "start":{"line":0,"character":0},
                        "end":{"line":0,"character":1}
                    }
                }
            }]"#,
        ) {
            JsonValue::Array(mut values) => values.remove(0),
            _ => unreachable!(),
        };
        let result = JsonValue::Array(vec![template; MAX_WORKSPACE_SYMBOLS + 1]);
        let report = parse_workspace_symbols(&result);

        assert_eq!(report.symbols.len(), MAX_WORKSPACE_SYMBOLS);
        assert!(report.truncated);
        assert_eq!(report.skipped_invalid, 0);
    }

    #[test]
    fn workspace_symbols_enforce_the_lsp_uinteger_position_limit() {
        let accepted = parse_workspace_symbols(&json(
            r#"[{
                "name":"largest valid position",
                "kind":13,
                "location":{
                    "uri":"file:///tmp/valid.rs",
                    "range":{
                        "start":{"line":2147483647,"character":2147483647},
                        "end":{"line":2147483647,"character":2147483647}
                    }
                }
            }]"#,
        ));
        assert_eq!(accepted.symbols.len(), 1);
        assert_eq!(accepted.skipped_invalid, 0);

        for invalid_range in [
            r#"{"start":{"line":2147483648,"character":0},"end":{"line":2147483648,"character":0}}"#,
            r#"{"start":{"line":0,"character":2147483648},"end":{"line":0,"character":2147483648}}"#,
            r#"{"start":{"line":18446744073709551615,"character":0},"end":{"line":18446744073709551615,"character":0}}"#,
        ] {
            let result = json(&format!(
                r#"[{{
                    "name":"invalid position",
                    "kind":13,
                    "location":{{"uri":"file:///tmp/invalid.rs","range":{invalid_range}}}
                }}]"#
            ));
            let report = parse_workspace_symbols(&result);
            assert!(report.symbols.is_empty(), "accepted {invalid_range}");
            assert_eq!(report.skipped_invalid, 1);
        }
    }

    #[test]
    fn parses_completion_location_hover_and_file_uri() {
        let completions = parse_completion(&json(
            r#"{"items":[{"label":"println!","detail":"macro","insertText":"println!($0)","insertTextFormat":2}]}"#,
        ));
        assert_eq!(completions[0].label, "println!");
        assert!(completions[0].is_snippet);

        let locations = parse_locations(&json(
            r#"[{"targetUri":"file:///tmp/a%20b.rs","targetSelectionRange":{"start":{"line":1,"character":2},"end":{"line":1,"character":3}}}]"#,
        ));
        assert_eq!(
            file_uri_to_path(&locations[0].uri).unwrap(),
            PathBuf::from("/tmp/a b.rs")
        );
        assert_eq!(
            render_hover(&json(
                r#"{"contents":{"kind":"markdown","value":"**type**"}}"#
            )),
            Some("**type**".to_owned())
        );
    }

    #[test]
    fn completion_text_edit_distinguishes_absent_valid_and_malformed_shapes() {
        let completions = parse_completion(&json(
            r#"[
                {"label":"plain","insertText":"plain()"},
                {
                    "label":"replace",
                    "textEdit":{
                        "range":{
                            "start":{"line":1,"character":2},
                            "end":{"line":1,"character":5}
                        },
                        "newText":"replacement"
                    }
                },
                {"label":"missing range","insertText":"must not fall back","textEdit":{"newText":"bad"}},
                {
                    "label":"invalid range",
                    "insertText":"must not fall back",
                    "textEdit":{
                        "range":{
                            "start":{"line":0,"character":0},
                            "end":{"line":0}
                        },
                        "newText":"bad"
                    }
                }
            ]"#,
        ));

        assert_eq!(completions.len(), 2);
        assert_eq!(completions[0].label, "plain");
        assert_eq!(completions[0].insert_text, "plain()");
        assert_eq!(completions[0].text_edit, None);
        assert_eq!(completions[1].label, "replace");
        assert_eq!(completions[1].insert_text, "replacement");
        assert_eq!(
            completions[1].text_edit,
            Some(TextEdit {
                range: LspRange::new(
                    LspPosition::new(1.into(), 2.into()),
                    LspPosition::new(1.into(), 5.into()),
                ),
                new_text: "replacement".to_owned(),
            })
        );
    }

    #[test]
    fn completion_insert_replace_edit_requires_both_valid_ranges() {
        let completions = parse_completion(&json(
            r#"[
                {
                    "label":"valid",
                    "textEdit":{
                        "insert":{
                            "start":{"line":2,"character":1},
                            "end":{"line":2,"character":3}
                        },
                        "replace":{
                            "start":{"line":2,"character":1},
                            "end":{"line":2,"character":6}
                        },
                        "newText":"valid edit"
                    }
                },
                {
                    "label":"missing insert",
                    "textEdit":{
                        "replace":{
                            "start":{"line":0,"character":0},
                            "end":{"line":0,"character":1}
                        },
                        "newText":"bad"
                    }
                },
                {
                    "label":"invalid insert",
                    "textEdit":{
                        "insert":{
                            "start":{"line":0,"character":0},
                            "end":{"line":2147483648,"character":1}
                        },
                        "replace":{
                            "start":{"line":0,"character":0},
                            "end":{"line":0,"character":1}
                        },
                        "newText":"bad"
                    }
                },
                {
                    "label":"different starts",
                    "textEdit":{
                        "insert":{
                            "start":{"line":0,"character":1},
                            "end":{"line":0,"character":3}
                        },
                        "replace":{
                            "start":{"line":0,"character":0},
                            "end":{"line":0,"character":6}
                        },
                        "newText":"bad"
                    }
                },
                {
                    "label":"insert exceeds replace",
                    "textEdit":{
                        "insert":{
                            "start":{"line":0,"character":1},
                            "end":{"line":0,"character":7}
                        },
                        "replace":{
                            "start":{"line":0,"character":1},
                            "end":{"line":0,"character":6}
                        },
                        "newText":"bad"
                    }
                },
                {
                    "label":"inverted insert",
                    "textEdit":{
                        "insert":{
                            "start":{"line":0,"character":2},
                            "end":{"line":0,"character":1}
                        },
                        "replace":{
                            "start":{"line":0,"character":2},
                            "end":{"line":0,"character":6}
                        },
                        "newText":"bad"
                    }
                }
            ]"#,
        ));

        assert_eq!(completions.len(), 1);
        assert_eq!(completions[0].label, "valid");
        assert_eq!(
            completions[0].text_edit.as_ref().unwrap().range,
            LspRange::new(
                LspPosition::new(2.into(), 1.into()),
                LspPosition::new(2.into(), 6.into()),
            )
        );
    }

    #[test]
    fn completion_insert_text_rejects_present_non_string_values() {
        let completions = parse_completion(&json(
            r#"[
                {"label":"absent"},
                {"label":"string","insertText":"inserted"},
                {"label":"number","insertText":7},
                {"label":"null","insertText":null},
                {"label":"object","insertText":{}},
                {"label":"array","insertText":[]}
            ]"#,
        ));

        assert_eq!(
            completions
                .iter()
                .map(|item| (item.label.as_str(), item.insert_text.as_str()))
                .collect::<Vec<_>>(),
            vec![("absent", "absent"), ("string", "inserted")]
        );
    }

    #[test]
    fn completion_insert_text_format_accepts_only_absent_plain_and_snippet() {
        let completions = parse_completion(&json(
            r#"[
                {"label":"absent"},
                {"label":"plain","insertTextFormat":1},
                {"label":"snippet","insertTextFormat":2},
                {"label":"zero","insertTextFormat":0},
                {"label":"future","insertTextFormat":3},
                {"label":"string","insertTextFormat":"2"},
                {"label":"null","insertTextFormat":null}
            ]"#,
        ));

        assert_eq!(
            completions
                .iter()
                .map(|item| (item.label.as_str(), item.is_snippet))
                .collect::<Vec<_>>(),
            vec![("absent", false), ("plain", false), ("snippet", true)]
        );
    }
}
