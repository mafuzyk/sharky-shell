//! Bounded FIFO in-memory session capture store for `op:"read_capture"`.
//!
//! Stores raw bytes (zero persistent file descriptors) from finished requests
//! so projected or truncated captures can be inspected cleanly on demand.

use std::borrow::Cow;
use std::collections::VecDeque;

use super::json::{CaptureResult, ErrorCode, Limits, StreamData};
use super::projection;
use super::protocol::{CaptureStream, ReadCaptureRequest};

#[derive(Debug, Clone)]
pub(crate) struct StoredStream {
    pub(crate) bytes: Vec<u8>,
    pub(crate) total_bytes: u64,
}

struct StoredEntry {
    id: String,
    stdout: StoredStream,
    stderr: StoredStream,
    bytes: usize,
}

pub(crate) struct CaptureStore {
    entries: VecDeque<StoredEntry>,
    next_id: u64,
    total_bytes: usize,
    max_entries: usize,
    max_bytes: usize,
}

impl CaptureStore {
    pub(crate) fn new(limits: &Limits) -> Self {
        Self {
            entries: VecDeque::new(),
            next_id: 0,
            total_bytes: 0,
            max_entries: limits.max_capture_store_entries,
            max_bytes: limits.max_capture_store_bytes,
        }
    }

    /// Stores raw stdout and stderr streams when at least one is non-empty,
    /// evicting oldest entries FIFO to respect entry-count and byte caps.
    pub(crate) fn record(
        &mut self,
        mut stdout: StoredStream,
        mut stderr: StoredStream,
    ) -> Option<String> {
        if stdout.total_bytes == 0 && stderr.total_bytes == 0 {
            return None;
        }
        if self.max_entries == 0 || self.max_bytes == 0 {
            return None;
        }

        let per_stream_cap = (self.max_bytes / 2).max(1);
        if stdout.bytes.len() > per_stream_cap {
            stdout.bytes.truncate(per_stream_cap);
        }
        if stderr.bytes.len() > per_stream_cap {
            stderr.bytes.truncate(per_stream_cap);
        }

        let entry_bytes = stdout.bytes.len() + stderr.bytes.len();
        while !self.entries.is_empty()
            && (self.entries.len() >= self.max_entries
                || self.total_bytes.saturating_add(entry_bytes) > self.max_bytes)
        {
            if let Some(evicted) = self.entries.pop_front() {
                self.total_bytes = self.total_bytes.saturating_sub(evicted.bytes);
            }
        }
        // Single entry larger than total store capacity still cannot exceed `max_bytes`
        // because `per_stream_cap * 2 <= max_bytes` (for `max_bytes >= 2`).
        if entry_bytes > self.max_bytes {
            return None;
        }

        self.next_id += 1;
        let id = format!("cap-{}", self.next_id);
        self.total_bytes = self.total_bytes.saturating_add(entry_bytes);
        self.entries.push_back(StoredEntry {
            id: id.clone(),
            stdout,
            stderr,
            bytes: entry_bytes,
        });
        Some(id)
    }

    /// Queries one stored stream by `capture_id`, supporting line paging + grep
    /// + ANSI stripping on UTF-8 text or raw byte slicing via `byte_offset`.
    pub(crate) fn read(
        &self,
        request: &ReadCaptureRequest,
        limits: &Limits,
    ) -> Result<CaptureResult, (ErrorCode, String)> {
        let Some(entry) = self.entries.iter().find(|entry| entry.id == request.capture_id) else {
            return Err((
                ErrorCode::CaptureNotFound,
                format!(
                    "capture {} was not found or has been evicted",
                    request.capture_id
                ),
            ));
        };

        let stream = match request.stream {
            CaptureStream::Stdout => &entry.stdout,
            CaptureStream::Stderr => &entry.stderr,
        };
        let stored_bytes = stream.bytes.len();
        let total_bytes = stream.total_bytes as usize;
        let default_limit = 65_536.min(limits.max_capture_bytes);
        let byte_limit = request.max_bytes.unwrap_or(default_limit);

        if let Some(offset) = request.byte_offset {
            let offset = offset.min(stored_bytes);
            let end = offset.saturating_add(byte_limit).min(stored_bytes);
            let slice = stream.bytes[offset..end].to_vec();
            let data = match String::from_utf8(slice) {
                Ok(text) => StreamData::Utf8(text),
                Err(err) => StreamData::Base64(err.into_bytes()),
            };
            let truncated = end < stored_bytes || (stored_bytes as u64) < stream.total_bytes;
            return Ok(CaptureResult {
                id: request.id.clone(),
                capture_id: request.capture_id.clone(),
                stream: request.stream.as_str().to_string(),
                data,
                total_bytes,
                stored_bytes,
                total_lines: None,
                line_start: None,
                returned_lines: None,
                matched_lines: None,
                byte_offset: Some(offset),
                truncated,
            });
        }

        let has_line_filters = request.line_start.is_some()
            || request.line_count.is_some()
            || request.grep.is_some()
            || request.strip_ansi;

        let text = match std::str::from_utf8(&stream.bytes) {
            Ok(text) => text,
            Err(err) => {
                // If capture truncation at the tail clipped a multibyte char,
                // trim only that trailing incomplete codepoint.
                if (stored_bytes as u64) < stream.total_bytes
                    && stored_bytes.saturating_sub(err.valid_up_to()) <= 3
                    && std::str::from_utf8(&stream.bytes[..err.valid_up_to()]).is_ok()
                {
                    std::str::from_utf8(&stream.bytes[..err.valid_up_to()]).unwrap()
                } else if has_line_filters {
                    return Err((
                        ErrorCode::InvalidRequest,
                        String::from(
                            "cannot apply line filters or strip_ansi to binary capture; use byte_offset",
                        ),
                    ));
                } else {
                    let end = stored_bytes.min(byte_limit);
                    let truncated = end < stored_bytes || (stored_bytes as u64) < stream.total_bytes;
                    return Ok(CaptureResult {
                        id: request.id.clone(),
                        capture_id: request.capture_id.clone(),
                        stream: request.stream.as_str().to_string(),
                        data: StreamData::Base64(stream.bytes[..end].to_vec()),
                        total_bytes,
                        stored_bytes,
                        total_lines: None,
                        line_start: None,
                        returned_lines: None,
                        matched_lines: None,
                        byte_offset: Some(0),
                        truncated,
                    });
                }
            }
        };

        let cleaned: Cow<'_, str> = if request.strip_ansi {
            Cow::Owned(projection::strip_ansi(text))
        } else {
            Cow::Borrowed(text)
        };
        let (lines, ended_with_nl) = projection::split_lines_lossless(&cleaned);
        let total_lines = lines.len();

        let (selected, matched_lines) = match &request.grep {
            Some(pattern) => {
                let (matched_vec, count) =
                    projection::filter_lines_with_grep(&lines, pattern, request.context)
                        .map_err(|msg| (ErrorCode::InvalidRequest, msg))?;
                (matched_vec, Some(count))
            }
            None => (lines.iter().map(|line| (*line).to_string()).collect(), None),
        };

        let line_start = request.line_start.unwrap_or(1);
        let start_idx = line_start - 1;
        let available = if start_idx >= selected.len() {
            &[]
        } else {
            &selected[start_idx..]
        };
        let count = request
            .line_count
            .unwrap_or(available.len())
            .min(available.len());
        let window = &available[..count];

        let mut joined = if window.is_empty() {
            String::new()
        } else {
            let mut s = window.join("\n");
            let has_trailing_nl = start_idx + count < selected.len() || ended_with_nl;
            if has_trailing_nl {
                s.push('\n');
            }
            s
        };

        let cut = projection::floor_char_boundary(&joined, byte_limit);
        let cut_bytes = cut < joined.len();
        joined.truncate(cut);

        let returned_lines = if joined.is_empty() {
            0
        } else {
            joined.split_terminator('\n').count()
        };
        let truncated = cut_bytes
            || (start_idx + count < selected.len())
            || ((stored_bytes as u64) < stream.total_bytes);

        Ok(CaptureResult {
            id: request.id.clone(),
            capture_id: request.capture_id.clone(),
            stream: request.stream.as_str().to_string(),
            data: StreamData::Utf8(joined),
            total_bytes,
            stored_bytes,
            total_lines: Some(total_lines),
            line_start: Some(line_start),
            returned_lines: Some(returned_lines),
            matched_lines,
            byte_offset: None,
            truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(bytes: &[u8]) -> StoredStream {
        StoredStream {
            bytes: bytes.to_vec(),
            total_bytes: bytes.len() as u64,
        }
    }

    #[test]
    fn capture_store_skips_empty_and_evicts_fifo_by_count_and_bytes() {
        let limits = Limits {
            max_capture_store_entries: 2,
            max_capture_store_bytes: 20,
            ..Limits::default()
        };
        let mut store = CaptureStore::new(&limits);

        assert_eq!(store.record(stream(b""), stream(b"")), None);
        assert_eq!(store.record(stream(b"12345"), stream(b"")).as_deref(), Some("cap-1"));
        assert_eq!(store.record(stream(b"abcde"), stream(b"")).as_deref(), Some("cap-2"));
        // Third entry evicts cap-1 by entry count.
        assert_eq!(store.record(stream(b"zzzzz"), stream(b"")).as_deref(), Some("cap-3"));

        let missing = store
            .read(
                &ReadCaptureRequest {
                    id: "r".into(),
                    capture_id: "cap-1".into(),
                    stream: CaptureStream::Stdout,
                    line_start: None,
                    line_count: None,
                    byte_offset: None,
                    max_bytes: None,
                    grep: None,
                    context: 0,
                    strip_ansi: false,
                },
                &limits,
            )
            .expect_err("cap-1 should be evicted");
        assert_eq!(missing.0, ErrorCode::CaptureNotFound);

        // Large entry evicts cap-2/cap-3 by byte budget after per-stream cap (10).
        assert_eq!(
            store
                .record(stream(b"0123456789012345"), stream(b"")),
            Some("cap-4".to_string())
        );
        let live = store
            .read(
                &ReadCaptureRequest {
                    id: "r".into(),
                    capture_id: "cap-4".into(),
                    stream: CaptureStream::Stdout,
                    line_start: None,
                    line_count: None,
                    byte_offset: Some(0),
                    max_bytes: Some(10),
                    grep: None,
                    context: 0,
                    strip_ansi: false,
                },
                &limits,
            )
            .expect("cap-4 present");
        assert_eq!(live.data, StreamData::Utf8("0123456789".to_string()));
        assert!(live.truncated);
    }
}
