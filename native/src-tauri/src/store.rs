use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use rand_core::{OsRng, RngCore};
use rusqlite::{
    params, params_from_iter, types::Value, Connection, OptionalExtension, Transaction,
    TransactionBehavior,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::ipc_boundary::{
    canonical_scope, decode_cursor, encode_cursor, ensure_serialized_budget, CursorKind,
    CursorPosition, IpcBoundaryError, IpcPayloadKind,
};
use crate::provider::{ProviderBatch, ProviderBatchApplyResult};
use crate::worker::{
    enqueue_in_transaction, ClaimedWork, NewWorkItem, ProviderSendAcceptance,
    ThreadMutationPayload, WorkKind, WorkerError, WorkerProjection,
};

pub mod benchmark;

const SCHEMA_VERSION: i64 = 23;
/// A person cannot hide more accounts than they can configure.
const MAX_HIDDEN_ACCOUNTS: usize = 64;
/// Default provider refresh cadence: once a minute.
pub(crate) const DEFAULT_REFRESH_SECONDS: i64 = 60;
/// Bounds keep a mistyped cadence from hammering a provider or stalling mail.
pub(crate) const MIN_REFRESH_SECONDS: i64 = 15;
pub(crate) const MAX_REFRESH_SECONDS: i64 = 24 * 60 * 60;
const MAX_ACTIVITY_ROWS: i64 = 100;
/// The bootstrap payload has a size budget, and an account can keep a great
/// many labels. Past this the sidebar would be unreadable anyway.
const MAX_CONTAINERS: i64 = 200;
/// Roles the eight fixed views already stand for. A folder holding one of them
/// is not a separate place to go, it is the place the sidebar already lists.
const FIXED_VIEW_ROLES: [&str; 7] = [
    "inbox", "archive", "all_mail", "drafts", "sent", "trash", "starred",
];
const MAX_SNOOZE_DISTANCE_MS: i64 = 10 * 366 * 86_400_000;
const MAX_RECIPIENT_HEADER_CHARS: usize = 10_000;
const MAX_RECIPIENT_MAILBOXES: usize = 500;
const MAX_PLAIN_DRAFT_CHARS: usize = 50_000;
const MAX_FORMATTED_DRAFT_CHARS: usize = 200_000;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("Invalid database schema version: {0}")]
    InvalidSchemaVersion(String),
    #[error("Database schema version {found} is newer than this Mux build supports ({supported})")]
    FutureSchema { found: i64, supported: i64 },
    #[error("{0}")]
    Validation(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSummary {
    id: String,
    name: String,
    email: String,
    color: String,
    signature: String,
    unread: i64,
    total: i64,
    /// How often Mux asks the provider for new mail, in seconds.
    refresh_seconds: i64,
    /// When a sync last finished, and how it went. Null throughout for an
    /// account with no provider attached, which is every account in a local-only
    /// mailbox.
    last_sync_at: Option<i64>,
    sync_state: Option<String>,
    last_error_code: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSummary {
    id: i64,
    account_id: String,
    subject: String,
    participants: String,
    snippet: String,
    latest_at: i64,
    message_count: i64,
    in_inbox: bool,
    unread: bool,
    starred: bool,
    has_from_me: bool,
    category: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageSummary {
    id: i64,
    thread_id: i64,
    sender_name: String,
    sender_email: String,
    recipients: String,
    cc_recipients: String,
    bcc_recipients: String,
    sent_at: i64,
    body_text: String,
    body_html: String,
    blocked_remote_resources: i64,
    remote_images: Vec<RemoteImageSummary>,
    is_from_me: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteImageSummary {
    id: i64,
    domain: String,
    alt_text: String,
    allowed_by_policy: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct StoredRemoteImage {
    pub message_id: i64,
    pub resource_id: i64,
    pub url: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FullResyncRequest {
    pub accounts_reset: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentSummary {
    id: String,
    message_id: i64,
    filename: String,
    media_type: String,
    byte_length: i64,
    content_id: String,
    disposition: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentContent {
    filename: String,
    media_type: String,
    byte_length: i64,
    data_base64: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftSummary {
    id: String,
    account_id: String,
    account_name: String,
    account_email: String,
    account_color: String,
    recipients: String,
    cc_recipients: String,
    bcc_recipients: String,
    subject: String,
    body: String,
    body_html: String,
    reply_to_thread_id: Option<i64>,
    updated_at: i64,
    revision: i64,
    locked: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DraftHeaderSummary {
    id: String,
    account_id: String,
    account_name: String,
    account_email: String,
    account_color: String,
    recipients: String,
    cc_recipients: String,
    bcc_recipients: String,
    subject: String,
    reply_to_thread_id: Option<i64>,
    updated_at: i64,
    revision: i64,
    locked: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveDraftInput {
    pub id: Option<String>,
    pub account_id: String,
    #[serde(default)]
    pub recipients: String,
    #[serde(default)]
    pub cc_recipients: String,
    #[serde(default)]
    pub bcc_recipients: String,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub body_html: String,
    pub reply_to_thread_id: Option<i64>,
    pub expected_revision: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationSummary {
    id: String,
    thread_id: Option<i64>,
    field: String,
    kind: String,
    state: String,
    not_before: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationActivitySummary {
    id: String,
    thread_id: Option<i64>,
    field: String,
    kind: String,
    state: String,
    created_at: i64,
    not_before: i64,
    confirmed_at: Option<i64>,
    undo_of: Option<String>,
    attempts: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListOperationsInput {
    limit: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessResult {
    changed: bool,
    confirmed: Vec<String>,
    failed: Vec<String>,
}

#[derive(Debug, Clone)]
struct OperationRecord {
    row_id: i64,
    id: String,
    thread_id: Option<i64>,
    field: String,
    kind: String,
    old_value: Option<String>,
    new_value: Option<String>,
    payload_json: Option<String>,
    state: String,
    not_before: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SendPayload {
    draft_id: String,
    message_id: String,
    #[serde(default)]
    snapshot_version: Option<u8>,
    #[serde(default)]
    submission_message_id: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    sender_email: Option<String>,
    #[serde(default)]
    recipients: Option<String>,
    #[serde(default)]
    cc_recipients: Option<String>,
    #[serde(default)]
    bcc_recipients: Option<String>,
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    body_html: Option<String>,
    #[serde(default)]
    reply_to_thread_id: Option<i64>,
    #[serde(default)]
    draft_revision: Option<i64>,
    #[serde(default)]
    content_fingerprint_hex: Option<String>,
    #[serde(default)]
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

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MailboxBootstrap {
    schema_version: i64,
    accounts: Vec<AccountSummary>,
    view_counts: Vec<ViewCountSummary>,
    drafts: Vec<DraftHeaderSummary>,
    containers: Vec<ContainerSummary>,
}

/// One of the account's own folders or labels. The mailbox's eight fixed views
/// cover the roles every provider has; this is everything else the account
/// keeps, which until now was synced and then never shown.
///
/// The pair (accountId, remoteId) names it. The interface treats both as opaque
/// strings it was handed and echoes back, so nothing about Gmail label ids or
/// IMAP folder paths leaks into how the interface works.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerSummary {
    account_id: String,
    remote_id: String,
    name: String,
    kind: String,
    role: String,
    unread: i64,
    total: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewCountSummary {
    account_id: Option<String>,
    inbox: i64,
    archive: i64,
    starred: i64,
    sent: i64,
    all: i64,
    snoozed: i64,
    trash: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InvitationSummary {
    thread_id: i64,
    uid: String,
    title: String,
    start_at: i64,
    end_at: i64,
    timezone: String,
    location: String,
    organizer: String,
    attendees: String,
    response: String,
    conflict_text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadPageInput {
    account_id: Option<String>,
    view: Option<String>,
    cursor: Option<String>,
    limit: Option<i64>,
    /// Accounts the person has hidden from the list. They keep syncing.
    #[serde(default)]
    hidden_account_ids: Vec<String>,
    /// One of the account's own folders or labels, named by the remote id the
    /// interface was handed. Only meaningful alongside the account it belongs
    /// to, so it is refused without one.
    #[serde(default)]
    container_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadPage {
    threads: Vec<ThreadSummary>,
    has_more: bool,
    next_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadLookupInput {
    thread_id: i64,
    account_id: Option<String>,
    view: Option<String>,
    query: Option<String>,
    #[serde(default)]
    timezone_offset_minutes: i32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagePageInput {
    thread_id: i64,
    cursor: Option<String>,
    limit: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MessagePage {
    messages: Vec<MessageSummary>,
    attachments: Vec<AttachmentSummary>,
    invitation: Option<InvitationSummary>,
    has_more: bool,
    next_cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchInput {
    query: String,
    account_id: Option<String>,
    view: Option<String>,
    cursor: Option<String>,
    limit: Option<i64>,
    #[serde(default)]
    timezone_offset_minutes: i32,
    /// Accounts the person has hidden from the list. They keep syncing.
    #[serde(default)]
    hidden_account_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchPage {
    rows: Vec<ThreadSummary>,
    has_more: bool,
    next_cursor: Option<String>,
}

pub struct MuxStore {
    connection: Connection,
    cursor_signing_key: [u8; 32],
}

impl From<IpcBoundaryError> for StoreError {
    fn from(error: IpcBoundaryError) -> Self {
        Self::Validation(error.to_string())
    }
}

impl MuxStore {
    pub fn open(path: &Path, seed_demo: bool) -> Result<Self, StoreError> {
        let mut connection = Connection::open(path)?;
        migrate(&mut connection)?;
        recover_interrupted_operations(&connection)?;
        if seed_demo {
            seed_demo_mailbox(&mut connection)?;
            seed_demo_account_signatures(&connection)?;
            seed_long_demo_threads(&mut connection)?;
            seed_safe_content_demo(&mut connection)?;
            seed_demo_threading_headers(&connection)?;
        }
        ensure_durable_work_for_pending_operations(&mut connection)?;
        let cursor_signing_key = load_or_create_cursor_signing_key(&connection)?;
        Ok(Self {
            connection,
            cursor_signing_key,
        })
    }

    pub fn apply_provider_batch(
        &mut self,
        batch: ProviderBatch,
    ) -> Result<ProviderBatchApplyResult, StoreError> {
        ensure_serialized_budget(&batch, IpcPayloadKind::ProviderBatch)?;
        crate::provider_ingest::apply_provider_batch(
            &mut self.connection,
            batch,
            crate::provider_ingest::ProviderBatchFailpoint::None,
        )
    }

    #[cfg(test)]
    fn apply_provider_batch_with_failpoint(
        &mut self,
        batch: ProviderBatch,
        failpoint: crate::provider_ingest::ProviderBatchFailpoint,
    ) -> Result<ProviderBatchApplyResult, StoreError> {
        crate::provider_ingest::apply_provider_batch(&mut self.connection, batch, failpoint)
    }

    pub fn bootstrap(&self) -> Result<MailboxBootstrap, StoreError> {
        let now = now_ms();
        let accounts = self
            .connection
            .prepare(
                "SELECT a.id, a.name, a.email, a.color, a.signature,
                        COALESCE(SUM(CASE
                          WHEN e.in_inbox = 1 AND e.unread = 1 AND NOT EXISTS (
                            SELECT 1 FROM snoozes s
                            WHERE s.thread_id = e.id AND s.wake_at > ?1
                          ) THEN 1 ELSE 0 END), 0) AS unread,
                        COUNT(e.id) AS total,
                        COALESCE(MAX(p.refresh_seconds), ?2) AS refresh_seconds,
                        -- An account has at most one provider row, so these
                        -- aggregates are the row itself; they are aggregates
                        -- only to satisfy the grouping the counts need.
                        MAX(p.last_sync_at) AS last_sync_at,
                        MAX(p.sync_state) AS sync_state,
                        MAX(p.last_error_code) AS last_error_code
                 FROM accounts a
                 LEFT JOIN thread_effective e
                   ON e.account_id = a.id AND e.remote_deleted = 0 AND e.trashed = 0
                 LEFT JOIN provider_accounts p ON p.account_id = a.id
                 GROUP BY a.id, a.name, a.email, a.color, a.signature
                 ORDER BY a.name",
            )?
            .query_map(params![now, DEFAULT_REFRESH_SECONDS], |row| {
                Ok(AccountSummary {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    email: row.get(2)?,
                    color: row.get(3)?,
                    signature: row.get(4)?,
                    unread: row.get(5)?,
                    total: row.get(6)?,
                    refresh_seconds: row.get(7)?,
                    last_sync_at: row.get(8)?,
                    sync_state: row.get(9)?,
                    last_error_code: row.get(10)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let per_account_counts = self
            .connection
            .prepare(
                "SELECT account_id,
                        COALESCE(SUM(CASE
                          WHEN trashed = 0 AND in_inbox = 1 AND NOT EXISTS (
                            SELECT 1 FROM snoozes s
                            WHERE s.thread_id = e.id AND s.wake_at > ?1
                          ) THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN trashed = 0 THEN starred ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN trashed = 0 THEN has_from_me ELSE 0 END), 0),
                        COALESCE(SUM(CASE
                          WHEN trashed = 0 AND in_inbox = 0 AND NOT EXISTS (
                            SELECT 1 FROM snoozes s
                            WHERE s.thread_id = e.id AND s.wake_at > ?1
                          ) THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN trashed = 0 THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN EXISTS (
                          SELECT 1 FROM snoozes s
                          WHERE s.thread_id = e.id AND s.wake_at > ?1
                        ) AND trashed = 0 THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(trashed), 0)
                 FROM thread_effective e
                 WHERE remote_deleted = 0
                 GROUP BY account_id",
            )?
            .query_map([now], |row| {
                Ok(ViewCountSummary {
                    account_id: Some(row.get(0)?),
                    inbox: row.get(1)?,
                    starred: row.get(2)?,
                    sent: row.get(3)?,
                    archive: row.get(4)?,
                    all: row.get(5)?,
                    snoozed: row.get(6)?,
                    trash: row.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut view_counts = Vec::with_capacity(accounts.len() + 1);
        view_counts.push(ViewCountSummary {
            account_id: None,
            inbox: per_account_counts.iter().map(|count| count.inbox).sum(),
            archive: per_account_counts.iter().map(|count| count.archive).sum(),
            starred: per_account_counts.iter().map(|count| count.starred).sum(),
            sent: per_account_counts.iter().map(|count| count.sent).sum(),
            all: per_account_counts.iter().map(|count| count.all).sum(),
            snoozed: per_account_counts.iter().map(|count| count.snoozed).sum(),
            trash: per_account_counts.iter().map(|count| count.trash).sum(),
        });
        for account in &accounts {
            view_counts.push(
                per_account_counts
                    .iter()
                    .find(|count| count.account_id.as_deref() == Some(account.id.as_str()))
                    .cloned()
                    .unwrap_or(ViewCountSummary {
                        account_id: Some(account.id.clone()),
                        inbox: 0,
                        archive: 0,
                        starred: 0,
                        sent: 0,
                        all: 0,
                        snoozed: 0,
                        trash: 0,
                    }),
            );
        }

        let drafts = self.list_draft_headers()?;
        let containers = self.list_containers()?;

        let bootstrap = MailboxBootstrap {
            schema_version: SCHEMA_VERSION,
            accounts,
            view_counts,
            drafts,
            containers,
        };
        ensure_serialized_budget(&bootstrap, IpcPayloadKind::Bootstrap)?;
        Ok(bootstrap)
    }

    /// Every folder or label the account keeps that the fixed views do not
    /// already stand for, with the thread counts the sidebar shows.
    ///
    /// Counted the same way the views are: a thread belongs to a folder when
    /// any of its live messages does, and trashed threads are left out so the
    /// count matches what opening the folder shows.
    pub fn list_containers(&self) -> Result<Vec<ContainerSummary>, StoreError> {
        let excluded = FIXED_VIEW_ROLES
            .iter()
            .map(|role| format!("'{role}'"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT container.account_id, container.remote_id, container.name,
                    container.kind, container.role,
                    COUNT(DISTINCT CASE WHEN thread.unread = 1 THEN thread.id END) AS unread,
                    COUNT(DISTINCT thread.id) AS total
             FROM provider_containers container
             LEFT JOIN provider_container_memberships membership
               ON membership.account_id = container.account_id
              AND membership.remote_container_id = container.remote_id
             LEFT JOIN provider_message_refs reference
               ON reference.account_id = membership.account_id
              AND reference.remote_message_id = membership.remote_message_id
             LEFT JOIN messages message
               ON message.id = reference.message_id AND message.remote_deleted = 0
             LEFT JOIN thread_effective thread
               ON thread.id = message.thread_id
              AND thread.remote_deleted = 0 AND thread.trashed = 0
             WHERE container.is_deleted = 0
               AND container.is_selectable = 1
               AND container.kind IN ('folder', 'label')
               AND container.role NOT IN ({excluded})
             GROUP BY container.account_id, container.remote_id, container.name,
                      container.kind, container.role, container.sort_order
             ORDER BY container.account_id, container.sort_order, container.name
             LIMIT ?1"
        );
        self.connection
            .prepare(&sql)?
            .query_map([MAX_CONTAINERS], |row| {
                Ok(ContainerSummary {
                    account_id: row.get(0)?,
                    remote_id: row.get(1)?,
                    name: row.get(2)?,
                    kind: row.get(3)?,
                    role: row.get(4)?,
                    unread: row.get(5)?,
                    total: row.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    /// Validates hidden account identifiers and returns them sorted and deduplicated
    /// so the same visibility always produces the same cursor scope.
    fn hidden_accounts(ids: &[String]) -> Result<Vec<String>, StoreError> {
        if ids.len() > MAX_HIDDEN_ACCOUNTS {
            return Err(StoreError::Validation(
                "Too many hidden accounts requested".into(),
            ));
        }
        let mut hidden = ids
            .iter()
            .map(|id| bounded_text(id, "hiddenAccountId", 200))
            .collect::<Result<Vec<_>, _>>()?;
        hidden.sort();
        hidden.dedup();
        Ok(hidden)
    }

    /// Sets how often one account asks its provider for new mail.
    pub(crate) fn set_account_refresh_seconds(
        &mut self,
        account_id: &str,
        seconds: i64,
    ) -> Result<(), StoreError> {
        let account_id = bounded_text(account_id, "accountId", 200)?;
        if !(MIN_REFRESH_SECONDS..=MAX_REFRESH_SECONDS).contains(&seconds) {
            return Err(StoreError::Validation(format!(
                "Refresh interval must be between {MIN_REFRESH_SECONDS} and {MAX_REFRESH_SECONDS} seconds"
            )));
        }
        let changed = self.connection.execute(
            "UPDATE provider_accounts SET refresh_seconds = ?2, updated_at = ?3
             WHERE account_id = ?1",
            params![account_id, seconds, now_ms()],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound(
                "Provider account was not found".into(),
            ));
        }
        Ok(())
    }

    /// Recolors one account. Every account has a color whether or not a
    /// provider is attached, so this writes to the account itself and a
    /// local-only mailbox can be recolored like any other.
    pub(crate) fn set_account_color(
        &mut self,
        account_id: &str,
        color: &str,
    ) -> Result<(), StoreError> {
        let account_id = bounded_text(account_id, "accountId", 200)?;
        let color = account_color(color)?;
        let changed = self.connection.execute(
            "UPDATE accounts SET color = ?2 WHERE id = ?1",
            params![account_id, color],
        )?;
        if changed == 0 {
            return Err(StoreError::NotFound("Account was not found".into()));
        }
        Ok(())
    }

    pub fn list_threads(&self, input: ThreadPageInput) -> Result<ThreadPage, StoreError> {
        let limit = input.limit.unwrap_or(50).clamp(1, 100);
        let account_id = exact_account_id(input.account_id.as_deref())?;
        let view = input.view.as_deref().unwrap_or("inbox");
        let hidden = Self::hidden_accounts(&input.hidden_account_ids)?;
        let container_id = match input.container_id.as_deref() {
            None => None,
            Some(container_id) => {
                if account_id.is_none() {
                    return Err(StoreError::Validation(
                        "A folder can only be listed within its own account".into(),
                    ));
                }
                Some(bounded_text(container_id, "containerId", 2_048)?)
            }
        };
        // Visibility is part of the scope: a cursor cannot be replayed against a
        // different set of hidden accounts, nor against a different folder.
        let hidden_scope = hidden.join(",");
        let container_scope = container_id.as_deref().unwrap_or("");
        let scope = match account_id.as_deref() {
            Some(account_id) => canonical_scope(&[
                "account:some",
                account_id,
                view,
                &hidden_scope,
                container_scope,
            ]),
            None => canonical_scope(&["account:none", view, &hidden_scope, container_scope]),
        };
        let mut snapshot_at = now_ms();
        let cursor_position = input
            .cursor
            .as_deref()
            .map(|cursor| {
                decode_cursor(&self.cursor_signing_key, CursorKind::Thread, &scope, cursor)
            })
            .transpose()?;
        if let Some(position) = cursor_position {
            snapshot_at = position.snapshot_at;
        }
        let mut conditions = vec!["e.remote_deleted = 0".to_string()];
        let mut values = Vec::new();
        if let Some(account_id) = account_id.as_deref() {
            conditions.push("e.account_id = ?".to_string());
            values.push(Value::Text(bounded_text(account_id, "accountId", 200)?));
        }
        if !hidden.is_empty() {
            let placeholders = std::iter::repeat_n("?", hidden.len())
                .collect::<Vec<_>>()
                .join(", ");
            conditions.push(format!("e.account_id NOT IN ({placeholders})"));
            values.extend(hidden.iter().map(|id| Value::Text(id.clone())));
        }
        append_mailbox_view(&mut conditions, &mut values, view, snapshot_at)?;
        if let Some(container_id) = container_id {
            // A thread is in a folder when any of its live messages is.
            conditions.push(
                "EXISTS (
                   SELECT 1
                   FROM messages message
                   JOIN provider_message_refs reference
                     ON reference.message_id = message.id
                   JOIN provider_container_memberships membership
                     ON membership.account_id = reference.account_id
                    AND membership.remote_message_id = reference.remote_message_id
                   WHERE message.thread_id = e.id AND message.remote_deleted = 0
                     AND membership.account_id = e.account_id
                     AND membership.remote_container_id = ?
                 )"
                .into(),
            );
            values.push(Value::Text(container_id));
        }
        if let Some(position) = cursor_position {
            conditions.push("(e.latest_at < ? OR (e.latest_at = ? AND e.id < ?))".into());
            values.push(Value::Integer(position.sort_timestamp));
            values.push(Value::Integer(position.sort_timestamp));
            values.push(Value::Integer(position.row_id));
        }
        values.push(Value::Integer(limit + 1));
        let predicate = if conditions.is_empty() {
            "1 = 1".to_string()
        } else {
            conditions.join(" AND ")
        };
        let sql = format!(
            "SELECT e.id, e.account_id, e.subject, e.participants, e.snippet, e.latest_at,
                    e.message_count, e.in_inbox, e.unread, e.starred, e.has_from_me, e.category
             FROM thread_effective e
             WHERE {predicate}
             ORDER BY e.latest_at DESC, e.id DESC
             LIMIT ?"
        );
        let mut rows = self
            .connection
            .prepare(&sql)?
            .query_map(params_from_iter(values.iter()), thread_summary_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = rows.len() > limit as usize;
        if has_more {
            rows.truncate(limit as usize);
        }
        let next_cursor = if has_more {
            rows.last()
                .map(|row| {
                    encode_cursor(
                        &self.cursor_signing_key,
                        CursorKind::Thread,
                        &scope,
                        CursorPosition {
                            sort_timestamp: row.latest_at,
                            row_id: row.id,
                            snapshot_at,
                        },
                    )
                })
                .transpose()?
        } else {
            None
        };
        let page = ThreadPage {
            threads: rows,
            has_more,
            next_cursor,
        };
        ensure_serialized_budget(&page, IpcPayloadKind::ThreadPage)?;
        Ok(page)
    }

    pub fn get_thread_summary(
        &self,
        input: ThreadLookupInput,
    ) -> Result<Option<ThreadSummary>, StoreError> {
        if input.thread_id <= 0 {
            return Err(StoreError::Validation("threadId must be positive".into()));
        }
        let query = input.query.unwrap_or_default();
        let compiled =
            crate::search::compile_search(&query, now_ms(), input.timezone_offset_minutes)
                .map_err(StoreError::Validation)?;
        let mut conditions = vec![
            compiled.clause,
            "e.id = ?".into(),
            "e.remote_deleted = 0".into(),
        ];
        let mut values = compiled.parameters;
        values.push(Value::Integer(input.thread_id));
        if let Some(account_id) = exact_account_id(input.account_id.as_deref())? {
            conditions.push("e.account_id = ?".into());
            values.push(Value::Text(account_id));
        }
        append_mailbox_view(
            &mut conditions,
            &mut values,
            input.view.as_deref().unwrap_or("inbox"),
            now_ms(),
        )?;
        let sql = format!(
            "SELECT e.id, e.account_id, e.subject, e.participants, e.snippet, e.latest_at,
                    e.message_count, e.in_inbox, e.unread, e.starred, e.has_from_me, e.category
             FROM thread_effective e
             LEFT JOIN snoozes s ON s.thread_id = e.id
             WHERE {}",
            conditions.join(" AND ")
        );
        self.connection
            .prepare(&sql)?
            .query_row(params_from_iter(values.iter()), thread_summary_from_row)
            .optional()
            .map_err(StoreError::from)
    }

    pub fn get_thread_messages(&self, input: MessagePageInput) -> Result<MessagePage, StoreError> {
        if input.thread_id <= 0 {
            return Err(StoreError::Validation("threadId must be positive".into()));
        }
        let exists = self.connection.query_row(
            "SELECT EXISTS(
               SELECT 1 FROM threads WHERE id = ?1 AND remote_deleted = 0
             )",
            [input.thread_id],
            |row| row.get::<_, i64>(0),
        )? != 0;
        if !exists {
            return Err(StoreError::NotFound("Thread was not found".into()));
        }
        let limit = input.limit.unwrap_or(50).clamp(1, 100);
        let thread_scope = input.thread_id.to_string();
        let scope = canonical_scope(&[&thread_scope]);
        let mut snapshot_at = now_ms();
        let cursor_position = input
            .cursor
            .as_deref()
            .map(|cursor| {
                decode_cursor(
                    &self.cursor_signing_key,
                    CursorKind::Message,
                    &scope,
                    cursor,
                )
            })
            .transpose()?;
        if let Some(position) = cursor_position {
            snapshot_at = position.snapshot_at;
        }
        let mut conditions = vec![
            "thread_id = ?".to_string(),
            "remote_deleted = 0".to_string(),
        ];
        let mut values = vec![Value::Integer(input.thread_id)];
        if let Some(position) = cursor_position {
            conditions.push("(sent_at < ? OR (sent_at = ? AND id < ?))".into());
            values.push(Value::Integer(position.sort_timestamp));
            values.push(Value::Integer(position.sort_timestamp));
            values.push(Value::Integer(position.row_id));
        }
        let invitation = self
            .connection
            .query_row(
                "SELECT thread_id, uid, title, start_at, end_at, timezone, location,
                        organizer, attendees, response, conflict_text
                 FROM invitations WHERE thread_id = ?1",
                [input.thread_id],
                |row| {
                    Ok(InvitationSummary {
                        thread_id: row.get(0)?,
                        uid: row.get(1)?,
                        title: row.get(2)?,
                        start_at: row.get(3)?,
                        end_at: row.get(4)?,
                        timezone: row.get(5)?,
                        location: row.get(6)?,
                        organizer: row.get(7)?,
                        attendees: row.get(8)?,
                        response: row.get(9)?,
                        conflict_text: row.get(10)?,
                    })
                },
            )
            .optional()?;
        if let Some(invitation) = &invitation {
            validate_invitation_summary(invitation)?;
        }
        values.push(Value::Integer(limit + 1));
        let sql = format!(
            "SELECT id, thread_id, sender_name, sender_email, recipients, cc_recipients,
                    bcc_recipients, sent_at, body_text, body_html,
                    blocked_remote_resources, is_from_me,
                    length(CAST(body_text AS BLOB)), length(CAST(body_html AS BLOB))
             FROM messages
             WHERE {}
             ORDER BY sent_at DESC, id DESC
             LIMIT ?",
            conditions.join(" AND ")
        );
        let mut statement = self.connection.prepare(&sql)?;
        let mut query = statement.query(params_from_iter(values.iter()))?;
        let mut rows = Vec::new();
        let mut attachments = Vec::new();
        let mut message_json_bytes = 0_usize;
        let mut attachment_json_bytes = 0_usize;
        let mut has_more = false;
        while let Some(row) = query.next()? {
            if rows.len() >= limit as usize {
                has_more = true;
                break;
            }
            let plain_bytes = row.get::<_, i64>(12)?;
            let html_bytes = row.get::<_, i64>(13)?;
            if plain_bytes < 0
                || html_bytes < 0
                || plain_bytes.saturating_add(html_bytes)
                    > crate::ipc_boundary::MESSAGE_DETAIL_BYTES as i64
            {
                return Err(message_detail_budget_error());
            }
            let mut message = message_summary_from_row(row)?;
            message.remote_images = self.remote_images_for_message(&message)?;
            let candidate_attachments =
                self.attachment_metadata_for_messages(std::slice::from_ref(&message))?;
            let candidate_message_bytes =
                ensure_serialized_budget(&message, IpcPayloadKind::MessageDetail)?;
            let candidate_attachment_bytes =
                candidate_attachments
                    .iter()
                    .try_fold(0_usize, |total, attachment| {
                        Ok::<_, StoreError>(total.saturating_add(ensure_serialized_budget(
                            attachment,
                            IpcPayloadKind::MessageDetail,
                        )?))
                    })?;
            let candidate_cursor = encode_cursor(
                &self.cursor_signing_key,
                CursorKind::Message,
                &scope,
                CursorPosition {
                    sort_timestamp: message.sent_at,
                    row_id: message.id,
                    snapshot_at,
                },
            )?;
            let conservative_shell = MessagePage {
                messages: Vec::new(),
                attachments: Vec::new(),
                invitation: invitation.clone(),
                has_more: true,
                next_cursor: Some(candidate_cursor),
            };
            let candidate_total =
                ensure_serialized_budget(&conservative_shell, IpcPayloadKind::MessageDetail)?
                    .saturating_add(json_array_growth(
                        message_json_bytes.saturating_add(candidate_message_bytes),
                        rows.len() + 1,
                    ))
                    .saturating_add(json_array_growth(
                        attachment_json_bytes.saturating_add(candidate_attachment_bytes),
                        attachments.len() + candidate_attachments.len(),
                    ));
            if candidate_total > crate::ipc_boundary::MESSAGE_DETAIL_BYTES {
                if rows.is_empty() {
                    return Err(message_detail_budget_error());
                }
                has_more = true;
                break;
            }
            message_json_bytes = message_json_bytes.saturating_add(candidate_message_bytes);
            attachment_json_bytes =
                attachment_json_bytes.saturating_add(candidate_attachment_bytes);
            rows.push(message);
            attachments.extend(candidate_attachments);
        }
        drop(query);
        drop(statement);
        let next_cursor = if has_more {
            rows.last()
                .map(|row| {
                    encode_cursor(
                        &self.cursor_signing_key,
                        CursorKind::Message,
                        &scope,
                        CursorPosition {
                            sort_timestamp: row.sent_at,
                            row_id: row.id,
                            snapshot_at,
                        },
                    )
                })
                .transpose()?
        } else {
            None
        };
        rows.reverse();
        attachments.sort_by(|left, right| {
            (left.message_id, left.id.as_str()).cmp(&(right.message_id, right.id.as_str()))
        });
        let page = MessagePage {
            messages: rows,
            attachments,
            invitation,
            has_more,
            next_cursor,
        };
        ensure_serialized_budget(&page, IpcPayloadKind::MessageDetail)?;
        Ok(page)
    }

    fn remote_images_for_message(
        &self,
        message: &MessageSummary,
    ) -> Result<Vec<RemoteImageSummary>, StoreError> {
        let sender = message.sender_email.trim().to_ascii_lowercase();
        let mut statement = self.connection.prepare(
            "SELECT image.resource_id, image.domain, image.alt_text,
                    CASE WHEN sender.sender_email IS NOT NULL OR domain.domain IS NOT NULL
                         THEN 1 ELSE 0 END
             FROM message_remote_images image
             JOIN messages message ON message.id = image.message_id
             JOIN threads thread ON thread.id = message.thread_id
             LEFT JOIN remote_content_sender_allowlist sender
               ON sender.account_id = thread.account_id AND sender.sender_email = ?2
             LEFT JOIN remote_content_domain_allowlist domain
               ON domain.account_id = thread.account_id AND domain.domain = image.domain
             WHERE image.message_id = ?1
             ORDER BY image.resource_id",
        )?;
        let images = statement
            .query_map(params![message.id, sender], |row| {
                Ok(RemoteImageSummary {
                    id: row.get(0)?,
                    domain: row.get(1)?,
                    alt_text: row.get(2)?,
                    allowed_by_policy: row.get::<_, i64>(3)? != 0,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(images)
    }

    pub(crate) fn stored_remote_image(
        &self,
        message_id: i64,
        resource_id: i64,
    ) -> Result<StoredRemoteImage, StoreError> {
        self.connection
            .query_row(
                "SELECT message_id, resource_id, url
                 FROM message_remote_images
                 WHERE message_id = ?1 AND resource_id = ?2",
                params![message_id, resource_id],
                |row| {
                    Ok(StoredRemoteImage {
                        message_id: row.get(0)?,
                        resource_id: row.get(1)?,
                        url: row.get(2)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound("Remote image was not found".into()))
    }

    pub(crate) fn allow_remote_content_sender(
        &mut self,
        message_id: i64,
    ) -> Result<(), StoreError> {
        let (account_id, sender): (String, String) = self
            .connection
            .query_row(
                "SELECT thread.account_id, lower(trim(message.sender_email))
                 FROM messages message JOIN threads thread ON thread.id = message.thread_id
                 WHERE message.id = ?1",
                [message_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound("Message was not found".into()))?;
        if !is_conservative_address(&sender) {
            return Err(StoreError::Validation(
                "Message sender cannot be added to the remote-content policy".into(),
            ));
        }
        self.connection.execute(
            "INSERT OR IGNORE INTO remote_content_sender_allowlist(account_id, sender_email)
             VALUES(?1, ?2)",
            params![account_id, sender],
        )?;
        Ok(())
    }

    pub(crate) fn allow_remote_content_domain(
        &mut self,
        message_id: i64,
        requested_domain: &str,
    ) -> Result<(), StoreError> {
        let domain = requested_domain
            .trim()
            .trim_end_matches('.')
            .to_ascii_lowercase();
        if domain.is_empty() || domain.len() > 253 || domain.parse::<std::net::IpAddr>().is_ok() {
            return Err(StoreError::Validation(
                "Remote image domain is invalid".into(),
            ));
        }
        let account_id = self
            .connection
            .query_row(
                "SELECT thread.account_id
                 FROM message_remote_images image
                 JOIN messages message ON message.id = image.message_id
                 JOIN threads thread ON thread.id = message.thread_id
                 WHERE image.message_id = ?1 AND image.domain = ?2
                 LIMIT 1",
                params![message_id, domain],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound("Remote image domain was not found".into()))?;
        self.connection.execute(
            "INSERT OR IGNORE INTO remote_content_domain_allowlist(account_id, domain)
             VALUES(?1, ?2)",
            params![account_id, domain],
        )?;
        Ok(())
    }

    /// Rewind every provider sync cursor so the next cycle re-enumerates the whole
    /// mailbox and re-ingests each message in place. Nothing is deleted: re-ingest
    /// upserts onto the existing thread and message rows, so pending local intent,
    /// Mux-owned metadata, drafts, and thread identity all survive untouched.
    pub(crate) fn request_full_resync(&mut self) -> Result<FullResyncRequest, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let accounts_reset = transaction.execute("DELETE FROM provider_sync_cursors", [])? as i64;
        transaction.commit()?;
        Ok(FullResyncRequest { accounts_reset })
    }

    fn attachment_metadata_for_messages(
        &self,
        messages: &[MessageSummary],
    ) -> Result<Vec<AttachmentSummary>, StoreError> {
        if messages.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = std::iter::repeat_n("?", messages.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT id, message_id, filename, media_type, byte_length, content_id, disposition
             FROM attachments WHERE message_id IN ({placeholders}) ORDER BY message_id, id"
        );
        self.connection
            .prepare(&sql)?
            .query_map(
                params_from_iter(messages.iter().map(|message| Value::Integer(message.id))),
                |row| {
                    Ok(AttachmentSummary {
                        id: row.get(0)?,
                        message_id: row.get(1)?,
                        filename: row.get(2)?,
                        media_type: row.get(3)?,
                        byte_length: row.get(4)?,
                        content_id: row.get(5)?,
                        disposition: row.get(6)?,
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn read_attachment(&self, attachment_id: &str) -> Result<AttachmentContent, StoreError> {
        let attachment_id = bounded_text(attachment_id.trim(), "attachmentId", 200)?;
        let metadata = self
            .connection
            .query_row(
                "SELECT byte_length,
                        length(content),
                        length(CAST(filename AS BLOB)),
                        length(CAST(media_type AS BLOB))
                 FROM attachments WHERE id = ?1",
                [&attachment_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound("Attachment was not found".into()))?;
        let (declared_length, stored_length, filename_length, media_type_length) = metadata;
        if declared_length != stored_length {
            return Err(StoreError::Validation(
                "Attachment byte metadata is inconsistent".into(),
            ));
        }
        if stored_length < 0
            || stored_length as u64 > crate::mime_ingest::MAX_ATTACHMENT_BYTES as u64
        {
            return Err(StoreError::Validation(format!(
                "Attachment content exceeds the {}-byte stored limit",
                crate::mime_ingest::MAX_ATTACHMENT_BYTES
            )));
        }
        if !(1..=crate::mime_ingest::MAX_FILENAME_BYTES as i64).contains(&filename_length)
            || !(1..=crate::mime_ingest::MAX_MEDIA_TYPE_BYTES as i64).contains(&media_type_length)
        {
            return Err(StoreError::Validation(
                "Attachment metadata exceeds its stored limits".into(),
            ));
        }

        let (filename, media_type, bytes) = self
            .connection
            .query_row(
                "SELECT filename, media_type, content
                 FROM attachments
                 WHERE id = ?1
                   AND byte_length = length(content)
                   AND length(content) = ?2
                   AND length(CAST(filename AS BLOB)) BETWEEN 1 AND ?3
                   AND length(CAST(media_type AS BLOB)) BETWEEN 1 AND ?4",
                params![
                    &attachment_id,
                    stored_length,
                    crate::mime_ingest::MAX_FILENAME_BYTES as i64,
                    crate::mime_ingest::MAX_MEDIA_TYPE_BYTES as i64
                ],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| {
                StoreError::Conflict("Attachment changed during the bounded read".into())
            })?;
        if bytes.len() as i64 != stored_length {
            return Err(StoreError::Conflict(
                "Attachment changed during the bounded read".into(),
            ));
        }
        let content = AttachmentContent {
            filename,
            media_type,
            byte_length: stored_length,
            data_base64: BASE64_STANDARD.encode(bytes),
        };
        ensure_serialized_budget(&content, IpcPayloadKind::AttachmentContent)?;
        Ok(content)
    }

    pub fn search_threads(&self, input: SearchInput) -> Result<SearchPage, StoreError> {
        let limit = input.limit.unwrap_or(50).clamp(1, 100);
        let account_id = exact_account_id(input.account_id.as_deref())?;
        let view = input.view.as_deref().unwrap_or("all");
        let timezone = input.timezone_offset_minutes.to_string();
        let hidden = Self::hidden_accounts(&input.hidden_account_ids)?;
        let hidden_scope = hidden.join(",");
        let scope = match account_id.as_deref() {
            Some(account_id) => canonical_scope(&[
                "account:some",
                account_id,
                &input.query,
                view,
                &timezone,
                &hidden_scope,
            ]),
            None => {
                canonical_scope(&["account:none", &input.query, view, &timezone, &hidden_scope])
            }
        };
        let mut snapshot_at = now_ms();
        let cursor_position = input
            .cursor
            .as_deref()
            .map(|cursor| {
                decode_cursor(&self.cursor_signing_key, CursorKind::Search, &scope, cursor)
            })
            .transpose()?;
        if let Some(position) = cursor_position {
            snapshot_at = position.snapshot_at;
        }
        let compiled =
            crate::search::compile_search(&input.query, snapshot_at, input.timezone_offset_minutes)
                .map_err(StoreError::Validation)?;
        let mut conditions = vec![compiled.clause, "e.remote_deleted = 0".into()];
        let mut values = compiled.parameters;

        if !hidden.is_empty() {
            let placeholders = std::iter::repeat_n("?", hidden.len())
                .collect::<Vec<_>>()
                .join(", ");
            conditions.push(format!("e.account_id NOT IN ({placeholders})"));
            values.extend(hidden.iter().map(|id| Value::Text(id.clone())));
        }
        if let Some(account_id) = account_id {
            conditions.push("e.account_id = ?".into());
            values.push(Value::Text(account_id));
        }
        append_mailbox_view(&mut conditions, &mut values, view, snapshot_at)?;
        if let Some(position) = cursor_position {
            conditions.push("(e.latest_at < ? OR (e.latest_at = ? AND e.id < ?))".into());
            values.push(Value::Integer(position.sort_timestamp));
            values.push(Value::Integer(position.sort_timestamp));
            values.push(Value::Integer(position.row_id));
        }
        values.push(Value::Integer(limit + 1));
        let sql = format!(
            "SELECT e.id, e.account_id, e.subject, e.participants, e.snippet, e.latest_at,
                    e.message_count, e.in_inbox, e.unread, e.starred, e.has_from_me, e.category
             FROM thread_effective e
             LEFT JOIN snoozes s ON s.thread_id = e.id
             WHERE {}
             ORDER BY e.latest_at DESC, e.id DESC
             LIMIT ?",
            conditions.join(" AND ")
        );
        let mut rows = self
            .connection
            .prepare(&sql)?
            .query_map(params_from_iter(values.iter()), |row| {
                Ok(ThreadSummary {
                    id: row.get(0)?,
                    account_id: row.get(1)?,
                    subject: row.get(2)?,
                    participants: row.get(3)?,
                    snippet: row.get(4)?,
                    latest_at: row.get(5)?,
                    message_count: row.get(6)?,
                    in_inbox: row.get::<_, i64>(7)? != 0,
                    unread: row.get::<_, i64>(8)? != 0,
                    starred: row.get::<_, i64>(9)? != 0,
                    has_from_me: row.get::<_, i64>(10)? != 0,
                    category: row.get(11)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = rows.len() > limit as usize;
        if has_more {
            rows.truncate(limit as usize);
        }
        let next_cursor = if has_more {
            rows.last()
                .map(|row| {
                    encode_cursor(
                        &self.cursor_signing_key,
                        CursorKind::Search,
                        &scope,
                        CursorPosition {
                            sort_timestamp: row.latest_at,
                            row_id: row.id,
                            snapshot_at,
                        },
                    )
                })
                .transpose()?
        } else {
            None
        };
        let page = SearchPage {
            rows,
            has_more,
            next_cursor,
        };
        ensure_serialized_budget(&page, IpcPayloadKind::SearchPage)?;
        Ok(page)
    }

    pub fn list_drafts(&self) -> Result<Vec<DraftSummary>, StoreError> {
        let drafts = self
            .connection
            .prepare(
                "SELECT d.id, d.account_id, a.name, a.email, a.color,
                        d.recipients, d.cc_recipients, d.bcc_recipients,
                        d.subject, d.body, d.body_html,
                        d.reply_to_thread_id, d.updated_at, d.revision,
                        EXISTS(
                          SELECT 1 FROM operations o
                          WHERE o.field = 'send'
                            AND json_extract(o.payload_json, '$.draftId') = d.id
                            AND o.state IN ('pending', 'executing', 'retrying', 'outcome_unknown')
                        ) AS locked
                 FROM drafts d
                 JOIN accounts a ON a.id = d.account_id
                 ORDER BY d.updated_at DESC, d.id DESC",
            )?
            .query_map([], |row| {
                Ok(DraftSummary {
                    id: row.get(0)?,
                    account_id: row.get(1)?,
                    account_name: row.get(2)?,
                    account_email: row.get(3)?,
                    account_color: row.get(4)?,
                    recipients: row.get(5)?,
                    cc_recipients: row.get(6)?,
                    bcc_recipients: row.get(7)?,
                    subject: row.get(8)?,
                    body: row.get(9)?,
                    body_html: row.get(10)?,
                    reply_to_thread_id: row.get(11)?,
                    updated_at: row.get(12)?,
                    revision: row.get(13)?,
                    locked: row.get::<_, i64>(14)? != 0,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(drafts)
    }

    fn list_draft_headers(&self) -> Result<Vec<DraftHeaderSummary>, StoreError> {
        self.connection
            .prepare(
                "SELECT d.id, d.account_id, a.name, a.email, a.color,
                        d.recipients, d.cc_recipients, d.bcc_recipients, d.subject,
                        d.reply_to_thread_id, d.updated_at, d.revision,
                        EXISTS(
                          SELECT 1 FROM operations o
                          WHERE o.field = 'send'
                            AND json_extract(o.payload_json, '$.draftId') = d.id
                            AND o.state IN ('pending', 'executing', 'retrying', 'outcome_unknown')
                        ) AS locked
                 FROM drafts d
                 JOIN accounts a ON a.id = d.account_id
                 ORDER BY d.updated_at DESC, d.id DESC",
            )?
            .query_map([], |row| {
                Ok(DraftHeaderSummary {
                    id: row.get(0)?,
                    account_id: row.get(1)?,
                    account_name: row.get(2)?,
                    account_email: row.get(3)?,
                    account_color: row.get(4)?,
                    recipients: row.get(5)?,
                    cc_recipients: row.get(6)?,
                    bcc_recipients: row.get(7)?,
                    subject: row.get(8)?,
                    reply_to_thread_id: row.get(9)?,
                    updated_at: row.get(10)?,
                    revision: row.get(11)?,
                    locked: row.get::<_, i64>(12)? != 0,
                })
            })?
            .collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }

    pub fn save_draft(&mut self, input: SaveDraftInput) -> Result<DraftSummary, StoreError> {
        let account_id = exact_account_id(Some(&input.account_id))?
            .expect("a present account ID remains present after validation");
        let recipients = bounded_header(
            input.recipients.trim(),
            "recipients",
            MAX_RECIPIENT_HEADER_CHARS,
        )?;
        let cc_recipients = bounded_header(
            input.cc_recipients.trim(),
            "ccRecipients",
            MAX_RECIPIENT_HEADER_CHARS,
        )?;
        let bcc_recipients = bounded_header(
            input.bcc_recipients.trim(),
            "bccRecipients",
            MAX_RECIPIENT_HEADER_CHARS,
        )?;
        let subject = bounded_header(input.subject.trim(), "subject", 2_000)?;
        let body = bounded_text(&input.body, "body", MAX_PLAIN_DRAFT_CHARS)?;
        let body_html = bounded_text(&input.body_html, "bodyHtml", MAX_FORMATTED_DRAFT_CHARS)?;
        let account_exists = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ?1)",
            [&account_id],
            |row| row.get::<_, i64>(0),
        )? != 0;
        if !account_exists {
            return Err(StoreError::NotFound("Draft account was not found".into()));
        }
        if let Some(thread_id) = input.reply_to_thread_id {
            let reply_account = self
                .connection
                .query_row(
                    "SELECT account_id FROM threads WHERE id = ?1",
                    [thread_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            match reply_account {
                None => return Err(StoreError::NotFound("Reply thread was not found".into())),
                Some(reply_account) if reply_account != account_id => {
                    return Err(StoreError::Validation(
                        "Reply drafts must use the thread's account".into(),
                    ));
                }
                Some(_) => {}
            }
        }

        let updated_at = now_ms();
        let draft_id = if let Some(id) = input.id.as_deref() {
            let current = self
                .connection
                .query_row(
                    "SELECT account_id, revision FROM drafts WHERE id = ?1",
                    [id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()?
                .ok_or_else(|| StoreError::NotFound("Draft was not found".into()))?;
            if self.draft_locked(id)? {
                return Err(StoreError::Conflict(
                    "Draft is locked while its send outcome is unresolved".into(),
                ));
            }
            if current.0 != account_id {
                return Err(StoreError::Validation(
                    "A saved draft cannot move between accounts".into(),
                ));
            }
            let expected = input.expected_revision.ok_or_else(|| {
                StoreError::Conflict("expectedRevision is required when updating a draft".into())
            })?;
            if expected != current.1 {
                return Err(StoreError::Conflict(format!(
                    "Draft revision changed (expected {expected}, found {})",
                    current.1
                )));
            }
            let changed = self.connection.execute(
                "UPDATE drafts
                 SET recipients = ?1, cc_recipients = ?2, bcc_recipients = ?3,
                     subject = ?4, body = ?5, body_html = ?6,
                     reply_to_thread_id = ?7, updated_at = ?8, revision = revision + 1
                 WHERE id = ?9 AND revision = ?10",
                params![
                    recipients,
                    cc_recipients,
                    bcc_recipients,
                    subject,
                    body,
                    body_html,
                    input.reply_to_thread_id,
                    updated_at,
                    id,
                    expected
                ],
            )?;
            if changed != 1 {
                return Err(StoreError::Conflict("Draft revision changed".into()));
            }
            id.to_string()
        } else {
            let id = new_id(&self.connection, "draft")?;
            self.connection.execute(
                "INSERT INTO drafts(
                   id, account_id, recipients, cc_recipients, bcc_recipients,
                   subject, body, body_html, reply_to_thread_id, updated_at, revision
                 ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1)",
                params![
                    id,
                    account_id,
                    recipients,
                    cc_recipients,
                    bcc_recipients,
                    subject,
                    body,
                    body_html,
                    input.reply_to_thread_id,
                    updated_at
                ],
            )?;
            id
        };
        self.get_draft(&draft_id)?
            .ok_or_else(|| StoreError::NotFound("Saved draft was not found".into()))
    }

    pub fn delete_draft(&mut self, draft_id: &str) -> Result<(), StoreError> {
        if self.draft_locked(draft_id)? {
            return Err(StoreError::Conflict(
                "Draft is locked while its send outcome is unresolved".into(),
            ));
        }
        let deleted = self
            .connection
            .execute("DELETE FROM drafts WHERE id = ?1", [draft_id])?;
        if deleted == 0 {
            return Err(StoreError::NotFound("Draft was not found".into()));
        }
        Ok(())
    }

    pub fn queue_send(
        &mut self,
        draft_id: &str,
        undo_window_ms: i64,
    ) -> Result<OperationSummary, StoreError> {
        let created_at = now_ms();
        let not_before = created_at + undo_window_ms.clamp(1_000, 60_000);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let draft = transaction
            .query_row(
                "SELECT d.id, d.account_id, a.name, a.email, a.color,
                        d.recipients, d.cc_recipients, d.bcc_recipients,
                        d.subject, d.body, d.body_html,
                        d.reply_to_thread_id, d.updated_at, d.revision,
                        EXISTS(
                          SELECT 1 FROM operations o
                          WHERE o.field = 'send'
                            AND json_extract(o.payload_json, '$.draftId') = d.id
                            AND o.state IN ('pending', 'executing', 'retrying', 'outcome_unknown')
                        ) AS locked
                 FROM drafts d JOIN accounts a ON a.id = d.account_id
                 WHERE d.id = ?1",
                [draft_id],
                |row| {
                    Ok(DraftSummary {
                        id: row.get(0)?,
                        account_id: row.get(1)?,
                        account_name: row.get(2)?,
                        account_email: row.get(3)?,
                        account_color: row.get(4)?,
                        recipients: row.get(5)?,
                        cc_recipients: row.get(6)?,
                        bcc_recipients: row.get(7)?,
                        subject: row.get(8)?,
                        body: row.get(9)?,
                        body_html: row.get(10)?,
                        reply_to_thread_id: row.get(11)?,
                        updated_at: row.get(12)?,
                        revision: row.get(13)?,
                        locked: row.get::<_, i64>(14)? != 0,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound("Draft was not found".into()))?;
        if draft.locked {
            return Err(StoreError::Conflict(
                "Draft already has an unresolved send".into(),
            ));
        }
        if !draft.recipients.trim().is_empty() {
            validate_recipient_list(&draft.recipients)?;
        }
        if !draft.cc_recipients.trim().is_empty() {
            validate_recipient_list(&draft.cc_recipients)?;
        }
        if !draft.bcc_recipients.trim().is_empty() {
            validate_recipient_list(&draft.bcc_recipients)?;
        }
        if draft.recipients.trim().is_empty()
            && draft.cc_recipients.trim().is_empty()
            && draft.bcc_recipients.trim().is_empty()
        {
            return Err(StoreError::Validation(
                "Add at least one recipient before sending".into(),
            ));
        }
        if draft.subject.trim().is_empty() && draft.body.trim().is_empty() {
            return Err(StoreError::Validation(
                "Add a subject or message before sending".into(),
            ));
        }
        let id = new_id(&transaction, "op")?;
        let work_id = new_id(&transaction, "work")?;
        let message_id = new_id(&transaction, "message")?;
        let submission_message_id = format!("<{}@mux.invalid>", id.replace('_', "-"));
        let client_correlation_id = format!("mux-{}", id.replace('_', "-"));
        let (in_reply_to, references) =
            reply_threading_headers(&transaction, draft.reply_to_thread_id)?;
        let (provider_kind, remote_thread_id) = resolve_send_provider_target(&transaction, &draft)?;
        if provider_kind == "imap"
            && !provider_capability_enabled(&transaction, &draft.account_id, "outgoing_mail")?
        {
            return Err(StoreError::Conflict(
                "This IMAP account has no SMTP transport configured".into(),
            ));
        }
        let payload = send_payload_for_draft(
            &draft,
            message_id,
            submission_message_id,
            created_at,
            in_reply_to,
            references,
            client_correlation_id,
            provider_kind.clone(),
            remote_thread_id,
        );
        let payload_json = serde_json::to_string(&payload)?;
        let prepared = crate::outgoing::prepare_from_durable_payload(&payload_json, &[])
            .map_err(|error| StoreError::Validation(error.to_string()))?;
        if prepared.reconciliation_key()
            != payload
                .submission_message_id
                .as_deref()
                .expect("new send snapshot has a submission identity")
        {
            return Err(StoreError::Validation(
                "Prepared outgoing identity does not match the durable snapshot".into(),
            ));
        }
        transaction.execute(
            "INSERT INTO operations(
               id, thread_id, field, kind, old_value, new_value, payload_json,
               state, created_at, not_before
             ) VALUES(?1, ?2, 'send', 'send', 'draft', 'submitted', ?3,
                      'pending', ?4, ?5)",
            params![
                id,
                draft.reply_to_thread_id,
                payload_json.clone(),
                created_at,
                not_before
            ],
        )?;
        let send_scope = match provider_kind.as_str() {
            "gmail" => crate::gmail::gmail_send_scope(&id),
            "imap" => crate::smtp::smtp_send_scope(&id),
            _ => "outgoing:v1".into(),
        };
        enqueue_in_transaction(
            &transaction,
            NewWorkItem {
                id: work_id.clone(),
                account_id: draft.account_id.clone(),
                operation_id: Some(id.clone()),
                kind: WorkKind::Send,
                scope: send_scope.clone(),
                ordering_key: payload
                    .submission_message_id
                    .clone()
                    .expect("new sends always have a submission identity"),
                payload_json: payload_json.clone(),
                priority: 100,
                available_at: not_before,
                max_attempts: 8,
            },
            created_at,
        )
        .map_err(durable_work_error)?;
        if provider_kind == "gmail" {
            enqueue_in_transaction(
                &transaction,
                crate::gmail::gmail_send_reconciliation_work(
                    &draft.account_id,
                    &id,
                    &work_id,
                    &send_scope,
                    &payload_json,
                    not_before.saturating_add(1_000),
                )
                .map_err(StoreError::Validation)?,
                created_at.saturating_add(1),
            )
            .map_err(durable_work_error)?;
        }
        transaction.commit()?;
        Ok(OperationSummary {
            id,
            thread_id: draft.reply_to_thread_id,
            field: "send".into(),
            kind: "send".into(),
            state: "pending".into(),
            not_before,
        })
    }

    pub fn apply_thread_action(
        &mut self,
        thread_id: i64,
        action: &str,
    ) -> Result<OperationSummary, StoreError> {
        let (field, new_value) = match action {
            "archive" => ("in_inbox", "0"),
            "restore" => ("in_inbox", "1"),
            "read" => ("unread", "0"),
            "unread" => ("unread", "1"),
            "star" => ("starred", "1"),
            "unstar" => ("starred", "0"),
            "delete" => ("trashed", "1"),
            "untrash" => ("trashed", "0"),
            _ => return Err(StoreError::Validation("Unknown thread action".into())),
        };
        let created_at = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old_value = transaction
            .query_row(
                &format!(
                    "SELECT {field} FROM thread_effective
                     WHERE id = ?1 AND remote_deleted = 0"
                ),
                [thread_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound("Thread was not found".into()))?
            .to_string();
        let not_before = created_at + 350;
        let id = new_id(&transaction, "op")?;
        let work_id = new_id(&transaction, "work")?;
        let target = resolve_thread_mutation_target(&transaction, thread_id)?;
        let payload_json = serde_json::to_string(&ThreadMutationPayload {
            format_version: target.remote_thread_id.as_ref().map(|_| 1),
            operation_id: id.clone(),
            account_id: target
                .remote_thread_id
                .as_ref()
                .map(|_| target.account_id.clone()),
            thread_id,
            field: field.into(),
            value: new_value.into(),
            remote_thread_id: target.remote_thread_id,
            remote_container_id: None,
            undo_of: None,
        })?;
        transaction.execute(
            "INSERT INTO operations(
               id, thread_id, field, kind, old_value, new_value,
               payload_json, state, created_at, not_before
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', ?8, ?9)",
            params![
                id,
                thread_id,
                field,
                action,
                old_value,
                new_value,
                payload_json,
                created_at,
                not_before
            ],
        )?;
        enqueue_in_transaction(
            &transaction,
            NewWorkItem {
                id: work_id,
                account_id: target.account_id,
                operation_id: Some(id.clone()),
                kind: WorkKind::Mutation,
                scope: target.scope,
                ordering_key: id.clone(),
                payload_json,
                priority: 50,
                available_at: not_before,
                max_attempts: 8,
            },
            created_at,
        )
        .map_err(durable_work_error)?;
        transaction.commit()?;
        Ok(OperationSummary {
            id,
            thread_id: Some(thread_id),
            field: field.into(),
            kind: action.into(),
            state: "pending".into(),
            not_before,
        })
    }

    /// Journals a provider container membership without exposing provider
    /// identities through the Svelte command surface. A future provider-neutral
    /// container DTO can call this boundary using a Mux-owned opaque ID.
    #[allow(dead_code)] // Awaiting a provider-neutral local container DTO; covered by Rust contracts.
    pub(crate) fn apply_thread_label_action(
        &mut self,
        thread_id: i64,
        remote_container_id: &str,
        present: bool,
    ) -> Result<OperationSummary, StoreError> {
        let remote_container_id =
            bounded_text(remote_container_id.trim(), "remoteContainerId", 2_048)?;
        let created_at = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let target = resolve_thread_mutation_target(&transaction, thread_id)?;
        let remote_thread_id = target.remote_thread_id.clone().ok_or_else(|| {
            StoreError::Conflict("Provider labels require a remote thread identity".into())
        })?;
        let selectable = transaction.query_row(
            "SELECT EXISTS(
               SELECT 1 FROM provider_containers
               WHERE account_id = ?1 AND remote_id = ?2
                 AND is_deleted = 0 AND is_selectable = 1 AND role = 'custom'
             )",
            params![target.account_id, remote_container_id],
            |row| row.get::<_, i64>(0),
        )? != 0;
        if !selectable {
            return Err(StoreError::Validation(
                "The provider label is unavailable or not user-selectable".into(),
            ));
        }
        // 0 = absent from every locally known remote message, 1 = present on
        // all of them, 2 = partial. Pending desired state overlays confirmed
        // membership in this provider-neutral effective view.
        let old_state = transaction.query_row(
            "SELECT label_state
             FROM provider_thread_label_effective
             WHERE account_id = ?1 AND remote_thread_id = ?2
               AND thread_id = ?3 AND remote_container_id = ?4",
            params![
                target.account_id,
                remote_thread_id,
                thread_id,
                remote_container_id
            ],
            |row| row.get::<_, i64>(0),
        )?;
        if (old_state == 1 && present) || (old_state == 0 && !present) {
            return Err(StoreError::Conflict(
                "The provider label already has that state".into(),
            ));
        }
        let id = new_id(&transaction, "op")?;
        let work_id = new_id(&transaction, "work")?;
        let not_before = created_at + 350;
        let payload_json = serde_json::to_string(&ThreadMutationPayload {
            format_version: Some(1),
            operation_id: id.clone(),
            account_id: Some(target.account_id.clone()),
            thread_id,
            field: "provider_label".into(),
            value: i64::from(present).to_string(),
            remote_thread_id: Some(remote_thread_id),
            remote_container_id: Some(remote_container_id),
            undo_of: None,
        })?;
        transaction.execute(
            "INSERT INTO operations(
               id, thread_id, field, kind, old_value, new_value, payload_json,
               state, created_at, not_before
             ) VALUES(?1, ?2, 'provider_label', ?3, ?4, ?5, ?6, 'pending', ?7, ?8)",
            params![
                id,
                thread_id,
                if present { "label" } else { "unlabel" },
                old_state.to_string(),
                i64::from(present).to_string(),
                payload_json,
                created_at,
                not_before
            ],
        )?;
        enqueue_in_transaction(
            &transaction,
            NewWorkItem {
                id: work_id,
                account_id: target.account_id,
                operation_id: Some(id.clone()),
                kind: WorkKind::Mutation,
                scope: target.scope,
                ordering_key: id.clone(),
                payload_json,
                priority: 50,
                available_at: not_before,
                max_attempts: 8,
            },
            created_at,
        )
        .map_err(durable_work_error)?;
        transaction.commit()?;
        Ok(OperationSummary {
            id,
            thread_id: Some(thread_id),
            field: "provider_label".into(),
            kind: if present { "label" } else { "unlabel" }.into(),
            state: "pending".into(),
            not_before,
        })
    }

    pub fn snooze_thread(
        &mut self,
        thread_id: i64,
        wake_at: i64,
    ) -> Result<OperationSummary, StoreError> {
        let created_at = now_ms();
        if thread_id <= 0 {
            return Err(StoreError::Validation("threadId must be positive".into()));
        }
        if wake_at <= created_at || wake_at > created_at.saturating_add(MAX_SNOOZE_DISTANCE_MS) {
            return Err(StoreError::Validation(
                "wakeAt must be in the future and no more than 10 years away".into(),
            ));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let in_inbox = transaction
            .query_row(
                "SELECT in_inbox FROM thread_effective
                 WHERE id = ?1 AND remote_deleted = 0",
                [thread_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound("Thread was not found".into()))?;
        let previous = transaction
            .query_row(
                "SELECT wake_at FROM snoozes WHERE thread_id = ?1",
                [thread_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let previous_location = transaction
            .query_row(
                "SELECT previous_location FROM snoozes WHERE thread_id = ?1",
                [thread_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .unwrap_or_else(|| {
                if in_inbox == 1 {
                    "inbox".into()
                } else {
                    "archive".into()
                }
            });
        let id = new_id(&transaction, "op")?;
        transaction.execute(
            "INSERT INTO snoozes(thread_id, wake_at, created_at, previous_location)
             VALUES(?1, ?2, ?3, ?4)
             ON CONFLICT(thread_id) DO UPDATE SET
               wake_at = excluded.wake_at,
               created_at = excluded.created_at",
            params![thread_id, wake_at, created_at, previous_location],
        )?;
        transaction.execute(
            "INSERT INTO operations(
               id, thread_id, field, kind, old_value, new_value,
               state, created_at, not_before, confirmed_at
             ) VALUES(?1, ?2, 'snooze', 'snooze', ?3, ?4,
                      'confirmed', ?5, ?5, ?5)",
            params![
                id,
                thread_id,
                previous.map(|value| value.to_string()),
                wake_at.to_string(),
                created_at
            ],
        )?;
        transaction.commit()?;
        Ok(OperationSummary {
            id,
            thread_id: Some(thread_id),
            field: "snooze".into(),
            kind: "snooze".into(),
            state: "confirmed".into(),
            not_before: created_at,
        })
    }

    pub fn rsvp_thread(
        &mut self,
        thread_id: i64,
        response: &str,
    ) -> Result<OperationSummary, StoreError> {
        if thread_id <= 0 {
            return Err(StoreError::Validation("threadId must be positive".into()));
        }
        if !matches!(
            response,
            "needsAction" | "accepted" | "tentative" | "declined"
        ) {
            return Err(StoreError::Validation("Invalid RSVP response".into()));
        }
        let created_at = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let previous = transaction
            .query_row(
                "SELECT response FROM invitations WHERE thread_id = ?1",
                [thread_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound("Invitation was not found".into()))?;
        let id = new_id(&transaction, "op")?;
        transaction.execute(
            "UPDATE invitations SET response = ?1 WHERE thread_id = ?2",
            params![response, thread_id],
        )?;
        transaction.execute(
            "INSERT INTO operations(
               id, thread_id, field, kind, old_value, new_value,
               state, created_at, not_before, confirmed_at
             ) VALUES(?1, ?2, 'rsvp', 'rsvp', ?3, ?4,
                      'confirmed', ?5, ?5, ?5)",
            params![id, thread_id, previous, response, created_at],
        )?;
        transaction.commit()?;
        Ok(OperationSummary {
            id,
            thread_id: Some(thread_id),
            field: "rsvp".into(),
            kind: "rsvp".into(),
            state: "confirmed".into(),
            not_before: created_at,
        })
    }

    pub fn list_operations(
        &self,
        input: ListOperationsInput,
    ) -> Result<Vec<OperationActivitySummary>, StoreError> {
        let limit = input.limit.unwrap_or(50);
        if !(1..=MAX_ACTIVITY_ROWS).contains(&limit) {
            return Err(StoreError::Validation(format!(
                "Activity limit must be between 1 and {MAX_ACTIVITY_ROWS}"
            )));
        }
        let operations = self
            .connection
            .prepare(
                "SELECT id, thread_id, field, kind, state, created_at, not_before,
                        confirmed_at, undo_of, attempts
                 FROM operations
                 ORDER BY created_at DESC, rowid DESC
                 LIMIT ?1",
            )?
            .query_map([limit], |row| {
                Ok(OperationActivitySummary {
                    id: row.get(0)?,
                    thread_id: row.get(1)?,
                    field: row.get(2)?,
                    kind: row.get(3)?,
                    state: row.get(4)?,
                    created_at: row.get(5)?,
                    not_before: row.get(6)?,
                    confirmed_at: row.get(7)?,
                    undo_of: row.get(8)?,
                    attempts: row.get(9)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for operation in &operations {
            validate_operation_activity(operation)?;
        }
        ensure_serialized_budget(&operations, IpcPayloadKind::Activity)?;
        Ok(operations)
    }

    pub fn resolve_outcome_unknown_send(
        &mut self,
        operation_id: &str,
    ) -> Result<OperationSummary, StoreError> {
        bounded_text(operation_id, "operationId", 256)?;
        let now = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let operation = transaction
            .query_row(
                "SELECT rowid, id, thread_id, field, kind, old_value, new_value,
                        payload_json, state, not_before
                 FROM operations WHERE id = ?1",
                [operation_id],
                operation_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound("Operation was not found".into()))?;
        if operation.field != "send" || operation.state != "outcome_unknown" {
            return Err(StoreError::Conflict(
                "Only an outcome-unknown send can be resolved without retry".into(),
            ));
        }
        let work_snapshot = transaction
            .query_row(
                "SELECT id, state, payload_json, payload_fingerprint
                 FROM provider_work_items
                 WHERE operation_id = ?1 AND kind = 'send'",
                [operation_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((send_work_id, work_state, work_payload, work_fingerprint)) = work_snapshot else {
            return Err(StoreError::Conflict("Linked send work is missing".into()));
        };
        if work_state != "outcome_unknown" {
            return Err(StoreError::Conflict(
                "Linked send work is not outcome-unknown".into(),
            ));
        }
        if Sha256::digest(work_payload.as_bytes())[..] != work_fingerprint {
            return Err(StoreError::Conflict(
                "Linked send snapshot fingerprint does not match".into(),
            ));
        }
        let payload: SendPayload = serde_json::from_str(&work_payload)?;
        if !send_payload_has_complete_snapshot(&payload) {
            return Err(StoreError::Conflict(
                "Send snapshot is incomplete; the draft cannot be restored safely".into(),
            ));
        }
        let (
            account_id,
            _sender_email,
            recipients,
            cc_recipients,
            bcc_recipients,
            subject,
            body,
            body_html,
            snapshot_reply_to_thread_id,
            draft_revision,
        ) = send_projection_fields(&transaction, &payload)?;
        let recipients = bounded_header(&recipients, "recipients", MAX_RECIPIENT_HEADER_CHARS)?;
        let cc_recipients =
            bounded_header(&cc_recipients, "ccRecipients", MAX_RECIPIENT_HEADER_CHARS)?;
        let bcc_recipients =
            bounded_header(&bcc_recipients, "bccRecipients", MAX_RECIPIENT_HEADER_CHARS)?;
        let subject = bounded_header(&subject, "subject", 2_000)?;
        let body = bounded_text(&body, "body", MAX_PLAIN_DRAFT_CHARS)?;
        let body_html = bounded_text(&body_html, "bodyHtml", MAX_FORMATTED_DRAFT_CHARS)?;

        let draft_exists = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM drafts WHERE id = ?1)",
            [&payload.draft_id],
            |row| row.get::<_, i64>(0),
        )? != 0;
        if !draft_exists {
            let account_exists = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ?1)",
                [&account_id],
                |row| row.get::<_, i64>(0),
            )? != 0;
            if !account_exists {
                return Err(StoreError::Conflict(
                    "The draft account no longer exists".into(),
                ));
            }
            let reply_to_thread_id = match snapshot_reply_to_thread_id {
                Some(thread_id)
                    if transaction.query_row(
                        "SELECT EXISTS(SELECT 1 FROM threads WHERE id = ?1)",
                        [thread_id],
                        |row| row.get::<_, i64>(0),
                    )? != 0 =>
                {
                    Some(thread_id)
                }
                _ => None,
            };
            transaction.execute(
                "INSERT INTO drafts(
                   id, account_id, recipients, cc_recipients, bcc_recipients,
                   subject, body, body_html, reply_to_thread_id, updated_at, revision
                 ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    payload.draft_id,
                    account_id,
                    recipients,
                    cc_recipients,
                    bcc_recipients,
                    subject,
                    body,
                    body_html,
                    reply_to_thread_id,
                    now,
                    draft_revision.unwrap_or(1).max(1),
                ],
            )?;
        }
        let work_changed = transaction.execute(
            "UPDATE provider_work_items
             SET state = 'cancelled', cancel_requested = 1,
                 last_error_code = 'uncertain_send_resolved'
             WHERE operation_id = ?1 AND kind = 'send' AND state = 'outcome_unknown'",
            [operation_id],
        )?;
        let operation_changed = transaction.execute(
            "UPDATE operations SET state = 'cancelled'
             WHERE id = ?1 AND field = 'send' AND state = 'outcome_unknown'",
            [operation_id],
        )?;
        if work_changed != 1 || operation_changed != 1 {
            return Err(StoreError::Conflict(
                "Uncertain send state changed during resolution".into(),
            ));
        }
        let reconciliation_id = format!("reconcile_{send_work_id}");
        transaction.execute(
            "UPDATE provider_work_items
             SET state = 'cancelled', completed_at = ?2, cancel_requested = 1,
                 last_error_code = 'uncertain_send_resolved',
                 auth_block_reason = NULL, retry_after_at = NULL
             WHERE id = ?1 AND state IN (
               'queued', 'retry_wait', 'rate_limited', 'authentication_blocked'
             )",
            params![reconciliation_id, now],
        )?;
        transaction.execute(
            "UPDATE provider_work_items
             SET cancel_requested = 1, last_error_code = 'uncertain_send_resolved'
             WHERE id = ?1 AND state = 'executing' AND kind = 'sync'",
            [reconciliation_id],
        )?;
        transaction.commit()?;
        Ok(OperationSummary {
            id: operation.id,
            thread_id: operation.thread_id,
            field: operation.field,
            kind: operation.kind,
            state: "cancelled".into(),
            not_before: operation.not_before,
        })
    }

    pub fn undo_operation(&mut self, operation_id: &str) -> Result<OperationSummary, StoreError> {
        let now = now_ms();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let operation = transaction
            .query_row(
                "SELECT rowid, id, thread_id, field, kind, old_value, new_value,
                        payload_json, state, not_before
                 FROM operations WHERE id = ?1",
                [operation_id],
                operation_from_row,
            )
            .optional()?
            .ok_or_else(|| StoreError::NotFound("Operation was not found".into()))?;
        if operation.field == "send" {
            if operation.state != "pending" {
                return Err(StoreError::Conflict(
                    "Send can only be undone before provider submission starts".into(),
                ));
            }
            let changed = transaction.execute(
                "UPDATE operations SET state = 'cancelled' WHERE id = ?1 AND state = 'pending'",
                [operation_id],
            )?;
            if changed != 1 {
                return Err(StoreError::Conflict(
                    "Send started before it could be undone".into(),
                ));
            }
            cancel_waiting_work(&transaction, operation_id, now)?;
            transaction.commit()?;
            return Ok(OperationSummary {
                id: operation.id,
                thread_id: operation.thread_id,
                field: operation.field,
                kind: operation.kind,
                state: "cancelled".into(),
                not_before: operation.not_before,
            });
        }
        if matches!(operation.field.as_str(), "snooze" | "rsvp") {
            return undo_confirmed_local_operation(transaction, operation, now);
        }
        if matches!(operation.state.as_str(), "pending" | "retrying") {
            let changed = transaction.execute(
                "UPDATE operations SET state = 'cancelled' WHERE id = ?1 AND state IN ('pending', 'retrying')",
                [operation_id],
            )?;
            if changed != 1 {
                return Err(StoreError::Conflict(
                    "Operation started before it could be undone".into(),
                ));
            }
            cancel_waiting_work(&transaction, operation_id, now)?;
            transaction.commit()?;
            return Ok(OperationSummary {
                id: operation.id,
                thread_id: operation.thread_id,
                field: operation.field,
                kind: operation.kind,
                state: "cancelled".into(),
                not_before: operation.not_before,
            });
        }
        if operation.state != "confirmed" {
            return Err(StoreError::Conflict(
                "That operation is no longer undoable".into(),
            ));
        }
        let thread_id = operation
            .thread_id
            .ok_or_else(|| StoreError::Conflict("Operation has no thread".into()))?;
        if operation.field == "provider_label" && operation.old_value.as_deref() == Some("2") {
            return Err(StoreError::Conflict(
                "A partially labeled thread cannot be restored exactly after confirmation".into(),
            ));
        }
        let current = current_operation_value(&transaction, &operation, thread_id)?;
        let expected = operation
            .new_value
            .as_deref()
            .unwrap_or_default()
            .parse::<i64>()
            .unwrap_or(-1);
        let later_exists = if operation.field == "provider_label" {
            let payload = operation
                .payload_json
                .as_deref()
                .map(serde_json::from_str::<ThreadMutationPayload>)
                .transpose()?
                .ok_or_else(|| StoreError::Conflict("Provider label snapshot is missing".into()))?;
            let remote_container_id = payload.remote_container_id.ok_or_else(|| {
                StoreError::Conflict("Provider label container identity is missing".into())
            })?;
            transaction.query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM operations
                   WHERE thread_id = ?1 AND field = 'provider_label' AND rowid > ?2
                     AND state NOT IN ('cancelled', 'failed', 'conflicted')
                     AND json_valid(payload_json)
                     AND json_extract(payload_json, '$.remoteContainerId') = ?3
                 )",
                params![thread_id, operation.row_id, remote_container_id],
                |row| row.get::<_, i64>(0),
            )? != 0
        } else {
            transaction.query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM operations
                   WHERE thread_id = ?1 AND field = ?2 AND rowid > ?3
                     AND state NOT IN ('cancelled', 'failed', 'conflicted')
                 )",
                params![thread_id, operation.field, operation.row_id],
                |row| row.get::<_, i64>(0),
            )? != 0
        };
        if current != expected || later_exists {
            transaction.execute(
                "UPDATE operations SET state = 'conflicted', error = 'State changed after this operation' WHERE id = ?1",
                [operation_id],
            )?;
            transaction.commit()?;
            return Err(StoreError::Conflict(
                "Cannot undo because the thread changed afterward".into(),
            ));
        }
        let id = new_id(&transaction, "op")?;
        let work_id = new_id(&transaction, "work")?;
        let previous_payload = operation
            .payload_json
            .as_deref()
            .map(serde_json::from_str::<ThreadMutationPayload>)
            .transpose()?;
        let target = match previous_payload.as_ref() {
            Some(payload) if payload.remote_thread_id.is_some() => {
                let account_id = payload.account_id.clone().ok_or_else(|| {
                    StoreError::Conflict("Remote mutation payload has no account identity".into())
                })?;
                ThreadMutationTarget {
                    scope: provider_mutation_scope(&transaction, &account_id)?,
                    account_id,
                    remote_thread_id: payload.remote_thread_id.clone(),
                }
            }
            _ => resolve_thread_mutation_target(&transaction, thread_id)?,
        };
        let undo_kind = format!("undo_{}", operation.kind);
        let payload_json = serde_json::to_string(&ThreadMutationPayload {
            format_version: target.remote_thread_id.as_ref().map(|_| 1),
            operation_id: id.clone(),
            account_id: target
                .remote_thread_id
                .as_ref()
                .map(|_| target.account_id.clone()),
            thread_id,
            field: operation.field.clone(),
            value: operation.old_value.clone().unwrap_or_default(),
            remote_thread_id: target.remote_thread_id,
            remote_container_id: previous_payload.and_then(|payload| payload.remote_container_id),
            undo_of: Some(operation.id.clone()),
        })?;
        transaction.execute(
            "INSERT INTO operations(
               id, thread_id, field, kind, old_value, new_value, state,
               created_at, not_before, undo_of, payload_json
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, 'pending', ?7, ?7, ?8, ?9)",
            params![
                id,
                thread_id,
                operation.field,
                undo_kind,
                operation.new_value,
                operation.old_value,
                now,
                operation.id,
                payload_json
            ],
        )?;
        enqueue_in_transaction(
            &transaction,
            NewWorkItem {
                id: work_id,
                account_id: target.account_id,
                operation_id: Some(id.clone()),
                kind: WorkKind::Mutation,
                scope: target.scope,
                ordering_key: id.clone(),
                payload_json,
                priority: 60,
                available_at: now,
                max_attempts: 8,
            },
            now,
        )
        .map_err(durable_work_error)?;
        transaction.commit()?;
        Ok(OperationSummary {
            id,
            thread_id: Some(thread_id),
            field: operation.field,
            kind: undo_kind,
            state: "pending".into(),
            not_before: now,
        })
    }

    pub fn process_due_operations(&mut self, limit: i64) -> Result<ProcessResult, StoreError> {
        let now = now_ms();
        let operations = self
            .connection
            .prepare(
                "SELECT rowid, id, thread_id, field, kind, old_value, new_value,
                        payload_json, state, not_before
                 FROM operations
                 WHERE state IN ('pending', 'retrying') AND not_before <= ?1
                 ORDER BY created_at, rowid
                 LIMIT ?2",
            )?
            .query_map(params![now, limit.clamp(1, 200)], operation_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        let mut confirmed = Vec::new();
        let mut failed = Vec::new();
        for operation in operations {
            let claimed = self.connection.execute(
                "UPDATE operations SET state = 'executing', attempts = attempts + 1, error = NULL
                 WHERE id = ?1 AND state IN ('pending', 'retrying')",
                [&operation.id],
            )?;
            if claimed != 1 {
                continue;
            }
            let outcome = if operation.field == "send" {
                self.confirm_send(&operation)
            } else {
                self.confirm_thread_action(&operation)
            };
            match outcome {
                Ok(true) => confirmed.push(operation.id),
                Ok(false) => failed.push(operation.id),
                Err(error) => {
                    self.connection.execute(
                        "UPDATE operations SET state = 'failed', error = ?2 WHERE id = ?1 AND state = 'executing'",
                        params![operation.id, error.to_string()],
                    )?;
                    failed.push(operation.id);
                }
            }
        }
        Ok(ProcessResult {
            changed: !confirmed.is_empty() || !failed.is_empty(),
            confirmed,
            failed,
        })
    }

    pub fn get_draft(&self, draft_id: &str) -> Result<Option<DraftSummary>, StoreError> {
        let draft_id = bounded_text(draft_id.trim(), "draftId", 200)?;
        self.connection
            .query_row(
                "SELECT d.id, d.account_id, a.name, a.email, a.color,
                        d.recipients, d.cc_recipients, d.bcc_recipients,
                        d.subject, d.body, d.body_html,
                        d.reply_to_thread_id, d.updated_at, d.revision,
                        EXISTS(
                          SELECT 1 FROM operations o
                          WHERE o.field = 'send'
                            AND json_extract(o.payload_json, '$.draftId') = d.id
                            AND o.state IN ('pending', 'executing', 'retrying', 'outcome_unknown')
                        ) AS locked
                 FROM drafts d JOIN accounts a ON a.id = d.account_id
                 WHERE d.id = ?1",
                [draft_id],
                |row| {
                    Ok(DraftSummary {
                        id: row.get(0)?,
                        account_id: row.get(1)?,
                        account_name: row.get(2)?,
                        account_email: row.get(3)?,
                        account_color: row.get(4)?,
                        recipients: row.get(5)?,
                        cc_recipients: row.get(6)?,
                        bcc_recipients: row.get(7)?,
                        subject: row.get(8)?,
                        body: row.get(9)?,
                        body_html: row.get(10)?,
                        reply_to_thread_id: row.get(11)?,
                        updated_at: row.get(12)?,
                        revision: row.get(13)?,
                        locked: row.get::<_, i64>(14)? != 0,
                    })
                },
            )
            .optional()
            .map_err(StoreError::from)
    }

    fn draft_locked(&self, draft_id: &str) -> Result<bool, StoreError> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(
               SELECT 1 FROM operations
               WHERE field = 'send'
                 AND json_extract(payload_json, '$.draftId') = ?1
                 AND state IN ('pending', 'executing', 'retrying', 'outcome_unknown')
             )",
            [draft_id],
            |row| row.get::<_, i64>(0),
        )? != 0)
    }

    fn confirm_thread_action(&mut self, operation: &OperationRecord) -> Result<bool, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        apply_thread_projection(&transaction, operation)?;
        transaction.execute(
            "UPDATE operations SET state = 'confirmed', confirmed_at = ?2 WHERE id = ?1 AND state = 'executing'",
            params![operation.id, now_ms()],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    fn confirm_send(&mut self, operation: &OperationRecord) -> Result<bool, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let sent_at = now_ms();
        apply_send_projection(&transaction, operation, sent_at)?;
        transaction.execute(
            "UPDATE operations SET state = 'confirmed', confirmed_at = ?2
             WHERE id = ?1 AND state = 'executing'",
            params![operation.id, sent_at],
        )?;
        transaction.commit()?;
        Ok(true)
    }
}

fn undo_confirmed_local_operation(
    transaction: Transaction<'_>,
    operation: OperationRecord,
    now: i64,
) -> Result<OperationSummary, StoreError> {
    if operation.state != "confirmed" {
        return Err(StoreError::Conflict(format!(
            "Local operation cannot be undone from state {}",
            operation.state
        )));
    }
    let thread_id = operation
        .thread_id
        .ok_or_else(|| StoreError::Conflict("Local operation has no thread".into()))?;
    let later_exists = transaction.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM operations
           WHERE thread_id = ?1 AND field = ?2 AND rowid > ?3
             AND state NOT IN ('cancelled', 'failed', 'conflicted')
         )",
        params![thread_id, operation.field, operation.row_id],
        |row| row.get::<_, i64>(0),
    )? != 0;
    let current = match operation.field.as_str() {
        "snooze" => transaction
            .query_row(
                "SELECT CAST(wake_at AS TEXT) FROM snoozes WHERE thread_id = ?1",
                [thread_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?,
        "rsvp" => transaction
            .query_row(
                "SELECT response FROM invitations WHERE thread_id = ?1",
                [thread_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?,
        _ => {
            return Err(StoreError::Conflict(
                "Unsupported local operation field".into(),
            ))
        }
    };
    if later_exists || current != operation.new_value {
        return Err(StoreError::Conflict(format!(
            "Cannot undo {} because the thread changed afterward",
            operation.field
        )));
    }

    match operation.field.as_str() {
        "snooze" => match operation.old_value.as_deref() {
            Some(value) => {
                let wake_at = value.parse::<i64>().map_err(|_| {
                    StoreError::Conflict("Snooze journal contains an invalid wake time".into())
                })?;
                transaction.execute(
                    "UPDATE snoozes SET wake_at = ?1, created_at = ?2
                     WHERE thread_id = ?3",
                    params![wake_at, now, thread_id],
                )?;
            }
            None => {
                transaction.execute("DELETE FROM snoozes WHERE thread_id = ?1", [thread_id])?;
            }
        },
        "rsvp" => {
            let previous = operation.old_value.as_deref().ok_or_else(|| {
                StoreError::Conflict("RSVP journal is missing its prior response".into())
            })?;
            let changed = transaction.execute(
                "UPDATE invitations SET response = ?1 WHERE thread_id = ?2",
                params![previous, thread_id],
            )?;
            if changed != 1 {
                return Err(StoreError::Conflict("Invitation was not found".into()));
            }
        }
        _ => unreachable!("local operation field was validated"),
    }
    let changed = transaction.execute(
        "UPDATE operations SET state = 'cancelled'
         WHERE id = ?1 AND state = 'confirmed'",
        [&operation.id],
    )?;
    if changed != 1 {
        return Err(StoreError::Conflict(
            "Local operation state changed during undo".into(),
        ));
    }
    transaction.commit()?;
    Ok(OperationSummary {
        id: operation.id,
        thread_id: Some(thread_id),
        field: operation.field,
        kind: operation.kind,
        state: "cancelled".into(),
        not_before: operation.not_before,
    })
}

pub(crate) fn apply_worker_projection(
    transaction: &Transaction<'_>,
    work: &ClaimedWork,
    projection: &WorkerProjection,
) -> Result<(), WorkerError> {
    if !matches!(projection, WorkerProjection::LocalOperation) {
        return Err(WorkerError::Conflict(
            "Provider batch projection is not wired into the local fake adapter".into(),
        ));
    }
    let operation_id = work.operation_id.as_deref().ok_or_else(|| {
        WorkerError::Conflict("Fake-provider work is missing its linked operation".into())
    })?;
    let operation = transaction
        .query_row(
            "SELECT rowid, id, thread_id, field, kind, old_value, new_value,
                    payload_json, state, not_before
             FROM operations WHERE id = ?1 AND state = 'executing'",
            [operation_id],
            operation_from_row,
        )
        .optional()?
        .ok_or(WorkerError::LeaseLost)?;

    if operation.field == "send" {
        if operation.payload_json.as_deref() != Some(work.payload_json.as_str()) {
            return Err(WorkerError::Conflict(
                "Linked send snapshot no longer matches durable provider work".into(),
            ));
        }
        apply_send_projection(transaction, &operation, now_ms())
            .map_err(|error| WorkerError::Conflict(format!("Could not project sent mail: {error}")))
    } else {
        let payload = serde_json::from_str::<ThreadMutationPayload>(&work.payload_json)
            .map_err(|_| WorkerError::Conflict("Durable mutation payload is invalid".into()))?;
        if payload.operation_id != operation.id
            || payload.thread_id != operation.thread_id.unwrap_or_default()
            || payload.field != operation.field
            || Some(payload.value.as_str()) != operation.new_value.as_deref()
            || payload
                .account_id
                .as_deref()
                .is_some_and(|account_id| account_id != work.account_id)
        {
            return Err(WorkerError::Conflict(
                "Linked mutation no longer matches durable provider work".into(),
            ));
        }
        if operation.payload_json.as_deref().is_some_and(|value| {
            serde_json::from_str::<serde_json::Value>(value).ok()
                != serde_json::from_str::<serde_json::Value>(&work.payload_json).ok()
        }) {
            return Err(WorkerError::Conflict(
                "Linked mutation snapshot changed after it was queued".into(),
            ));
        }
        if operation.field != "provider_label" {
            apply_thread_projection(transaction, &operation).map_err(|error| {
                WorkerError::Conflict(format!("Could not apply provider projection: {error}"))
            })?;
        }
        if let Some(remote_thread_id) = payload.remote_thread_id.as_deref() {
            apply_provider_thread_projection(
                transaction,
                &work.account_id,
                payload.thread_id,
                remote_thread_id,
                &payload.field,
                &payload.value,
                payload.remote_container_id.as_deref(),
            )
            .map_err(|error| {
                WorkerError::Conflict(format!("Could not apply provider memberships: {error}"))
            })?;
        }
        Ok(())
    }
}

pub(crate) fn apply_provider_send_acceptance(
    transaction: &Transaction<'_>,
    work: &ClaimedWork,
    acceptance: &ProviderSendAcceptance,
) -> Result<(), WorkerError> {
    let conflict = |message: &str| WorkerError::Conflict(message.into());
    if acceptance.provider_kind != "gmail"
        || !(0..=crate::outgoing::MAX_RFC3339_UNIX_MILLIS).contains(&acceptance.accepted_at_ms)
        || acceptance.remote_message_id.is_empty()
        || acceptance.remote_message_id.len() > 2_048
        || acceptance.remote_message_id.chars().any(char::is_control)
        || acceptance.remote_thread_id.is_empty()
        || acceptance.remote_thread_id.len() > 2_048
        || acceptance.remote_thread_id.chars().any(char::is_control)
        || acceptance.send_payload_fingerprint_hex.len() != 64
        || !acceptance
            .send_payload_fingerprint_hex
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(conflict("Provider send acceptance is outside its contract"));
    }
    let reconciliation = acceptance.reconciliation_work_id.as_deref();
    match (work.kind, reconciliation) {
        (WorkKind::Send, None) if work.id == acceptance.original_work_id => {}
        (WorkKind::Sync, Some(reconciliation_work_id))
            if reconciliation_work_id == work.id
                && work.scope == crate::gmail::gmail_send_scope(&acceptance.operation_id) => {}
        _ => {
            return Err(conflict(
                "Provider send acceptance does not match its submission or reconciliation work",
            ))
        }
    }
    let original = transaction
        .query_row(
            "SELECT send.account_id, send.operation_id, send.kind, send.scope,
                    send.state, lower(hex(send.payload_fingerprint)), send.payload_json,
                    operation.state, operation.payload_json
             FROM provider_work_items send
             JOIN operations operation ON operation.id = send.operation_id
             WHERE send.id = ?1",
            [&acceptance.original_work_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, Option<String>>(8)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| conflict("Original provider send work is missing"))?;
    let (
        account_id,
        operation_id,
        kind,
        scope,
        send_state,
        payload_fingerprint,
        payload_json,
        operation_state,
        operation_payload_json,
    ) = original;
    let expected_state = if reconciliation.is_some() {
        "outcome_unknown"
    } else {
        "executing"
    };
    if account_id != work.account_id
        || operation_id != acceptance.operation_id
        || kind != "send"
        || scope != crate::gmail::gmail_send_scope(&operation_id)
        || send_state != expected_state
        || operation_state != expected_state
        || payload_fingerprint != acceptance.send_payload_fingerprint_hex
        || operation_payload_json.as_deref() != Some(payload_json.as_str())
        || hex_digest(payload_json.as_bytes()) != acceptance.send_payload_fingerprint_hex
        || (reconciliation.is_none()
            && (work.operation_id.as_deref() != Some(operation_id.as_str())
                || work.payload_json != payload_json
                || work.payload_fingerprint_hex != payload_fingerprint))
    {
        return Err(conflict(
            "Provider send acceptance does not match the exact durable send snapshot",
        ));
    }
    if reconciliation.is_some() {
        let value: serde_json::Value = serde_json::from_str(&work.payload_json)
            .map_err(|_| conflict("Send reconciliation snapshot is malformed"))?;
        if value.get("provider").and_then(|value| value.as_str()) != Some("gmail")
            || value.get("originalWorkId").and_then(|value| value.as_str())
                != Some(acceptance.original_work_id.as_str())
            || value.get("operationId").and_then(|value| value.as_str())
                != Some(operation_id.as_str())
            || value
                .get("sendPayloadFingerprintHex")
                .and_then(|value| value.as_str())
                != Some(payload_fingerprint.as_str())
        {
            return Err(conflict(
                "Send reconciliation does not identify the exact outcome-unknown send",
            ));
        }
    }
    let operation = transaction
        .query_row(
            "SELECT rowid, id, thread_id, field, kind, old_value, new_value,
                    payload_json, state, not_before
             FROM operations WHERE id = ?1",
            [&operation_id],
            operation_from_row,
        )
        .optional()?
        .ok_or_else(|| conflict("Provider send operation is missing"))?;
    let payload: SendPayload = serde_json::from_str(&payload_json)
        .map_err(|_| conflict("Provider send snapshot is malformed"))?;
    send_projection_fields(transaction, &payload)
        .map_err(|error| conflict(&format!("Provider send snapshot is invalid: {error}")))?;
    if payload.snapshot_version != Some(3)
        || payload.provider_kind.as_deref() != Some("gmail")
        || payload
            .queued_at_ms
            .is_none_or(|queued_at_ms| acceptance.accepted_at_ms < queued_at_ms)
        || payload
            .remote_thread_id
            .as_deref()
            .is_some_and(|expected| expected != acceptance.remote_thread_id)
    {
        return Err(conflict(
            "Provider send acceptance violates the frozen provider target",
        ));
    }
    let provider_kind = transaction
        .query_row(
            "SELECT provider_kind FROM provider_accounts
             WHERE account_id = ?1",
            [&account_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| conflict("Provider send account is no longer configured"))?;
    if provider_kind != "gmail" {
        return Err(conflict("Provider send account kind changed"));
    }

    let existing_remote = transaction
        .query_row(
            "SELECT reference.message_id, message.thread_id,
                    message.internet_message_id, reference.remote_thread_id
             FROM provider_message_refs reference
             JOIN messages message ON message.id = reference.message_id
             JOIN threads thread ON thread.id = message.thread_id
             WHERE reference.account_id = ?1
               AND reference.remote_message_id = ?2
               AND thread.account_id = ?1",
            params![account_id, acceptance.remote_message_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()?;
    if let Some((_message_id, local_thread_id, observed_internet_id, remote_thread_id)) =
        existing_remote
    {
        let expected_internet_id = payload.submission_message_id.as_deref().unwrap_or_default();
        if crate::internet_message::canonicalize_message_id(&observed_internet_id).ok()
            != crate::internet_message::canonicalize_message_id(expected_internet_id).ok()
            || remote_thread_id.as_deref() != Some(acceptance.remote_thread_id.as_str())
            || payload
                .reply_to_thread_id
                .is_some_and(|expected| expected != local_thread_id)
        {
            return Err(conflict(
                "Existing provider message evidence conflicts with the accepted send",
            ));
        }
        delete_exact_send_draft(transaction, &payload)
            .map_err(|error| conflict(&format!("Could not consume sent draft: {error}")))?;
    } else {
        let draft_id = payload.draft_id.as_str();
        let draft_revision = payload
            .draft_revision
            .ok_or_else(|| conflict("Provider send snapshot has no draft revision"))?;
        let exact_draft = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM drafts WHERE id = ?1 AND revision = ?2)",
            params![draft_id, draft_revision],
            |row| row.get::<_, i64>(0),
        )? != 0;
        if !exact_draft {
            return Err(conflict("Frozen draft revision is unavailable"));
        }
        let previous_message_id =
            transaction.query_row("SELECT COALESCE(MAX(id), 0) FROM messages", [], |row| {
                row.get::<_, i64>(0)
            })?;
        apply_send_projection(transaction, &operation, acceptance.accepted_at_ms)
            .map_err(|error| conflict(&format!("Could not project accepted send: {error}")))?;
        let (local_message_id, local_thread_id) = transaction
            .query_row(
                "SELECT message.id, message.thread_id
                 FROM messages message
                 JOIN threads thread ON thread.id = message.thread_id
                 WHERE message.id > ?1 AND thread.account_id = ?2
                   AND message.internet_message_id = ?3
                 ORDER BY message.id LIMIT 1",
                params![
                    previous_message_id,
                    account_id,
                    payload.submission_message_id.as_deref().unwrap_or_default()
                ],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?
            .ok_or_else(|| conflict("Accepted local sent message was not projected"))?;
        let existing_thread = transaction
            .query_row(
                "SELECT thread_id FROM provider_thread_refs
                 WHERE account_id = ?1 AND remote_thread_id = ?2",
                params![account_id, acceptance.remote_thread_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        if existing_thread.is_some_and(|thread_id| thread_id != local_thread_id) {
            return Err(conflict(
                "Accepted provider thread is already mapped to another local thread",
            ));
        }
        transaction.execute(
            "INSERT OR IGNORE INTO provider_thread_refs(
               account_id, remote_thread_id, thread_id, revision
             ) VALUES(?1, ?2, ?3, NULL)",
            params![account_id, acceptance.remote_thread_id, local_thread_id],
        )?;
        transaction.execute(
            "INSERT INTO provider_message_refs(
               account_id, remote_message_id, message_id, remote_thread_id,
               revision, body_state, body_is_truncated
             ) VALUES(?1, ?2, ?3, ?4, NULL, 'normalized', 0)",
            params![
                account_id,
                acceptance.remote_message_id,
                local_message_id,
                acceptance.remote_thread_id
            ],
        )?;
    }

    if reconciliation.is_some() {
        let send_changed = transaction.execute(
            "UPDATE provider_work_items
             SET state = 'succeeded', completed_at = ?2, last_error_code = NULL,
                 auth_block_reason = NULL, retry_after_at = NULL
             WHERE id = ?1 AND state = 'outcome_unknown'
               AND lower(hex(payload_fingerprint)) = ?3",
            params![
                acceptance.original_work_id,
                acceptance.accepted_at_ms,
                acceptance.send_payload_fingerprint_hex
            ],
        )?;
        let operation_changed = transaction.execute(
            "UPDATE operations
             SET state = 'confirmed', confirmed_at = ?2, error = NULL
             WHERE id = ?1 AND state = 'outcome_unknown'",
            params![acceptance.operation_id, acceptance.accepted_at_ms],
        )?;
        if send_changed != 1 || operation_changed != 1 {
            return Err(conflict(
                "Outcome-unknown send changed while reconciliation was projected",
            ));
        }
    } else {
        let reconciliation_id = format!("reconcile_{}", acceptance.original_work_id);
        let cancelled = transaction.execute(
            "UPDATE provider_work_items
             SET state = 'cancelled', completed_at = ?2,
                 last_error_code = 'send_reconciliation_not_needed',
                 auth_block_reason = NULL, retry_after_at = NULL,
                 lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL
             WHERE id = ?1 AND account_id = ?3 AND scope = ?4
               AND state IN ('queued', 'retry_wait', 'rate_limited', 'authentication_blocked')",
            params![
                reconciliation_id,
                acceptance.accepted_at_ms,
                account_id,
                scope
            ],
        )?;
        if cancelled != 1 {
            return Err(conflict(
                "Dormant Gmail send reconciliation work is unavailable",
            ));
        }
    }
    Ok(())
}

fn delete_exact_send_draft(
    transaction: &Transaction<'_>,
    payload: &SendPayload,
) -> Result<(), StoreError> {
    let revision = payload
        .draft_revision
        .ok_or_else(|| StoreError::Validation("Send snapshot has no draft revision".into()))?;
    let changed = transaction.execute(
        "DELETE FROM drafts WHERE id = ?1 AND revision = ?2",
        params![payload.draft_id, revision],
    )?;
    if changed != 1 {
        return Err(StoreError::Conflict(
            "Frozen draft revision is unavailable".into(),
        ));
    }
    Ok(())
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn apply_missing_remote_thread_projection(
    transaction: &Transaction<'_>,
    work: &ClaimedWork,
    remote_thread_id: &str,
) -> Result<(), WorkerError> {
    let operation_id = work.operation_id.as_deref().ok_or_else(|| {
        WorkerError::Conflict("Remote-absence work is missing its linked operation".into())
    })?;
    let operation = transaction
        .query_row(
            "SELECT rowid, id, thread_id, field, kind, old_value, new_value,
                    payload_json, state, not_before
             FROM operations WHERE id = ?1 AND state = 'executing'",
            [operation_id],
            operation_from_row,
        )
        .optional()?
        .ok_or(WorkerError::LeaseLost)?;
    let payload = serde_json::from_str::<ThreadMutationPayload>(&work.payload_json)
        .map_err(|_| WorkerError::Conflict("Durable mutation payload is invalid".into()))?;
    if operation.payload_json.as_deref() != Some(work.payload_json.as_str())
        || payload.operation_id != operation.id
        || payload.account_id.as_deref() != Some(work.account_id.as_str())
        || payload.remote_thread_id.as_deref() != Some(remote_thread_id)
        || payload.thread_id != operation.thread_id.unwrap_or_default()
    {
        return Err(WorkerError::Conflict(
            "Remote-absence projection does not match the durable mutation".into(),
        ));
    }
    let thread_id = payload.thread_id;
    let mapping_matches = transaction.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM provider_thread_refs
           WHERE account_id = ?1 AND thread_id = ?2 AND remote_thread_id = ?3
         )",
        params![work.account_id, thread_id, remote_thread_id],
        |row| row.get::<_, i64>(0),
    )? != 0;
    if !mapping_matches {
        return Err(WorkerError::Conflict(
            "Remote thread identity changed before absence reconciliation".into(),
        ));
    }
    transaction.execute(
        "DELETE FROM provider_message_refs
         WHERE account_id = ?1 AND remote_thread_id = ?2",
        params![work.account_id, remote_thread_id],
    )?;
    transaction.execute(
        "DELETE FROM provider_thread_refs
         WHERE account_id = ?1 AND remote_thread_id = ?2 AND thread_id = ?3",
        params![work.account_id, remote_thread_id, thread_id],
    )?;
    let changed = transaction.execute(
        "UPDATE threads
         SET remote_deleted = 1, remote_in_inbox = 0, remote_unread = 0,
             remote_starred = 0, remote_trashed = 0
         WHERE id = ?1",
        [thread_id],
    )?;
    if changed != 1 {
        return Err(WorkerError::Conflict(
            "Remote-absence thread disappeared during projection".into(),
        ));
    }
    transaction.execute(
        "UPDATE messages SET remote_deleted = 1 WHERE thread_id = ?1",
        [thread_id],
    )?;
    Ok(())
}

fn apply_provider_thread_projection(
    transaction: &Transaction<'_>,
    account_id: &str,
    thread_id: i64,
    remote_thread_id: &str,
    field: &str,
    value: &str,
    remote_container_id: Option<&str>,
) -> Result<(), StoreError> {
    let mapping_matches = transaction.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM provider_thread_refs
           WHERE account_id = ?1 AND thread_id = ?2 AND remote_thread_id = ?3
         )",
        params![account_id, thread_id, remote_thread_id],
        |row| row.get::<_, i64>(0),
    )? != 0;
    if !mapping_matches {
        return Err(StoreError::Conflict(
            "Remote thread identity changed before mutation acknowledgement".into(),
        ));
    }
    let enabled = match value {
        "0" => false,
        "1" => true,
        _ => {
            return Err(StoreError::Validation(
                "Provider mutation value is invalid".into(),
            ))
        }
    };
    let label = match field {
        "in_inbox" => Some("INBOX"),
        "unread" => Some("UNREAD"),
        "starred" => Some("STARRED"),
        "trashed" => Some("TRASH"),
        "provider_label" => Some(remote_container_id.ok_or_else(|| {
            StoreError::Validation("Provider label mutation has no container identity".into())
        })?),
        _ => None,
    };
    if let Some(label) = label {
        if enabled {
            transaction.execute(
                "INSERT OR IGNORE INTO provider_container_memberships(
                   account_id, remote_message_id, remote_container_id
                 )
                 SELECT reference.account_id, reference.remote_message_id, ?4
                 FROM provider_message_refs reference
                 JOIN messages message ON message.id = reference.message_id
                 WHERE reference.account_id = ?1
                   AND reference.remote_thread_id = ?2
                   AND message.thread_id = ?3",
                params![account_id, remote_thread_id, thread_id, label],
            )?;
        } else {
            transaction.execute(
                "DELETE FROM provider_container_memberships
                 WHERE account_id = ?1 AND remote_container_id = ?4
                   AND remote_message_id IN (
                     SELECT reference.remote_message_id
                     FROM provider_message_refs reference
                     JOIN messages message ON message.id = reference.message_id
                     WHERE reference.account_id = ?1
                       AND reference.remote_thread_id = ?2
                       AND message.thread_id = ?3
                   )",
                params![account_id, remote_thread_id, thread_id, label],
            )?;
        }
    }
    if matches!(field, "unread" | "starred") {
        let keyword = if field == "unread" {
            "unread"
        } else {
            "starred"
        };
        if enabled {
            transaction.execute(
                "INSERT OR IGNORE INTO provider_message_keywords(account_id, remote_message_id, keyword)
                 SELECT reference.account_id, reference.remote_message_id, ?4
                 FROM provider_message_refs reference
                 JOIN messages message ON message.id = reference.message_id
                 WHERE reference.account_id = ?1
                   AND reference.remote_thread_id = ?2
                   AND message.thread_id = ?3",
                params![account_id, remote_thread_id, thread_id, keyword],
            )?;
        } else {
            transaction.execute(
                "DELETE FROM provider_message_keywords
                 WHERE account_id = ?1 AND keyword = ?4
                   AND remote_message_id IN (
                     SELECT reference.remote_message_id
                     FROM provider_message_refs reference
                     JOIN messages message ON message.id = reference.message_id
                     WHERE reference.account_id = ?1
                       AND reference.remote_thread_id = ?2
                       AND message.thread_id = ?3
                   )",
                params![account_id, remote_thread_id, thread_id, keyword],
            )?;
        }
    }
    if field == "trashed" && enabled {
        apply_provider_thread_projection(
            transaction,
            account_id,
            thread_id,
            remote_thread_id,
            "in_inbox",
            "0",
            None,
        )?;
    }
    crate::provider_ingest::derive_thread_projection(transaction, thread_id)
}

fn apply_thread_projection(
    transaction: &Transaction<'_>,
    operation: &OperationRecord,
) -> Result<(), StoreError> {
    let thread_id = operation
        .thread_id
        .ok_or_else(|| StoreError::Validation("Thread operation has no thread".into()))?;
    let column = match operation.field.as_str() {
        "in_inbox" => "remote_in_inbox",
        "unread" => "remote_unread",
        "starred" => "remote_starred",
        "trashed" => "remote_trashed",
        _ => return Err(StoreError::Validation("Unknown operation field".into())),
    };
    let value = operation
        .new_value
        .as_deref()
        .ok_or_else(|| StoreError::Validation("Operation has no value".into()))?
        .parse::<i64>()
        .map_err(|_| StoreError::Validation("Operation value is invalid".into()))?;
    let changed = transaction.execute(
        &format!("UPDATE threads SET {column} = ?1 WHERE id = ?2"),
        params![value, thread_id],
    )?;
    if changed != 1 {
        return Err(StoreError::NotFound("Thread was not found".into()));
    }
    Ok(())
}

#[allow(clippy::type_complexity)]
fn send_projection_fields(
    transaction: &Transaction<'_>,
    payload: &SendPayload,
) -> Result<
    (
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        String,
        Option<i64>,
        Option<i64>,
    ),
    StoreError,
> {
    if let (
        Some(account_id),
        Some(sender_email),
        Some(recipients),
        Some(cc_recipients),
        Some(bcc_recipients),
        Some(subject),
        Some(body),
        Some(body_html),
        Some(submission_message_id),
        Some(draft_revision),
        Some(content_fingerprint_hex),
    ) = (
        payload.account_id.as_ref(),
        payload.sender_email.as_ref(),
        payload.recipients.as_ref(),
        payload.cc_recipients.as_ref(),
        payload.bcc_recipients.as_ref(),
        payload.subject.as_ref(),
        payload.body.as_ref(),
        payload.body_html.as_ref(),
        payload.submission_message_id.as_ref(),
        payload.draft_revision,
        payload.content_fingerprint_hex.as_ref(),
    ) {
        let expected = match payload.snapshot_version {
            Some(3) => {
                let queued_at_ms = payload.queued_at_ms.ok_or_else(|| {
                    StoreError::Validation("Durable send snapshot date is missing".into())
                })?;
                let client_correlation_id =
                    payload.client_correlation_id.as_deref().ok_or_else(|| {
                        StoreError::Validation("Durable send correlation is missing".into())
                    })?;
                let provider_kind = payload.provider_kind.as_deref().ok_or_else(|| {
                    StoreError::Validation("Durable send provider is missing".into())
                })?;
                send_content_fingerprint_v3(
                    submission_message_id,
                    account_id,
                    sender_email,
                    recipients,
                    cc_recipients,
                    bcc_recipients,
                    subject,
                    body,
                    body_html,
                    payload.reply_to_thread_id,
                    draft_revision,
                    queued_at_ms,
                    payload.in_reply_to.as_deref(),
                    &payload.references,
                    client_correlation_id,
                    provider_kind,
                    payload.remote_thread_id.as_deref(),
                )
            }
            Some(2) => {
                let queued_at_ms = payload.queued_at_ms.ok_or_else(|| {
                    StoreError::Validation("Durable send snapshot date is missing".into())
                })?;
                send_content_fingerprint_v2(
                    submission_message_id,
                    account_id,
                    sender_email,
                    recipients,
                    cc_recipients,
                    bcc_recipients,
                    subject,
                    body,
                    body_html,
                    payload.reply_to_thread_id,
                    draft_revision,
                    queued_at_ms,
                    payload.in_reply_to.as_deref(),
                    &payload.references,
                )
            }
            None | Some(1) => send_content_fingerprint(
                submission_message_id,
                account_id,
                sender_email,
                recipients,
                cc_recipients,
                bcc_recipients,
                subject,
                body,
                body_html,
                payload.reply_to_thread_id,
                draft_revision,
            ),
            Some(_) => {
                return Err(StoreError::Validation(
                    "Durable send snapshot version is unsupported".into(),
                ))
            }
        };
        let threading_is_valid = payload
            .in_reply_to
            .as_deref()
            .is_none_or(|value| crate::internet_message::validate_message_id(value).is_ok())
            && crate::internet_message::validate_references(&payload.references).is_ok();
        if crate::internet_message::validate_message_id(submission_message_id).is_err()
            || !threading_is_valid
            || &expected != content_fingerprint_hex
        {
            return Err(StoreError::Validation(
                "Durable send snapshot identity is invalid".into(),
            ));
        }
        return Ok((
            account_id.clone(),
            sender_email.clone(),
            recipients.clone(),
            cc_recipients.clone(),
            bcc_recipients.clone(),
            subject.clone(),
            body.clone(),
            body_html.clone(),
            payload.reply_to_thread_id,
            Some(draft_revision),
        ));
    }

    transaction
        .query_row(
            "SELECT d.account_id, a.email, d.recipients, d.cc_recipients,
                    d.bcc_recipients, d.subject, d.body, d.body_html,
                    d.reply_to_thread_id, d.revision
             FROM drafts d JOIN accounts a ON a.id = d.account_id
             WHERE d.id = ?1",
            [&payload.draft_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| StoreError::Conflict("Draft disappeared before send confirmation".into()))
}

fn apply_send_projection(
    transaction: &Transaction<'_>,
    operation: &OperationRecord,
    sent_at: i64,
) -> Result<(), StoreError> {
    let payload: SendPayload = serde_json::from_str(
        operation
            .payload_json
            .as_deref()
            .ok_or_else(|| StoreError::Validation("Send payload is missing".into()))?,
    )?;
    let (
        account_id,
        sender_email,
        recipients,
        cc_recipients,
        bcc_recipients,
        subject,
        body,
        body_html,
        reply_to,
        draft_revision,
    ) = send_projection_fields(transaction, &payload)?;
    let internet_message_id = payload.submission_message_id.as_deref().unwrap_or_default();
    if !internet_message_id.is_empty() {
        crate::internet_message::validate_message_id(internet_message_id)
            .map_err(|()| StoreError::Validation("Durable send Message-ID is invalid".into()))?;
    }
    let in_reply_to = payload.in_reply_to.as_deref().unwrap_or_default();
    if !in_reply_to.is_empty() {
        crate::internet_message::validate_message_id(in_reply_to)
            .map_err(|()| StoreError::Validation("Durable In-Reply-To is invalid".into()))?;
    }
    crate::internet_message::validate_references(&payload.references)
        .map_err(|()| StoreError::Validation("Durable References are invalid".into()))?;
    let references_json = serde_json::to_string(&payload.references)?;
    if let Some((message_id, _)) =
        observed_imap_sent_message(transaction, &payload, &account_id, internet_message_id)?
    {
        // The provider page is already the confirmed projection. SMTP success
        // confirms the pending send intent, but must not overwrite IMAP's
        // normalized body, timestamp, headers, or restricted-content sidecars.
        // Bcc is the one local envelope-only supplement because correct SMTP
        // MIME does not disclose it to IMAP.
        transaction.execute(
            "UPDATE messages SET bcc_recipients = ?2 WHERE id = ?1",
            params![message_id, bcc_recipients],
        )?;
        match draft_revision {
            Some(revision) => {
                transaction.execute(
                    "DELETE FROM drafts WHERE id = ?1 AND revision = ?2",
                    params![payload.draft_id, revision],
                )?;
            }
            None => {
                transaction.execute("DELETE FROM drafts WHERE id = ?1", [&payload.draft_id])?;
            }
        }
        return Ok(());
    }
    let preview = snippet(&body);
    let indexed_thread_id = if let Some(thread_id) = reply_to {
        let thread_exists = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM threads WHERE id = ?1 AND account_id = ?2)",
            params![thread_id, account_id],
            |row| row.get::<_, i64>(0),
        )? != 0;
        if !thread_exists {
            return Err(StoreError::NotFound("Reply thread was not found".into()));
        }
        transaction.execute(
            "INSERT INTO messages(
               thread_id, sender_name, sender_email, recipients, cc_recipients,
               bcc_recipients, sent_at, body_text, body_html, is_from_me,
               internet_message_id, in_reply_to, references_json
             ) VALUES(
               ?1, 'Me', ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, ?9, ?10, ?11
             )",
            params![
                thread_id,
                sender_email,
                recipients,
                cc_recipients,
                bcc_recipients,
                sent_at,
                body,
                body_html,
                internet_message_id,
                in_reply_to,
                references_json,
            ],
        )?;
        transaction.execute(
            "UPDATE threads
             SET snippet = ?1, latest_at = ?2, message_count = message_count + 1,
                 has_from_me = 1, remote_unread = 0
             WHERE id = ?3",
            params![preview, sent_at, thread_id],
        )?;
        thread_id
    } else {
        transaction.execute(
            "INSERT INTO threads(
               account_id, subject, participants, snippet, latest_at, message_count,
               remote_in_inbox, remote_unread, remote_starred, has_attachment,
               has_invite, has_link, has_from_me, category, attachment_names
             ) VALUES(?1, ?2, ?3, ?4, ?5, 1, 0, 0, 0, 0, 0, 0, 1, 'Sent', '')",
            params![
                account_id,
                subject,
                join_visible_recipients(&recipients, &cc_recipients),
                preview,
                sent_at
            ],
        )?;
        let thread_id = transaction.last_insert_rowid();
        transaction.execute(
            "INSERT INTO messages(
               thread_id, sender_name, sender_email, recipients, cc_recipients,
               bcc_recipients, sent_at, body_text, body_html, is_from_me,
               internet_message_id, in_reply_to, references_json
             ) VALUES(
               ?1, 'Me', ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, ?9, ?10, ?11
             )",
            params![
                thread_id,
                sender_email,
                recipients,
                cc_recipients,
                bcc_recipients,
                sent_at,
                body,
                body_html,
                internet_message_id,
                in_reply_to,
                references_json,
            ],
        )?;
        thread_id
    };
    crate::provider_ingest::rebuild_search_index_for_thread(transaction, indexed_thread_id)?;
    match draft_revision {
        Some(revision) => {
            transaction.execute(
                "DELETE FROM drafts WHERE id = ?1 AND revision = ?2",
                params![payload.draft_id, revision],
            )?;
        }
        None => {
            transaction.execute("DELETE FROM drafts WHERE id = ?1", [&payload.draft_id])?;
        }
    }
    Ok(())
}

/// Finds the exact IMAP Sent observation that won the race with SMTP success.
/// Both identities are required because Message-ID is forgeable and correlation
/// alone is not a public mail identity. The provider reference persists the
/// correlation even when that UID is unchanged on later IMAP scans.
fn observed_imap_sent_message(
    transaction: &Transaction<'_>,
    payload: &SendPayload,
    account_id: &str,
    internet_message_id: &str,
) -> Result<Option<(i64, i64)>, StoreError> {
    if payload.snapshot_version != Some(3)
        || payload.provider_kind.as_deref() != Some("imap")
        || internet_message_id.is_empty()
    {
        return Ok(None);
    }
    let Some(correlation) = payload.client_correlation_id.as_deref() else {
        return Ok(None);
    };
    let mut statement = transaction.prepare(
        "SELECT message.id, message.thread_id
         FROM provider_message_refs reference
         JOIN provider_accounts account ON account.account_id = reference.account_id
         JOIN messages message ON message.id = reference.message_id
         JOIN threads thread ON thread.id = message.thread_id
         WHERE reference.account_id = ?1
           AND account.provider_kind = 'imap'
           AND thread.account_id = ?1
           AND reference.client_correlation_id = ?2
           AND message.internet_message_id = ?3
           AND message.is_from_me = 1 AND message.remote_deleted = 0
           AND EXISTS(
             SELECT 1
             FROM provider_container_memberships membership
             JOIN provider_containers container
               ON container.account_id = membership.account_id
              AND container.remote_id = membership.remote_container_id
             WHERE membership.account_id = reference.account_id
               AND membership.remote_message_id = reference.remote_message_id
               AND container.role = 'sent' AND container.is_deleted = 0
           )
         ORDER BY message.id LIMIT 2",
    )?;
    let candidates = statement
        .query_map(
            params![account_id, correlation, internet_message_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    match candidates.as_slice() {
        [] => Ok(None),
        [candidate] => Ok(Some(*candidate)),
        _ => Err(StoreError::Conflict(
            "SMTP sent-message confirmation is ambiguous".into(),
        )),
    }
}

fn operation_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<OperationRecord> {
    Ok(OperationRecord {
        row_id: row.get(0)?,
        id: row.get(1)?,
        thread_id: row.get(2)?,
        field: row.get(3)?,
        kind: row.get(4)?,
        old_value: row.get(5)?,
        new_value: row.get(6)?,
        payload_json: row.get(7)?,
        state: row.get(8)?,
        not_before: row.get(9)?,
    })
}

fn new_id(connection: &Connection, prefix: &str) -> Result<String, StoreError> {
    let suffix = connection.query_row("SELECT lower(hex(randomblob(12)))", [], |row| {
        row.get::<_, String>(0)
    })?;
    Ok(format!("{prefix}_{suffix}"))
}

fn durable_work_error(error: WorkerError) -> StoreError {
    StoreError::Conflict(format!(
        "Durable operation journal rejected the change: {error}"
    ))
}

struct ThreadMutationTarget {
    account_id: String,
    remote_thread_id: Option<String>,
    scope: String,
}

fn provider_mutation_scope(
    transaction: &Transaction<'_>,
    account_id: &str,
) -> Result<String, StoreError> {
    let provider = transaction
        .query_row(
            "SELECT provider_kind FROM provider_accounts WHERE account_id = ?1",
            [account_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| {
            StoreError::Conflict("Remote mutation account is no longer configured".into())
        })?;
    if provider == "gmail" {
        return Ok(crate::gmail::GMAIL_ACCOUNT_SCOPE.into());
    }
    if !provider_capability_enabled(transaction, account_id, "mutations")? {
        return Err(StoreError::Conflict(format!(
            "The {provider} account is read-only"
        )));
    }
    Ok(format!("sync:{provider}:account:v1"))
}

fn resolve_thread_mutation_target(
    transaction: &Transaction<'_>,
    thread_id: i64,
) -> Result<ThreadMutationTarget, StoreError> {
    let target = transaction
        .query_row(
            "SELECT thread.account_id, account.provider_kind, reference.remote_thread_id
             FROM threads thread
             LEFT JOIN provider_accounts account
               ON account.account_id = thread.account_id
             LEFT JOIN provider_thread_refs reference
               ON reference.account_id = thread.account_id
              AND reference.thread_id = thread.id
             WHERE thread.id = ?1 AND thread.remote_deleted = 0",
            [thread_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| StoreError::NotFound("Thread was not found".into()))?;
    match target {
        (account_id, None, None) => Ok(ThreadMutationTarget {
            account_id,
            remote_thread_id: None,
            scope: format!("thread:v1:{thread_id}"),
        }),
        (account_id, Some(provider), Some(remote_thread_id)) if provider == "gmail" => {
            Ok(ThreadMutationTarget {
                account_id,
                remote_thread_id: Some(remote_thread_id),
                scope: crate::gmail::GMAIL_ACCOUNT_SCOPE.into(),
            })
        }
        (_, Some(provider), None) => Err(StoreError::Conflict(format!(
            "The {provider} thread has no stable remote identity"
        ))),
        (account_id, Some(_), Some(remote_thread_id)) => {
            let scope = provider_mutation_scope(transaction, &account_id)?;
            Ok(ThreadMutationTarget {
                account_id,
                remote_thread_id: Some(remote_thread_id),
                scope,
            })
        }
        (_, None, Some(_)) => Err(StoreError::Conflict(
            "A remote thread identity has no configured provider account".into(),
        )),
    }
}

fn provider_capability_enabled(
    transaction: &Transaction<'_>,
    account_id: &str,
    capability: &str,
) -> Result<bool, StoreError> {
    transaction
        .query_row(
            "SELECT EXISTS(
               SELECT 1 FROM provider_capabilities
               WHERE account_id = ?1 AND capability = ?2 AND enabled = 1
             )",
            params![account_id, capability],
            |row| row.get::<_, i64>(0),
        )
        .map(|value| value != 0)
        .map_err(StoreError::from)
}

fn resolve_send_provider_target(
    transaction: &Transaction<'_>,
    draft: &DraftSummary,
) -> Result<(String, Option<String>), StoreError> {
    let provider_kind = transaction
        .query_row(
            "SELECT COALESCE(provider.provider_kind, account.provider)
             FROM accounts account
             LEFT JOIN provider_accounts provider
               ON provider.account_id = account.id
             WHERE account.id = ?1",
            [&draft.account_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::Conflict("Draft account is unavailable".into()))?;
    let remote_thread_id = if provider_kind == "gmail" {
        draft
            .reply_to_thread_id
            .map(|thread_id| {
                transaction
                    .query_row(
                        "SELECT reference.remote_thread_id
                         FROM provider_thread_refs reference
                         JOIN threads thread ON thread.id = reference.thread_id
                         WHERE reference.account_id = ?1
                           AND reference.thread_id = ?2
                           AND thread.account_id = ?1
                           AND thread.remote_deleted = 0",
                        params![draft.account_id, thread_id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
            })
            .transpose()?
            .flatten()
    } else {
        None
    };
    Ok((provider_kind, remote_thread_id))
}

fn current_operation_value(
    transaction: &Transaction<'_>,
    operation: &OperationRecord,
    thread_id: i64,
) -> Result<i64, StoreError> {
    if operation.field == "provider_label" {
        let payload = serde_json::from_str::<ThreadMutationPayload>(
            operation
                .payload_json
                .as_deref()
                .ok_or_else(|| StoreError::Conflict("Provider label snapshot is missing".into()))?,
        )?;
        let account_id = payload
            .account_id
            .as_deref()
            .ok_or_else(|| StoreError::Conflict("Provider label account is missing".into()))?;
        let remote_thread_id = payload.remote_thread_id.as_deref().ok_or_else(|| {
            StoreError::Conflict("Provider label thread identity is missing".into())
        })?;
        let remote_container_id = payload.remote_container_id.as_deref().ok_or_else(|| {
            StoreError::Conflict("Provider label container identity is missing".into())
        })?;
        return transaction
            .query_row(
                "SELECT label_state
                 FROM provider_thread_label_effective
                 WHERE account_id = ?1 AND remote_thread_id = ?2
                   AND thread_id = ?3 AND remote_container_id = ?4",
                params![account_id, remote_thread_id, thread_id, remote_container_id],
                |row| row.get(0),
            )
            .map_err(StoreError::from);
    }
    let column = match operation.field.as_str() {
        "in_inbox" | "unread" | "starred" | "trashed" => operation.field.as_str(),
        _ => {
            return Err(StoreError::Conflict(
                "Operation field is not undoable".into(),
            ))
        }
    };
    transaction
        .query_row(
            &format!("SELECT {column} FROM thread_effective WHERE id = ?1"),
            [thread_id],
            |row| row.get(0),
        )
        .map_err(StoreError::from)
}

fn cancel_waiting_work(
    transaction: &Transaction<'_>,
    operation_id: &str,
    now: i64,
) -> Result<(), StoreError> {
    transaction.execute(
        "UPDATE provider_work_items
         SET state = 'cancelled', completed_at = ?2,
             last_error_code = 'cancelled_before_execution',
             retry_after_at = NULL
         WHERE operation_id = ?1
           AND state IN ('queued', 'retry_wait', 'rate_limited', 'authentication_blocked')",
        params![operation_id, now],
    )?;
    Ok(())
}

mod demo;
mod stats;
mod validation;

pub use stats::{AddressCount, DayVolume, MailStats, MailStatsInput};

use validation::*;
// Reached from the Gmail adapter when it recomputes a send fingerprint.
pub(crate) use validation::send_content_fingerprint_v3;
mod schema;

use schema::{ensure_durable_work_for_pending_operations, migrate, recover_interrupted_operations};

use demo::{
    seed_demo_account_signatures, seed_demo_mailbox, seed_demo_threading_headers,
    seed_long_demo_threads, seed_safe_content_demo,
};

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn thread_summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ThreadSummary> {
    Ok(ThreadSummary {
        id: row.get(0)?,
        account_id: row.get(1)?,
        subject: row.get(2)?,
        participants: row.get(3)?,
        snippet: row.get(4)?,
        latest_at: row.get(5)?,
        message_count: row.get(6)?,
        in_inbox: row.get::<_, i64>(7)? != 0,
        unread: row.get::<_, i64>(8)? != 0,
        starred: row.get::<_, i64>(9)? != 0,
        has_from_me: row.get::<_, i64>(10)? != 0,
        category: row.get(11)?,
    })
}

fn message_summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageSummary> {
    Ok(MessageSummary {
        id: row.get(0)?,
        thread_id: row.get(1)?,
        sender_name: row.get(2)?,
        sender_email: row.get(3)?,
        recipients: row.get(4)?,
        cc_recipients: row.get(5)?,
        bcc_recipients: row.get(6)?,
        sent_at: row.get(7)?,
        body_text: row.get(8)?,
        body_html: row.get(9)?,
        blocked_remote_resources: row.get(10)?,
        remote_images: Vec::new(),
        is_from_me: row.get::<_, i64>(11)? != 0,
    })
}

fn append_mailbox_view(
    conditions: &mut Vec<String>,
    values: &mut Vec<Value>,
    view: &str,
    now: i64,
) -> Result<(), StoreError> {
    match view {
        "all" => conditions.push("e.trashed = 0".into()),
        "inbox" => {
            conditions.push(
                "e.trashed = 0 AND e.in_inbox = 1 AND NOT EXISTS (
                   SELECT 1 FROM snoozes s
                   WHERE s.thread_id = e.id AND s.wake_at > ?
                 )"
                .into(),
            );
            values.push(Value::Integer(now));
        }
        "archive" => {
            conditions.push(
                "e.trashed = 0 AND e.in_inbox = 0 AND NOT EXISTS (
                   SELECT 1 FROM snoozes s
                   WHERE s.thread_id = e.id AND s.wake_at > ?
                 )"
                .into(),
            );
            values.push(Value::Integer(now));
        }
        "snoozed" => {
            conditions.push(
                "e.trashed = 0 AND EXISTS (
                   SELECT 1 FROM snoozes s
                   WHERE s.thread_id = e.id AND s.wake_at > ?
                 )"
                .into(),
            );
            values.push(Value::Integer(now));
        }
        "starred" => conditions.push("e.trashed = 0 AND e.starred = 1".into()),
        "sent" => conditions.push("e.trashed = 0 AND e.has_from_me = 1".into()),
        "trash" => conditions.push("e.trashed = 1".into()),
        "drafts" => {
            return Err(StoreError::Validation(
                "Drafts are not part of the thread list".into(),
            ))
        }
        other => {
            return Err(StoreError::Validation(format!(
                "Unsupported mailbox view '{other}'"
            )))
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::demo::DEMO_FIXTURE_THREAD_ID;
    use super::*;
    use crate::provider::{OpaqueSyncCursor, ProviderBatch};
    use crate::provider_ingest::{
        provider_batch_fingerprint_v1, provider_batch_fingerprint_v2, ProviderBatchFailpoint,
    };
    use crate::worker::{DurableWorker, WorkerConfig, WorkerOutcome};
    use tempfile::tempdir;

    const EXACT_LEGACY_PROVIDER_ACCOUNTS_SCHEMA: &str = "CREATE TABLE provider_accounts (
           account_id TEXT PRIMARY KEY REFERENCES accounts(id) ON DELETE CASCADE,
           provider_kind TEXT NOT NULL CHECK(length(provider_kind) BETWEEN 1 AND 100),
           remote_account_id TEXT NOT NULL CHECK(length(remote_account_id) BETWEEN 1 AND 2000),
           auth_state TEXT NOT NULL DEFAULT 'locked' CHECK(auth_state IN (
             'locked', 'ready', 'expired', 'requires_action', 'unavailable'
           )),
           sync_state TEXT NOT NULL DEFAULT 'idle' CHECK(sync_state IN (
             'idle', 'syncing', 'offline', 'error'
           )),
           last_error_code TEXT CHECK(last_error_code IS NULL OR length(last_error_code) <= 200),
           last_sync_at INTEGER CHECK(last_sync_at IS NULL OR last_sync_at >= 0),
           created_at INTEGER NOT NULL CHECK(created_at >= 0),
           updated_at INTEGER NOT NULL CHECK(updated_at >= 0)
         );";

    fn install_exact_legacy_provider_accounts_schema(
        connection: &Connection,
        add_v11_columns: bool,
    ) {
        connection
            .execute_batch("PRAGMA foreign_keys = OFF; DROP TABLE provider_accounts;")
            .expect("remove the current parent table without cascading provider projections");
        connection
            .execute_batch(EXACT_LEGACY_PROVIDER_ACCOUNTS_SCHEMA)
            .expect("install the exact legacy provider account schema");
        if add_v11_columns {
            connection
                .execute_batch(
                    "ALTER TABLE provider_accounts
                       ADD COLUMN credential_ref TEXT CHECK(
                         credential_ref IS NULL OR
                         length(CAST(credential_ref AS BLOB)) BETWEEN 1 AND 256
                       );
                     ALTER TABLE provider_accounts
                       ADD COLUMN auth_block_reason TEXT CHECK(
                         auth_block_reason IS NULL OR auth_block_reason IN (
                           'credential_locked', 'provider_reauthorization'
                         )
                       );",
                )
                .expect("reproduce the v11 additive migration");
        }
        connection
            .execute_batch("PRAGMA foreign_keys = ON;")
            .expect("restore foreign key enforcement");
    }

    fn install_test_provider_receipt_schema(connection: &Connection, version_check: &str) {
        assert!(matches!(
            version_check,
            "= 1" | "IN (1, 2)" | "IN (1, 2, 3)"
        ));
        connection
            .execute_batch(&format!(
                "DROP INDEX provider_batches_applied_idx;
                 ALTER TABLE provider_applied_batches
                   RENAME TO provider_applied_batches_source;
                 CREATE TABLE provider_applied_batches (
                   account_id TEXT NOT NULL
                     REFERENCES provider_accounts(account_id) ON DELETE CASCADE,
                   batch_id TEXT NOT NULL CHECK(
                     length(CAST(batch_id AS BLOB)) BETWEEN 1 AND 256
                   ),
                   fingerprint BLOB NOT NULL CHECK(length(fingerprint) = 32),
                   fingerprint_version INTEGER NOT NULL
                     CHECK(fingerprint_version {version_check}),
                   cursor_scope TEXT NOT NULL CHECK(
                     length(CAST(cursor_scope AS BLOB)) BETWEEN 1 AND 4096
                   ),
                   cursor TEXT NOT NULL CHECK(
                     length(CAST(cursor AS BLOB)) BETWEEN 1 AND 16384
                   ),
                   applied_at INTEGER NOT NULL CHECK(applied_at >= 0),
                   PRIMARY KEY(account_id, batch_id)
                 ) WITHOUT ROWID;
                 INSERT INTO provider_applied_batches(
                   account_id, batch_id, fingerprint, fingerprint_version,
                   cursor_scope, cursor, applied_at
                 )
                 SELECT account_id, batch_id, fingerprint, fingerprint_version,
                        cursor_scope, cursor, applied_at
                 FROM provider_applied_batches_source;
                 DROP TABLE provider_applied_batches_source;
                 CREATE INDEX provider_batches_applied_idx
                   ON provider_applied_batches(account_id, applied_at, batch_id);"
            ))
            .expect("install test provider receipt schema");
    }

    fn install_v12_provider_receipt_schema(connection: &Connection) {
        connection
            .execute_batch(
                "DROP INDEX provider_batches_applied_idx;
                 ALTER TABLE provider_applied_batches
                   RENAME TO provider_applied_batches_source;
                 CREATE TABLE provider_applied_batches (
                   account_id TEXT NOT NULL
                     REFERENCES provider_accounts(account_id) ON DELETE CASCADE,
                   batch_id TEXT NOT NULL CHECK(
                     length(CAST(batch_id AS BLOB)) BETWEEN 1 AND 256
                   ),
                   fingerprint BLOB NOT NULL CHECK(length(fingerprint) = 32),
                   cursor_scope TEXT NOT NULL CHECK(
                     length(CAST(cursor_scope AS BLOB)) BETWEEN 1 AND 2048
                   ),
                   cursor TEXT NOT NULL CHECK(
                     length(CAST(cursor AS BLOB)) BETWEEN 1 AND 16384
                   ),
                   applied_at INTEGER NOT NULL CHECK(applied_at >= 0),
                   PRIMARY KEY(account_id, batch_id)
                 ) WITHOUT ROWID;
                 INSERT INTO provider_applied_batches(
                   account_id, batch_id, fingerprint,
                   cursor_scope, cursor, applied_at
                 )
                 SELECT account_id, batch_id, fingerprint,
                        cursor_scope, cursor, applied_at
                 FROM provider_applied_batches_source;
                 DROP TABLE provider_applied_batches_source;
                 CREATE INDEX provider_batches_applied_idx
                   ON provider_applied_batches(account_id, applied_at, batch_id);",
            )
            .expect("install exact v12 provider receipt schema");
    }

    fn assert_current_provider_accounts_schema(connection: &Connection) {
        let schema = connection
            .query_row(
                "SELECT sql FROM sqlite_master
                 WHERE type = 'table' AND name = 'provider_accounts'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("provider account table SQL");
        let normalized = schema.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(normalized.contains(
            "auth_state TEXT NOT NULL DEFAULT 'signed_out' CHECK(auth_state IN ( \
             'signed_out', 'ready', 'reauthorization_required', 'unavailable' ))"
        ));
        assert!(normalized.contains(
            "sync_state TEXT NOT NULL DEFAULT 'never_synced' CHECK(sync_state IN ( \
             'never_synced', 'idle', 'scheduled', 'syncing', 'backoff', \
             'authentication_blocked', 'offline', 'failed' ))"
        ));
        assert!(normalized.contains(
            "credential_ref TEXT CHECK( credential_ref IS NULL OR \
             length(CAST(credential_ref AS BLOB)) BETWEEN 1 AND 256 )"
        ));
        assert!(normalized.contains(
            "auth_block_reason TEXT CHECK( auth_block_reason IS NULL OR auth_block_reason = \
             'provider_reauthorization' )"
        ));
        assert!(normalized.contains(
            "CHECK( auth_state NOT IN ('ready', \
             'reauthorization_required') OR credential_ref IS NOT NULL )"
        ));
        assert!(!normalized.contains("DEFAULT 'locked'"));
        // The locked-credential state was removed in schema 22.
        assert!(!normalized.contains("'credential_locked'"));
        assert!(!normalized.contains("'expired'"));
        assert!(!normalized.contains("'requires_action'"));
        assert_eq!(
            connection
                .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
                .expect("integrity check"),
            "ok"
        );
        assert_eq!(
            connection
                .prepare("PRAGMA foreign_key_check")
                .expect("foreign key check")
                .query_map([], |_| Ok(()))
                .expect("foreign key rows")
                .count(),
            0
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM provider_accounts
                     WHERE auth_state IN ('ready', 'reauthorization_required')
                       AND credential_ref IS NULL",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("credential-reference invariant"),
            0
        );
    }

    fn configured_provider_store(path: &Path, account_ids: &[&str]) -> MuxStore {
        let store = MuxStore::open(path, false).expect("provider store opens");
        for account_id in account_ids {
            store
                .connection
                .execute(
                    "INSERT INTO accounts(id, name, email, color, provider)
                     VALUES(?1, ?2, ?3, '#334155', 'jmap')",
                    params![
                        account_id,
                        format!("Account {account_id}"),
                        format!("{account_id}@example.com")
                    ],
                )
                .expect("local provider account");
            store
                .connection
                .execute(
                    "INSERT INTO provider_accounts(
                       account_id, provider_kind, remote_account_id,
                       auth_state, credential_ref, sync_state, created_at, updated_at
                     ) VALUES(?1, 'jmap', ?2, 'ready', ?3, 'idle', 1, 1)",
                    params![
                        account_id,
                        format!("remote-{account_id}"),
                        format!("provider/{account_id}")
                    ],
                )
                .expect("provider account state");
            store
                .connection
                .execute(
                    "INSERT INTO provider_capabilities(account_id, capability, enabled)
                     VALUES(?1, 'mutations', 1)",
                    [account_id],
                )
                .expect("provider mutation authority");
        }
        store
    }

    fn sample_provider_batch(account_id: &str, batch_id: &str, cursor: &str) -> ProviderBatch {
        serde_json::from_value(serde_json::json!({
            "muxAccountId": account_id,
            "batchId": batch_id,
            "expectedPriorCursor": null,
            "cursor": {
                "muxAccountId": account_id,
                "scope": { "kind": "account" },
                "value": cursor
            },
            "observedAt": 1000,
            "threadUpserts": [{
                "identity": {
                    "muxAccountId": account_id,
                    "remoteThreadId": "remote-thread-1"
                },
                "subject": "Provider batch subject",
                "participants": "sender@example.com, recipient@example.com",
                "snippet": "Normalized provider message",
                "latestAt": 900,
                "messageCount": 1,
                "inInbox": true,
                "unread": true,
                "starred": false,
                "hasAttachments": false,
                "hasInvite": false,
                "hasLinks": false,
                "hasFromMe": false,
                "category": "primary",
                "revision": "thread-r1"
            }],
            "messageUpserts": [{
                "identity": {
                    "muxAccountId": account_id,
                    "remoteMessageId": "remote-message-1",
                    "remoteThreadId": "remote-thread-1"
                },
                "subject": "Provider batch subject",
                "senderName": "Provider Sender",
                "senderEmail": "sender@example.com",
                "recipients": "recipient@example.com",
                "ccRecipients": "",
                "bccRecipients": "",
                "sentAt": 900,
                "bodyText": "Normalized provider message body.",
                "bodyState": "complete",
                "isFromMe": false,
                "revision": "message-r1",
                "keywords": ["unread"]
            }],
            "containerUpserts": [{
                "identity": {
                    "muxAccountId": account_id,
                    "remoteContainerId": "inbox"
                },
                "displayName": "Inbox",
                "kind": "mailbox",
                "role": "inbox",
                "parentRemoteContainerId": null,
                "selectable": true
            }, {
                "identity": {
                    "muxAccountId": account_id,
                    "remoteContainerId": "archive"
                },
                "displayName": "Archive",
                "kind": "mailbox",
                "role": "archive",
                "parentRemoteContainerId": null,
                "selectable": true
            }],
            "membershipChanges": [{
                "kind": "upsert",
                "membership": {
                    "message": {
                        "muxAccountId": account_id,
                        "remoteMessageId": "remote-message-1",
                        "remoteThreadId": "remote-thread-1"
                    },
                    "container": {
                        "muxAccountId": account_id,
                        "remoteContainerId": "inbox"
                    }
                }
            }],
            "tombstones": []
        }))
        .expect("valid provider batch")
    }

    fn provider_delta_batch(
        account_id: &str,
        batch_id: &str,
        cursor: &str,
        membership_changes: serde_json::Value,
        tombstones: serde_json::Value,
    ) -> ProviderBatch {
        serde_json::from_value(serde_json::json!({
            "muxAccountId": account_id,
            "batchId": batch_id,
            "expectedPriorCursor": null,
            "cursor": {
                "muxAccountId": account_id,
                "scope": { "kind": "account" },
                "value": cursor
            },
            "observedAt": 2000,
            "threadUpserts": [],
            "messageUpserts": [],
            "containerUpserts": [],
            "membershipChanges": membership_changes,
            "tombstones": tombstones
        }))
        .expect("valid provider delta batch")
    }

    fn after_cursor(mut batch: ProviderBatch, prior_cursor: &str) -> ProviderBatch {
        batch.expected_prior_cursor =
            Some(OpaqueSyncCursor::new(prior_cursor).expect("valid expected provider cursor"));
        batch
    }

    fn provider_search_batch(
        account_id: &str,
        batch_id: &str,
        cursor: &str,
        prior_cursor: Option<&str>,
        thread_upserts: serde_json::Value,
        message_upserts: serde_json::Value,
        tombstones: serde_json::Value,
    ) -> ProviderBatch {
        serde_json::from_value(serde_json::json!({
            "muxAccountId": account_id,
            "batchId": batch_id,
            "expectedPriorCursor": prior_cursor,
            "cursor": {
                "muxAccountId": account_id,
                "scope": { "kind": "account" },
                "value": cursor
            },
            "observedAt": 5000,
            "threadUpserts": thread_upserts,
            "messageUpserts": message_upserts,
            "containerUpserts": [],
            "membershipChanges": [],
            "tombstones": tombstones
        }))
        .expect("valid provider search batch")
    }

    fn provider_search_thread(
        account_id: &str,
        remote_thread_id: &str,
        subject: &str,
        participants: &str,
        message_count: i64,
    ) -> serde_json::Value {
        serde_json::json!({
            "identity": {
                "muxAccountId": account_id,
                "remoteThreadId": remote_thread_id
            },
            "subject": subject,
            "participants": participants,
            "snippet": subject,
            "latestAt": 4900,
            "messageCount": message_count,
            "inInbox": true,
            "unread": false,
            "starred": false,
            "hasAttachments": false,
            "hasInvite": false,
            "hasLinks": false,
            "hasFromMe": false,
            "category": "primary",
            "revision": format!("{remote_thread_id}-revision")
        })
    }

    fn provider_search_message(
        account_id: &str,
        remote_thread_id: &str,
        body: &str,
        revision: &str,
    ) -> serde_json::Value {
        serde_json::json!({
            "identity": {
                "muxAccountId": account_id,
                "remoteMessageId": "search-message",
                "remoteThreadId": remote_thread_id
            },
            "subject": "Search contract message",
            "senderName": "Sender Only",
            "senderEmail": "sender-only@example.test",
            "recipients": "Recipient Only <recipient-only@example.test>",
            "ccRecipients": "CC Only <cc-only@example.test>",
            "bccRecipients": "BCC Only <bcc-only@example.test>",
            "sentAt": 4800,
            "bodyText": body,
            "bodyState": "complete",
            "isFromMe": false,
            "revision": revision,
            "keywords": []
        })
    }

    fn search_thread_ids(store: &MuxStore, query: &str) -> Vec<i64> {
        store
            .search_threads(SearchInput {
                query: query.into(),
                account_id: None,
                view: Some("all".into()),
                cursor: None,
                limit: Some(100),
                timezone_offset_minutes: 0,
                hidden_account_ids: Vec::new(),
            })
            .expect("search succeeds")
            .rows
            .into_iter()
            .map(|row| row.id)
            .collect()
    }

    fn fts_rows(store: &MuxStore) -> Vec<(i64, String, String, String, String)> {
        store
            .connection
            .prepare(
                "SELECT CAST(thread_id AS INTEGER), subject, participants, body,
                        attachment_names
                 FROM messages_fts
                 ORDER BY CAST(thread_id AS INTEGER)",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    fn provider_projection_counts(store: &MuxStore, account_id: &str) -> (i64, i64, i64, i64) {
        store
            .connection
            .query_row(
                "SELECT
                   (SELECT COUNT(*) FROM provider_thread_refs WHERE account_id = ?1),
                   (SELECT COUNT(*) FROM provider_message_refs WHERE account_id = ?1),
                   (SELECT COUNT(*) FROM provider_container_memberships WHERE account_id = ?1),
                   (SELECT COUNT(*) FROM provider_applied_batches WHERE account_id = ?1)",
                [account_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("provider projection counts")
    }

    fn provider_cached_body(
        store: &MuxStore,
        account_id: &str,
    ) -> (String, String, i64, String, i64) {
        store
            .connection
            .query_row(
                "SELECT messages.body_text, messages.body_html,
                        messages.blocked_remote_resources,
                        provider_message_refs.body_state,
                        provider_message_refs.body_is_truncated
                 FROM provider_message_refs
                 JOIN messages ON messages.id = provider_message_refs.message_id
                 WHERE provider_message_refs.account_id = ?1",
                [account_id],
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
            .expect("cached provider body")
    }

    fn memberships_for_message(
        store: &MuxStore,
        account_id: &str,
        remote_message_id: &str,
    ) -> Vec<String> {
        store
            .connection
            .prepare(
                "SELECT remote_container_id FROM provider_container_memberships
                 WHERE account_id = ?1 AND remote_message_id = ?2
                 ORDER BY remote_container_id",
            )
            .unwrap()
            .query_map(params![account_id, remote_message_id], |row| {
                row.get::<_, String>(0)
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    #[test]
    fn fresh_database_matches_the_reference_schema_and_is_integral() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("mux.db");
        let store = MuxStore::open(&path, true).expect("fresh store opens");

        let version: String = store
            .connection
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .expect("schema version");
        assert_eq!(version, "23");
        assert_eq!(
            store
                .connection
                .query_row("SELECT COUNT(*) FROM accounts", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            3
        );
        assert_eq!(
            store
                .connection
                .query_row("SELECT COUNT(*) FROM threads", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            5
        );
        assert_eq!(
            store
                .connection
                .query_row("SELECT COUNT(*) FROM messages_fts", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            5
        );
        assert_eq!(
            store
                .connection
                .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        let foreign_key_violations = store
            .connection
            .prepare("PRAGMA foreign_key_check")
            .unwrap()
            .query_map([], |_| Ok(()))
            .unwrap()
            .count();
        assert_eq!(foreign_key_violations, 0);

        let bootstrap = store.bootstrap().expect("bootstrap query");
        assert_eq!(bootstrap.schema_version, 23);
        assert_eq!(bootstrap.accounts.len(), 3);
        assert_eq!(bootstrap.view_counts.len(), 4);
        assert!(bootstrap.drafts.is_empty());
        for count in &bootstrap.view_counts {
            let expected: (i64, i64, i64) = if let Some(account_id) = &count.account_id {
                store
                    .connection
                    .query_row(
                        "SELECT COALESCE(SUM(in_inbox), 0), COALESCE(SUM(starred), 0), COALESCE(SUM(has_from_me), 0)
                         FROM thread_effective WHERE account_id = ?1",
                        [account_id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .expect("account counts")
            } else {
                store
                    .connection
                    .query_row(
                        "SELECT COALESCE(SUM(in_inbox), 0), COALESCE(SUM(starred), 0), COALESCE(SUM(has_from_me), 0)
                         FROM thread_effective",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .expect("global counts")
            };
            assert_eq!((count.inbox, count.starred, count.sent), expected);
        }
        let bootstrap_json = serde_json::to_value(&bootstrap).expect("bootstrap serializes");
        assert!(bootstrap_json.get("threads").is_none());
        assert!(bootstrap_json.get("messages").is_none());
        assert!(bootstrap_json.get("attachments").is_none());

        let threads = store
            .list_threads(ThreadPageInput {
                account_id: None,
                view: Some("all".into()),
                cursor: None,
                limit: Some(100),
                hidden_account_ids: Vec::new(),
                container_id: None,
            })
            .expect("thread page");
        assert_eq!(threads.threads.len(), 5);
        assert!(!threads.has_more);
        let messages = store
            .get_thread_messages(MessagePageInput {
                thread_id: DEMO_FIXTURE_THREAD_ID,
                cursor: None,
                limit: Some(100),
            })
            .expect("message page");
        assert_eq!(
            messages
                .messages
                .iter()
                .find(|message| !message.is_from_me)
                .map(|message| message.blocked_remote_resources),
            Some(1)
        );
        assert_eq!(messages.attachments.len(), 2);
        let messages_json = serde_json::to_value(&messages).expect("message page serializes");
        for attachment in messages_json["attachments"]
            .as_array()
            .expect("attachment array")
        {
            assert!(attachment.get("bytes").is_none());
            assert!(attachment.get("content").is_none());
        }
    }

    #[test]
    fn schema_v20_rebuilds_stale_provider_container_checks_for_gmail_starred() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("schema-v20-provider-containers.db");
        let store = configured_provider_store(&path, &["gmail-account"]);
        store
            .connection
            .execute_batch(
                "PRAGMA foreign_keys = OFF;
                 DROP TABLE provider_containers;
                 CREATE TABLE provider_containers (
                   account_id TEXT NOT NULL REFERENCES provider_accounts(account_id) ON DELETE CASCADE,
                   remote_id TEXT NOT NULL CHECK(length(remote_id) BETWEEN 1 AND 2000),
                   name TEXT NOT NULL CHECK(length(name) <= 1000),
                   kind TEXT NOT NULL CHECK(kind IN (
                     'mailbox', 'folder', 'label', 'virtual', 'import_state'
                   )),
                   role TEXT NOT NULL DEFAULT 'custom' CHECK(role IN (
                     'inbox', 'archive', 'drafts', 'sent', 'trash', 'spam', 'all',
                     'important', 'custom'
                   )),
                   parent_remote_id TEXT,
                   sort_order INTEGER NOT NULL DEFAULT 0,
                   is_selectable INTEGER NOT NULL DEFAULT 1,
                   is_deleted INTEGER NOT NULL DEFAULT 0,
                   revision TEXT,
                   PRIMARY KEY(account_id, remote_id)
                 ) WITHOUT ROWID;
                 INSERT INTO provider_containers(
                   account_id, remote_id, name, kind, role
                 ) VALUES('gmail-account', 'ALL', 'All mail', 'virtual', 'all');
                 UPDATE meta SET value = '19' WHERE key = 'schema_version';
                 PRAGMA foreign_keys = ON;",
            )
            .expect("stale v19 provider container schema");
        drop(store);

        let migrated = MuxStore::open(&path, false).expect("schema v20 migration");
        assert_eq!(
            migrated
                .connection
                .query_row(
                    "SELECT kind, role FROM provider_containers
                     WHERE account_id = 'gmail-account' AND remote_id = 'ALL'",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .expect("migrated legacy container"),
            ("retrieval_state".into(), "all_mail".into())
        );
        migrated
            .connection
            .execute(
                "INSERT INTO provider_containers(
                   account_id, remote_id, name, kind, role
                 ) VALUES('gmail-account', 'STARRED', 'Starred', 'label', 'starred')",
                [],
            )
            .expect("Gmail STARRED label is accepted after migration");
        assert_eq!(
            migrated
                .connection
                .query_row(
                    "SELECT \"table\" FROM pragma_foreign_key_list('provider_containers')
                     WHERE \"from\" = 'parent_remote_id'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("container parent foreign key"),
            "provider_containers"
        );
        assert_eq!(
            migrated
                .connection
                .query_row("PRAGMA foreign_key_check", [], |_| Ok(()))
                .optional()
                .expect("foreign key check"),
            None
        );
    }

    #[test]
    fn schema_v15_adds_threading_headers_without_losing_existing_messages() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("schema-v15.db");
        let store = MuxStore::open(&path, true).expect("seeded current store");
        let message_count: i64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        store
            .connection
            .execute_batch(
                "ALTER TABLE messages DROP COLUMN internet_message_id;
                 ALTER TABLE messages DROP COLUMN in_reply_to;
                 ALTER TABLE messages DROP COLUMN references_json;
                 UPDATE meta SET value = '14' WHERE key = 'schema_version';",
            )
            .expect("reproduce schema v14");
        drop(store);

        let reopened = MuxStore::open(&path, false).expect("schema v15 migration");
        assert_eq!(
            reopened
                .connection
                .query_row("SELECT COUNT(*) FROM messages", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            message_count
        );
        let defaults: (String, String, String) = reopened
            .connection
            .query_row(
                "SELECT internet_message_id, in_reply_to, references_json
                 FROM messages ORDER BY id LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(defaults, (String::new(), String::new(), "[]".into()));
        assert_eq!(
            reopened
                .connection
                .query_row(
                    "SELECT value FROM meta WHERE key = 'schema_version'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "23"
        );
    }

    #[test]
    fn schema_v16_adds_provider_reconciliation_without_losing_messages() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("schema-v16.db");
        let store = MuxStore::open(&path, true).expect("seeded current store");
        let message_count: i64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        store
            .connection
            .execute_batch(
                "DROP TABLE provider_reconciliation_seen;
                 DROP TABLE provider_reconciliation_runs;
                 ALTER TABLE messages DROP COLUMN provider_subject;
                 UPDATE meta SET value = '15' WHERE key = 'schema_version';",
            )
            .expect("reproduce schema v15");
        drop(store);

        let reopened = MuxStore::open(&path, false).expect("schema v16 migration");
        assert_eq!(
            reopened
                .connection
                .query_row("SELECT COUNT(*) FROM messages", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            message_count
        );
        assert_eq!(
            reopened
                .connection
                .query_row(
                    "SELECT provider_subject FROM messages ORDER BY id LIMIT 1",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            ""
        );
        for table in [
            "provider_reconciliation_runs",
            "provider_reconciliation_seen",
        ] {
            assert_eq!(
                reopened
                    .connection
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master
                         WHERE type = 'table' AND name = ?1",
                        [table],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap(),
                1,
                "{table} must be migrated"
            );
        }
    }

    #[test]
    fn schema_v19_retains_one_deterministic_reconciliation_run_and_cascades_stale_marks() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("schema-v19-reconciliation.db");
        let store = MuxStore::open(&path, true).expect("seeded current store");
        store
            .connection
            .execute_batch(
                "INSERT INTO provider_accounts(
                   account_id, provider_kind, remote_account_id, auth_state, sync_state,
                   credential_ref, created_at, updated_at
                 ) VALUES(
                   'acc_work', 'imap', 'migration-account', 'ready', 'scheduled',
                   'provider/acc_work', 0, 0
                 );
                 DROP INDEX provider_reconciliation_one_per_account_idx;
                 ALTER TABLE provider_reconciliation_runs DROP COLUMN containers_complete;
                 ALTER TABLE provider_reconciliation_runs DROP COLUMN threads_complete;
                 ALTER TABLE provider_reconciliation_runs DROP COLUMN messages_complete;
                 INSERT INTO provider_reconciliation_runs(
                   account_id, scope, generation_id, started_at
                 ) VALUES
                   ('acc_work', 'sync:account:old', 'generation-old', 10),
                   ('acc_work', 'sync:account:new', 'generation-new', 20);
                 INSERT INTO provider_reconciliation_seen(
                   account_id, generation_id, object_kind, remote_id
                 ) VALUES
                   ('acc_work', 'generation-old', 'message', 'old-message'),
                   ('acc_work', 'generation-new', 'message', 'new-message');
                 UPDATE meta SET value = '18' WHERE key = 'schema_version';",
            )
            .expect("handcraft a two-run v18 reconciliation database");
        drop(store);

        let migrated = MuxStore::open(&path, false).expect("schema v19 migration");
        let retained: (String, String, i64, i64, i64) = migrated
            .connection
            .query_row(
                "SELECT scope, generation_id, containers_complete,
                        threads_complete, messages_complete
                 FROM provider_reconciliation_runs WHERE account_id = 'acc_work'",
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
            .expect("retained reconciliation run");
        assert_eq!(
            retained,
            ("sync:account:new".into(), "generation-new".into(), 0, 0, 0,),
            "the newest durable run survives and all explicit completeness gates fail closed"
        );
        assert_eq!(
            migrated
                .connection
                .query_row(
                    "SELECT group_concat(remote_id, ',')
                     FROM provider_reconciliation_seen WHERE account_id = 'acc_work'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("retained seen inventory"),
            "new-message",
            "discarding the stale generation must cascade its seen marks"
        );
        assert!(
            migrated
                .connection
                .execute(
                    "INSERT INTO provider_reconciliation_runs(
                       account_id, scope, generation_id, started_at,
                       containers_complete, threads_complete, messages_complete
                     ) VALUES(
                       'acc_work', 'sync:account:other', 'generation-other', 30, 0, 0, 0
                     )",
                    [],
                )
                .is_err(),
            "v19 enforces one reconciliation run per account"
        );
        assert_eq!(
            migrated
                .connection
                .query_row(
                    "SELECT value FROM meta WHERE key = 'schema_version'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "23"
        );
    }

    #[test]
    fn provider_projection_is_account_scoped_and_contains_no_credentials() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("provider-projection.db");
        let store = MuxStore::open(&path, true).expect("seeded store opens");
        let work_thread: i64 = store
            .connection
            .query_row(
                "SELECT id FROM threads WHERE account_id = 'acc_work' ORDER BY id LIMIT 1",
                [],
                |row| row.get(0),
            )
            .expect("work thread");
        let personal_thread: i64 = store
            .connection
            .query_row(
                "SELECT id FROM threads WHERE account_id = 'acc_personal' ORDER BY id LIMIT 1",
                [],
                |row| row.get(0),
            )
            .expect("personal thread");
        let work_message: i64 = store
            .connection
            .query_row(
                "SELECT id FROM messages WHERE thread_id = ?1 ORDER BY id LIMIT 1",
                [work_thread],
                |row| row.get(0),
            )
            .expect("work message");
        let personal_message: i64 = store
            .connection
            .query_row(
                "SELECT id FROM messages WHERE thread_id = ?1 ORDER BY id LIMIT 1",
                [personal_thread],
                |row| row.get(0),
            )
            .expect("personal message");

        store
            .connection
            .execute_batch(&format!(
                "INSERT INTO provider_accounts(
                   account_id, provider_kind, remote_account_id, auth_state, sync_state,
                   credential_ref, auth_block_reason, created_at, updated_at
                 ) VALUES
                   ('acc_work', 'gmail', 'shared-account', 'ready', 'idle',
                    'provider/acc_work', NULL, 1, 1),
                   ('acc_personal', 'jmap', 'shared-account', 'reauthorization_required',
                    'offline', 'provider/acc_personal', 'provider_reauthorization', 1, 1);
                 INSERT INTO provider_capabilities(account_id, capability, enabled)
                   VALUES('acc_work', 'delta_sync', 1);
                 INSERT INTO provider_containers(
                   account_id, remote_id, name, kind, role, sort_order
                 ) VALUES
                   ('acc_work', 'shared-container', 'Inbox', 'label', 'inbox', 0),
                   ('acc_personal', 'shared-container', 'Inbox', 'mailbox', 'inbox', 0),
                   ('acc_work', 'archive-folder', 'Archive', 'folder', 'archive', 1),
                   ('acc_work', 'retrieval-state', 'POP retrieval state', 'retrieval_state', 'custom', 2);
                 INSERT INTO provider_thread_refs(
                   account_id, remote_thread_id, thread_id, revision
                 ) VALUES
                   ('acc_work', 'shared-thread', {work_thread}, 'r1'),
                   ('acc_personal', 'shared-thread', {personal_thread}, 'r2');
                 INSERT INTO provider_message_refs(
                   account_id, remote_message_id, message_id, remote_thread_id, body_state
                 ) VALUES
                   ('acc_work', 'shared-message', {work_message}, 'shared-thread', 'metadata'),
                   ('acc_personal', 'shared-message', {personal_message}, 'shared-thread', 'normalized');
                 INSERT INTO provider_container_memberships(
                   account_id, remote_message_id, remote_container_id
                 ) VALUES
                   ('acc_work', 'shared-message', 'shared-container'),
                   ('acc_personal', 'shared-message', 'shared-container');
                 INSERT INTO provider_message_keywords(account_id, remote_message_id, keyword)
                   VALUES('acc_work', 'shared-message', 'starred');
                 INSERT INTO provider_sync_cursors(account_id, scope, cursor, updated_at)
                   VALUES('acc_work', 'account', 'opaque-next-page', 2);
                 INSERT INTO provider_tombstones(
                   account_id, object_kind, remote_id, deleted_at
                 ) VALUES('acc_work', 'message', 'deleted-message', 3);"
            ))
            .expect("provider-neutral projection inserts");

        let duplicate = store.connection.execute(
            "INSERT INTO provider_message_refs(
               account_id, remote_message_id, message_id, body_state
             ) VALUES('acc_work', 'shared-message', ?1, 'unavailable')",
            [work_message],
        );
        assert!(
            duplicate.is_err(),
            "remote IDs must be unique within an account"
        );

        let cross_account = store.connection.execute(
            "INSERT INTO provider_thread_refs(account_id, remote_thread_id, thread_id)
             VALUES('acc_personal', 'wrong-account-thread', ?1)",
            [work_thread],
        );
        assert!(cross_account
            .expect_err("cross-account local references must fail")
            .to_string()
            .contains("provider thread account mismatch"));

        let change_thread_account = store.connection.execute(
            "UPDATE threads SET account_id = 'acc_personal' WHERE id = ?1",
            [work_thread],
        );
        assert!(change_thread_account
            .expect_err("referenced thread account must be immutable")
            .to_string()
            .contains("thread account is referenced by provider state"));
        let move_message_across_accounts = store.connection.execute(
            "UPDATE messages SET thread_id = ?1 WHERE id = ?2",
            params![personal_thread, work_message],
        );
        assert!(move_message_across_accounts
            .expect_err("referenced message cannot move across accounts")
            .to_string()
            .contains("message thread is referenced by provider state"));

        let second_work_thread: i64 = store
            .connection
            .query_row(
                "SELECT id FROM threads
                 WHERE account_id = 'acc_work' AND id <> ?1 ORDER BY id LIMIT 1",
                [work_thread],
                |row| row.get(0),
            )
            .expect("second work thread");
        store
            .connection
            .execute(
                "INSERT INTO provider_thread_refs(account_id, remote_thread_id, thread_id)
                 VALUES('acc_work', 'other-thread', ?1)",
                [second_work_thread],
            )
            .expect("second remote thread mapping");
        store
            .connection
            .execute(
                "INSERT INTO messages(
                   id, thread_id, sender_name, sender_email, recipients, sent_at,
                   body_text, is_from_me
                 ) VALUES(999999, ?1, 'Sender', 'sender@example.com',
                          'jordan@acme.example', 4, 'Mismatch test', 0)",
                [work_thread],
            )
            .expect("local message for thread consistency test");
        let mismatched_remote_thread = store.connection.execute(
            "INSERT INTO provider_message_refs(
               account_id, remote_message_id, message_id, remote_thread_id
             ) VALUES('acc_work', 'mismatched-message', 999999, 'other-thread')",
            [],
        );
        assert!(mismatched_remote_thread
            .expect_err("remote and local thread mappings must agree")
            .to_string()
            .contains("provider message thread mismatch"));

        let exact_remote_id = "x".repeat(2_048);
        store
            .connection
            .execute(
                "INSERT INTO provider_containers(account_id, remote_id, name, kind)
                 VALUES('acc_work', ?1, 'Exact ID boundary', 'mailbox')",
                [&exact_remote_id],
            )
            .expect("2048-byte remote ID is accepted");
        let multibyte_oversize_id = "é".repeat(1_025);
        assert!(
            store
                .connection
                .execute(
                    "INSERT INTO provider_containers(account_id, remote_id, name, kind)
                     VALUES('acc_work', ?1, 'Oversized ID', 'mailbox')",
                    [&multibyte_oversize_id],
                )
                .is_err(),
            "remote ID limit must count UTF-8 bytes"
        );
        let exact_cursor = "c".repeat(16_384);
        store
            .connection
            .execute(
                "INSERT INTO provider_sync_cursors(account_id, scope, cursor, updated_at)
                 VALUES('acc_work', 'container:exact', ?1, 4)",
                [&exact_cursor],
            )
            .expect("16384-byte cursor is accepted");
        let oversized_cursor = "c".repeat(16_385);
        assert!(
            store
                .connection
                .execute(
                    "INSERT INTO provider_sync_cursors(account_id, scope, cursor, updated_at)
                     VALUES('acc_work', 'container:oversized', ?1, 4)",
                    [&oversized_cursor],
                )
                .is_err(),
            "oversized cursor must be rejected"
        );

        assert!(
            store
                .connection
                .execute(
                    "INSERT INTO provider_tombstones(
                       account_id, object_kind, remote_id, deleted_at
                     ) VALUES('acc_work', 'membership', 'message-without-container', 5)",
                    [],
                )
                .is_err(),
            "membership tombstones require the related container ID"
        );
        assert!(
            store
                .connection
                .execute(
                    "INSERT INTO provider_tombstones(
                       account_id, object_kind, remote_id, related_remote_id, deleted_at
                     ) VALUES('acc_work', 'message', 'message-with-related', 'ambiguous', 5)",
                    [],
                )
                .is_err(),
            "non-membership tombstones cannot carry an ambiguous related ID"
        );
        store
            .connection
            .execute(
                "INSERT INTO provider_tombstones(
                   account_id, object_kind, remote_id, related_remote_id, deleted_at
                 ) VALUES('acc_work', 'membership', 'removed-message', 'removed-container', 5)",
                [],
            )
            .expect("membership tombstone identities are explicit");

        let provider_rows: i64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM provider_message_refs", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(provider_rows, 2, "same remote ID is valid across accounts");

        let mut provider_identifiers = store
            .connection
            .prepare(
                "SELECT lower(name)
                 FROM sqlite_master
                 WHERE name LIKE 'provider_%' AND type IN ('table', 'index', 'trigger')",
            )
            .unwrap()
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        for table in [
            "provider_accounts",
            "provider_capabilities",
            "provider_containers",
            "provider_thread_refs",
            "provider_message_refs",
            "provider_container_memberships",
            "provider_message_keywords",
            "provider_sync_cursors",
            "provider_applied_batches",
            "provider_work_items",
            "provider_tombstones",
            "provider_reconciliation_runs",
            "provider_reconciliation_seen",
        ] {
            provider_identifiers.extend(
                store
                    .connection
                    .prepare(&format!("PRAGMA table_info({table})"))
                    .unwrap()
                    .query_map([], |row| row.get::<_, String>(1))
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap(),
            );
        }
        let provider_identifiers = provider_identifiers.join("\n").to_lowercase();
        for forbidden in ["password", "secret", "access_token", "refresh_token"] {
            assert!(
                !provider_identifiers.contains(forbidden),
                "provider projection must not contain {forbidden}"
            );
        }
        assert!(provider_identifiers.contains("credential_ref"));
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM provider_accounts
                     WHERE credential_ref IS NOT NULL",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            2,
            "the projection contains only a non-secret vault lookup marker, never credentials"
        );
        assert_eq!(
            store
                .connection
                .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        assert_eq!(
            store
                .connection
                .prepare("PRAGMA foreign_key_check")
                .unwrap()
                .query_map([], |_| Ok(()))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn schema_v22_adds_provider_message_correlation_without_losing_references() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("provider-correlation-migration.db");
        let mut store = configured_provider_store(&path, &["migration-account"]);
        store
            .apply_provider_batch(sample_provider_batch(
                "migration-account",
                "pre-v23-batch",
                "pre-v23-cursor",
            ))
            .expect("provider reference fixture");
        let before: (String, i64, String) = store
            .connection
            .query_row(
                "SELECT remote_message_id, message_id, body_state
                 FROM provider_message_refs WHERE account_id = 'migration-account'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("pre-migration reference");
        store
            .connection
            .execute_batch(
                "ALTER TABLE provider_message_refs DROP COLUMN client_correlation_id;
                 UPDATE meta SET value = '22' WHERE key = 'schema_version';",
            )
            .expect("simulate schema v22");
        drop(store);

        let migrated = MuxStore::open(&path, false).expect("v22 migrates to v23");
        let after: (String, i64, String, Option<String>) = migrated
            .connection
            .query_row(
                "SELECT remote_message_id, message_id, body_state, client_correlation_id
                 FROM provider_message_refs WHERE account_id = 'migration-account'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("migrated reference");
        assert_eq!((after.0, after.1, after.2), before);
        assert_eq!(after.3, None);
        assert_eq!(
            migrated
                .connection
                .query_row(
                    "SELECT value FROM meta WHERE key = 'schema_version'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("schema version"),
            "23"
        );
    }

    #[test]
    fn schema_v6_migration_preserves_mail_and_adds_provider_projection_atomically() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("provider-migration.db");
        let store = MuxStore::open(&path, true).expect("schema created");
        let before: (i64, i64, String) = store
            .connection
            .query_row(
                "SELECT
                   (SELECT COUNT(*) FROM threads),
                   (SELECT COUNT(*) FROM messages),
                   (SELECT subject FROM threads ORDER BY id LIMIT 1)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("reference mail data");
        store
            .connection
            .execute_batch(
                "DROP TABLE provider_reconciliation_seen;
                 DROP TABLE provider_reconciliation_runs;
                 DROP TABLE provider_work_items;
                 DROP TABLE provider_message_keywords;
                 DROP TABLE provider_container_memberships;
                 DROP TABLE provider_message_refs;
                 DROP TABLE provider_thread_refs;
                 DROP TABLE provider_sync_cursors;
                 DROP TABLE provider_applied_batches;
                 DROP TABLE provider_tombstones;
                 DROP TABLE provider_containers;
                 DROP TABLE provider_capabilities;
                 DROP TABLE provider_accounts;
                 UPDATE meta SET value = '6' WHERE key = 'schema_version';",
            )
            .expect("simulate the v6 boundary");
        drop(store);

        let migrated = MuxStore::open(&path, false).expect("v6 migrates to provider schema");
        let after: (i64, i64, String) = migrated
            .connection
            .query_row(
                "SELECT
                   (SELECT COUNT(*) FROM threads),
                   (SELECT COUNT(*) FROM messages),
                   (SELECT subject FROM threads ORDER BY id LIMIT 1)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("migrated mail data");
        assert_eq!(after, before);
        assert_eq!(
            migrated
                .connection
                .query_row(
                    "SELECT value FROM meta WHERE key = 'schema_version'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "23"
        );
        for table in [
            "provider_accounts",
            "provider_capabilities",
            "provider_containers",
            "provider_thread_refs",
            "provider_message_refs",
            "provider_container_memberships",
            "provider_message_keywords",
            "provider_sync_cursors",
            "provider_applied_batches",
            "provider_work_items",
            "provider_tombstones",
        ] {
            assert_eq!(
                migrated
                    .connection
                    .query_row(
                        "SELECT EXISTS(
                           SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1
                         )",
                        [table],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap(),
                1,
                "{table} must exist after migration"
            );
        }
    }

    #[test]
    fn schema_v7_migration_preserves_provider_state_and_adds_batch_receipts() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("batch-receipt-migration.db");
        let store = MuxStore::open(&path, false).expect("schema created");
        store
            .connection
            .execute_batch(
                "INSERT INTO accounts(id, name, email, color, provider)
                   VALUES('acc-migrate', 'Migration', 'migration@example.com', '#000000', 'jmap');
                 INSERT INTO provider_accounts(
                   account_id, provider_kind, remote_account_id, created_at, updated_at
                 ) VALUES('acc-migrate', 'jmap', 'remote-migrate', 1, 1);
                 INSERT INTO provider_containers(account_id, remote_id, name, kind)
                   VALUES('acc-migrate', 'inbox', 'Inbox', 'mailbox');
                 DROP TABLE provider_work_items;
                 DROP TABLE provider_applied_batches;
                 UPDATE meta SET value = '7' WHERE key = 'schema_version';",
            )
            .expect("simulate schema v7");
        drop(store);

        let migrated = MuxStore::open(&path, false).expect("v7 migrates to batch receipts");
        assert_eq!(
            migrated
                .connection
                .query_row(
                    "SELECT name FROM provider_containers
                     WHERE account_id = 'acc-migrate' AND remote_id = 'inbox'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "Inbox"
        );
        assert_eq!(
            migrated
                .connection
                .query_row(
                    "SELECT EXISTS(
                       SELECT 1 FROM sqlite_master
                       WHERE type = 'table' AND name = 'provider_applied_batches'
                     )",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(
            migrated
                .connection
                .query_row(
                    "SELECT value FROM meta WHERE key = 'schema_version'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "23"
        );
    }

    #[test]
    fn schema_v8_migration_adds_body_completeness_and_tombstone_cursor_without_data_loss() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("provider-body-migration.db");
        let mut store = configured_provider_store(&path, &["acc-v8"]);
        store
            .apply_provider_batch(sample_provider_batch("acc-v8", "batch-v8", "cursor-v8"))
            .expect("provider state exists before migration");
        store
            .connection
            .execute_batch(
                "ALTER TABLE provider_message_refs DROP COLUMN body_is_truncated;
                 ALTER TABLE provider_tombstones DROP COLUMN cursor;
                 INSERT INTO provider_tombstones(
                   account_id, object_kind, remote_id, related_remote_id,
                   revision, deleted_at
                 ) VALUES('acc-v8', 'container', 'old-box', '', 'old-cursor', 10);
                 DROP TABLE provider_work_items;
                 UPDATE meta SET value = '8' WHERE key = 'schema_version';",
            )
            .expect("simulate schema v8");
        drop(store);

        let migrated = MuxStore::open(&path, false).expect("v8 migrates atomically");
        assert_eq!(
            migrated
                .connection
                .query_row(
                    "SELECT body_state, body_is_truncated FROM provider_message_refs
                     WHERE account_id = 'acc-v8' AND remote_message_id = 'remote-message-1'",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .unwrap(),
            ("normalized".into(), 1),
            "v8 normalized bodies have unknown completeness and migrate conservatively"
        );
        assert_eq!(
            migrated
                .connection
                .query_row(
                    "SELECT cursor FROM provider_tombstones
                     WHERE account_id = 'acc-v8' AND remote_id = 'old-box'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "old-cursor",
            "the v8 ingest path stored tombstone cursors in revision"
        );
        assert_eq!(
            migrated
                .connection
                .query_row(
                    "SELECT value FROM meta WHERE key = 'schema_version'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "23"
        );
    }

    #[test]
    fn schema_v9_migration_adds_durable_worker_without_changing_accounts() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("worker-migration.db");
        let store = MuxStore::open(&path, false).expect("schema created");
        store
            .connection
            .execute_batch(
                "INSERT INTO accounts(id, name, email, color, provider)
                   VALUES('acc-v9', 'Existing account', 'v9@example.com', '#000000', 'fake');
                 INSERT INTO drafts(
                   id, account_id, recipients, subject, body, body_html, updated_at, revision
                 ) VALUES(
                   'draft-v9', 'acc-v9', 'recipient@example.com', 'Pending v9 send',
                   'Preserve this body', '<p>Preserve this body</p>', 8, 2
                 );
                 INSERT INTO operations(
                   id, field, kind, old_value, new_value, payload_json, state,
                   created_at, not_before
                 ) VALUES(
                   'operation-v9', 'send', 'send', 'draft', 'submitted',
                   '{\"draftId\":\"draft-v9\",\"messageId\":\"message-v9\"}',
                   'pending', 9, 10
                 );
                 DROP TABLE provider_work_items;
                 UPDATE meta SET value = '9' WHERE key = 'schema_version';",
            )
            .expect("simulate schema v9");
        drop(store);

        let migrated = MuxStore::open(&path, false).expect("v9 migrates to durable worker schema");
        assert_eq!(
            migrated
                .connection
                .query_row("SELECT name FROM accounts WHERE id = 'acc-v9'", [], |row| {
                    row.get::<_, String>(0)
                },)
                .unwrap(),
            "Existing account"
        );
        assert_eq!(
            migrated
                .connection
                .query_row(
                    "SELECT value FROM meta WHERE key = 'schema_version'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "23"
        );
        assert_eq!(
            migrated
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('provider_work_items')
                     WHERE name IN ('scope', 'ordering_key', 'payload_fingerprint', 'lease_token')",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            4
        );
        let migrated_work: (String, String, String) = migrated
            .connection
            .query_row(
                "SELECT work.account_id, work.state, work.payload_json
                 FROM provider_work_items work
                 WHERE work.operation_id = 'operation-v9'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(migrated_work.0, "acc-v9");
        assert_eq!(migrated_work.1, "queued");
        let snapshot: serde_json::Value = serde_json::from_str(&migrated_work.2).unwrap();
        assert_eq!(snapshot["body"], "Preserve this body");
        assert_eq!(snapshot["accountId"], "acc-v9");
        assert_eq!(snapshot["messageId"], "message-v9");
    }

    #[test]
    fn schema_v10_migration_adds_auth_block_provenance_without_data_loss() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("auth-provenance-migration.db");
        let store = configured_provider_store(&path, &["reauth-account", "locked-account"]);
        let worker = DurableWorker::new(&path, WorkerConfig::default()).unwrap();
        worker
            .enqueue(
                NewWorkItem {
                    id: "reauth-work".into(),
                    account_id: "reauth-account".into(),
                    operation_id: None,
                    kind: WorkKind::Sync,
                    scope: "sync:v1:reauth".into(),
                    ordering_key: "sync:v1:reauth".into(),
                    payload_json: "{\"cursor\":\"preserved\"}".into(),
                    priority: 0,
                    available_at: 0,
                    max_attempts: 8,
                },
                0,
            )
            .unwrap();
        let claim = worker
            .claim_available(1, "migration-worker", 1)
            .unwrap()
            .remove(0);
        worker
            .acknowledge(
                &claim,
                WorkerOutcome::AuthenticationExpired {
                    code: "oauth_expired".into(),
                },
                2,
            )
            .unwrap();
        worker
            .enqueue(
                NewWorkItem {
                    id: "locked-work".into(),
                    account_id: "locked-account".into(),
                    operation_id: None,
                    kind: WorkKind::Sync,
                    scope: "sync:v1:locked".into(),
                    ordering_key: "sync:v1:locked".into(),
                    payload_json: "{\"cursor\":\"also-preserved\"}".into(),
                    priority: 0,
                    available_at: 0,
                    max_attempts: 8,
                },
                0,
            )
            .unwrap();
        install_exact_legacy_provider_accounts_schema(&store.connection, false);
        store
            .connection
            .execute_batch(
                "INSERT INTO provider_accounts(
                   account_id, provider_kind, remote_account_id, auth_state, sync_state,
                   created_at, updated_at
                 ) VALUES
                   ('locked-account', 'jmap', 'remote-locked-account', 'locked', 'offline', 1, 1),
                   ('reauth-account', 'jmap', 'remote-reauth-account', 'expired', 'error', 1, 1);
                 DROP TRIGGER provider_work_auth_block_insert;
                 DROP TRIGGER provider_work_auth_block_update;
                 UPDATE provider_work_items
                   SET state = 'authentication_blocked',
                       auth_block_reason = 'provider_reauthorization',
                       last_error_code = 'credential_locked'
                   WHERE id = 'locked-work';
                 ALTER TABLE provider_work_items DROP COLUMN auth_block_reason;
                 UPDATE meta SET value = '10' WHERE key = 'schema_version';",
            )
            .expect("simulate schema v10");
        drop(worker);
        drop(store);

        let migrated = MuxStore::open(&path, false).expect("v10 migrates atomically");
        assert_eq!(
            migrated
                .connection
                .query_row(
                    "SELECT value FROM meta WHERE key = 'schema_version'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "23"
        );
        let accounts = migrated
            .connection
            .prepare(
                "SELECT account_id, remote_account_id, credential_ref, auth_block_reason
                 FROM provider_accounts ORDER BY account_id",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            accounts,
            vec![
                (
                    "locked-account".into(),
                    "remote-locked-account".into(),
                    None,
                    None
                ),
                (
                    "reauth-account".into(),
                    "remote-reauth-account".into(),
                    None,
                    None
                )
            ]
        );
        let work = migrated
            .connection
            .prepare(
                "SELECT id, payload_json, state, attempt_count, auth_block_reason
                 FROM provider_work_items ORDER BY id",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            work,
            vec![
                (
                    "locked-work".into(),
                    "{\"cursor\":\"also-preserved\"}".into(),
                    "authentication_blocked".into(),
                    0,
                    "provider_reauthorization".into()
                ),
                (
                    "reauth-work".into(),
                    "{\"cursor\":\"preserved\"}".into(),
                    "authentication_blocked".into(),
                    1,
                    "provider_reauthorization".into()
                )
            ]
        );
        assert_eq!(
            migrated
                .connection
                .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
    }

    #[test]
    fn exact_legacy_provider_account_schema_migrates_without_losing_projection_rows() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("exact-legacy-provider-accounts.db");
        let store = MuxStore::open(&path, false).expect("current schema created");
        store
            .connection
            .execute_batch(
                "INSERT INTO accounts(id, name, email, color, provider) VALUES
                   ('legacy-ready', 'Ready', 'ready@example.com', '#111111', 'jmap'),
                   ('legacy-expired', 'Expired', 'expired@example.com', '#222222', 'imap'),
                   ('legacy-locked', 'Locked', 'locked@example.com', '#333333', 'gmail');
                 INSERT INTO threads(
                   id, account_id, subject, participants, snippet, latest_at, message_count,
                   remote_in_inbox, remote_unread, remote_starred, has_attachment,
                   has_invite, has_link, has_from_me
                 ) VALUES(
                   9101, 'legacy-ready', 'Preserved thread', 'sender@example.com',
                   'Preserved snippet', 10, 1, 1, 1, 0, 0, 0, 0, 0
                 );
                 INSERT INTO messages(
                   id, thread_id, sender_name, sender_email, recipients, sent_at,
                   body_text, is_from_me
                 ) VALUES(
                   9201, 9101, 'Sender', 'sender@example.com', 'ready@example.com',
                   10, 'Preserved body', 0
                 );",
            )
            .expect("local reference rows");
        install_exact_legacy_provider_accounts_schema(&store.connection, false);
        store
            .connection
            .execute_batch(
                "INSERT INTO provider_accounts(
                   account_id, provider_kind, remote_account_id, auth_state, sync_state,
                   last_error_code, last_sync_at, created_at, updated_at
                 ) VALUES
                   ('legacy-ready', 'jmap', 'remote-ready', 'ready', 'idle', NULL, 101, 1, 2),
                   ('legacy-expired', 'imap', 'remote-expired', 'expired', 'error',
                    'oauth_expired', 102, 3, 4),
                   ('legacy-locked', 'gmail', 'remote-locked', 'locked', 'idle', NULL, NULL, 5, 6);
                 INSERT INTO provider_capabilities(account_id, capability, enabled)
                   VALUES('legacy-ready', 'delta_sync', 1);
                 INSERT INTO provider_containers(account_id, remote_id, name, kind, role)
                   VALUES('legacy-ready', 'legacy-inbox', 'Inbox', 'mailbox', 'inbox');
                 INSERT INTO provider_thread_refs(
                   account_id, remote_thread_id, thread_id, revision
                 ) VALUES('legacy-ready', 'remote-thread', 9101, 'thread-r1');
                 INSERT INTO provider_message_refs(
                   account_id, remote_message_id, message_id, remote_thread_id,
                   revision, body_state
                 ) VALUES(
                   'legacy-ready', 'remote-message', 9201, 'remote-thread',
                   'message-r1', 'normalized'
                 );
                 INSERT INTO provider_sync_cursors(account_id, scope, cursor, updated_at)
                   VALUES('legacy-ready', 'a:v1', 'opaque-cursor', 103);
                 UPDATE meta SET value = '10' WHERE key = 'schema_version';",
            )
            .expect("exact legacy provider state");
        drop(store);

        let migrated = MuxStore::open(&path, false).expect("exact legacy schema migrates");
        assert_eq!(
            migrated
                .connection
                .query_row(
                    "SELECT value FROM meta WHERE key = 'schema_version'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "23"
        );
        let accounts = migrated
            .connection
            .prepare(
                "SELECT account_id, remote_account_id, auth_state, credential_ref,
                        auth_block_reason, sync_state, last_error_code, last_sync_at,
                        created_at, updated_at
                 FROM provider_accounts ORDER BY account_id",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<i64>>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, i64>(9)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            accounts,
            vec![
                (
                    "legacy-expired".into(),
                    "remote-expired".into(),
                    "signed_out".into(),
                    None,
                    None,
                    "offline".into(),
                    Some("oauth_expired".into()),
                    Some(102),
                    3,
                    4,
                ),
                (
                    "legacy-locked".into(),
                    "remote-locked".into(),
                    "signed_out".into(),
                    None,
                    None,
                    "offline".into(),
                    None,
                    None,
                    5,
                    6,
                ),
                (
                    "legacy-ready".into(),
                    "remote-ready".into(),
                    "signed_out".into(),
                    None,
                    None,
                    "offline".into(),
                    None,
                    Some(101),
                    1,
                    2,
                ),
            ]
        );
        assert_eq!(
            migrated
                .connection
                .query_row(
                    "SELECT
                       (SELECT capability FROM provider_capabilities WHERE account_id = 'legacy-ready'),
                       (SELECT name FROM provider_containers WHERE account_id = 'legacy-ready'),
                       (SELECT thread_id FROM provider_thread_refs WHERE account_id = 'legacy-ready'),
                       (SELECT message_id FROM provider_message_refs WHERE account_id = 'legacy-ready'),
                       (SELECT cursor FROM provider_sync_cursors WHERE account_id = 'legacy-ready')",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, String>(4)?,
                        ))
                    },
                )
                .unwrap(),
            (
                "delta_sync".into(),
                "Inbox".into(),
                9101,
                9201,
                "opaque-cursor".into(),
            )
        );
        assert_current_provider_accounts_schema(&migrated.connection);
    }

    #[test]
    fn schema_v11_repairs_stale_provider_account_checks_and_preserves_credential_markers() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("stale-v11-provider-accounts.db");
        let store = MuxStore::open(&path, false).expect("current schema created");
        store
            .connection
            .execute_batch(
                "INSERT INTO accounts(id, name, email, color, provider)
                   VALUES('stale-v11', 'Stale v11', 'v11@example.com', '#444444', 'jmap');",
            )
            .expect("local account");
        install_exact_legacy_provider_accounts_schema(&store.connection, true);
        store
            .connection
            .execute_batch(
                "INSERT INTO provider_accounts(
                   account_id, provider_kind, remote_account_id, auth_state, sync_state,
                   last_error_code, last_sync_at, created_at, updated_at,
                   credential_ref, auth_block_reason
                 ) VALUES(
                   'stale-v11', 'jmap', 'remote-v11', 'locked', 'idle',
                   'credential_locked', 222, 11, 12,
                   'provider/stale-v11', 'credential_locked'
                 );
                 INSERT INTO provider_capabilities(account_id, capability, enabled)
                   VALUES('stale-v11', 'push', 1);
                 INSERT INTO provider_containers(account_id, remote_id, name, kind, role)
                   VALUES('stale-v11', 'inbox', 'Inbox', 'mailbox', 'inbox');
                 UPDATE meta SET value = '11' WHERE key = 'schema_version';",
            )
            .expect("stale v11 state");
        drop(store);

        let migrated = MuxStore::open(&path, false).expect("stale v11 checks are repaired");
        assert_eq!(
            migrated
                .connection
                .query_row(
                    "SELECT value FROM meta WHERE key = 'schema_version'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "23"
        );
        assert_eq!(
            migrated
                .connection
                .query_row(
                    "SELECT remote_account_id, auth_state, credential_ref,
                            auth_block_reason, sync_state, last_error_code,
                            last_sync_at, created_at, updated_at
                     FROM provider_accounts WHERE account_id = 'stale-v11'",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, i64>(6)?,
                            row.get::<_, i64>(7)?,
                            row.get::<_, i64>(8)?,
                        ))
                    },
                )
                .unwrap(),
            (
                "remote-v11".into(),
                // Schema 22 turns a stale locked account into one needing reauthorization.
                "reauthorization_required".into(),
                "provider/stale-v11".into(),
                "provider_reauthorization".into(),
                "authentication_blocked".into(),
                "credential_locked".into(),
                222,
                11,
                12,
            )
        );
        assert_eq!(
            migrated
                .connection
                .query_row(
                    "SELECT
                       (SELECT capability FROM provider_capabilities WHERE account_id = 'stale-v11'),
                       (SELECT name FROM provider_containers WHERE account_id = 'stale-v11')",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .unwrap(),
            ("push".into(), "Inbox".into())
        );
        assert!(
            migrated
                .connection
                .execute(
                    "UPDATE provider_accounts SET credential_ref = NULL
                     WHERE account_id = 'stale-v11'",
                    [],
                )
                .is_err(),
            "credential-dependent auth state must reject a missing vault reference"
        );
        assert_current_provider_accounts_schema(&migrated.connection);
    }

    #[test]
    fn provider_batch_commit_is_idempotent_scoped_and_conflict_detecting() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("provider-batch.db");
        let mut store = configured_provider_store(&path, &["acc-one", "acc-two"]);
        let batch = sample_provider_batch("acc-one", "shared-batch", "cursor-1");
        let mut reordered = batch.clone();
        reordered.container_upserts.reverse();
        assert_eq!(
            provider_batch_fingerprint_v1(&batch),
            provider_batch_fingerprint_v1(&reordered),
            "batch fingerprint must ignore semantically irrelevant vector order"
        );
        assert_eq!(
            provider_batch_fingerprint_v2(&batch),
            provider_batch_fingerprint_v2(&reordered),
            "v2 fingerprint must ignore semantically irrelevant vector order"
        );
        let fingerprint_hex = provider_batch_fingerprint_v1(&batch)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            fingerprint_hex, "1e30812beea26445c3602a1fb9904b4a235049a893bf21ec5aa2e20906e98983",
            "fingerprint v1 is a pinned durable encoding"
        );
        let fingerprint_v2_hex = provider_batch_fingerprint_v2(&batch)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            fingerprint_v2_hex, "2cbaf83e0107347e5cbcf803914fff587a3fafe3ea0f9d07a0607dcfc365ae96",
            "fingerprint v2 is a pinned durable encoding"
        );

        let applied = store
            .apply_provider_batch(batch.clone())
            .expect("first batch applies");
        assert!(applied.applied);
        assert_eq!(applied.counts.thread_upserts, 1);
        assert_eq!(applied.counts.message_upserts, 1);
        assert_eq!(provider_projection_counts(&store, "acc-one"), (1, 1, 1, 1));
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT fingerprint_version FROM provider_applied_batches
                     WHERE account_id = 'acc-one' AND batch_id = 'shared-batch'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            2,
            "new receipts are always written with fingerprint v2"
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT scope || ':' || cursor FROM provider_sync_cursors
                     WHERE account_id = 'acc-one'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "a:v1:cursor-1"
        );
        assert_eq!(
            store
                .connection
                .query_row("SELECT COUNT(*) FROM threads", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .connection
                .query_row("SELECT COUNT(*) FROM messages", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .connection
                .query_row("SELECT COUNT(*) FROM messages_fts", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1,
            "provider projection creates exactly one current FTS row per live thread"
        );

        let stale_distinct_batch =
            sample_provider_batch("acc-one", "stale-distinct", "cursor-stale");
        assert!(matches!(
            store.apply_provider_batch(stale_distinct_batch),
            Err(StoreError::Conflict(_))
        ));
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT cursor FROM provider_sync_cursors
                     WHERE account_id = 'acc-one' AND scope = 'a:v1'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "cursor-1",
            "a distinct stale batch must not regress durable cursor state"
        );

        let mut changed_prior_cursor = batch.clone();
        changed_prior_cursor.expected_prior_cursor =
            Some(OpaqueSyncCursor::new("different-prior-cursor").expect("bounded prior cursor"));
        assert_eq!(
            store
                .apply_provider_batch(changed_prior_cursor)
                .expect_err("changed expected prior cursor must not replay")
                .to_string(),
            "Provider batch ID was reused with different content"
        );

        let replay = store
            .apply_provider_batch(reordered)
            .expect("same semantic batch replays");
        assert!(!replay.applied);
        assert_eq!(replay.counts.message_upserts, 0);
        assert_eq!(provider_projection_counts(&store, "acc-one"), (1, 1, 1, 1));

        drop(store);
        let mut reopened = MuxStore::open(&path, false).expect("committed provider store reopens");
        let replay_after_restart = reopened
            .apply_provider_batch(batch.clone())
            .expect("post-commit replay is a no-op");
        assert!(!replay_after_restart.applied);
        assert_eq!(
            provider_projection_counts(&reopened, "acc-one"),
            (1, 1, 1, 1)
        );

        let conflicting = sample_provider_batch("acc-one", "shared-batch", "different-cursor");
        let before_conflict = provider_projection_counts(&reopened, "acc-one");
        assert!(matches!(
            reopened.apply_provider_batch(conflicting),
            Err(StoreError::Conflict(_))
        ));
        assert_eq!(
            provider_projection_counts(&reopened, "acc-one"),
            before_conflict
        );

        let second_account = sample_provider_batch("acc-two", "shared-batch", "cursor-1");
        assert!(
            reopened
                .apply_provider_batch(second_account)
                .expect("batch IDs are account scoped")
                .applied
        );
        assert_eq!(
            provider_projection_counts(&reopened, "acc-two"),
            (1, 1, 1, 1)
        );
        assert_eq!(
            reopened
                .connection
                .query_row("SELECT COUNT(*) FROM threads", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn provider_batch_partial_updates_keep_aggregates_and_missing_refs_are_atomic() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("provider-partial.db");
        let mut store = configured_provider_store(&path, &["acc-partial"]);
        store
            .apply_provider_batch(sample_provider_batch(
                "acc-partial",
                "batch-initial",
                "cursor-initial",
            ))
            .expect("initial batch applies");
        let aggregate_before: (String, String, i64, i64, i64, i64) = store
            .connection
            .query_row(
                "SELECT subject, snippet, latest_at, message_count,
                        remote_in_inbox, remote_unread
                 FROM threads
                 WHERE account_id = 'acc-partial'",
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
            .expect("authoritative aggregate snapshot");

        let mut message_only = after_cursor(
            sample_provider_batch("acc-partial", "batch-message-only", "cursor-message-only"),
            "cursor-initial",
        );
        message_only.thread_upserts.clear();
        message_only.container_upserts.clear();
        message_only.membership_changes.clear();
        store
            .apply_provider_batch(message_only)
            .expect("message-only partial batch applies");
        assert!(
            !store
                .apply_provider_batch(sample_provider_batch(
                    "acc-partial",
                    "batch-initial",
                    "cursor-initial",
                ))
                .expect("an old receipt replays after the cursor advances")
                .applied,
            "receipt replay must win over a now-stale cursor precondition"
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT cursor FROM provider_sync_cursors
                     WHERE account_id = 'acc-partial' AND scope = 'a:v1'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "cursor-message-only"
        );
        let aggregate_after: (String, String, i64, i64, i64, i64) = store
            .connection
            .query_row(
                "SELECT subject, snippet, latest_at, message_count,
                        remote_in_inbox, remote_unread
                 FROM threads
                 WHERE account_id = 'acc-partial'",
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
            .expect("aggregate after partial batch");
        assert_eq!(aggregate_after, aggregate_before);
        assert_eq!(
            provider_projection_counts(&store, "acc-partial"),
            (1, 1, 1, 2)
        );

        let missing_path = directory.path().join("provider-missing-ref.db");
        let mut missing_store = configured_provider_store(&missing_path, &["acc-missing"]);
        let mut missing_value = serde_json::to_value(sample_provider_batch(
            "acc-missing",
            "batch-missing-ref",
            "cursor-missing-ref",
        ))
        .expect("sample batch serializes");
        missing_value["threadUpserts"] = serde_json::json!([]);
        missing_value["messageUpserts"][0]["identity"]["remoteThreadId"] =
            serde_json::json!("missing-thread");
        let missing_batch: ProviderBatch =
            serde_json::from_value(missing_value).expect("missing-reference batch validates");
        assert!(matches!(
            missing_store.apply_provider_batch(missing_batch),
            Err(StoreError::Validation(_))
        ));
        assert_eq!(
            provider_projection_counts(&missing_store, "acc-missing"),
            (0, 0, 0, 0)
        );
        assert_eq!(
            missing_store
                .connection
                .query_row(
                    "SELECT
                       (SELECT COUNT(*) FROM provider_containers WHERE account_id = 'acc-missing'),
                       (SELECT COUNT(*) FROM provider_sync_cursors WHERE account_id = 'acc-missing'),
                       (SELECT COUNT(*) FROM threads WHERE account_id = 'acc-missing'),
                       (SELECT COUNT(*) FROM messages
                        JOIN threads ON threads.id = messages.thread_id
                        WHERE threads.account_id = 'acc-missing')",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, i64>(3)?,
                        ))
                    },
                )
                .unwrap(),
            (0, 0, 0, 0),
            "a failed dependency must roll back every earlier projection write"
        );

        let cursor_path = directory.path().join("provider-long-scope.db");
        let mut cursor_store = configured_provider_store(&cursor_path, &["acc-cursor"]);
        let remote_container_id = "x".repeat(2_048);
        let mut cursor_value = serde_json::to_value(provider_delta_batch(
            "acc-cursor",
            "batch-long-scope",
            "cursor-long-scope",
            serde_json::json!([]),
            serde_json::json!([]),
        ))
        .expect("cursor batch serializes");
        cursor_value["cursor"]["scope"] = serde_json::json!({
            "kind": "container",
            "remoteContainerId": remote_container_id
        });
        let cursor_batch: ProviderBatch =
            serde_json::from_value(cursor_value).expect("maximum container cursor scope validates");
        cursor_store
            .apply_provider_batch(cursor_batch)
            .expect("maximum container cursor scope fits durable schema");
        assert_eq!(
            cursor_store
                .connection
                .query_row(
                    "SELECT length(CAST(scope AS BLOB)) FROM provider_sync_cursors
                     WHERE account_id = 'acc-cursor'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            2_053
        );
    }

    #[test]
    fn provider_search_index_is_exact_across_replacement_replay_rethread_and_deletion() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("provider-search-index.db");
        let mut store = configured_provider_store(&path, &["acc-search"]);
        let alpha = provider_search_thread(
            "acc-search",
            "thread-alpha",
            "Alpha subject",
            "sender-only@example.test, recipient-only@example.test",
            1,
        );
        let beta = provider_search_thread(
            "acc-search",
            "thread-beta",
            "Beta subject",
            "sender-only@example.test, recipient-only@example.test",
            0,
        );
        let initial = provider_search_batch(
            "acc-search",
            "search-initial",
            "search-cursor-1",
            None,
            serde_json::json!([alpha.clone(), beta.clone()]),
            serde_json::json!([provider_search_message(
                "acc-search",
                "thread-alpha",
                "oldbodytoken",
                "message-r1"
            )]),
            serde_json::json!([]),
        );
        store
            .apply_provider_batch(initial)
            .expect("initial search projection applies");

        let (alpha_id, beta_id, message_id): (i64, i64, i64) = store
            .connection
            .query_row(
                "SELECT
                   (SELECT thread_id FROM provider_thread_refs
                    WHERE account_id = 'acc-search' AND remote_thread_id = 'thread-alpha'),
                   (SELECT thread_id FROM provider_thread_refs
                    WHERE account_id = 'acc-search' AND remote_thread_id = 'thread-beta'),
                   (SELECT message_id FROM provider_message_refs
                    WHERE account_id = 'acc-search' AND remote_message_id = 'search-message')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("stable provider mappings");
        assert_eq!(search_thread_ids(&store, "oldbodytoken"), vec![alpha_id]);
        assert_eq!(
            search_thread_ids(&store, "from:sender-only@example.test"),
            vec![alpha_id]
        );
        assert!(search_thread_ids(&store, "from:recipient-only@example.test").is_empty());
        assert_eq!(
            search_thread_ids(&store, "to:recipient-only@example.test"),
            vec![alpha_id]
        );
        assert_eq!(
            search_thread_ids(&store, "to:cc-only@example.test"),
            vec![alpha_id]
        );
        assert_eq!(
            search_thread_ids(&store, "to:bcc-only@example.test"),
            vec![alpha_id]
        );
        assert!(search_thread_ids(&store, "to:sender-only@example.test").is_empty());
        assert!(search_thread_ids(&store, "from:cc-only@example.test").is_empty());
        assert_eq!(
            fts_rows(&store),
            vec![
                (
                    alpha_id,
                    "Alpha subject".into(),
                    "sender-only@example.test, recipient-only@example.test".into(),
                    "oldbodytoken".into(),
                    String::new(),
                ),
                (
                    beta_id,
                    "Beta subject".into(),
                    "sender-only@example.test, recipient-only@example.test".into(),
                    String::new(),
                    String::new(),
                ),
            ]
        );

        let replacement = provider_search_batch(
            "acc-search",
            "search-replacement",
            "search-cursor-2",
            Some("search-cursor-1"),
            serde_json::json!([]),
            serde_json::json!([provider_search_message(
                "acc-search",
                "thread-alpha",
                "newbodytoken",
                "message-r2"
            )]),
            serde_json::json!([]),
        );
        store
            .apply_provider_batch(replacement.clone())
            .expect("same provider message is replaced");
        assert!(search_thread_ids(&store, "oldbodytoken").is_empty());
        assert_eq!(search_thread_ids(&store, "newbodytoken"), vec![alpha_id]);
        let after_replacement = fts_rows(&store);
        assert_eq!(
            after_replacement
                .iter()
                .find(|row| row.0 == alpha_id)
                .map(|row| row.3.as_str()),
            Some("newbodytoken")
        );
        assert!(
            !store
                .apply_provider_batch(replacement)
                .expect("replacement replay is accepted")
                .applied
        );
        assert_eq!(
            fts_rows(&store),
            after_replacement,
            "replay is an exact no-op"
        );

        let operation = store
            .apply_thread_action(alpha_id, "star")
            .expect("local pending intent is durable before rethread");
        let moved_alpha = provider_search_thread(
            "acc-search",
            "thread-alpha",
            "Alpha subject",
            "sender-only@example.test, recipient-only@example.test",
            0,
        );
        let moved_beta = provider_search_thread(
            "acc-search",
            "thread-beta",
            "Beta subject",
            "sender-only@example.test, recipient-only@example.test",
            1,
        );
        let rethread = provider_search_batch(
            "acc-search",
            "search-rethread",
            "search-cursor-3",
            Some("search-cursor-2"),
            serde_json::json!([moved_alpha, moved_beta]),
            serde_json::json!([provider_search_message(
                "acc-search",
                "thread-beta",
                "newbodytoken",
                "message-r3"
            )]),
            serde_json::json!([]),
        );
        store
            .apply_provider_batch(rethread)
            .expect("provider rethread applies");
        assert_eq!(search_thread_ids(&store, "newbodytoken"), vec![beta_id]);
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT messages.id, messages.thread_id,
                            provider_message_refs.remote_thread_id
                     FROM provider_message_refs
                     JOIN messages ON messages.id = provider_message_refs.message_id
                     WHERE provider_message_refs.account_id = 'acc-search'
                       AND provider_message_refs.remote_message_id = 'search-message'",
                    [],
                    |row| Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    )),
                )
                .unwrap(),
            (message_id, beta_id, "thread-beta".into()),
            "rethread preserves local message identity and updates both mappings"
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT thread_id FROM operations WHERE id = ?1",
                    [&operation.id],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            alpha_id,
            "thread-level pending intent remains attached to its original thread"
        );
        let after_rethread = fts_rows(&store);
        assert_eq!(
            after_rethread
                .iter()
                .find(|row| row.0 == alpha_id)
                .map(|row| row.3.as_str()),
            Some("")
        );
        assert_eq!(
            after_rethread
                .iter()
                .find(|row| row.0 == beta_id)
                .map(|row| row.3.as_str()),
            Some("newbodytoken")
        );

        let deletion = provider_search_batch(
            "acc-search",
            "search-delete",
            "search-cursor-4",
            Some("search-cursor-3"),
            serde_json::json!([]),
            serde_json::json!([]),
            serde_json::json!([{
                "target": {
                    "kind": "message",
                    "identity": {
                        "muxAccountId": "acc-search",
                        "remoteMessageId": "search-message",
                        "remoteThreadId": "thread-beta"
                    }
                },
                "observedAt": 6000,
                "cursor": "search-cursor-4"
            }]),
        );
        assert!(store
            .apply_provider_batch_with_failpoint(
                deletion.clone(),
                ProviderBatchFailpoint::AfterProjection,
            )
            .is_err());
        assert_eq!(
            search_thread_ids(&store, "newbodytoken"),
            vec![beta_id],
            "failed projection and FTS mutations roll back together"
        );
        store
            .apply_provider_batch(deletion.clone())
            .expect("message tombstone applies");
        assert!(search_thread_ids(&store, "newbodytoken").is_empty());
        assert!(search_thread_ids(&store, "from:sender-only@example.test").is_empty());
        assert!(search_thread_ids(&store, "to:recipient-only@example.test").is_empty());
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT remote_deleted FROM messages WHERE id = ?1",
                    [message_id],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(
            fts_rows(&store)
                .iter()
                .find(|row| row.0 == beta_id)
                .map(|row| row.3.as_str()),
            Some(""),
            "a deleted message contributes no searchable body"
        );
        let after_deletion = fts_rows(&store);
        assert!(
            !store
                .apply_provider_batch(deletion)
                .expect("deletion replay is accepted")
                .applied
        );
        assert_eq!(fts_rows(&store), after_deletion);

        let thread_deletion = provider_search_batch(
            "acc-search",
            "search-delete-thread",
            "search-cursor-5",
            Some("search-cursor-4"),
            serde_json::json!([]),
            serde_json::json!([]),
            serde_json::json!([{
                "target": {
                    "kind": "thread",
                    "identity": {
                        "muxAccountId": "acc-search",
                        "remoteThreadId": "thread-beta"
                    }
                },
                "observedAt": 7000,
                "cursor": "search-cursor-5"
            }]),
        );
        store
            .apply_provider_batch(thread_deletion.clone())
            .expect("thread tombstone applies");
        assert!(fts_rows(&store).iter().all(|row| row.0 != beta_id));
        assert!(search_thread_ids(&store, "subject:\"Beta subject\"").is_empty());
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT remote_deleted FROM threads WHERE id = ?1",
                    [beta_id],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1,
            "thread projection and its FTS row are cleared in one commit"
        );
        let final_fts = fts_rows(&store);
        drop(store);
        let mut reopened = MuxStore::open(&path, false).expect("search projection reopens");
        assert!(
            !reopened
                .apply_provider_batch(thread_deletion)
                .expect("post-restart replay is accepted")
                .applied
        );
        assert_eq!(
            fts_rows(&reopened),
            final_fts,
            "post-restart replay leaves the exact index unchanged"
        );
    }

    #[test]
    fn schema_v13_rebuilds_stale_search_projection_once_and_idempotently() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("stale-search-index.db");
        let store = MuxStore::open(&path, false).expect("current schema created");
        store
            .connection
            .execute_batch(
                "INSERT INTO accounts(id, name, email, color, provider)
                   VALUES('migration-account', 'Migration', 'migration@example.test', '#111111', 'jmap');
                 INSERT INTO threads(
                   id, account_id, subject, participants, snippet, latest_at, message_count,
                   remote_in_inbox, remote_unread, remote_starred, has_attachment,
                   has_invite, has_link, has_from_me, category, attachment_names
                 ) VALUES(
                   7001, 'migration-account', 'Exact migration subject',
                   'exact-participant@example.test', 'Migration snippet', 7000, 2,
                   1, 0, 0, 0, 0, 0, 0, 'primary', 'exact-attachment.txt'
                 );
                 INSERT INTO messages(
                   id, thread_id, sender_name, sender_email, recipients, sent_at,
                   body_text, is_from_me, remote_deleted
                 ) VALUES(
                   7101, 7001, 'Live', 'live@example.test', 'to@example.test', 1,
                   'live migration token', 0, 0
                 );
                 INSERT INTO messages(
                   id, thread_id, sender_name, sender_email, recipients, sent_at,
                   body_text, is_from_me, remote_deleted
                 ) VALUES(
                   7102, 7001, 'Deleted', 'deleted@example.test', 'to@example.test', 2,
                   'deleted migration token', 0, 1
                 );
                 INSERT INTO messages_fts(thread_id, subject, participants, body, attachment_names)
                   VALUES(7001, 'stale one', 'stale one', 'stale body one', 'stale one');
                 INSERT INTO messages_fts(thread_id, subject, participants, body, attachment_names)
                   VALUES(7001, 'stale two', 'stale two', 'stale body two', 'stale two');
                 UPDATE meta SET value = '12' WHERE key = 'schema_version';",
            )
            .expect("stale v12 index fixture");
        drop(store);

        let migrated = MuxStore::open(&path, false).expect("v12 search index migrates");
        assert_eq!(
            migrated
                .connection
                .query_row(
                    "SELECT value FROM meta WHERE key = 'schema_version'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "23"
        );
        let expected = vec![(
            7001,
            "Exact migration subject".into(),
            "exact-participant@example.test".into(),
            "live migration token".into(),
            "exact-attachment.txt".into(),
        )];
        assert_eq!(fts_rows(&migrated), expected);
        assert_eq!(
            search_thread_ids(&migrated, "live migration token"),
            vec![7001]
        );
        assert!(search_thread_ids(&migrated, "deleted migration token").is_empty());
        drop(migrated);

        let reopened = MuxStore::open(&path, false).expect("v14 database reopens");
        assert_eq!(
            fts_rows(&reopened),
            expected,
            "reopening the migrated database does not append or duplicate FTS rows"
        );
    }

    #[test]
    fn schema_v14_preserves_v1_receipts_and_writes_v2_after_migration() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("provider-receipt-v14.db");
        let mut store = configured_provider_store(&path, &["acc-v1", "acc-v2"]);
        let legacy_batch = sample_provider_batch("acc-v1", "legacy-batch", "legacy-cursor");
        let v2_batch = sample_provider_batch("acc-v2", "v2-batch", "v2-cursor");
        store
            .apply_provider_batch(legacy_batch.clone())
            .expect("legacy fixture projection applies");
        let legacy_fingerprint = provider_batch_fingerprint_v1(&legacy_batch);
        store
            .connection
            .execute(
                "UPDATE provider_applied_batches
                 SET fingerprint = ?1, fingerprint_version = 1
                 WHERE account_id = 'acc-v1' AND batch_id = 'legacy-batch'",
                [legacy_fingerprint.as_slice()],
            )
            .expect("handcraft preserved v1 receipt");
        install_test_provider_receipt_schema(&store.connection, "= 1");
        store
            .connection
            .execute(
                "UPDATE meta SET value = '13' WHERE key = 'schema_version'",
                [],
            )
            .expect("simulate v13 receipt schema");
        let receipts_before = store
            .connection
            .prepare(
                "SELECT account_id, batch_id, fingerprint, fingerprint_version,
                        cursor_scope, cursor, applied_at
                 FROM provider_applied_batches ORDER BY account_id, batch_id",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            receipts_before
                .iter()
                .map(|receipt| receipt.3)
                .collect::<Vec<_>>(),
            vec![1]
        );
        drop(store);

        let mut migrated = MuxStore::open(&path, false).expect("v13 receipts migrate to v14");
        assert_eq!(
            migrated
                .connection
                .query_row(
                    "SELECT value FROM meta WHERE key = 'schema_version'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "23"
        );
        let receipts_after = migrated
            .connection
            .prepare(
                "SELECT account_id, batch_id, fingerprint, fingerprint_version,
                        cursor_scope, cursor, applied_at
                 FROM provider_applied_batches ORDER BY account_id, batch_id",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(
            receipts_after, receipts_before,
            "migration preserves both encodings"
        );
        let receipt_schema: String = migrated
            .connection
            .query_row(
                "SELECT sql FROM sqlite_master
                 WHERE type = 'table' AND name = 'provider_applied_batches'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(receipt_schema.contains("fingerprint_version IN (1, 2)"));
        assert_eq!(
            migrated
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                     WHERE type = 'index' AND name = 'provider_batches_applied_idx'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1,
            "receipt lookup index survives the table rebuild"
        );
        let receipt_foreign_keys = migrated
            .connection
            .prepare("PRAGMA foreign_key_list(provider_applied_batches)")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(2)?, row.get::<_, String>(3)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(receipt_foreign_keys.contains(&("provider_accounts".into(), "account_id".into())));
        assert_eq!(
            migrated
                .apply_provider_batch(legacy_batch.clone())
                .expect_err("v1 receipt cannot verify the prior cursor")
                .to_string(),
            "Provider batch receipt uses unverifiable legacy fingerprint version 1"
        );
        let mut changed_legacy = legacy_batch;
        changed_legacy.expected_prior_cursor = Some(
            OpaqueSyncCursor::new("changed-legacy-prior").expect("bounded legacy prior cursor"),
        );
        assert_eq!(
            migrated
                .apply_provider_batch(changed_legacy)
                .expect_err("all v1 receipts fail with the stable legacy conflict")
                .to_string(),
            "Provider batch receipt uses unverifiable legacy fingerprint version 1"
        );
        assert!(
            migrated
                .apply_provider_batch(v2_batch.clone())
                .expect("new post-migration batch applies with v2")
                .applied
        );
        assert_eq!(
            migrated
                .connection
                .query_row(
                    "SELECT fingerprint_version FROM provider_applied_batches
                     WHERE account_id = 'acc-v2' AND batch_id = 'v2-batch'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            2,
            "v14 writes only v2 receipts"
        );
        assert!(
            !migrated
                .apply_provider_batch(v2_batch.clone())
                .expect("new v2 receipt exact-replays")
                .applied
        );
        assert_eq!(
            migrated
                .connection
                .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        assert_eq!(
            migrated
                .connection
                .prepare("PRAGMA foreign_key_check")
                .unwrap()
                .query_map([], |_| Ok(()))
                .unwrap()
                .count(),
            0
        );
        drop(migrated);

        let mut reopened = MuxStore::open(&path, false).expect("v14 receipt schema reopens");
        assert!(
            !reopened
                .apply_provider_batch(v2_batch)
                .expect("exact v2 receipt replays after restart")
                .applied
        );
        assert_eq!(
            reopened
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                     WHERE name = 'provider_applied_batches_v14'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0,
            "idempotent reopen leaves no migration table"
        );
    }

    #[test]
    fn schema_v14_migrates_v12_receipts_without_a_version_column_as_v1() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("provider-receipt-v12.db");
        let mut store = configured_provider_store(&path, &["acc-v12"]);
        let legacy_batch = sample_provider_batch("acc-v12", "v12-batch", "v12-cursor");
        store
            .apply_provider_batch(legacy_batch.clone())
            .expect("legacy fixture projection applies");
        let legacy_fingerprint = provider_batch_fingerprint_v1(&legacy_batch);
        store
            .connection
            .execute(
                "UPDATE provider_applied_batches
                 SET fingerprint = ?1
                 WHERE account_id = 'acc-v12' AND batch_id = 'v12-batch'",
                [legacy_fingerprint.as_slice()],
            )
            .expect("install the legacy receipt fingerprint");
        install_v12_provider_receipt_schema(&store.connection);
        store
            .connection
            .execute(
                "UPDATE meta SET value = '12' WHERE key = 'schema_version'",
                [],
            )
            .expect("simulate the exact v12 receipt schema");
        drop(store);

        let mut migrated = MuxStore::open(&path, false)
            .expect("v12 receipt without a version column migrates to v14");
        let receipt = migrated
            .connection
            .query_row(
                "SELECT fingerprint, fingerprint_version, cursor_scope, cursor, applied_at
                 FROM provider_applied_batches
                 WHERE account_id = 'acc-v12' AND batch_id = 'v12-batch'",
                [],
                |row| {
                    Ok((
                        row.get::<_, Vec<u8>>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .expect("migrated v12 receipt");
        assert_eq!(receipt.0, legacy_fingerprint);
        assert_eq!(
            receipt.1, 1,
            "unversioned receipts use the legacy v1 format"
        );
        assert_eq!(receipt.2, "a:v1");
        assert_eq!(receipt.3, "v12-cursor");
        assert_eq!(receipt.4, 1_000, "migration preserves receipt time");
        assert_eq!(
            migrated
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                     WHERE type = 'index' AND name = 'provider_batches_applied_idx'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1,
            "receipt lookup index survives the v12 table rebuild"
        );
        assert_eq!(
            migrated
                .connection
                .query_row(
                    "SELECT value FROM meta WHERE key = 'schema_version'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "23"
        );
        assert_eq!(
            migrated
                .apply_provider_batch(legacy_batch)
                .expect_err("a preserved v1 receipt remains conservatively unverifiable")
                .to_string(),
            "Provider batch receipt uses unverifiable legacy fingerprint version 1"
        );
        assert_eq!(
            migrated
                .connection
                .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        assert_eq!(
            migrated
                .connection
                .prepare("PRAGMA foreign_key_check")
                .unwrap()
                .query_map([], |_| Ok(()))
                .unwrap()
                .count(),
            0
        );
        drop(migrated);

        let reopened = MuxStore::open(&path, false).expect("migrated v12 database reopens");
        assert_eq!(
            reopened
                .connection
                .query_row(
                    "SELECT fingerprint_version FROM provider_applied_batches
                     WHERE account_id = 'acc-v12' AND batch_id = 'v12-batch'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(
            reopened
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                     WHERE name = 'provider_applied_batches_v14'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0,
            "idempotent reopen leaves no migration table"
        );
    }

    #[test]
    fn schema_v14_receipt_rebuild_rolls_back_on_an_unsupported_version() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("invalid-provider-receipt-v14.db");
        let mut store = configured_provider_store(&path, &["acc-invalid-receipt"]);
        store
            .apply_provider_batch(sample_provider_batch(
                "acc-invalid-receipt",
                "invalid-version-batch",
                "invalid-version-cursor",
            ))
            .expect("receipt fixture applies");
        install_test_provider_receipt_schema(&store.connection, "IN (1, 2, 3)");
        store
            .connection
            .execute_batch(
                "UPDATE provider_applied_batches SET fingerprint_version = 3;
                 UPDATE meta SET value = '13' WHERE key = 'schema_version';",
            )
            .expect("construct invalid v13 receipt version");
        drop(store);

        MuxStore::open(&path, false)
            .err()
            .expect("unsupported receipt version must fail migration");
        let verify = Connection::open(&path).expect("failed migration database remains readable");
        assert_eq!(
            verify
                .query_row(
                    "SELECT value FROM meta WHERE key = 'schema_version'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "13",
            "failed migration cannot advance the schema version"
        );
        assert_eq!(
            verify
                .query_row(
                    "SELECT fingerprint_version FROM provider_applied_batches",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            3,
            "failed migration preserves the original receipt table"
        );
        assert_eq!(
            verify
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                     WHERE name = 'provider_applied_batches_v14'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0,
            "failed migration rolls back its temporary table"
        );
        assert_eq!(
            verify
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                     WHERE type = 'index' AND name = 'provider_batches_applied_idx'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1,
            "failed migration preserves the original receipt index"
        );
        assert_eq!(
            verify
                .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
        assert_eq!(
            verify
                .prepare("PRAGMA foreign_key_check")
                .unwrap()
                .query_map([], |_| Ok(()))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn provider_batch_body_updates_are_monotonic_and_preserve_safe_content() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("provider-body-merge.db");
        let mut store = configured_provider_store(&path, &["acc-body"]);
        store
            .apply_provider_batch(sample_provider_batch(
                "acc-body",
                "batch-complete",
                "cursor-complete",
            ))
            .expect("complete body applies");
        store
            .connection
            .execute(
                "UPDATE messages SET
                   body_html = '<p>Sanitized cached body</p>',
                   blocked_remote_resources = 3
                 WHERE id = (
                   SELECT message_id FROM provider_message_refs
                   WHERE account_id = 'acc-body' AND remote_message_id = 'remote-message-1'
                 )",
                [],
            )
            .expect("simulate separately sanitized cached MIME content");

        let mut unavailable_value = serde_json::to_value(after_cursor(
            sample_provider_batch("acc-body", "batch-unavailable", "cursor-unavailable"),
            "cursor-complete",
        ))
        .expect("unavailable batch serializes");
        unavailable_value["threadUpserts"] = serde_json::json!([]);
        unavailable_value["containerUpserts"] = serde_json::json!([]);
        unavailable_value["membershipChanges"] = serde_json::json!([]);
        unavailable_value["messageUpserts"][0]["bodyState"] = serde_json::json!("unavailable");
        unavailable_value["messageUpserts"][0]["bodyText"] = serde_json::json!("");
        store
            .apply_provider_batch(
                serde_json::from_value(unavailable_value)
                    .expect("metadata-only body batch validates"),
            )
            .expect("metadata-only update preserves cached content");
        assert_eq!(
            provider_cached_body(&store, "acc-body"),
            (
                "Normalized provider message body.".into(),
                "<p>Sanitized cached body</p>".into(),
                3,
                "normalized".into(),
                0,
            )
        );

        let mut truncated_value = serde_json::to_value(after_cursor(
            sample_provider_batch("acc-body", "batch-truncated", "cursor-truncated"),
            "cursor-unavailable",
        ))
        .expect("truncated batch serializes");
        truncated_value["threadUpserts"] = serde_json::json!([]);
        truncated_value["containerUpserts"] = serde_json::json!([]);
        truncated_value["membershipChanges"] = serde_json::json!([]);
        truncated_value["messageUpserts"][0]["bodyState"] = serde_json::json!("truncated");
        truncated_value["messageUpserts"][0]["bodyText"] =
            serde_json::json!("Partial provider body");
        store
            .apply_provider_batch(
                serde_json::from_value(truncated_value).expect("truncated body batch validates"),
            )
            .expect("truncated update does not downgrade complete content");
        assert_eq!(
            provider_cached_body(&store, "acc-body"),
            (
                "Normalized provider message body.".into(),
                "<p>Sanitized cached body</p>".into(),
                3,
                "normalized".into(),
                0,
            )
        );

        let mut refreshed_value = serde_json::to_value(after_cursor(
            sample_provider_batch("acc-body", "batch-refreshed", "cursor-refreshed"),
            "cursor-truncated",
        ))
        .expect("refreshed batch serializes");
        refreshed_value["threadUpserts"] = serde_json::json!([]);
        refreshed_value["containerUpserts"] = serde_json::json!([]);
        refreshed_value["membershipChanges"] = serde_json::json!([]);
        refreshed_value["messageUpserts"][0]["bodyText"] =
            serde_json::json!("Refreshed complete provider body.");
        store
            .apply_provider_batch(
                serde_json::from_value(refreshed_value).expect("complete refresh validates"),
            )
            .expect("complete body refresh applies");
        assert_eq!(
            provider_cached_body(&store, "acc-body"),
            (
                "Refreshed complete provider body.".into(),
                "<p>Sanitized cached body</p>".into(),
                3,
                "normalized".into(),
                0,
            ),
            "plain provider batches never erase separately sanitized HTML metadata"
        );
        let truncated_path = directory.path().join("provider-truncated-state.db");
        let mut truncated_store = configured_provider_store(&truncated_path, &["acc-truncated"]);
        let mut initial_truncated = serde_json::to_value(sample_provider_batch(
            "acc-truncated",
            "batch-initial-truncated",
            "cursor-initial-truncated",
        ))
        .expect("initial truncated batch serializes");
        initial_truncated["messageUpserts"][0]["bodyState"] = serde_json::json!("truncated");
        initial_truncated["messageUpserts"][0]["bodyText"] =
            serde_json::json!("Initial partial body");
        truncated_store
            .apply_provider_batch(
                serde_json::from_value(initial_truncated).expect("truncated body validates"),
            )
            .expect("truncated body applies");
        assert_eq!(
            truncated_store
                .connection
                .query_row(
                    "SELECT messages.body_text, provider_message_refs.body_state,
                            provider_message_refs.body_is_truncated
                     FROM provider_message_refs
                     JOIN messages ON messages.id = provider_message_refs.message_id
                     WHERE provider_message_refs.account_id = 'acc-truncated'",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    },
                )
                .unwrap(),
            ("Initial partial body".into(), "normalized".into(), 1)
        );

        let mut completed = after_cursor(
            sample_provider_batch(
                "acc-truncated",
                "batch-complete-later",
                "cursor-complete-later",
            ),
            "cursor-initial-truncated",
        );
        completed.thread_upserts.clear();
        completed.container_upserts.clear();
        completed.membership_changes.clear();
        truncated_store
            .apply_provider_batch(completed)
            .expect("complete body upgrades truncated cache");
        assert_eq!(
            truncated_store
                .connection
                .query_row(
                    "SELECT messages.body_text, provider_message_refs.body_is_truncated
                     FROM provider_message_refs
                     JOIN messages ON messages.id = provider_message_refs.message_id
                     WHERE provider_message_refs.account_id = 'acc-truncated'",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .unwrap(),
            ("Normalized provider message body.".into(), 0)
        );
    }

    #[test]
    fn provider_batch_failpoints_roll_back_projection_cursor_and_receipt() {
        let directory = tempdir().expect("temporary directory");
        for (name, failpoint) in [
            ("projection", ProviderBatchFailpoint::AfterProjection),
            ("cursor", ProviderBatchFailpoint::AfterCursor),
        ] {
            let path = directory.path().join(format!("provider-fail-{name}.db"));
            let mut store = configured_provider_store(&path, &["acc-fail"]);
            let batch = sample_provider_batch("acc-fail", "batch-fail", "cursor-fail");
            assert!(matches!(
                store.apply_provider_batch_with_failpoint(batch.clone(), failpoint),
                Err(StoreError::Conflict(_))
            ));
            drop(store);

            let mut reopened = MuxStore::open(&path, false).expect("rolled back store reopens");
            assert_eq!(
                provider_projection_counts(&reopened, "acc-fail"),
                (0, 0, 0, 0)
            );
            assert_eq!(
                reopened
                    .connection
                    .query_row(
                        "SELECT COUNT(*) FROM provider_sync_cursors
                         WHERE account_id = 'acc-fail'",
                        [],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap(),
                0
            );
            assert_eq!(
                reopened
                    .connection
                    .query_row("SELECT COUNT(*) FROM threads", [], |row| row
                        .get::<_, i64>(0))
                    .unwrap(),
                0,
                "failed batch must leave no orphan local rows"
            );
            assert!(
                reopened
                    .apply_provider_batch(batch.clone())
                    .expect("rolled back batch safely reapplies")
                    .applied
            );
            assert_eq!(
                provider_projection_counts(&reopened, "acc-fail"),
                (1, 1, 1, 1)
            );
        }
    }

    #[test]
    fn provider_batch_moves_and_tombstones_preserve_local_intent_and_identity() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("provider-moves.db");
        let mut store = configured_provider_store(&path, &["acc-move"]);
        store
            .apply_provider_batch(sample_provider_batch(
                "acc-move",
                "batch-initial",
                "cursor-initial",
            ))
            .expect("initial batch applies");
        let (local_thread_id, local_message_id): (i64, i64) = store
            .connection
            .query_row(
                "SELECT provider_thread_refs.thread_id, provider_message_refs.message_id
                 FROM provider_thread_refs
                 JOIN provider_message_refs ON
                   provider_message_refs.account_id = provider_thread_refs.account_id AND
                   provider_message_refs.remote_thread_id = provider_thread_refs.remote_thread_id
                 WHERE provider_thread_refs.account_id = 'acc-move'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("stable local mappings");

        let wrong_thread_membership = after_cursor(
            provider_delta_batch(
                "acc-move",
                "batch-wrong-membership-thread",
                "cursor-wrong-membership-thread",
                serde_json::json!([{
                    "kind": "upsert",
                    "membership": {
                        "message": {
                            "muxAccountId": "acc-move",
                            "remoteMessageId": "remote-message-1",
                            "remoteThreadId": "different-thread"
                        },
                        "container": {
                            "muxAccountId": "acc-move",
                            "remoteContainerId": "archive"
                        }
                    }
                }]),
                serde_json::json!([]),
            ),
            "cursor-initial",
        );
        assert!(matches!(
            store.apply_provider_batch(wrong_thread_membership),
            Err(StoreError::Validation(_))
        ));
        assert_eq!(
            memberships_for_message(&store, "acc-move", "remote-message-1"),
            vec!["inbox"]
        );

        let move_to_archive = after_cursor(
            provider_delta_batch(
                "acc-move",
                "batch-move-archive",
                "cursor-archive",
                serde_json::json!([{
                    "kind": "remove",
                    "membership": {
                        "message": {
                            "muxAccountId": "acc-move",
                            "remoteMessageId": "remote-message-1",
                            "remoteThreadId": "remote-thread-1"
                        },
                        "container": {
                            "muxAccountId": "acc-move",
                            "remoteContainerId": "inbox"
                        }
                    }
                }, {
                    "kind": "upsert",
                    "membership": {
                        "message": {
                            "muxAccountId": "acc-move",
                            "remoteMessageId": "remote-message-1",
                            "remoteThreadId": "remote-thread-1"
                        },
                        "container": {
                            "muxAccountId": "acc-move",
                            "remoteContainerId": "archive"
                        }
                    }
                }]),
                serde_json::json!([]),
            ),
            "cursor-initial",
        );
        store
            .apply_provider_batch(move_to_archive.clone())
            .expect("container move applies");
        let memberships = |store: &MuxStore| {
            store
                .connection
                .prepare(
                    "SELECT remote_container_id FROM provider_container_memberships
                     WHERE account_id = 'acc-move' AND remote_message_id = 'remote-message-1'
                     ORDER BY remote_container_id",
                )
                .unwrap()
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert_eq!(memberships(&store), vec!["archive"]);
        assert!(
            !store
                .apply_provider_batch(move_to_archive)
                .expect("move replay is a no-op")
                .applied
        );

        let move_back = after_cursor(
            provider_delta_batch(
                "acc-move",
                "batch-move-inbox",
                "cursor-inbox",
                serde_json::json!([{
                    "kind": "remove",
                    "membership": {
                        "message": {
                            "muxAccountId": "acc-move",
                            "remoteMessageId": "remote-message-1",
                            "remoteThreadId": "remote-thread-1"
                        },
                        "container": {
                            "muxAccountId": "acc-move",
                            "remoteContainerId": "archive"
                        }
                    }
                }, {
                    "kind": "upsert",
                    "membership": {
                        "message": {
                            "muxAccountId": "acc-move",
                            "remoteMessageId": "remote-message-1",
                            "remoteThreadId": "remote-thread-1"
                        },
                        "container": {
                            "muxAccountId": "acc-move",
                            "remoteContainerId": "inbox"
                        }
                    }
                }]),
                serde_json::json!([]),
            ),
            "cursor-archive",
        );
        assert!(store
            .apply_provider_batch_with_failpoint(
                move_back.clone(),
                ProviderBatchFailpoint::AfterProjection,
            )
            .is_err());
        assert_eq!(memberships(&store), vec!["archive"]);
        store
            .apply_provider_batch(move_back)
            .expect("move applies after rollback");
        assert_eq!(memberships(&store), vec!["inbox"]);

        let operation = store
            .apply_thread_action(local_thread_id, "star")
            .expect("durable local intent");
        store
            .connection
            .execute(
                "INSERT INTO snoozes(thread_id, wake_at, created_at, previous_location)
                 VALUES(?1, 999999, 1, 'inbox')",
                [local_thread_id],
            )
            .expect("Mux-owned metadata");

        let message_delete = after_cursor(
            provider_delta_batch(
                "acc-move",
                "batch-delete-message",
                "cursor-delete-message",
                serde_json::json!([]),
                serde_json::json!([{
                    "target": {
                        "kind": "message",
                        "identity": {
                            "muxAccountId": "acc-move",
                            "remoteMessageId": "remote-message-1",
                            "remoteThreadId": "remote-thread-1"
                        }
                    },
                    "observedAt": 3000,
                    "cursor": null
                }]),
            ),
            "cursor-inbox",
        );
        store
            .apply_provider_batch(message_delete)
            .expect("message tombstone applies");
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT remote_deleted FROM messages WHERE id = ?1",
                    [local_message_id],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(provider_projection_counts(&store, "acc-move").1, 1);
        assert!(store
            .get_thread_messages(MessagePageInput {
                thread_id: local_thread_id,
                cursor: None,
                limit: Some(50),
            })
            .expect("surviving thread remains readable")
            .messages
            .is_empty());

        let mut resurrect_message = after_cursor(
            sample_provider_batch(
                "acc-move",
                "batch-resurrect-message",
                "cursor-resurrect-message",
            ),
            "cursor-delete-message",
        );
        resurrect_message.thread_upserts.clear();
        resurrect_message.container_upserts.clear();
        resurrect_message.membership_changes.clear();
        store
            .apply_provider_batch(resurrect_message)
            .expect("message mapping resurrects");
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT message_id FROM provider_message_refs
                     WHERE account_id = 'acc-move' AND remote_message_id = 'remote-message-1'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            local_message_id
        );

        let thread_delete = after_cursor(
            provider_delta_batch(
                "acc-move",
                "batch-delete-thread",
                "cursor-delete-thread",
                serde_json::json!([]),
                serde_json::json!([{
                    "target": {
                        "kind": "thread",
                        "identity": {
                            "muxAccountId": "acc-move",
                            "remoteThreadId": "remote-thread-1"
                        }
                    },
                    "observedAt": 4000,
                    "cursor": null
                }]),
            ),
            "cursor-resurrect-message",
        );
        store
            .connection
            .execute(
                "INSERT INTO messages(
                   thread_id, sender_name, sender_email, recipients, sent_at,
                   body_text, is_from_me, remote_deleted
                 ) VALUES(?1, 'Local Sender', 'local@example.com',
                          'recipient@example.com', 950, 'Local-only pending content', 1, 0)",
                [local_thread_id],
            )
            .expect("local-only message shares the provider-backed thread");
        let local_only_message_id = store.connection.last_insert_rowid();
        store
            .apply_provider_batch(thread_delete)
            .expect("thread tombstone applies");
        assert!(store
            .list_threads(ThreadPageInput {
                account_id: Some("acc-move".into()),
                view: Some("all".into()),
                cursor: None,
                limit: Some(50),
                hidden_account_ids: Vec::new(),
                container_id: None,
            })
            .expect("visible projection")
            .threads
            .is_empty());
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT thread_id FROM operations WHERE id = ?1",
                    [&operation.id],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            local_thread_id,
            "provider deletion must retain durable local intent"
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT remote_deleted FROM messages WHERE id = ?1",
                    [local_only_message_id],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0,
            "a provider tombstone must not mark local-only content remotely deleted"
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM snoozes WHERE thread_id = ?1",
                    [local_thread_id],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1,
            "provider deletion must retain Mux-owned metadata"
        );
        assert_eq!(provider_projection_counts(&store, "acc-move").0, 1);

        store
            .apply_provider_batch(after_cursor(
                sample_provider_batch(
                    "acc-move",
                    "batch-resurrect-thread",
                    "cursor-resurrect-thread",
                ),
                "cursor-delete-thread",
            ))
            .expect("thread and message resurrect through stable mappings");
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT thread_id FROM provider_thread_refs
                     WHERE account_id = 'acc-move' AND remote_thread_id = 'remote-thread-1'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            local_thread_id
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT remote_deleted FROM threads WHERE id = ?1",
                    [local_thread_id],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );

        let add_archive = after_cursor(
            provider_delta_batch(
                "acc-move",
                "batch-add-archive",
                "cursor-add-archive",
                serde_json::json!([{
                    "kind": "upsert",
                    "membership": {
                        "message": {
                            "muxAccountId": "acc-move",
                            "remoteMessageId": "remote-message-1",
                            "remoteThreadId": "remote-thread-1"
                        },
                        "container": {
                            "muxAccountId": "acc-move",
                            "remoteContainerId": "archive"
                        }
                    }
                }]),
                serde_json::json!([]),
            ),
            "cursor-resurrect-thread",
        );
        store
            .apply_provider_batch(add_archive)
            .expect("archive membership applies");
        let delete_archive = after_cursor(
            provider_delta_batch(
                "acc-move",
                "batch-delete-archive",
                "cursor-delete-archive",
                serde_json::json!([]),
                serde_json::json!([{
                    "target": {
                        "kind": "container",
                        "identity": {
                            "muxAccountId": "acc-move",
                            "remoteContainerId": "archive"
                        }
                    },
                    "observedAt": 5000,
                    "cursor": "c".repeat(16_384)
                }]),
            ),
            "cursor-add-archive",
        );
        store
            .apply_provider_batch(delete_archive)
            .expect("container tombstone applies");
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT is_deleted FROM provider_containers
                     WHERE account_id = 'acc-move' AND remote_id = 'archive'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM provider_container_memberships
                     WHERE account_id = 'acc-move' AND remote_container_id = 'archive'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT length(CAST(cursor AS BLOB)) FROM provider_tombstones
                     WHERE account_id = 'acc-move' AND object_kind = 'container'
                       AND remote_id = 'archive'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            16_384,
            "the maximum bounded tombstone cursor must fit durable storage"
        );
        let mut resurrect_containers = after_cursor(
            sample_provider_batch(
                "acc-move",
                "batch-resurrect-containers",
                "cursor-resurrect-containers",
            ),
            "cursor-delete-archive",
        );
        resurrect_containers.thread_upserts.clear();
        resurrect_containers.message_upserts.clear();
        resurrect_containers.membership_changes.clear();
        store
            .apply_provider_batch(resurrect_containers)
            .expect("container upsert clears its tombstone");
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT is_deleted FROM provider_containers
                     WHERE account_id = 'acc-move' AND remote_id = 'archive'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn failed_provider_migration_does_not_advance_version_or_leave_partial_tables() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("failed-provider-migration.db");
        let store = MuxStore::open(&path, false).expect("schema created");
        store
            .connection
            .execute_batch(
                "DROP TABLE provider_reconciliation_seen;
                 DROP TABLE provider_reconciliation_runs;
                 DROP TABLE provider_work_items;
                 DROP TABLE provider_message_keywords;
                 DROP TABLE provider_container_memberships;
                 DROP TABLE provider_message_refs;
                 DROP TABLE provider_thread_refs;
                 DROP TABLE provider_sync_cursors;
                 DROP TABLE provider_applied_batches;
                 DROP TABLE provider_tombstones;
                 DROP TABLE provider_containers;
                 DROP TABLE provider_capabilities;
                 DROP TABLE provider_accounts;
                 CREATE TABLE provider_thread_refs(dummy TEXT);
                 UPDATE meta SET value = '6' WHERE key = 'schema_version';",
            )
            .expect("construct incompatible v6 database");
        drop(store);

        MuxStore::open(&path, false)
            .err()
            .expect("incompatible migration must fail");
        let verify = Connection::open(&path).expect("failed database remains readable");
        assert_eq!(
            verify
                .query_row(
                    "SELECT value FROM meta WHERE key = 'schema_version'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "6"
        );
        assert_eq!(
            verify
                .query_row(
                    "SELECT EXISTS(
                       SELECT 1 FROM sqlite_master
                       WHERE type = 'table' AND name = 'provider_accounts'
                     )",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0,
            "tables created before the failure must roll back"
        );
        let malformed_columns = verify
            .prepare("PRAGMA table_info(provider_thread_refs)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(malformed_columns, vec!["dummy"]);
    }

    #[test]
    fn safe_demo_attachment_bytes_require_an_explicit_lookup() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("safe-content.db");
        let store = MuxStore::open(&path, true).expect("seeded store opens");
        let page = store
            .get_thread_messages(MessagePageInput {
                thread_id: DEMO_FIXTURE_THREAD_ID,
                cursor: None,
                limit: Some(100),
            })
            .expect("thread content page");
        let inline = page
            .attachments
            .iter()
            .find(|attachment| attachment.disposition == "inline")
            .expect("inline attachment metadata");
        assert_eq!(inline.media_type, "image/png");
        assert!(inline.byte_length > 0);
        let content = store
            .read_attachment(&inline.id)
            .expect("explicit content lookup");
        assert_eq!(content.filename, inline.filename);
        assert_eq!(content.media_type, inline.media_type);
        assert_eq!(content.byte_length, inline.byte_length);
        assert_eq!(
            BASE64_STANDARD.decode(&content.data_base64).unwrap().len() as i64,
            inline.byte_length
        );
        assert!(matches!(
            store.read_attachment("missing"),
            Err(StoreError::NotFound(_))
        ));
    }

    #[test]
    fn attachment_ipc_uses_bounded_base64_and_rejects_oversize_or_inconsistent_rows() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("attachment-ipc-bounds.db");
        let store = MuxStore::open(&path, true).expect("seeded store opens");
        let raw_cap = crate::mime_ingest::MAX_ATTACHMENT_BYTES as i64;
        store
            .connection
            .execute(
                "INSERT INTO attachments(
                   id, message_id, filename, media_type, byte_length,
                   content, content_id, disposition
                 ) VALUES('attachment-at-cap', 1, 'at-cap.bin',
                          'application/octet-stream', ?1, zeroblob(?1), '', 'attachment')",
                [raw_cap],
            )
            .unwrap();
        let at_cap = store
            .read_attachment("attachment-at-cap")
            .expect("the exact MIME attachment cap is IPC-readable");
        assert_eq!(at_cap.byte_length, raw_cap);
        assert_eq!(
            BASE64_STANDARD.decode(&at_cap.data_base64).unwrap().len() as i64,
            raw_cap
        );
        assert!(
            serde_json::to_vec(&at_cap).unwrap().len()
                <= crate::ipc_boundary::ATTACHMENT_CONTENT_BYTES
        );

        let over_cap = raw_cap + 1;
        store
            .connection
            .execute(
                "INSERT INTO attachments(
                   id, message_id, filename, media_type, byte_length,
                   content, content_id, disposition
                 ) VALUES('attachment-over-cap', 1, 'over-cap.bin',
                          'application/octet-stream', ?1, zeroblob(?1), '', 'attachment')",
                [over_cap],
            )
            .unwrap();
        assert!(matches!(
            store.read_attachment("attachment-over-cap"),
            Err(StoreError::Validation(ref message))
                if message == "Attachment content exceeds the 20971520-byte stored limit"
        ));

        store
            .connection
            .execute(
                "INSERT INTO attachments(
                   id, message_id, filename, media_type, byte_length,
                   content, content_id, disposition
                 ) VALUES('attachment-mismatch', 1, 'mismatch.bin',
                          'application/octet-stream', 2, zeroblob(3), '', 'attachment')",
                [],
            )
            .unwrap();
        assert!(matches!(
            store.read_attachment("attachment-mismatch"),
            Err(StoreError::Validation(ref message))
                if message == "Attachment byte metadata is inconsistent"
        ));
    }

    #[test]
    fn long_demo_threads_are_large_and_seed_only_once() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("long-demo.db");
        let store = MuxStore::open(&path, true).expect("fresh store opens");
        let threads = store
            .list_threads(ThreadPageInput {
                account_id: None,
                view: Some("all".into()),
                cursor: None,
                limit: Some(100),
                hidden_account_ids: Vec::new(),
                container_id: None,
            })
            .expect("thread page");
        let counts = threads
            .threads
            .iter()
            .filter(|thread| thread.id == DEMO_FIXTURE_THREAD_ID)
            .map(|thread| (thread.id, thread.message_count))
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(counts.get(&DEMO_FIXTURE_THREAD_ID), Some(&35));
        drop(store);

        let reopened = MuxStore::open(&path, true).expect("seeded store reopens");
        let message_count: i64 = reopened
            .connection
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .expect("message count");
        assert_eq!(message_count, 42);
    }

    #[test]
    fn account_color_is_validated_normalized_and_persistent() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("account-color.db");
        let mut store = MuxStore::open(&path, true).expect("seeded store opens");

        store
            .set_account_color("acc_work", "#Aa11Ff")
            .expect("complete hexadecimal color");
        let stored: String = store
            .connection
            .query_row(
                "SELECT color FROM accounts WHERE id = 'acc_work'",
                [],
                |row| row.get(0),
            )
            .expect("stored account color");
        assert_eq!(stored, "#aa11ff");

        for invalid in ["", "#123", "123456", "#12345g", "#1234567"] {
            assert!(matches!(
                store.set_account_color("acc_work", invalid),
                Err(StoreError::Validation(_))
            ));
        }
        assert!(matches!(
            store.set_account_color("missing-account", "#123456"),
            Err(StoreError::NotFound(ref message)) if message == "Account was not found"
        ));
        drop(store);

        let reopened = MuxStore::open(&path, false).expect("store reopens");
        let persisted: String = reopened
            .connection
            .query_row(
                "SELECT color FROM accounts WHERE id = 'acc_work'",
                [],
                |row| row.get(0),
            )
            .expect("persisted account color");
        assert_eq!(persisted, "#aa11ff");
    }

    #[test]
    fn native_mailbox_pages_are_bounded_stable_and_chronological() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("mailbox-pages.db");
        let store = MuxStore::open(&path, true).expect("seeded store opens");

        store
            .connection
            .execute(
                "UPDATE threads SET latest_at = 9000000000000 WHERE id IN (1, 2)",
                [],
            )
            .expect("same-time threads prepared");
        let tied_first = store
            .list_threads(ThreadPageInput {
                account_id: None,
                view: Some("all".into()),
                cursor: None,
                limit: Some(1),
                hidden_account_ids: Vec::new(),
                container_id: None,
            })
            .expect("first tied page");
        let tied_second = store
            .list_threads(ThreadPageInput {
                account_id: None,
                view: Some("all".into()),
                cursor: tied_first.next_cursor.clone(),
                limit: Some(1),
                hidden_account_ids: Vec::new(),
                container_id: None,
            })
            .expect("second tied page");
        assert_eq!(tied_first.threads[0].id, 2);
        assert_eq!(tied_second.threads[0].id, 1);

        for id in [9_000_001_i64, 9_000_002_i64] {
            store
                .connection
                .execute(
                    "INSERT INTO messages(
                       id, thread_id, sender_name, sender_email, recipients, sent_at,
                       body_text, is_from_me
                     ) VALUES(?1, 4, 'Tie sender', 'tie@example.test', 'jordan@example.test',
                              9100000000000, 'same timestamp', 0)",
                    [id],
                )
                .expect("same-time message inserted");
        }
        let message_tied_first = store
            .get_thread_messages(MessagePageInput {
                thread_id: 4,
                cursor: None,
                limit: Some(1),
            })
            .expect("first tied message page");
        let message_tied_second = store
            .get_thread_messages(MessagePageInput {
                thread_id: 4,
                cursor: message_tied_first.next_cursor.clone(),
                limit: Some(1),
            })
            .expect("second tied message page");
        assert_eq!(message_tied_first.messages[0].id, 9_000_002);
        assert_eq!(message_tied_second.messages[0].id, 9_000_001);

        let first = store
            .list_threads(ThreadPageInput {
                account_id: None,
                view: Some("all".into()),
                cursor: None,
                limit: Some(3),
                hidden_account_ids: Vec::new(),
                container_id: None,
            })
            .expect("first thread page");
        assert_eq!(first.threads.len(), 3);
        assert!(first.has_more);
        let second = store
            .list_threads(ThreadPageInput {
                account_id: None,
                view: Some("all".into()),
                cursor: first.next_cursor,
                limit: Some(3),
                hidden_account_ids: Vec::new(),
                container_id: None,
            })
            .expect("second thread page");
        assert!(first
            .threads
            .iter()
            .all(|left| second.threads.iter().all(|right| left.id != right.id)));

        let newest = store
            .get_thread_messages(MessagePageInput {
                thread_id: DEMO_FIXTURE_THREAD_ID,
                cursor: None,
                limit: Some(5),
            })
            .expect("newest message page");
        assert_eq!(newest.messages.len(), 5);
        assert!(newest.has_more);
        assert!(newest
            .messages
            .windows(2)
            .all(|pair| (pair[0].sent_at, pair[0].id) < (pair[1].sent_at, pair[1].id)));
        let oldest_loaded = (newest.messages[0].sent_at, newest.messages[0].id);
        let older = store
            .get_thread_messages(MessagePageInput {
                thread_id: DEMO_FIXTURE_THREAD_ID,
                cursor: newest.next_cursor,
                limit: Some(5),
            })
            .expect("older message page");
        assert!(older
            .messages
            .iter()
            .all(|message| (message.sent_at, message.id) < oldest_loaded));
    }

    #[test]
    fn message_pages_adapt_to_the_serialized_cap_and_traverse_large_bodies_completely() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("adaptive-message-pages.db");
        let store = MuxStore::open(&path, true).expect("seeded store opens");
        let thread_id = 8_900_000_i64;
        let first_message_id = 8_910_000_i64;
        store
            .connection
            .execute(
                "INSERT INTO threads(
                   id, account_id, subject, participants, snippet, latest_at, message_count,
                   remote_in_inbox, remote_unread, remote_starred, has_attachment, has_invite,
                   has_link, has_from_me, category, attachment_names
                 ) VALUES(?1, 'acc_work', 'Adaptive bodies', 'Scale sender', 'Large messages',
                          9900000000011, 12, 1, 0, 0, 1, 0, 0, 0, 'Scale', 'oldest.txt')",
                [thread_id],
            )
            .unwrap();
        let body = "x".repeat(2 * 1024 * 1024);
        for offset in 0..12_i64 {
            store
                .connection
                .execute(
                    "INSERT INTO messages(
                       id, thread_id, sender_name, sender_email, recipients, sent_at,
                       body_text, body_html, is_from_me
                     ) VALUES(?1, ?2, 'Scale sender', 'scale@example.test',
                              'jordan@example.test', ?3, ?4, '', 0)",
                    params![
                        first_message_id + offset,
                        thread_id,
                        9_900_000_000_000_i64 + offset,
                        &body
                    ],
                )
                .unwrap();
        }
        store
            .connection
            .execute(
                "INSERT INTO attachments(
                   id, message_id, filename, media_type, byte_length, content,
                   content_id, disposition
                 ) VALUES('adaptive-oldest', ?1, 'oldest.txt', 'text/plain', 6,
                          X'6d61726b6572', '', 'attachment')",
                [first_message_id],
            )
            .unwrap();

        let mut cursor = None;
        let mut previous_cursor = None;
        let mut seen = std::collections::HashSet::new();
        let mut attachment_seen = false;
        let mut page_count = 0;
        loop {
            let page = store
                .get_thread_messages(MessagePageInput {
                    thread_id,
                    cursor,
                    limit: Some(50),
                })
                .expect("adaptive message page");
            page_count += 1;
            assert!(!page.messages.is_empty());
            assert!(
                page.messages.len() < 50,
                "byte budget must adapt the row count"
            );
            assert!(page
                .messages
                .windows(2)
                .all(|pair| (pair[0].sent_at, pair[0].id) < (pair[1].sent_at, pair[1].id)));
            let serialized = serde_json::to_vec(&page).unwrap();
            assert!(serialized.len() <= crate::ipc_boundary::MESSAGE_DETAIL_BYTES);
            assert!(!String::from_utf8_lossy(&serialized).contains("dataBase64"));
            for message in &page.messages {
                assert!(seen.insert(message.id), "message {} duplicated", message.id);
            }
            for attachment in &page.attachments {
                assert!(page
                    .messages
                    .iter()
                    .any(|message| message.id == attachment.message_id));
                if attachment.id == "adaptive-oldest" {
                    attachment_seen = true;
                }
            }
            if page_count == 1 {
                assert!(
                    !attachment_seen,
                    "older attachment metadata leaked into a newer page"
                );
            }
            if page.has_more {
                let next = page.next_cursor.expect("continuation cursor");
                assert_ne!(previous_cursor.as_deref(), Some(next.as_str()));
                previous_cursor = Some(next.clone());
                cursor = Some(next);
            } else {
                assert!(page.next_cursor.is_none());
                break;
            }
        }
        assert!(page_count >= 2);
        assert_eq!(seen.len(), 12);
        assert!(attachment_seen);
        assert_eq!(
            seen,
            (0..12_i64)
                .map(|offset| first_message_id + offset)
                .collect::<std::collections::HashSet<_>>()
        );
    }

    #[test]
    fn mailbox_page_limits_hold_with_more_than_the_maximum_rows() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("bounded-pages.db");
        let store = MuxStore::open(&path, true).expect("seeded store opens");
        for offset in 0..120_i64 {
            let thread_id = 10_000 + offset;
            store
                .connection
                .execute(
                    "INSERT INTO threads(
                       id, account_id, subject, participants, snippet, latest_at, message_count,
                       remote_in_inbox, remote_unread, remote_starred, has_attachment, has_invite,
                       has_link, has_from_me, category, attachment_names
                     ) VALUES(?1, 'acc_work', ?2, 'Scale sender', 'Bounded row', ?3, 1,
                              1, 0, 0, 0, 0, 0, 0, 'Scale', '')",
                    params![
                        thread_id,
                        format!("Scale {offset}"),
                        8_000_000_000_000_i64 + offset
                    ],
                )
                .expect("scale thread inserted");
            store
                .connection
                .execute(
                    "INSERT INTO messages(
                       id, thread_id, sender_name, sender_email, recipients, sent_at,
                       body_text, is_from_me
                     ) VALUES(?1, 3, 'Scale sender', 'scale@example.test',
                              'jordan@example.test', ?2, 'Bounded message', 0)",
                    params![20_000 + offset, 8_000_000_000_000_i64 + offset],
                )
                .expect("scale message inserted");
        }

        let first_threads = store
            .list_threads(ThreadPageInput {
                account_id: None,
                view: Some("all".into()),
                cursor: None,
                limit: Some(500),
                hidden_account_ids: Vec::new(),
                container_id: None,
            })
            .expect("bounded thread page");
        assert_eq!(first_threads.threads.len(), 100);
        assert!(first_threads.has_more);
        let second_threads = store
            .list_threads(ThreadPageInput {
                account_id: None,
                view: Some("all".into()),
                cursor: first_threads.next_cursor,
                limit: Some(500),
                hidden_account_ids: Vec::new(),
                container_id: None,
            })
            .expect("thread continuation");
        assert!(first_threads.threads.iter().all(|left| second_threads
            .threads
            .iter()
            .all(|right| left.id != right.id)));

        let first_messages = store
            .get_thread_messages(MessagePageInput {
                thread_id: 3,
                cursor: None,
                limit: Some(500),
            })
            .expect("bounded message page");
        assert_eq!(first_messages.messages.len(), 100);
        assert!(first_messages.has_more);
        let second_messages = store
            .get_thread_messages(MessagePageInput {
                thread_id: 3,
                cursor: first_messages.next_cursor,
                limit: Some(500),
            })
            .expect("message continuation");
        assert!(first_messages.messages.iter().all(|left| second_messages
            .messages
            .iter()
            .all(|right| left.id != right.id)));
    }

    #[test]
    fn mailbox_bootstrap_is_read_only_and_page_inputs_are_validated() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("pure-bootstrap.db");
        let mut store = MuxStore::open(&path, true).expect("seeded store opens");
        let before_inbox = store
            .bootstrap()
            .expect("initial bootstrap")
            .view_counts
            .into_iter()
            .find(|count| count.account_id.is_none())
            .expect("global counts")
            .inbox;
        let operation = store
            .apply_thread_action(1, "archive")
            .expect("archive is queued");
        store
            .connection
            .execute(
                "UPDATE operations SET not_before = 0 WHERE id = ?1",
                [&operation.id],
            )
            .expect("operation made due");

        let after_inbox = store
            .bootstrap()
            .expect("metadata bootstrap")
            .view_counts
            .into_iter()
            .find(|count| count.account_id.is_none())
            .expect("global counts")
            .inbox;
        assert_eq!(after_inbox, before_inbox - 1);
        let state: (String, i64) = store
            .connection
            .query_row(
                "SELECT state, attempts FROM operations WHERE id = ?1",
                [&operation.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("operation state");
        assert_eq!(state, ("pending".into(), 0));

        assert!(matches!(
            store.list_threads(ThreadPageInput {
                account_id: None,
                view: Some("inbox".into()),
                cursor: Some("bad".into()),
                limit: Some(50),
                hidden_account_ids: Vec::new(),
                container_id: None,
            }),
            Err(StoreError::Validation(_))
        ));
        assert!(matches!(
            store.get_thread_messages(MessagePageInput {
                thread_id: 0,
                cursor: None,
                limit: Some(50),
            }),
            Err(StoreError::Validation(_))
        ));
        for cursor in ["-1:1", "1:0", "9223372036854775808:1"] {
            assert!(matches!(
                store.list_threads(ThreadPageInput {
                    account_id: None,
                    view: Some("inbox".into()),
                    cursor: Some(cursor.into()),
                    limit: Some(50),
                    hidden_account_ids: Vec::new(),
                    container_id: None,
                }),
                Err(StoreError::Validation(_))
            ));
            assert!(matches!(
                store.get_thread_messages(MessagePageInput {
                    thread_id: 1,
                    cursor: Some(cursor.into()),
                    limit: Some(50),
                }),
                Err(StoreError::Validation(_))
            ));
        }
    }

    #[test]
    fn the_seeded_inbox_is_four_written_conversations_and_no_filler() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("seed.db");
        let store = MuxStore::open(&path, true).expect("seeded store");
        let inbox = store
            .list_threads(ThreadPageInput {
                account_id: None,
                view: Some("inbox".into()),
                cursor: None,
                limit: Some(50),
                hidden_account_ids: Vec::new(),
                container_id: None,
            })
            .expect("inbox page");
        assert_eq!(
            inbox
                .threads
                .iter()
                .map(|thread| thread.subject.as_str())
                .collect::<Vec<_>>(),
            [
                "draft 3 — the methods section reads better now",
                "your wheel is ready",
                "leftovers",
                "radiator service — Thursday between 9 and 12",
            ]
        );

        // Bodies are written, not generated from the subject. These are the
        // phrasings the old templated seed could never have produced.
        let bodies: String = (1..=4)
            .flat_map(|thread_id| {
                store
                    .get_thread_messages(MessagePageInput {
                        thread_id,
                        cursor: None,
                        limit: Some(50),
                    })
                    .expect("thread messages")
                    .messages
                    .into_iter()
                    .map(|message| message.body_text)
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(bodies.contains("the second cohort is n=14"));
        assert!(bodies.contains("your name on it in sharpie"));
        assert!(bodies.contains("$137.00"));
        for filler in [
            "Let me know what you think when you have a chance",
            "I took a look and added my notes",
            "Update 1 on",
        ] {
            assert!(
                !bodies.contains(filler),
                "templated filler returned: {filler}"
            );
        }

        // The shop message references images it declines to fetch.
        let shop = store
            .get_thread_messages(MessagePageInput {
                thread_id: 2,
                cursor: None,
                limit: Some(10),
            })
            .expect("shop thread");
        assert_eq!(shop.messages[0].blocked_remote_resources, 2);
        assert_eq!(shop.messages[0].remote_images.len(), 2);
        assert!(shop.messages[0].body_html.contains("mux-remote-image"));
        // The radiator notice arrives as plain text only.
        let plain = store
            .get_thread_messages(MessagePageInput {
                thread_id: 4,
                cursor: None,
                limit: Some(10),
            })
            .expect("radiator thread");
        assert!(plain.messages[0].body_html.is_empty());
    }

    #[test]
    fn native_search_supports_fields_scope_and_stable_cursors() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("search.db");
        let store = MuxStore::open(&path, true).expect("seeded store opens");
        let result = store
            .search_threads(SearchInput {
                query: "subject:draft (from:tomas OR from:priya)".into(),
                account_id: Some("acc_work".into()),
                view: Some("inbox".into()),
                cursor: None,
                limit: Some(10),
                timezone_offset_minutes: 0,
                hidden_account_ids: Vec::new(),
            })
            .expect("fielded search");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].id, 1);
        let scoped = store
            .get_thread_summary(ThreadLookupInput {
                thread_id: 1,
                account_id: Some("acc_work".into()),
                view: Some("inbox".into()),
                query: Some("subject:draft from:tomas".into()),
                timezone_offset_minutes: 0,
            })
            .expect("scoped thread lookup")
            .expect("matching thread");
        assert_eq!(scoped.id, 1);
        assert!(store
            .get_thread_summary(ThreadLookupInput {
                thread_id: 1,
                account_id: Some("acc_personal".into()),
                view: Some("inbox".into()),
                query: Some("subject:draft".into()),
                timezone_offset_minutes: 0,
            })
            .expect("wrong-account lookup")
            .is_none());
        assert!(store
            .get_thread_summary(ThreadLookupInput {
                thread_id: 1,
                account_id: Some("acc_work".into()),
                view: Some("inbox".into()),
                query: Some("subject:does-not-match".into()),
                timezone_offset_minutes: 0,
            })
            .expect("nonmatching lookup")
            .is_none());

        store
            .connection
            .execute(
                "UPDATE messages SET cc_recipients = 'planning@acme.example' WHERE thread_id = 1 AND id = (SELECT MIN(id) FROM messages WHERE thread_id = 1)",
                [],
            )
            .unwrap();
        let cc_result = store
            .search_threads(SearchInput {
                query: "to:planning@acme.example".into(),
                account_id: Some("acc_work".into()),
                view: Some("inbox".into()),
                cursor: None,
                limit: Some(10),
                timezone_offset_minutes: 0,
                hidden_account_ids: Vec::new(),
            })
            .expect("Cc search");
        assert_eq!(
            cc_result.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![1]
        );

        let first = store
            .search_threads(SearchInput {
                query: String::new(),
                account_id: None,
                view: Some("all".into()),
                cursor: None,
                limit: Some(3),
                timezone_offset_minutes: 0,
                hidden_account_ids: Vec::new(),
            })
            .expect("first page");
        assert!(first.has_more);
        let second = store
            .search_threads(SearchInput {
                query: String::new(),
                account_id: None,
                view: Some("all".into()),
                cursor: first.next_cursor,
                limit: Some(3),
                timezone_offset_minutes: 0,
                hidden_account_ids: Vec::new(),
            })
            .expect("second page");
        assert!(first
            .rows
            .iter()
            .all(|left| second.rows.iter().all(|right| left.id != right.id)));
    }

    #[test]
    fn native_search_rejects_bad_syntax_values_and_cursors() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("bad-search.db");
        let store = MuxStore::open(&path, true).expect("seeded store opens");
        for (query, cursor) in [
            ("subject:", None),
            ("has:tracking-pixel", None),
            ("project", Some("not-a-cursor".into())),
        ] {
            let error = store
                .search_threads(SearchInput {
                    query: query.into(),
                    account_id: None,
                    view: Some("all".into()),
                    cursor,
                    limit: Some(10),
                    timezone_offset_minutes: 0,
                    hidden_account_ids: Vec::new(),
                })
                .expect_err("invalid search must fail");
            assert!(matches!(error, StoreError::Validation(_)));
        }
    }

    #[test]
    fn hidden_accounts_leave_the_listing_without_touching_their_mail() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("hidden-accounts.db");
        let store = MuxStore::open(&path, true).expect("seeded store opens");

        let all = store
            .list_threads(ThreadPageInput {
                account_id: None,
                view: Some("all".into()),
                cursor: None,
                limit: Some(100),
                hidden_account_ids: Vec::new(),
                container_id: None,
            })
            .expect("unfiltered listing");
        let hidden_account = all
            .threads
            .first()
            .map(|thread| thread.account_id.clone())
            .expect("seeded thread");
        let expected = all
            .threads
            .iter()
            .filter(|thread| thread.account_id != hidden_account)
            .count();

        let filtered = store
            .list_threads(ThreadPageInput {
                account_id: None,
                view: Some("all".into()),
                cursor: None,
                limit: Some(100),
                hidden_account_ids: vec![hidden_account.clone()],
                container_id: None,
            })
            .expect("filtered listing");
        assert_eq!(filtered.threads.len(), expected);
        assert!(filtered
            .threads
            .iter()
            .all(|thread| thread.account_id != hidden_account));
        assert!(expected < all.threads.len(), "fixture must hide something");

        // Search honours the same visibility.
        let searched = store
            .search_threads(SearchInput {
                query: "is:unread".into(),
                account_id: None,
                view: Some("all".into()),
                cursor: None,
                limit: Some(100),
                timezone_offset_minutes: 0,
                hidden_account_ids: vec![hidden_account.clone()],
            })
            .expect("filtered search");
        assert!(searched
            .rows
            .iter()
            .all(|thread| thread.account_id != hidden_account));

        // Hiding is a view filter only: the mail itself is untouched.
        let threads: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM threads WHERE account_id = ?1",
                [&hidden_account],
                |row| row.get(0),
            )
            .expect("hidden account rows");
        assert!(threads > 0, "hidden account keeps its threads");

        // Duplicates and order must not change the accepted set.
        let repeated = store
            .list_threads(ThreadPageInput {
                account_id: None,
                view: Some("all".into()),
                cursor: None,
                limit: Some(100),
                hidden_account_ids: vec![hidden_account.clone(), hidden_account.clone()],
                container_id: None,
            })
            .expect("duplicate hidden ids");
        assert_eq!(repeated.threads.len(), expected);

        let too_many = (0..(MAX_HIDDEN_ACCOUNTS + 1))
            .map(|index| format!("acc_{index}"))
            .collect::<Vec<_>>();
        assert!(matches!(
            store.list_threads(ThreadPageInput {
                account_id: None,
                view: Some("all".into()),
                cursor: None,
                limit: Some(100),
                hidden_account_ids: too_many,
                container_id: None,
            }),
            Err(StoreError::Validation(_))
        ));
    }

    #[test]
    fn recolouring_an_account_reads_back_lowercased_through_the_listing() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("account-color.db");
        let mut store = MuxStore::open(&path, true).expect("seeded store opens");
        let color_of = |store: &MuxStore, id: &str| {
            store
                .bootstrap()
                .expect("bootstrap")
                .accounts
                .into_iter()
                .find(|account| account.id == id)
                .map(|account| account.color)
                .expect("seeded account")
        };
        assert_eq!(color_of(&store, "acc_work"), "#3b82f6");

        store
            .set_account_color("acc_work", "#C93B63")
            .expect("recolor");
        // Kept the way it will be written into a style: lowercase, six digits.
        assert_eq!(color_of(&store, "acc_work"), "#c93b63");
        // The other account keeps its own.
        assert_eq!(color_of(&store, "acc_research"), "#21b89a");
    }

    #[test]
    fn anything_but_a_six_digit_hex_colour_is_refused_and_changes_nothing() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("bad-account-color.db");
        let mut store = MuxStore::open(&path, true).expect("seeded store opens");
        for bad in [
            "",
            "#",
            "#abc",
            "red",
            "c93b63",
            "#abcdefg",
            "#12345g",
            " #c93b63",
            "#c93b63 ",
            "#c93b63;background:url(x)",
            "url(#c93b63)",
            "javascript:",
            "#ｃ９３ｂ６３",
        ] {
            assert!(
                matches!(
                    store.set_account_color("acc_work", bad),
                    Err(StoreError::Validation(_))
                ),
                "{bad:?} must be refused"
            );
        }
        let color: String = store
            .connection
            .query_row(
                "SELECT color FROM accounts WHERE id = 'acc_work'",
                [],
                |row| row.get(0),
            )
            .expect("color");
        assert_eq!(color, "#3b82f6");
    }

    #[test]
    fn recolouring_an_unknown_account_is_refused() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("unknown-account-color.db");
        let mut store = MuxStore::open(&path, true).expect("seeded store opens");
        assert!(matches!(
            store.set_account_color("acc_nobody", "#c93b63"),
            Err(StoreError::NotFound(_))
        ));
        let recolored: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM accounts WHERE color = '#c93b63'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(recolored, 0);
    }

    #[test]
    fn native_cursors_reject_cross_scope_reuse_tampering_and_survive_restart() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("opaque-cursors.db");
        let store = MuxStore::open(&path, true).expect("seeded store opens");
        let first = store
            .list_threads(ThreadPageInput {
                account_id: None,
                view: Some("all".into()),
                cursor: None,
                limit: Some(2),
                hidden_account_ids: Vec::new(),
                container_id: None,
            })
            .expect("first page");
        let cursor = first.next_cursor.expect("continuation cursor");
        assert!(cursor.len() <= crate::ipc_boundary::MAX_CURSOR_BYTES);

        for input in [
            ThreadPageInput {
                account_id: None,
                view: Some("inbox".into()),
                cursor: Some(cursor.clone()),
                limit: Some(2),
                hidden_account_ids: Vec::new(),
                container_id: None,
            },
            ThreadPageInput {
                account_id: Some("acc_work".into()),
                view: Some("all".into()),
                cursor: Some(cursor.clone()),
                limit: Some(2),
                hidden_account_ids: Vec::new(),
                container_id: None,
            },
            ThreadPageInput {
                account_id: None,
                view: Some("all".into()),
                cursor: Some(cursor.clone()),
                limit: Some(2),
                hidden_account_ids: vec!["acc_personal".into()],
                container_id: None,
            },
        ] {
            assert!(matches!(
                store.list_threads(input),
                Err(StoreError::Validation(ref message)) if message == "Thread cursor is invalid"
            ));
        }
        assert!(matches!(
            store.search_threads(SearchInput {
                query: String::new(),
                account_id: None,
                view: Some("all".into()),
                cursor: Some(cursor.clone()),
                limit: Some(2),
                timezone_offset_minutes: 0,
        hidden_account_ids: Vec::new(),
            }),
            Err(StoreError::Validation(ref message)) if message == "Search cursor is invalid"
        ));

        let mut tampered = cursor.clone().into_bytes();
        let last = tampered.len() - 1;
        tampered[last] = if tampered[last] == b'a' { b'b' } else { b'a' };
        assert!(matches!(
            store.list_threads(ThreadPageInput {
                account_id: None,
                view: Some("all".into()),
                cursor: Some(String::from_utf8(tampered).unwrap()),
                limit: Some(2),
        hidden_account_ids: Vec::new(),
        container_id: None,
            }),
            Err(StoreError::Validation(ref message)) if message == "Thread cursor is invalid"
        ));

        let first_ids = first
            .threads
            .iter()
            .map(|thread| thread.id)
            .collect::<std::collections::HashSet<_>>();
        drop(store);
        let reopened = MuxStore::open(&path, false).expect("store reopens");
        let second = reopened
            .list_threads(ThreadPageInput {
                account_id: None,
                view: Some("all".into()),
                cursor: Some(cursor),
                limit: Some(2),
                hidden_account_ids: Vec::new(),
                container_id: None,
            })
            .expect("persistently signed cursor continues");
        assert!(second
            .threads
            .iter()
            .all(|thread| !first_ids.contains(&thread.id)));

        let search = reopened
            .search_threads(SearchInput {
                query: String::new(),
                account_id: None,
                view: Some("all".into()),
                cursor: None,
                limit: Some(2),
                timezone_offset_minutes: 0,
                hidden_account_ids: Vec::new(),
            })
            .expect("search page");
        let search_cursor = search.next_cursor.expect("search continuation");
        assert!(search_cursor.len() <= crate::ipc_boundary::MAX_CURSOR_BYTES);
        assert!(matches!(
            reopened.search_threads(SearchInput {
                query: "subject:architecture".into(),
                account_id: None,
                view: Some("all".into()),
                cursor: Some(search_cursor),
                limit: Some(2),
                timezone_offset_minutes: 0,
        hidden_account_ids: Vec::new(),
            }),
            Err(StoreError::Validation(ref message)) if message == "Search cursor is invalid"
        ));
    }

    #[test]
    fn account_scopes_preserve_literal_sentinels_and_whitespace_exactly() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("exact-account-scopes.db");
        let mut store = MuxStore::open(&path, true).expect("seeded store opens");
        for (account_index, account_id) in ["*", "all", " acc_work "].into_iter().enumerate() {
            store
                .connection
                .execute(
                    "INSERT INTO accounts(id, name, email, color, provider, signature)
                     VALUES(?1, ?2, ?3, '#123456', 'fixture', '')",
                    params![
                        account_id,
                        format!("Exact {account_index}"),
                        format!("exact-{account_index}@example.test")
                    ],
                )
                .unwrap();
            for offset in 0..2_i64 {
                let thread_id = 8_800_000_i64 + account_index as i64 * 10 + offset;
                store
                    .connection
                    .execute(
                        "INSERT INTO threads(
                           id, account_id, subject, participants, snippet, latest_at,
                           message_count, remote_in_inbox, remote_unread, remote_starred,
                           has_attachment, has_invite, has_link, has_from_me, category,
                           attachment_names
                         ) VALUES(?1, ?2, ?3, 'Exact sender', 'Exact account', ?4, 1,
                                  1, 0, 0, 0, 0, 0, 0, 'Exact', '')",
                        params![
                            thread_id,
                            account_id,
                            format!("Exact account {account_index}-{offset}"),
                            9_500_000_000_000_i64 + account_index as i64 * 10 + offset
                        ],
                    )
                    .unwrap();
            }
        }

        let page = |account_id: Option<&str>| {
            store
                .list_threads(ThreadPageInput {
                    account_id: account_id.map(str::to_string),
                    view: Some("all".into()),
                    cursor: None,
                    limit: Some(1),
                    hidden_account_ids: Vec::new(),
                    container_id: None,
                })
                .unwrap()
        };
        let unified = page(None);
        let literal_star = page(Some("*"));
        let literal_all = page(Some("all"));
        let whitespace = page(Some(" acc_work "));
        assert_eq!(literal_star.threads[0].account_id, "*");
        assert_eq!(literal_all.threads[0].account_id, "all");
        assert_eq!(whitespace.threads[0].account_id, " acc_work ");
        assert_ne!(whitespace.threads[0].account_id, "acc_work");

        for (cursor, account_id) in [
            (unified.next_cursor, Some("*")),
            (literal_star.next_cursor.clone(), None),
            (literal_star.next_cursor.clone(), Some("all")),
            (literal_star.next_cursor, Some(" acc_work ")),
        ] {
            assert!(matches!(
                    store.list_threads(ThreadPageInput {
                        account_id: account_id.map(str::to_string),
                        view: Some("all".into()),
                        cursor,
                        limit: Some(1),
            hidden_account_ids: Vec::new(),
            container_id: None,
                    }),
                    Err(StoreError::Validation(ref message)) if message == "Thread cursor is invalid"
                ));
        }

        let star_search = store
            .search_threads(SearchInput {
                query: String::new(),
                account_id: Some("*".into()),
                view: Some("all".into()),
                cursor: None,
                limit: Some(1),
                timezone_offset_minutes: 0,
                hidden_account_ids: Vec::new(),
            })
            .unwrap();
        assert_eq!(star_search.rows[0].account_id, "*");
        assert!(matches!(
            store.search_threads(SearchInput {
                query: String::new(),
                account_id: Some("all".into()),
                view: Some("all".into()),
                cursor: star_search.next_cursor,
                limit: Some(1),
                timezone_offset_minutes: 0,
        hidden_account_ids: Vec::new(),
            }),
            Err(StoreError::Validation(ref message)) if message == "Search cursor is invalid"
        ));

        assert!(store
            .get_thread_summary(ThreadLookupInput {
                thread_id: 8_800_000,
                account_id: Some("*".into()),
                view: Some("all".into()),
                query: None,
                timezone_offset_minutes: 0,
            })
            .unwrap()
            .is_some());
        assert!(store
            .get_thread_summary(ThreadLookupInput {
                thread_id: 8_800_000,
                account_id: Some("all".into()),
                view: Some("all".into()),
                query: None,
                timezone_offset_minutes: 0,
            })
            .unwrap()
            .is_none());
        assert!(matches!(
            store.list_threads(ThreadPageInput {
                account_id: Some(String::new()),
                view: Some("all".into()),
                cursor: None,
                limit: Some(1),
        hidden_account_ids: Vec::new(),
        container_id: None,
            }),
            Err(StoreError::Validation(ref message))
                if message == "accountId must not be empty when present"
        ));
        let whitespace_draft = store
            .save_draft(SaveDraftInput {
                id: None,
                account_id: " acc_work ".into(),
                recipients: "recipient@example.test".into(),
                cc_recipients: String::new(),
                bcc_recipients: String::new(),
                subject: "Exact account draft".into(),
                body: String::new(),
                body_html: String::new(),
                reply_to_thread_id: None,
                expected_revision: None,
            })
            .unwrap();
        assert_eq!(whitespace_draft.account_id, " acc_work ");
    }

    #[test]
    fn fifty_thousand_row_mailbox_and_search_traversals_do_not_duplicate_or_stick() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("fifty-thousand-cursors.db");
        let mut store = MuxStore::open(&path, true).expect("seeded store opens");
        let existing: i64 = store
            .connection
            .query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))
            .unwrap();
        let additional = 50_000_i64 - existing;
        let transaction = store.connection.transaction().unwrap();
        {
            let mut insert = transaction
                .prepare(
                    "INSERT INTO threads(
                       id, account_id, subject, participants, snippet, latest_at, message_count,
                       remote_in_inbox, remote_unread, remote_starred, has_attachment, has_invite,
                       has_link, has_from_me, category, attachment_names
                     ) VALUES(?1, 'acc_work', ?2, 'Cursor scale', 'Cursor scale', ?3, 1,
                              1, 0, 0, 0, 0, 0, 0, 'Scale', '')",
                )
                .unwrap();
            for offset in 0..additional {
                insert
                    .execute(params![
                        1_000_000_i64 + offset,
                        format!("Cursor scale {offset}"),
                        7_000_000_000_000_i64 + offset,
                    ])
                    .unwrap();
            }
        }
        transaction.commit().unwrap();
        let initial_count: usize = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM thread_effective WHERE remote_deleted = 0",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(initial_count, 50_000);

        let first = store
            .list_threads(ThreadPageInput {
                account_id: None,
                view: Some("all".into()),
                cursor: None,
                limit: Some(100),
                hidden_account_ids: Vec::new(),
                container_id: None,
            })
            .unwrap();
        let mut seen = first
            .threads
            .iter()
            .map(|thread| thread.id)
            .collect::<std::collections::HashSet<_>>();
        let mut cursor = first.next_cursor;

        let deleted_id = 1_000_000_i64 + additional / 2;
        store
            .connection
            .execute(
                "UPDATE threads SET remote_deleted = 1 WHERE id = ?1",
                [deleted_id],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO threads(
                   id, account_id, subject, participants, snippet, latest_at, message_count,
                   remote_in_inbox, remote_unread, remote_starred, has_attachment, has_invite,
                   has_link, has_from_me, category, attachment_names
                 ) VALUES(9999999, 'acc_work', 'Arrived during traversal', 'Cursor scale',
                          'Must not jump into older pages', 9000000000000, 1,
                          1, 0, 0, 0, 0, 0, 0, 'Scale', '')",
                [],
            )
            .unwrap();

        let mut prior_cursor = String::new();
        while let Some(next) = cursor {
            assert!(next.len() <= crate::ipc_boundary::MAX_CURSOR_BYTES);
            assert_ne!(next, prior_cursor, "cursor must always advance");
            prior_cursor = next.clone();
            let page = store
                .list_threads(ThreadPageInput {
                    account_id: None,
                    view: Some("all".into()),
                    cursor: Some(next),
                    limit: Some(100),
                    hidden_account_ids: Vec::new(),
                    container_id: None,
                })
                .unwrap();
            for thread in page.threads {
                assert!(seen.insert(thread.id), "thread {} duplicated", thread.id);
            }
            cursor = page.next_cursor;
        }
        assert_eq!(seen.len(), initial_count - 1);
        assert!(!seen.contains(&deleted_id));
        assert!(!seen.contains(&9_999_999));

        let current_count: usize = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM thread_effective WHERE remote_deleted = 0",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(current_count, initial_count);
        let mut search_seen = std::collections::HashSet::new();
        let mut search_cursor = None;
        let mut prior_search_cursor = String::new();
        loop {
            let page = store
                .search_threads(SearchInput {
                    query: String::new(),
                    account_id: None,
                    view: Some("all".into()),
                    cursor: search_cursor,
                    limit: Some(100),
                    timezone_offset_minutes: 0,
                    hidden_account_ids: Vec::new(),
                })
                .unwrap();
            for thread in page.rows {
                assert!(
                    search_seen.insert(thread.id),
                    "search thread {} duplicated",
                    thread.id
                );
            }
            match page.next_cursor {
                Some(next) => {
                    assert!(next.len() <= crate::ipc_boundary::MAX_CURSOR_BYTES);
                    assert_ne!(next, prior_search_cursor, "search cursor must advance");
                    prior_search_cursor = next.clone();
                    search_cursor = Some(next);
                }
                None => break,
            }
        }
        assert_eq!(search_seen.len(), current_count);
    }

    #[test]
    fn ipc_dtos_have_measured_caps_and_keep_bodies_and_binary_out_of_bootstrap() {
        for kind in [
            IpcPayloadKind::Bootstrap,
            IpcPayloadKind::ThreadPage,
            IpcPayloadKind::SearchPage,
            IpcPayloadKind::MessageDetail,
            IpcPayloadKind::Activity,
            IpcPayloadKind::ProviderBatch,
        ] {
            let at_cap = "x".repeat(kind.limit() - 2);
            assert_eq!(serde_json::to_vec(&at_cap).unwrap().len(), kind.limit());
            assert_eq!(ensure_serialized_budget(&at_cap, kind), Ok(kind.limit()));

            let over_cap = "x".repeat(kind.limit() - 1);
            assert_eq!(
                serde_json::to_vec(&over_cap).unwrap().len(),
                kind.limit() + 1
            );
            let error = ensure_serialized_budget(&over_cap, kind).unwrap_err();
            assert_eq!(
                error.to_string(),
                format!(
                    "{} exceeds the {}-byte IPC limit",
                    kind.label(),
                    kind.limit()
                )
            );
        }

        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("ipc-budgets.db");
        let mut store = MuxStore::open(&path, true).expect("seeded store opens");
        let draft = store
            .save_draft(SaveDraftInput {
                id: None,
                account_id: "acc_work".into(),
                recipients: "recipient@example.test".into(),
                cc_recipients: String::new(),
                bcc_recipients: String::new(),
                subject: "Header-only bootstrap".into(),
                body: "unique-secret-plain-body".into(),
                body_html: "<p>unique-secret-html-body</p>".into(),
                reply_to_thread_id: None,
                expected_revision: None,
            })
            .unwrap();
        let bootstrap = store.bootstrap().unwrap();
        let bootstrap_json = serde_json::to_value(&bootstrap).unwrap();
        let bootstrap_bytes = serde_json::to_vec(&bootstrap).unwrap();
        assert!(bootstrap_bytes.len() <= crate::ipc_boundary::BOOTSTRAP_BYTES);
        assert!(!bootstrap_json
            .to_string()
            .contains("unique-secret-plain-body"));
        assert!(!bootstrap_json
            .to_string()
            .contains("unique-secret-html-body"));
        assert!(bootstrap_json["drafts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|header| header.get("body").is_none() && header.get("bodyHtml").is_none()));
        let full_draft = store
            .get_draft(&draft.id)
            .unwrap()
            .expect("explicit draft lookup");
        assert_eq!(full_draft.body, "unique-secret-plain-body");
        assert_eq!(full_draft.body_html, "<p>unique-secret-html-body</p>");

        let detail = store
            .get_thread_messages(MessagePageInput {
                thread_id: 2,
                cursor: None,
                limit: Some(100),
            })
            .unwrap();
        let detail_json = serde_json::to_value(&detail).unwrap();
        assert!(
            serde_json::to_vec(&detail).unwrap().len() <= crate::ipc_boundary::MESSAGE_DETAIL_BYTES
        );
        assert!(detail_json["attachments"]
            .as_array()
            .unwrap()
            .iter()
            .all(|attachment| attachment.get("bytes").is_none()));

        let activity = store
            .list_operations(ListOperationsInput { limit: Some(100) })
            .unwrap();
        assert!(
            serde_json::to_vec(&activity).unwrap().len() <= crate::ipc_boundary::ACTIVITY_BYTES
        );
    }

    #[test]
    fn future_schema_is_rejected_without_downgrade() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("future.db");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta(key, value) VALUES('schema_version', '999');",
            )
            .unwrap();
        drop(connection);

        let error = MuxStore::open(&path, false)
            .err()
            .expect("future schema must fail");
        assert!(error
            .to_string()
            .contains("newer than this Mux build supports"));
        let verify = Connection::open(&path).unwrap();
        let version: String = verify
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "999");
    }

    #[test]
    fn schema_v2_migration_preserves_drafts_and_adds_revision() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("legacy.db");
        let store = MuxStore::open(&path, false).expect("schema created");
        store.connection.execute(
            "INSERT INTO accounts(id, name, email, color, provider) VALUES('acc_work', 'Work', 'jordan@acme.example', '#3b82f6', 'fake')",
            [],
        ).unwrap();
        store.connection.execute(
            "INSERT INTO drafts(id, account_id, recipients, subject, body, updated_at, revision)
             VALUES('draft-1', 'acc_work', 'alice@example.com', 'Preserve across migration', 'Existing local data must survive.', 1, 7)",
            [],
        ).unwrap();
        store
            .connection
            .execute_batch(
                "ALTER TABLE drafts DROP COLUMN revision;
             UPDATE meta SET value = '2' WHERE key = 'schema_version';",
            )
            .unwrap();
        drop(store);

        let migrated = MuxStore::open(&path, false).expect("v2 migrates");
        let draft: (String, String, i64) = migrated
            .connection
            .query_row(
                "SELECT subject, body, revision FROM drafts WHERE id = 'draft-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            draft,
            (
                "Preserve across migration".into(),
                "Existing local data must survive.".into(),
                1
            )
        );
    }

    #[test]
    fn schema_v3_migration_adds_rich_text_columns_without_data_loss() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("rich-text-migration.db");
        let store = MuxStore::open(&path, true).expect("schema created");
        store
            .connection
            .execute_batch(
                "ALTER TABLE drafts DROP COLUMN body_html;
                 ALTER TABLE messages DROP COLUMN body_html;
                 UPDATE meta SET value = '3' WHERE key = 'schema_version';",
            )
            .unwrap();
        drop(store);

        let migrated = MuxStore::open(&path, false).expect("v3 migrates");
        let draft_column = migrated
            .connection
            .prepare("PRAGMA table_info(drafts)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let message: (String, String) = migrated
            .connection
            .query_row(
                "SELECT body_text, body_html FROM messages ORDER BY id LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(draft_column.iter().any(|column| column == "body_html"));
        assert!(!message.0.is_empty());
        assert_eq!(message.1, "");
    }

    #[test]
    fn schema_v5_migration_adds_safe_content_storage() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("safe-content-migration.db");
        let store = MuxStore::open(&path, false).expect("schema created");
        store
            .connection
            .execute_batch(
                "DROP TABLE attachments;
                 ALTER TABLE messages DROP COLUMN blocked_remote_resources;
                 UPDATE meta SET value = '5' WHERE key = 'schema_version';",
            )
            .unwrap();
        drop(store);

        let migrated = MuxStore::open(&path, false).expect("v5 migrates");
        let message_columns = migrated
            .connection
            .prepare("PRAGMA table_info(messages)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(message_columns
            .iter()
            .any(|column| column == "blocked_remote_resources"));
        let attachment_table: i64 = migrated
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'attachments')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(attachment_table, 1);
    }

    #[test]
    fn drafts_preserve_restricted_rich_text_and_reject_stale_edits() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("drafts.db");
        let mut store = MuxStore::open(&path, true).expect("seeded store");
        let created = store
            .save_draft(SaveDraftInput {
                id: None,
                account_id: "acc_work".into(),
                recipients: "jane@acme.example".into(),
                cc_recipients: "team@acme.example".into(),
                bcc_recipients: String::new(),
                subject: "Working draft".into(),
                body: "A linked plan".into(),
                body_html: "<p>A <strong>linked</strong> plan</p>".into(),
                reply_to_thread_id: None,
                expected_revision: None,
            })
            .expect("draft created");
        assert_eq!(created.revision, 1);
        assert_eq!(created.body_html, "<p>A <strong>linked</strong> plan</p>");
        assert_eq!(created.cc_recipients, "team@acme.example");

        let stale = store.save_draft(SaveDraftInput {
            id: Some(created.id.clone()),
            account_id: created.account_id.clone(),
            recipients: created.recipients.clone(),
            cc_recipients: created.cc_recipients.clone(),
            bcc_recipients: created.bcc_recipients.clone(),
            subject: created.subject.clone(),
            body: created.body.clone(),
            body_html: created.body_html.clone(),
            reply_to_thread_id: None,
            expected_revision: Some(0),
        });
        assert!(matches!(stale, Err(StoreError::Conflict(_))));

        let updated = store
            .save_draft(SaveDraftInput {
                id: Some(created.id),
                account_id: created.account_id,
                recipients: created.recipients,
                cc_recipients: created.cc_recipients,
                bcc_recipients: created.bcc_recipients,
                subject: "Working draft, revised".into(),
                body: created.body,
                body_html: created.body_html,
                reply_to_thread_id: None,
                expected_revision: Some(1),
            })
            .expect("current revision saves");
        assert_eq!(updated.revision, 2);
    }

    #[test]
    fn recipient_mailbox_lists_preserve_quoted_commas_and_reject_malformed_sends() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("recipient-mailboxes.db");
        let mut store = MuxStore::open(&path, true).expect("seeded store");
        let quoted_recipients = "\"Doe, Jane\" <jane@example.com>, Alan <alan@example.com>";
        let draft = store
            .save_draft(SaveDraftInput {
                id: None,
                account_id: "acc_work".into(),
                recipients: quoted_recipients.into(),
                cc_recipients: "\"Smith, Pat\" <pat@example.com>".into(),
                bcc_recipients: String::new(),
                subject: "Quoted recipient names".into(),
                body: "This mailbox list must remain intact.".into(),
                body_html: "<p>This mailbox list must remain intact.</p>".into(),
                reply_to_thread_id: None,
                expected_revision: None,
            })
            .expect("quoted display names save");
        assert_eq!(draft.recipients, quoted_recipients);
        let queued = store
            .queue_send(&draft.id, 5_000)
            .expect("quoted display names queue");
        assert_eq!(queued.state, "pending");
        store
            .undo_operation(&queued.id)
            .expect("queued validation send undone");

        let draft_count_before_injection = store.list_drafts().unwrap().len();
        let injection = store.save_draft(SaveDraftInput {
            id: None,
            account_id: "acc_work".into(),
            recipients: "Jane <jane@example.com>\r\nBcc: evil@example.com".into(),
            cc_recipients: String::new(),
            bcc_recipients: String::new(),
            subject: "Header injection".into(),
            body: "Must not save".into(),
            body_html: String::new(),
            reply_to_thread_id: None,
            expected_revision: None,
        });
        assert!(matches!(injection, Err(StoreError::Validation(_))));
        assert_eq!(
            store.list_drafts().unwrap().len(),
            draft_count_before_injection,
            "a rejected header must not create a draft"
        );

        for (index, malformed) in [
            "\"Doe, Jane <jane@example.com>",
            "Jane <jane@example.com",
            "Jane <jane@example.com> trailing",
            "Jane <jane@example.com><alan@example.com>",
            "jane@example.com,,alan@example.com",
            "jane@-example.com",
            "jane@example",
        ]
        .into_iter()
        .enumerate()
        {
            let malformed_draft = store
                .save_draft(SaveDraftInput {
                    id: None,
                    account_id: "acc_work".into(),
                    recipients: malformed.into(),
                    cc_recipients: String::new(),
                    bcc_recipients: String::new(),
                    subject: format!("Malformed recipient {index}"),
                    body: "Drafts may remain incomplete, but cannot be queued.".into(),
                    body_html: String::new(),
                    reply_to_thread_id: None,
                    expected_revision: None,
                })
                .expect("incomplete recipient text remains draftable");
            let operation_count_before: i64 = store
                .connection
                .query_row("SELECT COUNT(*) FROM operations", [], |row| row.get(0))
                .unwrap();
            let rejected = store.queue_send(&malformed_draft.id, 5_000);
            assert!(
                matches!(rejected, Err(StoreError::Validation(_))),
                "malformed mailbox queued: {malformed}"
            );
            assert_eq!(
                store
                    .connection
                    .query_row("SELECT COUNT(*) FROM operations", [], |row| row
                        .get::<_, i64>(0))
                    .unwrap(),
                operation_count_before,
                "rejected queue must not journal an operation"
            );
            assert!(
                !store
                    .get_draft(&malformed_draft.id)
                    .unwrap()
                    .unwrap()
                    .locked
            );
        }
    }

    #[test]
    fn plain_draft_body_limit_is_unicode_aware_and_rejections_do_not_mutate() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("draft-body-boundary.db");
        let mut store = MuxStore::open(&path, true).expect("seeded store");
        let exact_plain_body = "é".repeat(MAX_PLAIN_DRAFT_CHARS);
        let formatted_over_plain_limit =
            format!("<p>{}</p>", "é".repeat(MAX_PLAIN_DRAFT_CHARS + 1));
        let created = store
            .save_draft(SaveDraftInput {
                id: None,
                account_id: "acc_work".into(),
                recipients: "jane@example.com".into(),
                cc_recipients: String::new(),
                bcc_recipients: String::new(),
                subject: "Unicode boundary".into(),
                body: exact_plain_body.clone(),
                body_html: formatted_over_plain_limit.clone(),
                reply_to_thread_id: None,
                expected_revision: None,
            })
            .expect("exactly 50,000 Unicode characters save");
        assert_eq!(created.body.chars().count(), MAX_PLAIN_DRAFT_CHARS);
        assert_eq!(created.body, exact_plain_body);
        assert_eq!(created.body_html, formatted_over_plain_limit);

        let missing_update = store.save_draft(SaveDraftInput {
            id: Some("draft-does-not-exist".into()),
            account_id: "acc_work".into(),
            recipients: "jane@example.com".into(),
            cc_recipients: String::new(),
            bcc_recipients: String::new(),
            subject: "Missing update".into(),
            body: "Must not insert".into(),
            body_html: String::new(),
            reply_to_thread_id: None,
            expected_revision: Some(1),
        });
        assert!(matches!(missing_update, Err(StoreError::NotFound(_))));
        assert_eq!(store.list_drafts().unwrap().len(), 1);

        let oversized_plain_body = "🦀".repeat(MAX_PLAIN_DRAFT_CHARS + 1);
        assert_eq!(
            oversized_plain_body.chars().count(),
            MAX_PLAIN_DRAFT_CHARS + 1
        );
        let rejected_update = store.save_draft(SaveDraftInput {
            id: Some(created.id.clone()),
            account_id: created.account_id.clone(),
            recipients: created.recipients.clone(),
            cc_recipients: created.cc_recipients.clone(),
            bcc_recipients: created.bcc_recipients.clone(),
            subject: "Rejected mutation".into(),
            body: oversized_plain_body,
            body_html: "<p>Rejected mutation</p>".into(),
            reply_to_thread_id: created.reply_to_thread_id,
            expected_revision: Some(created.revision),
        });
        assert!(matches!(rejected_update, Err(StoreError::Validation(_))));

        let unchanged = store
            .get_draft(&created.id)
            .unwrap()
            .expect("original draft remains");
        assert_eq!(unchanged.revision, created.revision);
        assert_eq!(unchanged.subject, created.subject);
        assert_eq!(unchanged.body, created.body);
        assert_eq!(unchanged.body_html, created.body_html);
        assert_eq!(unchanged.updated_at, created.updated_at);
    }

    #[test]
    fn queued_send_locks_the_draft_and_can_be_undone_before_submission() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("undo-send.db");
        let mut store = MuxStore::open(&path, true).expect("seeded store");
        let draft = store
            .save_draft(SaveDraftInput {
                id: None,
                account_id: "acc_work".into(),
                recipients: "jane@acme.example".into(),
                cc_recipients: String::new(),
                bcc_recipients: String::new(),
                subject: "Undo me".into(),
                body: "This should remain a draft.".into(),
                body_html: "<p>This should remain a draft.</p>".into(),
                reply_to_thread_id: None,
                expected_revision: None,
            })
            .unwrap();
        let operation = store.queue_send(&draft.id, 5_000).expect("send queued");
        assert!(store.get_draft(&draft.id).unwrap().unwrap().locked);
        let durable: (String, String, i64, String) = store
            .connection
            .query_row(
                "SELECT kind, state, length(payload_fingerprint), payload_json
                 FROM provider_work_items WHERE operation_id = ?1",
                [&operation.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(durable.0, "send");
        assert_eq!(durable.1, "queued");
        assert_eq!(durable.2, 32);
        let durable_payload: serde_json::Value = serde_json::from_str(&durable.3).unwrap();
        assert_eq!(durable_payload["accountId"], "acc_work");
        assert_eq!(durable_payload["body"], "This should remain a draft.");
        assert_eq!(durable_payload["snapshotVersion"], 3);
        assert!(durable_payload["queuedAtMs"].as_i64().is_some());
        assert!(durable_payload["submissionMessageId"]
            .as_str()
            .unwrap()
            .starts_with('<'));
        let prepared = crate::outgoing::prepare_from_durable_payload(&durable.3, &[])
            .expect("queued snapshot prepares as MIME");
        assert_eq!(
            prepared.reconciliation_key(),
            durable_payload["submissionMessageId"].as_str().unwrap()
        );
        assert_eq!(prepared.envelope_from, "jordan@acme.example");
        assert_eq!(prepared.envelope_recipients, ["jane@acme.example"]);
        assert!(!prepared.requires_smtp_utf8);
        crate::content::parse_mime(&prepared.raw_message)
            .expect("queued snapshot round-trips through the hostile MIME boundary");
        let undone = store
            .undo_operation(&operation.id)
            .expect("pending send undone");
        assert_eq!(undone.state, "cancelled");
        assert!(!store.get_draft(&draft.id).unwrap().unwrap().locked);
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT state FROM provider_work_items WHERE operation_id = ?1",
                    [&operation.id],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "cancelled"
        );
    }

    #[test]
    fn send_snapshot_v2_covers_the_frozen_date_and_v1_remains_readable() {
        let mut connection = Connection::open_in_memory().unwrap();
        let transaction = connection.transaction().unwrap();
        let draft = DraftSummary {
            id: "draft-versioned".into(),
            account_id: "account-versioned".into(),
            account_name: "Versioned".into(),
            account_email: "sender@example.com".into(),
            account_color: "#000000".into(),
            recipients: "recipient@example.com".into(),
            cc_recipients: String::new(),
            bcc_recipients: String::new(),
            subject: "Versioned snapshot".into(),
            body: "Immutable content".into(),
            body_html: "<p>Immutable content</p>".into(),
            reply_to_thread_id: Some(42),
            updated_at: 100,
            revision: 7,
            locked: true,
        };
        let submission_message_id = "<stable-v1@example.com>";
        let legacy_fingerprint = send_content_fingerprint(
            submission_message_id,
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
        );
        let legacy = SendPayload {
            draft_id: draft.id.clone(),
            message_id: "legacy-local-message".into(),
            snapshot_version: None,
            submission_message_id: Some(submission_message_id.into()),
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
            content_fingerprint_hex: Some(legacy_fingerprint),
            queued_at_ms: None,
            in_reply_to: None,
            references: Vec::new(),
            client_correlation_id: None,
            provider_kind: None,
            remote_thread_id: None,
        };
        let legacy_fields = send_projection_fields(&transaction, &legacy)
            .expect("complete schema-v14/v1 snapshot remains readable");
        assert_eq!(legacy_fields.0, draft.account_id);
        assert_eq!(legacy_fields.6, draft.body);

        let mut current = send_payload_for_draft(
            &draft,
            "current-local-message".into(),
            "<stable-v2@example.com>".into(),
            123_456,
            None,
            Vec::new(),
            "mux-stable-v3".into(),
            "fake".into(),
            None,
        );
        send_projection_fields(&transaction, &current).expect("v3 snapshot is valid");
        current.queued_at_ms = Some(123_457);
        assert!(matches!(
            send_projection_fields(&transaction, &current),
            Err(StoreError::Validation(_))
        ));
    }

    #[test]
    fn reopening_upgrades_pre_submission_v1_work_for_the_outgoing_boundary() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("queued-v1-upgrade.db");
        let mut store = MuxStore::open(&path, true).expect("seeded store");
        let draft = store
            .save_draft(SaveDraftInput {
                id: None,
                account_id: "acc_work".into(),
                recipients: "recipient@example.com".into(),
                cc_recipients: String::new(),
                bcc_recipients: String::new(),
                subject: "Queued legacy send".into(),
                body: "Preserve this immutable body".into(),
                body_html: String::new(),
                reply_to_thread_id: None,
                expected_revision: None,
            })
            .unwrap();
        let operation = store.queue_send(&draft.id, 60_000).unwrap();
        let (work_id, current_json, created_at, available_at): (String, String, i64, i64) = store
            .connection
            .query_row(
                "SELECT work.id, work.payload_json, operation.created_at, work.available_at
                 FROM provider_work_items work
                 JOIN operations operation ON operation.id = work.operation_id
                 WHERE operation.id = ?1",
                [&operation.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        let mut legacy: SendPayload = serde_json::from_str(&current_json).unwrap();
        let submission_message_id = legacy.submission_message_id.clone().unwrap();
        legacy.snapshot_version = None;
        legacy.queued_at_ms = None;
        legacy.client_correlation_id = None;
        legacy.provider_kind = None;
        legacy.remote_thread_id = None;
        legacy.content_fingerprint_hex = Some(send_content_fingerprint(
            &submission_message_id,
            legacy.account_id.as_deref().unwrap(),
            legacy.sender_email.as_deref().unwrap(),
            legacy.recipients.as_deref().unwrap(),
            legacy.cc_recipients.as_deref().unwrap(),
            legacy.bcc_recipients.as_deref().unwrap(),
            legacy.subject.as_deref().unwrap(),
            legacy.body.as_deref().unwrap(),
            legacy.body_html.as_deref().unwrap(),
            legacy.reply_to_thread_id,
            legacy.draft_revision.unwrap(),
        ));
        let legacy_json = serde_json::to_string(&legacy).unwrap();
        let legacy_digest = Sha256::digest(legacy_json.as_bytes());
        let transaction = store.connection.transaction().unwrap();
        transaction
            .execute(
                "UPDATE operations SET payload_json = ?2 WHERE id = ?1",
                params![operation.id, &legacy_json],
            )
            .unwrap();
        transaction
            .execute("DELETE FROM provider_work_items WHERE id = ?1", [&work_id])
            .unwrap();
        transaction
            .execute(
                "INSERT INTO provider_work_items(
                   id, account_id, operation_id, kind, scope, ordering_key, retry_safety,
                   payload_json, payload_fingerprint, state, priority, created_at,
                   available_at, attempt_count, max_attempts, cancel_requested
                 ) VALUES(
                   ?1, 'acc_work', ?2, 'send', 'outgoing:v1', ?3,
                   'non_idempotent_send', ?4, ?5, 'queued', 100, ?6, ?7, 0, 8, 0
                 )",
                params![
                    work_id,
                    operation.id,
                    submission_message_id,
                    legacy_json,
                    legacy_digest.as_slice(),
                    created_at,
                    available_at,
                ],
            )
            .unwrap();
        transaction.commit().unwrap();
        drop(store);

        let reopened = MuxStore::open(&path, false).expect("v1 work upgrades atomically");
        let (work_json, operation_json): (String, String) = reopened
            .connection
            .query_row(
                "SELECT work.payload_json, operation.payload_json
                 FROM provider_work_items work
                 JOIN operations operation ON operation.id = work.operation_id
                 WHERE work.id = ?1",
                [&work_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(work_json, operation_json);
        let upgraded: serde_json::Value = serde_json::from_str(&work_json).unwrap();
        assert_eq!(upgraded["snapshotVersion"], 2);
        assert_eq!(upgraded["queuedAtMs"], created_at);
        assert_eq!(upgraded["submissionMessageId"], submission_message_id);
        crate::outgoing::prepare_from_durable_payload(&work_json, &[])
            .expect("upgraded v1 work reaches outgoing preparation");
    }

    #[test]
    fn a_reply_to_a_confirmed_local_send_freezes_real_threading_headers() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("reply-threading.db");
        let mut store = MuxStore::open(&path, true).expect("seeded store");
        let initial_parent_message_id: String = store
            .connection
            .query_row(
                "SELECT internet_message_id FROM messages
                 WHERE thread_id = 1 ORDER BY sent_at DESC, id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let first = store
            .save_draft(SaveDraftInput {
                id: None,
                account_id: "acc_work".into(),
                recipients: "jane@example.com".into(),
                cc_recipients: String::new(),
                bcc_recipients: String::new(),
                subject: "First local reply".into(),
                body: "First".into(),
                body_html: String::new(),
                reply_to_thread_id: Some(1),
                expected_revision: None,
            })
            .unwrap();
        let first_operation = store.queue_send(&first.id, 1_000).unwrap();
        let first_message_id: String = store
            .connection
            .query_row(
                "SELECT json_extract(payload_json, '$.submissionMessageId')
                 FROM operations WHERE id = ?1",
                [&first_operation.id],
                |row| row.get(0),
            )
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE operations SET not_before = 0 WHERE id = ?1",
                [&first_operation.id],
            )
            .unwrap();
        store
            .process_due_operations(10)
            .expect("first send confirms");

        let second = store
            .save_draft(SaveDraftInput {
                id: None,
                account_id: "acc_work".into(),
                recipients: "jane@example.com".into(),
                cc_recipients: String::new(),
                bcc_recipients: String::new(),
                subject: "Second local reply".into(),
                body: "Second".into(),
                body_html: String::new(),
                reply_to_thread_id: Some(1),
                expected_revision: None,
            })
            .unwrap();
        let second_operation = store.queue_send(&second.id, 60_000).unwrap();
        let second_json: String = store
            .connection
            .query_row(
                "SELECT payload_json FROM operations WHERE id = ?1",
                [&second_operation.id],
                |row| row.get(0),
            )
            .unwrap();
        let second_payload: serde_json::Value = serde_json::from_str(&second_json).unwrap();
        assert_eq!(second_payload["inReplyTo"], first_message_id);
        assert_eq!(
            second_payload["references"],
            serde_json::json!([initial_parent_message_id, first_message_id])
        );
        let prepared = crate::outgoing::prepare_from_durable_payload(&second_json, &[])
            .expect("threaded reply prepares");
        let raw = String::from_utf8(prepared.raw_message).unwrap();
        assert!(raw.contains(&format!("In-Reply-To: {first_message_id}\r\n")));
        let parsed = crate::content::parse_mime(raw.as_bytes()).expect("threaded MIME parses");
        assert_eq!(
            parsed.references,
            [initial_parent_message_id, first_message_id]
        );
    }

    #[test]
    fn first_reply_to_a_provider_message_freezes_its_real_threading_headers() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("provider-reply-threading.db");
        let mut store = configured_provider_store(&path, &["acc-threading"]);
        let mut batch = sample_provider_batch("acc-threading", "threading-batch", "cursor-1");
        let message = batch.message_upserts.first_mut().unwrap();
        message.internet_message_id = Some("<provider-message@example.test>".into());
        message.in_reply_to = Some("<provider-parent@example.test>".into());
        message.references = Some(vec![
            "<provider-root@example.test>".into(),
            "<provider-parent@example.test>".into(),
        ]);
        store.apply_provider_batch(batch).expect("provider batch");
        let thread_id: i64 = store
            .connection
            .query_row(
                "SELECT thread_id FROM provider_thread_refs
                 WHERE account_id = 'acc-threading' AND remote_thread_id = 'remote-thread-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let draft = store
            .save_draft(SaveDraftInput {
                id: None,
                account_id: "acc-threading".into(),
                recipients: "sender@example.com".into(),
                cc_recipients: String::new(),
                bcc_recipients: String::new(),
                subject: "Re: Provider batch subject".into(),
                body: "A first real reply".into(),
                body_html: String::new(),
                reply_to_thread_id: Some(thread_id),
                expected_revision: None,
            })
            .unwrap();
        let operation = store.queue_send(&draft.id, 60_000).unwrap();
        let payload_json: String = store
            .connection
            .query_row(
                "SELECT payload_json FROM operations WHERE id = ?1",
                [&operation.id],
                |row| row.get(0),
            )
            .unwrap();
        let payload: serde_json::Value = serde_json::from_str(&payload_json).unwrap();
        assert_eq!(payload["inReplyTo"], "<provider-message@example.test>");
        assert_eq!(
            payload["references"],
            serde_json::json!([
                "<provider-root@example.test>",
                "<provider-parent@example.test>",
                "<provider-message@example.test>"
            ])
        );
        let prepared = crate::outgoing::prepare_from_durable_payload(&payload_json, &[])
            .expect("provider reply prepares");
        let parsed = crate::content::parse_mime(&prepared.raw_message).expect("reply MIME parses");
        assert_eq!(
            parsed.in_reply_to.as_deref(),
            Some("<provider-message@example.test>")
        );
        assert_eq!(
            parsed.references,
            [
                "<provider-root@example.test>",
                "<provider-parent@example.test>",
                "<provider-message@example.test>"
            ]
        );
    }

    #[test]
    fn due_reply_send_appends_rich_message_and_removes_draft_atomically() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("reply-send.db");
        let mut store = MuxStore::open(&path, true).expect("seeded store");
        let before: i64 = store
            .connection
            .query_row(
                "SELECT message_count FROM threads WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let draft = store
            .save_draft(SaveDraftInput {
                id: None,
                account_id: "acc_work".into(),
                recipients: "jane@acme.example".into(),
                cc_recipients: "kevin@acme.example".into(),
                bcc_recipients: String::new(),
                subject: "Re: Project update".into(),
                body: "The important plan".into(),
                body_html: "<p>The <strong>important</strong> plan</p>".into(),
                reply_to_thread_id: Some(1),
                expected_revision: None,
            })
            .unwrap();
        let operation = store.queue_send(&draft.id, 1_000).unwrap();
        store
            .connection
            .execute(
                "UPDATE operations SET not_before = 0 WHERE id = ?1",
                [&operation.id],
            )
            .unwrap();
        let result = store.process_due_operations(10).expect("send confirms");
        assert_eq!(result.confirmed, vec![operation.id.clone()]);
        assert!(store.get_draft(&draft.id).unwrap().is_none());
        let sent: (i64, String) = store
            .connection
            .query_row(
                "SELECT t.message_count, m.body_html
                 FROM threads t JOIN messages m ON m.thread_id = t.id
                 WHERE t.id = 1 ORDER BY m.id DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(sent.0, before + 1);
        assert_eq!(sent.1, "<p>The <strong>important</strong> plan</p>");
    }

    #[test]
    fn durable_worker_confirms_local_projection_without_a_renderer_loop() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("native-worker-integration.db");
        let mut store = MuxStore::open(&path, true).expect("seeded store");
        let operation = store.apply_thread_action(1, "archive").unwrap();
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT in_inbox FROM thread_effective WHERE id = 1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0,
            "pending local intent renders before provider acknowledgement"
        );
        drop(store);

        let worker = DurableWorker::new(&path, WorkerConfig::default()).unwrap();
        let due = operation.not_before + 1;
        let result = worker
            .run_cycle(
                "integration-worker",
                &|_: &ClaimedWork, _: &crate::worker::WorkerExecutionContext| {
                    WorkerOutcome::Succeeded {
                        projection: WorkerProjection::LocalOperation,
                    }
                },
                &apply_worker_projection,
                &|| due,
            )
            .unwrap();
        assert_eq!(result.succeeded, 1);
        assert!(result.errors.is_empty());

        let verify = MuxStore::open(&path, false).unwrap();
        let states: (String, String, i64) = verify
            .connection
            .query_row(
                "SELECT operation.state, work.state, thread.remote_in_inbox
                 FROM operations operation
                 JOIN provider_work_items work ON work.operation_id = operation.id
                 JOIN threads thread ON thread.id = operation.thread_id
                 WHERE operation.id = ?1",
                [&operation.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(states, ("confirmed".into(), "succeeded".into(), 0));
    }

    #[test]
    fn gmail_provider_conformance_mutation_journal_projects_memberships_trash_labels_and_undo() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("gmail-mutation-journal.db");
        let mut store = configured_provider_store(&path, &["gmail-account"]);
        store
            .apply_provider_batch(sample_provider_batch(
                "gmail-account",
                "gmail-seed",
                "cursor-1",
            ))
            .unwrap();
        store
            .connection
            .execute_batch(
                "UPDATE provider_accounts SET provider_kind = 'gmail'
                 WHERE account_id = 'gmail-account';
                 DELETE FROM provider_container_memberships
                 WHERE account_id = 'gmail-account';
                 DELETE FROM provider_containers WHERE account_id = 'gmail-account';
                 INSERT INTO provider_containers(
                   account_id, remote_id, name, kind, role, sort_order, is_selectable
                 ) VALUES
                   ('gmail-account', 'INBOX', 'Inbox', 'label', 'inbox', 0, 1),
                   ('gmail-account', 'UNREAD', 'Unread', 'label', 'custom', 1, 0),
                   ('gmail-account', 'STARRED', 'Starred', 'label', 'starred', 2, 1),
                   ('gmail-account', 'TRASH', 'Trash', 'label', 'trash', 3, 1),
                   ('gmail-account', 'Label_1', 'Projects', 'label', 'custom', 4, 1);
                 INSERT INTO messages(
                   id, thread_id, sender_name, sender_email, recipients,
                   sent_at, body_text, is_from_me
                 ) SELECT 900001, thread_id, 'Second Sender', 'second@example.com',
                          'reader@example.com', 901, 'Second provider message', 0
                   FROM provider_thread_refs
                   WHERE account_id = 'gmail-account'
                     AND remote_thread_id = 'remote-thread-1';
                 INSERT INTO provider_message_refs(
                   account_id, remote_message_id, message_id, remote_thread_id,
                   revision, body_state
                 ) VALUES(
                   'gmail-account', 'remote-message-2', 900001,
                   'remote-thread-1', 'message-r2', 'normalized'
                 );
                 UPDATE threads SET message_count = 2
                  WHERE id = (SELECT thread_id FROM provider_thread_refs
                              WHERE account_id = 'gmail-account'
                                AND remote_thread_id = 'remote-thread-1');
                 INSERT INTO provider_container_memberships(
                   account_id, remote_message_id, remote_container_id
                 ) VALUES
                   ('gmail-account', 'remote-message-1', 'INBOX'),
                   ('gmail-account', 'remote-message-2', 'INBOX'),
                   ('gmail-account', 'remote-message-1', 'Label_1');",
            )
            .unwrap();
        let thread_id: i64 = store
            .connection
            .query_row(
                "SELECT thread_id FROM provider_thread_refs
                 WHERE account_id = 'gmail-account' AND remote_thread_id = 'remote-thread-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let archive = store.apply_thread_action(thread_id, "archive").unwrap();
        let snapshots: (String, String, String) = store
            .connection
            .query_row(
                "SELECT operation.payload_json, work.payload_json, work.scope
                 FROM operations operation
                 JOIN provider_work_items work ON work.operation_id = operation.id
                 WHERE operation.id = ?1",
                [&archive.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(snapshots.0, snapshots.1);
        assert_eq!(snapshots.2, crate::gmail::GMAIL_ACCOUNT_SCOPE);
        let payload: ThreadMutationPayload = serde_json::from_str(&snapshots.0).unwrap();
        assert_eq!(payload.remote_thread_id.as_deref(), Some("remote-thread-1"));
        assert_eq!(payload.account_id.as_deref(), Some("gmail-account"));
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT in_inbox FROM thread_effective WHERE id = ?1",
                    [thread_id],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0,
            "pending Gmail intent overlays immediately"
        );

        let worker = DurableWorker::new(&path, WorkerConfig::default()).unwrap();
        let run_success = |now: i64| {
            worker
                .run_cycle(
                    "gmail-mutation-worker",
                    &|_: &ClaimedWork, _: &crate::worker::WorkerExecutionContext| {
                        WorkerOutcome::Succeeded {
                            projection: WorkerProjection::LocalOperation,
                        }
                    },
                    &apply_worker_projection,
                    &|| now,
                )
                .unwrap()
        };
        assert_eq!(run_success(archive.not_before + 1).succeeded, 1);
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM provider_container_memberships
                     WHERE account_id = 'gmail-account'
                       AND remote_message_id = 'remote-message-1'
                       AND remote_container_id = 'INBOX'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );

        let undo = store
            .undo_operation(&archive.id)
            .expect("confirmed Gmail undo queues");
        let undo_payload: ThreadMutationPayload = serde_json::from_str(
            &store
                .connection
                .query_row(
                    "SELECT payload_json FROM operations WHERE id = ?1",
                    [&undo.id],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
        )
        .unwrap();
        assert_eq!(undo_payload.undo_of.as_deref(), Some(archive.id.as_str()));
        assert_eq!(run_success(undo.not_before + 1).succeeded, 1);
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM provider_container_memberships
                     WHERE account_id = 'gmail-account'
                       AND remote_message_id = 'remote-message-1'
                       AND remote_container_id = 'INBOX'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );

        let label = store
            .apply_thread_label_action(thread_id, "Label_1", true)
            .expect("partial custom label normalizes to present");
        let pending_label: (i64, String) = store
            .connection
            .query_row(
                "SELECT effective.label_state, operation.old_value
                 FROM provider_thread_label_effective effective
                 JOIN operations operation ON operation.id = ?1
                 WHERE effective.account_id = 'gmail-account'
                   AND effective.thread_id = ?2
                   AND effective.remote_container_id = 'Label_1'",
                params![label.id, thread_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            pending_label,
            (1, "2".into()),
            "pending desired state overlays the confirmed partial membership"
        );
        assert_eq!(run_success(label.not_before + 1).succeeded, 1);
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM provider_container_memberships
                     WHERE account_id = 'gmail-account'
                       AND remote_container_id = 'Label_1'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            2
        );
        assert!(matches!(
            store.undo_operation(&label.id),
            Err(StoreError::Conflict(message))
                if message.contains("cannot be restored exactly")
        ));

        let trash = store.apply_thread_action(thread_id, "delete").unwrap();
        assert!(store
            .list_threads(ThreadPageInput {
                account_id: Some("gmail-account".into()),
                view: Some("all".into()),
                cursor: None,
                limit: Some(10),
                hidden_account_ids: Vec::new(),
                container_id: None,
            })
            .unwrap()
            .threads
            .is_empty());
        assert_eq!(
            store
                .list_threads(ThreadPageInput {
                    account_id: Some("gmail-account".into()),
                    view: Some("trash".into()),
                    cursor: None,
                    limit: Some(10),
                    hidden_account_ids: Vec::new(),
                    container_id: None,
                })
                .unwrap()
                .threads
                .len(),
            1
        );
        assert_eq!(run_success(trash.not_before + 1).succeeded, 1);
        let confirmed: (i64, i64) = store
            .connection
            .query_row(
                "SELECT
                   EXISTS(SELECT 1 FROM provider_container_memberships
                     WHERE account_id = 'gmail-account'
                       AND remote_message_id = 'remote-message-1'
                       AND remote_container_id = 'TRASH'),
                   EXISTS(SELECT 1 FROM provider_container_memberships
                     WHERE account_id = 'gmail-account'
                       AND remote_message_id = 'remote-message-1'
                       AND remote_container_id = 'INBOX')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(confirmed, (1, 0));
    }

    #[test]
    fn gmail_provider_conformance_missing_mutation_target_is_reconciled_atomically() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("gmail-mutation-404.db");
        let mut store = configured_provider_store(&path, &["gmail-account"]);
        store
            .apply_provider_batch(sample_provider_batch(
                "gmail-account",
                "gmail-seed",
                "cursor-1",
            ))
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE provider_accounts SET provider_kind = 'gmail'
                 WHERE account_id = 'gmail-account'",
                [],
            )
            .unwrap();
        let thread_id: i64 = store
            .connection
            .query_row(
                "SELECT thread_id FROM provider_thread_refs
                 WHERE account_id = 'gmail-account' AND remote_thread_id = 'remote-thread-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let operation = store.apply_thread_action(thread_id, "star").unwrap();
        drop(store);

        let worker = DurableWorker::new(&path, WorkerConfig::default()).unwrap();
        let result = worker
            .run_cycle(
                "gmail-404-worker",
                &|_: &ClaimedWork, _: &crate::worker::WorkerExecutionContext| {
                    WorkerOutcome::Succeeded {
                        projection: WorkerProjection::RemoteThreadAbsent {
                            remote_thread_id: "remote-thread-1".into(),
                        },
                    }
                },
                &crate::provider_conformance::apply_worker_projection,
                &|| operation.not_before + 1,
            )
            .unwrap();
        assert_eq!(result.succeeded, 1);
        assert!(result.errors.is_empty());
        let connection = Connection::open(&path).unwrap();
        let state: (String, String, i64, i64, i64) = connection
            .query_row(
                "SELECT operation.state, work.state, thread.remote_deleted,
                        (SELECT COUNT(*) FROM provider_thread_refs
                         WHERE account_id = 'gmail-account'),
                        (SELECT COUNT(*) FROM provider_message_refs
                         WHERE account_id = 'gmail-account')
                 FROM operations operation
                 JOIN provider_work_items work ON work.operation_id = operation.id
                 JOIN threads thread ON thread.id = operation.thread_id
                 WHERE operation.id = ?1",
                [&operation.id],
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
        assert_eq!(state, ("confirmed".into(), "succeeded".into(), 1, 0, 0));
    }

    #[test]
    fn durable_worker_projects_the_immutable_send_snapshot() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("native-worker-send.db");
        let mut store = MuxStore::open(&path, true).expect("seeded store");
        let draft = store
            .save_draft(SaveDraftInput {
                id: None,
                account_id: "acc_work".into(),
                recipients: "jane@acme.example".into(),
                cc_recipients: String::new(),
                bcc_recipients: String::new(),
                subject: "Worker send".into(),
                body: "A durable snapshot".into(),
                body_html: "<p>A durable snapshot</p>".into(),
                reply_to_thread_id: Some(1),
                expected_revision: None,
            })
            .unwrap();
        let operation = store.queue_send(&draft.id, 1_000).unwrap();
        drop(store);

        let worker = DurableWorker::new(&path, WorkerConfig::default()).unwrap();
        let due = operation.not_before + 1;
        let result = worker
            .run_cycle(
                "send-worker",
                &|_: &ClaimedWork, _: &crate::worker::WorkerExecutionContext| {
                    WorkerOutcome::Succeeded {
                        projection: WorkerProjection::LocalOperation,
                    }
                },
                &apply_worker_projection,
                &|| due,
            )
            .unwrap();
        assert_eq!(result.succeeded, 1);
        assert!(result.errors.is_empty());

        let verify = MuxStore::open(&path, false).unwrap();
        assert!(verify.get_draft(&draft.id).unwrap().is_none());
        let projected: (String, String, String) = verify
            .connection
            .query_row(
                "SELECT operation.state, work.state, message.body_html
                 FROM operations operation
                 JOIN provider_work_items work ON work.operation_id = operation.id
                 JOIN messages message ON message.thread_id = operation.thread_id
                 WHERE operation.id = ?1
                 ORDER BY message.id DESC LIMIT 1",
                [&operation.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            projected,
            (
                "confirmed".into(),
                "succeeded".into(),
                "<p>A durable snapshot</p>".into()
            )
        );
    }

    #[test]
    fn smtp_confirmation_reuses_an_imap_sent_observation_that_arrived_first() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("smtp-imap-sent-race.db");
        let mut store = configured_provider_store(&path, &["imap-account"]);
        store
            .connection
            .execute_batch(
                "UPDATE accounts SET provider = 'imap' WHERE id = 'imap-account';
                 UPDATE provider_accounts SET provider_kind = 'imap'
                 WHERE account_id = 'imap-account';
                 INSERT INTO provider_capabilities(account_id, capability, enabled)
                 VALUES('imap-account', 'outgoing_mail', 1);",
            )
            .expect("SMTP-capable IMAP account");
        let draft = store
            .save_draft(SaveDraftInput {
                id: None,
                account_id: "imap-account".into(),
                recipients: "recipient@example.test".into(),
                cc_recipients: String::new(),
                bcc_recipients: "blind@example.test".into(),
                subject: "Race-safe send".into(),
                body: "Frozen local body".into(),
                body_html: "<p>Frozen local body</p>".into(),
                reply_to_thread_id: None,
                expected_revision: None,
            })
            .expect("draft");
        let operation = store.queue_send(&draft.id, 10).expect("queued SMTP send");
        let payload_json: String = store
            .connection
            .query_row(
                "SELECT payload_json FROM operations WHERE id = ?1",
                [&operation.id],
                |row| row.get(0),
            )
            .expect("send snapshot");
        let payload: SendPayload =
            serde_json::from_str(&payload_json).expect("typed send snapshot");
        let message_id = payload
            .submission_message_id
            .clone()
            .expect("submission Message-ID");
        let correlation = payload
            .client_correlation_id
            .clone()
            .expect("client correlation");
        let batch: ProviderBatch = serde_json::from_value(serde_json::json!({
            "muxAccountId": "imap-account",
            "batchId": "sent-before-smtp-confirmation",
            "expectedPriorCursor": null,
            "cursor": {
                "muxAccountId": "imap-account",
                "scope": { "kind": "account" },
                "value": "sent-cursor-1"
            },
            "observedAt": 11,
            "threadUpserts": [{
                "identity": {
                    "muxAccountId": "imap-account",
                    "remoteThreadId": "imap-sent-thread"
                },
                "subject": "Race-safe send",
                "participants": "recipient@example.test",
                "snippet": "Remote parsed body",
                "latestAt": 11,
                "messageCount": 1,
                "inInbox": false,
                "unread": false,
                "starred": false,
                "hasAttachments": false,
                "hasInvite": false,
                "hasLinks": false,
                "hasFromMe": true,
                "category": "sent",
                "revision": "thread-r1"
            }],
            "messageUpserts": [{
                "identity": {
                    "muxAccountId": "imap-account",
                    "remoteMessageId": "imap:sent-folder:7:42",
                    "remoteThreadId": "imap-sent-thread"
                },
                "subject": "Race-safe send",
                "senderName": "Me",
                "senderEmail": "imap-account@example.com",
                "recipients": "recipient@example.test",
                "ccRecipients": "",
                "bccRecipients": "",
                "sentAt": 11,
                "bodyText": "Remote parsed body",
                "bodyState": "complete",
                "isFromMe": true,
                "revision": "message-r1",
                "keywords": [],
                "internetMessageId": message_id,
                "clientCorrelationId": correlation
            }],
            "containerUpserts": [{
                "identity": {
                    "muxAccountId": "imap-account",
                    "remoteContainerId": "sent-folder"
                },
                "displayName": "Sent",
                "kind": "folder",
                "role": "sent",
                "parentRemoteContainerId": null,
                "selectable": true
            }],
            "membershipChanges": [{
                "kind": "upsert",
                "membership": {
                    "message": {
                        "muxAccountId": "imap-account",
                        "remoteMessageId": "imap:sent-folder:7:42",
                        "remoteThreadId": "imap-sent-thread"
                    },
                    "container": {
                        "muxAccountId": "imap-account",
                        "remoteContainerId": "sent-folder"
                    }
                }
            }],
            "tombstones": []
        }))
        .expect("IMAP Sent observation");
        drop(store);

        let worker = DurableWorker::new(&path, WorkerConfig::default()).expect("worker");
        let result = worker
            .run_cycle(
                "smtp-race-worker",
                &|_: &ClaimedWork, _: &crate::worker::WorkerExecutionContext| {
                    let mut racing_connection =
                        Connection::open(&path).expect("open during SMTP execution");
                    racing_connection
                        .busy_timeout(std::time::Duration::from_secs(2))
                        .expect("race connection timeout");
                    let executing: (String, String) = racing_connection
                        .query_row(
                            "SELECT operation.state, work.state
                             FROM operations operation
                             JOIN provider_work_items work
                               ON work.operation_id = operation.id
                             WHERE operation.id = ?1",
                            [&operation.id],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .expect("executing send fence");
                    assert_eq!(executing, ("executing".into(), "executing".into()));
                    let sync_claim = ClaimedWork {
                        id: "sent-race-sync".into(),
                        account_id: "imap-account".into(),
                        operation_id: None,
                        kind: WorkKind::Sync,
                        scope: "sync:account".into(),
                        ordering_key: "sent-before-smtp-confirmation".into(),
                        payload_json: "{}".into(),
                        payload_fingerprint_hex: "00".repeat(32),
                        attempt: 1,
                        lease_token: "sent-race-lease".into(),
                        lease_expires_at: operation.not_before + 10_000,
                    };
                    let page = crate::worker::ProviderSyncPage {
                        batch: Box::new(batch.clone()),
                        restricted_message_content: vec![
                            crate::worker::RestrictedMessageContent {
                                identity: batch.message_upserts[0].identity.clone(),
                                body_html: "<p>Remote parsed body<mux-remote-image data-id=\"1\"></mux-remote-image></p>".into(),
                                blocked_remote_resources: 1,
                                remote_images: vec![crate::content::RemoteImageCandidate {
                                    resource_id: 1,
                                    url: "https://images.example.test/tracker.png".into(),
                                    domain: "images.example.test".into(),
                                    alt_text: "Tracker".into(),
                                }],
                            },
                        ],
                        continuation: None,
                        complete: true,
                        capabilities: None,
                        replace_memberships_for_upserted_messages: true,
                        derive_thread_state_from_messages: true,
                        reconciliation: None,
                    };
                    let transaction = racing_connection
                        .transaction()
                        .expect("provider page transaction");
                    crate::provider_conformance::apply_worker_projection(
                        &transaction,
                        &sync_claim,
                        &WorkerProjection::ProviderSyncPage(Box::new(page)),
                    )
                    .expect("Sent observation before SMTP acknowledgement");
                    transaction.commit().expect("commit provider page");
                    let observed: (i64, String, i64, i64) = racing_connection
                        .query_row(
                            "SELECT COUNT(*), reference.client_correlation_id,
                                    message.blocked_remote_resources,
                                    (SELECT COUNT(*) FROM message_remote_images
                                     WHERE message_id = reference.message_id)
                             FROM provider_message_refs reference
                             JOIN messages message ON message.id = reference.message_id
                             WHERE reference.account_id = 'imap-account'",
                            [],
                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                        )
                        .expect("persisted Sent identity");
                    assert_eq!(observed, (1, correlation.clone(), 1, 1));
                    WorkerOutcome::Succeeded {
                        projection: WorkerProjection::LocalOperation,
                    }
                },
                &apply_worker_projection,
                &|| operation.not_before + 1,
            )
            .expect("SMTP success projection");
        assert_eq!(result.succeeded, 1);
        assert!(result.errors.is_empty());

        let verify = MuxStore::open(&path, false).expect("reopen projected mail");
        let final_state: (i64, i64, String, String, String, i64, i64) = verify
            .connection
            .query_row(
                "SELECT
                   (SELECT COUNT(*) FROM messages),
                   (SELECT COUNT(*) FROM threads WHERE remote_deleted = 0),
                   operation.state, message.body_text, message.bcc_recipients,
                   message.blocked_remote_resources,
                   (SELECT COUNT(*) FROM message_remote_images
                    WHERE message_id = message.id)
                 FROM operations operation
                 JOIN provider_message_refs reference ON reference.account_id = 'imap-account'
                 JOIN messages message ON message.id = reference.message_id
                 WHERE operation.id = ?1",
                [&operation.id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .expect("race-safe final state");
        assert_eq!(
            final_state,
            (
                1,
                1,
                "confirmed".into(),
                "Remote parsed body".into(),
                "blind@example.test".into(),
                1,
                1,
            )
        );
        let provider_html: String = verify
            .connection
            .query_row(
                "SELECT message.body_html
                 FROM provider_message_refs reference
                 JOIN messages message ON message.id = reference.message_id
                 WHERE reference.account_id = 'imap-account'",
                [],
                |row| row.get(0),
            )
            .expect("provider HTML remains authoritative");
        assert!(provider_html.contains("<mux-remote-image data-id=\"1\""));
        assert!(verify.get_draft(&draft.id).expect("draft lookup").is_none());
    }

    #[test]
    fn pending_thread_action_overlays_immediately_and_undo_restores_projection() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("thread-action.db");
        let mut store = MuxStore::open(&path, true).expect("seeded store");
        let operation = store.apply_thread_action(1, "archive").unwrap();
        let effective = |store: &MuxStore| {
            store
                .connection
                .query_row(
                    "SELECT in_inbox FROM thread_effective WHERE id = 1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap()
        };
        assert_eq!(effective(&store), 0);
        store.undo_operation(&operation.id).unwrap();
        assert_eq!(effective(&store), 1);
    }

    #[test]
    fn read_only_imap_mutation_is_rejected_before_local_intent_is_journaled() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("imap-read-only-mutation.db");
        let mut store = configured_provider_store(&path, &["imap-account"]);
        store
            .apply_provider_batch(sample_provider_batch(
                "imap-account",
                "imap-seed",
                "imap-cursor",
            ))
            .expect("IMAP projection fixture");
        store
            .connection
            .execute_batch(
                "UPDATE provider_accounts SET provider_kind = 'imap'
                 WHERE account_id = 'imap-account';
                 DELETE FROM provider_capabilities
                 WHERE account_id = 'imap-account' AND capability = 'mutations';",
            )
            .expect("mark account as IMAP");
        let thread_id: i64 = store
            .connection
            .query_row(
                "SELECT thread_id FROM provider_thread_refs
                 WHERE account_id = 'imap-account' AND remote_thread_id = 'remote-thread-1'",
                [],
                |row| row.get(0),
            )
            .expect("remote thread mapping");
        let before: (i64, i64, i64) = store
            .connection
            .query_row(
                "SELECT
                   (SELECT COUNT(*) FROM operations),
                   (SELECT COUNT(*) FROM provider_work_items),
                   (SELECT in_inbox FROM thread_effective WHERE id = ?1)",
                [thread_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("preflight state");
        let error = store
            .apply_thread_action(thread_id, "archive")
            .expect_err("receive-only IMAP must reject remote mutation");
        assert!(matches!(error, StoreError::Conflict(message) if message.contains("read-only")));
        let after: (i64, i64, i64) = store
            .connection
            .query_row(
                "SELECT
                   (SELECT COUNT(*) FROM operations),
                   (SELECT COUNT(*) FROM provider_work_items),
                   (SELECT in_inbox FROM thread_effective WHERE id = ?1)",
                [thread_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("post-rejection state");
        assert_eq!(
            after, before,
            "rejection must not create pending local intent"
        );

        let legacy_payload = serde_json::to_string(&ThreadMutationPayload {
            format_version: Some(1),
            operation_id: "legacy-imap-star".into(),
            account_id: Some("imap-account".into()),
            thread_id,
            field: "starred".into(),
            value: "1".into(),
            remote_thread_id: Some("remote-thread-1".into()),
            remote_container_id: None,
            undo_of: None,
        })
        .expect("legacy payload");
        store
            .connection
            .execute(
                "UPDATE threads SET remote_starred = 1 WHERE id = ?1",
                [thread_id],
            )
            .expect("legacy confirmed projection");
        store
            .connection
            .execute(
                "INSERT INTO operations(
                   id, thread_id, field, kind, old_value, new_value,
                   payload_json, state, created_at, not_before, confirmed_at
                 ) VALUES(
                   'legacy-imap-star', ?1, 'starred', 'star', '0', '1',
                   ?2, 'confirmed', 1, 1, 2
                 )",
                params![thread_id, legacy_payload],
            )
            .expect("legacy confirmed operation");
        let before_undo: (i64, i64, i64) = store
            .connection
            .query_row(
                "SELECT
                   (SELECT COUNT(*) FROM operations),
                   (SELECT COUNT(*) FROM provider_work_items),
                   (SELECT starred FROM thread_effective WHERE id = ?1)",
                [thread_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("state before legacy undo");
        let error = store
            .undo_operation("legacy-imap-star")
            .expect_err("confirmed legacy IMAP mutation must not create an inverse");
        assert!(matches!(error, StoreError::Conflict(message) if message.contains("read-only")));
        let after_undo: (i64, i64, i64) = store
            .connection
            .query_row(
                "SELECT
                   (SELECT COUNT(*) FROM operations),
                   (SELECT COUNT(*) FROM provider_work_items),
                   (SELECT starred FROM thread_effective WHERE id = ?1)",
                [thread_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("state after rejected legacy undo");
        assert_eq!(after_undo, before_undo);
    }

    #[test]
    fn newest_active_intent_overlays_confirmed_projection_deterministically() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("overlay.db");
        let store = MuxStore::open(&path, true).expect("seeded store");
        let insert = "INSERT INTO operations(
            id, thread_id, field, kind, old_value, new_value, state, created_at, not_before
          ) VALUES(?1, 1, 'in_inbox', ?2, ?3, ?4, 'pending', 42, 42)";
        store
            .connection
            .execute(insert, params!["archive-1", "archive", "1", "0"])
            .unwrap();
        store
            .connection
            .execute(insert, params!["restore-1", "restore", "0", "1"])
            .unwrap();

        let effective = || {
            store
                .connection
                .query_row(
                    "SELECT in_inbox FROM thread_effective WHERE id = 1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap()
        };
        assert_eq!(effective(), 1);
        store
            .connection
            .execute(
                "UPDATE operations SET state = 'cancelled' WHERE id = 'restore-1'",
                [],
            )
            .unwrap();
        assert_eq!(effective(), 0);
        store
            .connection
            .execute(
                "UPDATE operations SET state = 'cancelled' WHERE id = 'archive-1'",
                [],
            )
            .unwrap();
        assert_eq!(effective(), 1);
    }

    #[test]
    fn restart_distinguishes_uncertain_send_from_retryable_mutation() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("recovery.db");
        let store = MuxStore::open(&path, true).expect("seeded store");
        let insert = "INSERT INTO operations(
            id, thread_id, field, kind, old_value, new_value, state, created_at, not_before
          ) VALUES(?1, 1, ?2, ?3, ?4, ?5, 'executing', 1, 1)";
        store
            .connection
            .execute(
                insert,
                params!["send-1", "send", "send", "draft", "submitted"],
            )
            .unwrap();
        store
            .connection
            .execute(
                insert,
                params!["archive-1", "in_inbox", "archive", "1", "0"],
            )
            .unwrap();
        drop(store);

        let recovered = MuxStore::open(&path, false).expect("store recovers");
        let send_state: String = recovered
            .connection
            .query_row(
                "SELECT state FROM operations WHERE id = 'send-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let archive: (String, i64) = recovered
            .connection
            .query_row(
                "SELECT state, not_before FROM operations WHERE id = 'archive-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(send_state, "outcome_unknown");
        assert_eq!(archive.0, "retrying");
        assert!(archive.1 > 1);
    }

    #[test]
    fn mailbox_views_counts_and_search_treat_only_future_snoozes_as_hidden() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("mailbox-views.db");
        let mut store = MuxStore::open(&path, true).expect("seeded store");
        let before = store.bootstrap().unwrap().view_counts.remove(0);
        assert_eq!(
            (before.inbox, before.archive, before.all, before.snoozed),
            (4, 1, 5, 0)
        );

        let wake_at = now_ms() + 86_400_000;
        store.snooze_thread(1, wake_at).expect("thread snoozed");
        store
            .apply_thread_action(2, "archive")
            .expect("archive overlays immediately");
        let counts = store.bootstrap().unwrap().view_counts.remove(0);
        assert_eq!(
            (counts.inbox, counts.archive, counts.all, counts.snoozed),
            (2, 2, 5, 1)
        );

        let page = |store: &MuxStore, view: &str| {
            store
                .list_threads(ThreadPageInput {
                    account_id: None,
                    view: Some(view.into()),
                    cursor: None,
                    limit: Some(100),
                    hidden_account_ids: Vec::new(),
                    container_id: None,
                })
                .unwrap()
                .threads
                .into_iter()
                .map(|thread| thread.id)
                .collect::<Vec<_>>()
        };
        assert!(!page(&store, "inbox").contains(&1));
        assert_eq!(page(&store, "snoozed"), vec![1]);
        // The fixture thread is out of the inbox, so it lands in archive too.
        assert_eq!(page(&store, "archive"), vec![2, DEMO_FIXTURE_THREAD_ID]);
        assert_eq!(page(&store, "all").len(), 5);

        let lookup = |store: &MuxStore, thread_id, view: &str| {
            store
                .get_thread_summary(ThreadLookupInput {
                    thread_id,
                    account_id: None,
                    view: Some(view.into()),
                    query: None,
                    timezone_offset_minutes: 0,
                })
                .unwrap()
                .is_some()
        };
        assert!(lookup(&store, 1, "all"));
        assert!(lookup(&store, 1, "snoozed"));
        assert!(!lookup(&store, 1, "inbox"));
        assert!(lookup(&store, 2, "archive"));

        let searched = store
            .search_threads(SearchInput {
                query: String::new(),
                account_id: None,
                view: Some("snoozed".into()),
                cursor: None,
                limit: Some(100),
                timezone_offset_minutes: 0,
                hidden_account_ids: Vec::new(),
            })
            .unwrap();
        assert_eq!(
            searched
                .rows
                .iter()
                .map(|thread| thread.id)
                .collect::<Vec<_>>(),
            vec![1]
        );
        for query in ["is:snoozed", "in:snoozed"] {
            let alias = store
                .search_threads(SearchInput {
                    query: query.into(),
                    account_id: None,
                    view: Some("all".into()),
                    cursor: None,
                    limit: Some(100),
                    timezone_offset_minutes: 0,
                    hidden_account_ids: Vec::new(),
                })
                .unwrap();
            assert_eq!(
                alias
                    .rows
                    .iter()
                    .map(|thread| thread.id)
                    .collect::<Vec<_>>(),
                vec![1]
            );
        }

        store
            .connection
            .execute("UPDATE snoozes SET wake_at = 0 WHERE thread_id = 1", [])
            .unwrap();
        assert!(page(&store, "snoozed").is_empty());
        assert!(page(&store, "inbox").contains(&1));
        let awakened = store.bootstrap().unwrap().view_counts.remove(0);
        assert_eq!(
            (
                awakened.inbox,
                awakened.archive,
                awakened.all,
                awakened.snoozed
            ),
            (3, 2, 5, 0)
        );
    }

    #[test]
    fn thread_detail_returns_the_bounded_invitation_dto() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("invitation-detail.db");
        let store = MuxStore::open(&path, true).expect("seeded store");
        let detail = store
            .get_thread_messages(MessagePageInput {
                thread_id: DEMO_FIXTURE_THREAD_ID,
                cursor: None,
                limit: Some(10),
            })
            .unwrap();
        let invitation = detail.invitation.expect("seeded invitation");
        assert_eq!(invitation.thread_id, DEMO_FIXTURE_THREAD_ID);
        assert_eq!(invitation.uid, "mux-native-demo-invite");
        assert_eq!(invitation.title, "Mux architecture review");
        assert_eq!(invitation.response, "needsAction");
        assert!(invitation.end_at > invitation.start_at);
        assert!(store
            .get_thread_messages(MessagePageInput {
                thread_id: 1,
                cursor: None,
                limit: Some(10),
            })
            .unwrap()
            .invitation
            .is_none());
    }

    #[test]
    fn local_snooze_and_rsvp_journals_are_undoable_but_reject_stale_undo() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("local-journal.db");
        let mut store = MuxStore::open(&path, true).expect("seeded store");
        let first_wake = now_ms() + 86_400_000;
        let second_wake = first_wake + 86_400_000;
        let first_snooze = store.snooze_thread(1, first_wake).unwrap();
        let second_snooze = store.snooze_thread(1, second_wake).unwrap();
        assert_eq!(first_snooze.state, "confirmed");
        assert!(matches!(
            store.undo_operation(&first_snooze.id),
            Err(StoreError::Conflict(_))
        ));
        assert_eq!(
            store.undo_operation(&second_snooze.id).unwrap().state,
            "cancelled"
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT wake_at FROM snoozes WHERE thread_id = 1",
                    [],
                    |row| { row.get::<_, i64>(0) }
                )
                .unwrap(),
            first_wake
        );
        store.undo_operation(&first_snooze.id).unwrap();
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM snoozes WHERE thread_id = 1",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );

        let first_rsvp = store
            .rsvp_thread(DEMO_FIXTURE_THREAD_ID, "accepted")
            .unwrap();
        let second_rsvp = store
            .rsvp_thread(DEMO_FIXTURE_THREAD_ID, "tentative")
            .unwrap();
        assert!(matches!(
            store.undo_operation(&first_rsvp.id),
            Err(StoreError::Conflict(_))
        ));
        store.undo_operation(&second_rsvp.id).unwrap();
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT response FROM invitations WHERE thread_id = 5",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "accepted"
        );
        store.undo_operation(&first_rsvp.id).unwrap();
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT response FROM invitations WHERE thread_id = 5",
                    [],
                    |row| row.get::<_, String>(0)
                )
                .unwrap(),
            "needsAction"
        );
    }

    #[test]
    fn activity_is_bounded_and_excludes_payloads_and_human_error_text() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("activity.db");
        let mut store = MuxStore::open(&path, true).expect("seeded store");
        store
            .rsvp_thread(DEMO_FIXTURE_THREAD_ID, "accepted")
            .unwrap();
        store
            .rsvp_thread(DEMO_FIXTURE_THREAD_ID, "tentative")
            .unwrap();
        let one = store
            .list_operations(ListOperationsInput { limit: Some(1) })
            .unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].field, "rsvp");
        let payload = serde_json::to_value(&one[0]).unwrap();
        assert!(payload.get("payloadJson").is_none());
        assert!(payload.get("error").is_none());
        for invalid in [0, MAX_ACTIVITY_ROWS + 1] {
            assert!(matches!(
                store.list_operations(ListOperationsInput {
                    limit: Some(invalid)
                }),
                Err(StoreError::Validation(_))
            ));
        }
    }

    #[test]
    fn resolving_an_uncertain_send_restores_and_unlocks_without_retrying() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("resolve-uncertain.db");
        let mut store = MuxStore::open(&path, true).expect("seeded store");
        let draft = store
            .save_draft(SaveDraftInput {
                id: None,
                account_id: "acc_work".into(),
                recipients: "jane@acme.example".into(),
                cc_recipients: String::new(),
                bcc_recipients: String::new(),
                subject: "Uncertain delivery".into(),
                body: "Preserve this exact draft".into(),
                body_html: "<p>Preserve this exact draft</p>".into(),
                reply_to_thread_id: Some(1),
                expected_revision: None,
            })
            .unwrap();
        let operation = store.queue_send(&draft.id, 5_000).unwrap();
        store
            .connection
            .execute(
                "UPDATE provider_work_items
                 SET state = 'outcome_unknown', completed_at = 100
                 WHERE operation_id = ?1",
                [&operation.id],
            )
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE operations SET state = 'outcome_unknown' WHERE id = ?1",
                [&operation.id],
            )
            .unwrap();
        store
            .connection
            .execute("DELETE FROM drafts WHERE id = ?1", [&draft.id])
            .unwrap();

        let resolved = store
            .resolve_outcome_unknown_send(&operation.id)
            .expect("explicit resolution succeeds");
        assert_eq!(resolved.state, "cancelled");
        let states = store
            .connection
            .query_row(
                "SELECT operation.state, work.state, work.cancel_requested,
                        work.last_error_code
                 FROM operations operation
                 JOIN provider_work_items work ON work.operation_id = operation.id
                 WHERE operation.id = ?1",
                [&operation.id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            states,
            (
                "cancelled".into(),
                "cancelled".into(),
                1,
                "uncertain_send_resolved".into()
            )
        );
        let restored = store.get_draft(&draft.id).unwrap().expect("draft restored");
        assert_eq!(restored.body, "Preserve this exact draft");
        assert_eq!(restored.body_html, "<p>Preserve this exact draft</p>");
        assert!(!restored.locked);

        let worker = DurableWorker::new(&path, WorkerConfig::default()).unwrap();
        assert!(worker
            .claim_available(now_ms() + 100_000, "resolution-check", 10)
            .unwrap()
            .is_empty());
        assert!(matches!(
            store.resolve_outcome_unknown_send(&operation.id),
            Err(StoreError::Conflict(_))
        ));
    }

    #[test]
    fn full_resync_rewinds_cursors_without_removing_anything() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("full-resync.db");
        let mut store = MuxStore::open(&path, false).unwrap();
        store
            .connection
            .execute(
                "INSERT INTO accounts(id, name, email, color, provider)
             VALUES('acct', 'acct', 'a@example.test', '#000', 'fake')",
                [],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO threads(id, account_id, subject, participants, snippet, latest_at,
               message_count, remote_in_inbox, remote_unread, remote_starred, has_attachment,
               has_invite, has_link, has_from_me)
             VALUES(7, 'acct', '', '', '', 1, 1, 1, 0, 0, 0, 0, 0, 0)",
                [],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO messages(id, thread_id, sender_name, sender_email, recipients,
               sent_at, body_text, is_from_me) VALUES(7, 7, '', 's@example.test', '', 1, '', 0)",
                [],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO snoozes(thread_id, wake_at, created_at) VALUES(7, 99, 1)",
                [],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO drafts(id, account_id, reply_to_thread_id, recipients, subject, body,
               body_html, updated_at, revision)
             VALUES('draft-1', 'acct', 7, '', '', '', '', 1, 1)",
                [],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO operations(id, thread_id, field, kind, state, created_at, not_before)
             VALUES('op-live', 7, 'unread', 'set', 'pending', 1, 1)",
                [],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO provider_accounts(account_id, provider_kind, remote_account_id,
               auth_state, created_at, updated_at)
             VALUES('acct', 'gmail', 'remote-acct', 'signed_out', 1, 1)",
                [],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO provider_sync_cursors(account_id, scope, cursor, updated_at)
             VALUES('acct', 'a:v1', '{\"phase\":\"history\"}', 1)",
                [],
            )
            .unwrap();

        let requested = store.request_full_resync().unwrap();
        assert_eq!(requested.accounts_reset, 1);

        let count = |sql: &str| -> i64 {
            store
                .connection
                .query_row(sql, [], |row| row.get(0))
                .unwrap()
        };
        // Only the sync bookmark is rewound, so the next cycle re-enumerates everything.
        assert_eq!(count("SELECT COUNT(*) FROM provider_sync_cursors"), 0);
        // Every layer of durable state is still exactly where it was.
        assert_eq!(count("SELECT COUNT(*) FROM threads"), 1);
        assert_eq!(count("SELECT COUNT(*) FROM messages"), 1);
        assert_eq!(count("SELECT COUNT(*) FROM snoozes"), 1);
        assert_eq!(count("SELECT COUNT(*) FROM drafts"), 1);
        assert_eq!(count("SELECT COUNT(*) FROM accounts"), 1);
        assert_eq!(
            count("SELECT COUNT(*) FROM operations WHERE state = 'pending' AND thread_id = 7"),
            1
        );
        assert_eq!(
            count("SELECT COUNT(*) FROM drafts WHERE reply_to_thread_id = 7"),
            1
        );
    }

    #[test]
    fn no_product_statement_can_empty_a_table_of_mail_intent_or_metadata() {
        // A resync must never be able to become a wipe. Bulk deletion of anything a
        // person owns is not vocabulary this app has; single-row deletes stay legal.
        let sources = [
            ("store.rs", include_str!("store.rs")),
            ("store/demo.rs", include_str!("store/demo.rs")),
            ("store/validation.rs", include_str!("store/validation.rs")),
            ("lib.rs", include_str!("lib.rs")),
            ("provider_ingest.rs", include_str!("provider_ingest.rs")),
            (
                "provider_conformance.rs",
                include_str!("provider_conformance.rs"),
            ),
            ("gmail.rs", include_str!("gmail.rs")),
            ("worker.rs", include_str!("worker.rs")),
        ];
        let owned = [
            "threads",
            "messages",
            "attachments",
            "drafts",
            "operations",
            "snoozes",
            "invitations",
            "accounts",
            "message_remote_images",
            "remote_content_sender_allowlist",
            "remote_content_domain_allowlist",
        ];
        for (name, source) in sources {
            // Tests build and tear down their own fixtures; only shipping code is bound.
            let product = source.split("#[cfg(test)]").next().unwrap_or(source);
            for statement in product.split("DELETE FROM ").skip(1) {
                let table = statement
                    .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .find(|value| !value.is_empty())
                    .unwrap_or_default();
                if !owned.contains(&table) {
                    continue;
                }
                let clause = statement
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .to_ascii_uppercase();
                assert!(
                    clause.contains("WHERE"),
                    "{name} can empty {table} without a WHERE clause"
                );
            }
        }
    }

    #[test]
    fn remote_content_policies_are_persistent_exact_and_account_scoped() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("remote-content-policy.db");
        let mut store = MuxStore::open(&path, false).unwrap();
        for (account, message_id, sender) in [
            ("account-a", 101_i64, "news@sender.example"),
            ("account-b", 201_i64, "news@sender.example"),
        ] {
            store.connection.execute(
                "INSERT INTO accounts(id, name, email, color, provider) VALUES(?1, ?1, ?2, '#000', 'fake')",
                params![account, format!("{account}@example.test")],
            ).unwrap();
            store
                .connection
                .execute(
                    "INSERT INTO threads(id, account_id, subject, participants, snippet, latest_at,
                  message_count, remote_in_inbox, remote_unread, remote_starred, has_attachment,
                  has_invite, has_link, has_from_me)
                 VALUES(?1, ?2, '', '', '', 1, 1, 1, 0, 0, 0, 0, 0, 0)",
                    params![message_id, account],
                )
                .unwrap();
            store
                .connection
                .execute(
                    "INSERT INTO messages(id, thread_id, sender_name, sender_email, recipients,
                  sent_at, body_text, is_from_me) VALUES(?1, ?1, '', ?2, '', 1, '', 0)",
                    params![message_id, sender],
                )
                .unwrap();
            store.connection.execute(
                "INSERT INTO message_remote_images(message_id, resource_id, url, domain, alt_text)
                 VALUES(?1, 1, 'https://images.sender.example/a.png', 'images.sender.example', 'A')",
                [message_id],
            ).unwrap();
        }

        store.allow_remote_content_sender(101).unwrap();
        let message_a = MessageSummary {
            id: 101,
            thread_id: 101,
            sender_name: String::new(),
            sender_email: "news@sender.example".into(),
            recipients: String::new(),
            cc_recipients: String::new(),
            bcc_recipients: String::new(),
            sent_at: 1,
            body_text: String::new(),
            body_html: String::new(),
            blocked_remote_resources: 1,
            remote_images: Vec::new(),
            is_from_me: false,
        };
        let mut message_b = message_a.clone();
        message_b.id = 201;
        message_b.thread_id = 201;
        let projected_images = store.remote_images_for_message(&message_a).unwrap();
        assert!(projected_images[0].allowed_by_policy);
        let mut projected_message = message_a.clone();
        projected_message.remote_images = projected_images;
        let message_json = serde_json::to_string(&projected_message).unwrap();
        assert!(message_json.contains("images.sender.example"));
        assert!(!message_json.contains("a.png"));
        assert!(!message_json.contains("https://"));
        assert!(!store.remote_images_for_message(&message_b).unwrap()[0].allowed_by_policy);

        store
            .allow_remote_content_domain(201, "images.sender.example")
            .unwrap();
        assert!(store.remote_images_for_message(&message_b).unwrap()[0].allowed_by_policy);
        assert!(matches!(
            store.allow_remote_content_domain(201, "sender.example"),
            Err(StoreError::NotFound(_))
        ));
        drop(store);
        let reopened = MuxStore::open(&path, false).unwrap();
        assert!(reopened.remote_images_for_message(&message_a).unwrap()[0].allowed_by_policy);
        assert!(reopened.remote_images_for_message(&message_b).unwrap()[0].allowed_by_policy);
    }

    #[test]
    fn account_folders_are_listed_with_counts_and_can_be_opened_on_their_own() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("folders.db");
        let mut store = configured_provider_store(&path, &["acc-folders"]);
        let batch: ProviderBatch = serde_json::from_value(serde_json::json!({
            "muxAccountId": "acc-folders",
            "batchId": "folder-batch",
            "expectedPriorCursor": null,
            "cursor": {
                "muxAccountId": "acc-folders",
                "scope": { "kind": "account" },
                "value": "folder-cursor"
            },
            "observedAt": 1000,
            "threadUpserts": [{
                "identity": {
                    "muxAccountId": "acc-folders",
                    "remoteThreadId": "thread-in-label"
                },
                "subject": "Filed away",
                "participants": "sender@example.com",
                "snippet": "Filed under a label",
                "latestAt": 900,
                "messageCount": 1,
                "inInbox": true,
                "unread": true,
                "starred": false,
                "hasAttachments": false,
                "hasInvite": false,
                "hasLinks": false,
                "hasFromMe": false,
                "category": "primary",
                "revision": "thread-r1"
            }, {
                "identity": {
                    "muxAccountId": "acc-folders",
                    "remoteThreadId": "thread-unfiled"
                },
                "subject": "Not filed",
                "participants": "other@example.com",
                "snippet": "No label at all",
                "latestAt": 800,
                "messageCount": 1,
                "inInbox": true,
                "unread": false,
                "starred": false,
                "hasAttachments": false,
                "hasInvite": false,
                "hasLinks": false,
                "hasFromMe": false,
                "category": "primary",
                "revision": "thread-r2"
            }],
            "messageUpserts": [{
                "identity": {
                    "muxAccountId": "acc-folders",
                    "remoteMessageId": "message-in-label",
                    "remoteThreadId": "thread-in-label"
                },
                "subject": "Filed away",
                "senderName": "Sender",
                "senderEmail": "sender@example.com",
                "recipients": "recipient@example.com",
                "ccRecipients": "",
                "bccRecipients": "",
                "sentAt": 900,
                "bodyText": "Filed under a label.",
                "bodyState": "complete",
                "isFromMe": false,
                "revision": "message-r1",
                "keywords": ["unread"]
            }, {
                "identity": {
                    "muxAccountId": "acc-folders",
                    "remoteMessageId": "message-unfiled",
                    "remoteThreadId": "thread-unfiled"
                },
                "subject": "Not filed",
                "senderName": "Other",
                "senderEmail": "other@example.com",
                "recipients": "recipient@example.com",
                "ccRecipients": "",
                "bccRecipients": "",
                "sentAt": 800,
                "bodyText": "No label at all.",
                "bodyState": "complete",
                "isFromMe": false,
                "revision": "message-r2",
                "keywords": []
            }],
            "containerUpserts": [{
                "identity": {
                    "muxAccountId": "acc-folders",
                    "remoteContainerId": "inbox"
                },
                "displayName": "Inbox",
                "kind": "mailbox",
                "role": "inbox",
                "parentRemoteContainerId": null,
                "selectable": true
            }, {
                "identity": {
                    "muxAccountId": "acc-folders",
                    "remoteContainerId": "Label_17"
                },
                "displayName": "Zoomie Cycle",
                "kind": "label",
                // No role at all is what a label of the account's own making
                // looks like on the wire; the store records it as "custom".
                "role": null,
                "parentRemoteContainerId": null,
                "selectable": true
            }, {
                "identity": {
                    "muxAccountId": "acc-folders",
                    "remoteContainerId": "Label_hidden"
                },
                "displayName": "Not selectable",
                "kind": "label",
                "role": null,
                "parentRemoteContainerId": null,
                "selectable": false
            }],
            "membershipChanges": [{
                "kind": "upsert",
                "membership": {
                    "message": {
                        "muxAccountId": "acc-folders",
                        "remoteMessageId": "message-in-label",
                        "remoteThreadId": "thread-in-label"
                    },
                    "container": {
                        "muxAccountId": "acc-folders",
                        "remoteContainerId": "Label_17"
                    }
                }
            }, {
                "kind": "upsert",
                "membership": {
                    "message": {
                        "muxAccountId": "acc-folders",
                        "remoteMessageId": "message-unfiled",
                        "remoteThreadId": "thread-unfiled"
                    },
                    "container": {
                        "muxAccountId": "acc-folders",
                        "remoteContainerId": "inbox"
                    }
                }
            }],
            "tombstones": []
        }))
        .expect("valid provider batch");
        assert!(
            store
                .apply_provider_batch(batch)
                .expect("batch applies")
                .applied
        );

        // Only the account's own folders: the roles the fixed views stand for
        // are the sidebar's existing rows, and an unselectable label is not a
        // place anyone can go.
        let containers = store.list_containers().expect("containers list");
        assert_eq!(
            containers
                .iter()
                .map(|container| (container.remote_id.as_str(), container.name.as_str()))
                .collect::<Vec<_>>(),
            [("Label_17", "Zoomie Cycle")]
        );
        assert_eq!((containers[0].total, containers[0].unread), (1, 1));
        assert_eq!(containers[0].kind, "label");
        assert_eq!(containers[0].role, "custom");

        // Opening the folder lists exactly what is filed in it.
        let filed = store
            .list_threads(ThreadPageInput {
                account_id: Some("acc-folders".into()),
                view: Some("all".into()),
                cursor: None,
                limit: Some(50),
                hidden_account_ids: Vec::new(),
                container_id: Some("Label_17".into()),
            })
            .expect("folder page");
        assert_eq!(
            filed
                .threads
                .iter()
                .map(|thread| thread.subject.as_str())
                .collect::<Vec<_>>(),
            ["Filed away"]
        );

        // And a folder nothing is filed in is empty rather than everything.
        let empty = store
            .list_threads(ThreadPageInput {
                account_id: Some("acc-folders".into()),
                view: Some("all".into()),
                cursor: None,
                limit: Some(50),
                hidden_account_ids: Vec::new(),
                container_id: Some("Label_hidden".into()),
            })
            .expect("empty folder page");
        assert!(empty.threads.is_empty());

        // A folder belongs to one account, so it cannot be listed across all.
        let unscoped = store.list_threads(ThreadPageInput {
            account_id: None,
            view: Some("all".into()),
            cursor: None,
            limit: Some(50),
            hidden_account_ids: Vec::new(),
            container_id: Some("Label_17".into()),
        });
        assert!(matches!(unscoped, Err(StoreError::Validation(_))));
    }
}
