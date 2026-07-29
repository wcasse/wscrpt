//! Conservative, bounded extraction of navigable locations from task output.
//!
//! This module does not attempt to understand every compiler or test runner.
//! It accepts rustc's primary `--> path:line:column` marker, the common
//! `path:line:column: message` form, Go/Python-style line-only locations,
//! JavaScript stack-frame locations, TypeScript/Visual Studio style
//! `path(line,column): message`, and two-line lint output that prints a file
//! path followed by `line:column severity message`. Every returned path is
//! canonical, points to a regular file, and remains inside the canonical
//! workspace root.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Newest task-output bytes considered by one scan.
pub const MAX_TASK_PROBLEM_INPUT_BYTES: usize = 1024 * 1024;
/// Newest logical lines considered by one scan.
pub const MAX_TASK_PROBLEM_LINES: usize = 16_384;
/// Maximum bytes accepted from one task-output line.
pub const MAX_TASK_PROBLEM_LINE_BYTES: usize = 16 * 1024;
/// Maximum syntactically plausible locations resolved against the filesystem.
pub const MAX_TASK_PROBLEM_CANDIDATES: usize = 2_048;
/// Maximum distinct problems returned by one scan.
pub const MAX_TASK_PROBLEMS: usize = 1_024;
/// Maximum UTF-8 bytes accepted in a path printed by a task.
pub const MAX_TASK_PROBLEM_PATH_BYTES: usize = 4 * 1024;
/// Maximum UTF-8 bytes retained from one diagnostic message.
pub const MAX_TASK_PROBLEM_MESSAGE_BYTES: usize = 1_024;
/// Maximum bytes accepted in one ANSI CSI escape sequence.
pub const MAX_TASK_PROBLEM_ANSI_SEQUENCE_BYTES: usize = 64;

const TASK_OUTPUT_TRIM_MARKER: &str = "[… earlier task output trimmed …]";

/// Severity inferred conservatively from task output.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TaskProblemSeverity {
    Error,
    Warning,
    Information,
    Unknown,
}

/// Coordinate convention known from the diagnostic format.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TaskProblemColumnKind {
    /// rustc's primary arrow locations count Unicode scalar values.
    UnicodeScalar,
    /// A generic `path:line:column` producer did not identify its convention.
    Unknown,
}

impl TaskProblemSeverity {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Information => "information",
            Self::Unknown => "problem",
        }
    }
}

/// One task-produced source location.
///
/// `path` is canonical and workspace-contained. `line` and `column` are
/// zero-based. `column_kind` records whether the syntax establishes a scalar
/// coordinate; generic compiler conventions remain unknown. Callers should
/// clamp positions against the current document because task output can be
/// stale.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct TaskProblem {
    pub path: PathBuf,
    pub line: usize,
    pub column: usize,
    pub column_kind: TaskProblemColumnKind,
    pub severity: TaskProblemSeverity,
    pub message: String,
}

/// Bounded result and coverage metadata for one task-output scan.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskProblemReport {
    pub problems: Vec<TaskProblem>,
    /// True when any byte, line, candidate, result, path, message, or ANSI
    /// sequence bound prevented the scan from considering or retaining all
    /// otherwise relevant input.
    pub truncated: bool,
    pub scanned_bytes: usize,
    pub scanned_lines: usize,
    pub candidates_checked: usize,
}

#[derive(Clone, Copy, Debug)]
struct ParseLimits {
    input_bytes: usize,
    lines: usize,
    line_bytes: usize,
    candidates: usize,
    results: usize,
    path_bytes: usize,
    message_bytes: usize,
    ansi_sequence_bytes: usize,
}

impl Default for ParseLimits {
    fn default() -> Self {
        Self {
            input_bytes: MAX_TASK_PROBLEM_INPUT_BYTES,
            lines: MAX_TASK_PROBLEM_LINES,
            line_bytes: MAX_TASK_PROBLEM_LINE_BYTES,
            candidates: MAX_TASK_PROBLEM_CANDIDATES,
            results: MAX_TASK_PROBLEMS,
            path_bytes: MAX_TASK_PROBLEM_PATH_BYTES,
            message_bytes: MAX_TASK_PROBLEM_MESSAGE_BYTES,
            ansi_sequence_bytes: MAX_TASK_PROBLEM_ANSI_SEQUENCE_BYTES,
        }
    }
}

#[derive(Clone, Debug)]
struct RustcContext {
    severity: TaskProblemSeverity,
    message: String,
    message_truncated: bool,
}

#[derive(Clone, Copy, Debug)]
struct RawLocation<'a> {
    path: &'a str,
    line: usize,
    column: usize,
    message: Option<&'a str>,
}

/// Extract navigable problems from bounded task output.
///
/// Relative paths are resolved against `task_cwd`, not the editor process's
/// current directory. Failure to canonicalize either required directory is an
/// error. Individual malformed, missing, non-file, or out-of-workspace
/// candidates are ignored without hiding valid candidates on other lines.
pub fn parse_task_problems(
    output: &str,
    task_cwd: &Path,
    workspace_root: &Path,
) -> io::Result<TaskProblemReport> {
    parse_task_problems_with_limits(output, task_cwd, workspace_root, ParseLimits::default())
}

fn parse_task_problems_with_limits(
    output: &str,
    task_cwd: &Path,
    workspace_root: &Path,
    limits: ParseLimits,
) -> io::Result<TaskProblemReport> {
    let workspace_root = canonical_directory(workspace_root, "workspace root")?;
    let task_cwd = canonical_directory(task_cwd, "task working directory")?;
    let (bounded, input_truncated) = bounded_suffix(output, limits.input_bytes);
    let mut report = TaskProblemReport {
        truncated: input_truncated,
        scanned_bytes: bounded.len(),
        ..TaskProblemReport::default()
    };

    let mut newest_lines: Vec<&str> = bounded
        .lines()
        .rev()
        .take(limits.lines.saturating_add(1))
        .collect();
    if newest_lines.len() > limits.lines {
        newest_lines.pop();
        report.truncated = true;
    }
    newest_lines.reverse();

    let mut seen = HashSet::new();
    let mut rustc_context = None;
    let mut path_context: Option<String> = None;
    let mut skip_after_output_gap = false;

    'lines: for raw_line in newest_lines {
        report.scanned_lines += 1;
        if raw_line.len() > limits.line_bytes {
            report.truncated = true;
            rustc_context = None;
            continue;
        }
        let Some(cleaned) = strip_ansi_csi(raw_line, limits.ansi_sequence_bytes) else {
            report.truncated |= raw_line.as_bytes().contains(&0x1b);
            rustc_context = None;
            continue;
        };
        let line = cleaned.trim();
        if line.is_empty() {
            continue;
        }

        if is_task_output_gap_marker(line) {
            // The retained text immediately after a trim/drop boundary may be
            // the tail of a severed pipe line. Lose at most one candidate
            // rather than synthesizing a location across missing bytes.
            report.truncated = true;
            rustc_context = None;
            path_context = None;
            skip_after_output_gap = true;
            continue;
        }
        if skip_after_output_gap {
            skip_after_output_gap = false;
            rustc_context = None;
            path_context = None;
            continue;
        }

        if let Some(location_text) = line.strip_prefix("-->").map(str::trim) {
            path_context = None;
            if let Some(location) = parse_location(location_text, false) {
                let context = rustc_context.take();
                if !push_candidate(
                    location,
                    TaskProblemColumnKind::UnicodeScalar,
                    context.as_ref(),
                    &task_cwd,
                    &workspace_root,
                    limits,
                    &mut seen,
                    &mut report,
                ) {
                    break 'lines;
                }
            } else {
                rustc_context = None;
            }
            continue;
        }

        if let Some(context) = parse_rustc_context(line, limits.message_bytes) {
            rustc_context = Some(context);
            path_context = None;
            continue;
        }

        rustc_context = None;
        if let Some(context_path) = path_context.as_deref()
            && let Some(location) = parse_context_location(line, context_path)
        {
            if !push_candidate(
                location,
                TaskProblemColumnKind::Unknown,
                None,
                &task_cwd,
                &workspace_root,
                limits,
                &mut seen,
                &mut report,
            ) {
                break 'lines;
            }
            continue;
        }
        path_context = None;

        if let Some(location) = parse_location(line, true) {
            if !push_candidate(
                location,
                TaskProblemColumnKind::Unknown,
                None,
                &task_cwd,
                &workspace_root,
                limits,
                &mut seen,
                &mut report,
            ) {
                break 'lines;
            }
            continue;
        }
        if let Some(location) = parse_parenthesized_location(line) {
            if !push_candidate(
                location,
                TaskProblemColumnKind::Unknown,
                None,
                &task_cwd,
                &workspace_root,
                limits,
                &mut seen,
                &mut report,
            ) {
                break 'lines;
            }
            continue;
        }
        if let Some(location) = parse_stack_frame_location(line) {
            if !push_candidate(
                location,
                TaskProblemColumnKind::Unknown,
                None,
                &task_cwd,
                &workspace_root,
                limits,
                &mut seen,
                &mut report,
            ) {
                break 'lines;
            }
            continue;
        }
        if let Some(location) = parse_line_only_location(line) {
            if !push_candidate(
                location,
                TaskProblemColumnKind::Unknown,
                None,
                &task_cwd,
                &workspace_root,
                limits,
                &mut seen,
                &mut report,
            ) {
                break 'lines;
            }
            continue;
        }
        if let Some(location) = parse_python_file_line_location(line) {
            if !push_candidate(
                location,
                TaskProblemColumnKind::Unknown,
                None,
                &task_cwd,
                &workspace_root,
                limits,
                &mut seen,
                &mut report,
            ) {
                break 'lines;
            }
            continue;
        }
        if plausible_context_path(line, limits.path_bytes) {
            path_context = Some(line.to_owned());
        }
    }

    Ok(report)
}

fn is_task_output_gap_marker(line: &str) -> bool {
    if line == TASK_OUTPUT_TRIM_MARKER {
        return true;
    }
    let Some(count) = line
        .strip_prefix("[… ")
        .and_then(|line| line.strip_suffix(" output bytes dropped …]"))
    else {
        return false;
    };
    !count.is_empty() && count.bytes().all(|byte| byte.is_ascii_digit())
}

#[allow(clippy::too_many_arguments)]
fn push_candidate(
    raw: RawLocation<'_>,
    column_kind: TaskProblemColumnKind,
    context: Option<&RustcContext>,
    task_cwd: &Path,
    workspace_root: &Path,
    limits: ParseLimits,
    seen: &mut HashSet<TaskProblem>,
    report: &mut TaskProblemReport,
) -> bool {
    if report.candidates_checked >= limits.candidates {
        report.truncated = true;
        return false;
    }
    report.candidates_checked += 1;

    let raw_path = raw.path.trim();
    if raw_path.is_empty()
        || raw_path.len() > limits.path_bytes
        || raw_path.chars().any(char::is_control)
    {
        report.truncated |= raw_path.len() > limits.path_bytes;
        return true;
    }
    if raw.line == 0 || raw.column == 0 {
        return true;
    }

    let unresolved = Path::new(raw_path);
    let unresolved = if unresolved.is_absolute() {
        unresolved.to_path_buf()
    } else {
        task_cwd.join(unresolved)
    };
    let Ok(path) = fs::canonicalize(unresolved) else {
        return true;
    };
    if !path.starts_with(workspace_root)
        || !fs::metadata(&path).is_ok_and(|metadata| metadata.is_file())
    {
        return true;
    }

    let (severity, raw_message) = context.map_or_else(
        || severity_and_message(raw.message.unwrap_or_default()),
        |context| (context.severity, context.message.as_str()),
    );
    if raw_message.chars().any(char::is_control) {
        return true;
    }
    let fallback = severity.label();
    let source_message = if raw_message.trim().is_empty() {
        fallback
    } else {
        raw_message.trim()
    };
    let (message, message_truncated) = truncate_utf8(source_message, limits.message_bytes);
    report.truncated |=
        message_truncated || context.is_some_and(|context| context.message_truncated);

    let problem = TaskProblem {
        path,
        line: raw.line - 1,
        column: raw.column - 1,
        column_kind,
        severity,
        message,
    };
    if seen.insert(problem.clone()) {
        if report.problems.len() >= limits.results {
            report.truncated = true;
            return false;
        }
        report.problems.push(problem);
    }
    true
}

fn canonical_directory(path: &Path, label: &str) -> io::Result<PathBuf> {
    let canonical = fs::canonicalize(path)?;
    if !fs::metadata(&canonical)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} is not a directory: {}", canonical.display()),
        ));
    }
    Ok(canonical)
}

/// Keep the newest bounded suffix. When the bound cuts through a line, discard
/// that first partial line rather than interpreting attacker-controlled tail
/// bytes as a path.
fn bounded_suffix(input: &str, max_bytes: usize) -> (&str, bool) {
    if input.len() <= max_bytes {
        return (input, false);
    }
    if max_bytes == 0 {
        return ("", true);
    }
    let mut start = input.len() - max_bytes;
    while start < input.len() && !input.is_char_boundary(start) {
        start += 1;
    }
    let suffix = &input[start..];
    let suffix = suffix
        .find('\n')
        .map_or("", |newline| &suffix[newline + 1..]);
    (suffix, true)
}

fn strip_ansi_csi(input: &str, max_sequence_bytes: usize) -> Option<String> {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == 0x1b {
            let sequence_start = index;
            if bytes.get(index + 1) != Some(&b'[') {
                return None;
            }
            index += 2;
            let mut complete = false;
            while index < bytes.len() {
                if index - sequence_start >= max_sequence_bytes {
                    return None;
                }
                let current = bytes[index];
                if (0x40..=0x7e).contains(&current) {
                    index += 1;
                    complete = true;
                    break;
                }
                if !(0x20..=0x3f).contains(&current) {
                    return None;
                }
                index += 1;
            }
            if !complete {
                return None;
            }
            continue;
        }
        if byte < 0x20 && byte != b'\t' {
            return None;
        }
        output.push(byte);
        index += 1;
    }
    String::from_utf8(output).ok()
}

fn parse_rustc_context(line: &str, message_bytes: usize) -> Option<RustcContext> {
    let (severity, prefix_len) = severity_prefix(line)?;
    if !matches!(
        severity,
        TaskProblemSeverity::Error | TaskProblemSeverity::Warning
    ) {
        return None;
    }
    let remainder = &line[prefix_len..];
    if !remainder.starts_with(':') && !remainder.starts_with('[') {
        return None;
    }
    let message = remainder
        .find(':')
        .map_or(severity.label(), |colon| remainder[colon + 1..].trim());
    let message = if message.is_empty() {
        severity.label()
    } else {
        message
    };
    if message.chars().any(char::is_control) {
        return None;
    }
    let (message, message_truncated) = truncate_utf8(message, message_bytes);
    Some(RustcContext {
        severity,
        message,
        message_truncated,
    })
}

fn severity_and_message(message: &str) -> (TaskProblemSeverity, &str) {
    let message = message.trim();
    let Some((severity, prefix_len)) = severity_prefix(message) else {
        return (TaskProblemSeverity::Unknown, message);
    };
    let remainder = &message[prefix_len..];
    if remainder.starts_with(char::is_whitespace) {
        return (severity, remainder.trim());
    }
    if !remainder.starts_with(':') && !remainder.starts_with('[') {
        return (TaskProblemSeverity::Unknown, message);
    }
    let detail = remainder
        .find(':')
        .map_or("", |colon| remainder[colon + 1..].trim());
    (severity, detail)
}

fn severity_prefix(message: &str) -> Option<(TaskProblemSeverity, usize)> {
    let lowered = message.get(..message.len().min(16))?.to_ascii_lowercase();
    for (prefix, severity) in [
        ("error", TaskProblemSeverity::Error),
        ("warning", TaskProblemSeverity::Warning),
        ("note", TaskProblemSeverity::Information),
        ("info", TaskProblemSeverity::Information),
        ("help", TaskProblemSeverity::Information),
    ] {
        if lowered.starts_with(prefix) {
            return Some((severity, prefix.len()));
        }
    }
    None
}

fn plausible_context_path(input: &str, path_bytes: usize) -> bool {
    !input.is_empty()
        && input.len() <= path_bytes
        && !input.chars().any(char::is_control)
        && !input.bytes().any(|byte| byte == b'(' || byte == b')')
        && input
            .chars()
            .any(|character| matches!(character, '/' | '\\' | '.'))
}

fn parse_location(input: &str, require_message: bool) -> Option<RawLocation<'_>> {
    let bytes = input.as_bytes();
    for first_colon in bytes
        .iter()
        .enumerate()
        .filter_map(|(index, byte)| (*byte == b':').then_some(index))
    {
        let Some((line, after_line)) = parse_decimal(bytes, first_colon + 1) else {
            continue;
        };
        if bytes.get(after_line) != Some(&b':') {
            continue;
        }
        let Some((column, after_column)) = parse_decimal(bytes, after_line + 1) else {
            continue;
        };
        let message = if bytes.get(after_column) == Some(&b':') {
            Some(&input[after_column + 1..])
        } else if after_column == bytes.len() && !require_message {
            None
        } else {
            continue;
        };
        let path = &input[..first_colon];
        return Some(RawLocation {
            path,
            line,
            column,
            message,
        });
    }
    None
}

fn parse_parenthesized_location(input: &str) -> Option<RawLocation<'_>> {
    let bytes = input.as_bytes();
    for open in bytes
        .iter()
        .enumerate()
        .filter_map(|(index, byte)| (*byte == b'(').then_some(index))
    {
        let Some((line, after_line)) = parse_decimal(bytes, open + 1) else {
            continue;
        };
        if bytes.get(after_line) != Some(&b',') {
            continue;
        }
        let mut column_start = after_line + 1;
        while bytes.get(column_start) == Some(&b' ') {
            column_start += 1;
        }
        let Some((column, after_column)) = parse_decimal(bytes, column_start) else {
            continue;
        };
        if bytes.get(after_column) != Some(&b')') || bytes.get(after_column + 1) != Some(&b':') {
            continue;
        }
        let path = input[..open].trim();
        let message = input[after_column + 2..].trim();
        if path.is_empty() || message.is_empty() {
            return None;
        }
        return Some(RawLocation {
            path,
            line,
            column,
            message: Some(message),
        });
    }
    None
}

fn parse_stack_frame_location(input: &str) -> Option<RawLocation<'_>> {
    let trimmed = input.trim();
    if !trimmed.starts_with("at ") {
        return None;
    }
    let candidate = trimmed
        .rfind('(')
        .and_then(|open| {
            trimmed[open + 1..]
                .find(')')
                .map(|relative_close| &trimmed[open + 1..open + 1 + relative_close])
        })
        .or_else(|| trimmed.strip_prefix("at ").map(str::trim))?;
    let location = parse_location(candidate, false)?;
    Some(RawLocation {
        message: Some(trimmed),
        ..location
    })
}

fn parse_line_only_location(input: &str) -> Option<RawLocation<'_>> {
    let bytes = input.as_bytes();
    for first_colon in bytes
        .iter()
        .enumerate()
        .filter_map(|(index, byte)| (*byte == b':').then_some(index))
    {
        let Some((line, after_line)) = parse_decimal(bytes, first_colon + 1) else {
            continue;
        };
        if bytes.get(after_line) != Some(&b':') {
            continue;
        }
        if matches!(bytes.get(after_line + 1), Some(b'0'..=b'9')) {
            continue;
        }
        let message = input[after_line + 1..].trim();
        if message.is_empty() {
            continue;
        }
        return Some(RawLocation {
            path: input[..first_colon].trim(),
            line,
            column: 1,
            message: Some(message),
        });
    }
    None
}

fn parse_python_file_line_location(input: &str) -> Option<RawLocation<'_>> {
    let trimmed = input.trim();
    let quoted_path = trimmed.strip_prefix("File \"")?;
    let close = quoted_path.find('"')?;
    let path = &quoted_path[..close];
    let after_path = &quoted_path[close + 1..];
    let line_text = after_path.trim_start().strip_prefix(", line ")?;
    let bytes = line_text.as_bytes();
    let (line, after_line) = parse_decimal(bytes, 0)?;
    let message = line_text[after_line..]
        .trim_start_matches(',')
        .trim()
        .trim_start_matches("in ")
        .trim();
    Some(RawLocation {
        path,
        line,
        column: 1,
        message: Some(if message.is_empty() {
            "python traceback"
        } else {
            message
        }),
    })
}

fn parse_context_location<'a>(input: &'a str, path: &'a str) -> Option<RawLocation<'a>> {
    let input = input.trim();
    let bytes = input.as_bytes();
    let (line, after_line) = parse_decimal(bytes, 0)?;
    if bytes.get(after_line) != Some(&b':') {
        return None;
    }
    let (column, after_column) = parse_decimal(bytes, after_line + 1)?;
    let message = input.get(after_column..)?.trim();
    if message.is_empty() || message.as_bytes().first().is_some_and(|byte| *byte == b':') {
        return None;
    }
    Some(RawLocation {
        path,
        line,
        column,
        message: Some(message),
    })
}

fn parse_decimal(bytes: &[u8], start: usize) -> Option<(usize, usize)> {
    let mut index = start;
    let mut value = 0_usize;
    let mut digits = 0_usize;
    while let Some(byte @ b'0'..=b'9') = bytes.get(index) {
        value = value
            .checked_mul(10)?
            .checked_add(usize::from(*byte - b'0'))?;
        index += 1;
        digits += 1;
    }
    (digits > 0).then_some((value, index))
}

fn truncate_utf8(input: &str, max_bytes: usize) -> (String, bool) {
    if input.len() <= max_bytes {
        return (input.to_owned(), false);
    }
    if max_bytes == 0 {
        return (String::new(), true);
    }
    const MARKER: &str = "…";
    if max_bytes < MARKER.len() {
        let mut end = max_bytes;
        while end > 0 && !input.is_char_boundary(end) {
            end -= 1;
        }
        return (input[..end].to_owned(), true);
    }
    let mut end = max_bytes - MARKER.len();
    while end > 0 && !input.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = input[..end].to_owned();
    truncated.push_str(MARKER);
    (truncated, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, "first\nsecond\nthird\n").unwrap();
    }

    #[test]
    fn parses_rustc_arrow_and_plain_compiler_locations() {
        let workspace = tempfile::tempdir().unwrap();
        let main = workspace.path().join("src/main.rs");
        let lib = workspace.path().join("src/lib.rs");
        write_file(&main);
        write_file(&lib);
        let output = concat!(
            "error[E0425]: cannot find value `bird` in this scope\n",
            "  --> src/main.rs:2:5\n",
            "src/lib.rs:3:2: warning: unused import\n",
        );

        let report = parse_task_problems(output, workspace.path(), workspace.path()).unwrap();

        assert!(!report.truncated);
        assert_eq!(report.problems.len(), 2);
        assert_eq!(report.problems[0].path, fs::canonicalize(main).unwrap());
        assert_eq!((report.problems[0].line, report.problems[0].column), (1, 4));
        assert_eq!(
            report.problems[0].column_kind,
            TaskProblemColumnKind::UnicodeScalar
        );
        assert_eq!(report.problems[0].severity, TaskProblemSeverity::Error);
        assert_eq!(
            report.problems[0].message,
            "cannot find value `bird` in this scope"
        );
        assert_eq!(report.problems[1].path, fs::canonicalize(lib).unwrap());
        assert_eq!((report.problems[1].line, report.problems[1].column), (2, 1));
        assert_eq!(
            report.problems[1].column_kind,
            TaskProblemColumnKind::Unknown
        );
        assert_eq!(report.problems[1].severity, TaskProblemSeverity::Warning);
        assert_eq!(report.problems[1].message, "unused import");
    }

    #[test]
    fn parses_typescript_parenthesized_locations() {
        let workspace = tempfile::tempdir().unwrap();
        let component = workspace.path().join("src/App.tsx");
        let config = workspace.path().join("vite.config.ts");
        write_file(&component);
        write_file(&config);
        let output = concat!(
            "src/App.tsx(12, 7): error TS2322: Type 'number' is not assignable\n",
            "vite.config.ts(3,1): warning TS9999: unusual option\n",
        );

        let report = parse_task_problems(output, workspace.path(), workspace.path()).unwrap();

        assert_eq!(report.problems.len(), 2);
        assert_eq!(
            report.problems[0].path,
            fs::canonicalize(component).unwrap()
        );
        assert_eq!(
            (report.problems[0].line, report.problems[0].column),
            (11, 6)
        );
        assert_eq!(report.problems[0].severity, TaskProblemSeverity::Error);
        assert_eq!(
            report.problems[0].message,
            "TS2322: Type 'number' is not assignable"
        );
        assert_eq!(report.problems[1].path, fs::canonicalize(config).unwrap());
        assert_eq!((report.problems[1].line, report.problems[1].column), (2, 0));
        assert_eq!(report.problems[1].severity, TaskProblemSeverity::Warning);
    }

    #[test]
    fn parses_eslint_stylish_file_context_locations() {
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("src/app.ts");
        write_file(&source);
        let output = concat!(
            "src/app.ts\n",
            "  4:9   error    Unexpected any. Specify a different type  @typescript-eslint/no-explicit-any\n",
            "  7:1   warning  Missing return type                    @typescript-eslint/explicit-function-return-type\n",
        );

        let report = parse_task_problems(output, workspace.path(), workspace.path()).unwrap();

        assert_eq!(report.problems.len(), 2);
        assert!(
            report
                .problems
                .iter()
                .all(|problem| problem.path == fs::canonicalize(&source).unwrap())
        );
        assert_eq!((report.problems[0].line, report.problems[0].column), (3, 8));
        assert_eq!(report.problems[0].severity, TaskProblemSeverity::Error);
        assert_eq!(
            report.problems[0].message,
            "Unexpected any. Specify a different type  @typescript-eslint/no-explicit-any"
        );
        assert_eq!((report.problems[1].line, report.problems[1].column), (6, 0));
        assert_eq!(report.problems[1].severity, TaskProblemSeverity::Warning);
        assert_eq!(
            report.problems[1].message,
            "Missing return type                    @typescript-eslint/explicit-function-return-type"
        );
    }

    #[test]
    fn parses_go_python_and_javascript_stack_locations() {
        let workspace = tempfile::tempdir().unwrap();
        let go_source = workspace.path().join("cmd/server/main.go");
        let python_source = workspace.path().join("tests/test_bird.py");
        let js_source = workspace.path().join("src/app.ts");
        write_file(&go_source);
        write_file(&python_source);
        write_file(&js_source);
        let output = concat!(
            "cmd/server/main.go:9: undefined: buildNest\n",
            "  File \"tests/test_bird.py\", line 4, in test_flight\n",
            "    assert fly()\n",
            "    at renderNest (src/app.ts:12:7)\n",
        );

        let report = parse_task_problems(output, workspace.path(), workspace.path()).unwrap();

        assert_eq!(report.problems.len(), 3);
        assert_eq!(
            report.problems[0].path,
            fs::canonicalize(go_source).unwrap()
        );
        assert_eq!((report.problems[0].line, report.problems[0].column), (8, 0));
        assert_eq!(report.problems[0].message, "undefined: buildNest");
        assert_eq!(
            report.problems[1].path,
            fs::canonicalize(python_source).unwrap()
        );
        assert_eq!((report.problems[1].line, report.problems[1].column), (3, 0));
        assert_eq!(report.problems[1].message, "test_flight");
        assert_eq!(
            report.problems[2].path,
            fs::canonicalize(js_source).unwrap()
        );
        assert_eq!(
            (report.problems[2].line, report.problems[2].column),
            (11, 6)
        );
        assert_eq!(
            report.problems[2].message,
            "at renderNest (src/app.ts:12:7)"
        );
    }

    #[test]
    fn supports_workspace_paths_with_spaces_and_strips_ansi_csi() {
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("source files/blue bird.rs");
        write_file(&source);
        let output = concat!(
            "\x1b[31;1merror[E0001]: bright failure\x1b[0m\n",
            "\x1b[2K  --> source files/blue bird.rs:1:2\x1b[0m\n",
        );

        let report = parse_task_problems(output, workspace.path(), workspace.path()).unwrap();

        assert_eq!(report.problems.len(), 1);
        assert_eq!(report.problems[0].path, fs::canonicalize(source).unwrap());
        assert_eq!(report.problems[0].message, "bright failure");
        assert_eq!(report.problems[0].severity, TaskProblemSeverity::Error);
    }

    #[test]
    fn resolves_paths_from_the_actual_task_working_directory() {
        let workspace = tempfile::tempdir().unwrap();
        let task_cwd = workspace.path().join("crates/bird");
        fs::create_dir_all(&task_cwd).unwrap();
        let source = workspace.path().join("crates/shared/src/lib.rs");
        write_file(&source);

        let report = parse_task_problems(
            "../shared/src/lib.rs:2:3: error: broken\n",
            &task_cwd,
            workspace.path(),
        )
        .unwrap();

        assert_eq!(report.problems.len(), 1);
        assert_eq!(report.problems[0].path, fs::canonicalize(source).unwrap());
    }

    #[test]
    fn rejects_zero_overflow_malformed_and_control_heavy_locations() {
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("src/main.rs");
        write_file(&source);
        let output = format!(
            concat!(
                "src/main.rs:0:1: error: zero line\n",
                "src/main.rs:1:0: error: zero column\n",
                "src/main.rs:{}0:2: error: overflow\n",
                "src/main.rs:1:2 error: malformed\n",
                "src/main.rs:1:2: error:\x07bell\n",
                "src/main.rs:2:2: error: valid\n",
            ),
            usize::MAX
        );

        let report = parse_task_problems(&output, workspace.path(), workspace.path()).unwrap();

        assert_eq!(report.problems.len(), 1);
        assert_eq!(report.problems[0].message, "valid");
        assert_eq!((report.problems[0].line, report.problems[0].column), (1, 1));
    }

    #[test]
    fn rejects_outside_paths_nonfiles_and_missing_files() {
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("outside.rs");
        write_file(&outside_file);
        fs::create_dir(workspace.path().join("directory.rs")).unwrap();
        let output = format!(
            "{}:1:1: error: outside\ndirectory.rs:1:1: error: directory\nmissing.rs:1:1: error: missing\n",
            outside_file.display()
        );

        let report = parse_task_problems(&output, workspace.path(), workspace.path()).unwrap();

        assert!(report.problems.is_empty());
        assert_eq!(report.candidates_checked, 3);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_escape_from_workspace() {
        use std::os::unix::fs::symlink;

        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("outside.rs");
        write_file(&outside_file);
        symlink(&outside_file, workspace.path().join("escape.rs")).unwrap();

        let report = parse_task_problems(
            "escape.rs:1:1: error: escaped\n",
            workspace.path(),
            workspace.path(),
        )
        .unwrap();

        assert!(report.problems.is_empty());
    }

    #[test]
    fn deduplicates_and_caps_task_problem_candidates() {
        let workspace = tempfile::tempdir().unwrap();
        write_file(&workspace.path().join("src/main.rs"));
        let limits = ParseLimits {
            candidates: 2,
            ..ParseLimits::default()
        };
        let output = "src/main.rs:1:1: error: same\n".repeat(4);

        let report =
            parse_task_problems_with_limits(&output, workspace.path(), workspace.path(), limits)
                .unwrap();

        assert_eq!(report.problems.len(), 1);
        assert_eq!(report.candidates_checked, 2);
        assert!(report.truncated);
    }

    #[test]
    fn caps_distinct_results_without_partially_appending() {
        let workspace = tempfile::tempdir().unwrap();
        for name in ["one.rs", "two.rs", "three.rs"] {
            write_file(&workspace.path().join(name));
        }
        let limits = ParseLimits {
            results: 2,
            ..ParseLimits::default()
        };

        let report = parse_task_problems_with_limits(
            "one.rs:1:1: error: one\ntwo.rs:1:1: error: two\nthree.rs:1:1: error: three\n",
            workspace.path(),
            workspace.path(),
            limits,
        )
        .unwrap();

        assert_eq!(report.problems.len(), 2);
        assert!(report.truncated);
    }

    #[test]
    fn caps_newest_lines_and_truncates_messages_on_utf8_boundaries() {
        let workspace = tempfile::tempdir().unwrap();
        for name in ["old.rs", "middle.rs", "new.rs"] {
            write_file(&workspace.path().join(name));
        }
        let limits = ParseLimits {
            lines: 2,
            message_bytes: 7,
            ..ParseLimits::default()
        };

        let report = parse_task_problems_with_limits(
            "old.rs:1:1: error: old\nmiddle.rs:1:1: warning: café-long\nnew.rs:1:1: note: newest\n",
            workspace.path(),
            workspace.path(),
            limits,
        )
        .unwrap();

        assert_eq!(report.problems.len(), 2);
        assert!(
            report.problems.iter().all(|problem| problem.path
                != fs::canonicalize(workspace.path().join("old.rs")).unwrap())
        );
        assert!(
            report
                .problems
                .iter()
                .all(|problem| problem.message.len() <= 7)
        );
        assert_eq!(report.problems[0].message, "caf…");
        assert!(report.truncated);
    }

    #[test]
    fn rejects_overlong_paths_lines_and_unbounded_or_non_csi_escapes() {
        let workspace = tempfile::tempdir().unwrap();
        write_file(&workspace.path().join("ok.rs"));
        let limits = ParseLimits {
            line_bytes: 48,
            path_bytes: 8,
            ansi_sequence_bytes: 8,
            ..ParseLimits::default()
        };
        let output = concat!(
            "a-very-long-name.rs:1:1: error: path\n",
            "ok.rs:1:1: error: this entire line is deliberately far too long to parse\n",
            "\x1b]0;title\x07ok.rs:1:1: error: osc\n",
            "\x1b[123456789mok.rs:1:1: error: long csi\n",
        );

        let report =
            parse_task_problems_with_limits(output, workspace.path(), workspace.path(), limits)
                .unwrap();

        assert!(report.problems.is_empty());
        assert!(report.truncated);
    }

    #[test]
    fn invalid_required_directories_are_reported() {
        let workspace = tempfile::tempdir().unwrap();
        let file = workspace.path().join("not-a-directory");
        fs::write(&file, "text").unwrap();

        let error = parse_task_problems("", &file, workspace.path()).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn skips_the_first_candidate_after_trim_and_drop_gaps() {
        let workspace = tempfile::tempdir().unwrap();
        for name in ["trim-tail.rs", "drop-tail.rs", "valid.rs"] {
            write_file(&workspace.path().join(name));
        }
        let output = concat!(
            "[… earlier task output trimmed …]\n",
            "trim-tail.rs:1:1: error: severed trim tail\n",
            "[… 128 output bytes dropped …]\n",
            "drop-tail.rs:1:1: error: severed pipe tail\n",
            "valid.rs:2:3: error: intact\n",
        );

        let report = parse_task_problems(output, workspace.path(), workspace.path()).unwrap();

        assert_eq!(report.problems.len(), 1);
        assert_eq!(
            report.problems[0].path,
            fs::canonicalize(workspace.path().join("valid.rs")).unwrap()
        );
        assert!(report.truncated);
    }
}
