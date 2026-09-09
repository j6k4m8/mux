//! Bounding and validating everything that crosses into the store.
//!
//! Recipient syntax, field limits, send fingerprints, and the cursor signing
//! key. None of it touches mailbox state; it decides whether a value is
//! allowed to become mailbox state.

use rusqlite::{Connection, OptionalExtension, Transaction};
use sha2::{Digest, Sha256};

use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn send_content_fingerprint(
    submission_message_id: &str,
    account_id: &str,
    sender_email: &str,
    recipients: &str,
    cc_recipients: &str,
    bcc_recipients: &str,
    subject: &str,
    body: &str,
    body_html: &str,
    reply_to_thread_id: Option<i64>,
    draft_revision: i64,
) -> String {
    let mut fingerprint = Sha256::new();
    fingerprint.update(b"mux-send-snapshot-v1\0");
    for field in [
        submission_message_id,
        account_id,
        sender_email,
        recipients,
        cc_recipients,
        bcc_recipients,
        subject,
        body,
        body_html,
    ] {
        fingerprint.update((field.len() as u64).to_be_bytes());
        fingerprint.update(field.as_bytes());
    }
    fingerprint.update(reply_to_thread_id.unwrap_or_default().to_be_bytes());
    fingerprint.update(draft_revision.to_be_bytes());
    fingerprint
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub(super) fn send_content_fingerprint_v2(
    submission_message_id: &str,
    account_id: &str,
    sender_email: &str,
    recipients: &str,
    cc_recipients: &str,
    bcc_recipients: &str,
    subject: &str,
    body: &str,
    body_html: &str,
    reply_to_thread_id: Option<i64>,
    draft_revision: i64,
    queued_at_ms: i64,
    in_reply_to: Option<&str>,
    references: &[String],
) -> String {
    let mut fingerprint = Sha256::new();
    fingerprint.update(b"mux-send-snapshot-v2\0");
    for field in [
        submission_message_id,
        account_id,
        sender_email,
        recipients,
        cc_recipients,
        bcc_recipients,
        subject,
        body,
        body_html,
    ] {
        fingerprint.update((field.len() as u64).to_be_bytes());
        fingerprint.update(field.as_bytes());
    }
    fingerprint.update(reply_to_thread_id.unwrap_or_default().to_be_bytes());
    fingerprint.update(draft_revision.to_be_bytes());
    fingerprint.update(queued_at_ms.to_be_bytes());
    let in_reply_to = in_reply_to.unwrap_or_default();
    fingerprint.update((in_reply_to.len() as u64).to_be_bytes());
    fingerprint.update(in_reply_to.as_bytes());
    fingerprint.update((references.len() as u64).to_be_bytes());
    for reference in references {
        fingerprint.update((reference.len() as u64).to_be_bytes());
        fingerprint.update(reference.as_bytes());
    }
    fingerprint
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn send_content_fingerprint_v3(
    submission_message_id: &str,
    account_id: &str,
    sender_email: &str,
    recipients: &str,
    cc_recipients: &str,
    bcc_recipients: &str,
    subject: &str,
    body: &str,
    body_html: &str,
    reply_to_thread_id: Option<i64>,
    draft_revision: i64,
    queued_at_ms: i64,
    in_reply_to: Option<&str>,
    references: &[String],
    client_correlation_id: &str,
    provider_kind: &str,
    remote_thread_id: Option<&str>,
) -> String {
    let mut fingerprint = Sha256::new();
    fingerprint.update(b"mux-send-snapshot-v3\0");
    for field in [
        submission_message_id,
        account_id,
        sender_email,
        recipients,
        cc_recipients,
        bcc_recipients,
        subject,
        body,
        body_html,
    ] {
        fingerprint.update((field.len() as u64).to_be_bytes());
        fingerprint.update(field.as_bytes());
    }
    fingerprint.update(reply_to_thread_id.unwrap_or_default().to_be_bytes());
    fingerprint.update(draft_revision.to_be_bytes());
    fingerprint.update(queued_at_ms.to_be_bytes());
    let in_reply_to = in_reply_to.unwrap_or_default();
    fingerprint.update((in_reply_to.len() as u64).to_be_bytes());
    fingerprint.update(in_reply_to.as_bytes());
    fingerprint.update((references.len() as u64).to_be_bytes());
    for reference in references {
        fingerprint.update((reference.len() as u64).to_be_bytes());
        fingerprint.update(reference.as_bytes());
    }
    for field in [
        client_correlation_id,
        provider_kind,
        remote_thread_id.unwrap_or_default(),
    ] {
        fingerprint.update((field.len() as u64).to_be_bytes());
        fingerprint.update(field.as_bytes());
    }
    fingerprint
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub(super) fn send_payload_for_draft(
    draft: &DraftSummary,
    message_id: String,
    submission_message_id: String,
    queued_at_ms: i64,
    in_reply_to: Option<String>,
    references: Vec<String>,
    client_correlation_id: String,
    provider_kind: String,
    remote_thread_id: Option<String>,
) -> SendPayload {
    let content_fingerprint_hex = send_content_fingerprint_v3(
        &submission_message_id,
        &draft.account_id,
        &draft.account_email,
        &draft.recipients,
        &draft.cc_recipients,
        &draft.bcc_recipients,
        &draft.subject,
        &draft.body,
        &draft.body_html,
        draft.reply_to_thread_id,
        draft.revision,
        queued_at_ms,
        in_reply_to.as_deref(),
        &references,
        &client_correlation_id,
        &provider_kind,
        remote_thread_id.as_deref(),
    );
    SendPayload {
        draft_id: draft.id.clone(),
        message_id,
        snapshot_version: Some(3),
        submission_message_id: Some(submission_message_id),
        account_id: Some(draft.account_id.clone()),
        sender_email: Some(draft.account_email.clone()),
        recipients: Some(draft.recipients.clone()),
        cc_recipients: Some(draft.cc_recipients.clone()),
        bcc_recipients: Some(draft.bcc_recipients.clone()),
        subject: Some(draft.subject.clone()),
        body: Some(draft.body.clone()),
        body_html: Some(draft.body_html.clone()),
        reply_to_thread_id: draft.reply_to_thread_id,
        draft_revision: Some(draft.revision),
        content_fingerprint_hex: Some(content_fingerprint_hex),
        queued_at_ms: Some(queued_at_ms),
        in_reply_to,
        references,
        client_correlation_id: Some(client_correlation_id),
        provider_kind: Some(provider_kind),
        remote_thread_id,
    }
}

pub(super) fn send_payload_has_complete_snapshot(payload: &SendPayload) -> bool {
    payload.submission_message_id.is_some()
        && payload.account_id.is_some()
        && payload.sender_email.is_some()
        && payload.recipients.is_some()
        && payload.cc_recipients.is_some()
        && payload.bcc_recipients.is_some()
        && payload.subject.is_some()
        && payload.body.is_some()
        && payload.body_html.is_some()
        && payload.draft_revision.is_some()
        && payload.content_fingerprint_hex.is_some()
        && match payload.snapshot_version {
            None | Some(1) => true,
            Some(2) => payload.queued_at_ms.is_some(),
            Some(3) => {
                payload.queued_at_ms.is_some()
                    && payload.client_correlation_id.is_some()
                    && payload.provider_kind.is_some()
            }
            Some(_) => false,
        }
}

pub(super) fn reply_threading_headers(
    transaction: &Transaction<'_>,
    reply_to_thread_id: Option<i64>,
) -> Result<(Option<String>, Vec<String>), StoreError> {
    let Some(thread_id) = reply_to_thread_id else {
        return Ok((None, Vec::new()));
    };
    let persisted_parent = transaction
        .query_row(
            "SELECT internet_message_id, references_json FROM messages
             WHERE thread_id = ?1 AND remote_deleted = 0 AND internet_message_id <> ''
             ORDER BY sent_at DESC, id DESC LIMIT 1",
            [thread_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    if let Some((parent_message_id, references_json)) = persisted_parent {
        crate::internet_message::validate_message_id(&parent_message_id)
            .map_err(|()| StoreError::Validation("Stored parent Message-ID is invalid".into()))?;
        let references: Vec<String> = serde_json::from_str(&references_json)?;
        crate::internet_message::validate_references(&references)
            .map_err(|()| StoreError::Validation("Stored parent References are invalid".into()))?;
        return Ok(threading_headers_for_parent(parent_message_id, references));
    }
    let parent_json = transaction
        .query_row(
            "SELECT payload_json FROM operations
             WHERE thread_id = ?1 AND field = 'send' AND state = 'confirmed'
             ORDER BY confirmed_at DESC, rowid DESC LIMIT 1",
            [thread_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    let Some(parent_json) = parent_json else {
        return Ok((None, Vec::new()));
    };
    let parent: SendPayload = serde_json::from_str(&parent_json)?;
    send_projection_fields(transaction, &parent)?;
    let parent_message_id = parent.submission_message_id.ok_or_else(|| {
        StoreError::Validation("Confirmed parent send has no submission identity".into())
    })?;
    Ok(threading_headers_for_parent(
        parent_message_id,
        parent.references,
    ))
}

pub(super) fn threading_headers_for_parent(
    parent_message_id: String,
    mut references: Vec<String>,
) -> (Option<String>, Vec<String>) {
    if references.last() != Some(&parent_message_id) {
        references.push(parent_message_id.clone());
    }
    if references.len() > crate::internet_message::MAX_REFERENCES {
        references.drain(..references.len() - crate::internet_message::MAX_REFERENCES);
    }
    while references.iter().map(String::len).sum::<usize>()
        > crate::internet_message::MAX_REFERENCE_BYTES
        && references.len() > 1
    {
        references.remove(0);
    }
    (Some(parent_message_id), references)
}

pub(super) fn validate_invitation_summary(
    invitation: &InvitationSummary,
) -> Result<(), StoreError> {
    bounded_text(&invitation.uid, "invitation uid", 2_000)?;
    bounded_text(&invitation.title, "invitation title", 2_000)?;
    bounded_text(&invitation.timezone, "invitation timezone", 200)?;
    bounded_text(&invitation.location, "invitation location", 10_000)?;
    bounded_text(&invitation.organizer, "invitation organizer", 10_000)?;
    bounded_text(&invitation.attendees, "invitation attendees", 50_000)?;
    if let Some(conflict) = &invitation.conflict_text {
        bounded_text(conflict, "invitation conflict", 50_000)?;
    }
    if invitation.start_at < 0 || invitation.end_at < invitation.start_at {
        return Err(StoreError::Validation(
            "Invitation timestamps are invalid".into(),
        ));
    }
    Ok(())
}

pub(super) fn validate_operation_activity(
    operation: &OperationActivitySummary,
) -> Result<(), StoreError> {
    bounded_text(&operation.id, "operation id", 256)?;
    bounded_text(&operation.field, "operation field", 200)?;
    bounded_text(&operation.kind, "operation kind", 200)?;
    bounded_text(&operation.state, "operation state", 32)?;
    if let Some(undo_of) = &operation.undo_of {
        bounded_text(undo_of, "operation undo id", 256)?;
    }
    if operation.created_at < 0
        || operation.not_before < 0
        || operation.confirmed_at.is_some_and(|value| value < 0)
        || operation.attempts < 0
    {
        return Err(StoreError::Validation(
            "Operation activity contains an invalid numeric value".into(),
        ));
    }
    Ok(())
}

pub(super) fn json_array_growth(serialized_items: usize, item_count: usize) -> usize {
    serialized_items.saturating_add(item_count.saturating_sub(1))
}

pub(super) fn message_detail_budget_error() -> StoreError {
    StoreError::Validation(format!(
        "Message detail exceeds the {}-byte IPC limit",
        crate::ipc_boundary::MESSAGE_DETAIL_BYTES
    ))
}

pub(super) fn bounded_text(value: &str, field: &str, maximum: usize) -> Result<String, StoreError> {
    if value.chars().count() > maximum {
        return Err(StoreError::Validation(format!(
            "{field} exceeds the {maximum} character limit"
        )));
    }
    Ok(value.to_string())
}

pub(super) fn bounded_header(
    value: &str,
    field: &str,
    maximum: usize,
) -> Result<String, StoreError> {
    if value
        .chars()
        .any(|character| character.is_control() && character != '\t')
    {
        return Err(StoreError::Validation(format!(
            "{field} contains invalid control characters"
        )));
    }
    bounded_text(value, field, maximum)
}

pub(super) fn exact_account_id(value: Option<&str>) -> Result<Option<String>, StoreError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_empty() {
        return Err(StoreError::Validation(
            "accountId must not be empty when present".into(),
        ));
    }
    if value.len() > 200 {
        return Err(StoreError::Validation(
            "accountId exceeds the 200-byte limit".into(),
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(StoreError::Validation(
            "accountId contains invalid control characters".into(),
        ));
    }
    Ok(Some(value.to_string()))
}

/// Exactly `#rrggbb`, handed back lowercased. The interface writes an account's
/// color into inline styles, so nothing looser than a literal color is kept:
/// no names, no shorthand, and nothing that could carry a `url(` along.
pub(super) fn account_color(value: &str) -> Result<String, StoreError> {
    let digits = value
        .strip_prefix('#')
        .filter(|digits| digits.len() == 6 && digits.bytes().all(|byte| byte.is_ascii_hexdigit()));
    match digits {
        Some(digits) => Ok(format!("#{}", digits.to_ascii_lowercase())),
        None => Err(StoreError::Validation(
            "Account color must be a hex color like #5168f4".into(),
        )),
    }
}

pub(super) fn load_or_create_cursor_signing_key(
    connection: &Connection,
) -> Result<[u8; 32], StoreError> {
    let existing = connection
        .query_row(
            "SELECT value FROM meta WHERE key = 'ipc_cursor_signing_key_v1'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(encoded) = existing {
        return decode_cursor_signing_key(&encoded).ok_or_else(|| {
            StoreError::InvalidSchemaVersion("cursor signing metadata is malformed".into())
        });
    }

    let mut key = [0_u8; 32];
    OsRng.fill_bytes(&mut key);
    let encoded = encode_cursor_signing_key(&key);
    connection.execute(
        "INSERT OR IGNORE INTO meta(key, value) VALUES('ipc_cursor_signing_key_v1', ?1)",
        [encoded],
    )?;
    let durable = connection.query_row(
        "SELECT value FROM meta WHERE key = 'ipc_cursor_signing_key_v1'",
        [],
        |row| row.get::<_, String>(0),
    )?;
    decode_cursor_signing_key(&durable).ok_or_else(|| {
        StoreError::InvalidSchemaVersion("cursor signing metadata is malformed".into())
    })
}

pub(super) fn encode_cursor_signing_key(key: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(64);
    for byte in key {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

pub(super) fn decode_cursor_signing_key(encoded: &str) -> Option<[u8; 32]> {
    if encoded.len() != 64 || !encoded.is_ascii() {
        return None;
    }
    let mut key = [0_u8; 32];
    for (index, pair) in encoded.as_bytes().chunks_exact(2).enumerate() {
        let high = decode_hex_nibble(pair[0])?;
        let low = decode_hex_nibble(pair[1])?;
        key[index] = (high << 4) | low;
    }
    Some(key)
}

fn decode_hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

pub(super) fn validate_recipient_list(recipients: &str) -> Result<(), StoreError> {
    let mailboxes = split_recipient_list(recipients)?;
    if mailboxes.is_empty()
        || mailboxes
            .iter()
            .any(|entry| !is_conservative_mailbox(entry))
    {
        return Err(StoreError::Validation(
            "Enter one or more valid email mailboxes".into(),
        ));
    }
    Ok(())
}

/// Splits the subset of RFC mailbox lists Mux can safely submit. This deliberately
/// handles quoted display names and angle addresses without claiming to parse MIME
/// groups, comments, encoded words, or every RFC 5322 address form.
pub(super) fn split_recipient_list(recipients: &str) -> Result<Vec<&str>, StoreError> {
    let source = recipients.trim();
    bounded_header(source, "recipients", MAX_RECIPIENT_HEADER_CHARS)?;
    if source.is_empty() {
        return Ok(Vec::new());
    }

    let mut mailboxes = Vec::new();
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
            '<' if inside_angle => return Err(invalid_recipient_syntax()),
            '<' => inside_angle = true,
            '>' if !inside_angle => return Err(invalid_recipient_syntax()),
            '>' => inside_angle = false,
            ',' | ';' if !inside_angle => {
                push_recipient_mailbox(&mut mailboxes, source[start..index].trim())?;
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }

    if quoted || escaped || inside_angle {
        return Err(invalid_recipient_syntax());
    }
    push_recipient_mailbox(&mut mailboxes, source[start..].trim())?;
    Ok(mailboxes)
}

fn push_recipient_mailbox<'a>(
    mailboxes: &mut Vec<&'a str>,
    mailbox: &'a str,
) -> Result<(), StoreError> {
    if mailbox.is_empty() || mailboxes.len() >= MAX_RECIPIENT_MAILBOXES {
        return Err(invalid_recipient_syntax());
    }
    mailboxes.push(mailbox);
    Ok(())
}

fn invalid_recipient_syntax() -> StoreError {
    StoreError::Validation("Recipients contain a malformed mailbox list".into())
}

pub(super) fn is_conservative_mailbox(mailbox: &str) -> bool {
    let opening_angles = mailbox.match_indices('<').collect::<Vec<_>>();
    let closing_angles = mailbox.match_indices('>').collect::<Vec<_>>();
    let address = match (opening_angles.as_slice(), closing_angles.as_slice()) {
        ([], []) => mailbox.trim(),
        ([(opening, _)], [(closing, _)]) if opening < closing => {
            if !mailbox[closing + 1..].trim().is_empty() {
                return false;
            }
            mailbox[opening + 1..*closing].trim()
        }
        _ => return false,
    };
    is_conservative_address(address)
}

pub(super) fn is_conservative_address(address: &str) -> bool {
    if address.is_empty() || address.len() > 320 || !address.is_ascii() {
        return false;
    }
    let Some((local, domain)) = address.split_once('@') else {
        return false;
    };
    if local.is_empty()
        || local.len() > 64
        || local.starts_with('.')
        || local.ends_with('.')
        || local.contains("..")
        || domain.is_empty()
        || domain.len() > 255
        || domain.contains('@')
        || !local.bytes().all(|byte| {
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
        })
    {
        return false;
    }

    let labels = domain.split('.').collect::<Vec<_>>();
    labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

pub(super) fn snippet(body: &str) -> String {
    let normalized = body.split_whitespace().collect::<Vec<_>>().join(" ");
    normalized.chars().take(180).collect()
}

pub(super) fn join_visible_recipients(to: &str, cc: &str) -> String {
    match (to.trim().is_empty(), cc.trim().is_empty()) {
        (false, false) => format!("{}, {}", to.trim(), cc.trim()),
        (false, true) => to.trim().to_string(),
        (true, false) => cc.trim().to_string(),
        (true, true) => String::new(),
    }
}
