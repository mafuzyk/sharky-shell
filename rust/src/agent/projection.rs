//! Runtime-side output projection (`strip_ansi`, `grep`, `context`, `head_lines`,
//! `tail_lines`, `max_bytes`) over captured task streams.
//!
//! Projection reduces token bloat for coding agents without hiding process
//! outcomes or true stream byte counts: raw byte counts remain truthful,
//! failures never become successes, and binary streams stay raw base64.

use std::borrow::Cow;

use super::capture::{Capture, CaptureContent};
use super::json::{Captured, ProjectionSummary, StreamData, Value, parse_line};
use super::protocol::{OutputParser, OutputSpec};
use super::render;
use super::store::StoredStream;

/// Maximum raw bytes retained per stream inside the session capture store and
/// read ahead for `output` projection (4 MiB).
pub(crate) const STORE_STREAM_CAP: usize = 4 * 1024 * 1024;

/// Per-stream projection summary metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StreamProjection {
    pub(crate) total_lines: usize,
    pub(crate) returned_lines: usize,
    pub(crate) matched_lines: Option<usize>,
}

pub(crate) fn build_projection_summary(
    stdout: Option<StreamProjection>,
    stderr: Option<StreamProjection>,
) -> Option<ProjectionSummary> {
    if stdout.is_none() && stderr.is_none() {
        return None;
    }
    Some(ProjectionSummary {
        stdout_total_lines: stdout.map(|p| p.total_lines),
        stdout_returned_lines: stdout.map(|p| p.returned_lines),
        stdout_matched_lines: stdout.and_then(|p| p.matched_lines),
        stderr_total_lines: stderr.map(|p| p.total_lines),
        stderr_returned_lines: stderr.map(|p| p.returned_lines),
        stderr_matched_lines: stderr.and_then(|p| p.matched_lines),
    })
}

/// Returns the largest index `<= max_bytes` that lands on a UTF-8 char boundary.
pub(crate) fn floor_char_boundary(text: &str, max_bytes: usize) -> usize {
    let mut end = max_bytes.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// Strips ANSI/VT escape sequences (CSI, OSC, and 2-byte ESC) cleanly.
pub(crate) fn strip_ansi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let mut run_start = 0;

    while i < bytes.len() {
        if bytes[i] != 0x1b {
            i += 1;
            continue;
        }
        out.extend_from_slice(&bytes[run_start..i]);
        i += 1;
        if i >= bytes.len() {
            run_start = i;
            break;
        }
        match bytes[i] {
            b'[' => {
                // CSI sequence: ESC [ params intermediates final_byte
                i += 1;
                while i < bytes.len() && (0x30..=0x3f).contains(&bytes[i]) {
                    i += 1;
                }
                while i < bytes.len() && (0x20..=0x2f).contains(&bytes[i]) {
                    i += 1;
                }
                if i < bytes.len() && (0x40..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
            }
            b']' => {
                // OSC sequence: ESC ] ... (BEL | ESC \)
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            }
            b' '..=b'/' => {
                i += 1;
                while i < bytes.len() && (0x20..=0x2f).contains(&bytes[i]) {
                    i += 1;
                }
                if i < bytes.len() && (0x30..=0x7e).contains(&bytes[i]) {
                    i += 1;
                }
            }
            0x30..=0x7e => {
                i += 1;
            }
            _ => {}
        }
        run_start = i;
    }

    if run_start < bytes.len() {
        out.extend_from_slice(&bytes[run_start..]);
    }
    // SAFETY: input was valid UTF-8 and we only removed ASCII escape sequences
    // bounded by ASCII bytes (`0x1b`..`0x7e`), so UTF-8 codepoints remain intact.
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

pub(crate) fn split_lines_lossless(text: &str) -> (Vec<&str>, bool) {
    let ended_with_nl = text.ends_with('\n');
    let lines: Vec<&str> = text.split_terminator('\n').collect();
    (lines, ended_with_nl)
}

/// Filters `lines` by regex `pattern`, merging `[m - context ..= m + context]`
/// windows without duplicating overlapping lines.
pub(crate) fn filter_lines_with_grep(
    lines: &[&str],
    pattern: &str,
    context: usize,
) -> Result<(Vec<String>, usize), String> {
    let regex = regex_lite::Regex::new(pattern).map_err(|err| format!("invalid regex: {err}"))?;
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut matched = 0usize;

    for (idx, line) in lines.iter().enumerate() {
        if regex.is_match(line) {
            matched += 1;
            let start = idx.saturating_sub(context);
            let end = idx.saturating_add(context).min(lines.len().saturating_sub(1));
            if let Some(last) = ranges.last_mut()
                && start <= last.1 + 1
            {
                last.1 = last.1.max(end);
            } else {
                ranges.push((start, end));
            }
        }
    }

    let mut out = Vec::new();
    for (start, end) in ranges {
        for line in &lines[start..=end] {
            out.push((*line).to_string());
        }
    }
    Ok((out, matched))
}

fn slice_head_tail(
    mut lines: Vec<String>,
    head: Option<usize>,
    tail: Option<usize>,
) -> Vec<String> {
    match (head, tail) {
        (Some(h), Some(t)) => {
            if h.saturating_add(t) >= lines.len() {
                lines
            } else {
                let omitted = lines.len() - h - t;
                let tail_start = lines.len() - t;
                let mut combined = Vec::with_capacity(h + 1 + t);
                combined.extend(lines[..h].iter().cloned());
                combined.push(format!("... ({omitted} lines omitted)"));
                combined.extend(lines[tail_start..].iter().cloned());
                combined
            }
        }
        (Some(h), None) => {
            lines.truncate(h);
            lines
        }
        (None, Some(t)) => {
            let start = lines.len().saturating_sub(t);
            lines.split_off(start)
        }
        (None, None) => lines,
    }
}

/// Applies an [`OutputSpec`] over raw bytes and accounts for the group/session budget.
pub(crate) fn project_stream(
    raw: &[u8],
    raw_total: u64,
    spec: &OutputSpec,
    max_capture_bytes: usize,
    budget: &mut usize,
    budget_truncated: &mut bool,
) -> (Captured, Option<StreamProjection>) {
    let stream_cap = spec
        .max_bytes
        .unwrap_or(usize::MAX)
        .min(max_capture_bytes);
    let budget_capped = *budget <= stream_cap;
    let effective_limit = stream_cap.min(*budget);

    let Ok(text) = std::str::from_utf8(raw) else {
        // Binary bytes are never text-filtered so arbitrary binary protocols stay intact.
        let slice_len = raw.len().min(effective_limit);
        if budget_capped && slice_len < raw.len() {
            *budget_truncated = true;
        }
        *budget = budget.saturating_sub(slice_len);
        return (
            Captured {
                data: StreamData::Base64(raw[..slice_len].to_vec()),
                bytes: raw_total as usize,
                truncated: slice_len as u64 != raw_total,
            },
            None,
        );
    };

    let cleaned: Cow<'_, str> = if spec.strip_ansi {
        Cow::Owned(strip_ansi(text))
    } else {
        Cow::Borrowed(text)
    };
    let (lines, ended_with_nl) = split_lines_lossless(&cleaned);
    let total_lines = lines.len();

    let (selected, matched_lines) = match &spec.grep {
        Some(pattern) => {
            match filter_lines_with_grep(&lines, pattern, spec.context) {
                Ok((matched_vec, count)) => (matched_vec, Some(count)),
                // Pattern was already validated at decode time.
                Err(_) => (Vec::new(), Some(0)),
            }
        }
        None => (
            lines.iter().map(|line| (*line).to_string()).collect(),
            None,
        ),
    };

    let sliced = slice_head_tail(selected, spec.head_lines, spec.tail_lines);
    let mut rejoined = if sliced.is_empty() {
        String::new()
    } else {
        let mut s = sliced.join("\n");
        if ended_with_nl {
            s.push('\n');
        }
        s
    };

    let byte_cut = floor_char_boundary(&rejoined, effective_limit);
    if budget_capped && byte_cut < rejoined.len() {
        *budget_truncated = true;
    }
    rejoined.truncate(byte_cut);
    *budget = budget.saturating_sub(rejoined.len());

    let returned_lines = if rejoined.is_empty() {
        0
    } else {
        rejoined.split_terminator('\n').count()
    };
    let truncated = (rejoined.len() as u64) != raw_total;

    (
        Captured {
            data: StreamData::Utf8(rejoined),
            bytes: raw_total as usize,
            truncated,
        },
        Some(StreamProjection {
            total_lines,
            returned_lines,
            matched_lines,
        }),
    )
}

/// Reads an unlinked [`Capture`] once and produces both the wire [`Captured`]
/// (projected if `spec` is set, bounded by `max_capture_bytes` and `budget`)
/// and the raw [`StoredStream`] for the session capture store.
pub(crate) fn read_capture_for_projection(
    capture: &Capture,
    spec: Option<&OutputSpec>,
    max_capture_bytes: usize,
    budget: &mut usize,
    budget_truncated: &mut bool,
) -> Result<(Captured, Option<StreamProjection>, StoredStream), String> {
    if let Some(spec) = spec {
        let captured = capture
            .read_back(STORE_STREAM_CAP)
            .map_err(|error| format!("cannot read a capture: {error}"))?;
        let raw_total = captured.total;
        let raw_bytes = match captured.content {
            CaptureContent::Text(text) => text.into_bytes(),
            CaptureContent::Binary(bytes) => bytes,
        };
        let (wire, summary) = project_stream(
            &raw_bytes,
            raw_total,
            spec,
            max_capture_bytes,
            budget,
            budget_truncated,
        );
        return Ok((
            wire,
            summary,
            StoredStream {
                bytes: raw_bytes,
                total_bytes: raw_total,
            },
        ));
    }

    let budget_capped = *budget <= max_capture_bytes;
    let limit = max_capture_bytes.min(*budget);
    let captured = capture
        .read_back(limit)
        .map_err(|error| format!("cannot read a capture: {error}"))?;
    if budget_capped && captured.truncated {
        *budget_truncated = true;
    }
    *budget = budget.saturating_sub(captured.byte_len());

    let store_bytes = if captured.truncated && captured.total > 0 {
        let full = capture
            .read_back(STORE_STREAM_CAP)
            .map_err(|error| format!("cannot read a capture: {error}"))?;
        match full.content {
            CaptureContent::Text(text) => text.into_bytes(),
            CaptureContent::Binary(bytes) => bytes,
        }
    } else {
        match &captured.content {
            CaptureContent::Text(text) => text.as_bytes().to_vec(),
            CaptureContent::Binary(bytes) => bytes.clone(),
        }
    };
    let raw_total = captured.total;
    let wire = render(captured);

    Ok((
        wire,
        None,
        StoredStream {
            bytes: store_bytes,
            total_bytes: raw_total,
        },
    ))
}

pub(crate) fn parse_structured_output(
    parser: OutputParser,
    raw_stdout: &[u8],
    raw_stderr: &[u8],
) -> Value {
    let stdout_lossy = String::from_utf8_lossy(raw_stdout);
    let stderr_lossy = String::from_utf8_lossy(raw_stderr);
    let stdout = strip_ansi(&stdout_lossy);
    let stderr = strip_ansi(&stderr_lossy);

    match parser {
        OutputParser::Lines => {
            let lines: Vec<Value> = stdout
                .lines()
                .filter(|line| !line.is_empty())
                .map(|line| Value::String(line.to_string()))
                .collect();
            let count = lines.len() as i64;
            Value::Object(vec![
                (String::from("count"), Value::Int(count)),
                (String::from("lines"), Value::Array(lines)),
            ])
        }
        OutputParser::Json => match parse_line(stdout.trim().as_bytes()) {
            Ok(value) => Value::Object(vec![
                (String::from("ok"), Value::Bool(true)),
                (String::from("value"), value),
            ]),
            Err(err) => Value::Object(vec![
                (String::from("ok"), Value::Bool(false)),
                (String::from("error"), Value::String(err.to_string())),
            ]),
        },
        OutputParser::GitStatus => parse_git_status(&stdout),
        OutputParser::Rg => parse_rg(&stdout),
        OutputParser::Cargo => parse_cargo(&stdout, &stderr),
        OutputParser::Pytest => parse_pytest(&stdout, &stderr),
    }
}

fn parse_git_status(stdout: &str) -> Value {
    let mut branch: Option<String> = None;
    let mut staged = Vec::new();
    let mut unstaged = Vec::new();
    let mut untracked = Vec::new();

    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            let name = rest.split("...").next().unwrap_or(rest).trim();
            if !name.is_empty() {
                branch = Some(name.to_string());
            }
            continue;
        }
        if line.len() < 3 {
            continue;
        }
        let bytes = line.as_bytes();
        let x = bytes[0] as char;
        let y = bytes[1] as char;
        if bytes[2] != b' ' {
            continue;
        }
        let path = line[3..].trim().to_string();
        if x == '?' && y == '?' {
            untracked.push(Value::String(path));
            continue;
        }
        if x != ' ' && x != '?' {
            staged.push(Value::Object(vec![
                (String::from("status"), Value::String(x.to_string())),
                (String::from("path"), Value::String(path.clone())),
            ]));
        }
        if y != ' ' && y != '?' {
            unstaged.push(Value::Object(vec![
                (String::from("status"), Value::String(y.to_string())),
                (String::from("path"), Value::String(path)),
            ]));
        }
    }

    Value::Object(vec![
        (
            String::from("branch"),
            branch.map(Value::String).unwrap_or(Value::Null),
        ),
        (String::from("staged"), Value::Array(staged)),
        (String::from("unstaged"), Value::Array(unstaged)),
        (String::from("untracked"), Value::Array(untracked)),
    ])
}

fn parse_rg(stdout: &str) -> Value {
    let mut matches = Vec::new();
    for line in stdout.lines() {
        let mut parts = line.splitn(4, ':');
        let Some(file) = parts.next() else {
            continue;
        };
        let Some(line_str) = parts.next() else {
            continue;
        };
        let Ok(line_no) = line_str.parse::<i64>() else {
            continue;
        };
        let Some(third) = parts.next() else {
            continue;
        };
        if let Some(fourth) = parts.next() {
            if let Ok(col_no) = third.parse::<i64>() {
                matches.push(Value::Object(vec![
                    (String::from("file"), Value::String(file.to_string())),
                    (String::from("line"), Value::Int(line_no)),
                    (String::from("col"), Value::Int(col_no)),
                    (String::from("text"), Value::String(fourth.to_string())),
                ]));
            } else {
                matches.push(Value::Object(vec![
                    (String::from("file"), Value::String(file.to_string())),
                    (String::from("line"), Value::Int(line_no)),
                    (String::from("text"), Value::String(format!("{third}:{fourth}"))),
                ]));
            }
        } else {
            matches.push(Value::Object(vec![
                (String::from("file"), Value::String(file.to_string())),
                (String::from("line"), Value::Int(line_no)),
                (String::from("text"), Value::String(third.to_string())),
            ]));
        }
    }
    let count = matches.len() as i64;
    Value::Object(vec![
        (String::from("count"), Value::Int(count)),
        (String::from("matches"), Value::Array(matches)),
    ])
}

fn parse_cargo(stdout: &str, stderr: &str) -> Value {
    let mut errors: Vec<Value> = Vec::new();
    let mut warnings: Vec<Value> = Vec::new();
    let mut failed_tests: Vec<Value> = Vec::new();
    let mut summaries: Vec<Value> = Vec::new();

    for source in [stderr, stdout] {
        let lines: Vec<&str> = source.lines().collect();
        for (idx, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("error[") || trimmed.starts_with("error:") {
                let mut obj = vec![(String::from("message"), Value::String(trimmed.to_string()))];
                if let Some(next) = lines.get(idx + 1).map(|l| l.trim())
                    && let Some(loc) = next.strip_prefix("--> ")
                {
                    obj.push((String::from("location"), Value::String(loc.trim().to_string())));
                }
                errors.push(Value::Object(obj));
            } else if trimmed.starts_with("warning:") {
                warnings.push(Value::String(trimmed.to_string()));
            } else if let Some(rest) = trimmed.strip_prefix("test ")
                && rest.ends_with(" ... FAILED")
            {
                let name = rest.strip_suffix(" ... FAILED").unwrap_or("").trim();
                if !name.is_empty() {
                    failed_tests.push(Value::String(name.to_string()));
                }
            } else if trimmed.starts_with("test result:") {
                summaries.push(Value::String(trimmed.to_string()));
            }
        }
    }

    Value::Object(vec![
        (String::from("errors"), Value::Array(errors)),
        (String::from("warnings"), Value::Array(warnings)),
        (String::from("failed_tests"), Value::Array(failed_tests)),
        (String::from("summaries"), Value::Array(summaries)),
    ])
}

fn parse_pytest(stdout: &str, stderr: &str) -> Value {
    let mut failed = Vec::new();
    let mut passed_count: i64 = 0;
    let mut failed_count: i64 = 0;
    let mut errors_count: i64 = 0;
    let mut skipped_count: i64 = 0;
    let mut summary: Option<String> = None;

    for source in [stdout, stderr] {
        for line in source.lines() {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("FAILED ") {
                let node = rest.split(" - ").next().unwrap_or(rest).trim();
                if !node.is_empty() {
                    failed.push(Value::String(node.to_string()));
                }
            }
            if (trimmed.starts_with("===") && trimmed.ends_with("==="))
                && (trimmed.contains(" passed")
                    || trimmed.contains(" failed")
                    || trimmed.contains(" error")
                    || trimmed.contains(" skipped"))
            {
                let inner = trimmed.trim_matches('=').trim().to_string();
                for part in inner.split(',') {
                    let tokens: Vec<&str> = part.split_whitespace().collect();
                    if tokens.len() >= 2
                        && let Ok(n) = tokens[0].parse::<i64>()
                    {
                        match tokens[1] {
                            "passed" => passed_count = n,
                            "failed" => failed_count = n,
                            "error" | "errors" => errors_count = n,
                            "skipped" => skipped_count = n,
                            _ => {}
                        }
                    }
                }
                summary = Some(inner);
            }
        }
    }

    Value::Object(vec![
        (String::from("failed"), Value::Array(failed)),
        (String::from("passed"), Value::Int(passed_count)),
        (String::from("failed_count"), Value::Int(failed_count)),
        (String::from("errors"), Value::Int(errors_count)),
        (String::from("skipped"), Value::Int(skipped_count)),
        (
            String::from("summary"),
            summary.map(Value::String).unwrap_or(Value::Null),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_csi_osc_and_two_byte_escapes_without_breaking_utf8() {
        let raw = "\x1b[31merr\x1b[0m: café \x1b]0;title\x07ok \x1b=done\n";
        assert_eq!(strip_ansi(raw), "err: café ok done\n");
    }

    #[test]
    fn project_stream_handles_grep_context_head_tail_and_utf8_boundaries() {
        let text = "a1\nmatch1\na2\na3\nmatch2\na4\n";
        let spec = OutputSpec {
            strip_ansi: false,
            grep: Some("match".to_string()),
            context: 1,
            head_lines: None,
            tail_lines: None,
            max_bytes: None,
            parse: None,
        };
        let mut budget = 1024;
        let mut btrunc = false;
        let (cap, summary) = project_stream(
            text.as_bytes(),
            text.len() as u64,
            &spec,
            1024,
            &mut budget,
            &mut btrunc,
        );
        assert_eq!(cap.data, StreamData::Utf8(text.to_string()));
        assert!(!cap.truncated);
        assert!(!btrunc);
        assert_eq!(
            summary,
            Some(StreamProjection {
                total_lines: 6,
                returned_lines: 6,
                matched_lines: Some(2),
            })
        );

        let lines = (1..=10).map(|i| format!("L{i}\n")).collect::<String>();
        let ht = OutputSpec {
            strip_ansi: false,
            grep: None,
            context: 0,
            head_lines: Some(2),
            tail_lines: Some(2),
            max_bytes: None,
            parse: None,
        };
        let mut budget = 1024;
        let (cap, _) = project_stream(
            lines.as_bytes(),
            lines.len() as u64,
            &ht,
            1024,
            &mut budget,
            &mut btrunc,
        );
        assert_eq!(
            cap.data,
            StreamData::Utf8("L1\nL2\n... (6 lines omitted)\nL9\nL10\n".to_string())
        );

        let utf8 = "aaéé";
        let cut = OutputSpec {
            strip_ansi: false,
            grep: None,
            context: 0,
            head_lines: None,
            tail_lines: None,
            max_bytes: Some(3),
            parse: None,
        };
        let mut budget = 1024;
        let (cap, _) = project_stream(
            utf8.as_bytes(),
            utf8.len() as u64,
            &cut,
            1024,
            &mut budget,
            &mut btrunc,
        );
        assert_eq!(cap.data, StreamData::Utf8("aa".to_string()));
        assert_eq!(cap.bytes, utf8.len());
        assert!(cap.truncated);
    }
}
