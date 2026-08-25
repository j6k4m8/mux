//! Bounded RFC 5322/MIME ingestion.
//!
//! `mail-parser` owns standards parsing, transfer decoding, RFC 2047/2231
//! handling, and charset conversion. This module owns Mux's trust boundary:
//! it rejects messages whose raw or decoded representation exceeds an
//! explicit budget before anything reaches SQLite or the renderer.

use std::collections::BTreeSet;
use std::panic::{catch_unwind, AssertUnwindSafe};

use mail_parser::{Message, MessageParser, MimeHeaders, PartType};
use thiserror::Error;

pub(crate) const MAX_RAW_MESSAGE_BYTES: usize = 32 * 1024 * 1024;
pub(crate) const MAX_MIME_DEPTH: usize = 16;
pub(crate) const MAX_PART_COUNT: usize = 256;
pub(crate) const MAX_HEADERS_PER_PART: usize = 200;
pub(crate) const MAX_TOTAL_HEADER_COUNT: usize = 2_000;
pub(crate) const MAX_HEADER_BYTES_PER_PART: usize = 128 * 1024;
pub(crate) const MAX_TOTAL_HEADER_BYTES: usize = 512 * 1024;
pub(crate) const MAX_HEADER_LINE_BYTES: usize = 16 * 1024;
pub(crate) const MAX_DECODED_TEXT_PART_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_TOTAL_DECODED_TEXT_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;
pub(crate) const MAX_TOTAL_ATTACHMENT_BYTES: usize = 24 * 1024 * 1024;
pub(crate) const MAX_TOTAL_DECODED_BYTES: usize = 32 * 1024 * 1024;
pub(crate) const MAX_ATTACHMENT_COUNT: usize = 64;
pub(crate) const MAX_FILENAME_BYTES: usize = 1_024;
pub(crate) const MAX_MEDIA_TYPE_BYTES: usize = 127;
pub(crate) const MAX_CONTENT_ID_BYTES: usize = 998;
pub(crate) const MAX_SUBJECT_BYTES: usize = 4 * 1024;
pub(crate) const MAX_ADDRESS_COUNT: usize = 256;
pub(crate) const MAX_ADDRESS_NAME_BYTES: usize = 1_024;
pub(crate) const MAX_ADDRESS_BYTES: usize = 320;
pub(crate) const MAX_TOTAL_ENVELOPE_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy)]
struct IngestLimits {
    raw_message_bytes: usize,
    mime_depth: usize,
    part_count: usize,
    headers_per_part: usize,
    total_header_count: usize,
    header_bytes_per_part: usize,
    total_header_bytes: usize,
    header_line_bytes: usize,
    decoded_text_part_bytes: usize,
    total_decoded_text_bytes: usize,
    attachment_bytes: usize,
    total_attachment_bytes: usize,
    total_decoded_bytes: usize,
    attachment_count: usize,
    filename_bytes: usize,
    media_type_bytes: usize,
    content_id_bytes: usize,
    subject_bytes: usize,
    address_count: usize,
    address_name_bytes: usize,
    address_bytes: usize,
    total_envelope_bytes: usize,
}

const PRODUCTION_LIMITS: IngestLimits = IngestLimits {
    raw_message_bytes: MAX_RAW_MESSAGE_BYTES,
    mime_depth: MAX_MIME_DEPTH,
    part_count: MAX_PART_COUNT,
    headers_per_part: MAX_HEADERS_PER_PART,
    total_header_count: MAX_TOTAL_HEADER_COUNT,
    header_bytes_per_part: MAX_HEADER_BYTES_PER_PART,
    total_header_bytes: MAX_TOTAL_HEADER_BYTES,
    header_line_bytes: MAX_HEADER_LINE_BYTES,
    decoded_text_part_bytes: MAX_DECODED_TEXT_PART_BYTES,
    total_decoded_text_bytes: MAX_TOTAL_DECODED_TEXT_BYTES,
    attachment_bytes: MAX_ATTACHMENT_BYTES,
    total_attachment_bytes: MAX_TOTAL_ATTACHMENT_BYTES,
    total_decoded_bytes: MAX_TOTAL_DECODED_BYTES,
    attachment_count: MAX_ATTACHMENT_COUNT,
    filename_bytes: MAX_FILENAME_BYTES,
    media_type_bytes: MAX_MEDIA_TYPE_BYTES,
    content_id_bytes: MAX_CONTENT_ID_BYTES,
    subject_bytes: MAX_SUBJECT_BYTES,
    address_count: MAX_ADDRESS_COUNT,
    address_name_bytes: MAX_ADDRESS_NAME_BYTES,
    address_bytes: MAX_ADDRESS_BYTES,
    total_envelope_bytes: MAX_TOTAL_ENVELOPE_BYTES,
};

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum MimeIngestError {
    #[error("MIME ingestion rejected: raw message byte limit exceeded")]
    RawMessageBytes,
    #[error("MIME ingestion rejected: malformed message structure")]
    MalformedStructure,
    #[error("MIME ingestion rejected: nesting depth limit exceeded")]
    NestingDepth,
    #[error("MIME ingestion rejected: part count limit exceeded")]
    PartCount,
    #[error("MIME ingestion rejected: header count limit exceeded")]
    HeaderCount,
    #[error("MIME ingestion rejected: header byte limit exceeded")]
    HeaderBytes,
    #[error("MIME ingestion rejected: decoded text byte limit exceeded")]
    DecodedTextBytes,
    #[error("MIME ingestion rejected: attachment count limit exceeded")]
    AttachmentCount,
    #[error("MIME ingestion rejected: attachment byte limit exceeded")]
    AttachmentBytes,
    #[error("MIME ingestion rejected: aggregate decoded byte limit exceeded")]
    TotalDecodedBytes,
    #[error("MIME ingestion rejected: attachment metadata limit exceeded")]
    AttachmentMetadata,
    #[error("MIME ingestion rejected: decoded header projection limit exceeded")]
    HeaderProjection,
    #[error("MIME ingestion rejected: malformed transfer encoding")]
    TransferEncoding,
    #[error("MIME ingestion rejected: parser failed safely")]
    ParserFailure,
}

pub(crate) struct IngestedAttachment {
    pub(crate) filename: String,
    pub(crate) media_type: String,
    pub(crate) bytes: Vec<u8>,
    pub(crate) content_id: String,
    pub(crate) disposition: String,
}

pub(crate) struct IngestedMailbox {
    pub(crate) name: String,
    pub(crate) address: String,
}

struct ThreadingHeaders {
    internet_message_id: Option<String>,
    in_reply_to: Option<String>,
    references: Vec<String>,
}

pub(crate) struct IngestedMime {
    pub(crate) subject: String,
    pub(crate) from: Vec<IngestedMailbox>,
    pub(crate) reply_to: Vec<IngestedMailbox>,
    pub(crate) to: Vec<IngestedMailbox>,
    pub(crate) cc: Vec<IngestedMailbox>,
    pub(crate) internet_message_id: Option<String>,
    pub(crate) in_reply_to: Option<String>,
    pub(crate) references: Vec<String>,
    pub(crate) plain: Option<String>,
    pub(crate) html: Option<String>,
    pub(crate) attachments: Vec<IngestedAttachment>,
}

pub(crate) fn ingest_mime(raw: &[u8]) -> Result<IngestedMime, MimeIngestError> {
    ingest_with_limits(raw, PRODUCTION_LIMITS)
}

fn ingest_with_limits(raw: &[u8], limits: IngestLimits) -> Result<IngestedMime, MimeIngestError> {
    if raw.len() > limits.raw_message_bytes {
        return Err(MimeIngestError::RawMessageBytes);
    }
    preflight_root_headers(raw, limits)?;

    // Upstream promises a non-panicking best-effort parser and is safe Rust,
    // but this is a hostile-content boundary. Convert an unexpected unwind
    // into a content rejection instead of letting it cross the application.
    let message = catch_unwind(AssertUnwindSafe(|| MessageParser::default().parse(raw)))
        .map_err(|_| MimeIngestError::ParserFailure)?
        .ok_or(MimeIngestError::MalformedStructure)?;

    let mut budget = ValidationBudget::default();
    validate_message(&message, 0, &mut budget, limits)?;
    project_message(&message, limits)
}

#[derive(Default)]
struct ValidationBudget {
    parts: usize,
    headers: usize,
    header_bytes: usize,
    decoded_text_bytes: usize,
    decoded_bytes: usize,
    attachments: usize,
    attachment_bytes: usize,
}

fn validate_message(
    message: &Message<'_>,
    depth: usize,
    budget: &mut ValidationBudget,
    limits: IngestLimits,
) -> Result<(), MimeIngestError> {
    if depth > limits.mime_depth {
        return Err(MimeIngestError::NestingDepth);
    }
    if message.parts.is_empty() {
        return Err(MimeIngestError::MalformedStructure);
    }
    budget.parts = checked_add(budget.parts, message.parts.len())?;
    if budget.parts > limits.part_count {
        return Err(MimeIngestError::PartCount);
    }

    for part in &message.parts {
        validate_part_offsets(
            message,
            part.offset_header,
            part.offset_body,
            part.offset_end,
        )?;
        let header_start = part.offset_header as usize;
        let body_start = part.offset_body as usize;
        let (header_count, header_bytes) = validate_header_block(
            &message.raw_message.as_ref()[header_start..body_start],
            false,
            limits,
        )?;
        budget.headers = checked_add(budget.headers, header_count)?;
        budget.header_bytes = checked_add(budget.header_bytes, header_bytes)?;
        if header_count > limits.headers_per_part || budget.headers > limits.total_header_count {
            return Err(MimeIngestError::HeaderCount);
        }
        if header_bytes > limits.header_bytes_per_part
            || budget.header_bytes > limits.total_header_bytes
        {
            return Err(MimeIngestError::HeaderBytes);
        }
        if is_composite_part(part) && !has_identity_transfer_encoding(part) {
            return Err(MimeIngestError::TransferEncoding);
        }
        if part.is_encoding_problem && !recoverable_encoding_problem(part) {
            return Err(MimeIngestError::TransferEncoding);
        }
        if part
            .content_type()
            .is_some_and(|value| value.ctype().eq_ignore_ascii_case("multipart"))
            && !matches!(part.body, PartType::Multipart(_))
        {
            return Err(MimeIngestError::MalformedStructure);
        }

        match &part.body {
            PartType::Text(text) | PartType::Html(text) => {
                let length = text.len();
                if length > limits.decoded_text_part_bytes {
                    return Err(MimeIngestError::DecodedTextBytes);
                }
                budget.decoded_text_bytes = checked_add(budget.decoded_text_bytes, length)?;
                if budget.decoded_text_bytes > limits.total_decoded_text_bytes {
                    return Err(MimeIngestError::DecodedTextBytes);
                }
                add_decoded_bytes(budget, length, limits)?;
            }
            PartType::Binary(bytes) | PartType::InlineBinary(bytes) => {
                add_decoded_bytes(budget, bytes.len(), limits)?;
            }
            PartType::Message(_) | PartType::Multipart(_) => {}
        }
    }

    let mut attachment_ids = BTreeSet::new();
    for &part_id in &message.attachments {
        if !attachment_ids.insert(part_id) {
            return Err(MimeIngestError::MalformedStructure);
        }
        let part = message
            .parts
            .get(part_id as usize)
            .ok_or(MimeIngestError::MalformedStructure)?;
        if recoverable_truncated_body(part) {
            continue;
        }
        budget.attachments = checked_add(budget.attachments, 1)?;
        if budget.attachments > limits.attachment_count {
            return Err(MimeIngestError::AttachmentCount);
        }
        let length = part.contents().len();
        if length > limits.attachment_bytes {
            return Err(MimeIngestError::AttachmentBytes);
        }
        budget.attachment_bytes = checked_add(budget.attachment_bytes, length)?;
        if budget.attachment_bytes > limits.total_attachment_bytes {
            return Err(MimeIngestError::AttachmentBytes);
        }
        validate_attachment_metadata(part, limits)?;
    }

    validate_body_ids(message)?;
    let mut visited = vec![false; message.parts.len()];
    let mut path = vec![false; message.parts.len()];
    walk_part(message, 0, depth, &mut visited, &mut path, budget, limits)?;
    if visited.iter().any(|seen| !seen) {
        return Err(MimeIngestError::MalformedStructure);
    }
    Ok(())
}

fn validate_part_offsets(
    message: &Message<'_>,
    header: u32,
    body: u32,
    end: u32,
) -> Result<(), MimeIngestError> {
    // Nested `message/rfc822` parts retain offsets into the parser's backing
    // raw buffer. `Message::raw_message()` intentionally returns only the
    // nested root range, so offset validation must use the backing buffer.
    let raw_len = message.raw_message.as_ref().len();
    let header = header as usize;
    let body = body as usize;
    let end = end as usize;
    if header > body || body > end || end > raw_len {
        return Err(MimeIngestError::MalformedStructure);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn walk_part(
    message: &Message<'_>,
    part_id: usize,
    depth: usize,
    visited: &mut [bool],
    path: &mut [bool],
    budget: &mut ValidationBudget,
    limits: IngestLimits,
) -> Result<(), MimeIngestError> {
    if depth > limits.mime_depth {
        return Err(MimeIngestError::NestingDepth);
    }
    if part_id >= message.parts.len() || path[part_id] || visited[part_id] {
        return Err(MimeIngestError::MalformedStructure);
    }
    path[part_id] = true;
    visited[part_id] = true;
    match &message.parts[part_id].body {
        PartType::Multipart(children) => {
            for child in children {
                walk_part(
                    message,
                    *child as usize,
                    depth + 1,
                    visited,
                    path,
                    budget,
                    limits,
                )?;
            }
        }
        PartType::Message(nested) => {
            validate_message(nested, depth + 1, budget, limits)?;
        }
        PartType::Text(_) | PartType::Html(_) | PartType::Binary(_) | PartType::InlineBinary(_) => {
        }
    }
    path[part_id] = false;
    Ok(())
}

fn validate_body_ids(message: &Message<'_>) -> Result<(), MimeIngestError> {
    if message.text_body.len() > message.parts.len()
        || message.html_body.len() > message.parts.len()
    {
        return Err(MimeIngestError::MalformedStructure);
    }
    for &part_id in message.text_body.iter().chain(&message.html_body) {
        if message.parts.get(part_id as usize).is_none() {
            return Err(MimeIngestError::MalformedStructure);
        }
    }
    Ok(())
}

fn recoverable_encoding_problem(part: &mail_parser::MessagePart<'_>) -> bool {
    matches!(
        part.body,
        PartType::Multipart(_) | PartType::Text(_) | PartType::Html(_)
    ) && has_identity_transfer_encoding(part)
}

fn has_identity_transfer_encoding(part: &mail_parser::MessagePart<'_>) -> bool {
    part.content_transfer_encoding()
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .is_none_or(|encoding| matches!(encoding.as_str(), "" | "7bit" | "8bit" | "binary"))
}

fn is_composite_part(part: &mail_parser::MessagePart<'_>) -> bool {
    matches!(part.body, PartType::Multipart(_) | PartType::Message(_))
        || part.content_type().is_some_and(|content_type| {
            matches!(
                content_type.ctype().to_ascii_lowercase().as_str(),
                "multipart" | "message"
            )
        })
}

fn recoverable_truncated_body(part: &mail_parser::MessagePart<'_>) -> bool {
    part.is_encoding_problem
        && recoverable_encoding_problem(part)
        && part.attachment_name().is_none()
        && !part
            .content_disposition()
            .is_some_and(|value| value.is_attachment())
}

fn add_decoded_bytes(
    budget: &mut ValidationBudget,
    length: usize,
    limits: IngestLimits,
) -> Result<(), MimeIngestError> {
    budget.decoded_bytes = checked_add(budget.decoded_bytes, length)?;
    if budget.decoded_bytes > limits.total_decoded_bytes {
        return Err(MimeIngestError::TotalDecodedBytes);
    }
    Ok(())
}

fn checked_add(left: usize, right: usize) -> Result<usize, MimeIngestError> {
    left.checked_add(right)
        .ok_or(MimeIngestError::TotalDecodedBytes)
}

fn validate_attachment_metadata(
    part: &mail_parser::MessagePart<'_>,
    limits: IngestLimits,
) -> Result<(), MimeIngestError> {
    let filename = part.attachment_name().unwrap_or("attachment");
    let content_id = normalized_content_id(part.content_id().unwrap_or_default());
    let media_type = normalized_media_type(part);
    if filename.len() > limits.filename_bytes
        || media_type.len() > limits.media_type_bytes
        || content_id.len() > limits.content_id_bytes
        || filename.contains(['\0', '\r', '\n'])
        || content_id.contains(['\0', '\r', '\n'])
    {
        return Err(MimeIngestError::AttachmentMetadata);
    }
    Ok(())
}

fn project_message(
    message: &Message<'_>,
    limits: IngestLimits,
) -> Result<IngestedMime, MimeIngestError> {
    let (subject, from, reply_to, to, cc) = project_envelope(message, limits)?;
    let ThreadingHeaders {
        internet_message_id,
        in_reply_to,
        references,
    } = project_threading_headers(message)?;
    let attachment_ids = message
        .attachments
        .iter()
        .copied()
        .filter(|part_id| {
            message
                .parts
                .get(*part_id as usize)
                .is_some_and(|part| !recoverable_truncated_body(part))
        })
        .collect::<BTreeSet<_>>();
    let plain = first_text_projection(message, &message.text_body, &attachment_ids, false);
    let html = first_text_projection(message, &message.html_body, &attachment_ids, true);

    let mut attachments = Vec::with_capacity(message.attachments.len());
    for &part_id in &message.attachments {
        let part = message
            .parts
            .get(part_id as usize)
            .ok_or(MimeIngestError::MalformedStructure)?;
        if recoverable_truncated_body(part) {
            continue;
        }
        let filename = part.attachment_name().unwrap_or("attachment").to_string();
        let content_id = normalized_content_id(part.content_id().unwrap_or_default()).to_string();
        let media_type = normalized_media_type(part);
        if filename.len() > limits.filename_bytes
            || media_type.len() > limits.media_type_bytes
            || content_id.len() > limits.content_id_bytes
        {
            return Err(MimeIngestError::AttachmentMetadata);
        }
        let disposition = if part
            .content_disposition()
            .is_some_and(|value| value.is_inline())
            || matches!(part.body, PartType::InlineBinary(_))
        {
            "inline"
        } else {
            "attachment"
        };
        attachments.push(IngestedAttachment {
            filename,
            media_type,
            bytes: part.contents().to_vec(),
            content_id,
            disposition: disposition.to_string(),
        });
    }
    Ok(IngestedMime {
        subject,
        from,
        reply_to,
        to,
        cc,
        internet_message_id,
        in_reply_to,
        references,
        plain,
        html,
        attachments,
    })
}

fn project_threading_headers(message: &Message<'_>) -> Result<ThreadingHeaders, MimeIngestError> {
    let internet_message_id = message
        .message_id()
        .map(crate::internet_message::canonicalize_message_id)
        .transpose()
        .map_err(|()| MimeIngestError::HeaderProjection)?;
    let in_reply_to = message
        .in_reply_to()
        .as_text_list()
        .and_then(|values| values.last())
        .map(|value| crate::internet_message::canonicalize_message_id(value.as_ref()))
        .transpose()
        .map_err(|()| MimeIngestError::HeaderProjection)?;

    let mut references = Vec::new();
    let mut reference_bytes = 0_usize;
    if let Some(values) = message.references().as_text_list() {
        for value in values.iter().rev() {
            let value = crate::internet_message::canonicalize_message_id(value.as_ref())
                .map_err(|()| MimeIngestError::HeaderProjection)?;
            if references.len() == crate::internet_message::MAX_REFERENCES
                || reference_bytes.saturating_add(value.len())
                    > crate::internet_message::MAX_REFERENCE_BYTES
            {
                break;
            }
            reference_bytes += value.len();
            references.push(value);
        }
        references.reverse();
    }
    Ok(ThreadingHeaders {
        internet_message_id,
        in_reply_to,
        references,
    })
}

type EnvelopeProjection = (
    String,
    Vec<IngestedMailbox>,
    Vec<IngestedMailbox>,
    Vec<IngestedMailbox>,
    Vec<IngestedMailbox>,
);

fn project_envelope(
    message: &Message<'_>,
    limits: IngestLimits,
) -> Result<EnvelopeProjection, MimeIngestError> {
    let subject = message.subject().unwrap_or_default();
    if subject.len() > limits.subject_bytes || contains_header_control(subject) {
        return Err(MimeIngestError::HeaderProjection);
    }
    let mut count = 0usize;
    let mut bytes = subject.len();
    let mut from = Vec::new();
    let mut reply_to = Vec::new();
    let mut to = Vec::new();
    let mut cc = Vec::new();
    append_addresses(message.from(), &mut from, &mut count, &mut bytes, limits)?;
    append_addresses(
        message.reply_to(),
        &mut reply_to,
        &mut count,
        &mut bytes,
        limits,
    )?;
    for address in message.all_to() {
        append_addresses(Some(address), &mut to, &mut count, &mut bytes, limits)?;
    }
    for address in message.all_cc() {
        append_addresses(Some(address), &mut cc, &mut count, &mut bytes, limits)?;
    }
    Ok((subject.to_string(), from, reply_to, to, cc))
}

fn append_addresses(
    header: Option<&mail_parser::Address<'_>>,
    output: &mut Vec<IngestedMailbox>,
    count: &mut usize,
    bytes: &mut usize,
    limits: IngestLimits,
) -> Result<(), MimeIngestError> {
    let Some(header) = header else {
        return Ok(());
    };
    for mailbox in header.iter() {
        let Some(address) = mailbox.address() else {
            continue;
        };
        let name = mailbox.name().unwrap_or_default();
        if name.len() > limits.address_name_bytes
            || address.len() > limits.address_bytes
            || contains_header_control(name)
            || contains_header_control(address)
        {
            return Err(MimeIngestError::HeaderProjection);
        }
        *count = checked_add(*count, 1)?;
        *bytes = checked_add(*bytes, checked_add(name.len(), address.len())?)?;
        if *count > limits.address_count || *bytes > limits.total_envelope_bytes {
            return Err(MimeIngestError::HeaderProjection);
        }
        output.push(IngestedMailbox {
            name: name.to_string(),
            address: address.to_string(),
        });
    }
    Ok(())
}

fn contains_header_control(value: &str) -> bool {
    value
        .chars()
        .any(|character| character == '\0' || character == '\r' || character == '\n')
}

fn first_text_projection(
    message: &Message<'_>,
    preferred: &[u32],
    attachment_ids: &BTreeSet<u32>,
    html: bool,
) -> Option<String> {
    preferred
        .iter()
        .copied()
        .chain(0..message.parts.len() as u32)
        .filter(|part_id| !attachment_ids.contains(part_id))
        .find_map(|part_id| match &message.parts.get(part_id as usize)?.body {
            PartType::Html(value) if html => Some(value.to_string()),
            PartType::Text(value) if !html => Some(value.to_string()),
            _ => None,
        })
}

fn normalized_media_type(part: &mail_parser::MessagePart<'_>) -> String {
    let Some(content_type) = part.content_type() else {
        return "application/octet-stream".to_string();
    };
    let type_ = content_type.ctype().trim().to_ascii_lowercase();
    let subtype = content_type
        .subtype()
        .unwrap_or("octet-stream")
        .trim()
        .to_ascii_lowercase();
    if type_.is_empty() || subtype.is_empty() {
        "application/octet-stream".to_string()
    } else {
        format!("{type_}/{subtype}")
    }
}

fn normalized_content_id(value: &str) -> &str {
    value.trim().trim_start_matches('<').trim_end_matches('>')
}

fn preflight_root_headers(raw: &[u8], limits: IngestLimits) -> Result<(), MimeIngestError> {
    let search_length = raw
        .len()
        .min(limits.header_bytes_per_part.saturating_add(4));
    let search = &raw[..search_length];
    let Some(body_start) = header_body_offset(search) else {
        return if raw.len() > limits.header_bytes_per_part {
            Err(MimeIngestError::HeaderBytes)
        } else {
            Err(MimeIngestError::MalformedStructure)
        };
    };
    let (header_count, header_bytes) = validate_header_block(&raw[..body_start], true, limits)?;
    if header_count > limits.headers_per_part {
        return Err(MimeIngestError::HeaderCount);
    }
    if header_bytes > limits.header_bytes_per_part {
        return Err(MimeIngestError::HeaderBytes);
    }
    Ok(())
}

fn header_body_offset(raw: &[u8]) -> Option<usize> {
    let crlf = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4);
    let lf = raw
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|position| position + 2);
    match (crlf, lf) {
        (Some(left), Some(right)) => Some(left.min(right)),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn validate_header_block(
    raw: &[u8],
    require_valid_field: bool,
    limits: IngestLimits,
) -> Result<(usize, usize), MimeIngestError> {
    if raw.len() > limits.header_bytes_per_part {
        return Err(MimeIngestError::HeaderBytes);
    }
    if raw.contains(&0) {
        return Err(MimeIngestError::MalformedStructure);
    }
    let mut fields = 0usize;
    let mut valid_fields = 0usize;
    let mut has_preceding_field = false;
    for physical_line in raw.split(|byte| *byte == b'\n') {
        let line = physical_line.strip_suffix(b"\r").unwrap_or(physical_line);
        if line.contains(&b'\r') {
            return Err(MimeIngestError::MalformedStructure);
        }
        if line.len() > limits.header_line_bytes {
            return Err(MimeIngestError::HeaderBytes);
        }
        if line.is_empty() {
            continue;
        }
        if line[0] == b' ' || line[0] == b'\t' {
            if !has_preceding_field {
                return Err(MimeIngestError::MalformedStructure);
            }
            continue;
        }
        fields = checked_add(fields, 1)?;
        has_preceding_field = true;
        if line
            .iter()
            .position(|byte| *byte == b':')
            .is_some_and(|colon| colon > 0 && valid_header_name(&line[..colon]))
        {
            valid_fields = checked_add(valid_fields, 1)?;
        }
    }
    if require_valid_field && valid_fields == 0 {
        return Err(MimeIngestError::MalformedStructure);
    }
    Ok((fields, raw.len()))
}

fn valid_header_name(value: &[u8]) -> bool {
    value
        .iter()
        .all(|byte| byte.is_ascii_graphic() && *byte != b':')
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use sha2::{Digest, Sha256};

    fn tiny_limits() -> IngestLimits {
        IngestLimits {
            raw_message_bytes: 8 * 1024,
            mime_depth: 4,
            part_count: 8,
            headers_per_part: 5,
            total_header_count: 20,
            header_bytes_per_part: 512,
            total_header_bytes: 1_024,
            header_line_bytes: 256,
            decoded_text_part_bytes: 128,
            total_decoded_text_bytes: 192,
            attachment_bytes: 256,
            total_attachment_bytes: 384,
            total_decoded_bytes: 512,
            attachment_count: 3,
            filename_bytes: 64,
            media_type_bytes: 64,
            content_id_bytes: 64,
            subject_bytes: 64,
            address_count: 8,
            address_name_bytes: 32,
            address_bytes: 64,
            total_envelope_bytes: 256,
        }
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn rejection(result: Result<IngestedMime, MimeIngestError>) -> MimeIngestError {
        match result {
            Ok(_) => panic!("expected MIME ingestion rejection"),
            Err(error) => error,
        }
    }

    #[test]
    fn binary_attachment_bytes_and_digest_are_preserved_exactly() {
        let original = (0u8..=255).cycle().take(8_193).collect::<Vec<_>>();
        let encoded = STANDARD.encode(&original);
        let raw = format!(
            "MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n\
             --x\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nhello\r\n\
             --x\r\nContent-Type: application/octet-stream\r\n\
             Content-Disposition: attachment; filename=bytes.bin\r\n\
             Content-Transfer-Encoding: base64\r\n\r\n{encoded}\r\n--x--\r\n"
        );
        let projected = ingest_mime(raw.as_bytes()).expect("bounded MIME");
        assert_eq!(projected.attachments.len(), 1);
        assert_eq!(projected.attachments[0].bytes, original);
        assert_eq!(
            sha256_hex(&projected.attachments[0].bytes),
            "db8e82fcacaeceb336ecb8be90fad31da698c72a6c10fd1ea68a2e5874b78b17"
        );
    }

    #[test]
    fn standards_parser_decodes_charset_and_rfc2231_filename_continuations() {
        let raw = concat!(
            "MIME-Version: 1.0\r\n",
            "Subject: =?UTF-8?Q?Quarterly_=E2=9C=93?=\r\n",
            "From: =?UTF-8?Q?Jos=C3=A9_Alvarez?= <jose@example.test>\r\n",
            "Reply-To: Support <support@example.test>\r\n",
            "To: =?UTF-8?B?5p2O6Zu3?= <li@example.test>, team@example.test\r\n",
            "Cc: Reviewers: one@example.test, two@example.test;\r\n",
            "Content-Type: multipart/mixed; boundary=x\r\n\r\n",
            "--x\r\nContent-Type: text/plain; charset=windows-1252\r\n",
            "Content-Transfer-Encoding: quoted-printable\r\n\r\nPrice: =80 5\r\n",
            "--x\r\nContent-Type: image/gif; name*1=\"about \"; ",
            "name*0=\"Book \"; name*2*=utf-8''%e2%98%95%20tables.gif\r\n",
            "Content-Disposition: attachment\r\nContent-Transfer-Encoding: base64\r\n\r\n",
            "R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7\r\n--x--\r\n"
        );
        let projected = ingest_mime(raw.as_bytes()).expect("standards MIME");
        assert_eq!(projected.subject, "Quarterly ✓");
        assert_eq!(projected.from[0].name, "José Alvarez");
        assert_eq!(projected.from[0].address, "jose@example.test");
        assert_eq!(projected.reply_to[0].address, "support@example.test");
        assert_eq!(projected.to[0].name, "李雷");
        assert_eq!(projected.to[1].address, "team@example.test");
        assert_eq!(projected.cc.len(), 2);
        assert_eq!(projected.plain.as_deref(), Some("Price: € 5"));
        assert_eq!(
            projected.attachments[0].filename,
            "Book about ☕ tables.gif"
        );
    }

    #[test]
    fn recoverable_missing_closing_boundary_is_projected_without_unbounded_search() {
        let raw = b"MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\nContent-Type: text/plain\r\n\r\nrecovered";
        let projected = ingest_mime(raw).expect("best-effort recovery");
        assert_eq!(projected.plain.as_deref(), Some("recovered"));
    }

    #[test]
    fn multipart_transfer_encoding_is_never_misclassified_as_boundary_recovery() {
        for encoding in ["base64", "quoted-printable", "x-hostile"] {
            let raw = format!(
                "MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=x\r\n\
                 Content-Transfer-Encoding: {encoding}\r\n\r\n\
                 --x\r\nContent-Type: text/plain\r\n\r\nmust not project\r\n--x--\r\n"
            );
            assert_eq!(
                rejection(ingest_mime(raw.as_bytes())),
                MimeIngestError::TransferEncoding,
                "multipart {encoding}"
            );
        }

        let missing_boundary = b"MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n--x\r\nContent-Type: text/plain\r\n\r\nrecovered";
        let projected = ingest_mime(missing_boundary).expect("identity multipart recovery");
        assert_eq!(projected.plain.as_deref(), Some("recovered"));

        let nested = b"From: nested@example.test\r\nTo: recipient@example.test\r\nSubject: Nested\r\nContent-Type: text/plain\r\n\r\nbody";
        let encoded_nested = STANDARD.encode(nested);
        let encoded_message = format!(
            "MIME-Version: 1.0\r\nContent-Type: message/rfc822\r\n\
             Content-Disposition: attachment; filename=nested.eml\r\n\
             Content-Transfer-Encoding: base64\r\n\r\n{encoded_nested}"
        );
        assert_eq!(
            rejection(ingest_mime(encoded_message.as_bytes())),
            MimeIngestError::TransferEncoding,
            "base64 message/rfc822 is an invalid composite encoding"
        );
    }

    #[test]
    fn nested_message_rfc822_is_bounded_and_retained_as_an_eml_attachment() {
        let nested = concat!(
            "From: Nested Sender <nested@example.test>\r\n",
            "To: recipient@example.test\r\n",
            "Subject: =?UTF-8?Q?Nested_=E2=9C=93?=\r\n",
            "Content-Type: text/plain; charset=utf-8\r\n\r\n",
            "Nested body.\r\n"
        );
        let raw = format!(
            "MIME-Version: 1.0\r\nSubject: Outer\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n\
             --x\r\nContent-Type: text/plain\r\n\r\nOuter body.\r\n\
             --x\r\nContent-Type: message/rfc822\r\nContent-Disposition: attachment; filename=forwarded.eml\r\n\r\n{nested}\
             --x--\r\n"
        );
        let projected = ingest_mime(raw.as_bytes()).expect("nested message");
        assert_eq!(projected.plain.as_deref(), Some("Outer body."));
        assert_eq!(projected.attachments.len(), 1);
        assert_eq!(projected.attachments[0].filename, "forwarded.eml");
        assert_eq!(projected.attachments[0].media_type, "message/rfc822");
        // The CRLF immediately before a MIME boundary is boundary framing,
        // not part of the embedded message payload.
        assert_eq!(
            projected.attachments[0].bytes,
            nested.strip_suffix("\r\n").unwrap().as_bytes()
        );
    }

    #[test]
    fn malformed_transfer_data_is_rejected_without_echoing_hostile_bytes() {
        let sentinel = "DO-NOT-ECHO-SECRET";
        let raw = format!(
            "MIME-Version: 1.0\r\nContent-Type: application/octet-stream\r\n\
             Content-Disposition: attachment; filename=x.bin\r\n\
             Content-Transfer-Encoding: base64\r\n\r\n%%%{sentinel}%%%"
        );
        let error = rejection(ingest_mime(raw.as_bytes()));
        assert_eq!(error, MimeIngestError::TransferEncoding);
        assert!(!format!("{error:?} {error}").contains(sentinel));

        let corrupt_nested = b"MIME-Version: 1.0\r\nContent-Type: message/rfc822\r\nContent-Disposition: attachment; filename=broken.eml\r\nContent-Transfer-Encoding: base64\r\n\r\n%%%not-an-eml%%%";
        assert_eq!(
            rejection(ingest_mime(corrupt_nested)),
            MimeIngestError::TransferEncoding
        );
    }

    #[test]
    fn every_ingestion_budget_rejects_at_its_boundary() {
        let limits = tiny_limits();
        let plain = b"Content-Type: text/plain\r\n\r\nhello";

        let mut raw_limited = limits;
        raw_limited.raw_message_bytes = plain.len() - 1;
        assert_eq!(
            rejection(ingest_with_limits(plain, raw_limited)),
            MimeIngestError::RawMessageBytes
        );
        let mut exact_raw_limit = limits;
        exact_raw_limit.raw_message_bytes = plain.len();
        assert!(ingest_with_limits(plain, exact_raw_limit).is_ok());

        let headers = b"A: 1\r\nB: 2\r\nContent-Type: text/plain\r\n\r\nbody";
        let mut header_count_limited = limits;
        header_count_limited.headers_per_part = 2;
        assert_eq!(
            rejection(ingest_with_limits(headers, header_count_limited)),
            MimeIngestError::HeaderCount
        );

        let mut header_bytes_limited = limits;
        header_bytes_limited.header_bytes_per_part = 10;
        assert_eq!(
            rejection(ingest_with_limits(plain, header_bytes_limited)),
            MimeIngestError::HeaderBytes
        );
        let mut header_line_limited = limits;
        header_line_limited.header_line_bytes = 10;
        assert_eq!(
            rejection(ingest_with_limits(plain, header_line_limited)),
            MimeIngestError::HeaderBytes
        );

        let long_text = format!("Content-Type: text/plain\r\n\r\n{}", "x".repeat(129));
        assert_eq!(
            rejection(ingest_with_limits(long_text.as_bytes(), limits)),
            MimeIngestError::DecodedTextBytes
        );

        let encoded = STANDARD.encode(vec![7u8; 257]);
        let large_attachment = format!(
            "Content-Type: application/octet-stream\r\nContent-Disposition: attachment\r\n\
             Content-Transfer-Encoding: base64\r\n\r\n{encoded}"
        );
        assert_eq!(
            rejection(ingest_with_limits(large_attachment.as_bytes(), limits)),
            MimeIngestError::AttachmentBytes
        );
    }

    #[test]
    fn excessive_parts_attachments_and_depth_are_rejected() {
        let limits = tiny_limits();
        let mut many_parts =
            String::from("MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n");
        for index in 0..8 {
            many_parts.push_str(&format!(
                "--x\r\nContent-Type: text/plain\r\nContent-Disposition: attachment; filename={index}.txt\r\n\r\nx\r\n"
            ));
        }
        many_parts.push_str("--x--\r\n");
        assert!(matches!(
            ingest_with_limits(many_parts.as_bytes(), limits),
            Err(MimeIngestError::PartCount | MimeIngestError::AttachmentCount)
        ));

        fn nested(level: usize) -> String {
            if level == 0 {
                return "Content-Type: text/plain\r\n\r\nleaf\r\n".to_string();
            }
            format!(
                "Content-Type: multipart/mixed; boundary=b{level}\r\n\r\n--b{level}\r\n{}--b{level}--\r\n",
                nested(level - 1)
            )
        }
        assert_eq!(
            rejection(ingest_with_limits(nested(5).as_bytes(), limits)),
            MimeIngestError::NestingDepth
        );
    }

    #[test]
    fn aggregate_decoded_and_attachment_budgets_are_independent() {
        let mut limits = tiny_limits();
        limits.attachment_bytes = 200;
        limits.total_attachment_bytes = 150;
        limits.total_decoded_bytes = 512;
        let encoded = STANDARD.encode(vec![1u8; 80]);
        let raw = format!(
            "MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n\
             --x\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=a\r\nContent-Transfer-Encoding: base64\r\n\r\n{encoded}\r\n\
             --x\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=b\r\nContent-Transfer-Encoding: base64\r\n\r\n{encoded}\r\n--x--\r\n"
        );
        assert_eq!(
            rejection(ingest_with_limits(raw.as_bytes(), limits)),
            MimeIngestError::AttachmentBytes
        );

        limits.total_attachment_bytes = 400;
        limits.total_decoded_bytes = 150;
        assert_eq!(
            rejection(ingest_with_limits(raw.as_bytes(), limits)),
            MimeIngestError::TotalDecodedBytes
        );
    }

    #[test]
    fn aggregate_header_count_and_bytes_have_independent_limits() {
        let raw = concat!(
            "MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n",
            "--x\r\nContent-Type: text/plain\r\nX-Long-One: 123456789012345678901234567890\r\n\r\none\r\n",
            "--x\r\nContent-Type: text/plain\r\nX-Long-Two: 123456789012345678901234567890\r\n\r\ntwo\r\n",
            "--x--\r\n"
        );
        let mut count_limits = tiny_limits();
        count_limits.part_count = 16;
        count_limits.headers_per_part = 5;
        count_limits.total_header_count = 5;
        assert_eq!(
            rejection(ingest_with_limits(raw.as_bytes(), count_limits)),
            MimeIngestError::HeaderCount
        );

        let mut byte_limits = tiny_limits();
        byte_limits.part_count = 16;
        byte_limits.headers_per_part = 10;
        byte_limits.total_header_count = 20;
        byte_limits.header_bytes_per_part = 256;
        byte_limits.total_header_bytes = 180;
        assert_eq!(
            rejection(ingest_with_limits(raw.as_bytes(), byte_limits)),
            MimeIngestError::HeaderBytes
        );
    }

    #[test]
    fn aggregate_text_attachment_count_and_metadata_limits_are_independent() {
        let text_raw = concat!(
            "MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n",
            "--x\r\nContent-Type: text/plain\r\n\r\n12345\r\n",
            "--x\r\nContent-Type: text/plain\r\nContent-Disposition: attachment; filename=a.txt\r\n\r\n67890\r\n",
            "--x--\r\n"
        );
        let mut text_limits = tiny_limits();
        text_limits.part_count = 16;
        text_limits.decoded_text_part_bytes = 16;
        text_limits.total_decoded_text_bytes = 9;
        assert_eq!(
            rejection(ingest_with_limits(text_raw.as_bytes(), text_limits)),
            MimeIngestError::DecodedTextBytes
        );

        let attachments_raw = concat!(
            "MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=x\r\n\r\n",
            "--x\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=a\r\n\r\na\r\n",
            "--x\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=b\r\n\r\nb\r\n",
            "--x--\r\n"
        );
        let mut count_limits = tiny_limits();
        count_limits.part_count = 16;
        count_limits.attachment_count = 1;
        assert_eq!(
            rejection(ingest_with_limits(attachments_raw.as_bytes(), count_limits)),
            MimeIngestError::AttachmentCount
        );

        let metadata_cases = [
            (
                "Content-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=filename-too-long.bin\r\n\r\nx",
                "filename",
                (8, 64, 64),
            ),
            (
                "Content-Type: application/vnd.mux.very-long-subtype\r\nContent-Disposition: attachment; filename=x\r\n\r\nx",
                "media type",
                (64, 16, 64),
            ),
            (
                "Content-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=x\r\nContent-ID: <content-id-too-long>\r\n\r\nx",
                "content id",
                (64, 64, 8),
            ),
        ];
        for (raw, label, (filename_bytes, media_type_bytes, content_id_bytes)) in metadata_cases {
            let mut metadata_limits = tiny_limits();
            metadata_limits.filename_bytes = filename_bytes;
            metadata_limits.media_type_bytes = media_type_bytes;
            metadata_limits.content_id_bytes = content_id_bytes;
            assert_eq!(
                rejection(ingest_with_limits(raw.as_bytes(), metadata_limits)),
                MimeIngestError::AttachmentMetadata,
                "{label}"
            );
        }
    }

    #[test]
    fn decoded_subject_and_address_envelope_limits_are_independent() {
        let raw = b"Subject: decoded subject\r\nFrom: Display Name <sender@example.test>\r\nTo: one@example.test, two@example.test\r\nContent-Type: text/plain\r\n\r\nbody";

        let mut subject_limits = tiny_limits();
        subject_limits.subject_bytes = 8;
        assert_eq!(
            rejection(ingest_with_limits(raw, subject_limits)),
            MimeIngestError::HeaderProjection
        );

        let mut count_limits = tiny_limits();
        count_limits.address_count = 2;
        assert_eq!(
            rejection(ingest_with_limits(raw, count_limits)),
            MimeIngestError::HeaderProjection
        );

        let mut aggregate_limits = tiny_limits();
        aggregate_limits.total_envelope_bytes = 30;
        assert_eq!(
            rejection(ingest_with_limits(raw, aggregate_limits)),
            MimeIngestError::HeaderProjection
        );

        let mut name_limits = tiny_limits();
        name_limits.address_name_bytes = 4;
        assert_eq!(
            rejection(ingest_with_limits(raw, name_limits)),
            MimeIngestError::HeaderProjection
        );

        let mut address_limits = tiny_limits();
        address_limits.address_bytes = 8;
        assert_eq!(
            rejection(ingest_with_limits(raw, address_limits)),
            MimeIngestError::HeaderProjection
        );
    }

    #[test]
    fn deterministic_hostile_byte_corpus_never_panics_or_returns_unbounded_projection() {
        let mut state = 0x6d75_782d_6d69_6d65u64;
        for length in 0..512usize {
            let mut input = vec![0u8; length];
            for byte in &mut input {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *byte = state as u8;
            }
            if length > 8 {
                input[..8].copy_from_slice(b"X: y\r\n\r\n");
            }
            let result = catch_unwind(AssertUnwindSafe(|| ingest_mime(&input)));
            assert!(
                result.is_ok(),
                "ingestion panicked for corpus length {length}"
            );
            if let Ok(Ok(projected)) = result {
                assert!(
                    projected.plain.as_ref().map_or(0, String::len) <= MAX_DECODED_TEXT_PART_BYTES
                );
                assert!(
                    projected.html.as_ref().map_or(0, String::len) <= MAX_DECODED_TEXT_PART_BYTES
                );
                assert!(projected.attachments.len() <= MAX_ATTACHMENT_COUNT);
                assert!(projected
                    .attachments
                    .iter()
                    .all(|part| part.bytes.len() <= MAX_ATTACHMENT_BYTES));
            }
        }
    }
}
