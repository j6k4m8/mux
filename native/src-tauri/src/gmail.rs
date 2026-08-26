//! Gmail progressive synchronization and desired-state mailbox mutations.
//!
//! Gmail wire responses and bearer authority remain behind this Rust module.
//! The adapter emits only provider-neutral batches and durable continuations;
//! Svelte never sees Gmail IDs, page tokens, history IDs, or credentials.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine as _;
use reqwest::blocking::{Client, RequestBuilder};
use reqwest::redirect::Policy;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::content::{parse_mime, ParsedMailbox, SafeMessageContent};
use crate::provider::{
    ContainerDisplayName, ContainerKind, ContainerMembership, ContainerMembershipChange,
    ContainerRole, MuxAccountId, NormalizedPlainBody, OpaqueSyncCursor, ProviderBatch,
    ProviderBatchId, ProviderCapabilities, ProviderCapability, ProviderCategory, ProviderContainer,
    ProviderEmail, ProviderMessageKeyword, ProviderMessageUpsert, ProviderParticipants,
    ProviderRecipients, ProviderRevision, ProviderSenderName, ProviderSnippet, ProviderSubject,
    ProviderSyncCursor, ProviderThreadMessageCount, ProviderThreadUpsert, ProviderTombstone,
    RemoteContainerId, RemoteContainerIdentity, RemoteMessageId, RemoteMessageIdentity,
    RemoteThreadId, RemoteThreadIdentity, SyncCursorScope, TombstoneTarget, UnixMillis,
};
use crate::worker::{
    enqueue_in_transaction, ClaimedWork, NewWorkItem, ProviderReconciliationPage,
    ProviderSendAcceptance, ProviderSyncContinuation, ProviderSyncPage, ReconciliationObjectKind,
    RestrictedMessageContent, ThreadMutationPayload, WorkKind, WorkerAdapter, WorkerError,
    WorkerExecutionContext, WorkerOutcome, WorkerProjection,
};

const WORK_FORMAT_VERSION: u8 = 1;
pub(crate) const GMAIL_ACCOUNT_SCOPE: &str = "sync:gmail:account:v1";
const SYNC_SCOPE: &str = GMAIL_ACCOUNT_SCOPE;
const MESSAGE_PAGE_SIZE: usize = 10;
const MAX_LABELS: usize = 10_000;
const MAX_MESSAGE_LABELS: usize = 1_000;
const LABEL_PAGE_SIZE: usize = 1_000;
const MAX_HISTORY_CHANGES: usize = 4_000;
const MAX_REMOTE_ID_BYTES: usize = 2_048;
const MAX_PAGE_TOKEN_BYTES: usize = 16 * 1024;
const MAX_HISTORY_ID_BYTES: usize = 256;
const MAX_GENERATION_BYTES: usize = 256;
const MAX_RAW_JSON_VALUE_BYTES: usize = 8 * 1024 * 1024;
const MAX_BODY_TEXT_BYTES: usize = 2 * 1024 * 1024;
const MAX_SUBJECT_BYTES: usize = 2_000;
const MAX_PARTICIPANTS_BYTES: usize = 16_000;
const MAX_SNIPPET_BYTES: usize = 2_000;
const GMAIL_ORIGIN: &str = "https://gmail.googleapis.com/";
const MAX_SMALL_RESPONSE_BYTES: u64 = 64 * 1024;
const MAX_LIST_RESPONSE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_LABEL_RESPONSE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_RAW_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_SEND_RESPONSE_BYTES: u64 = 64 * 1024;
const MAX_SEND_EVIDENCE_RESULTS: usize = 10;
const MAX_SEND_EVIDENCE_FUTURE_SKEW_MS: i64 = 5 * 60 * 1_000;
const SEND_RECONCILIATION_VERSION: u8 = 1;
const SEND_RECONCILIATION_SCOPE_PREFIX: &str = "outgoing:gmail:v1:";

pub(crate) fn gmail_send_scope(operation_id: &str) -> String {
    format!("{SEND_RECONCILIATION_SCOPE_PREFIX}{operation_id}")
}

pub(crate) fn is_gmail_owned_work(work: &ClaimedWork) -> bool {
    is_gmail_sync_work(work)
        || (work.kind == WorkKind::Send && work.scope.starts_with(SEND_RECONCILIATION_SCOPE_PREFIX))
        || is_gmail_send_reconciliation_work(work)
}

pub(crate) fn is_gmail_sync_work(work: &ClaimedWork) -> bool {
    work.kind == WorkKind::Sync && work.scope == SYNC_SCOPE
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GmailSendReconciliationWork {
    version: u8,
    provider: String,
    original_work_id: String,
    operation_id: String,
    send_payload_fingerprint_hex: String,
    submission_message_id: String,
    client_correlation_id: String,
    frozen_remote_thread_id: Option<String>,
    queued_at_ms: i64,
}

fn is_gmail_send_reconciliation_work(work: &ClaimedWork) -> bool {
    work.kind == WorkKind::Sync && work.scope.starts_with(SEND_RECONCILIATION_SCOPE_PREFIX)
}

pub(crate) fn gmail_send_reconciliation_work(
    account_id: &str,
    operation_id: &str,
    original_work_id: &str,
    scope: &str,
    send_payload_json: &str,
    available_at: i64,
) -> Result<NewWorkItem, String> {
    let prepared = crate::outgoing::prepare_from_durable_payload(send_payload_json, &[])
        .map_err(|_| "Gmail reconciliation rejected the outgoing snapshot".to_string())?;
    if prepared.provider_kind.as_deref() != Some("gmail") {
        return Err("Gmail reconciliation requires a Gmail snapshot".into());
    }
    let client_correlation_id = prepared
        .client_correlation_id
        .ok_or_else(|| "Gmail reconciliation requires a client correlation".to_string())?;
    for value in [account_id, operation_id, original_work_id, scope] {
        validate_bounded(value, 4_096).map_err(str::to_owned)?;
    }
    if scope != gmail_send_scope(operation_id) || available_at < 0 {
        return Err("Gmail reconciliation scope or timestamp is invalid".into());
    }
    let payload = GmailSendReconciliationWork {
        version: SEND_RECONCILIATION_VERSION,
        provider: "gmail".into(),
        original_work_id: original_work_id.into(),
        operation_id: operation_id.into(),
        send_payload_fingerprint_hex: hex(&Sha256::digest(send_payload_json.as_bytes())),
        submission_message_id: prepared.message_id,
        client_correlation_id,
        frozen_remote_thread_id: prepared.remote_thread_id,
        queued_at_ms: prepared.queued_at_ms,
    };
    let payload_json = serde_json::to_string(&payload)
        .map_err(|_| "Gmail reconciliation serialization failed".to_string())?;
    Ok(NewWorkItem {
        id: format!("reconcile_{original_work_id}"),
        account_id: account_id.into(),
        operation_id: None,
        kind: WorkKind::Sync,
        scope: scope.into(),
        ordering_key: payload.submission_message_id,
        payload_json,
        priority: 90,
        available_at,
        max_attempts: 8,
    })
}

pub(crate) fn schedule_initial_syncs(
    database_path: &Path,
    now_ms: i64,
) -> Result<usize, WorkerError> {
    let mut connection = Connection::open(database_path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let accounts = {
        let mut statement = transaction.prepare(
            "SELECT account.account_id
             FROM provider_accounts account
             WHERE account.provider_kind = 'gmail'
               AND account.auth_state = 'ready'
               AND account.sync_state = 'never_synced'
               AND account.credential_ref IS NOT NULL
               AND NOT EXISTS (
                 SELECT 1 FROM provider_work_items work
                 WHERE work.account_id = account.account_id
                   AND work.kind = 'sync' AND work.scope = ?1
               )
             ORDER BY account.account_id",
        )?;
        let values = statement
            .query_map([SYNC_SCOPE], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        values
    };
    for (index, account_id) in accounts.iter().enumerate() {
        let request_id = format!("startup-{now_ms}-{index}");
        let work = initial_sync_work(account_id, &request_id, now_ms)
            .map_err(|error| WorkerError::Validation(error.into()))?;
        enqueue_in_transaction(&transaction, work, now_ms)?;
        transaction.execute(
            "UPDATE provider_accounts
             SET sync_state = 'scheduled', updated_at = ?2
             WHERE account_id = ?1 AND sync_state = 'never_synced'",
            params![account_id, now_ms],
        )?;
    }
    transaction.commit()?;
    Ok(accounts.len())
}

/// Starts a new bounded delta cycle from the last fully committed Gmail
/// history cursor after a successful vault unlock. Terminal work rows remain
/// an audit trail, so only non-terminal work suppresses this scheduling pass.
pub(crate) fn schedule_resumable_syncs(
    database_path: &Path,
    now_ms: i64,
) -> Result<usize, WorkerError> {
    schedule_cursor_syncs(database_path, now_ms, false)
}

/// Schedules a delta cycle only for accounts whose own refresh cadence has
/// elapsed since their last successful sync. Driven by the refresh timer.
pub(crate) fn schedule_due_syncs(database_path: &Path, now_ms: i64) -> Result<usize, WorkerError> {
    schedule_cursor_syncs(database_path, now_ms, true)
}

fn schedule_cursor_syncs(
    database_path: &Path,
    now_ms: i64,
    only_due: bool,
) -> Result<usize, WorkerError> {
    if now_ms < 0 {
        return Err(WorkerError::Validation(
            "Gmail sync timestamp is invalid".into(),
        ));
    }
    let mut connection = Connection::open(database_path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let accounts = {
        let mut statement = transaction.prepare(
            "SELECT account.account_id, cursor.cursor
             FROM provider_accounts account
             JOIN provider_sync_cursors cursor
               ON cursor.account_id = account.account_id AND cursor.scope = 'a:v1'
             WHERE account.provider_kind = 'gmail'
               AND account.auth_state = 'ready'
               AND account.sync_state IN ('idle', 'scheduled')
               AND account.credential_ref IS NOT NULL
               AND NOT EXISTS (
                 SELECT 1 FROM provider_work_items work
                 WHERE work.account_id = account.account_id
                   AND work.kind = 'sync' AND work.scope = ?1
                   AND work.state IN (
                     'queued', 'executing', 'retry_wait', 'rate_limited',
                     'authentication_blocked'
                   )
               )
               AND (?3 = 0 OR account.last_sync_at IS NULL
                 OR ?2 - account.last_sync_at >= account.refresh_seconds * 1000)
             ORDER BY account.account_id",
        )?;
        let values = statement
            .query_map(params![SYNC_SCOPE, now_ms, i64::from(only_due)], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        values
    };
    let mut scheduled = 0;
    for (index, (account_id, cursor)) in accounts.iter().enumerate() {
        let terminal_count = transaction.query_row(
            "SELECT COUNT(*) FROM provider_work_items
             WHERE account_id = ?1 AND kind = 'sync' AND scope = ?2",
            params![account_id, SYNC_SCOPE],
            |row| row.get::<_, i64>(0),
        )?;
        let identity = format!("refresh-{now_ms}-{index}-{terminal_count}");
        let phase = match serde_json::from_str::<GmailCursor>(cursor)
            .ok()
            .and_then(|envelope| envelope.resume_phase().ok())
        {
            Some(phase) => phase,
            None if validate_bounded(cursor, 16 * 1024).is_ok() => GmailSyncPhase::Bootstrap {
                generation_id: format!("gmail-cursor-recovery-{identity}"),
                baseline_history_id: None,
                label_offset: 0,
                page_token: None,
            },
            None => {
                transaction.execute(
                    "UPDATE provider_accounts
                     SET sync_state = 'failed', last_error_code = 'gmail_cursor_invalid',
                         updated_at = ?2
                     WHERE account_id = ?1",
                    params![account_id, now_ms],
                )?;
                continue;
            }
        };
        let request = make_work_seeded(phase, Some(cursor.clone()), &identity)
            .map_err(|error| WorkerError::Validation(error.into()))?;
        let work = work_item(account_id, request, now_ms)
            .map_err(|error| WorkerError::Validation(error.into()))?;
        enqueue_in_transaction(&transaction, work, now_ms)?;
        transaction.execute(
            "UPDATE provider_accounts
             SET sync_state = 'scheduled', updated_at = ?2
             WHERE account_id = ?1 AND sync_state IN ('idle', 'scheduled')",
            params![account_id, now_ms],
        )?;
        scheduled += 1;
    }
    transaction.commit()?;
    Ok(scheduled)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GmailAccessError {
    CredentialUnavailable,
    ReauthorizationRequired,
    Retryable,
    RateLimited { retry_after_at: i64 },
    Permanent,
}

/// Yields a zeroizing, short-lived access token only after validating the
/// configured account's current typed keychain record. Implementations must not
/// cache authority beyond the validated record.
pub(crate) struct GmailAccessGrant {
    pub access_value: Zeroizing<String>,
    pub remote_account_id: String,
}

pub(crate) trait GmailAccessSource {
    fn access_for_account(
        &self,
        account_id: &str,
        now_ms: i64,
    ) -> Result<GmailAccessGrant, GmailAccessError>;

    fn access_for_mutation(
        &self,
        account_id: &str,
        now_ms: i64,
    ) -> Result<GmailAccessGrant, GmailAccessError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GmailApiError {
    Unauthorized,
    RateLimited {
        retry_after_at: i64,
    },
    Retryable,
    Permanent,
    HistoryExpired,
    NotFound,
    /// No submission I/O occurred; a bounded retry is safe.
    PreSubmissionRetryable,
    /// The provider explicitly rejected the request without accepting it.
    RejectedBeforeSubmission,
    /// Submission I/O may have succeeded; only evidence reconciliation is safe.
    AmbiguousSubmission,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GmailProfile {
    pub email_address: String,
    pub history_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GmailLabel {
    pub id: String,
    pub name: String,
    pub system: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GmailMessageRef {
    pub id: String,
    pub thread_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GmailMessagePage {
    pub messages: Vec<GmailMessageRef>,
    pub next_page_token: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GmailMessageSnapshot {
    pub id: String,
    pub thread_id: String,
    pub label_ids: Vec<String>,
    pub snippet: String,
    pub history_id: String,
    pub internal_date_ms: i64,
    /// Gmail `format=raw` base64url. The decoded bytes always cross the
    /// existing hostile MIME boundary before becoming provider projection.
    pub raw: Option<String>,
    /// A bounded synthetic RFC 5322 header block built only from explicitly
    /// requested Gmail metadata when the raw response exceeds its memory cap.
    pub metadata_headers: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct GmailHistoryChange {
    pub message_id: String,
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GmailHistoryPage {
    pub changes: Vec<GmailHistoryChange>,
    pub next_page_token: Option<String>,
    pub history_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GmailSendAcknowledgement {
    pub message_id: String,
    pub thread_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GmailSendEvidence {
    pub message_id: String,
    pub thread_id: String,
    pub accepted_at_ms: i64,
    pub observed_message_id: String,
    pub observed_client_correlation: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GmailSendEvidencePage {
    pub exact: Vec<GmailSendEvidence>,
    pub mismatched_candidates: usize,
    pub has_more: bool,
}

/// Bounded protocol surface. The production implementation owns fixed Google
/// origins, HTTPS, redirects, timeouts, and response limits. Tests inject a
/// deterministic in-memory implementation of this exact contract.
pub(crate) trait GmailApi {
    fn get_profile(&self, access: &str) -> Result<GmailProfile, GmailApiError>;
    fn list_labels(&self, access: &str) -> Result<Vec<GmailLabel>, GmailApiError>;
    fn list_messages(
        &self,
        access: &str,
        page_token: Option<&str>,
        max_results: usize,
    ) -> Result<GmailMessagePage, GmailApiError>;
    fn get_message(
        &self,
        access: &str,
        message_id: &str,
    ) -> Result<GmailMessageSnapshot, GmailApiError>;
    fn list_history(
        &self,
        access: &str,
        start_history_id: &str,
        page_token: Option<&str>,
    ) -> Result<GmailHistoryPage, GmailApiError>;
    fn modify_thread(
        &self,
        access: &str,
        thread_id: &str,
        add_label_ids: &[String],
        remove_label_ids: &[String],
    ) -> Result<(), GmailApiError>;
    fn trash_thread(&self, access: &str, thread_id: &str) -> Result<(), GmailApiError>;
    fn untrash_thread(&self, access: &str, thread_id: &str) -> Result<(), GmailApiError>;
    /// Performs a bounded metadata-only existence check after an ambiguous
    /// mutation 404. A mutation 404 alone is not evidence that the thread was
    /// deleted: another referenced resource (for example a label) may be stale.
    fn thread_exists(&self, access: &str, thread_id: &str) -> Result<bool, GmailApiError>;
    fn send_message(
        &self,
        access: &str,
        raw_base64url: &str,
        frozen_thread_id: Option<&str>,
        before_io: &mut dyn FnMut() -> Result<(), GmailApiError>,
    ) -> Result<GmailSendAcknowledgement, GmailApiError>;
    fn find_send_evidence(
        &self,
        access: &str,
        submission_message_id: &str,
        client_correlation_id: &str,
    ) -> Result<GmailSendEvidencePage, GmailApiError>;
}

pub(crate) struct GoogleGmailApi {
    client: Client,
}

impl GoogleGmailApi {
    pub(crate) fn new() -> Result<Self, WorkerError> {
        let client = Client::builder()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(45))
            .user_agent("Mux/0.1")
            .build()
            .map_err(|_| WorkerError::Conflict("Could not initialize Gmail transport".into()))?;
        Ok(Self { client })
    }

    fn get<T: for<'de> Deserialize<'de>>(
        &self,
        request: RequestBuilder,
        max_bytes: u64,
        not_found: GmailApiError,
    ) -> Result<T, GoogleCallError> {
        let response = request.send().map_err(|error| {
            GoogleCallError::Api(if error.is_timeout() || error.is_connect() {
                GmailApiError::Retryable
            } else {
                GmailApiError::Permanent
            })
        })?;
        let status = response.status();
        let retry_after_at = retry_after_at(response.headers());
        let read_limit = if status.is_success() {
            max_bytes
        } else {
            MAX_SMALL_RESPONSE_BYTES
        };
        if response
            .content_length()
            .is_some_and(|length| length > read_limit)
        {
            return Err(GoogleCallError::TooLarge);
        }
        let mut bytes = Vec::new();
        response
            .take(read_limit.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| GoogleCallError::Api(GmailApiError::Retryable))?;
        if bytes.len() as u64 > read_limit {
            return Err(GoogleCallError::TooLarge);
        }
        if status.as_u16() == 401 {
            return Err(GoogleCallError::Api(GmailApiError::Unauthorized));
        }
        if status.as_u16() == 404 {
            return Err(GoogleCallError::Api(not_found));
        }
        if status.as_u16() == 429 {
            return Err(GoogleCallError::Api(GmailApiError::RateLimited {
                retry_after_at,
            }));
        }
        if status.as_u16() == 403 {
            return Err(GoogleCallError::Api(classify_forbidden(
                &bytes,
                retry_after_at,
            )));
        }
        if status.is_server_error() {
            return Err(GoogleCallError::Api(GmailApiError::Retryable));
        }
        if !status.is_success() {
            return Err(GoogleCallError::Api(GmailApiError::Permanent));
        }
        serde_json::from_slice(&bytes).map_err(|_| GoogleCallError::Api(GmailApiError::Permanent))
    }
}

enum GoogleCallError {
    Api(GmailApiError),
    TooLarge,
}

impl GmailApi for GoogleGmailApi {
    fn get_profile(&self, access: &str) -> Result<GmailProfile, GmailApiError> {
        let url = gmail_url(&["gmail", "v1", "users", "me", "profile"])?;
        let wire: GmailProfileWire = self
            .get(
                self.client.get(url).bearer_auth(access),
                MAX_SMALL_RESPONSE_BYTES,
                GmailApiError::Permanent,
            )
            .map_err(call_error)?;
        Ok(GmailProfile {
            email_address: wire.email_address,
            history_id: wire.history_id,
        })
    }

    fn list_labels(&self, access: &str) -> Result<Vec<GmailLabel>, GmailApiError> {
        let url = gmail_url(&["gmail", "v1", "users", "me", "labels"])?;
        let wire: GmailLabelListWire = self
            .get(
                self.client
                    .get(url)
                    .bearer_auth(access)
                    .query(&[("fields", "labels(id,name,type)")]),
                MAX_LABEL_RESPONSE_BYTES,
                GmailApiError::Permanent,
            )
            .map_err(call_error)?;
        Ok(wire
            .labels
            .into_iter()
            .map(|label| GmailLabel {
                id: label.id,
                name: label.name,
                system: label.label_type.eq_ignore_ascii_case("system"),
            })
            .collect())
    }

    fn list_messages(
        &self,
        access: &str,
        page_token: Option<&str>,
        max_results: usize,
    ) -> Result<GmailMessagePage, GmailApiError> {
        let url = gmail_url(&["gmail", "v1", "users", "me", "messages"])?;
        let mut request = self.client.get(url).bearer_auth(access).query(&[
            ("includeSpamTrash", "true"),
            ("maxResults", &max_results.to_string()),
            ("fields", "messages(id,threadId),nextPageToken"),
        ]);
        if let Some(page_token) = page_token {
            request = request.query(&[("pageToken", page_token)]);
        }
        let wire: GmailMessageListWire = self
            .get(request, MAX_LIST_RESPONSE_BYTES, GmailApiError::Permanent)
            .map_err(call_error)?;
        Ok(GmailMessagePage {
            messages: wire
                .messages
                .into_iter()
                .map(|message| GmailMessageRef {
                    id: message.id,
                    thread_id: message.thread_id,
                })
                .collect(),
            next_page_token: wire.next_page_token,
        })
    }

    fn get_message(
        &self,
        access: &str,
        message_id: &str,
    ) -> Result<GmailMessageSnapshot, GmailApiError> {
        let url = gmail_url(&["gmail", "v1", "users", "me", "messages", message_id])?;
        let request = self.client.get(url.clone()).bearer_auth(access).query(&[
            ("format", "raw"),
            (
                "fields",
                "id,threadId,labelIds,snippet,historyId,internalDate,raw",
            ),
        ]);
        let (wire, metadata_fallback): (GmailMessageWire, bool) = match self.get(
            request,
            MAX_RAW_RESPONSE_BYTES,
            GmailApiError::NotFound,
        ) {
            Ok(wire) => (wire, false),
            Err(GoogleCallError::TooLarge) => {
                let mut metadata = self.client.get(url.clone()).bearer_auth(access).query(&[
                        ("format", "metadata"),
                        (
                            "fields",
                            "id,threadId,labelIds,snippet,historyId,internalDate,payload(headers(name,value))",
                        ),
                    ]);
                for name in [
                    "Subject",
                    "From",
                    "To",
                    "Cc",
                    "Message-ID",
                    "In-Reply-To",
                    "References",
                ] {
                    metadata = metadata.query(&[("metadataHeaders", name)]);
                }
                let wire =
                    match self.get(metadata, MAX_LIST_RESPONSE_BYTES, GmailApiError::NotFound) {
                        Ok(wire) => wire,
                        // A pathological References or address header can make even
                        // Gmail's metadata response enormous. Keep the sync moving
                        // with provider identity, labels, date, and snippet only;
                        // no unbounded body or header bytes cross the boundary.
                        Err(GoogleCallError::TooLarge) => {
                            let minimal = self.client.get(url).bearer_auth(access).query(&[
                                ("format", "minimal"),
                                (
                                    "fields",
                                    "id,threadId,labelIds,snippet,historyId,internalDate",
                                ),
                            ]);
                            self.get(minimal, MAX_SMALL_RESPONSE_BYTES, GmailApiError::NotFound)
                                .map_err(call_error)?
                        }
                        Err(error) => return Err(call_error(error)),
                    };
                (wire, true)
            }
            Err(error) => return Err(call_error(error)),
        };
        let metadata_headers = metadata_fallback
            .then(|| build_metadata_headers(&wire.payload.headers))
            .transpose()?;
        let internal_date_ms = wire
            .internal_date
            .parse::<i64>()
            .map_err(|_| GmailApiError::Permanent)?;
        Ok(GmailMessageSnapshot {
            id: wire.id,
            thread_id: wire.thread_id,
            label_ids: wire.label_ids,
            snippet: wire.snippet,
            history_id: wire.history_id,
            internal_date_ms,
            raw: wire.raw,
            metadata_headers,
        })
    }

    fn list_history(
        &self,
        access: &str,
        start_history_id: &str,
        page_token: Option<&str>,
    ) -> Result<GmailHistoryPage, GmailApiError> {
        let url = gmail_url(&["gmail", "v1", "users", "me", "history"])?;
        let mut request = self.client.get(url).bearer_auth(access).query(&[
            ("startHistoryId", start_history_id),
            // One immutable history record at a time keeps repeated bounded
            // slices stable even while newer mailbox history is appended.
            ("maxResults", "1"),
            (
                "fields",
                "history(id,messagesAdded(message(id,threadId)),messagesDeleted(message(id,threadId)),labelsAdded(message(id,threadId),labelIds),labelsRemoved(message(id,threadId),labelIds)),nextPageToken,historyId",
            ),
        ]);
        if let Some(page_token) = page_token {
            request = request.query(&[("pageToken", page_token)]);
        }
        let wire: GmailHistoryListWire = self
            .get(
                request,
                MAX_LIST_RESPONSE_BYTES,
                GmailApiError::HistoryExpired,
            )
            .map_err(call_error)?;
        let mut changes = Vec::new();
        for history in wire.history {
            for event in history
                .messages_added
                .into_iter()
                .chain(history.messages_deleted)
                .chain(history.labels_added)
                .chain(history.labels_removed)
            {
                changes.push(GmailHistoryChange {
                    message_id: event.message.id,
                    thread_id: event.message.thread_id,
                });
            }
        }
        Ok(GmailHistoryPage {
            changes,
            next_page_token: wire.next_page_token,
            history_id: wire.history_id,
        })
    }

    fn modify_thread(
        &self,
        access: &str,
        thread_id: &str,
        add_label_ids: &[String],
        remove_label_ids: &[String],
    ) -> Result<(), GmailApiError> {
        validate_mutation_labels(add_label_ids, remove_label_ids)?;
        let url = gmail_url(&["gmail", "v1", "users", "me", "threads", thread_id, "modify"])?;
        let request = self
            .client
            .post(url)
            .bearer_auth(access)
            .query(&[("fields", "id")])
            .json(&serde_json::json!({
                "addLabelIds": add_label_ids,
                "removeLabelIds": remove_label_ids,
            }));
        let acknowledgement: GmailThreadMutationWire = self
            .get(request, MAX_SMALL_RESPONSE_BYTES, GmailApiError::NotFound)
            .map_err(call_error)?;
        validate_mutation_acknowledgement(thread_id, acknowledgement)
    }

    fn trash_thread(&self, access: &str, thread_id: &str) -> Result<(), GmailApiError> {
        self.change_trash_state(access, thread_id, "trash")
    }

    fn untrash_thread(&self, access: &str, thread_id: &str) -> Result<(), GmailApiError> {
        self.change_trash_state(access, thread_id, "untrash")
    }

    fn thread_exists(&self, access: &str, thread_id: &str) -> Result<bool, GmailApiError> {
        validate_bounded(thread_id, MAX_REMOTE_ID_BYTES).map_err(|_| GmailApiError::Permanent)?;
        let url = gmail_url(&["gmail", "v1", "users", "me", "threads", thread_id])?;
        let request = self
            .client
            .get(url)
            .bearer_auth(access)
            .query(&[("format", "minimal"), ("fields", "id")]);
        match self.get::<GmailThreadMutationWire>(
            request,
            MAX_SMALL_RESPONSE_BYTES,
            GmailApiError::NotFound,
        ) {
            Ok(thread) => {
                validate_mutation_acknowledgement(thread_id, thread)?;
                Ok(true)
            }
            Err(GoogleCallError::Api(GmailApiError::NotFound)) => Ok(false),
            Err(error) => Err(call_error(error)),
        }
    }

    fn send_message(
        &self,
        access: &str,
        raw_base64url: &str,
        frozen_thread_id: Option<&str>,
        before_io: &mut dyn FnMut() -> Result<(), GmailApiError>,
    ) -> Result<GmailSendAcknowledgement, GmailApiError> {
        if raw_base64url.is_empty()
            || raw_base64url.len() > (crate::mime_ingest::MAX_RAW_MESSAGE_BYTES * 4 / 3 + 8)
            || !raw_base64url
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(GmailApiError::RejectedBeforeSubmission);
        }
        if let Some(thread_id) = frozen_thread_id {
            validate_bounded(thread_id, MAX_REMOTE_ID_BYTES)
                .map_err(|_| GmailApiError::RejectedBeforeSubmission)?;
        }
        let url = gmail_url(&["gmail", "v1", "users", "me", "messages", "send"])?;
        let mut body = serde_json::Map::new();
        body.insert(
            "raw".into(),
            serde_json::Value::String(raw_base64url.into()),
        );
        if let Some(thread_id) = frozen_thread_id {
            body.insert(
                "threadId".into(),
                serde_json::Value::String(thread_id.into()),
            );
        }
        let request = self
            .client
            .post(url)
            .bearer_auth(access)
            .query(&[("fields", "id,threadId,labelIds")])
            .json(&body)
            .build()
            .map_err(|_| GmailApiError::PreSubmissionRetryable)?;
        before_io()?;
        let response = self
            .client
            .execute(request)
            .map_err(|_| GmailApiError::AmbiguousSubmission)?;
        decode_send_response(response, frozen_thread_id)
    }

    fn find_send_evidence(
        &self,
        access: &str,
        submission_message_id: &str,
        client_correlation_id: &str,
    ) -> Result<GmailSendEvidencePage, GmailApiError> {
        let canonical = crate::internet_message::canonicalize_message_id(submission_message_id)
            .map_err(|_| GmailApiError::Permanent)?;
        validate_bounded(client_correlation_id, 256).map_err(|_| GmailApiError::Permanent)?;
        let query = format!("rfc822msgid:{}", &canonical[1..canonical.len() - 1]);
        let url = gmail_url(&["gmail", "v1", "users", "me", "messages"])?;
        let list: GmailMessageListWire = self
            .get(
                self.client.get(url).bearer_auth(access).query(&[
                    ("includeSpamTrash", "true"),
                    ("maxResults", &MAX_SEND_EVIDENCE_RESULTS.to_string()),
                    ("q", &query),
                    ("fields", "messages(id,threadId),nextPageToken"),
                ]),
                MAX_LIST_RESPONSE_BYTES,
                GmailApiError::Permanent,
            )
            .map_err(call_error)?;
        if list.messages.len() > MAX_SEND_EVIDENCE_RESULTS {
            return Err(GmailApiError::Permanent);
        }
        let mut exact = Vec::new();
        let mut mismatched_candidates = 0;
        for candidate in list.messages {
            validate_bounded(&candidate.id, MAX_REMOTE_ID_BYTES)
                .map_err(|_| GmailApiError::Permanent)?;
            validate_bounded(&candidate.thread_id, MAX_REMOTE_ID_BYTES)
                .map_err(|_| GmailApiError::Permanent)?;
            let url = gmail_url(&["gmail", "v1", "users", "me", "messages", &candidate.id])?;
            let mut request = self.client.get(url).bearer_auth(access).query(&[
                ("format", "metadata"),
                (
                    "fields",
                    "id,threadId,internalDate,payload(headers(name,value))",
                ),
            ]);
            for name in ["Message-ID", "X-Mux-Client-Correlation"] {
                request = request.query(&[("metadataHeaders", name)]);
            }
            let wire: GmailMessageWire =
                match self.get(request, MAX_SMALL_RESPONSE_BYTES, GmailApiError::NotFound) {
                    Ok(wire) => wire,
                    Err(GoogleCallError::Api(GmailApiError::NotFound)) => continue,
                    Err(error) => return Err(call_error(error)),
                };
            let accepted_at_ms = wire
                .internal_date
                .parse::<i64>()
                .ok()
                .filter(|value| (0..=crate::outgoing::MAX_RFC3339_UNIX_MILLIS).contains(value));
            let observed_message_id = unique_header(&wire.payload.headers, "Message-ID")
                .and_then(|value| crate::internet_message::canonicalize_message_id(&value).ok());
            let observed_correlation =
                unique_header(&wire.payload.headers, "X-Mux-Client-Correlation");
            if wire.id != candidate.id
                || wire.thread_id != candidate.thread_id
                || accepted_at_ms.is_none()
                || observed_message_id.as_deref() != Some(canonical.as_str())
                || observed_correlation.as_deref() != Some(client_correlation_id)
            {
                mismatched_candidates += 1;
                continue;
            }
            exact.push(GmailSendEvidence {
                message_id: wire.id,
                thread_id: wire.thread_id,
                accepted_at_ms: accepted_at_ms.expect("validated evidence timestamp"),
                observed_message_id: observed_message_id.expect("validated Message-ID"),
                observed_client_correlation: observed_correlation
                    .expect("validated client correlation"),
            });
        }
        Ok(GmailSendEvidencePage {
            exact,
            mismatched_candidates,
            has_more: list.next_page_token.is_some(),
        })
    }
}

fn unique_header(headers: &[GmailHeaderWire], name: &str) -> Option<String> {
    let mut matches = headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case(name))
        .map(|header| header.value.trim().to_owned());
    let first = matches.next()?;
    if first.is_empty() || matches.next().is_some() {
        None
    } else {
        Some(first)
    }
}

fn decode_send_response(
    response: reqwest::blocking::Response,
    frozen_thread_id: Option<&str>,
) -> Result<GmailSendAcknowledgement, GmailApiError> {
    let status = response.status();
    let retry_after_at = retry_after_at(response.headers());
    if response
        .content_length()
        .is_some_and(|length| length > MAX_SEND_RESPONSE_BYTES)
    {
        return if status.is_success() || status.is_server_error() || status.as_u16() == 408 {
            Err(GmailApiError::AmbiguousSubmission)
        } else {
            Err(GmailApiError::RejectedBeforeSubmission)
        };
    }
    let mut bytes = Vec::new();
    response
        .take(MAX_SEND_RESPONSE_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| GmailApiError::AmbiguousSubmission)?;
    if bytes.len() as u64 > MAX_SEND_RESPONSE_BYTES {
        return if status.is_success() || status.is_server_error() || status.as_u16() == 408 {
            Err(GmailApiError::AmbiguousSubmission)
        } else {
            Err(GmailApiError::RejectedBeforeSubmission)
        };
    }
    decode_send_response_bytes(status, retry_after_at, &bytes, frozen_thread_id)
}

fn decode_send_response_bytes(
    status: reqwest::StatusCode,
    retry_after_at: i64,
    bytes: &[u8],
    frozen_thread_id: Option<&str>,
) -> Result<GmailSendAcknowledgement, GmailApiError> {
    if bytes.len() as u64 > MAX_SEND_RESPONSE_BYTES {
        return if status.is_success() || status.is_server_error() || status.as_u16() == 408 {
            Err(GmailApiError::AmbiguousSubmission)
        } else {
            Err(GmailApiError::RejectedBeforeSubmission)
        };
    }
    if status.as_u16() == 401 {
        return Err(GmailApiError::Unauthorized);
    }
    if status.as_u16() == 429 {
        return Err(GmailApiError::RateLimited { retry_after_at });
    }
    if status.as_u16() == 403 {
        return match classify_forbidden(bytes, retry_after_at) {
            GmailApiError::Unauthorized => Err(GmailApiError::Unauthorized),
            GmailApiError::RateLimited { retry_after_at } => {
                Err(GmailApiError::RateLimited { retry_after_at })
            }
            _ => Err(GmailApiError::RejectedBeforeSubmission),
        };
    }
    if status.is_server_error() {
        return Err(GmailApiError::AmbiguousSubmission);
    }
    if status.as_u16() == 408 {
        return Err(GmailApiError::AmbiguousSubmission);
    }
    if !status.is_success() {
        return Err(GmailApiError::RejectedBeforeSubmission);
    }
    let wire: GmailSentMessageWire =
        serde_json::from_slice(bytes).map_err(|_| GmailApiError::AmbiguousSubmission)?;
    if validate_bounded(&wire.id, MAX_REMOTE_ID_BYTES).is_err()
        || validate_bounded(&wire.thread_id, MAX_REMOTE_ID_BYTES).is_err()
        || wire.label_ids.len() > MAX_MESSAGE_LABELS
        || wire
            .label_ids
            .iter()
            .any(|label| validate_bounded(label, MAX_REMOTE_ID_BYTES).is_err())
        || frozen_thread_id.is_some_and(|expected| wire.thread_id != expected)
    {
        return Err(GmailApiError::AmbiguousSubmission);
    }
    Ok(GmailSendAcknowledgement {
        message_id: wire.id,
        thread_id: wire.thread_id,
    })
}

impl GoogleGmailApi {
    fn change_trash_state(
        &self,
        access: &str,
        thread_id: &str,
        action: &str,
    ) -> Result<(), GmailApiError> {
        if !matches!(action, "trash" | "untrash") {
            return Err(GmailApiError::Permanent);
        }
        validate_bounded(thread_id, MAX_REMOTE_ID_BYTES).map_err(|_| GmailApiError::Permanent)?;
        let url = gmail_url(&["gmail", "v1", "users", "me", "threads", thread_id, action])?;
        let acknowledgement: GmailThreadMutationWire = self
            .get(
                self.client
                    .post(url)
                    .bearer_auth(access)
                    .query(&[("fields", "id")])
                    .json(&serde_json::json!({})),
                MAX_SMALL_RESPONSE_BYTES,
                GmailApiError::NotFound,
            )
            .map_err(call_error)?;
        validate_mutation_acknowledgement(thread_id, acknowledgement)
    }
}

#[derive(Deserialize)]
struct GmailThreadMutationWire {
    id: String,
}

fn validate_mutation_acknowledgement(
    expected_thread_id: &str,
    acknowledgement: GmailThreadMutationWire,
) -> Result<(), GmailApiError> {
    validate_bounded(expected_thread_id, MAX_REMOTE_ID_BYTES)
        .map_err(|_| GmailApiError::Permanent)?;
    if acknowledgement.id != expected_thread_id {
        return Err(GmailApiError::Permanent);
    }
    Ok(())
}

fn validate_mutation_labels(
    add_label_ids: &[String],
    remove_label_ids: &[String],
) -> Result<(), GmailApiError> {
    if add_label_ids.len() > 100 || remove_label_ids.len() > 100 {
        return Err(GmailApiError::Permanent);
    }
    let mut seen = BTreeSet::new();
    for label_id in add_label_ids.iter().chain(remove_label_ids) {
        validate_bounded(label_id, MAX_REMOTE_ID_BYTES).map_err(|_| GmailApiError::Permanent)?;
        if !seen.insert(label_id) {
            return Err(GmailApiError::Permanent);
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GmailProfileWire {
    email_address: String,
    history_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GmailLabelListWire {
    #[serde(default)]
    labels: Vec<GmailLabelWire>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GmailLabelWire {
    id: String,
    name: String,
    #[serde(rename = "type", default)]
    label_type: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GmailMessageListWire {
    #[serde(default)]
    messages: Vec<GmailMessageRefWire>,
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GmailMessageRefWire {
    id: String,
    thread_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GmailSentMessageWire {
    id: String,
    thread_id: String,
    #[serde(default)]
    label_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GmailMessageWire {
    id: String,
    thread_id: String,
    #[serde(default)]
    label_ids: Vec<String>,
    #[serde(default)]
    snippet: String,
    history_id: String,
    internal_date: String,
    raw: Option<String>,
    #[serde(default)]
    payload: GmailMessagePayloadWire,
}

#[derive(Default, Deserialize)]
struct GmailMessagePayloadWire {
    #[serde(default)]
    headers: Vec<GmailHeaderWire>,
}

#[derive(Deserialize)]
struct GmailHeaderWire {
    name: String,
    value: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GmailHistoryListWire {
    #[serde(default)]
    history: Vec<GmailHistoryWire>,
    next_page_token: Option<String>,
    history_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GmailHistoryWire {
    #[serde(default)]
    messages_added: Vec<GmailHistoryEventWire>,
    #[serde(default)]
    messages_deleted: Vec<GmailHistoryEventWire>,
    #[serde(default)]
    labels_added: Vec<GmailHistoryEventWire>,
    #[serde(default)]
    labels_removed: Vec<GmailHistoryEventWire>,
}

#[derive(Deserialize)]
struct GmailHistoryEventWire {
    message: GmailHistoryMessageWire,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GmailHistoryMessageWire {
    id: String,
    thread_id: Option<String>,
}

#[derive(Deserialize)]
struct GmailErrorEnvelope {
    error: Option<GmailErrorBody>,
}

#[derive(Deserialize)]
struct GmailErrorBody {
    #[serde(default)]
    errors: Vec<GmailErrorReason>,
}

#[derive(Deserialize)]
struct GmailErrorReason {
    reason: Option<String>,
}

fn gmail_url(segments: &[&str]) -> Result<reqwest::Url, GmailApiError> {
    let mut url = reqwest::Url::parse(GMAIL_ORIGIN).map_err(|_| GmailApiError::Permanent)?;
    url.path_segments_mut()
        .map_err(|_| GmailApiError::Permanent)?
        .extend(segments);
    Ok(url)
}

fn call_error(error: GoogleCallError) -> GmailApiError {
    match error {
        GoogleCallError::Api(error) => error,
        GoogleCallError::TooLarge => GmailApiError::Permanent,
    }
}

fn classify_forbidden(bytes: &[u8], retry_after_at: i64) -> GmailApiError {
    if bytes.len() > MAX_SMALL_RESPONSE_BYTES as usize {
        return GmailApiError::Permanent;
    }
    let Ok(envelope) = serde_json::from_slice::<GmailErrorEnvelope>(bytes) else {
        return GmailApiError::Permanent;
    };
    let reasons = envelope.error.map(|error| error.errors).unwrap_or_default();
    if reasons.iter().any(|value| {
        matches!(
            value.reason.as_deref(),
            Some(
                "rateLimitExceeded"
                    | "userRateLimitExceeded"
                    | "quotaExceeded"
                    | "dailyLimitExceeded"
            )
        )
    }) {
        return GmailApiError::RateLimited { retry_after_at };
    }
    if reasons.iter().any(|value| {
        matches!(
            value.reason.as_deref(),
            Some("authError" | "insufficientPermissions")
        )
    }) {
        return GmailApiError::Unauthorized;
    }
    GmailApiError::Permanent
}

fn retry_after_at(headers: &reqwest::header::HeaderMap) -> i64 {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX);
    let seconds = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(60)
        .clamp(1, 24 * 60 * 60);
    now_ms.saturating_add(seconds.saturating_mul(1_000))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct GmailSyncWork {
    version: u8,
    batch_id: String,
    expected_prior_cursor: Option<String>,
    phase: GmailSyncPhase,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
enum GmailSyncPhase {
    Bootstrap {
        generation_id: String,
        baseline_history_id: Option<String>,
        label_offset: usize,
        page_token: Option<String>,
    },
    History {
        committed_history_id: String,
        #[serde(default)]
        committed_label_digest: Option<String>,
        page_token: Option<String>,
        offset: usize,
        page_digest: Option<String>,
        reconciliation_generation: Option<String>,
    },
    SweepLabels {
        generation_id: String,
        final_history_id: String,
        label_offset: usize,
        #[serde(default)]
        label_digest: Option<String>,
    },
    Sweep {
        generation_id: String,
        final_history_id: String,
        #[serde(default)]
        label_digest: Option<String>,
        pass: u64,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GmailCursor {
    version: u8,
    provider: String,
    phase: GmailSyncPhase,
}

impl GmailCursor {
    fn resume_phase(self) -> Result<GmailSyncPhase, WorkerError> {
        if self.version != WORK_FORMAT_VERSION || self.provider != "gmail" {
            return Err(WorkerError::Conflict(
                "Durable Gmail history cursor has the wrong authority".into(),
            ));
        }
        match &self.phase {
            GmailSyncPhase::History {
                page_token,
                offset,
                page_digest,
                reconciliation_generation,
                ..
            } if page_token.is_none()
                && *offset == 0
                && page_digest.is_none()
                && reconciliation_generation.is_none() => {}
            _ => {
                return Err(WorkerError::Conflict(
                    "Durable Gmail cursor is not a completed history boundary".into(),
                ))
            }
        }
        let validation = GmailSyncWork {
            version: WORK_FORMAT_VERSION,
            batch_id: "cursor-validation".into(),
            expected_prior_cursor: None,
            phase: self.phase.clone(),
        };
        validation
            .validate()
            .map_err(|error| WorkerError::Conflict(error.into()))?;
        Ok(self.phase)
    }
}

impl GmailSyncWork {
    fn validate(&self) -> Result<(), &'static str> {
        if self.version != WORK_FORMAT_VERSION {
            return Err("unsupported Gmail work version");
        }
        validate_bounded(&self.batch_id, 256)?;
        if let Some(cursor) = &self.expected_prior_cursor {
            validate_bounded(cursor, 16 * 1024)?;
        }
        match &self.phase {
            GmailSyncPhase::Bootstrap {
                generation_id,
                baseline_history_id,
                label_offset,
                page_token,
            } => {
                validate_bounded(generation_id, MAX_GENERATION_BYTES)?;
                validate_optional(baseline_history_id.as_deref(), MAX_HISTORY_ID_BYTES)?;
                if *label_offset > MAX_LABELS {
                    return Err("Gmail label offset is out of bounds");
                }
                validate_optional(page_token.as_deref(), MAX_PAGE_TOKEN_BYTES)?;
            }
            GmailSyncPhase::History {
                committed_history_id,
                committed_label_digest,
                page_token,
                offset,
                page_digest,
                reconciliation_generation,
            } => {
                validate_bounded(committed_history_id, MAX_HISTORY_ID_BYTES)?;
                validate_digest(committed_label_digest.as_deref())?;
                validate_optional(page_token.as_deref(), MAX_PAGE_TOKEN_BYTES)?;
                if *offset > MAX_HISTORY_CHANGES {
                    return Err("Gmail history offset is out of bounds");
                }
                if let Some(digest) = page_digest {
                    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                        return Err("Gmail history digest is invalid");
                    }
                }
                validate_optional(reconciliation_generation.as_deref(), MAX_GENERATION_BYTES)?;
            }
            GmailSyncPhase::SweepLabels {
                generation_id,
                final_history_id,
                label_offset,
                label_digest,
            } => {
                validate_bounded(generation_id, MAX_GENERATION_BYTES)?;
                validate_bounded(final_history_id, MAX_HISTORY_ID_BYTES)?;
                validate_digest(label_digest.as_deref())?;
                if *label_offset > MAX_LABELS {
                    return Err("Gmail label offset is out of bounds");
                }
            }
            GmailSyncPhase::Sweep {
                generation_id,
                final_history_id,
                label_digest,
                ..
            } => {
                validate_bounded(generation_id, MAX_GENERATION_BYTES)?;
                validate_bounded(final_history_id, MAX_HISTORY_ID_BYTES)?;
                validate_digest(label_digest.as_deref())?;
            }
        }
        Ok(())
    }
}

pub(crate) fn initial_sync_work(
    account_id: &str,
    request_id: &str,
    now_ms: i64,
) -> Result<NewWorkItem, &'static str> {
    validate_bounded(account_id, 256)?;
    validate_bounded(request_id, 128)?;
    if now_ms < 0 {
        return Err("Gmail sync timestamp is invalid");
    }
    let generation_id = format!("gmail-scan-{request_id}");
    validate_bounded(&generation_id, MAX_GENERATION_BYTES)?;
    let request = make_work(
        GmailSyncPhase::Bootstrap {
            generation_id,
            baseline_history_id: None,
            label_offset: 0,
            page_token: None,
        },
        None,
    )?;
    work_item(account_id, request, now_ms)
}

pub(crate) struct GmailAdapter<A, P, C> {
    access: A,
    api: P,
    clock: C,
    database_path: Option<PathBuf>,
}

impl<A, P, C> GmailAdapter<A, P, C> {
    pub(crate) fn new(access: A, api: P, clock: C) -> Self {
        Self {
            access,
            api,
            clock,
            database_path: None,
        }
    }

    pub(crate) fn with_database_path(mut self, database_path: &Path) -> Self {
        self.database_path = Some(database_path.to_owned());
        self
    }
}

impl<A, P, C> WorkerAdapter for GmailAdapter<A, P, C>
where
    A: GmailAccessSource + Sync,
    P: GmailApi + Sync,
    C: Fn() -> i64 + Sync,
{
    fn execute(&self, work: &ClaimedWork, context: &WorkerExecutionContext) -> WorkerOutcome {
        match work.kind {
            WorkKind::Sync if is_gmail_send_reconciliation_work(work) => {
                self.execute_send_reconciliation(work, context)
            }
            WorkKind::Sync if is_gmail_sync_work(work) => self.execute_sync(work, context),
            WorkKind::Mutation if work.scope == GMAIL_ACCOUNT_SCOPE => {
                self.execute_mutation(work, context)
            }
            WorkKind::Send if work.scope.starts_with(SEND_RECONCILIATION_SCOPE_PREFIX) => {
                self.execute_send(work, context)
            }
            WorkKind::Sync | WorkKind::Mutation | WorkKind::Send => {
                WorkerOutcome::PermanentFailure {
                    code: "gmail_invalid_work_kind".into(),
                }
            }
        }
    }
}

impl<A, P, C> GmailAdapter<A, P, C>
where
    A: GmailAccessSource + Sync,
    P: GmailApi + Sync,
    C: Fn() -> i64 + Sync,
{
    fn execute_sync(&self, work: &ClaimedWork, context: &WorkerExecutionContext) -> WorkerOutcome {
        let request = match serde_json::from_str::<GmailSyncWork>(&work.payload_json) {
            Ok(request) if request.validate().is_ok() && request.batch_id == work.ordering_key => {
                request
            }
            _ => {
                return WorkerOutcome::PermanentFailure {
                    code: "gmail_invalid_work_payload".into(),
                }
            }
        };
        if context.is_cancelled() {
            return WorkerOutcome::Cancelled;
        }
        let now_ms = (self.clock)();
        let grant = match self.access.access_for_account(&work.account_id, now_ms) {
            Ok(grant) => grant,
            Err(error) => return access_error_outcome(error),
        };
        if context.is_cancelled() {
            return WorkerOutcome::Cancelled;
        }
        let profile = match self.api.get_profile(grant.access_value.as_str()) {
            Ok(profile) => profile,
            Err(error) => return api_error_outcome(error),
        };
        if !profile
            .email_address
            .eq_ignore_ascii_case(&grant.remote_account_id)
        {
            return WorkerOutcome::AuthenticationExpired {
                code: "gmail_account_mismatch".into(),
            };
        }
        match execute_page(
            &self.api,
            grant.access_value.as_str(),
            &work.account_id,
            request,
            profile,
            now_ms,
        ) {
            Ok(page) => WorkerOutcome::Succeeded {
                projection: WorkerProjection::ProviderSyncPage(Box::new(page)),
            },
            Err(error) => api_error_outcome(error),
        }
    }

    fn execute_mutation(
        &self,
        work: &ClaimedWork,
        context: &WorkerExecutionContext,
    ) -> WorkerOutcome {
        let mutation = match GmailThreadMutation::from_work(work) {
            Ok(mutation) => mutation,
            Err(()) => {
                return WorkerOutcome::PermanentFailure {
                    code: "gmail_invalid_mutation_payload".into(),
                }
            }
        };
        if context.is_cancelled() {
            return WorkerOutcome::Cancelled;
        }
        let now_ms = (self.clock)();
        let grant = match self.access.access_for_mutation(&work.account_id, now_ms) {
            Ok(grant) => grant,
            Err(error) => return access_error_outcome(error),
        };
        if context.is_cancelled() {
            return WorkerOutcome::Cancelled;
        }
        let profile = match self.api.get_profile(grant.access_value.as_str()) {
            Ok(profile) => profile,
            Err(error) => return api_error_outcome(error),
        };
        if !profile
            .email_address
            .eq_ignore_ascii_case(&grant.remote_account_id)
        {
            return WorkerOutcome::AuthenticationExpired {
                code: "gmail_account_mismatch".into(),
            };
        }
        if context.is_cancelled() {
            return WorkerOutcome::Cancelled;
        }
        let result = match &mutation.effect {
            GmailMutationEffect::Modify {
                add_label_ids,
                remove_label_ids,
            } => self.api.modify_thread(
                grant.access_value.as_str(),
                &mutation.remote_thread_id,
                add_label_ids,
                remove_label_ids,
            ),
            GmailMutationEffect::Trash => self
                .api
                .trash_thread(grant.access_value.as_str(), &mutation.remote_thread_id),
            GmailMutationEffect::Untrash => self
                .api
                .untrash_thread(grant.access_value.as_str(), &mutation.remote_thread_id),
        };
        match result {
            Ok(()) => WorkerOutcome::Succeeded {
                projection: WorkerProjection::LocalOperation,
            },
            Err(GmailApiError::NotFound) => match self
                .api
                .thread_exists(grant.access_value.as_str(), &mutation.remote_thread_id)
            {
                Ok(false) => WorkerOutcome::Succeeded {
                    projection: WorkerProjection::RemoteThreadAbsent {
                        remote_thread_id: mutation.remote_thread_id,
                    },
                },
                Ok(true) => WorkerOutcome::PermanentFailure {
                    code: "gmail_mutation_target_stale".into(),
                },
                Err(error) => api_error_outcome(error),
            },
            Err(error) => api_error_outcome(error),
        }
    }

    fn execute_send(&self, work: &ClaimedWork, context: &WorkerExecutionContext) -> WorkerOutcome {
        let prepared = match crate::outgoing::prepare_from_durable_payload(&work.payload_json, &[])
        {
            Ok(prepared)
                if prepared.provider_kind.as_deref() == Some("gmail")
                    && prepared.client_correlation_id.is_some()
                    && work.operation_id.is_some()
                    && work.ordering_key == prepared.message_id
                    && work.scope
                        == gmail_send_scope(work.operation_id.as_deref().expect("validated")) =>
            {
                prepared
            }
            _ => {
                return WorkerOutcome::PermanentFailure {
                    code: "gmail_invalid_send_snapshot".into(),
                }
            }
        };
        if context.is_cancelled() {
            return WorkerOutcome::RejectedBeforeSubmission {
                code: "gmail_send_cancelled_before_submission".into(),
            };
        }
        let Some(database_path) = self.database_path.as_deref() else {
            return WorkerOutcome::RejectedBeforeSubmission {
                code: "gmail_send_gate_unavailable".into(),
            };
        };
        let now_ms = (self.clock)();
        let grant = match self.access.access_for_mutation(&work.account_id, now_ms) {
            Ok(grant) => grant,
            Err(error) => return send_access_error_outcome(error),
        };
        if context.is_cancelled() {
            return WorkerOutcome::RejectedBeforeSubmission {
                code: "gmail_send_cancelled_before_submission".into(),
            };
        }
        let profile = match self.api.get_profile(grant.access_value.as_str()) {
            Ok(profile) => profile,
            Err(error) => return pre_submission_api_error_outcome(error),
        };
        if !profile
            .email_address
            .eq_ignore_ascii_case(&grant.remote_account_id)
        {
            return WorkerOutcome::AuthenticationExpired {
                code: "gmail_account_mismatch".into(),
            };
        }
        if context.is_cancelled() {
            return WorkerOutcome::RejectedBeforeSubmission {
                code: "gmail_send_cancelled_before_submission".into(),
            };
        }
        let raw_base64url = URL_SAFE_NO_PAD.encode(&prepared.raw_message);
        let mut before_io = || {
            crate::worker::mark_send_submission_started(database_path, work, (self.clock)())
                .map_err(|_| GmailApiError::PreSubmissionRetryable)
        };
        match self.api.send_message(
            grant.access_value.as_str(),
            &raw_base64url,
            prepared.remote_thread_id.as_deref(),
            &mut before_io,
        ) {
            Ok(acknowledgement)
                if validate_bounded(&acknowledgement.message_id, MAX_REMOTE_ID_BYTES).is_ok()
                    && validate_bounded(&acknowledgement.thread_id, MAX_REMOTE_ID_BYTES)
                        .is_ok()
                    && prepared
                        .remote_thread_id
                        .as_deref()
                        .is_none_or(|expected| expected == acknowledgement.thread_id) =>
            {
                WorkerOutcome::Succeeded {
                    projection: WorkerProjection::ProviderSendAcceptance(Box::new(
                        ProviderSendAcceptance {
                            provider_kind: "gmail".into(),
                            original_work_id: work.id.clone(),
                            operation_id: work.operation_id.clone().expect("validated operation"),
                            send_payload_fingerprint_hex: work.payload_fingerprint_hex.clone(),
                            remote_message_id: acknowledgement.message_id,
                            remote_thread_id: acknowledgement.thread_id,
                            accepted_at_ms: (self.clock)(),
                            reconciliation_work_id: None,
                        },
                    )),
                }
            }
            Ok(_) => WorkerOutcome::OutcomeUnknown {
                code: "gmail_send_response_identity_invalid".into(),
            },
            Err(error) => send_submission_error_outcome(error),
        }
    }

    fn execute_send_reconciliation(
        &self,
        work: &ClaimedWork,
        context: &WorkerExecutionContext,
    ) -> WorkerOutcome {
        let request = match serde_json::from_str::<GmailSendReconciliationWork>(&work.payload_json)
        {
            Ok(request)
                if request.version == SEND_RECONCILIATION_VERSION
                    && request.provider == "gmail"
                    && work.id == format!("reconcile_{}", request.original_work_id)
                    && work.scope == gmail_send_scope(&request.operation_id)
                    && work.ordering_key == request.submission_message_id
                    && validate_send_reconciliation_request(&request).is_ok() =>
            {
                request
            }
            _ => {
                return WorkerOutcome::PermanentFailure {
                    code: "gmail_invalid_send_reconciliation".into(),
                }
            }
        };
        let Some(database_path) = self.database_path.as_deref() else {
            return WorkerOutcome::RetryableFailure {
                code: "gmail_reconciliation_state_unavailable".into(),
            };
        };
        match send_reconciliation_is_eligible(database_path, work, &request) {
            Ok(true) => {}
            Ok(false) => return WorkerOutcome::Cancelled,
            Err(()) => {
                return WorkerOutcome::RetryableFailure {
                    code: "gmail_reconciliation_state_unavailable".into(),
                }
            }
        }
        if context.is_cancelled() {
            return WorkerOutcome::Cancelled;
        }
        let now_ms = (self.clock)();
        let grant = match self.access.access_for_account(&work.account_id, now_ms) {
            Ok(grant) => grant,
            Err(error) => return access_error_outcome(error),
        };
        let profile = match self.api.get_profile(grant.access_value.as_str()) {
            Ok(profile) => profile,
            Err(error) => return api_error_outcome(error),
        };
        if !profile
            .email_address
            .eq_ignore_ascii_case(&grant.remote_account_id)
        {
            return WorkerOutcome::AuthenticationExpired {
                code: "gmail_account_mismatch".into(),
            };
        }
        if context.is_cancelled() {
            return WorkerOutcome::Cancelled;
        }
        let evidence = match self.api.find_send_evidence(
            grant.access_value.as_str(),
            &request.submission_message_id,
            &request.client_correlation_id,
        ) {
            Ok(evidence) => evidence,
            Err(error) => return api_error_outcome(error),
        };
        if evidence.has_more {
            return WorkerOutcome::PermanentFailure {
                code: "gmail_send_evidence_truncated".into(),
            };
        }
        if evidence.mismatched_candidates > 0 {
            return WorkerOutcome::PermanentFailure {
                code: "gmail_send_evidence_mismatch".into(),
            };
        }
        let mut exact = evidence.exact;
        if exact.is_empty() {
            return WorkerOutcome::RetryableFailure {
                code: "gmail_send_evidence_pending".into(),
            };
        }
        if exact.len() != 1 {
            return WorkerOutcome::PermanentFailure {
                code: "gmail_send_evidence_duplicate".into(),
            };
        }
        let evidence = exact.pop().expect("one exact evidence row");
        let latest_plausible_evidence = now_ms
            .saturating_add(MAX_SEND_EVIDENCE_FUTURE_SKEW_MS)
            .min(crate::outgoing::MAX_RFC3339_UNIX_MILLIS);
        if !(request.queued_at_ms..=latest_plausible_evidence).contains(&evidence.accepted_at_ms)
            || validate_bounded(&evidence.message_id, MAX_REMOTE_ID_BYTES).is_err()
            || validate_bounded(&evidence.thread_id, MAX_REMOTE_ID_BYTES).is_err()
            || crate::internet_message::canonicalize_message_id(&evidence.observed_message_id).ok()
                != crate::internet_message::canonicalize_message_id(&request.submission_message_id)
                    .ok()
            || evidence.observed_client_correlation != request.client_correlation_id
        {
            return WorkerOutcome::PermanentFailure {
                code: "gmail_send_evidence_identity_mismatch".into(),
            };
        }
        if request
            .frozen_remote_thread_id
            .as_deref()
            .is_some_and(|expected| expected != evidence.thread_id)
        {
            return WorkerOutcome::PermanentFailure {
                code: "gmail_send_evidence_thread_mismatch".into(),
            };
        }
        WorkerOutcome::Succeeded {
            projection: WorkerProjection::ProviderSendAcceptance(Box::new(
                ProviderSendAcceptance {
                    provider_kind: "gmail".into(),
                    original_work_id: request.original_work_id,
                    operation_id: request.operation_id,
                    send_payload_fingerprint_hex: request.send_payload_fingerprint_hex,
                    remote_message_id: evidence.message_id,
                    remote_thread_id: evidence.thread_id,
                    accepted_at_ms: evidence.accepted_at_ms,
                    reconciliation_work_id: Some(work.id.clone()),
                },
            )),
        }
    }
}

struct GmailThreadMutation {
    remote_thread_id: String,
    effect: GmailMutationEffect,
}

enum GmailMutationEffect {
    Modify {
        add_label_ids: Vec<String>,
        remove_label_ids: Vec<String>,
    },
    Trash,
    Untrash,
}

impl GmailThreadMutation {
    fn from_work(work: &ClaimedWork) -> Result<Self, ()> {
        let payload =
            serde_json::from_str::<ThreadMutationPayload>(&work.payload_json).map_err(|_| ())?;
        if payload.format_version != Some(1)
            || payload.operation_id != work.ordering_key
            || work.operation_id.as_deref() != Some(payload.operation_id.as_str())
            || payload.account_id.as_deref() != Some(work.account_id.as_str())
            || payload.thread_id <= 0
            || payload.remote_thread_id.is_none()
        {
            return Err(());
        }
        validate_bounded(&payload.operation_id, 256).map_err(|_| ())?;
        validate_bounded(&work.account_id, 256).map_err(|_| ())?;
        if let Some(undo_of) = payload.undo_of.as_deref() {
            validate_bounded(undo_of, 256).map_err(|_| ())?;
        }
        let remote_thread_id = payload.remote_thread_id.ok_or(())?;
        validate_bounded(&remote_thread_id, MAX_REMOTE_ID_BYTES).map_err(|_| ())?;
        let label_effect = |label: String, enabled: bool| GmailMutationEffect::Modify {
            add_label_ids: enabled.then_some(label.clone()).into_iter().collect(),
            remove_label_ids: (!enabled).then_some(label).into_iter().collect(),
        };
        let enabled = match payload.value.as_str() {
            "0" => false,
            "1" => true,
            _ => return Err(()),
        };
        let effect = match payload.field.as_str() {
            "in_inbox" if payload.remote_container_id.is_none() => {
                label_effect("INBOX".into(), enabled)
            }
            "unread" if payload.remote_container_id.is_none() => {
                label_effect("UNREAD".into(), enabled)
            }
            "starred" if payload.remote_container_id.is_none() => {
                label_effect("STARRED".into(), enabled)
            }
            "provider_label" => {
                let label = payload.remote_container_id.ok_or(())?;
                validate_bounded(&label, MAX_REMOTE_ID_BYTES).map_err(|_| ())?;
                label_effect(label, enabled)
            }
            "trashed" if payload.remote_container_id.is_none() && enabled => {
                GmailMutationEffect::Trash
            }
            "trashed" if payload.remote_container_id.is_none() => GmailMutationEffect::Untrash,
            _ => return Err(()),
        };
        Ok(Self {
            remote_thread_id,
            effect,
        })
    }
}

fn execute_page<P: GmailApi>(
    api: &P,
    access: &str,
    account_id: &str,
    request: GmailSyncWork,
    profile: GmailProfile,
    now_ms: i64,
) -> Result<ProviderSyncPage, GmailApiError> {
    match request.phase.clone() {
        GmailSyncPhase::Bootstrap {
            generation_id,
            baseline_history_id,
            label_offset,
            page_token,
        } => execute_bootstrap(
            api,
            access,
            account_id,
            request,
            profile,
            generation_id,
            baseline_history_id,
            label_offset,
            page_token,
            now_ms,
        ),
        GmailSyncPhase::History {
            committed_history_id,
            committed_label_digest,
            page_token,
            offset,
            page_digest,
            reconciliation_generation,
        } => execute_history(
            api,
            access,
            account_id,
            request,
            committed_history_id,
            committed_label_digest,
            page_token,
            offset,
            page_digest,
            reconciliation_generation,
            now_ms,
        ),
        GmailSyncPhase::SweepLabels {
            generation_id,
            final_history_id,
            label_offset,
            label_digest,
        } => execute_sweep_labels(
            api,
            access,
            account_id,
            request,
            generation_id,
            final_history_id,
            label_offset,
            label_digest,
            now_ms,
        ),
        GmailSyncPhase::Sweep {
            generation_id,
            final_history_id,
            label_digest,
            pass,
        } => execute_sweep(
            account_id,
            request,
            generation_id,
            final_history_id,
            label_digest,
            pass,
            now_ms,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_bootstrap<P: GmailApi>(
    api: &P,
    access: &str,
    account_id: &str,
    request: GmailSyncWork,
    profile: GmailProfile,
    generation_id: String,
    baseline_history_id: Option<String>,
    label_offset: usize,
    page_token: Option<String>,
    now_ms: i64,
) -> Result<ProviderSyncPage, GmailApiError> {
    let first_page = baseline_history_id.is_none();
    let baseline_history_id = match baseline_history_id {
        Some(value) => value,
        None => validate_history_id(profile.history_id)?,
    };
    let labels = validate_labels(api.list_labels(access)?)?;
    let label_offset = if label_offset > labels.len() {
        0
    } else {
        label_offset
    };
    if label_offset < labels.len() {
        let end = label_offset
            .saturating_add(LABEL_PAGE_SIZE)
            .min(labels.len());
        let next_phase = GmailSyncPhase::Bootstrap {
            generation_id: generation_id.clone(),
            baseline_history_id: Some(baseline_history_id),
            label_offset: end,
            page_token,
        };
        let (next_request, cursor) = next_request(next_phase, now_ms, &request)?;
        return build_sync_page(
            account_id,
            request,
            cursor,
            now_ms,
            labels[label_offset..end].to_vec(),
            Vec::new(),
            Vec::new(),
            Some(next_request),
            false,
            if first_page {
                Some(ProviderCapabilities::new([
                    ProviderCapability::DeltaSync,
                    ProviderCapability::RemoteThreads,
                    ProviderCapability::MultiContainerMembership,
                    ProviderCapability::Mutations,
                ]))
            } else {
                None
            },
            Some(ProviderReconciliationPage {
                generation_id,
                begin: first_page,
                reset_seen_containers: false,
                complete_kinds: BTreeSet::new(),
                sweep_kinds: BTreeSet::new(),
                seen_remote_messages: Vec::new(),
            }),
        );
    }
    let page = api.list_messages(access, page_token.as_deref(), MESSAGE_PAGE_SIZE)?;
    validate_message_page(&page)?;
    let snapshots = fetch_snapshots(api, access, &page.messages)?;
    let referenced_labels = labels_for_snapshots(&labels, &snapshots)?;
    let next_phase = match page.next_page_token {
        Some(next_page_token) => GmailSyncPhase::Bootstrap {
            generation_id: generation_id.clone(),
            baseline_history_id: Some(baseline_history_id.clone()),
            label_offset: labels.len(),
            page_token: Some(next_page_token),
        },
        None => GmailSyncPhase::History {
            committed_history_id: baseline_history_id,
            committed_label_digest: None,
            page_token: None,
            offset: 0,
            page_digest: None,
            reconciliation_generation: Some(generation_id.clone()),
        },
    };
    let (next_request, cursor) = next_request(next_phase, now_ms, &request)?;
    build_sync_page(
        account_id,
        request,
        cursor,
        now_ms,
        referenced_labels,
        snapshots,
        Vec::new(),
        Some(next_request),
        false,
        if first_page {
            Some(ProviderCapabilities::new([
                ProviderCapability::DeltaSync,
                ProviderCapability::RemoteThreads,
                ProviderCapability::MultiContainerMembership,
                ProviderCapability::Mutations,
            ]))
        } else {
            None
        },
        Some(ProviderReconciliationPage {
            generation_id,
            begin: first_page,
            reset_seen_containers: false,
            complete_kinds: BTreeSet::new(),
            sweep_kinds: BTreeSet::new(),
            seen_remote_messages: Vec::new(),
        }),
    )
}

#[allow(clippy::too_many_arguments)]
fn execute_history<P: GmailApi>(
    api: &P,
    access: &str,
    account_id: &str,
    request: GmailSyncWork,
    committed_history_id: String,
    committed_label_digest: Option<String>,
    page_token: Option<String>,
    offset: usize,
    page_digest: Option<String>,
    reconciliation_generation: Option<String>,
    now_ms: i64,
) -> Result<ProviderSyncPage, GmailApiError> {
    let page = match api.list_history(access, &committed_history_id, page_token.as_deref()) {
        Ok(page) => page,
        Err(GmailApiError::HistoryExpired) => {
            return invalid_history_recovery(account_id, request, now_ms)
        }
        Err(error) => return Err(error),
    };
    validate_history_page(&page)?;
    let labels = validate_labels(api.list_labels(access)?)?;
    let current_label_digest = label_digest(&labels);
    let changes = dedupe_changes(page.changes.clone());
    let digest = history_digest(&changes, &page);
    if page_digest
        .as_deref()
        .is_some_and(|expected| expected != digest)
    {
        let restart_phase = GmailSyncPhase::History {
            committed_history_id,
            committed_label_digest,
            page_token,
            offset: 0,
            page_digest: None,
            reconciliation_generation,
        };
        let (next, cursor) = next_request(restart_phase, now_ms, &request)?;
        return build_sync_page(
            account_id,
            request,
            cursor,
            now_ms,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Some(next),
            false,
            None,
            None,
        );
    }
    if offset > changes.len() {
        return Err(GmailApiError::Permanent);
    }
    let end = offset.saturating_add(MESSAGE_PAGE_SIZE).min(changes.len());
    let mut snapshots = Vec::new();
    let mut tombstones = Vec::new();
    for change in &changes[offset..end] {
        match api.get_message(access, &change.message_id) {
            Ok(snapshot) => snapshots.push(snapshot),
            Err(GmailApiError::NotFound) => tombstones.push(change.clone()),
            Err(error) => return Err(error),
        }
    }
    let referenced_labels = labels_for_snapshots(&labels, &snapshots)?;

    let more_in_response = end < changes.len();
    let next_phase = if more_in_response {
        Some(GmailSyncPhase::History {
            committed_history_id: committed_history_id.clone(),
            committed_label_digest: committed_label_digest.clone(),
            page_token: page_token.clone(),
            offset: end,
            page_digest: Some(digest),
            reconciliation_generation: reconciliation_generation.clone(),
        })
    } else if let Some(next_page_token) = page.next_page_token.clone() {
        Some(GmailSyncPhase::History {
            committed_history_id: committed_history_id.clone(),
            committed_label_digest: committed_label_digest.clone(),
            page_token: Some(next_page_token),
            offset: 0,
            page_digest: None,
            reconciliation_generation: reconciliation_generation.clone(),
        })
    } else if let Some(generation_id) = reconciliation_generation.clone() {
        Some(GmailSyncPhase::SweepLabels {
            generation_id,
            final_history_id: page.history_id.clone(),
            label_offset: 0,
            label_digest: Some(current_label_digest.clone()),
        })
    } else if committed_label_digest.as_deref() != Some(current_label_digest.as_str()) {
        Some(GmailSyncPhase::Bootstrap {
            generation_id: label_reconciliation_generation(&request.batch_id),
            baseline_history_id: None,
            label_offset: 0,
            page_token: None,
        })
    } else {
        None
    };
    let (continuation, cursor, complete) = match next_phase {
        Some(next_phase) => {
            let (next, cursor) = next_request(next_phase, now_ms, &request)?;
            (Some(next), cursor, false)
        }
        None => (
            None,
            cursor_json(&GmailSyncPhase::History {
                committed_history_id: page.history_id.clone(),
                committed_label_digest: Some(current_label_digest),
                page_token: None,
                offset: 0,
                page_digest: None,
                reconciliation_generation: None,
            })?,
            true,
        ),
    };
    build_sync_page(
        account_id,
        request,
        cursor,
        now_ms,
        referenced_labels,
        snapshots,
        tombstones,
        continuation,
        complete,
        None,
        reconciliation_generation.map(|generation_id| ProviderReconciliationPage {
            generation_id,
            begin: false,
            reset_seen_containers: false,
            complete_kinds: BTreeSet::new(),
            sweep_kinds: BTreeSet::new(),
            seen_remote_messages: Vec::new(),
        }),
    )
}

#[allow(clippy::too_many_arguments)]
fn execute_sweep_labels<P: GmailApi>(
    api: &P,
    access: &str,
    account_id: &str,
    request: GmailSyncWork,
    generation_id: String,
    final_history_id: String,
    label_offset: usize,
    expected_label_digest: Option<String>,
    now_ms: i64,
) -> Result<ProviderSyncPage, GmailApiError> {
    let labels = validate_labels(api.list_labels(access)?)?;
    let current_label_digest = label_digest(&labels);
    if label_offset > 0 && expected_label_digest.as_deref() != Some(current_label_digest.as_str()) {
        let restart = GmailSyncPhase::SweepLabels {
            generation_id,
            final_history_id,
            label_offset: 0,
            label_digest: Some(current_label_digest),
        };
        let (next, cursor) = next_request(restart, now_ms, &request)?;
        return build_sync_page(
            account_id,
            request,
            cursor,
            now_ms,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Some(next),
            false,
            None,
            None,
        );
    }
    if label_offset > labels.len() {
        return Err(GmailApiError::Permanent);
    }
    let end = label_offset
        .saturating_add(LABEL_PAGE_SIZE)
        .min(labels.len());
    let next_phase = if end < labels.len() {
        GmailSyncPhase::SweepLabels {
            generation_id: generation_id.clone(),
            final_history_id: final_history_id.clone(),
            label_offset: end,
            label_digest: Some(current_label_digest.clone()),
        }
    } else {
        GmailSyncPhase::Sweep {
            generation_id: generation_id.clone(),
            final_history_id,
            label_digest: Some(current_label_digest),
            pass: 0,
        }
    };
    let (next, cursor) = next_request(next_phase, now_ms, &request)?;
    build_sync_page(
        account_id,
        request,
        cursor,
        now_ms,
        labels[label_offset..end].to_vec(),
        Vec::new(),
        Vec::new(),
        Some(next),
        false,
        None,
        Some(ProviderReconciliationPage {
            generation_id,
            begin: false,
            reset_seen_containers: label_offset == 0,
            complete_kinds: BTreeSet::new(),
            sweep_kinds: BTreeSet::new(),
            seen_remote_messages: Vec::new(),
        }),
    )
}

fn execute_sweep(
    account_id: &str,
    request: GmailSyncWork,
    generation_id: String,
    final_history_id: String,
    label_digest: Option<String>,
    pass: u64,
    now_ms: i64,
) -> Result<ProviderSyncPage, GmailApiError> {
    let next_phase = GmailSyncPhase::Sweep {
        generation_id: generation_id.clone(),
        final_history_id: final_history_id.clone(),
        label_digest: label_digest.clone(),
        pass: pass.checked_add(1).ok_or(GmailApiError::Permanent)?,
    };
    let cursor = cursor_json(&GmailSyncPhase::History {
        committed_history_id: final_history_id,
        committed_label_digest: label_digest,
        page_token: None,
        offset: 0,
        page_digest: None,
        reconciliation_generation: None,
    })?;
    let next = make_work(next_phase, Some(cursor.clone())).map_err(|_| GmailApiError::Permanent)?;
    build_sync_page(
        account_id,
        request,
        cursor,
        now_ms,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Some(next),
        false,
        None,
        Some(ProviderReconciliationPage {
            generation_id,
            begin: false,
            reset_seen_containers: false,
            complete_kinds: [
                ReconciliationObjectKind::Containers,
                ReconciliationObjectKind::Threads,
                ReconciliationObjectKind::Messages,
            ]
            .into_iter()
            .collect(),
            sweep_kinds: [
                ReconciliationObjectKind::Containers,
                ReconciliationObjectKind::Threads,
                ReconciliationObjectKind::Messages,
            ]
            .into_iter()
            .collect(),
            seen_remote_messages: Vec::new(),
        }),
    )
}

fn invalid_history_recovery(
    account_id: &str,
    request: GmailSyncWork,
    now_ms: i64,
) -> Result<ProviderSyncPage, GmailApiError> {
    let generation_id = format!("gmail-rescan-{}", request.batch_id);
    let phase = GmailSyncPhase::Bootstrap {
        generation_id,
        baseline_history_id: None,
        label_offset: 0,
        page_token: None,
    };
    let (next, cursor) = next_request(phase, now_ms, &request)?;
    build_sync_page(
        account_id,
        request,
        cursor,
        now_ms,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Some(next),
        false,
        None,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn build_sync_page(
    account_id: &str,
    request: GmailSyncWork,
    cursor_value: String,
    now_ms: i64,
    labels: Vec<GmailLabel>,
    snapshots: Vec<GmailMessageSnapshot>,
    deleted: Vec<GmailHistoryChange>,
    continuation: Option<GmailSyncWork>,
    complete: bool,
    capabilities: Option<ProviderCapabilities>,
    reconciliation: Option<ProviderReconciliationPage>,
) -> Result<ProviderSyncPage, GmailApiError> {
    let mux_account_id = MuxAccountId::new(account_id.to_owned()).map_err(contract_error)?;
    let container_upserts = labels
        .iter()
        .map(|label| project_label(&mux_account_id, label))
        .collect::<Result<Vec<_>, _>>()?;
    let mut message_upserts = Vec::with_capacity(snapshots.len());
    let mut restricted_message_content = Vec::with_capacity(snapshots.len());
    let mut thread_by_id = BTreeMap::<String, ProviderThreadUpsert>::new();
    let mut membership_changes = Vec::new();
    for snapshot in snapshots {
        let projected = project_message(&mux_account_id, snapshot)?;
        for label_id in &projected.label_ids {
            membership_changes.push(ContainerMembershipChange::Upsert {
                membership: ContainerMembership {
                    message: projected.message.identity.clone(),
                    container: RemoteContainerIdentity {
                        mux_account_id: mux_account_id.clone(),
                        remote_container_id: RemoteContainerId::new(label_id.clone())
                            .map_err(contract_error)?,
                    },
                },
            });
        }
        let replace = thread_by_id
            .get(projected.thread.identity.remote_thread_id.as_str())
            .is_none_or(|prior| prior.latest_at < projected.thread.latest_at);
        if replace {
            thread_by_id.insert(
                projected
                    .thread
                    .identity
                    .remote_thread_id
                    .as_str()
                    .to_owned(),
                projected.thread,
            );
        }
        if let Some(content) = projected.restricted_content {
            restricted_message_content.push(content);
        }
        message_upserts.push(projected.message);
    }
    let tombstones = deleted
        .into_iter()
        .map(|change| {
            Ok(ProviderTombstone {
                target: TombstoneTarget::Message {
                    identity: RemoteMessageIdentity {
                        mux_account_id: mux_account_id.clone(),
                        remote_message_id: RemoteMessageId::new(change.message_id)
                            .map_err(contract_error)?,
                        remote_thread_id: change
                            .thread_id
                            .map(RemoteThreadId::new)
                            .transpose()
                            .map_err(contract_error)?,
                    },
                },
                observed_at: UnixMillis::new(now_ms).map_err(contract_error)?,
                cursor: Some(OpaqueSyncCursor::new(cursor_value.clone()).map_err(contract_error)?),
            })
        })
        .collect::<Result<Vec<_>, GmailApiError>>()?;
    let batch = ProviderBatch {
        mux_account_id: mux_account_id.clone(),
        batch_id: ProviderBatchId::new(request.batch_id).map_err(contract_error)?,
        expected_prior_cursor: request
            .expected_prior_cursor
            .map(OpaqueSyncCursor::new)
            .transpose()
            .map_err(contract_error)?,
        cursor: ProviderSyncCursor {
            mux_account_id,
            scope: SyncCursorScope::Account,
            value: OpaqueSyncCursor::new(cursor_value).map_err(contract_error)?,
        },
        observed_at: UnixMillis::new(now_ms).map_err(contract_error)?,
        thread_upserts: thread_by_id.into_values().collect(),
        message_upserts,
        container_upserts,
        membership_changes,
        tombstones,
    };
    batch.validate().map_err(contract_error)?;
    let continuation = continuation
        .map(|next| continuation_for(next, now_ms))
        .transpose()?;
    Ok(ProviderSyncPage {
        batch: Box::new(batch),
        restricted_message_content,
        continuation,
        complete,
        capabilities,
        replace_memberships_for_upserted_messages: true,
        derive_thread_state_from_messages: true,
        reconciliation,
    })
}

struct ProjectedMessage {
    message: ProviderMessageUpsert,
    thread: ProviderThreadUpsert,
    label_ids: Vec<String>,
    restricted_content: Option<RestrictedMessageContent>,
}

fn project_message(
    account_id: &MuxAccountId,
    snapshot: GmailMessageSnapshot,
) -> Result<ProjectedMessage, GmailApiError> {
    validate_snapshot(&snapshot)?;
    let raw_content = snapshot.raw.as_deref().and_then(parse_gmail_raw);
    let parsed_raw = raw_content.is_some();
    let content = raw_content.or_else(|| {
        snapshot
            .metadata_headers
            .as_deref()
            .and_then(|headers| parse_mime(headers).ok())
    });
    let ProjectedSafeContent {
        subject,
        sender_name,
        sender_email,
        recipients,
        cc_recipients,
        body_text,
        body_html,
        blocked_remote_resources,
        remote_images,
        headers,
        flags,
    } = match content {
        Some(content) => project_safe_content(content),
        None => ProjectedSafeContent {
            subject: String::new(),
            sender_name: String::new(),
            sender_email: "unknown@example.invalid".into(),
            recipients: String::new(),
            cc_recipients: String::new(),
            body_text: String::new(),
            body_html: String::new(),
            blocked_remote_resources: 0,
            remote_images: Vec::new(),
            headers: (None, None, None),
            flags: (false, false, false),
        },
    };
    let subject = truncate_utf8(&normalize_single_line(&subject), MAX_SUBJECT_BYTES);
    let sender_name = truncate_utf8(&normalize_single_line(&sender_name), 2_000);
    let sender_email = truncate_utf8(&normalize_single_line(&sender_email), 2_048);
    let recipients = truncate_utf8(&normalize_single_line(&recipients), MAX_PARTICIPANTS_BYTES);
    let cc_recipients = truncate_utf8(
        &normalize_single_line(&cc_recipients),
        MAX_PARTICIPANTS_BYTES,
    );
    let normalized_body_text = body_text.replace("\r\n", "\n").replace('\r', "\n");
    let body_was_truncated = normalized_body_text.len() > MAX_BODY_TEXT_BYTES;
    let body_text = truncate_utf8(&normalized_body_text, MAX_BODY_TEXT_BYTES);
    let remote_message_id = RemoteMessageId::new(snapshot.id.clone()).map_err(contract_error)?;
    let remote_thread_id =
        RemoteThreadId::new(snapshot.thread_id.clone()).map_err(contract_error)?;
    let identity = RemoteMessageIdentity {
        mux_account_id: account_id.clone(),
        remote_message_id,
        remote_thread_id: Some(remote_thread_id.clone()),
    };
    let restricted_content = parsed_raw.then(|| RestrictedMessageContent {
        identity: identity.clone(),
        body_html,
        blocked_remote_resources,
        remote_images,
    });
    let mut keywords = BTreeSet::new();
    for (label, keyword) in [
        ("UNREAD", "unread"),
        ("STARRED", "starred"),
        ("IMPORTANT", "important"),
    ] {
        if snapshot.label_ids.iter().any(|value| value == label) {
            keywords.insert(ProviderMessageKeyword::new(keyword).map_err(contract_error)?);
        }
    }
    for (present, keyword) in [
        (flags.0, "has_attachment"),
        (flags.1, "has_invite"),
        (flags.2, "has_link"),
    ] {
        if present {
            keywords.insert(ProviderMessageKeyword::new(keyword).map_err(contract_error)?);
        }
    }
    let body_state = if !parsed_raw {
        crate::provider::ProviderBodyState::Unavailable
    } else if body_was_truncated {
        crate::provider::ProviderBodyState::Truncated
    } else {
        crate::provider::ProviderBodyState::Complete
    };
    let is_from_me = snapshot
        .label_ids
        .iter()
        .any(|value| matches!(value.as_str(), "SENT" | "DRAFT"));
    let message = ProviderMessageUpsert {
        identity,
        subject: ProviderSubject::new(subject.clone()).map_err(contract_error)?,
        sender_name: ProviderSenderName::new(sender_name.clone()).map_err(contract_error)?,
        sender_email: ProviderEmail::new(sender_email.clone()).map_err(contract_error)?,
        recipients: ProviderRecipients::new(recipients).map_err(contract_error)?,
        cc_recipients: ProviderRecipients::new(cc_recipients).map_err(contract_error)?,
        bcc_recipients: ProviderRecipients::new(String::new()).map_err(contract_error)?,
        sent_at: UnixMillis::new(snapshot.internal_date_ms).map_err(contract_error)?,
        body_text: NormalizedPlainBody::new(body_text.clone()).map_err(contract_error)?,
        body_state,
        is_from_me,
        revision: Some(ProviderRevision::new(snapshot.history_id).map_err(contract_error)?),
        keywords,
        internet_message_id: headers.0,
        in_reply_to: headers.1,
        references: headers.2,
    };
    let participants = truncate_utf8(
        &if sender_name.is_empty() {
            sender_email
        } else {
            format!("{sender_name} <{sender_email}>")
        },
        MAX_PARTICIPANTS_BYTES,
    );
    let thread = ProviderThreadUpsert {
        identity: RemoteThreadIdentity {
            mux_account_id: account_id.clone(),
            remote_thread_id,
        },
        subject: ProviderSubject::new(subject).map_err(contract_error)?,
        participants: ProviderParticipants::new(participants).map_err(contract_error)?,
        snippet: ProviderSnippet::new(truncate_utf8(
            if snapshot.snippet.is_empty() {
                &body_text
            } else {
                &snapshot.snippet
            },
            MAX_SNIPPET_BYTES,
        ))
        .map_err(contract_error)?,
        latest_at: UnixMillis::new(snapshot.internal_date_ms).map_err(contract_error)?,
        message_count: ProviderThreadMessageCount::new(1).map_err(contract_error)?,
        in_inbox: snapshot.label_ids.iter().any(|value| value == "INBOX"),
        unread: snapshot.label_ids.iter().any(|value| value == "UNREAD"),
        starred: snapshot.label_ids.iter().any(|value| value == "STARRED"),
        has_attachments: flags.0,
        has_invite: flags.1,
        has_links: flags.2,
        has_from_me: is_from_me,
        category: ProviderCategory::new(String::new()).map_err(contract_error)?,
        revision: Some(ProviderRevision::new("gmail-derived-v1").map_err(contract_error)?),
    };
    Ok(ProjectedMessage {
        message,
        thread,
        label_ids: snapshot.label_ids,
        restricted_content,
    })
}

type ThreadingProjection = (Option<String>, Option<String>, Option<Vec<String>>);
type ContentFlags = (bool, bool, bool);

struct ProjectedSafeContent {
    subject: String,
    sender_name: String,
    sender_email: String,
    recipients: String,
    cc_recipients: String,
    body_text: String,
    body_html: String,
    blocked_remote_resources: i64,
    remote_images: Vec<crate::content::RemoteImageCandidate>,
    headers: ThreadingProjection,
    flags: ContentFlags,
}

fn project_safe_content(content: SafeMessageContent) -> ProjectedSafeContent {
    let sender = content.from.first();
    let sender_name = sender.map(|value| value.name.clone()).unwrap_or_default();
    let sender_email = sender
        .map(|value| value.address.clone())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown@example.invalid".into());
    let recipients = format_mailboxes(&content.to);
    let cc_recipients = format_mailboxes(&content.cc);
    let lower = content.body_text.to_ascii_lowercase();
    let flags = (
        !content.attachments.is_empty(),
        content
            .attachments
            .iter()
            .any(|attachment| attachment.media_type.eq_ignore_ascii_case("text/calendar")),
        lower.contains("http://") || lower.contains("https://"),
    );
    ProjectedSafeContent {
        subject: content.subject,
        sender_name,
        sender_email,
        recipients,
        cc_recipients,
        body_text: content.body_text,
        body_html: content.body_html,
        blocked_remote_resources: content.blocked_remote_resources,
        remote_images: content.remote_images,
        headers: (
            content.internet_message_id,
            content.in_reply_to,
            (!content.references.is_empty()).then_some(content.references),
        ),
        flags,
    }
}

fn format_mailboxes(values: &[ParsedMailbox]) -> String {
    values
        .iter()
        .map(|value| {
            if value.name.is_empty() {
                value.address.clone()
            } else {
                format!("{} <{}>", value.name, value.address)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn parse_gmail_raw(value: &str) -> Option<SafeMessageContent> {
    if value.len() > MAX_RAW_JSON_VALUE_BYTES {
        return None;
    }
    let raw = URL_SAFE_NO_PAD
        .decode(value)
        .or_else(|_| URL_SAFE.decode(value))
        .ok()?;
    parse_mime(&raw).ok()
}

fn project_label(
    account_id: &MuxAccountId,
    label: &GmailLabel,
) -> Result<ProviderContainer, GmailApiError> {
    Ok(ProviderContainer {
        identity: RemoteContainerIdentity {
            mux_account_id: account_id.clone(),
            remote_container_id: RemoteContainerId::new(label.id.clone())
                .map_err(contract_error)?,
        },
        display_name: ContainerDisplayName::new(truncate_utf8(&label.name, 512))
            .map_err(contract_error)?,
        kind: ContainerKind::Label,
        role: gmail_label_role(&label.id),
        parent_remote_container_id: None,
        // Unknown Gmail system labels must never become user-selectable custom
        // labels merely because Mux does not assign them a special role.
        selectable: !label.system,
    })
}

fn labels_for_snapshots(
    labels: &[GmailLabel],
    snapshots: &[GmailMessageSnapshot],
) -> Result<Vec<GmailLabel>, GmailApiError> {
    let by_id = labels
        .iter()
        .map(|label| (label.id.as_str(), label))
        .collect::<BTreeMap<_, _>>();
    let referenced = snapshots
        .iter()
        .flat_map(|snapshot| snapshot.label_ids.iter().map(String::as_str))
        .collect::<BTreeSet<_>>();
    if referenced.len() > LABEL_PAGE_SIZE {
        return Err(GmailApiError::Permanent);
    }
    referenced
        .into_iter()
        .map(|id| {
            Ok(by_id.get(id).map_or_else(
                || GmailLabel {
                    id: id.to_owned(),
                    // A label can be attached between labels.list and
                    // messages.get. Its bounded ID is a safe temporary name;
                    // the next label page replaces it with the provider name.
                    name: id.to_owned(),
                    system: gmail_label_role(id).is_some(),
                },
                |label| (*label).clone(),
            ))
        })
        .collect()
}

fn gmail_label_role(label_id: &str) -> Option<ContainerRole> {
    match label_id {
        "INBOX" => Some(ContainerRole::Inbox),
        "SENT" => Some(ContainerRole::Sent),
        "DRAFT" => Some(ContainerRole::Drafts),
        "TRASH" => Some(ContainerRole::Trash),
        "SPAM" => Some(ContainerRole::Spam),
        "STARRED" => Some(ContainerRole::Starred),
        "IMPORTANT" => Some(ContainerRole::Important),
        _ => None,
    }
}

fn fetch_snapshots<P: GmailApi>(
    api: &P,
    access: &str,
    messages: &[GmailMessageRef],
) -> Result<Vec<GmailMessageSnapshot>, GmailApiError> {
    let mut snapshots = Vec::with_capacity(messages.len());
    for message in messages {
        match api.get_message(access, &message.id) {
            Ok(snapshot) => snapshots.push(snapshot),
            // Deletion between list and fetch is a normal bootstrap race. A
            // rescan leaves it unseen and the bounded final sweep removes any
            // older local projection of the same remote message.
            Err(GmailApiError::NotFound) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(snapshots)
}

fn validate_message_page(page: &GmailMessagePage) -> Result<(), GmailApiError> {
    if page.messages.len() > MESSAGE_PAGE_SIZE {
        return Err(GmailApiError::Permanent);
    }
    validate_optional(page.next_page_token.as_deref(), MAX_PAGE_TOKEN_BYTES)
        .map_err(|_| GmailApiError::Permanent)?;
    let mut ids = BTreeSet::new();
    for message in &page.messages {
        validate_bounded(&message.id, MAX_REMOTE_ID_BYTES).map_err(|_| GmailApiError::Permanent)?;
        validate_bounded(&message.thread_id, MAX_REMOTE_ID_BYTES)
            .map_err(|_| GmailApiError::Permanent)?;
        if !ids.insert(&message.id) {
            return Err(GmailApiError::Permanent);
        }
    }
    Ok(())
}

fn validate_snapshot(snapshot: &GmailMessageSnapshot) -> Result<(), GmailApiError> {
    validate_bounded(&snapshot.id, MAX_REMOTE_ID_BYTES).map_err(|_| GmailApiError::Permanent)?;
    validate_bounded(&snapshot.thread_id, MAX_REMOTE_ID_BYTES)
        .map_err(|_| GmailApiError::Permanent)?;
    validate_history_id(snapshot.history_id.clone())?;
    if snapshot.internal_date_ms < 0 || snapshot.label_ids.len() > MAX_MESSAGE_LABELS {
        return Err(GmailApiError::Permanent);
    }
    for label_id in &snapshot.label_ids {
        validate_bounded(label_id, MAX_REMOTE_ID_BYTES).map_err(|_| GmailApiError::Permanent)?;
    }
    if snapshot.snippet.len() > 64 * 1024 {
        return Err(GmailApiError::Permanent);
    }
    Ok(())
}

fn validate_labels(mut labels: Vec<GmailLabel>) -> Result<Vec<GmailLabel>, GmailApiError> {
    if labels.len() > MAX_LABELS {
        return Err(GmailApiError::Permanent);
    }
    let mut ids = BTreeSet::new();
    for label in &labels {
        validate_bounded(&label.id, MAX_REMOTE_ID_BYTES).map_err(|_| GmailApiError::Permanent)?;
        validate_bounded(&label.name, 512).map_err(|_| GmailApiError::Permanent)?;
        if !ids.insert(&label.id) {
            return Err(GmailApiError::Permanent);
        }
    }
    labels.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(labels)
}

fn validate_history_page(page: &GmailHistoryPage) -> Result<(), GmailApiError> {
    if page.changes.len() > MAX_HISTORY_CHANGES {
        return Err(GmailApiError::Permanent);
    }
    validate_history_id(page.history_id.clone())?;
    validate_optional(page.next_page_token.as_deref(), MAX_PAGE_TOKEN_BYTES)
        .map_err(|_| GmailApiError::Permanent)?;
    for change in &page.changes {
        validate_bounded(&change.message_id, MAX_REMOTE_ID_BYTES)
            .map_err(|_| GmailApiError::Permanent)?;
        validate_optional(change.thread_id.as_deref(), MAX_REMOTE_ID_BYTES)
            .map_err(|_| GmailApiError::Permanent)?;
    }
    Ok(())
}

fn validate_history_id(value: String) -> Result<String, GmailApiError> {
    validate_bounded(&value, MAX_HISTORY_ID_BYTES).map_err(|_| GmailApiError::Permanent)?;
    if !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(GmailApiError::Permanent);
    }
    Ok(value)
}

fn dedupe_changes(changes: Vec<GmailHistoryChange>) -> Vec<GmailHistoryChange> {
    let mut values = BTreeMap::new();
    for change in changes {
        values.insert(change.message_id.clone(), change);
    }
    values.into_values().collect()
}

fn history_digest(changes: &[GmailHistoryChange], page: &GmailHistoryPage) -> String {
    let mut hasher = Sha256::new();
    for change in changes {
        hasher.update(change.message_id.as_bytes());
        hasher.update([0]);
        if let Some(thread_id) = &change.thread_id {
            hasher.update(thread_id.as_bytes());
        }
        hasher.update([0xff]);
    }
    // Gmail's top-level historyId is a moving current-mailbox watermark. It
    // can advance while we revisit one immutable history record in bounded
    // slices, so it is deliberately excluded from the record-content digest.
    if let Some(token) = &page.next_page_token {
        hasher.update(token.as_bytes());
    }
    hex(&hasher.finalize())
}

fn label_digest(labels: &[GmailLabel]) -> String {
    let mut hasher = Sha256::new();
    for label in labels {
        hasher.update(label.id.as_bytes());
        hasher.update([0]);
        hasher.update(label.name.as_bytes());
        hasher.update([0]);
        hasher.update([u8::from(label.system), 0xff]);
    }
    hex(&hasher.finalize())
}

fn label_reconciliation_generation(batch_id: &str) -> String {
    format!(
        "gmail-label-rescan-{}",
        &hex(&Sha256::digest(batch_id.as_bytes()))[..40]
    )
}

fn next_request(
    phase: GmailSyncPhase,
    _now_ms: i64,
    current: &GmailSyncWork,
) -> Result<(GmailSyncWork, String), GmailApiError> {
    let cursor = cursor_json(&phase)?;
    let next = make_work(phase, Some(cursor.clone())).map_err(|_| GmailApiError::Permanent)?;
    if next.batch_id == current.batch_id {
        return Err(GmailApiError::Permanent);
    }
    Ok((next, cursor))
}

fn make_work(
    phase: GmailSyncPhase,
    expected_prior_cursor: Option<String>,
) -> Result<GmailSyncWork, &'static str> {
    make_work_seeded(phase, expected_prior_cursor, "")
}

fn make_work_seeded(
    phase: GmailSyncPhase,
    expected_prior_cursor: Option<String>,
    identity_seed: &str,
) -> Result<GmailSyncWork, &'static str> {
    if identity_seed.len() > 256 || identity_seed.chars().any(char::is_control) {
        return Err("Gmail work identity seed is invalid");
    }
    let identity = serde_json::to_vec(&(&phase, &expected_prior_cursor, identity_seed))
        .map_err(|_| "Gmail work serialization failed")?;
    let batch_id = format!("gmail-{}", &hex(&Sha256::digest(identity))[..40]);
    let request = GmailSyncWork {
        version: WORK_FORMAT_VERSION,
        batch_id,
        expected_prior_cursor,
        phase,
    };
    request.validate()?;
    Ok(request)
}

fn cursor_json(phase: &GmailSyncPhase) -> Result<String, GmailApiError> {
    let value = serde_json::to_string(&serde_json::json!({
        "version": WORK_FORMAT_VERSION,
        "provider": "gmail",
        "phase": phase,
    }))
    .map_err(|_| GmailApiError::Permanent)?;
    validate_bounded(&value, 16 * 1024).map_err(|_| GmailApiError::Permanent)?;
    Ok(value)
}

fn continuation_for(
    request: GmailSyncWork,
    now_ms: i64,
) -> Result<ProviderSyncContinuation, GmailApiError> {
    let payload_json = serde_json::to_string(&request).map_err(|_| GmailApiError::Permanent)?;
    if payload_json.len() > 1024 * 1024 {
        return Err(GmailApiError::Permanent);
    }
    Ok(ProviderSyncContinuation {
        id: format!("work-{}", request.batch_id),
        ordering_key: request.batch_id,
        payload_json,
        priority: 0,
        available_at: now_ms,
        max_attempts: 8,
    })
}

fn work_item(
    account_id: &str,
    request: GmailSyncWork,
    now_ms: i64,
) -> Result<NewWorkItem, &'static str> {
    let payload_json =
        serde_json::to_string(&request).map_err(|_| "Gmail work serialization failed")?;
    Ok(NewWorkItem {
        id: format!("work-{}", request.batch_id),
        account_id: account_id.into(),
        operation_id: None,
        kind: WorkKind::Sync,
        scope: SYNC_SCOPE.into(),
        ordering_key: request.batch_id,
        payload_json,
        priority: 0,
        available_at: now_ms,
        max_attempts: 8,
    })
}

fn access_error_outcome(error: GmailAccessError) -> WorkerOutcome {
    match error {
        GmailAccessError::CredentialUnavailable => WorkerOutcome::CredentialUnavailable,
        GmailAccessError::ReauthorizationRequired => WorkerOutcome::AuthenticationExpired {
            code: "gmail_reauthorization_required".into(),
        },
        GmailAccessError::Retryable => WorkerOutcome::RetryableFailure {
            code: "gmail_access_retryable".into(),
        },
        GmailAccessError::RateLimited { retry_after_at } => WorkerOutcome::RateLimited {
            code: "gmail_access_rate_limited".into(),
            retry_after_at,
        },
        GmailAccessError::Permanent => WorkerOutcome::PermanentFailure {
            code: "gmail_access_invalid".into(),
        },
    }
}

fn send_access_error_outcome(error: GmailAccessError) -> WorkerOutcome {
    match error {
        GmailAccessError::CredentialUnavailable => WorkerOutcome::CredentialUnavailable,
        GmailAccessError::ReauthorizationRequired => WorkerOutcome::AuthenticationExpired {
            code: "gmail_reauthorization_required".into(),
        },
        GmailAccessError::Retryable => WorkerOutcome::RejectedBeforeSubmission {
            code: "gmail_access_retryable_before_send".into(),
        },
        GmailAccessError::RateLimited { retry_after_at } => WorkerOutcome::RateLimited {
            code: "gmail_access_rate_limited".into(),
            retry_after_at,
        },
        GmailAccessError::Permanent => WorkerOutcome::PermanentFailure {
            code: "gmail_access_invalid".into(),
        },
    }
}

fn api_error_outcome(error: GmailApiError) -> WorkerOutcome {
    match error {
        GmailApiError::Unauthorized => WorkerOutcome::AuthenticationExpired {
            code: "gmail_authorization_expired".into(),
        },
        GmailApiError::RateLimited { retry_after_at } => WorkerOutcome::RateLimited {
            code: "gmail_rate_limited".into(),
            retry_after_at,
        },
        GmailApiError::Retryable => WorkerOutcome::RetryableFailure {
            code: "gmail_service_unavailable".into(),
        },
        GmailApiError::Permanent
        | GmailApiError::NotFound
        | GmailApiError::RejectedBeforeSubmission => WorkerOutcome::PermanentFailure {
            code: "gmail_response_invalid".into(),
        },
        GmailApiError::PreSubmissionRetryable => WorkerOutcome::RetryableFailure {
            code: "gmail_pre_submission_retryable".into(),
        },
        GmailApiError::AmbiguousSubmission => WorkerOutcome::OutcomeUnknown {
            code: "gmail_submission_outcome_unknown".into(),
        },
        GmailApiError::HistoryExpired => WorkerOutcome::PermanentFailure {
            code: "gmail_history_state_invalid".into(),
        },
    }
}

fn pre_submission_api_error_outcome(error: GmailApiError) -> WorkerOutcome {
    match error {
        GmailApiError::Unauthorized => WorkerOutcome::AuthenticationExpired {
            code: "gmail_authorization_expired".into(),
        },
        GmailApiError::RateLimited { retry_after_at } => WorkerOutcome::RateLimited {
            code: "gmail_rate_limited".into(),
            retry_after_at,
        },
        GmailApiError::Retryable | GmailApiError::PreSubmissionRetryable => {
            WorkerOutcome::RejectedBeforeSubmission {
                code: "gmail_pre_submission_retryable".into(),
            }
        }
        _ => WorkerOutcome::PermanentFailure {
            code: "gmail_pre_submission_invalid".into(),
        },
    }
}

fn send_submission_error_outcome(error: GmailApiError) -> WorkerOutcome {
    match error {
        GmailApiError::Unauthorized => WorkerOutcome::AuthenticationExpired {
            code: "gmail_authorization_expired".into(),
        },
        GmailApiError::RateLimited { retry_after_at } => WorkerOutcome::RateLimited {
            code: "gmail_send_rate_limited".into(),
            retry_after_at,
        },
        GmailApiError::PreSubmissionRetryable => WorkerOutcome::RejectedBeforeSubmission {
            code: "gmail_send_not_submitted".into(),
        },
        GmailApiError::RejectedBeforeSubmission => WorkerOutcome::PermanentFailure {
            code: "gmail_send_rejected".into(),
        },
        GmailApiError::AmbiguousSubmission | GmailApiError::Retryable => {
            WorkerOutcome::OutcomeUnknown {
                code: "gmail_submission_outcome_unknown".into(),
            }
        }
        GmailApiError::Permanent | GmailApiError::NotFound | GmailApiError::HistoryExpired => {
            WorkerOutcome::PermanentFailure {
                code: "gmail_send_response_invalid".into(),
            }
        }
    }
}

fn validate_send_reconciliation_request(
    request: &GmailSendReconciliationWork,
) -> Result<(), &'static str> {
    for value in [
        request.original_work_id.as_str(),
        request.operation_id.as_str(),
        request.submission_message_id.as_str(),
        request.client_correlation_id.as_str(),
    ] {
        validate_bounded(value, MAX_REMOTE_ID_BYTES)?;
    }
    validate_digest(Some(&request.send_payload_fingerprint_hex))?;
    validate_optional(
        request.frozen_remote_thread_id.as_deref(),
        MAX_REMOTE_ID_BYTES,
    )?;
    if !(0..=crate::outgoing::MAX_RFC3339_UNIX_MILLIS).contains(&request.queued_at_ms) {
        return Err("Gmail reconciliation queued timestamp is invalid");
    }
    crate::internet_message::validate_message_id(&request.submission_message_id)
        .map_err(|_| "Gmail reconciliation Message-ID is invalid")
}

fn send_reconciliation_is_eligible(
    database_path: &Path,
    work: &ClaimedWork,
    request: &GmailSendReconciliationWork,
) -> Result<bool, ()> {
    let connection = Connection::open(database_path).map_err(|_| ())?;
    let row = connection
        .query_row(
            "SELECT original.account_id, original.kind, original.state,
                    original.operation_id, lower(hex(original.payload_fingerprint)),
                    original.payload_json, operation.state
             FROM provider_work_items original
             JOIN operations operation ON operation.id = original.operation_id
             WHERE original.id = ?1",
            [&request.original_work_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                ))
            },
        )
        .optional()
        .map_err(|_| ())?;
    let Some((account_id, kind, state, operation_id, fingerprint, payload_json, operation_state)) =
        row
    else {
        return Ok(false);
    };
    if account_id != work.account_id
        || kind != "send"
        || operation_id != request.operation_id
        || fingerprint != request.send_payload_fingerprint_hex
        || hex(&Sha256::digest(payload_json.as_bytes())) != request.send_payload_fingerprint_hex
    {
        return Ok(false);
    }
    let prepared =
        crate::outgoing::prepare_from_durable_payload(&payload_json, &[]).map_err(|_| ())?;
    if prepared.provider_kind.as_deref() != Some("gmail")
        || prepared.message_id != request.submission_message_id
        || prepared.client_correlation_id.as_deref() != Some(request.client_correlation_id.as_str())
        || prepared.remote_thread_id != request.frozen_remote_thread_id
        || prepared.queued_at_ms != request.queued_at_ms
    {
        return Ok(false);
    }
    Ok(state == "outcome_unknown" && operation_state == "outcome_unknown")
}

fn contract_error<T>(_error: T) -> GmailApiError {
    GmailApiError::Permanent
}

fn validate_optional(value: Option<&str>, max_bytes: usize) -> Result<(), &'static str> {
    if let Some(value) = value {
        validate_bounded(value, max_bytes)?;
    }
    Ok(())
}

fn validate_digest(value: Option<&str>) -> Result<(), &'static str> {
    if value.is_some_and(|digest| {
        digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        return Err("Gmail digest is invalid");
    }
    Ok(())
}

fn validate_bounded(value: &str, max_bytes: usize) -> Result<(), &'static str> {
    if value.is_empty() || value.len() > max_bytes || value.chars().any(char::is_control) {
        return Err("Gmail value is outside its contract bound");
    }
    Ok(())
}

fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

fn normalize_single_line(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn build_metadata_headers(headers: &[GmailHeaderWire]) -> Result<Vec<u8>, GmailApiError> {
    const MAX_METADATA_HEADERS: usize = 128;
    const MAX_METADATA_BYTES: usize = 64 * 1024;
    const MAX_METADATA_VALUE_BYTES: usize = 16 * 1024;
    if headers.len() > MAX_METADATA_HEADERS {
        return Err(GmailApiError::Permanent);
    }
    let mut output = String::new();
    for header in headers {
        let canonical_name = [
            "Subject",
            "From",
            "To",
            "Cc",
            "Message-ID",
            "In-Reply-To",
            "References",
        ]
        .into_iter()
        .find(|name| header.name.eq_ignore_ascii_case(name));
        let Some(name) = canonical_name else {
            continue;
        };
        let value = truncate_utf8(
            &normalize_single_line(&header.value),
            MAX_METADATA_VALUE_BYTES,
        );
        let overhead = name.len().saturating_add(4);
        let remaining = MAX_METADATA_BYTES.saturating_sub(output.len().saturating_add(overhead));
        if remaining == 0 {
            break;
        }
        let value = truncate_utf8(&value, remaining);
        writeln!(&mut output, "{name}: {value}\r").expect("writing to String cannot fail");
    }
    if output.len().saturating_add(2) > MAX_METADATA_BYTES {
        return Err(GmailApiError::Permanent);
    }
    output.push_str("\r\n");
    Ok(output.into_bytes())
}

fn hex(bytes: &[u8]) -> String {
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut value, "{byte:02x}").expect("writing to String cannot fail");
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::ProviderBodyState;
    use crate::store::MuxStore;
    use crate::worker::{DurableWorker, WorkState, WorkerConfig};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tempfile::tempdir;

    struct OpenAccess;

    impl GmailAccessSource for OpenAccess {
        fn access_for_account(
            &self,
            _account_id: &str,
            _now_ms: i64,
        ) -> Result<GmailAccessGrant, GmailAccessError> {
            Ok(GmailAccessGrant {
                access_value: Zeroizing::new("synthetic-access-value".into()),
                remote_account_id: "subject-1".into(),
            })
        }

        fn access_for_mutation(
            &self,
            account_id: &str,
            now_ms: i64,
        ) -> Result<GmailAccessGrant, GmailAccessError> {
            self.access_for_account(account_id, now_ms)
        }
    }

    struct UnavailableCredentialAccess;

    impl GmailAccessSource for UnavailableCredentialAccess {
        fn access_for_account(
            &self,
            _account_id: &str,
            _now_ms: i64,
        ) -> Result<GmailAccessGrant, GmailAccessError> {
            Err(GmailAccessError::CredentialUnavailable)
        }

        fn access_for_mutation(
            &self,
            _account_id: &str,
            _now_ms: i64,
        ) -> Result<GmailAccessGrant, GmailAccessError> {
            Err(GmailAccessError::CredentialUnavailable)
        }
    }

    struct ReadOnlyAccess;

    impl GmailAccessSource for ReadOnlyAccess {
        fn access_for_account(
            &self,
            _account_id: &str,
            _now_ms: i64,
        ) -> Result<GmailAccessGrant, GmailAccessError> {
            Ok(GmailAccessGrant {
                access_value: Zeroizing::new("synthetic-read-access".into()),
                remote_account_id: "subject-1".into(),
            })
        }

        fn access_for_mutation(
            &self,
            _account_id: &str,
            _now_ms: i64,
        ) -> Result<GmailAccessGrant, GmailAccessError> {
            Err(GmailAccessError::ReauthorizationRequired)
        }
    }

    type RecordedMutation = (String, Vec<String>, Vec<String>);

    struct FixtureApi {
        calls: AtomicUsize,
        label_calls: AtomicUsize,
        history_calls: AtomicUsize,
        history_expired: bool,
        advancing_history_watermark: bool,
        changing_history_token: bool,
        listed_message_disappears: bool,
        profile_email: String,
        labels: Vec<GmailLabel>,
        label_versions: Vec<Vec<GmailLabel>>,
        messages: BTreeMap<String, GmailMessageSnapshot>,
        history_changes: Vec<GmailHistoryChange>,
        mutation_error: Option<GmailApiError>,
        mutation_thread_exists_after_not_found: bool,
        mutation_thread_exists_error: Option<GmailApiError>,
        mutations: Mutex<Vec<RecordedMutation>>,
        send_error: Option<GmailApiError>,
        send_acknowledgement: GmailSendAcknowledgement,
        send_evidence: GmailSendEvidencePage,
        submissions: Mutex<Vec<(String, Option<String>)>>,
    }

    impl FixtureApi {
        fn standard() -> Self {
            let mut messages = BTreeMap::new();
            for index in 0..12 {
                let id = format!("message-{index:02}");
                messages.insert(id.clone(), fixture_message(&id, "thread-a", index));
            }
            let labels = vec![
                GmailLabel {
                    id: "INBOX".into(),
                    name: "Inbox".into(),
                    system: true,
                },
                GmailLabel {
                    id: "Label_1".into(),
                    name: "Projects".into(),
                    system: false,
                },
            ];
            Self {
                calls: AtomicUsize::new(0),
                label_calls: AtomicUsize::new(0),
                history_calls: AtomicUsize::new(0),
                history_expired: false,
                advancing_history_watermark: false,
                changing_history_token: false,
                listed_message_disappears: false,
                profile_email: "subject-1".into(),
                labels,
                label_versions: Vec::new(),
                messages,
                history_changes: Vec::new(),
                mutation_error: None,
                mutation_thread_exists_after_not_found: false,
                mutation_thread_exists_error: None,
                mutations: Mutex::new(Vec::new()),
                send_error: None,
                send_acknowledgement: GmailSendAcknowledgement {
                    message_id: "gmail-sent-message".into(),
                    thread_id: "gmail-sent-thread".into(),
                },
                send_evidence: GmailSendEvidencePage {
                    exact: Vec::new(),
                    mismatched_candidates: 0,
                    has_more: false,
                },
                submissions: Mutex::new(Vec::new()),
            }
        }
    }

    impl GmailApi for FixtureApi {
        fn get_profile(&self, _access: &str) -> Result<GmailProfile, GmailApiError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(GmailProfile {
                email_address: self.profile_email.clone(),
                history_id: "100".into(),
            })
        }

        fn list_labels(&self, _access: &str) -> Result<Vec<GmailLabel>, GmailApiError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let call = self.label_calls.fetch_add(1, Ordering::SeqCst);
            Ok(if self.label_versions.is_empty() {
                self.labels.clone()
            } else {
                self.label_versions[call.min(self.label_versions.len() - 1)].clone()
            })
        }

        fn list_messages(
            &self,
            _access: &str,
            page_token: Option<&str>,
            max_results: usize,
        ) -> Result<GmailMessagePage, GmailApiError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(max_results, MESSAGE_PAGE_SIZE);
            let offset = if page_token.is_some() { 10 } else { 0 };
            let mut messages = self
                .messages
                .values()
                .skip(offset)
                .take(MESSAGE_PAGE_SIZE)
                .map(|message| GmailMessageRef {
                    id: message.id.clone(),
                    thread_id: message.thread_id.clone(),
                })
                .collect::<Vec<_>>();
            if offset == 0 && self.listed_message_disappears {
                messages[0] = GmailMessageRef {
                    id: "deleted-between-list-and-fetch".into(),
                    thread_id: "thread-a".into(),
                };
            }
            let has_more = offset.saturating_add(messages.len()) < self.messages.len();
            Ok(GmailMessagePage {
                messages,
                next_page_token: has_more.then(|| "page-2".into()),
            })
        }

        fn get_message(
            &self,
            _access: &str,
            message_id: &str,
        ) -> Result<GmailMessageSnapshot, GmailApiError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.messages
                .get(message_id)
                .cloned()
                .ok_or(GmailApiError::NotFound)
        }

        fn list_history(
            &self,
            _access: &str,
            _start_history_id: &str,
            _page_token: Option<&str>,
        ) -> Result<GmailHistoryPage, GmailApiError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.history_expired {
                return Err(GmailApiError::HistoryExpired);
            }
            let watermark = self.history_calls.fetch_add(1, Ordering::SeqCst);
            Ok(GmailHistoryPage {
                changes: self.history_changes.clone(),
                next_page_token: self
                    .changing_history_token
                    .then(|| format!("history-page-{watermark}")),
                history_id: if self.advancing_history_watermark {
                    format!("{}", 120 + watermark)
                } else {
                    "120".into()
                },
            })
        }

        fn modify_thread(
            &self,
            _access: &str,
            thread_id: &str,
            add_label_ids: &[String],
            remove_label_ids: &[String],
        ) -> Result<(), GmailApiError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(error) = self.mutation_error.clone() {
                return Err(error);
            }
            self.mutations.lock().unwrap().push((
                format!("modify:{thread_id}"),
                add_label_ids.to_vec(),
                remove_label_ids.to_vec(),
            ));
            Ok(())
        }

        fn trash_thread(&self, _access: &str, thread_id: &str) -> Result<(), GmailApiError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(error) = self.mutation_error.clone() {
                return Err(error);
            }
            self.mutations.lock().unwrap().push((
                format!("trash:{thread_id}"),
                Vec::new(),
                Vec::new(),
            ));
            Ok(())
        }

        fn untrash_thread(&self, _access: &str, thread_id: &str) -> Result<(), GmailApiError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(error) = self.mutation_error.clone() {
                return Err(error);
            }
            self.mutations.lock().unwrap().push((
                format!("untrash:{thread_id}"),
                Vec::new(),
                Vec::new(),
            ));
            Ok(())
        }

        fn thread_exists(&self, _access: &str, _thread_id: &str) -> Result<bool, GmailApiError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(error) = self.mutation_thread_exists_error.clone() {
                return Err(error);
            }
            Ok(self.mutation_thread_exists_after_not_found)
        }

        fn send_message(
            &self,
            _access: &str,
            raw_base64url: &str,
            frozen_thread_id: Option<&str>,
            before_io: &mut dyn FnMut() -> Result<(), GmailApiError>,
        ) -> Result<GmailSendAcknowledgement, GmailApiError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if matches!(self.send_error, Some(GmailApiError::PreSubmissionRetryable)) {
                return Err(GmailApiError::PreSubmissionRetryable);
            }
            before_io()?;
            self.submissions
                .lock()
                .unwrap()
                .push((raw_base64url.into(), frozen_thread_id.map(str::to_owned)));
            if let Some(error) = self.send_error.clone() {
                return Err(error);
            }
            Ok(self.send_acknowledgement.clone())
        }

        fn find_send_evidence(
            &self,
            _access: &str,
            _submission_message_id: &str,
            _client_correlation_id: &str,
        ) -> Result<GmailSendEvidencePage, GmailApiError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.send_evidence.clone())
        }
    }

    fn fixture_message(id: &str, thread_id: &str, index: usize) -> GmailMessageSnapshot {
        let raw = format!(
            "From: Example Sender <sender@example.test>\r\nTo: Reader <reader@example.test>\r\nSubject: Message {index}\r\nMessage-ID: <message-{index}@example.test>\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nBody {index} with https://example.test/{index}\r\n"
        );
        GmailMessageSnapshot {
            id: id.into(),
            thread_id: thread_id.into(),
            label_ids: vec!["INBOX".into(), "UNREAD".into(), "Label_1".into()],
            snippet: format!("Body {index}"),
            history_id: format!("{}", 101 + index),
            internal_date_ms: 1_000 + index as i64,
            raw: Some(URL_SAFE_NO_PAD.encode(raw)),
            metadata_headers: None,
        }
    }

    fn fixture_label_digest() -> String {
        label_digest(&FixtureApi::standard().labels)
    }

    fn claim_for(item: NewWorkItem) -> ClaimedWork {
        ClaimedWork {
            id: item.id,
            account_id: item.account_id,
            operation_id: None,
            kind: item.kind,
            scope: item.scope,
            ordering_key: item.ordering_key,
            payload_json: item.payload_json,
            payload_fingerprint_hex: "00".repeat(32),
            attempt: 1,
            lease_token: "lease".into(),
            lease_expires_at: 10_000,
        }
    }

    fn claim_for_continuation(
        account_id: &str,
        continuation: ProviderSyncContinuation,
    ) -> ClaimedWork {
        ClaimedWork {
            id: continuation.id,
            account_id: account_id.into(),
            operation_id: None,
            kind: WorkKind::Sync,
            scope: SYNC_SCOPE.into(),
            ordering_key: continuation.ordering_key,
            payload_json: continuation.payload_json,
            payload_fingerprint_hex: "00".repeat(32),
            attempt: 1,
            lease_token: "lease".into(),
            lease_expires_at: 10_000,
        }
    }

    fn mutation_claim(
        operation_id: &str,
        field: &str,
        value: &str,
        remote_container_id: Option<&str>,
    ) -> ClaimedWork {
        let payload = ThreadMutationPayload {
            format_version: Some(1),
            operation_id: operation_id.into(),
            account_id: Some("gmail-account".into()),
            thread_id: 7,
            field: field.into(),
            value: value.into(),
            remote_thread_id: Some("remote-thread".into()),
            remote_container_id: remote_container_id.map(str::to_owned),
            undo_of: None,
        };
        ClaimedWork {
            id: format!("work-{operation_id}"),
            account_id: "gmail-account".into(),
            operation_id: Some(operation_id.into()),
            kind: WorkKind::Mutation,
            scope: GMAIL_ACCOUNT_SCOPE.into(),
            ordering_key: operation_id.into(),
            payload_json: serde_json::to_string(&payload).unwrap(),
            payload_fingerprint_hex: "00".repeat(32),
            attempt: 1,
            lease_token: "lease".into(),
            lease_expires_at: 10_000,
        }
    }

    #[test]
    fn refresh_cadence_schedules_a_sync_only_once_the_interval_has_elapsed() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("gmail-refresh.db");
        drop(MuxStore::open(&path, false).expect("native schema"));
        let connection = Connection::open(&path).expect("fixture connection");
        connection
            .execute(
                "INSERT INTO accounts(id, name, email, color, provider)
                 VALUES('gmail-account', 'Gmail', 'reader@example.test', '#000000', 'gmail')",
                [],
            )
            .expect("account fixture");
        connection
            .execute(
                "INSERT INTO provider_accounts(
                   account_id, provider_kind, remote_account_id, auth_state,
                   credential_ref, sync_state, refresh_seconds, last_sync_at,
                   created_at, updated_at
                 ) VALUES(
                   'gmail-account', 'gmail', 'subject-1', 'ready',
                   'gmail/account/a', 'idle', 60, 1_000_000, 1, 1
                 )",
                [],
            )
            .expect("provider account fixture");
        let cursor = cursor_json(&GmailSyncPhase::History {
            committed_history_id: "120".into(),
            committed_label_digest: None,
            page_token: None,
            offset: 0,
            page_digest: None,
            reconciliation_generation: None,
        })
        .expect("history cursor");
        connection
            .execute(
                "INSERT INTO provider_sync_cursors(account_id, scope, cursor, updated_at)
                 VALUES('gmail-account', 'a:v1', ?1, 1_000_000)",
                [&cursor],
            )
            .expect("durable cursor");
        drop(connection);

        // One second short of the cadence: nothing is due.
        assert_eq!(
            schedule_due_syncs(&path, 1_059_000).expect("early refresh"),
            0
        );
        // Exactly at the cadence: the account is scheduled.
        assert_eq!(
            schedule_due_syncs(&path, 1_060_000).expect("due refresh"),
            1
        );
        // Work is already queued, so a second tick must not pile on.
        assert_eq!(
            schedule_due_syncs(&path, 1_120_000).expect("no duplicate refresh"),
            0
        );

        // A longer cadence pushes the next refresh out.
        let connection = Connection::open(&path).expect("verify connection");
        connection
            .execute(
                "UPDATE provider_work_items SET state = 'succeeded', completed_at = 1_060_500
                 WHERE account_id = 'gmail-account'",
                [],
            )
            .expect("complete the scheduled work");
        connection
            .execute(
                "UPDATE provider_accounts
                 SET refresh_seconds = 900, sync_state = 'idle', last_sync_at = 1_060_500
                 WHERE account_id = 'gmail-account'",
                [],
            )
            .expect("slower cadence");
        drop(connection);
        assert_eq!(
            schedule_due_syncs(&path, 1_120_000).expect("still inside the longer cadence"),
            0
        );
        assert_eq!(
            schedule_due_syncs(&path, 1_960_500).expect("longer cadence elapsed"),
            1
        );
    }

    #[test]
    fn gmail_provider_conformance_initial_schedule_is_atomic_and_idempotent() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("gmail-schedule.db");
        drop(MuxStore::open(&path, false).expect("native schema"));
        let connection = Connection::open(&path).expect("fixture connection");
        connection
            .execute(
                "INSERT INTO accounts(id, name, email, color, provider)
                 VALUES('gmail-account', 'Gmail', 'reader@example.test', '#000000', 'gmail')",
                [],
            )
            .expect("account fixture");
        connection
            .execute(
                "INSERT INTO provider_accounts(
                   account_id, provider_kind, remote_account_id, auth_state,
                   credential_ref, sync_state, created_at, updated_at
                 ) VALUES(
                   'gmail-account', 'gmail', 'subject-1', 'ready',
                   'gmail/fixture/a', 'never_synced', 0, 0
                 )",
                [],
            )
            .expect("provider account fixture");
        drop(connection);

        assert_eq!(schedule_initial_syncs(&path, 5_000).expect("schedule"), 1);
        assert_eq!(schedule_initial_syncs(&path, 5_001).expect("idempotent"), 0);
        let connection = Connection::open(&path).expect("inspect fixture");
        let state: String = connection
            .query_row(
                "SELECT sync_state FROM provider_accounts WHERE account_id = 'gmail-account'",
                [],
                |row| row.get(0),
            )
            .expect("scheduled state");
        let work_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM provider_work_items
                 WHERE account_id = 'gmail-account' AND scope = ?1",
                [SYNC_SCOPE],
                |row| row.get(0),
            )
            .expect("scheduled work count");
        assert_eq!(state, "scheduled");
        assert_eq!(work_count, 1);

        connection
            .execute(
                "UPDATE provider_work_items
                 SET state = 'succeeded', completed_at = 5_100
                 WHERE account_id = 'gmail-account'",
                [],
            )
            .expect("complete initial work");
        connection
            .execute(
                "UPDATE provider_accounts SET sync_state = 'idle'
                 WHERE account_id = 'gmail-account'",
                [],
            )
            .expect("idle account");
        let cursor = cursor_json(&GmailSyncPhase::History {
            committed_history_id: "120".into(),
            committed_label_digest: None,
            page_token: None,
            offset: 0,
            page_digest: None,
            reconciliation_generation: None,
        })
        .expect("completed history cursor");
        connection
            .execute(
                "INSERT INTO provider_sync_cursors(account_id, scope, cursor, updated_at)
                 VALUES('gmail-account', 'a:v1', ?1, 5_100)",
                [&cursor],
            )
            .expect("durable history cursor");
        drop(connection);

        let worker = DurableWorker::new(&path, WorkerConfig::default()).expect("durable worker");
        assert_eq!(
            schedule_resumable_syncs(&path, 6_000).expect("resume durable cursor"),
            1
        );
        assert_eq!(
            schedule_resumable_syncs(&path, 6_001).expect("active work suppresses duplicate"),
            0
        );
        let connection = Connection::open(&path).expect("inspect resumed fixture");
        let resumed: (String, i64) = connection
            .query_row(
                "SELECT account.sync_state, COUNT(work.id)
                 FROM provider_accounts account
                 JOIN provider_work_items work ON work.account_id = account.account_id
                 WHERE account.account_id = 'gmail-account'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("resumed work");
        assert_eq!(resumed, ("scheduled".into(), 2));
    }

    #[test]
    fn gmail_provider_conformance_corrupt_cursor_isolated_from_other_unlocked_account() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("gmail-cursor-isolation.db");
        drop(MuxStore::open(&path, false).expect("native schema"));
        let connection = Connection::open(&path).expect("fixture connection");
        for account_id in ["gmail-good", "gmail-corrupt"] {
            connection
                .execute(
                    "INSERT INTO accounts(id, name, email, color, provider)
                     VALUES(?1, ?1, ?2, '#000000', 'gmail')",
                    params![account_id, format!("{account_id}@example.test")],
                )
                .expect("account fixture");
            connection
                .execute(
                    "INSERT INTO provider_accounts(
                       account_id, provider_kind, remote_account_id, auth_state,
                       credential_ref, sync_state, created_at, updated_at
                     ) VALUES(?1, 'gmail', ?2, 'ready', ?3, 'idle', 0, 0)",
                    params![
                        account_id,
                        format!("{account_id}@example.test"),
                        format!("gmail/{account_id}")
                    ],
                )
                .expect("provider account fixture");
        }
        let good_cursor = cursor_json(&GmailSyncPhase::History {
            committed_history_id: "120".into(),
            committed_label_digest: None,
            page_token: None,
            offset: 0,
            page_digest: None,
            reconciliation_generation: None,
        })
        .expect("good cursor");
        connection
            .execute(
                "INSERT INTO provider_sync_cursors(account_id, scope, cursor, updated_at)
                 VALUES('gmail-good', 'a:v1', ?1, 1)",
                [&good_cursor],
            )
            .expect("good cursor fixture");
        connection
            .execute(
                "INSERT INTO provider_sync_cursors(account_id, scope, cursor, updated_at)
                 VALUES('gmail-corrupt', 'a:v1', ?1, 1)",
                ["invalid\ncursor"],
            )
            .expect("corrupt cursor fixture");
        drop(connection);
        let worker = DurableWorker::new(&path, WorkerConfig::default()).expect("durable worker");

        assert_eq!(
            schedule_resumable_syncs(&path, 4).expect("isolated scheduling"),
            1
        );
        let connection = Connection::open(&path).expect("inspect isolation");
        let good: (String, i64) = connection
            .query_row(
                "SELECT sync_state, (SELECT COUNT(*) FROM provider_work_items
                   WHERE account_id = 'gmail-good')
                 FROM provider_accounts WHERE account_id = 'gmail-good'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("good account state");
        let corrupt: String = connection
            .query_row(
                "SELECT sync_state || ':' || last_error_code
                 FROM provider_accounts WHERE account_id = 'gmail-corrupt'",
                [],
                |row| row.get(0),
            )
            .expect("corrupt account state");
        assert_eq!(good, ("scheduled".into(), 1));
        assert_eq!(corrupt, "failed:gmail_cursor_invalid");
    }

    #[test]
    fn gmail_provider_conformance_bootstrap_is_bounded_and_uses_hostile_mime_boundary() {
        let adapter = GmailAdapter::new(OpenAccess, FixtureApi::standard(), || 5_000);
        let work = initial_sync_work("gmail-account", "initial", 0).expect("initial work");
        let outcome = adapter.execute(&claim_for(work), &WorkerExecutionContext::new());
        let WorkerOutcome::Succeeded { projection } = outcome else {
            panic!("bootstrap must succeed: {outcome:?}")
        };
        let WorkerProjection::ProviderSyncPage(label_page) = projection else {
            panic!("Gmail emits a sync page")
        };
        assert!(label_page.batch.message_upserts.is_empty());
        assert_eq!(label_page.batch.container_upserts.len(), 2);
        assert!(label_page
            .reconciliation
            .as_ref()
            .is_some_and(|run| run.begin));
        assert!(label_page.capabilities.is_some());
        let outcome = adapter.execute(
            &claim_for_continuation(
                "gmail-account",
                label_page.continuation.expect("message-page continuation"),
            ),
            &WorkerExecutionContext::new(),
        );
        let WorkerOutcome::Succeeded { projection } = outcome else {
            panic!("bootstrap message page must succeed: {outcome:?}")
        };
        let WorkerProjection::ProviderSyncPage(page) = projection else {
            panic!("Gmail emits a sync page")
        };
        assert_eq!(page.batch.message_upserts.len(), MESSAGE_PAGE_SIZE);
        assert_eq!(page.batch.thread_upserts.len(), 1);
        assert_eq!(page.batch.container_upserts.len(), 3);
        assert!(page
            .batch
            .container_upserts
            .iter()
            .any(|container| { container.identity.remote_container_id.as_str() == "UNREAD" }));
        assert!(page
            .batch
            .message_upserts
            .iter()
            .all(|message| message.body_text.as_str().starts_with("Body ")));
        assert!(page.continuation.is_some());
        assert!(!page.complete);
        assert!(page.reconciliation.as_ref().is_some_and(|run| !run.begin));
        assert_eq!(page.batch.membership_changes.len(), MESSAGE_PAGE_SIZE * 3);
        assert!(page.batch.message_upserts.iter().all(|message| {
            message.body_state == ProviderBodyState::Complete
                && message
                    .keywords
                    .iter()
                    .any(|keyword| keyword.as_str() == "unread")
                && message
                    .keywords
                    .iter()
                    .any(|keyword| keyword.as_str() == "has_link")
        }));
    }

    #[test]
    fn gmail_provider_conformance_carries_only_sanitized_html_in_process() {
        let mut api = FixtureApi::standard();
        let html = concat!(
            "From: Sender <sender@example.test>\r\n",
            "To: Reader <reader@example.test>\r\n",
            "Subject: Safe HTML\r\n",
            "Message-ID: <safe-html@example.test>\r\n",
            "Content-Type: text/html; charset=utf-8\r\n\r\n",
            "<p><strong>Hello</strong> <a href=\"https://example.test/path\">there</a>",
            "<img src=\"https://tracker.invalid/pixel\"><script>bad()</script></p>"
        );
        api.messages.get_mut("message-00").unwrap().raw = Some(URL_SAFE_NO_PAD.encode(html));
        let adapter = GmailAdapter::new(OpenAccess, api, || 5_000);
        let work = initial_sync_work("gmail-account", "safe-html", 0).expect("initial work");
        let WorkerOutcome::Succeeded { projection } =
            adapter.execute(&claim_for(work), &WorkerExecutionContext::new())
        else {
            panic!("label page must succeed")
        };
        let WorkerProjection::ProviderSyncPage(label_page) = projection else {
            panic!("Gmail emits a sync page")
        };
        let WorkerOutcome::Succeeded { projection } = adapter.execute(
            &claim_for_continuation(
                "gmail-account",
                label_page.continuation.expect("message continuation"),
            ),
            &WorkerExecutionContext::new(),
        ) else {
            panic!("message page must succeed")
        };
        let WorkerProjection::ProviderSyncPage(page) = projection else {
            panic!("Gmail emits a sync page")
        };
        let content = page
            .restricted_message_content
            .iter()
            .find(|content| content.identity.remote_message_id.as_str() == "message-00")
            .expect("sanitized sidecar");
        assert!(content.body_html.contains("<strong>Hello</strong>"));
        assert!(content
            .body_html
            .contains("href=\"https://example.test/path\""));
        assert!(!content.body_html.contains("<img"));
        assert!(!content.body_html.contains("script"));
        assert_eq!(content.blocked_remote_resources, 1);
        assert_eq!(content.remote_images.len(), 1);
        assert_eq!(content.remote_images[0].domain, "tracker.invalid");
        assert!(content
            .body_html
            .contains("<mux-remote-image data-id=\"1\""));
        let provider_json = serde_json::to_value(page.batch.as_ref())
            .unwrap()
            .to_string();
        assert!(!provider_json.contains("bodyHtml"));
        assert!(!provider_json.contains("tracker.invalid"));
    }

    #[test]
    fn gmail_provider_conformance_bootstrap_tolerates_delete_between_list_and_fetch() {
        let mut api = FixtureApi::standard();
        api.listed_message_disappears = true;
        let adapter = GmailAdapter::new(OpenAccess, api, || 5_000);
        let work = initial_sync_work("gmail-account", "delete-race", 0).expect("initial work");
        let outcome = adapter.execute(&claim_for(work), &WorkerExecutionContext::new());
        let WorkerOutcome::Succeeded { projection } = outcome else {
            panic!("bootstrap delete race must succeed: {outcome:?}")
        };
        let WorkerProjection::ProviderSyncPage(label_page) = projection else {
            panic!("Gmail emits a sync page")
        };
        let outcome = adapter.execute(
            &claim_for_continuation(
                "gmail-account",
                label_page.continuation.expect("message-page continuation"),
            ),
            &WorkerExecutionContext::new(),
        );
        let WorkerOutcome::Succeeded { projection } = outcome else {
            panic!("bootstrap delete race message page must succeed: {outcome:?}")
        };
        let WorkerProjection::ProviderSyncPage(page) = projection else {
            panic!("Gmail emits a sync page")
        };
        assert_eq!(page.batch.message_upserts.len(), MESSAGE_PAGE_SIZE - 1);
        assert!(page.batch.tombstones.is_empty());
        assert!(page.continuation.is_some());
    }

    #[test]
    fn gmail_provider_conformance_ten_thousand_account_labels_are_bounded_into_pages() {
        let mut api = FixtureApi::standard();
        api.labels = (0..10_000)
            .map(|index| GmailLabel {
                id: format!("Label_{index:05}"),
                name: format!("Label {index}"),
                system: false,
            })
            .collect();
        let adapter = GmailAdapter::new(OpenAccess, api, || 5_000);
        let first = adapter.execute(
            &claim_for(initial_sync_work("gmail-account", "many-labels", 0).expect("initial work")),
            &WorkerExecutionContext::new(),
        );
        let WorkerOutcome::Succeeded { projection } = first else {
            panic!("first label page succeeds: {first:?}")
        };
        let WorkerProjection::ProviderSyncPage(first_page) = projection else {
            panic!("Gmail emits a sync page")
        };
        assert_eq!(first_page.batch.container_upserts.len(), LABEL_PAGE_SIZE);
        assert!(first_page.batch.message_upserts.is_empty());
        let second = adapter.execute(
            &claim_for_continuation(
                "gmail-account",
                first_page.continuation.expect("second label page"),
            ),
            &WorkerExecutionContext::new(),
        );
        let WorkerOutcome::Succeeded { projection } = second else {
            panic!("second label page succeeds: {second:?}")
        };
        let WorkerProjection::ProviderSyncPage(second_page) = projection else {
            panic!("Gmail emits a sync page")
        };
        assert_eq!(second_page.batch.container_upserts.len(), LABEL_PAGE_SIZE);
        assert!(second_page.batch.message_upserts.is_empty());
        let next: GmailSyncWork = serde_json::from_str(
            &second_page
                .continuation
                .expect("bounded label continuation")
                .payload_json,
        )
        .expect("label continuation payload");
        assert!(matches!(
            next.phase,
            GmailSyncPhase::Bootstrap {
                label_offset: 2_000,
                ..
            }
        ));
    }

    #[test]
    fn gmail_provider_conformance_sweep_labels_restarts_when_boundary_moves() {
        let original = (0..=LABEL_PAGE_SIZE)
            .map(|index| GmailLabel {
                id: format!("Label_{index:05}"),
                name: format!("Label {index}"),
                system: false,
            })
            .collect::<Vec<_>>();
        let mut changed = original
            .iter()
            .filter(|label| label.id != "Label_00010")
            .cloned()
            .collect::<Vec<_>>();
        changed.push(GmailLabel {
            id: "Label_20000".into(),
            name: "New label".into(),
            system: false,
        });
        let mut api = FixtureApi::standard();
        api.label_versions = vec![original, changed.clone(), changed];
        let adapter = GmailAdapter::new(OpenAccess, api, || 5_000);
        let request = make_work(
            GmailSyncPhase::SweepLabels {
                generation_id: "generation-label-race".into(),
                final_history_id: "120".into(),
                label_offset: 0,
                label_digest: None,
            },
            Some("cursor-before-label-sweep".into()),
        )
        .expect("initial label sweep");
        let first = adapter.execute(
            &claim_for(work_item("gmail-account", request, 0).expect("first label work")),
            &WorkerExecutionContext::new(),
        );
        let WorkerOutcome::Succeeded { projection } = first else {
            panic!("first label page succeeds: {first:?}")
        };
        let WorkerProjection::ProviderSyncPage(first_page) = projection else {
            panic!("Gmail emits a sync page")
        };
        assert_eq!(first_page.batch.container_upserts.len(), LABEL_PAGE_SIZE);

        let second = adapter.execute(
            &claim_for_continuation(
                "gmail-account",
                first_page.continuation.expect("second label page"),
            ),
            &WorkerExecutionContext::new(),
        );
        let WorkerOutcome::Succeeded { projection } = second else {
            panic!("changed label collection restarts: {second:?}")
        };
        let WorkerProjection::ProviderSyncPage(second_page) = projection else {
            panic!("Gmail emits a sync page")
        };
        assert!(second_page.batch.container_upserts.is_empty());
        assert!(second_page.reconciliation.is_none());
        let restart = second_page.continuation.expect("restart continuation");
        let restarted_work: GmailSyncWork =
            serde_json::from_str(&restart.payload_json).expect("restart payload");
        assert!(matches!(
            restarted_work.phase,
            GmailSyncPhase::SweepLabels {
                label_offset: 0,
                label_digest: Some(_),
                ..
            }
        ));

        let third = adapter.execute(
            &claim_for_continuation("gmail-account", restart),
            &WorkerExecutionContext::new(),
        );
        let WorkerOutcome::Succeeded { projection } = third else {
            panic!("restarted label page succeeds: {third:?}")
        };
        let WorkerProjection::ProviderSyncPage(third_page) = projection else {
            panic!("Gmail emits a sync page")
        };
        assert_eq!(third_page.batch.container_upserts.len(), LABEL_PAGE_SIZE);
        assert!(third_page
            .batch
            .container_upserts
            .iter()
            .any(|container| { container.identity.remote_container_id.as_str() == "Label_01000" }));
        assert!(third_page
            .reconciliation
            .as_ref()
            .is_some_and(|reconciliation| reconciliation.reset_seen_containers));
    }

    #[test]
    fn gmail_provider_conformance_zero_message_label_changes_run_authoritative_rescan() {
        let prior_labels = validate_labels(vec![
            GmailLabel {
                id: "Label_keep".into(),
                name: "Old name".into(),
                system: false,
            },
            GmailLabel {
                id: "Label_delete".into(),
                name: "Delete me".into(),
                system: false,
            },
        ])
        .expect("prior labels");
        let current_labels = vec![
            GmailLabel {
                id: "Label_keep".into(),
                name: "Renamed".into(),
                system: false,
            },
            GmailLabel {
                id: "Label_create".into(),
                name: "Created".into(),
                system: false,
            },
        ];
        let mut api = FixtureApi::standard();
        api.labels = current_labels;
        api.messages.clear();
        let adapter = GmailAdapter::new(OpenAccess, api, || 5_000);
        let request = make_work(
            GmailSyncPhase::History {
                committed_history_id: "100".into(),
                committed_label_digest: Some(label_digest(&prior_labels)),
                page_token: None,
                offset: 0,
                page_digest: None,
                reconciliation_generation: None,
            },
            Some("cursor-before-label-change".into()),
        )
        .expect("ordinary delta work");
        let history = adapter.execute(
            &claim_for(work_item("gmail-account", request, 0).expect("history work")),
            &WorkerExecutionContext::new(),
        );
        let WorkerOutcome::Succeeded { projection } = history else {
            panic!("zero-change history succeeds: {history:?}")
        };
        let WorkerProjection::ProviderSyncPage(history_page) = projection else {
            panic!("Gmail emits a sync page")
        };
        assert!(history_page.batch.message_upserts.is_empty());
        let bootstrap = history_page
            .continuation
            .expect("changed labels require authoritative rescan");
        let bootstrap_work: GmailSyncWork =
            serde_json::from_str(&bootstrap.payload_json).expect("bootstrap payload");
        assert!(matches!(
            bootstrap_work.phase,
            GmailSyncPhase::Bootstrap {
                baseline_history_id: None,
                label_offset: 0,
                page_token: None,
                ..
            }
        ));

        let label_begin = adapter.execute(
            &claim_for_continuation("gmail-account", bootstrap),
            &WorkerExecutionContext::new(),
        );
        let WorkerOutcome::Succeeded { projection } = label_begin else {
            panic!("authoritative label scan begins: {label_begin:?}")
        };
        let WorkerProjection::ProviderSyncPage(label_begin_page) = projection else {
            panic!("Gmail emits a sync page")
        };
        assert_eq!(label_begin_page.batch.container_upserts.len(), 2);
        assert!(label_begin_page
            .batch
            .container_upserts
            .iter()
            .any(|container| container.display_name.as_str() == "Renamed"));
        assert!(label_begin_page
            .reconciliation
            .as_ref()
            .is_some_and(|reconciliation| reconciliation.begin));

        let message_scan = adapter.execute(
            &claim_for_continuation(
                "gmail-account",
                label_begin_page
                    .continuation
                    .expect("empty mailbox scan continuation"),
            ),
            &WorkerExecutionContext::new(),
        );
        let WorkerOutcome::Succeeded { projection } = message_scan else {
            panic!("empty mailbox scan succeeds: {message_scan:?}")
        };
        let WorkerProjection::ProviderSyncPage(message_scan_page) = projection else {
            panic!("Gmail emits a sync page")
        };
        let catch_up = adapter.execute(
            &claim_for_continuation(
                "gmail-account",
                message_scan_page
                    .continuation
                    .expect("history catch-up continuation"),
            ),
            &WorkerExecutionContext::new(),
        );
        let WorkerOutcome::Succeeded { projection } = catch_up else {
            panic!("history catch-up succeeds: {catch_up:?}")
        };
        let WorkerProjection::ProviderSyncPage(catch_up_page) = projection else {
            panic!("Gmail emits a sync page")
        };
        let final_labels = adapter.execute(
            &claim_for_continuation(
                "gmail-account",
                catch_up_page
                    .continuation
                    .expect("final label continuation"),
            ),
            &WorkerExecutionContext::new(),
        );
        let WorkerOutcome::Succeeded { projection } = final_labels else {
            panic!("final labels succeed: {final_labels:?}")
        };
        let WorkerProjection::ProviderSyncPage(final_label_page) = projection else {
            panic!("Gmail emits a sync page")
        };
        assert_eq!(final_label_page.batch.container_upserts.len(), 2);
        assert!(final_label_page
            .batch
            .container_upserts
            .iter()
            .all(|container| container.identity.remote_container_id.as_str() != "Label_delete"));
        assert!(final_label_page
            .reconciliation
            .as_ref()
            .is_some_and(|reconciliation| reconciliation.reset_seen_containers));

        let sweep = adapter.execute(
            &claim_for_continuation(
                "gmail-account",
                final_label_page
                    .continuation
                    .expect("unseen-label sweep continuation"),
            ),
            &WorkerExecutionContext::new(),
        );
        let WorkerOutcome::Succeeded { projection } = sweep else {
            panic!("unseen-label sweep succeeds: {sweep:?}")
        };
        let WorkerProjection::ProviderSyncPage(sweep_page) = projection else {
            panic!("Gmail emits a sync page")
        };
        assert!(sweep_page
            .reconciliation
            .as_ref()
            .is_some_and(|reconciliation| !reconciliation.sweep_kinds.is_empty()));
    }

    #[test]
    fn gmail_provider_conformance_malformed_raw_keeps_bounded_metadata_projection() {
        let mut snapshot = fixture_message("message-bad", "thread-bad", 0);
        snapshot.raw = Some("not-base64url!".into());
        let account = MuxAccountId::new("gmail-account").expect("account identity");
        let projected = project_message(&account, snapshot).expect("metadata projection");
        assert_eq!(projected.message.body_state, ProviderBodyState::Unavailable);
        assert!(projected.message.body_text.as_str().is_empty());
        assert_eq!(
            projected.message.sender_email.as_str(),
            "unknown@example.invalid"
        );
    }

    #[test]
    fn gmail_provider_conformance_large_raw_metadata_fallback_preserves_safe_headers() {
        let mut snapshot = fixture_message("message-large", "thread-large", 0);
        snapshot.raw = None;
        snapshot.metadata_headers = Some(
            build_metadata_headers(&[
                GmailHeaderWire {
                    name: "Subject".into(),
                    value: "Large attachment report".into(),
                },
                GmailHeaderWire {
                    name: "From".into(),
                    value: "Sender <sender@example.test>".into(),
                },
                GmailHeaderWire {
                    name: "To".into(),
                    value: "reader@example.test".into(),
                },
                GmailHeaderWire {
                    name: "Message-ID".into(),
                    value: "<large@example.test>".into(),
                },
            ])
            .expect("bounded metadata headers"),
        );
        let account = MuxAccountId::new("gmail-account").expect("account identity");
        let projected = project_message(&account, snapshot).expect("metadata projection");
        assert_eq!(
            projected.message.subject.as_str(),
            "Large attachment report"
        );
        assert_eq!(
            projected.message.sender_email.as_str(),
            "sender@example.test"
        );
        assert_eq!(
            projected.message.internet_message_id.as_deref(),
            Some("<large@example.test>")
        );
        assert_eq!(projected.message.body_state, ProviderBodyState::Unavailable);
    }

    #[test]
    fn gmail_provider_conformance_draft_is_projected_as_authored_by_local_user() {
        let mut snapshot = fixture_message("draft-message", "draft-thread", 0);
        snapshot.label_ids = vec!["DRAFT".into()];
        let account = MuxAccountId::new("gmail-account").expect("account identity");
        let projected = project_message(&account, snapshot).expect("draft projection");
        assert!(projected.message.is_from_me);
        assert!(projected.thread.has_from_me);
    }

    #[test]
    fn gmail_provider_conformance_body_state_distinguishes_empty_complete_and_truncated() {
        let account = MuxAccountId::new("gmail-account").expect("account identity");
        let make_snapshot = |id: &str, body: &str| {
            GmailMessageSnapshot {
            id: id.into(),
            thread_id: "thread-body-state".into(),
            label_ids: vec!["INBOX".into()],
            snippet: String::new(),
            history_id: "200".into(),
            internal_date_ms: 2_000,
            raw: Some(URL_SAFE_NO_PAD.encode(format!(
                "From: sender@example.test\r\nSubject: Body state\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{body}"
            ))),
            metadata_headers: None,
        }
        };
        let empty = project_message(&account, make_snapshot("empty", "")).expect("empty body");
        assert_eq!(empty.message.body_state, ProviderBodyState::Complete);
        assert!(empty.message.body_text.as_str().is_empty());

        let exact_text = "x".repeat(MAX_BODY_TEXT_BYTES);
        let exact =
            project_message(&account, make_snapshot("exact", &exact_text)).expect("exact body cap");
        assert_eq!(exact.message.body_state, ProviderBodyState::Complete);
        assert_eq!(exact.message.body_text.as_str().len(), MAX_BODY_TEXT_BYTES);

        let over_text = "x".repeat(MAX_BODY_TEXT_BYTES + 1);
        let over =
            project_message(&account, make_snapshot("over", &over_text)).expect("over-cap body");
        assert_eq!(over.message.body_state, ProviderBodyState::Truncated);
        assert_eq!(over.message.body_text.as_str().len(), MAX_BODY_TEXT_BYTES);
    }

    #[test]
    fn gmail_provider_conformance_profile_identity_mismatch_stops_before_mailbox_projection() {
        let mut api = FixtureApi::standard();
        api.profile_email = "different-account@example.test".into();
        let adapter = GmailAdapter::new(OpenAccess, api, || 5_000);
        let work =
            initial_sync_work("gmail-account", "identity-mismatch", 0).expect("initial work");
        assert!(matches!(
            adapter.execute(&claim_for(work), &WorkerExecutionContext::new()),
            WorkerOutcome::AuthenticationExpired { code }
                if code == "gmail_account_mismatch"
        ));
        assert_eq!(adapter.api.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn gmail_provider_conformance_locked_vault_prevents_all_api_calls() {
        let api = FixtureApi::standard();
        let adapter = GmailAdapter::new(UnavailableCredentialAccess, api, || 5_000);
        let work = initial_sync_work("gmail-account", "locked", 0).expect("initial work");
        assert!(matches!(
            adapter.execute(&claim_for(work), &WorkerExecutionContext::new()),
            WorkerOutcome::CredentialUnavailable
        ));
        assert_eq!(adapter.api.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn gmail_provider_conformance_invalid_history_schedules_rescan_without_tombstones() {
        let mut api = FixtureApi::standard();
        api.history_expired = true;
        let adapter = GmailAdapter::new(OpenAccess, api, || 5_000);
        let request = make_work(
            GmailSyncPhase::History {
                committed_history_id: "50".into(),
                committed_label_digest: None,
                page_token: None,
                offset: 0,
                page_digest: None,
                reconciliation_generation: None,
            },
            Some("durable-history-cursor".into()),
        )
        .expect("history work");
        let work = work_item("gmail-account", request, 0).expect("work item");
        let outcome = adapter.execute(&claim_for(work), &WorkerExecutionContext::new());
        let WorkerOutcome::Succeeded { projection } = outcome else {
            panic!("invalid history transitions to rescan")
        };
        let WorkerProjection::ProviderSyncPage(page) = projection else {
            panic!("rescan transition is a sync page")
        };
        assert!(page.batch.message_upserts.is_empty());
        assert!(page.batch.tombstones.is_empty());
        let continuation = page.continuation.expect("rescan continuation");
        let next: GmailSyncWork =
            serde_json::from_str(&continuation.payload_json).expect("rescan payload");
        assert!(matches!(next.phase, GmailSyncPhase::Bootstrap { .. }));
    }

    #[test]
    fn gmail_provider_conformance_history_slices_large_change_sets_without_advancing_early() {
        let mut api = FixtureApi::standard();
        api.history_changes = api
            .messages
            .values()
            .map(|message| GmailHistoryChange {
                message_id: message.id.clone(),
                thread_id: Some(message.thread_id.clone()),
            })
            .collect();
        let adapter = GmailAdapter::new(OpenAccess, api, || 5_000);
        let request = make_work(
            GmailSyncPhase::History {
                committed_history_id: "100".into(),
                committed_label_digest: Some(fixture_label_digest()),
                page_token: None,
                offset: 0,
                page_digest: None,
                reconciliation_generation: None,
            },
            Some("cursor-before-history".into()),
        )
        .expect("history request");
        let work = work_item("gmail-account", request, 0).expect("history work");
        let outcome = adapter.execute(&claim_for(work), &WorkerExecutionContext::new());
        let WorkerOutcome::Succeeded { projection } = outcome else {
            panic!("history slice succeeds: {outcome:?}")
        };
        let WorkerProjection::ProviderSyncPage(page) = projection else {
            panic!("history emits a sync page")
        };
        assert_eq!(page.batch.message_upserts.len(), MESSAGE_PAGE_SIZE);
        assert!(!page.complete);
        let next: GmailSyncWork =
            serde_json::from_str(&page.continuation.expect("slice continuation").payload_json)
                .expect("next slice payload");
        assert!(matches!(
            next.phase,
            GmailSyncPhase::History {
                committed_history_id,
                offset: 10,
                page_digest: Some(_),
                ..
            } if committed_history_id == "100"
        ));
    }

    #[test]
    fn gmail_provider_conformance_moving_history_watermark_does_not_starve_a_bounded_slice() {
        let mut api = FixtureApi::standard();
        api.advancing_history_watermark = true;
        api.history_changes = api
            .messages
            .values()
            .map(|message| GmailHistoryChange {
                message_id: message.id.clone(),
                thread_id: Some(message.thread_id.clone()),
            })
            .collect();
        let adapter = GmailAdapter::new(OpenAccess, api, || 5_000);
        let request = make_work(
            GmailSyncPhase::History {
                committed_history_id: "100".into(),
                committed_label_digest: Some(fixture_label_digest()),
                page_token: None,
                offset: 0,
                page_digest: None,
                reconciliation_generation: None,
            },
            Some("cursor-before-churn".into()),
        )
        .expect("history request");
        let first = adapter.execute(
            &claim_for(work_item("gmail-account", request, 0).expect("history work")),
            &WorkerExecutionContext::new(),
        );
        let WorkerOutcome::Succeeded { projection } = first else {
            panic!("first bounded slice succeeds: {first:?}")
        };
        let WorkerProjection::ProviderSyncPage(first_page) = projection else {
            panic!("history emits a sync page")
        };
        let next: GmailSyncWork = serde_json::from_str(
            &first_page
                .continuation
                .expect("slice continuation")
                .payload_json,
        )
        .expect("continuation payload");
        let second = adapter.execute(
            &claim_for(work_item("gmail-account", next, 0).expect("second slice")),
            &WorkerExecutionContext::new(),
        );
        let WorkerOutcome::Succeeded { projection } = second else {
            panic!("second bounded slice succeeds: {second:?}")
        };
        let WorkerProjection::ProviderSyncPage(second_page) = projection else {
            panic!("history emits a sync page")
        };
        assert_eq!(second_page.batch.message_upserts.len(), 2);
        assert!(second_page.complete);
        assert!(second_page.batch.cursor.value.as_str().contains("121"));
    }

    #[test]
    fn gmail_provider_conformance_changed_history_page_token_restarts_the_bounded_slice() {
        let mut api = FixtureApi::standard();
        api.changing_history_token = true;
        api.history_changes = api
            .messages
            .values()
            .map(|message| GmailHistoryChange {
                message_id: message.id.clone(),
                thread_id: Some(message.thread_id.clone()),
            })
            .collect();
        let adapter = GmailAdapter::new(OpenAccess, api, || 5_000);
        let request = make_work(
            GmailSyncPhase::History {
                committed_history_id: "100".into(),
                committed_label_digest: Some(fixture_label_digest()),
                page_token: None,
                offset: 0,
                page_digest: None,
                reconciliation_generation: None,
            },
            Some("cursor-before-token-change".into()),
        )
        .expect("history request");
        let first = adapter.execute(
            &claim_for(work_item("gmail-account", request, 0).expect("history work")),
            &WorkerExecutionContext::new(),
        );
        let WorkerOutcome::Succeeded { projection } = first else {
            panic!("first bounded slice succeeds: {first:?}")
        };
        let WorkerProjection::ProviderSyncPage(first_page) = projection else {
            panic!("history emits a sync page")
        };
        let second = adapter.execute(
            &claim_for_continuation(
                "gmail-account",
                first_page.continuation.expect("slice continuation"),
            ),
            &WorkerExecutionContext::new(),
        );
        let WorkerOutcome::Succeeded { projection } = second else {
            panic!("changed token restarts safely: {second:?}")
        };
        let WorkerProjection::ProviderSyncPage(second_page) = projection else {
            panic!("history emits a sync page")
        };
        assert!(second_page.batch.message_upserts.is_empty());
        let next: GmailSyncWork = serde_json::from_str(
            &second_page
                .continuation
                .expect("restart continuation")
                .payload_json,
        )
        .expect("restart payload");
        assert!(matches!(
            next.phase,
            GmailSyncPhase::History {
                offset: 0,
                page_digest: None,
                ..
            }
        ));
    }

    #[test]
    fn gmail_provider_conformance_final_history_page_commits_server_history_id() {
        let adapter = GmailAdapter::new(OpenAccess, FixtureApi::standard(), || 5_000);
        let request = make_work(
            GmailSyncPhase::History {
                committed_history_id: "100".into(),
                committed_label_digest: Some(fixture_label_digest()),
                page_token: None,
                offset: 0,
                page_digest: None,
                reconciliation_generation: None,
            },
            Some("cursor-before-history".into()),
        )
        .expect("history request");
        let outcome = adapter.execute(
            &claim_for(work_item("gmail-account", request, 0).expect("history work")),
            &WorkerExecutionContext::new(),
        );
        let WorkerOutcome::Succeeded { projection } = outcome else {
            panic!("final history page succeeds: {outcome:?}")
        };
        let WorkerProjection::ProviderSyncPage(page) = projection else {
            panic!("history emits a sync page")
        };
        assert!(page.complete);
        assert!(page.continuation.is_none());
        assert!(page.batch.cursor.value.as_str().contains("120"));
    }

    #[test]
    fn gmail_provider_conformance_missing_changed_message_becomes_tombstone() {
        let mut api = FixtureApi::standard();
        api.history_changes = vec![GmailHistoryChange {
            message_id: "deleted-message".into(),
            thread_id: Some("thread-a".into()),
        }];
        let adapter = GmailAdapter::new(OpenAccess, api, || 5_000);
        let request = make_work(
            GmailSyncPhase::History {
                committed_history_id: "100".into(),
                committed_label_digest: Some(fixture_label_digest()),
                page_token: None,
                offset: 0,
                page_digest: None,
                reconciliation_generation: None,
            },
            Some("cursor-before-delete".into()),
        )
        .expect("history request");
        let outcome = adapter.execute(
            &claim_for(work_item("gmail-account", request, 0).expect("history work")),
            &WorkerExecutionContext::new(),
        );
        let WorkerOutcome::Succeeded { projection } = outcome else {
            panic!("delete history succeeds: {outcome:?}")
        };
        let WorkerProjection::ProviderSyncPage(page) = projection else {
            panic!("history emits a sync page")
        };
        assert_eq!(page.batch.tombstones.len(), 1);
        assert!(page.batch.message_upserts.is_empty());
        assert!(page.complete);
    }

    #[test]
    fn gmail_provider_conformance_multi_pass_sweep_fences_each_continuation_to_durable_cursor() {
        let request = make_work(
            GmailSyncPhase::Sweep {
                generation_id: "generation-a".into(),
                final_history_id: "120".into(),
                label_digest: Some(fixture_label_digest()),
                pass: 0,
            },
            Some("cursor-before-sweep".into()),
        )
        .expect("sweep request");
        let page = execute_sweep(
            "gmail-account",
            request,
            "generation-a".into(),
            "120".into(),
            Some(fixture_label_digest()),
            0,
            5_000,
        )
        .expect("sweep page");
        let next: GmailSyncWork = serde_json::from_str(
            &page
                .continuation
                .expect("bounded sweep continuation")
                .payload_json,
        )
        .expect("continuation payload");
        assert_eq!(
            next.expected_prior_cursor.as_deref(),
            Some(page.batch.cursor.value.as_str())
        );
    }

    #[test]
    fn gmail_provider_conformance_worker_outcomes_keep_auth_retry_and_rate_limit_distinct() {
        assert!(matches!(
            access_error_outcome(GmailAccessError::CredentialUnavailable),
            WorkerOutcome::CredentialUnavailable
        ));
        assert!(matches!(
            api_error_outcome(GmailApiError::Unauthorized),
            WorkerOutcome::AuthenticationExpired { .. }
        ));
        assert!(matches!(
            api_error_outcome(GmailApiError::Retryable),
            WorkerOutcome::RetryableFailure { .. }
        ));
        assert!(matches!(
            api_error_outcome(GmailApiError::RateLimited { retry_after_at: 42 }),
            WorkerOutcome::RateLimited {
                retry_after_at: 42,
                ..
            }
        ));
        assert!(matches!(
            api_error_outcome(GmailApiError::Permanent),
            WorkerOutcome::PermanentFailure { .. }
        ));
        let envelope = |reason: &str| {
            serde_json::json!({"error":{"errors":[{"reason":reason}]}})
                .to_string()
                .into_bytes()
        };
        assert_eq!(
            classify_forbidden(&envelope("dailyLimitExceeded"), 42),
            GmailApiError::RateLimited { retry_after_at: 42 }
        );
        assert_eq!(
            classify_forbidden(&envelope("insufficientPermissions"), 42),
            GmailApiError::Unauthorized
        );
        assert_eq!(
            classify_forbidden(&envelope("domainPolicy"), 42),
            GmailApiError::Permanent
        );
    }

    #[test]
    fn gmail_provider_conformance_mutations_map_exact_desired_state_deltas() {
        let cases = [
            (
                "archive",
                "in_inbox",
                "0",
                None,
                "modify:remote-thread",
                vec![],
                vec!["INBOX"],
            ),
            (
                "restore",
                "in_inbox",
                "1",
                None,
                "modify:remote-thread",
                vec!["INBOX"],
                vec![],
            ),
            (
                "read",
                "unread",
                "0",
                None,
                "modify:remote-thread",
                vec![],
                vec!["UNREAD"],
            ),
            (
                "unread",
                "unread",
                "1",
                None,
                "modify:remote-thread",
                vec!["UNREAD"],
                vec![],
            ),
            (
                "star",
                "starred",
                "1",
                None,
                "modify:remote-thread",
                vec!["STARRED"],
                vec![],
            ),
            (
                "unstar",
                "starred",
                "0",
                None,
                "modify:remote-thread",
                vec![],
                vec!["STARRED"],
            ),
            (
                "label",
                "provider_label",
                "1",
                Some("Label_1"),
                "modify:remote-thread",
                vec!["Label_1"],
                vec![],
            ),
            (
                "unlabel",
                "provider_label",
                "0",
                Some("Label_1"),
                "modify:remote-thread",
                vec![],
                vec!["Label_1"],
            ),
            (
                "delete",
                "trashed",
                "1",
                None,
                "trash:remote-thread",
                vec![],
                vec![],
            ),
            (
                "untrash",
                "trashed",
                "0",
                None,
                "untrash:remote-thread",
                vec![],
                vec![],
            ),
        ];
        for (operation, field, value, container, expected_call, add, remove) in cases {
            let adapter = GmailAdapter::new(OpenAccess, FixtureApi::standard(), || 5_000);
            let outcome = adapter.execute(
                &mutation_claim(operation, field, value, container),
                &WorkerExecutionContext::new(),
            );
            assert!(matches!(
                outcome,
                WorkerOutcome::Succeeded {
                    projection: WorkerProjection::LocalOperation
                }
            ));
            assert_eq!(
                adapter.api.mutations.lock().unwrap().as_slice(),
                &[(
                    expected_call.into(),
                    add.into_iter().map(str::to_owned).collect(),
                    remove.into_iter().map(str::to_owned).collect(),
                )],
                "wrong provider call for {operation}"
            );
        }
    }

    #[test]
    fn gmail_provider_conformance_mutation_errors_preserve_typed_worker_outcomes() {
        let locked = GmailAdapter::new(UnavailableCredentialAccess, FixtureApi::standard(), || {
            5_000
        });
        assert!(matches!(
            locked.execute(
                &mutation_claim("locked", "starred", "1", None),
                &WorkerExecutionContext::new()
            ),
            WorkerOutcome::CredentialUnavailable
        ));
        assert_eq!(locked.api.calls.load(Ordering::SeqCst), 0);

        for (error, expected) in [
            (GmailApiError::Unauthorized, "auth"),
            (GmailApiError::Retryable, "retry"),
            (
                GmailApiError::RateLimited {
                    retry_after_at: 9_000,
                },
                "rate",
            ),
            (GmailApiError::Permanent, "permanent"),
            (GmailApiError::NotFound, "absent"),
        ] {
            let api = FixtureApi {
                mutation_error: Some(error),
                ..FixtureApi::standard()
            };
            let adapter = GmailAdapter::new(OpenAccess, api, || 5_000);
            let outcome = adapter.execute(
                &mutation_claim(expected, "starred", "1", None),
                &WorkerExecutionContext::new(),
            );
            match expected {
                "auth" => assert!(matches!(
                    outcome,
                    WorkerOutcome::AuthenticationExpired { .. }
                )),
                "retry" => assert!(matches!(outcome, WorkerOutcome::RetryableFailure { .. })),
                "rate" => assert!(matches!(
                    outcome,
                    WorkerOutcome::RateLimited {
                        retry_after_at: 9_000,
                        ..
                    }
                )),
                "permanent" => assert!(matches!(outcome, WorkerOutcome::PermanentFailure { .. })),
                "absent" => assert!(matches!(
                    outcome,
                    WorkerOutcome::Succeeded {
                        projection: WorkerProjection::RemoteThreadAbsent { ref remote_thread_id }
                    } if remote_thread_id == "remote-thread"
                )),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn gmail_provider_conformance_mutation_404_requires_confirmed_thread_absence() {
        let api = FixtureApi {
            mutation_error: Some(GmailApiError::NotFound),
            mutation_thread_exists_after_not_found: true,
            ..FixtureApi::standard()
        };
        let adapter = GmailAdapter::new(OpenAccess, api, || 5_000);
        assert!(matches!(
            adapter.execute(
                &mutation_claim("stale-label", "provider_label", "1", Some("Label_deleted")),
                &WorkerExecutionContext::new(),
            ),
            WorkerOutcome::PermanentFailure { ref code }
                if code == "gmail_mutation_target_stale"
        ));
        assert_eq!(
            adapter.api.calls.load(Ordering::SeqCst),
            3,
            "profile, failed mutation, and bounded existence check are the only calls"
        );

        for (error, expected) in [
            (GmailApiError::Unauthorized, "auth"),
            (GmailApiError::Retryable, "retry"),
            (
                GmailApiError::RateLimited {
                    retry_after_at: 9_000,
                },
                "rate",
            ),
            (GmailApiError::Permanent, "permanent"),
        ] {
            let api = FixtureApi {
                mutation_error: Some(GmailApiError::NotFound),
                mutation_thread_exists_error: Some(error),
                ..FixtureApi::standard()
            };
            let adapter = GmailAdapter::new(OpenAccess, api, || 5_000);
            let outcome = adapter.execute(
                &mutation_claim(expected, "starred", "1", None),
                &WorkerExecutionContext::new(),
            );
            match expected {
                "auth" => assert!(matches!(
                    outcome,
                    WorkerOutcome::AuthenticationExpired { .. }
                )),
                "retry" => assert!(matches!(outcome, WorkerOutcome::RetryableFailure { .. })),
                "rate" => assert!(matches!(
                    outcome,
                    WorkerOutcome::RateLimited {
                        retry_after_at: 9_000,
                        ..
                    }
                )),
                "permanent" => {
                    assert!(matches!(outcome, WorkerOutcome::PermanentFailure { .. }))
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn gmail_provider_conformance_unknown_system_labels_are_not_selectable_custom_labels() {
        let projected = project_label(
            &MuxAccountId::new("gmail-account").unwrap(),
            &GmailLabel {
                id: "CATEGORY_FORUMS".into(),
                name: "Forums".into(),
                system: true,
            },
        )
        .unwrap();
        assert!(!projected.selectable);
        assert_eq!(projected.role, None);
    }

    fn send_payload_v3(provider_kind: &str, remote_thread_id: Option<&str>) -> String {
        let fingerprint = crate::store::send_content_fingerprint_v3(
            "<send-op@mux.invalid>",
            "gmail-account",
            "subject-1@example.test",
            "recipient@example.test",
            "",
            "",
            "Exact Gmail send",
            "Body",
            "<p>Body</p>",
            None,
            1,
            1_000,
            None,
            &[],
            "mux-send-op",
            provider_kind,
            remote_thread_id,
        );
        serde_json::to_string(&serde_json::json!({
            "draftId": "draft-send",
            "messageId": "local-message",
            "snapshotVersion": 3,
            "submissionMessageId": "<send-op@mux.invalid>",
            "accountId": "gmail-account",
            "senderEmail": "subject-1@example.test",
            "recipients": "recipient@example.test",
            "ccRecipients": "",
            "bccRecipients": "",
            "subject": "Exact Gmail send",
            "body": "Body",
            "bodyHtml": "<p>Body</p>",
            "replyToThreadId": null,
            "draftRevision": 1,
            "contentFingerprintHex": fingerprint,
            "queuedAtMs": 1_000,
            "inReplyTo": null,
            "references": [],
            "clientCorrelationId": "mux-send-op",
            "providerKind": provider_kind,
            "remoteThreadId": remote_thread_id
        }))
        .unwrap()
    }

    fn durable_send_claim(path: &Path, payload_json: &str) -> (DurableWorker, ClaimedWork) {
        drop(MuxStore::open(path, false).expect("send fixture schema"));
        let mut connection = Connection::open(path).unwrap();
        let transaction = connection.transaction().unwrap();
        transaction
            .execute(
                "INSERT INTO accounts(id, name, email, color, provider)
                 VALUES('gmail-account', 'Gmail', 'subject-1@example.test', '#000', 'gmail')",
                [],
            )
            .unwrap();
        transaction
            .execute(
                "INSERT INTO provider_accounts(
                   account_id, provider_kind, remote_account_id, auth_state,
                   credential_ref, sync_state, created_at, updated_at
                 ) VALUES(
                   'gmail-account', 'gmail', 'subject-1', 'ready',
                   'gmail/fixture', 'idle', 0, 0
                 )",
                [],
            )
            .unwrap();
        transaction
            .execute(
                "INSERT INTO drafts(
                   id, account_id, recipients, cc_recipients, bcc_recipients,
                   subject, body, body_html, updated_at, revision
                 ) VALUES(
                   'draft-send', 'gmail-account', 'recipient@example.test', '', '',
                   'Exact Gmail send', 'Body', '<p>Body</p>', 1_000, 1
                 )",
                [],
            )
            .unwrap();
        transaction
            .execute(
                "INSERT INTO operations(
                   id, field, kind, old_value, new_value, payload_json,
                   state, created_at, not_before
                 ) VALUES(
                   'send-op', 'send', 'send', 'draft', 'submitted', ?1,
                   'pending', 1_000, 1_000
                 )",
                [payload_json],
            )
            .unwrap();
        enqueue_in_transaction(
            &transaction,
            NewWorkItem {
                id: "send-work".into(),
                account_id: "gmail-account".into(),
                operation_id: Some("send-op".into()),
                kind: WorkKind::Send,
                scope: gmail_send_scope("send-op"),
                ordering_key: "<send-op@mux.invalid>".into(),
                payload_json: payload_json.into(),
                priority: 100,
                available_at: 1_000,
                max_attempts: 8,
            },
            1_000,
        )
        .unwrap();
        let scope = gmail_send_scope("send-op");
        if let Ok(reconciliation) = gmail_send_reconciliation_work(
            "gmail-account",
            "send-op",
            "send-work",
            &scope,
            payload_json,
            3_000,
        ) {
            enqueue_in_transaction(&transaction, reconciliation, 1_001).unwrap();
        }
        transaction.commit().unwrap();
        let worker = DurableWorker::new(path, WorkerConfig::default()).unwrap();
        let claim = worker
            .claim_available(2_000, "gmail-send-test", 1)
            .unwrap()
            .pop()
            .expect("send claim");
        (worker, claim)
    }

    fn acknowledge_success(
        worker: &DurableWorker,
        claim: &ClaimedWork,
        outcome: WorkerOutcome,
        now_ms: i64,
    ) -> Result<WorkState, WorkerError> {
        let WorkerOutcome::Succeeded { projection } = outcome else {
            panic!("test expected provider success");
        };
        worker.acknowledge_success_with_projection(claim, now_ms, |transaction| {
            crate::provider_conformance::apply_worker_projection(transaction, claim, &projection)
        })
    }

    #[test]
    fn gmail_provider_conformance_send_uses_exact_v3_base64url_snapshot_and_marker() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("gmail-send.db");
        let (worker, claim) =
            durable_send_claim(&path, &send_payload_v3("gmail", Some("frozen-thread")));
        let api = FixtureApi {
            send_acknowledgement: GmailSendAcknowledgement {
                message_id: "remote-message".into(),
                thread_id: "frozen-thread".into(),
            },
            ..FixtureApi::standard()
        };
        let adapter = GmailAdapter::new(OpenAccess, api, || 2_100).with_database_path(&path);
        let outcome = adapter.execute(&claim, &WorkerExecutionContext::new());
        let submissions = adapter.api.submissions.lock().unwrap();
        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].1.as_deref(), Some("frozen-thread"));
        assert!(!submissions[0].0.contains('='));
        let raw = URL_SAFE_NO_PAD.decode(&submissions[0].0).unwrap();
        let raw = String::from_utf8(raw).unwrap();
        assert!(raw.contains("Message-ID: <send-op@mux.invalid>\r\n"));
        assert!(raw.contains("X-Mux-Client-Correlation: mux-send-op\r\n"));
        let marker: String = Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT last_error_code FROM provider_work_items WHERE id = 'send-work'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(marker, "send_submission_started");
        assert_eq!(
            acknowledge_success(&worker, &claim, outcome, 2_200).unwrap(),
            WorkState::Succeeded
        );
        let connection = Connection::open(&path).unwrap();
        let states: (String, String, String, i64, i64, i64) = connection
            .query_row(
                "SELECT send.state, operation.state, reconciliation.state,
                        (SELECT COUNT(*) FROM drafts WHERE id = 'draft-send'),
                        (SELECT COUNT(*) FROM provider_message_refs
                         WHERE account_id = 'gmail-account'
                           AND remote_message_id = 'remote-message'),
                        (SELECT COUNT(*) FROM provider_thread_refs
                         WHERE account_id = 'gmail-account'
                           AND remote_thread_id = 'frozen-thread')
                 FROM provider_work_items send
                 JOIN operations operation ON operation.id = send.operation_id
                 JOIN provider_work_items reconciliation
                   ON reconciliation.id = 'reconcile_send-work'
                 WHERE send.id = 'send-work'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            states,
            (
                "succeeded".into(),
                "confirmed".into(),
                "cancelled".into(),
                0,
                1,
                1
            )
        );
    }

    #[test]
    fn gmail_provider_conformance_v2_target_smuggling_and_provider_mismatch_never_send() {
        let directory = tempdir().unwrap();
        let mut legacy: serde_json::Value =
            serde_json::from_str(&send_payload_v3("gmail", Some("crafted-thread"))).unwrap();
        legacy["snapshotVersion"] = 2.into();
        assert!(crate::outgoing::prepare_from_durable_payload(&legacy.to_string(), &[]).is_err());

        let other_path = directory.path().join("gmail-send-provider-mismatch.db");
        let (_worker, claim) = durable_send_claim(&other_path, &send_payload_v3("imap", None));
        let adapter = GmailAdapter::new(OpenAccess, FixtureApi::standard(), || 2_100)
            .with_database_path(&other_path);
        assert!(matches!(
            adapter.execute(&claim, &WorkerExecutionContext::new()),
            WorkerOutcome::PermanentFailure { .. }
        ));
        assert_eq!(adapter.api.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn gmail_provider_conformance_send_requires_modify_authority_before_submission_marker() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("gmail-send-modify-authority.db");
        let (_worker, claim) = durable_send_claim(&path, &send_payload_v3("gmail", None));
        let adapter = GmailAdapter::new(ReadOnlyAccess, FixtureApi::standard(), || 2_100)
            .with_database_path(&path);
        assert!(matches!(
            adapter.execute(&claim, &WorkerExecutionContext::new()),
            WorkerOutcome::AuthenticationExpired { ref code }
                if code == "gmail_reauthorization_required"
        ));
        assert_eq!(adapter.api.calls.load(Ordering::SeqCst), 0);
        let marker: Option<String> = Connection::open(path)
            .unwrap()
            .query_row(
                "SELECT last_error_code FROM provider_work_items WHERE id = 'send-work'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(marker, None);
    }

    fn reconciliation_fixture(
        path: &Path,
        page: GmailSendEvidencePage,
    ) -> (DurableWorker, ClaimedWork, WorkerOutcome) {
        let (worker, send_claim) = durable_send_claim(path, &send_payload_v3("gmail", None));
        let ambiguous_api = FixtureApi {
            send_error: Some(GmailApiError::AmbiguousSubmission),
            ..FixtureApi::standard()
        };
        let adapter =
            GmailAdapter::new(OpenAccess, ambiguous_api, || 2_100).with_database_path(path);
        let ambiguous = adapter.execute(&send_claim, &WorkerExecutionContext::new());
        assert!(matches!(ambiguous, WorkerOutcome::OutcomeUnknown { .. }));
        worker.acknowledge(&send_claim, ambiguous, 2_200).unwrap();
        let reconciliation = worker
            .claim_available(5_000, "gmail-reconciliation-test", 1)
            .unwrap()
            .pop()
            .expect("dormant reconciliation activates only after uncertainty");
        assert!(is_gmail_send_reconciliation_work(&reconciliation));
        let api = FixtureApi {
            send_evidence: page,
            ..FixtureApi::standard()
        };
        let outcome = GmailAdapter::new(OpenAccess, api, || 5_100)
            .with_database_path(path)
            .execute(&reconciliation, &WorkerExecutionContext::new());
        (worker, reconciliation, outcome)
    }

    fn reconciliation_outcome(page: GmailSendEvidencePage) -> WorkerOutcome {
        let directory = tempdir().unwrap();
        let path = directory.path().join("gmail-send-reconcile.db");
        reconciliation_fixture(&path, page).2
    }

    fn exact_send_evidence(message_id: &str, accepted_at_ms: i64) -> GmailSendEvidence {
        GmailSendEvidence {
            message_id: message_id.into(),
            thread_id: "remote-thread".into(),
            accepted_at_ms,
            observed_message_id: "<send-op@mux.invalid>".into(),
            observed_client_correlation: "mux-send-op".into(),
        }
    }

    fn exact_direct_acceptance(claim: &ClaimedWork) -> ProviderSendAcceptance {
        ProviderSendAcceptance {
            provider_kind: "gmail".into(),
            original_work_id: claim.id.clone(),
            operation_id: claim.operation_id.clone().expect("send operation"),
            send_payload_fingerprint_hex: claim.payload_fingerprint_hex.clone(),
            remote_message_id: "remote-message".into(),
            remote_thread_id: "remote-thread".into(),
            accepted_at_ms: 2_100,
            reconciliation_work_id: None,
        }
    }

    fn assert_send_projection_rolled_back(path: &Path) {
        let connection = Connection::open(path).unwrap();
        let durable: (i64, i64, i64) = connection
            .query_row(
                "SELECT
                   (SELECT COUNT(*) FROM drafts WHERE id = 'draft-send'),
                   (SELECT COUNT(*) FROM messages
                    WHERE internet_message_id = '<send-op@mux.invalid>'),
                   (SELECT COUNT(*) FROM provider_message_refs
                    WHERE account_id = 'gmail-account'
                      AND remote_message_id = 'remote-message')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(durable, (1, 0, 0));
    }

    #[test]
    fn gmail_provider_conformance_forged_acceptances_roll_back_atomically() {
        for name in ["provider", "fingerprint", "timestamp"] {
            let directory = tempdir().unwrap();
            let path = directory.path().join(format!("gmail-forged-{name}.db"));
            let (worker, claim) = durable_send_claim(&path, &send_payload_v3("gmail", None));
            let mut acceptance = exact_direct_acceptance(&claim);
            match name {
                "provider" => acceptance.provider_kind = "imap".into(),
                "fingerprint" => acceptance.send_payload_fingerprint_hex = "11".repeat(32),
                "timestamp" => {
                    acceptance.accepted_at_ms =
                        crate::outgoing::MAX_RFC3339_UNIX_MILLIS.saturating_add(1)
                }
                _ => unreachable!(),
            }
            let result = acknowledge_success(
                &worker,
                &claim,
                WorkerOutcome::Succeeded {
                    projection: WorkerProjection::ProviderSendAcceptance(Box::new(acceptance)),
                },
                2_200,
            );
            assert!(matches!(result, Err(WorkerError::Conflict(_))), "{name}");
            assert_send_projection_rolled_back(&path);
        }

        let directory = tempdir().unwrap();
        let path = directory.path().join("gmail-forged-account.db");
        let (worker, claim) = durable_send_claim(&path, &send_payload_v3("gmail", None));
        let acceptance = exact_direct_acceptance(&claim);
        let mut forged_claim = claim.clone();
        forged_claim.account_id = "another-account".into();
        let result = acknowledge_success(
            &worker,
            &forged_claim,
            WorkerOutcome::Succeeded {
                projection: WorkerProjection::ProviderSendAcceptance(Box::new(acceptance)),
            },
            2_200,
        );
        assert!(matches!(result, Err(WorkerError::Conflict(_))));
        assert_send_projection_rolled_back(&path);
    }

    #[test]
    fn gmail_provider_conformance_collision_and_changed_draft_never_partially_project() {
        let directory = tempdir().unwrap();
        let collision_path = directory.path().join("gmail-provider-ref-collision.db");
        let (worker, claim) = durable_send_claim(&collision_path, &send_payload_v3("gmail", None));
        let connection = Connection::open(&collision_path).unwrap();
        connection
            .execute(
                "INSERT INTO threads(
                   id, account_id, subject, participants, snippet, latest_at,
                   message_count, remote_in_inbox, remote_unread, remote_starred,
                   has_attachment, has_invite, has_link, has_from_me
                 ) VALUES(900, 'gmail-account', 'Other', '', '', 1, 0, 0, 0, 0, 0, 0, 0, 0)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO provider_thread_refs(
                   account_id, remote_thread_id, thread_id, revision
                 ) VALUES('gmail-account', 'remote-thread', 900, NULL)",
                [],
            )
            .unwrap();
        drop(connection);
        let result = acknowledge_success(
            &worker,
            &claim,
            WorkerOutcome::Succeeded {
                projection: WorkerProjection::ProviderSendAcceptance(Box::new(
                    exact_direct_acceptance(&claim),
                )),
            },
            2_200,
        );
        assert!(matches!(result, Err(WorkerError::Conflict(_))));
        assert_send_projection_rolled_back(&collision_path);
        assert_eq!(
            Connection::open(&collision_path)
                .unwrap()
                .query_row("SELECT COUNT(*) FROM threads", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1,
            "the newly projected sent thread must roll back"
        );

        let changed_path = directory.path().join("gmail-changed-draft.db");
        let (worker, claim) = durable_send_claim(&changed_path, &send_payload_v3("gmail", None));
        Connection::open(&changed_path)
            .unwrap()
            .execute(
                "UPDATE drafts SET revision = 2, body = 'changed' WHERE id = 'draft-send'",
                [],
            )
            .unwrap();
        let result = acknowledge_success(
            &worker,
            &claim,
            WorkerOutcome::Succeeded {
                projection: WorkerProjection::ProviderSendAcceptance(Box::new(
                    exact_direct_acceptance(&claim),
                )),
            },
            2_200,
        );
        assert!(matches!(result, Err(WorkerError::Conflict(_))));
        assert_send_projection_rolled_back(&changed_path);
        assert_eq!(
            Connection::open(&changed_path)
                .unwrap()
                .query_row(
                    "SELECT revision FROM drafts WHERE id = 'draft-send'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            2
        );
    }

    #[test]
    fn gmail_provider_conformance_reconciliation_cannot_confirm_wrong_send_state() {
        for (send_state, operation_state) in [
            ("queued", "pending"),
            ("retry_wait", "retrying"),
            ("executing", "executing"),
            ("failed", "failed"),
            ("cancelled", "cancelled"),
        ] {
            let directory = tempdir().unwrap();
            let path = directory
                .path()
                .join(format!("gmail-reconcile-wrong-state-{send_state}.db"));
            let (worker, reconciliation, outcome) = reconciliation_fixture(
                &path,
                GmailSendEvidencePage {
                    exact: vec![exact_send_evidence("remote-one", 2_000)],
                    mismatched_candidates: 0,
                    has_more: false,
                },
            );
            let connection = Connection::open(&path).unwrap();
            connection
                .execute(
                    "UPDATE provider_work_items
                     SET state = ?1,
                         completed_at = CASE WHEN ?1 IN ('failed', 'cancelled') THEN 5_100 ELSE NULL END,
                         lease_owner = CASE WHEN ?1 = 'executing' THEN 'other-worker' ELSE NULL END,
                         lease_token = CASE WHEN ?1 = 'executing' THEN 'other-lease' ELSE NULL END,
                         lease_expires_at = CASE WHEN ?1 = 'executing' THEN 99_999 ELSE NULL END
                     WHERE id = 'send-work'",
                    [send_state],
                )
                .unwrap();
            connection
                .execute(
                    "UPDATE operations SET state = ?1 WHERE id = 'send-op'",
                    [operation_state],
                )
                .unwrap();
            drop(connection);
            let result = acknowledge_success(&worker, &reconciliation, outcome, 5_200);
            assert!(
                matches!(result, Err(WorkerError::Conflict(_))),
                "reconciliation accepted {send_state}/{operation_state}"
            );
            assert_send_projection_rolled_back(&path);
        }
    }

    #[test]
    fn gmail_provider_conformance_reconciliation_never_accepts_partial_or_ambiguous_evidence() {
        let cases = [
            (
                GmailSendEvidencePage {
                    exact: Vec::new(),
                    mismatched_candidates: 0,
                    has_more: false,
                },
                "pending",
            ),
            (
                GmailSendEvidencePage {
                    exact: vec![exact_send_evidence("remote-one", 2_000)],
                    mismatched_candidates: 0,
                    has_more: true,
                },
                "truncated",
            ),
            (
                GmailSendEvidencePage {
                    exact: vec![exact_send_evidence("remote-one", 2_000)],
                    mismatched_candidates: 1,
                    has_more: false,
                },
                "mismatch",
            ),
            (
                GmailSendEvidencePage {
                    exact: vec![
                        exact_send_evidence("remote-one", 2_000),
                        exact_send_evidence("remote-two", 2_000),
                    ],
                    mismatched_candidates: 0,
                    has_more: false,
                },
                "duplicate",
            ),
            (
                GmailSendEvidencePage {
                    exact: vec![exact_send_evidence("remote-one", 999)],
                    mismatched_candidates: 0,
                    has_more: false,
                },
                "identity",
            ),
            (
                GmailSendEvidencePage {
                    exact: vec![exact_send_evidence(
                        "remote-one",
                        5_100 + MAX_SEND_EVIDENCE_FUTURE_SKEW_MS + 1,
                    )],
                    mismatched_candidates: 0,
                    has_more: false,
                },
                "identity",
            ),
        ];
        for (page, expected) in cases {
            let outcome = reconciliation_outcome(page);
            match expected {
                "pending" => assert!(matches!(
                    outcome,
                    WorkerOutcome::RetryableFailure { ref code }
                        if code == "gmail_send_evidence_pending"
                )),
                "truncated" => assert!(matches!(
                    outcome,
                    WorkerOutcome::PermanentFailure { ref code }
                        if code == "gmail_send_evidence_truncated"
                )),
                "mismatch" => assert!(matches!(
                    outcome,
                    WorkerOutcome::PermanentFailure { ref code }
                        if code == "gmail_send_evidence_mismatch"
                )),
                "duplicate" => assert!(matches!(
                    outcome,
                    WorkerOutcome::PermanentFailure { ref code }
                        if code == "gmail_send_evidence_duplicate"
                )),
                "identity" => assert!(matches!(
                    outcome,
                    WorkerOutcome::PermanentFailure { ref code }
                        if code == "gmail_send_evidence_identity_mismatch"
                )),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn gmail_provider_conformance_one_exact_complete_evidence_resolves_without_resubmission() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("gmail-send-reconcile-success.db");
        let (worker, reconciliation, outcome) = reconciliation_fixture(
            &path,
            GmailSendEvidencePage {
                exact: vec![exact_send_evidence("remote-one", 2_000)],
                mismatched_candidates: 0,
                has_more: false,
            },
        );
        assert!(matches!(
            &outcome,
            WorkerOutcome::Succeeded {
                projection: WorkerProjection::ProviderSendAcceptance(acceptance)
            } if acceptance.reconciliation_work_id.is_some()
                && acceptance.remote_message_id == "remote-one"
        ));
        assert_eq!(
            acknowledge_success(&worker, &reconciliation, outcome, 5_200).unwrap(),
            WorkState::Succeeded
        );
        let connection = Connection::open(path).unwrap();
        let resolved: (String, String, String, i64, i64) = connection
            .query_row(
                "SELECT original.state, operation.state, reconciliation.state,
                        (SELECT COUNT(*) FROM drafts WHERE id = 'draft-send'),
                        (SELECT COUNT(*) FROM provider_message_refs
                         WHERE account_id = 'gmail-account'
                           AND remote_message_id = 'remote-one')
                 FROM provider_work_items original
                 JOIN operations operation ON operation.id = original.operation_id
                 JOIN provider_work_items reconciliation
                   ON reconciliation.id = 'reconcile_send-work'
                 WHERE original.id = 'send-work'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            resolved,
            (
                "succeeded".into(),
                "confirmed".into(),
                "succeeded".into(),
                0,
                1
            )
        );
    }
}
