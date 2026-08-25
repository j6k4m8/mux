use std::io::{self, Write};

use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

pub(crate) const MAX_CURSOR_BYTES: usize = 256;
pub(crate) const BOOTSTRAP_BYTES: usize = 512 * 1024;
pub(crate) const THREAD_PAGE_BYTES: usize = 3 * 1024 * 1024;
pub(crate) const SEARCH_PAGE_BYTES: usize = 3 * 1024 * 1024;
pub(crate) const MESSAGE_DETAIL_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const ATTACHMENT_CONTENT_BYTES: usize = 28 * 1024 * 1024;
pub(crate) const ACTIVITY_BYTES: usize = 256 * 1024;
pub(crate) const PROVIDER_BATCH_BYTES: usize = 32 * 1024 * 1024;

const CURSOR_VERSION: &str = "mx1";
const DIGEST_BYTES: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CursorKind {
    Thread,
    Message,
    Search,
}

impl CursorKind {
    fn wire(self) -> &'static str {
        match self {
            Self::Thread => "t",
            Self::Message => "m",
            Self::Search => "s",
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Thread => "Thread",
            Self::Message => "Message",
            Self::Search => "Search",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CursorPosition {
    pub(crate) sort_timestamp: i64,
    pub(crate) row_id: i64,
    pub(crate) snapshot_at: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IpcPayloadKind {
    Bootstrap,
    ThreadPage,
    SearchPage,
    MessageDetail,
    AttachmentContent,
    Activity,
    ProviderBatch,
}

impl IpcPayloadKind {
    pub(crate) fn limit(self) -> usize {
        match self {
            Self::Bootstrap => BOOTSTRAP_BYTES,
            Self::ThreadPage => THREAD_PAGE_BYTES,
            Self::SearchPage => SEARCH_PAGE_BYTES,
            Self::MessageDetail => MESSAGE_DETAIL_BYTES,
            Self::AttachmentContent => ATTACHMENT_CONTENT_BYTES,
            Self::Activity => ACTIVITY_BYTES,
            Self::ProviderBatch => PROVIDER_BATCH_BYTES,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Bootstrap => "Mailbox bootstrap",
            Self::ThreadPage => "Thread page",
            Self::SearchPage => "Search page",
            Self::MessageDetail => "Message detail",
            Self::AttachmentContent => "Attachment content",
            Self::Activity => "Activity",
            Self::ProviderBatch => "Provider batch",
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum IpcBoundaryError {
    #[error("{label} exceeds the {limit}-byte IPC limit")]
    Limit { label: &'static str, limit: usize },
    #[error("{0} cursor is invalid")]
    InvalidCursor(&'static str),
    #[error("IPC payload could not be serialized")]
    Serialization,
}

struct CappedWriter {
    length: usize,
    limit: usize,
    exceeded: bool,
}

impl Write for CappedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let next = self.length.saturating_add(bytes.len());
        if next > self.limit {
            self.exceeded = true;
            return Err(io::Error::other("IPC byte limit exceeded"));
        }
        self.length = next;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn ensure_serialized_budget<T: Serialize>(
    value: &T,
    kind: IpcPayloadKind,
) -> Result<usize, IpcBoundaryError> {
    ensure_serialized_with_limit(value, kind.label(), kind.limit())
}

fn ensure_serialized_with_limit<T: Serialize>(
    value: &T,
    label: &'static str,
    limit: usize,
) -> Result<usize, IpcBoundaryError> {
    let mut writer = CappedWriter {
        length: 0,
        limit,
        exceeded: false,
    };
    if serde_json::to_writer(&mut writer, value).is_err() {
        return if writer.exceeded {
            Err(IpcBoundaryError::Limit { label, limit })
        } else {
            Err(IpcBoundaryError::Serialization)
        };
    }
    Ok(writer.length)
}

pub(crate) fn canonical_scope(parts: &[&str]) -> String {
    let mut scope = String::new();
    for part in parts {
        scope.push_str(&part.len().to_string());
        scope.push(':');
        scope.push_str(part);
        scope.push('|');
    }
    scope
}

pub(crate) fn encode_cursor(
    signing_key: &[u8; 32],
    kind: CursorKind,
    scope: &str,
    position: CursorPosition,
) -> Result<String, IpcBoundaryError> {
    if position.sort_timestamp < 0 || position.row_id < 1 || position.snapshot_at < 0 {
        return Err(IpcBoundaryError::InvalidCursor(kind.label()));
    }
    let scope_digest = digest_hex(scope.as_bytes());
    let payload = format!(
        "{CURSOR_VERSION}:{}:{}:{}:{}:{scope_digest}",
        kind.wire(),
        position.sort_timestamp,
        position.row_id,
        position.snapshot_at
    );
    let signature = signature_hex(signing_key, payload.as_bytes());
    let cursor = format!("{payload}:{signature}");
    if cursor.len() > MAX_CURSOR_BYTES {
        return Err(IpcBoundaryError::InvalidCursor(kind.label()));
    }
    Ok(cursor)
}

pub(crate) fn decode_cursor(
    signing_key: &[u8; 32],
    kind: CursorKind,
    scope: &str,
    cursor: &str,
) -> Result<CursorPosition, IpcBoundaryError> {
    if cursor.is_empty() || cursor.len() > MAX_CURSOR_BYTES || !cursor.is_ascii() {
        return Err(IpcBoundaryError::InvalidCursor(kind.label()));
    }
    let fields = cursor.split(':').collect::<Vec<_>>();
    if fields.len() != 7 || fields[0] != CURSOR_VERSION || fields[1] != kind.wire() {
        return Err(IpcBoundaryError::InvalidCursor(kind.label()));
    }
    let payload = fields[..6].join(":");
    let expected_signature = signature_hex(signing_key, payload.as_bytes());
    if !constant_time_eq(fields[6].as_bytes(), expected_signature.as_bytes())
        || !constant_time_eq(
            fields[5].as_bytes(),
            digest_hex(scope.as_bytes()).as_bytes(),
        )
    {
        return Err(IpcBoundaryError::InvalidCursor(kind.label()));
    }
    let position = CursorPosition {
        sort_timestamp: parse_nonnegative(fields[2], kind)?,
        row_id: parse_positive(fields[3], kind)?,
        snapshot_at: parse_nonnegative(fields[4], kind)?,
    };
    Ok(position)
}

fn parse_nonnegative(value: &str, kind: CursorKind) -> Result<i64, IpcBoundaryError> {
    value
        .parse::<i64>()
        .ok()
        .filter(|value| *value >= 0)
        .ok_or(IpcBoundaryError::InvalidCursor(kind.label()))
}

fn parse_positive(value: &str, kind: CursorKind) -> Result<i64, IpcBoundaryError> {
    value
        .parse::<i64>()
        .ok()
        .filter(|value| *value >= 1)
        .ok_or(IpcBoundaryError::InvalidCursor(kind.label()))
}

fn digest_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex_prefix(&digest, DIGEST_BYTES)
}

fn signature_hex(key: &[u8; 32], payload: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(b"mux-ipc-cursor-signature-v1\0");
    hash.update(key);
    hash.update(payload);
    hash.update(key);
    hex_prefix(&hash.finalize(), DIGEST_BYTES)
}

fn hex_prefix(bytes: &[u8], length: usize) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(length * 2);
    for byte in bytes.iter().take(length) {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actual_json_size_is_accepted_at_the_cap_and_rejected_one_byte_over() {
        let value = "x".repeat(64);
        let actual = serde_json::to_vec(&value).unwrap().len();
        assert_eq!(
            ensure_serialized_with_limit(&value, "Test", actual),
            Ok(actual)
        );
        assert_eq!(
            ensure_serialized_with_limit(&value, "Test", actual - 1),
            Err(IpcBoundaryError::Limit {
                label: "Test",
                limit: actual - 1,
            })
        );
    }

    #[test]
    fn cursor_is_short_scoped_versioned_and_integrity_checked() {
        let key = [7_u8; 32];
        let scope = canonical_scope(&["all", "inbox"]);
        let position = CursorPosition {
            sort_timestamp: i64::MAX,
            row_id: i64::MAX,
            snapshot_at: i64::MAX,
        };
        let cursor = encode_cursor(&key, CursorKind::Thread, &scope, position).unwrap();
        assert!(cursor.len() <= MAX_CURSOR_BYTES);
        assert_eq!(
            decode_cursor(&key, CursorKind::Thread, &scope, &cursor),
            Ok(position)
        );
        assert!(decode_cursor(
            &key,
            CursorKind::Thread,
            &canonical_scope(&["acc_work", "inbox"]),
            &cursor
        )
        .is_err());
        assert!(decode_cursor(&key, CursorKind::Search, &scope, &cursor).is_err());

        let mut tampered = cursor.into_bytes();
        let index = tampered.iter().position(|byte| *byte == b'7').unwrap();
        tampered[index] = b'8';
        assert!(decode_cursor(
            &key,
            CursorKind::Thread,
            &scope,
            std::str::from_utf8(&tampered).unwrap()
        )
        .is_err());
    }
}
