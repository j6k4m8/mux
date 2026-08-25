//! Provider-neutral construction of immutable outgoing RFC 5322/MIME messages.
//!
//! Provider adapters consume this boundary after the undo window expires. It
//! deliberately does not perform network I/O or decide whether a send may be
//! retried: the durable worker retains that non-idempotent state machine.

use std::collections::BTreeSet;

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use mail_builder::{
    headers::{
        address::Address, content_type::ContentType, date::Date, message_id::MessageId, raw::Raw,
    },
    mime::{BodyPart, MimePart},
    MessageBuilder,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

const SNAPSHOT_VERSION: u8 = 2;
const MAX_RECIPIENT_HEADER_BYTES: usize = 10_000;
const MAX_RECIPIENTS: usize = 500;
const MAX_SUBJECT_BYTES: usize = 4 * 1024;
const MAX_CORRELATION_BYTES: usize = 256;
const MAX_PROVIDER_KIND_BYTES: usize = 64;
const MAX_REMOTE_THREAD_ID_BYTES: usize = 2_048;
const MAX_OUTGOING_FILENAME_BYTES: usize = 180;
const MAX_TOTAL_OUTGOING_ATTACHMENT_BYTES: usize = crate::mime_ingest::MAX_ATTACHMENT_BYTES;
pub(crate) const MAX_RFC3339_UNIX_MILLIS: i64 = 253_402_300_799_999;

#[derive(Debug, Error, PartialEq, Eq)]
pub(crate) enum OutgoingError {
    #[error("Outgoing snapshot is malformed: {0}")]
    MalformedSnapshot(String),
    #[error("Outgoing message is invalid: {0}")]
    Validation(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Draft attachment persistence is the next consumer of both variants.
pub(crate) enum AttachmentDisposition {
    Inline,
    Attachment,
}

#[derive(Debug, Clone)]
pub(crate) struct OutgoingAttachment {
    pub filename: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
    pub content_id: Option<String>,
    pub disposition: AttachmentDisposition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreparedOutgoingMessage {
    pub envelope_from: String,
    pub envelope_recipients: Vec<String>,
    pub raw_message: Vec<u8>,
    pub raw_sha256_hex: String,
    pub message_id: String,
    pub client_correlation_id: Option<String>,
    pub provider_kind: Option<String>,
    pub remote_thread_id: Option<String>,
    pub queued_at_ms: i64,
    /// Provider adapters must request SMTPUTF8 when this is true. The current
    /// conservative mailbox boundary accepts ASCII envelopes only, so it is
    /// false for every successfully prepared message rather than an implicit
    /// provider assumption.
    pub requires_smtp_utf8: bool,
}

impl PreparedOutgoingMessage {
    pub fn reconciliation_key(&self) -> &str {
        &self.message_id
    }

    #[allow(dead_code)] // Provider reconciliation consumes this after submission is implemented.
    pub fn matches_observed_message_id(&self, observed: &str) -> bool {
        crate::internet_message::canonicalize_message_id(observed)
            .is_ok_and(|observed| self.message_id == observed)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DurableSendSnapshot {
    snapshot_version: Option<u8>,
    submission_message_id: Option<String>,
    sender_email: Option<String>,
    recipients: Option<String>,
    cc_recipients: Option<String>,
    bcc_recipients: Option<String>,
    subject: Option<String>,
    body: Option<String>,
    body_html: Option<String>,
    queued_at_ms: Option<i64>,
    #[serde(default)]
    in_reply_to: Option<String>,
    #[serde(default)]
    references: Vec<String>,
    #[serde(default)]
    client_correlation_id: Option<String>,
    #[serde(default)]
    provider_kind: Option<String>,
    #[serde(default)]
    remote_thread_id: Option<String>,
}

#[derive(Debug, Clone)]
struct Mailbox {
    address: String,
    display_name: Option<String>,
}

pub(crate) fn prepare_from_durable_payload(
    payload_json: &str,
    attachments: &[OutgoingAttachment],
) -> Result<PreparedOutgoingMessage, OutgoingError> {
    let snapshot: DurableSendSnapshot = serde_json::from_str(payload_json)
        .map_err(|error| OutgoingError::MalformedSnapshot(error.to_string()))?;
    if !matches!(snapshot.snapshot_version, Some(SNAPSHOT_VERSION | 3)) {
        return Err(OutgoingError::MalformedSnapshot(
            "unsupported or missing snapshotVersion".into(),
        ));
    }
    if snapshot.snapshot_version == Some(SNAPSHOT_VERSION)
        && (snapshot.client_correlation_id.is_some()
            || snapshot.provider_kind.is_some()
            || snapshot.remote_thread_id.is_some())
    {
        return Err(OutgoingError::MalformedSnapshot(
            "legacy snapshot contains unsupported provider submission fields".into(),
        ));
    }

    let message_id = required(snapshot.submission_message_id, "submissionMessageId")?;
    validate_message_id(&message_id)?;
    let sender = required(snapshot.sender_email, "senderEmail")?;
    let mut senders = parse_mailbox_list(&sender, true)?;
    if senders.len() != 1 {
        return Err(OutgoingError::Validation(
            "sender must contain exactly one mailbox".into(),
        ));
    }
    let sender = senders.pop().expect("one sender was validated");
    let to = parse_mailbox_list(&required(snapshot.recipients, "recipients")?, false)?;
    let cc = parse_mailbox_list(snapshot.cc_recipients.as_deref().unwrap_or_default(), false)?;
    let bcc = parse_mailbox_list(
        snapshot.bcc_recipients.as_deref().unwrap_or_default(),
        false,
    )?;
    let recipient_count = to.len().saturating_add(cc.len()).saturating_add(bcc.len());
    if recipient_count == 0 {
        return Err(OutgoingError::Validation(
            "at least one envelope recipient is required".into(),
        ));
    }
    if recipient_count > MAX_RECIPIENTS {
        return Err(OutgoingError::Validation(
            "recipient count limit exceeded".into(),
        ));
    }

    let subject = required(snapshot.subject, "subject")?;
    validate_unstructured_header(&subject, "subject", MAX_SUBJECT_BYTES)?;
    let body = required(snapshot.body, "body")?;
    let body_html = required(snapshot.body_html, "bodyHtml")?;
    if body.contains('\0') || body_html.contains('\0') {
        return Err(OutgoingError::Validation(
            "message bodies cannot contain NUL bytes".into(),
        ));
    }
    let queued_at_ms = snapshot
        .queued_at_ms
        .ok_or_else(|| OutgoingError::MalformedSnapshot("queuedAtMs is missing".into()))?;
    if !(0..=MAX_RFC3339_UNIX_MILLIS).contains(&queued_at_ms) {
        return Err(OutgoingError::Validation(
            "queuedAtMs is out of range".into(),
        ));
    }

    if let Some(value) = snapshot.in_reply_to.as_deref() {
        validate_message_id(value)?;
    }
    validate_references(&snapshot.references)?;
    let client_correlation_id = match snapshot.snapshot_version {
        Some(3) => Some(required(
            snapshot.client_correlation_id,
            "clientCorrelationId",
        )?),
        _ => snapshot.client_correlation_id,
    };
    if let Some(value) = client_correlation_id.as_deref() {
        validate_token(value, "clientCorrelationId", MAX_CORRELATION_BYTES)?;
    }
    let provider_kind = match snapshot.snapshot_version {
        Some(3) => Some(required(snapshot.provider_kind, "providerKind")?),
        _ => snapshot.provider_kind,
    };
    if let Some(value) = provider_kind.as_deref() {
        validate_token(value, "providerKind", MAX_PROVIDER_KIND_BYTES)?;
    }
    if let Some(value) = snapshot.remote_thread_id.as_deref() {
        validate_token(value, "remoteThreadId", MAX_REMOTE_THREAD_ID_BYTES)?;
    }
    validate_attachments(attachments)?;

    let mut builder = MessageBuilder::new()
        .date(Date::new(queued_at_ms.div_euclid(1_000)))
        .from(mailbox_address(&sender))
        .subject(subject.clone())
        .message_id(MessageId::new(message_id_inner(&message_id).to_string()));
    if !to.is_empty() {
        builder = builder.to(mailbox_addresses(&to));
    }
    if !cc.is_empty() {
        builder = builder.cc(mailbox_addresses(&cc));
    }
    if let Some(in_reply_to) = snapshot.in_reply_to.as_deref() {
        builder = builder.in_reply_to(MessageId::new(message_id_inner(in_reply_to).to_string()));
    }
    if !snapshot.references.is_empty() {
        builder = builder.references(MessageId::new_list(
            snapshot
                .references
                .iter()
                .map(|value| message_id_inner(value).to_string()),
        ));
    }
    if let Some(value) = client_correlation_id.as_deref() {
        builder = builder.header("X-Mux-Client-Correlation", Raw::new(value.to_owned()));
    }
    let boundary_seed = boundary_seed(&message_id, &body, &body_html, attachments);
    builder = builder.body(build_mime_body(
        &body,
        &body_html,
        attachments,
        &boundary_seed,
    )?);
    let mut raw_message = Vec::new();
    builder.write_to(&mut raw_message).map_err(|error| {
        OutgoingError::Validation(format!("MIME serialization failed: {error}"))
    })?;
    if raw_message.len() > crate::mime_ingest::MAX_RAW_MESSAGE_BYTES {
        return Err(OutgoingError::Validation(
            "encoded message byte limit exceeded".into(),
        ));
    }

    let mut envelope_recipients = Vec::with_capacity(recipient_count);
    envelope_recipients.extend(to.into_iter().map(|mailbox| mailbox.address));
    envelope_recipients.extend(cc.into_iter().map(|mailbox| mailbox.address));
    envelope_recipients.extend(bcc.into_iter().map(|mailbox| mailbox.address));
    let raw_sha256_hex = sha256_hex(&raw_message);
    Ok(PreparedOutgoingMessage {
        envelope_from: sender.address,
        envelope_recipients,
        raw_message,
        raw_sha256_hex,
        message_id,
        client_correlation_id,
        provider_kind,
        remote_thread_id: snapshot.remote_thread_id,
        queued_at_ms,
        requires_smtp_utf8: false,
    })
}

fn required(value: Option<String>, name: &str) -> Result<String, OutgoingError> {
    value.ok_or_else(|| OutgoingError::MalformedSnapshot(format!("{name} is missing")))
}

fn parse_mailbox_list(source: &str, required: bool) -> Result<Vec<Mailbox>, OutgoingError> {
    validate_unstructured_header(source, "mailbox list", MAX_RECIPIENT_HEADER_BYTES)?;
    if source.trim().is_empty() {
        return if required {
            Err(OutgoingError::Validation("mailbox list is empty".into()))
        } else {
            Ok(Vec::new())
        };
    }
    let entries = split_mailboxes(source)?;
    entries.into_iter().map(parse_mailbox).collect()
}

fn split_mailboxes(source: &str) -> Result<Vec<&str>, OutgoingError> {
    let source = source.trim();
    let mut entries = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    let mut inside_angle = false;
    for (index, character) in source.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if quoted {
            match character {
                '\\' => escaped = true,
                '"' => quoted = false,
                _ => {}
            }
            continue;
        }
        match character {
            '"' => quoted = true,
            '<' if inside_angle => return Err(invalid_mailbox()),
            '<' => inside_angle = true,
            '>' if !inside_angle => return Err(invalid_mailbox()),
            '>' => inside_angle = false,
            ',' | ';' if !inside_angle => {
                push_mailbox_entry(&mut entries, source[start..index].trim())?;
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    if quoted || escaped || inside_angle {
        return Err(invalid_mailbox());
    }
    push_mailbox_entry(&mut entries, source[start..].trim())?;
    Ok(entries)
}

fn push_mailbox_entry<'a>(entries: &mut Vec<&'a str>, entry: &'a str) -> Result<(), OutgoingError> {
    if entry.is_empty() || entries.len() >= MAX_RECIPIENTS {
        return Err(invalid_mailbox());
    }
    entries.push(entry);
    Ok(())
}

fn parse_mailbox(source: &str) -> Result<Mailbox, OutgoingError> {
    let opening = source.match_indices('<').collect::<Vec<_>>();
    let closing = source.match_indices('>').collect::<Vec<_>>();
    let (display, address) = match (opening.as_slice(), closing.as_slice()) {
        ([], []) => (None, source.trim()),
        ([(open, _)], [(close, _)]) if open < close && source[close + 1..].trim().is_empty() => {
            let display = parse_display_name(source[..*open].trim())?;
            (display, source[open + 1..*close].trim())
        }
        _ => return Err(invalid_mailbox()),
    };
    validate_ascii_address(address)?;
    Ok(Mailbox {
        address: address.to_string(),
        display_name: display.filter(|value| !value.is_empty()),
    })
}

fn parse_display_name(value: &str) -> Result<Option<String>, OutgoingError> {
    if value.is_empty() {
        return Ok(None);
    }
    if value.starts_with('"') {
        if !value.ends_with('"') || value.len() < 2 {
            return Err(invalid_mailbox());
        }
        let inner = &value[1..value.len() - 1];
        let mut decoded = String::with_capacity(inner.len());
        let mut escaped = false;
        for character in inner.chars() {
            if escaped {
                if !matches!(character, '\\' | '"') {
                    return Err(invalid_mailbox());
                }
                decoded.push(character);
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                return Err(invalid_mailbox());
            } else {
                decoded.push(character);
            }
        }
        if escaped {
            return Err(invalid_mailbox());
        }
        Ok(Some(decoded))
    } else if value.contains('"') {
        Err(invalid_mailbox())
    } else {
        Ok(Some(value.to_string()))
    }
}

fn validate_ascii_address(address: &str) -> Result<(), OutgoingError> {
    if !address.is_ascii() {
        return Err(OutgoingError::Validation(
            "internationalized envelope addresses require SMTPUTF8 and are not enabled".into(),
        ));
    }
    if address.is_empty() || address.len() > 320 {
        return Err(invalid_mailbox());
    }
    let Some((local, domain)) = address.split_once('@') else {
        return Err(invalid_mailbox());
    };
    let local_ok = !local.is_empty()
        && local.len() <= 64
        && !local.starts_with('.')
        && !local.ends_with('.')
        && !local.contains("..")
        && local.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'.' | b'!'
                        | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'/'
                        | b'='
                        | b'?'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'{'
                        | b'|'
                        | b'}'
                        | b'~'
                )
        });
    let labels = domain.split('.').collect::<Vec<_>>();
    let domain_ok = !domain.is_empty()
        && domain.len() <= 255
        && !domain.contains('@')
        && labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        });
    if local_ok && domain_ok {
        Ok(())
    } else {
        Err(invalid_mailbox())
    }
}

fn invalid_mailbox() -> OutgoingError {
    OutgoingError::Validation("mailbox list contains unsupported or malformed syntax".into())
}

fn validate_unstructured_header(
    value: &str,
    name: &str,
    max_bytes: usize,
) -> Result<(), OutgoingError> {
    if value.len() > max_bytes {
        return Err(OutgoingError::Validation(format!(
            "{name} byte limit exceeded"
        )));
    }
    if value.bytes().any(|byte| byte < b' ' || byte == 0x7f) {
        return Err(OutgoingError::Validation(format!(
            "{name} contains forbidden control bytes"
        )));
    }
    Ok(())
}

fn validate_token(value: &str, name: &str, max_bytes: usize) -> Result<(), OutgoingError> {
    if value.is_empty()
        || value.len() > max_bytes
        || !value.is_ascii()
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':' | b'@')
        })
    {
        return Err(OutgoingError::Validation(format!(
            "{name} contains unsupported syntax"
        )));
    }
    Ok(())
}

fn validate_message_id(value: &str) -> Result<(), OutgoingError> {
    crate::internet_message::validate_message_id(value)
        .map_err(|()| OutgoingError::Validation("message identifier contains unsafe syntax".into()))
}

fn validate_references(references: &[String]) -> Result<(), OutgoingError> {
    crate::internet_message::validate_references(references)
        .map_err(|()| OutgoingError::Validation("References header is invalid or too large".into()))
}

fn validate_attachments(attachments: &[OutgoingAttachment]) -> Result<(), OutgoingError> {
    if attachments.len() > crate::mime_ingest::MAX_ATTACHMENT_COUNT {
        return Err(OutgoingError::Validation(
            "attachment count limit exceeded".into(),
        ));
    }
    let mut total = 0_usize;
    let mut supplied_content_ids = BTreeSet::new();
    for attachment in attachments {
        if attachment.filename.is_empty() || attachment.filename.len() > MAX_OUTGOING_FILENAME_BYTES
        {
            return Err(OutgoingError::Validation(
                "attachment filename is invalid or too long".into(),
            ));
        }
        validate_unstructured_header(
            &attachment.filename,
            "attachment filename",
            MAX_OUTGOING_FILENAME_BYTES,
        )?;
        validate_media_type(&attachment.media_type)?;
        if attachment.bytes.len() > crate::mime_ingest::MAX_ATTACHMENT_BYTES {
            return Err(OutgoingError::Validation(
                "attachment byte limit exceeded".into(),
            ));
        }
        total = total
            .checked_add(attachment.bytes.len())
            .ok_or_else(|| OutgoingError::Validation("attachment byte limit exceeded".into()))?;
        if total > MAX_TOTAL_OUTGOING_ATTACHMENT_BYTES {
            return Err(OutgoingError::Validation(
                "total attachment byte limit exceeded".into(),
            ));
        }
        if let Some(content_id) = attachment.content_id.as_deref() {
            if content_id.len() + 2 > crate::mime_ingest::MAX_CONTENT_ID_BYTES {
                return Err(OutgoingError::Validation(
                    "attachment content ID byte limit exceeded".into(),
                ));
            }
            validate_message_id(&format!("<{content_id}>"))?;
            if !supplied_content_ids.insert(content_id) {
                return Err(OutgoingError::Validation(
                    "attachment content IDs must be unique".into(),
                ));
            }
        }
    }
    Ok(())
}

fn validate_media_type(value: &str) -> Result<(), OutgoingError> {
    if value.is_empty()
        || value.len() > crate::mime_ingest::MAX_MEDIA_TYPE_BYTES
        || !value.is_ascii()
    {
        return Err(OutgoingError::Validation(
            "attachment media type is invalid".into(),
        ));
    }
    let Some((top, subtype)) = value.split_once('/') else {
        return Err(OutgoingError::Validation(
            "attachment media type is invalid".into(),
        ));
    };
    let token = |part: &str| {
        !part.is_empty()
            && part.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(
                        byte,
                        b'!' | b'#' | b'$' | b'&' | b'^' | b'_' | b'.' | b'+' | b'-'
                    )
            })
    };
    if token(top) && token(subtype) {
        Ok(())
    } else {
        Err(OutgoingError::Validation(
            "attachment media type is invalid".into(),
        ))
    }
}

fn mailbox_address(mailbox: &Mailbox) -> Address<'static> {
    Address::new_address(mailbox.display_name.clone(), mailbox.address.clone())
}

fn mailbox_addresses(mailboxes: &[Mailbox]) -> Address<'static> {
    Address::new_list(mailboxes.iter().map(mailbox_address).collect())
}

fn message_id_inner(value: &str) -> &str {
    &value[1..value.len() - 1]
}

fn boundary_seed(
    message_id: &str,
    body: &str,
    body_html: &str,
    attachments: &[OutgoingAttachment],
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"mux-mime-boundary-v1\0");
    digest.update(message_id.as_bytes());
    digest.update(body.as_bytes());
    digest.update(body_html.as_bytes());
    for attachment in attachments {
        digest.update(attachment.filename.as_bytes());
        digest.update(attachment.media_type.as_bytes());
        digest.update(&attachment.bytes);
        digest.update(
            attachment
                .content_id
                .as_deref()
                .unwrap_or_default()
                .as_bytes(),
        );
    }
    let digest = digest.finalize();
    digest[..12]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn build_mime_body(
    body: &str,
    body_html: &str,
    attachments: &[OutgoingAttachment],
    seed: &str,
) -> Result<MimePart<'static>, OutgoingError> {
    let text = MimePart::new("text/plain", normalize_crlf(body));
    let content = if body_html.is_empty() {
        text
    } else if body.is_empty() {
        MimePart::new("text/html", normalize_crlf(body_html))
    } else {
        let boundary = collision_free_boundary("alternative", seed, body, body_html, attachments)?;
        MimePart::new(
            ContentType::new("multipart/alternative").attribute("boundary", boundary),
            vec![text, MimePart::new("text/html", normalize_crlf(body_html))],
        )
    };
    if attachments.is_empty() {
        return Ok(content);
    }
    let mut parts = Vec::with_capacity(attachments.len() + 1);
    parts.push(content);
    let mut effective_content_ids = BTreeSet::new();
    for (index, attachment) in attachments.iter().enumerate() {
        let encoded = encode_base64_mime(&attachment.bytes);
        let mut part = MimePart::new(
            attachment.media_type.clone(),
            BodyPart::Binary(encoded.into_bytes().into()),
        )
        .transfer_encoding("base64");
        part = part.header(
            "Content-Disposition",
            Raw::new(content_disposition(attachment)),
        );
        let content_id = attachment.content_id.clone().or_else(|| {
            matches!(attachment.disposition, AttachmentDisposition::Inline)
                .then(|| format!("inline-{index}-{seed}@mux.invalid"))
        });
        if let Some(content_id) = content_id {
            if !effective_content_ids.insert(content_id.clone()) {
                return Err(OutgoingError::Validation(
                    "attachment content IDs must be unique".into(),
                ));
            }
            part = part.cid(content_id);
        }
        parts.push(part);
    }
    let boundary = collision_free_boundary("mixed", seed, body, body_html, attachments)?;
    Ok(MimePart::new(
        ContentType::new("multipart/mixed").attribute("boundary", boundary),
        parts,
    ))
}

fn collision_free_boundary(
    kind: &str,
    seed: &str,
    body: &str,
    body_html: &str,
    attachments: &[OutgoingAttachment],
) -> Result<String, OutgoingError> {
    for suffix in 0..=256 {
        let candidate = if suffix == 0 {
            format!("mux-{kind}-{seed}")
        } else {
            format!("mux-{kind}-{seed}-{suffix}")
        };
        let bytes = candidate.as_bytes();
        let collides = body
            .as_bytes()
            .windows(bytes.len())
            .any(|window| window == bytes)
            || body_html
                .as_bytes()
                .windows(bytes.len())
                .any(|window| window == bytes)
            || attachments.iter().any(|attachment| {
                attachment
                    .bytes
                    .windows(bytes.len())
                    .any(|window| window == bytes)
            });
        if !collides {
            return Ok(candidate);
        }
    }
    Err(OutgoingError::Validation(
        "could not select a collision-free MIME boundary".into(),
    ))
}

fn normalize_crlf(value: &str) -> String {
    let normalized = value.replace("\r\n", "\n").replace('\r', "\n");
    normalized.replace('\n', "\r\n")
}

fn content_disposition(attachment: &OutgoingAttachment) -> String {
    let disposition = match attachment.disposition {
        AttachmentDisposition::Inline => "inline",
        AttachmentDisposition::Attachment => "attachment",
    };
    if attachment.filename.is_ascii() {
        let escaped = attachment
            .filename
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        format!("{disposition}; filename=\"{escaped}\"")
    } else {
        format!(
            "{disposition}; filename*=utf-8''{}",
            percent_encode_utf8(&attachment.filename)
        )
    }
}

fn percent_encode_utf8(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            )
        {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push(HEX[(byte >> 4) as usize] as char);
            encoded.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    encoded
}

fn encode_base64_mime(bytes: &[u8]) -> String {
    let encoded = BASE64_STANDARD.encode(bytes);
    let mut output = String::with_capacity(encoded.len() + encoded.len() / 76 * 2 + 2);
    for chunk in encoded.as_bytes().chunks(76) {
        output.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
        output.push_str("\r\n");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(overrides: serde_json::Value) -> String {
        let mut value = serde_json::json!({
            "snapshotVersion": 2,
            "submissionMessageId": "<op-123@mux.invalid>",
            "senderEmail": "jordan@mux.example",
            "recipients": "\"Doe, Jane\" <jane@example.com>",
            "ccRecipients": "Álex <alex@example.com>",
            "bccRecipients": "secret@example.com",
            "subject": "Résumé update",
            "body": "Plain body\nsecond line",
            "bodyHtml": "<p>Plain <strong>body</strong></p>",
            "queuedAtMs": 1_787_462_400_000_i64,
            "inReplyTo": "<parent@example.com>",
            "references": ["<root@example.com>", "<parent@example.com>"]
        });
        if let (Some(target), Some(source)) = (value.as_object_mut(), overrides.as_object()) {
            target.extend(source.clone());
        }
        serde_json::to_string(&value).unwrap()
    }

    #[test]
    fn alternative_message_has_stable_headers_envelope_and_no_bcc_header() {
        let json = payload(serde_json::json!({}));
        let first = prepare_from_durable_payload(&json, &[]).expect("prepared message");
        let second = prepare_from_durable_payload(&json, &[]).expect("stable retry message");
        assert_eq!(first, second);
        assert_eq!(first.raw_sha256_hex.len(), 64);
        assert_eq!(first.raw_sha256_hex, sha256_hex(&first.raw_message));
        assert_eq!(first.envelope_from, "jordan@mux.example");
        assert_eq!(
            first.envelope_recipients,
            ["jane@example.com", "alex@example.com", "secret@example.com"]
        );
        assert!(!first.requires_smtp_utf8);
        assert_eq!(first.reconciliation_key(), "<op-123@mux.invalid>");
        assert!(first.matches_observed_message_id(" <op-123@mux.invalid> "));
        assert!(first.matches_observed_message_id("op-123@mux.invalid"));
        assert!(!first.matches_observed_message_id("<other@example.com>"));
        assert!(!first.matches_observed_message_id("unsafe\r\n@example.com"));

        let raw = String::from_utf8(first.raw_message).unwrap();
        assert!(raw.contains("Message-ID: <op-123@mux.invalid>\r\n"));
        assert!(raw.contains("In-Reply-To: <parent@example.com>\r\n"));
        assert!(raw.contains("References: <root@example.com> <parent@example.com>\r\n"));
        assert!(raw.contains("Content-Type: multipart/alternative;"));
        assert!(!raw.contains("\r\nBcc:"));
        assert!(!raw.contains("secret@example.com"));
        assert!(raw.contains("=?utf-8?B?"));

        let parsed = crate::content::parse_mime(raw.as_bytes()).expect("self-ingested MIME");
        assert_eq!(parsed.body_text, "Plain body\r\nsecond line");
        assert!(parsed.body_html.contains("<strong>body</strong>"));
    }

    #[test]
    fn v3_freezes_provider_target_and_exact_client_correlation_header() {
        let prepared = prepare_from_durable_payload(
            &payload(serde_json::json!({
                "snapshotVersion": 3,
                "clientCorrelationId": "mux-op-123",
                "providerKind": "gmail",
                "remoteThreadId": "gmail-thread-1"
            })),
            &[],
        )
        .expect("v3 outgoing snapshot");
        assert_eq!(prepared.provider_kind.as_deref(), Some("gmail"));
        assert_eq!(prepared.remote_thread_id.as_deref(), Some("gmail-thread-1"));
        assert_eq!(
            prepared.client_correlation_id.as_deref(),
            Some("mux-op-123")
        );
        let raw = String::from_utf8(prepared.raw_message).unwrap();
        assert!(raw.contains("X-Mux-Client-Correlation: mux-op-123\r\n"));
    }

    #[test]
    fn legacy_v2_cannot_smuggle_v3_provider_submission_fields() {
        for fields in [
            serde_json::json!({"providerKind": "gmail"}),
            serde_json::json!({"remoteThreadId": "crafted-thread"}),
            serde_json::json!({"clientCorrelationId": "crafted-correlation"}),
        ] {
            assert!(prepare_from_durable_payload(&payload(fields), &[]).is_err());
        }
    }

    #[test]
    fn bcc_only_message_has_an_envelope_recipient_and_no_visible_recipient_header() {
        let prepared = prepare_from_durable_payload(
            &payload(serde_json::json!({
                "recipients": "",
                "ccRecipients": "",
                "bccRecipients": "hidden@example.com"
            })),
            &[],
        )
        .expect("Bcc-only message");
        assert_eq!(prepared.envelope_recipients, ["hidden@example.com"]);
        let raw = String::from_utf8(prepared.raw_message).unwrap();
        assert!(!raw.contains("\r\nTo:"));
        assert!(!raw.contains("\r\nCc:"));
        assert!(!raw.contains("\r\nBcc:"));
        assert!(!raw.contains("hidden@example.com"));

        assert!(prepare_from_durable_payload(
            &payload(serde_json::json!({
                "recipients": "",
                "ccRecipients": "",
                "bccRecipients": ""
            })),
            &[],
        )
        .is_err());
    }

    #[test]
    fn mixed_message_preserves_binary_attachments_and_rfc2231_filename() {
        let attachments = [
            OutgoingAttachment {
                filename: "résumé.pdf".into(),
                media_type: "application/pdf".into(),
                bytes: vec![0, 1, 2, 0xff],
                content_id: Some("document-1@example.com".into()),
                disposition: AttachmentDisposition::Attachment,
            },
            OutgoingAttachment {
                filename: "pixel.png".into(),
                media_type: "image/png".into(),
                bytes: b"pixel".to_vec(),
                content_id: Some("pixel-1@example.com".into()),
                disposition: AttachmentDisposition::Inline,
            },
        ];
        let prepared = prepare_from_durable_payload(&payload(serde_json::json!({})), &attachments)
            .expect("mixed message");
        let raw = String::from_utf8(prepared.raw_message).unwrap();
        assert!(raw.contains("Content-Type: multipart/mixed;"));
        assert!(raw.contains("filename*=utf-8''r%C3%A9sum%C3%A9.pdf"));
        assert!(raw.contains("Content-ID: <document-1@example.com>"));
        assert!(raw.contains("Content-ID: <pixel-1@example.com>"));
        let parsed = crate::content::parse_mime(raw.as_bytes()).expect("self-ingested MIME");
        assert_eq!(parsed.attachments.len(), 2);
        assert_eq!(parsed.attachments[0].filename, "résumé.pdf");
        assert_eq!(parsed.attachments[0].content_id, "document-1@example.com");
        assert_eq!(parsed.attachments[0].bytes, vec![0, 1, 2, 0xff]);
        assert_eq!(parsed.attachments[1].content_id, "pixel-1@example.com");
        assert_eq!(parsed.attachments[1].bytes, b"pixel");
    }

    #[test]
    fn text_media_type_attachments_preserve_exact_binary_line_endings() {
        let original = b"a\nb\0c\rd\r\ne".to_vec();
        let attachments = [OutgoingAttachment {
            filename: "exact.txt".into(),
            media_type: "text/plain".into(),
            bytes: original.clone(),
            content_id: None,
            disposition: AttachmentDisposition::Attachment,
        }];
        let prepared = prepare_from_durable_payload(&payload(serde_json::json!({})), &attachments)
            .expect("exact text attachment");
        let parsed = crate::content::parse_mime(&prepared.raw_message).expect("self-ingested MIME");
        assert_eq!(parsed.attachments[0].bytes, original);
    }

    #[test]
    fn plain_only_message_and_empty_body_are_valid() {
        for body in ["plain", ""] {
            let prepared = prepare_from_durable_payload(
                &payload(serde_json::json!({"body": body, "bodyHtml": ""})),
                &[],
            )
            .expect("plain message");
            let raw = String::from_utf8(prepared.raw_message).unwrap();
            assert!(raw.contains("Content-Type: text/plain; charset=\"utf-8\""));
            assert!(!raw.contains("multipart/alternative"));
            crate::content::parse_mime(raw.as_bytes()).expect("plain MIME parses");
        }
    }

    #[test]
    fn unsafe_headers_mailboxes_ids_and_smtputf8_are_rejected() {
        for override_value in [
            serde_json::json!({"subject": "hello\r\nBcc: injected@example.com"}),
            serde_json::json!({"recipients": "Jane <jane@example.com> trailing"}),
            serde_json::json!({"recipients": "josé@example.com"}),
            serde_json::json!({"senderEmail": "one@example.com, two@example.com"}),
            serde_json::json!({"submissionMessageId": "<safe@example.com>\r\nX-Bad: yes"}),
            serde_json::json!({"submissionMessageId": "<safe>@example.com>"}),
            serde_json::json!({"inReplyTo": "parent@example.com"}),
            serde_json::json!({"snapshotVersion": 1}),
        ] {
            assert!(prepare_from_durable_payload(&payload(override_value), &[]).is_err());
        }
    }

    #[test]
    fn attachment_metadata_and_aggregate_bounds_are_enforced() {
        let invalid_type = [OutgoingAttachment {
            filename: "safe.bin".into(),
            media_type: "application/octet-stream\r\nX-Bad: yes".into(),
            bytes: vec![1],
            content_id: None,
            disposition: AttachmentDisposition::Attachment,
        }];
        assert!(
            prepare_from_durable_payload(&payload(serde_json::json!({})), &invalid_type).is_err()
        );

        let oversized = [OutgoingAttachment {
            filename: "safe.bin".into(),
            media_type: "application/octet-stream".into(),
            bytes: vec![0; crate::mime_ingest::MAX_ATTACHMENT_BYTES + 1],
            content_id: None,
            disposition: AttachmentDisposition::Attachment,
        }];
        assert!(prepare_from_durable_payload(&payload(serde_json::json!({})), &oversized).is_err());

        let exact_total = [OutgoingAttachment {
            filename: "exact.bin".into(),
            media_type: "application/octet-stream".into(),
            bytes: vec![0; MAX_TOTAL_OUTGOING_ATTACHMENT_BYTES],
            content_id: None,
            disposition: AttachmentDisposition::Attachment,
        }];
        let exact = prepare_from_durable_payload(&payload(serde_json::json!({})), &exact_total)
            .expect("exact outgoing attachment cap");
        assert!(exact.raw_message.len() < crate::mime_ingest::MAX_RAW_MESSAGE_BYTES);

        let over_total = [
            OutgoingAttachment {
                filename: "first.bin".into(),
                media_type: "application/octet-stream".into(),
                bytes: vec![0; MAX_TOTAL_OUTGOING_ATTACHMENT_BYTES / 2],
                content_id: None,
                disposition: AttachmentDisposition::Attachment,
            },
            OutgoingAttachment {
                filename: "second.bin".into(),
                media_type: "application/octet-stream".into(),
                bytes: vec![0; MAX_TOTAL_OUTGOING_ATTACHMENT_BYTES / 2 + 1],
                content_id: None,
                disposition: AttachmentDisposition::Attachment,
            },
        ];
        assert!(
            prepare_from_durable_payload(&payload(serde_json::json!({})), &over_total).is_err()
        );

        for invalid in [
            OutgoingAttachment {
                filename: "unsafe\tname.bin".into(),
                media_type: "application/octet-stream".into(),
                bytes: vec![1],
                content_id: None,
                disposition: AttachmentDisposition::Attachment,
            },
            OutgoingAttachment {
                filename: "safe.bin".into(),
                media_type: "application/octet-stream".into(),
                bytes: vec![1],
                content_id: Some("missing-at-sign".into()),
                disposition: AttachmentDisposition::Inline,
            },
        ] {
            assert!(
                prepare_from_durable_payload(&payload(serde_json::json!({})), &[invalid]).is_err()
            );
        }

        let duplicate_content_ids = (0..2)
            .map(|index| OutgoingAttachment {
                filename: format!("duplicate-{index}.png"),
                media_type: "image/png".into(),
                bytes: vec![index],
                content_id: Some("same@example.com".into()),
                disposition: AttachmentDisposition::Inline,
            })
            .collect::<Vec<_>>();
        assert!(prepare_from_durable_payload(
            &payload(serde_json::json!({})),
            &duplicate_content_ids,
        )
        .is_err());
    }

    #[test]
    fn deterministic_boundary_selection_skips_body_and_binary_collisions() {
        let attachments = [OutgoingAttachment {
            filename: "safe.bin".into(),
            media_type: "application/octet-stream".into(),
            bytes: b"contains mux-mixed-fixed-1 too".to_vec(),
            content_id: None,
            disposition: AttachmentDisposition::Attachment,
        }];
        let boundary = collision_free_boundary(
            "mixed",
            "fixed",
            "contains mux-mixed-fixed exactly",
            "",
            &attachments,
        )
        .expect("collision-free boundary");
        assert_eq!(boundary, "mux-mixed-fixed-2");
    }

    #[test]
    fn generated_inline_content_ids_are_valid_and_unique() {
        let attachments = (0..2)
            .map(|index| OutgoingAttachment {
                filename: format!("inline-{index}.png"),
                media_type: "image/png".into(),
                bytes: vec![index],
                content_id: None,
                disposition: AttachmentDisposition::Inline,
            })
            .collect::<Vec<_>>();
        let prepared = prepare_from_durable_payload(&payload(serde_json::json!({})), &attachments)
            .expect("inline content IDs");
        let raw = String::from_utf8(prepared.raw_message).unwrap();
        assert!(raw.contains("Content-ID: <inline-0-"));
        assert!(raw.contains("Content-ID: <inline-1-"));
        assert_eq!(raw.matches("Content-ID: <inline-").count(), 2);
    }
}
