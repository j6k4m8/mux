//! Schema creation, migration, and the recovery a database needs before it can
//! be trusted.
//!
//! Separated from the store's read and write paths because this runs once at
//! open: it is the only code allowed to reshape tables, and keeping it apart
//! makes it obvious that nothing else does.

use sha2::{Digest, Sha256};

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

use super::{
    durable_work_error, new_id, now_ms, operation_from_row, resolve_send_provider_target,
    send_content_fingerprint_v2, send_payload_for_draft, send_payload_has_complete_snapshot,
    send_projection_fields, DraftSummary, SendPayload, StoreError, SCHEMA_VERSION,
};
use crate::worker::{enqueue_in_transaction, NewWorkItem, WorkKind};

pub(super) fn migrate(connection: &mut Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA temp_store = MEMORY;
         PRAGMA busy_timeout = 5000;

         CREATE TABLE IF NOT EXISTS meta (
           key TEXT PRIMARY KEY,
           value TEXT NOT NULL
         );",
    )?;

    let stored_value: Option<String> = connection
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let stored_version = match stored_value.as_deref() {
        None => 0,
        Some(value) => value
            .parse::<i64>()
            .ok()
            .filter(|version| *version >= 0)
            .ok_or_else(|| StoreError::InvalidSchemaVersion(value.to_string()))?,
    };
    if stored_version > SCHEMA_VERSION {
        return Err(StoreError::FutureSchema {
            found: stored_version,
            supported: SCHEMA_VERSION,
        });
    }

    let rebuild_provider_accounts =
        crate::provider_schema::provider_accounts_need_rebuild(connection, stored_version)?;
    let rebuild_provider_containers =
        crate::provider_schema::provider_containers_need_rebuild(connection, stored_version)?;
    if rebuild_provider_accounts || rebuild_provider_containers {
        connection.execute_batch("PRAGMA foreign_keys = OFF;")?;
    }

    let migration_result = (|| -> Result<(), StoreError> {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS accounts (
           id TEXT PRIMARY KEY,
           name TEXT NOT NULL,
           email TEXT NOT NULL,
           color TEXT NOT NULL,
           provider TEXT NOT NULL,
           signature TEXT NOT NULL DEFAULT ''
         );

         CREATE TABLE IF NOT EXISTS threads (
           id INTEGER PRIMARY KEY,
           account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
           subject TEXT NOT NULL,
           participants TEXT NOT NULL,
           snippet TEXT NOT NULL,
           latest_at INTEGER NOT NULL,
           message_count INTEGER NOT NULL,
           remote_in_inbox INTEGER NOT NULL CHECK (remote_in_inbox IN (0, 1)),
           remote_unread INTEGER NOT NULL CHECK (remote_unread IN (0, 1)),
           remote_starred INTEGER NOT NULL CHECK (remote_starred IN (0, 1)),
           has_attachment INTEGER NOT NULL CHECK (has_attachment IN (0, 1)),
           has_invite INTEGER NOT NULL CHECK (has_invite IN (0, 1)),
           has_link INTEGER NOT NULL CHECK (has_link IN (0, 1)),
           has_from_me INTEGER NOT NULL CHECK (has_from_me IN (0, 1)),
           category TEXT NOT NULL DEFAULT '',
           attachment_names TEXT NOT NULL DEFAULT '',
           remote_deleted INTEGER NOT NULL DEFAULT 0 CHECK(remote_deleted IN (0, 1))
         );

         CREATE TABLE IF NOT EXISTS messages (
           id INTEGER PRIMARY KEY,
           thread_id INTEGER NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
           sender_name TEXT NOT NULL,
           sender_email TEXT NOT NULL,
           recipients TEXT NOT NULL,
           cc_recipients TEXT NOT NULL DEFAULT '',
           bcc_recipients TEXT NOT NULL DEFAULT '',
           sent_at INTEGER NOT NULL,
           body_text TEXT NOT NULL,
           body_html TEXT NOT NULL DEFAULT '',
           blocked_remote_resources INTEGER NOT NULL DEFAULT 0 CHECK (blocked_remote_resources >= 0),
           is_from_me INTEGER NOT NULL CHECK (is_from_me IN (0, 1)),
           remote_deleted INTEGER NOT NULL DEFAULT 0 CHECK(remote_deleted IN (0, 1)),
           internet_message_id TEXT NOT NULL DEFAULT '',
           in_reply_to TEXT NOT NULL DEFAULT '',
           references_json TEXT NOT NULL DEFAULT '[]' CHECK(json_valid(references_json)),
           provider_subject TEXT NOT NULL DEFAULT ''
         );

         CREATE TABLE IF NOT EXISTS attachments (
           id TEXT PRIMARY KEY,
           message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
           filename TEXT NOT NULL,
           media_type TEXT NOT NULL,
           byte_length INTEGER NOT NULL CHECK (byte_length >= 0),
           content BLOB NOT NULL,
           content_id TEXT NOT NULL DEFAULT '',
           disposition TEXT NOT NULL CHECK (disposition IN ('inline', 'attachment'))
         );

         CREATE TABLE IF NOT EXISTS message_remote_images (
           message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
           resource_id INTEGER NOT NULL CHECK(resource_id > 0 AND resource_id <= 64),
           url TEXT NOT NULL CHECK(length(url) <= 2048),
           domain TEXT NOT NULL CHECK(length(domain) BETWEEN 1 AND 253),
           alt_text TEXT NOT NULL,
           PRIMARY KEY(message_id, resource_id)
         );

         CREATE TABLE IF NOT EXISTS remote_content_sender_allowlist (
           account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
           sender_email TEXT NOT NULL,
           PRIMARY KEY(account_id, sender_email)
         );

         CREATE TABLE IF NOT EXISTS remote_content_domain_allowlist (
           account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
           domain TEXT NOT NULL,
           PRIMARY KEY(account_id, domain)
         );

         CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
           thread_id UNINDEXED,
           subject,
           participants,
           body,
           attachment_names,
           tokenize = 'unicode61 remove_diacritics 2'
         );

         CREATE TABLE IF NOT EXISTS operations (
           id TEXT PRIMARY KEY,
           thread_id INTEGER REFERENCES threads(id) ON DELETE SET NULL,
           field TEXT NOT NULL,
           kind TEXT NOT NULL,
           old_value TEXT,
           new_value TEXT,
           payload_json TEXT,
           state TEXT NOT NULL CHECK (state IN (
             'pending', 'executing', 'confirmed', 'retrying', 'conflicted',
             'failed', 'cancelled', 'outcome_unknown'
           )),
           created_at INTEGER NOT NULL,
           not_before INTEGER NOT NULL,
           confirmed_at INTEGER,
           undo_of TEXT REFERENCES operations(id),
           error TEXT,
           attempts INTEGER NOT NULL DEFAULT 0
         );

         CREATE TABLE IF NOT EXISTS snoozes (
           thread_id INTEGER PRIMARY KEY REFERENCES threads(id) ON DELETE CASCADE,
           wake_at INTEGER NOT NULL,
           created_at INTEGER NOT NULL,
           previous_location TEXT NOT NULL DEFAULT 'inbox'
         );

         CREATE TABLE IF NOT EXISTS invitations (
           thread_id INTEGER PRIMARY KEY REFERENCES threads(id) ON DELETE CASCADE,
           uid TEXT NOT NULL,
           title TEXT NOT NULL,
           start_at INTEGER NOT NULL,
           end_at INTEGER NOT NULL,
           timezone TEXT NOT NULL,
           location TEXT NOT NULL,
           organizer TEXT NOT NULL,
           attendees TEXT NOT NULL,
           response TEXT NOT NULL CHECK (response IN ('needsAction', 'accepted', 'tentative', 'declined')),
           conflict_text TEXT
         );

         CREATE TABLE IF NOT EXISTS drafts (
           id TEXT PRIMARY KEY,
           account_id TEXT NOT NULL REFERENCES accounts(id),
           recipients TEXT NOT NULL,
           cc_recipients TEXT NOT NULL DEFAULT '',
           bcc_recipients TEXT NOT NULL DEFAULT '',
           subject TEXT NOT NULL,
           body TEXT NOT NULL,
           body_html TEXT NOT NULL DEFAULT '',
           reply_to_thread_id INTEGER REFERENCES threads(id) ON DELETE SET NULL,
           updated_at INTEGER NOT NULL,
           revision INTEGER NOT NULL DEFAULT 1
         );

         CREATE INDEX IF NOT EXISTS threads_latest_idx ON threads(latest_at DESC, id DESC);
         CREATE INDEX IF NOT EXISTS threads_account_latest_idx ON threads(account_id, latest_at DESC, id DESC);
         CREATE INDEX IF NOT EXISTS messages_thread_idx ON messages(thread_id, sent_at, id);
         CREATE INDEX IF NOT EXISTS attachments_message_idx ON attachments(message_id, id);
         CREATE INDEX IF NOT EXISTS message_remote_images_domain_idx
           ON message_remote_images(message_id, domain);
         CREATE INDEX IF NOT EXISTS operations_due_idx ON operations(state, not_before, created_at);
         CREATE INDEX IF NOT EXISTS operations_overlay_idx ON operations(thread_id, field, state, created_at DESC);
         CREATE INDEX IF NOT EXISTS snoozes_wake_idx ON snoozes(wake_at);

         DROP VIEW IF EXISTS thread_effective;
         CREATE VIEW thread_effective AS
         SELECT
           t.*,
           COALESCE((
             SELECT CAST(o.new_value AS INTEGER)
             FROM operations o
             WHERE o.thread_id = t.id
               AND o.field = 'in_inbox'
               AND o.state IN ('pending', 'executing', 'retrying')
             ORDER BY o.created_at DESC, o.rowid DESC
             LIMIT 1
           ), t.remote_in_inbox) AS in_inbox,
           COALESCE((
             SELECT CAST(o.new_value AS INTEGER)
             FROM operations o
             WHERE o.thread_id = t.id
               AND o.field = 'unread'
               AND o.state IN ('pending', 'executing', 'retrying')
             ORDER BY o.created_at DESC, o.rowid DESC
             LIMIT 1
           ), t.remote_unread) AS unread,
           COALESCE((
             SELECT CAST(o.new_value AS INTEGER)
             FROM operations o
             WHERE o.thread_id = t.id
               AND o.field = 'starred'
               AND o.state IN ('pending', 'executing', 'retrying')
             ORDER BY o.created_at DESC, o.rowid DESC
             LIMIT 1
           ), t.remote_starred) AS starred
         FROM threads t;",
        )?;

        if !column_exists(&transaction, "operations", "attempts")? {
            transaction.execute_batch(
                "ALTER TABLE operations ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
        if !column_exists(&transaction, "drafts", "revision")? {
            transaction.execute_batch(
                "ALTER TABLE drafts ADD COLUMN revision INTEGER NOT NULL DEFAULT 1;",
            )?;
        }
        if !column_exists(&transaction, "drafts", "body_html")? {
            transaction.execute_batch(
                "ALTER TABLE drafts ADD COLUMN body_html TEXT NOT NULL DEFAULT '';",
            )?;
        }
        if !column_exists(&transaction, "messages", "body_html")? {
            transaction.execute_batch(
                "ALTER TABLE messages ADD COLUMN body_html TEXT NOT NULL DEFAULT '';",
            )?;
        }
        if !column_exists(&transaction, "accounts", "signature")? {
            transaction.execute_batch(
                "ALTER TABLE accounts ADD COLUMN signature TEXT NOT NULL DEFAULT '';",
            )?;
        }
        if !column_exists(&transaction, "drafts", "cc_recipients")? {
            transaction.execute_batch(
                "ALTER TABLE drafts ADD COLUMN cc_recipients TEXT NOT NULL DEFAULT '';
             ALTER TABLE drafts ADD COLUMN bcc_recipients TEXT NOT NULL DEFAULT '';",
            )?;
        }
        if !column_exists(&transaction, "messages", "cc_recipients")? {
            transaction.execute_batch(
                "ALTER TABLE messages ADD COLUMN cc_recipients TEXT NOT NULL DEFAULT '';
             ALTER TABLE messages ADD COLUMN bcc_recipients TEXT NOT NULL DEFAULT '';",
            )?;
        }
        if !column_exists(&transaction, "messages", "blocked_remote_resources")? {
            transaction.execute_batch(
            "ALTER TABLE messages ADD COLUMN blocked_remote_resources INTEGER NOT NULL DEFAULT 0 CHECK (blocked_remote_resources >= 0);",
        )?;
        }
        if !column_exists(&transaction, "threads", "remote_deleted")? {
            transaction.execute_batch(
                "ALTER TABLE threads ADD COLUMN remote_deleted INTEGER NOT NULL DEFAULT 0
               CHECK(remote_deleted IN (0, 1));",
            )?;
        }
        if !column_exists(&transaction, "threads", "remote_trashed")? {
            transaction.execute_batch(
                "ALTER TABLE threads ADD COLUMN remote_trashed INTEGER NOT NULL DEFAULT 0
               CHECK(remote_trashed IN (0, 1));",
            )?;
        }
        if !column_exists(&transaction, "messages", "remote_deleted")? {
            transaction.execute_batch(
                "ALTER TABLE messages ADD COLUMN remote_deleted INTEGER NOT NULL DEFAULT 0
               CHECK(remote_deleted IN (0, 1));",
            )?;
        }
        if !column_exists(&transaction, "messages", "internet_message_id")? {
            transaction.execute_batch(
                "ALTER TABLE messages ADD COLUMN internet_message_id TEXT NOT NULL DEFAULT '';",
            )?;
        }
        if !column_exists(&transaction, "messages", "in_reply_to")? {
            transaction.execute_batch(
                "ALTER TABLE messages ADD COLUMN in_reply_to TEXT NOT NULL DEFAULT '';",
            )?;
        }
        if !column_exists(&transaction, "messages", "references_json")? {
            transaction.execute_batch(
                "ALTER TABLE messages ADD COLUMN references_json TEXT NOT NULL DEFAULT '[]'
                   CHECK(json_valid(references_json));",
            )?;
        }
        if !column_exists(&transaction, "messages", "provider_subject")? {
            transaction.execute_batch(
                "ALTER TABLE messages ADD COLUMN provider_subject TEXT NOT NULL DEFAULT '';",
            )?;
        }

        transaction.execute_batch(
            "DROP VIEW IF EXISTS thread_effective;
             CREATE VIEW thread_effective AS
             SELECT
               t.*,
               COALESCE((
                 SELECT CAST(o.new_value AS INTEGER)
                 FROM operations o
                 WHERE o.thread_id = t.id
                   AND o.field = 'in_inbox'
                   AND o.state IN ('pending', 'executing', 'retrying')
                 ORDER BY o.created_at DESC, o.rowid DESC
                 LIMIT 1
               ), t.remote_in_inbox) AS in_inbox,
               COALESCE((
                 SELECT CAST(o.new_value AS INTEGER)
                 FROM operations o
                 WHERE o.thread_id = t.id
                   AND o.field = 'unread'
                   AND o.state IN ('pending', 'executing', 'retrying')
                 ORDER BY o.created_at DESC, o.rowid DESC
                 LIMIT 1
               ), t.remote_unread) AS unread,
               COALESCE((
                 SELECT CAST(o.new_value AS INTEGER)
                 FROM operations o
                 WHERE o.thread_id = t.id
                   AND o.field = 'starred'
                   AND o.state IN ('pending', 'executing', 'retrying')
                 ORDER BY o.created_at DESC, o.rowid DESC
                 LIMIT 1
               ), t.remote_starred) AS starred,
               COALESCE((
                 SELECT CAST(o.new_value AS INTEGER)
                 FROM operations o
                 WHERE o.thread_id = t.id
                   AND o.field = 'trashed'
                   AND o.state IN ('pending', 'executing', 'retrying')
                 ORDER BY o.created_at DESC, o.rowid DESC
                 LIMIT 1
               ), t.remote_trashed) AS trashed
             FROM threads t;",
        )?;

        crate::provider_schema::migrate(
            &transaction,
            stored_version,
            rebuild_provider_accounts,
            rebuild_provider_containers,
        )?;
        install_provider_effective_views(&transaction)?;

        if stored_version < 13 {
            crate::provider_ingest::rebuild_all_search_indexes(&transaction)?;
        }

        if rebuild_provider_accounts || rebuild_provider_containers {
            let violations = transaction
                .prepare("PRAGMA foreign_key_check")?
                .query_map([], |_| Ok(()))?
                .count();
            if violations != 0 {
                return Err(StoreError::Validation(format!(
                    "Provider account migration left {violations} foreign key violation(s)"
                )));
            }
        }

        transaction.execute(
            "INSERT INTO meta(key, value) VALUES('schema_version', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [SCHEMA_VERSION.to_string()],
        )?;
        transaction.commit()?;
        Ok(())
    })();

    if rebuild_provider_accounts || rebuild_provider_containers {
        connection.execute_batch("PRAGMA foreign_keys = ON;")?;
    }
    migration_result
}

fn install_provider_effective_views(transaction: &Transaction<'_>) -> Result<(), StoreError> {
    transaction.execute_batch(
        "DROP VIEW IF EXISTS provider_thread_label_effective;
         CREATE VIEW provider_thread_label_effective AS
         WITH confirmed AS (
           SELECT thread_ref.account_id,
                  thread_ref.thread_id,
                  thread_ref.remote_thread_id,
                  container.remote_id AS remote_container_id,
                  COUNT(message_ref.remote_message_id) AS remote_message_count,
                  COUNT(membership.remote_message_id) AS labelled_message_count
           FROM provider_thread_refs thread_ref
           JOIN provider_containers container
             ON container.account_id = thread_ref.account_id
            AND container.role = 'custom'
            AND container.is_selectable = 1
            AND container.is_deleted = 0
           LEFT JOIN provider_message_refs message_ref
             ON message_ref.account_id = thread_ref.account_id
            AND message_ref.remote_thread_id = thread_ref.remote_thread_id
           LEFT JOIN provider_container_memberships membership
             ON membership.account_id = message_ref.account_id
            AND membership.remote_message_id = message_ref.remote_message_id
            AND membership.remote_container_id = container.remote_id
           GROUP BY thread_ref.account_id, thread_ref.thread_id,
                    thread_ref.remote_thread_id, container.remote_id
         )
         SELECT confirmed.account_id,
                confirmed.thread_id,
                confirmed.remote_thread_id,
                confirmed.remote_container_id,
                COALESCE((
                  SELECT CAST(operation.new_value AS INTEGER)
                  FROM operations operation
                  WHERE operation.thread_id = confirmed.thread_id
                    AND operation.field = 'provider_label'
                    AND operation.state IN ('pending', 'executing', 'retrying')
                    AND json_valid(operation.payload_json)
                    AND json_extract(operation.payload_json, '$.accountId') = confirmed.account_id
                    AND json_extract(operation.payload_json, '$.remoteContainerId') = confirmed.remote_container_id
                  ORDER BY operation.created_at DESC, operation.rowid DESC
                  LIMIT 1
                ), CASE
                  WHEN confirmed.remote_message_count > 0
                   AND confirmed.labelled_message_count = confirmed.remote_message_count THEN 1
                  WHEN confirmed.labelled_message_count = 0 THEN 0
                  ELSE 2
                END) AS label_state
         FROM confirmed;",
    )?;
    Ok(())
}

fn column_exists(
    transaction: &Transaction<'_>,
    table: &str,
    column: &str,
) -> Result<bool, rusqlite::Error> {
    let mut statement = transaction.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(columns.iter().any(|name| name == column))
}

pub(super) fn recover_interrupted_operations(connection: &Connection) -> Result<(), StoreError> {
    let interrupted_at = now_ms();
    connection.execute(
        "UPDATE operations
         SET state = 'outcome_unknown',
             error = COALESCE(error, 'Mux restarted while send outcome was unresolved')
         WHERE state = 'executing' AND field = 'send'
           AND (
             NOT EXISTS (
               SELECT 1 FROM provider_work_items work
               WHERE work.operation_id = operations.id AND work.kind = 'send'
             ) OR EXISTS (
               SELECT 1 FROM provider_work_items work
               WHERE work.operation_id = operations.id AND work.kind = 'send'
                 AND work.last_error_code = 'send_submission_started'
             )
           )",
        [],
    )?;
    connection.execute(
        "UPDATE operations
         SET state = 'retrying', not_before = ?1,
             error = COALESCE(error, 'Mux restarted before provider submission began')
         WHERE state = 'executing' AND field = 'send'
           AND EXISTS (
             SELECT 1 FROM provider_work_items work
             WHERE work.operation_id = operations.id AND work.kind = 'send'
               AND work.last_error_code IS NOT 'send_submission_started'
           )",
        [interrupted_at],
    )?;
    connection.execute(
        "UPDATE operations
         SET state = 'retrying', not_before = ?1,
             error = COALESCE(error, 'Mux restarted while provider acknowledgement was pending')
         WHERE state = 'executing' AND field <> 'send'",
        [interrupted_at],
    )?;
    Ok(())
}

pub(super) fn ensure_durable_work_for_pending_operations(
    connection: &mut Connection,
) -> Result<(), StoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    upgrade_legacy_send_work(&transaction)?;
    let operations = {
        let mut statement = transaction.prepare(
            "SELECT operation.rowid, operation.id, operation.thread_id, operation.field,
                    operation.kind, operation.old_value, operation.new_value,
                    operation.payload_json, operation.state, operation.not_before,
                    operation.created_at
             FROM operations operation
             WHERE operation.state IN ('pending', 'retrying')
               AND NOT EXISTS (
                 SELECT 1 FROM provider_work_items work
                 WHERE work.operation_id = operation.id
               )
             ORDER BY operation.created_at, operation.rowid",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((operation_from_row(row)?, row.get::<_, i64>(10)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };

    for (operation, created_at) in operations {
        let work_id = format!("work_migrated_{:020}", operation.row_id);
        if operation.field == "send" {
            let old_payload: SendPayload = serde_json::from_str(
                operation
                    .payload_json
                    .as_deref()
                    .ok_or_else(|| StoreError::Validation("Send payload is missing".into()))?,
            )?;
            let draft = transaction
                .query_row(
                    "SELECT d.id, d.account_id, a.name, a.email, a.color,
                            d.recipients, d.cc_recipients, d.bcc_recipients,
                            d.subject, d.body, d.body_html, d.reply_to_thread_id,
                            d.updated_at, d.revision
                     FROM drafts d JOIN accounts a ON a.id = d.account_id
                     WHERE d.id = ?1",
                    [&old_payload.draft_id],
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
                            locked: true,
                        })
                    },
                )
                .optional()?;
            let Some(draft) = draft else {
                transaction.execute(
                    "UPDATE operations
                     SET state = 'failed', error = 'Draft was unavailable during worker migration'
                     WHERE id = ?1 AND state IN ('pending', 'retrying')",
                    [&operation.id],
                )?;
                continue;
            };
            let (resolved_provider_kind, resolved_remote_thread_id) =
                resolve_send_provider_target(&transaction, &draft)?;
            let payload = if send_payload_has_complete_snapshot(&old_payload) {
                old_payload
            } else {
                let message_id = if old_payload.message_id.is_empty() {
                    new_id(&transaction, "message")?
                } else {
                    old_payload.message_id
                };
                send_payload_for_draft(
                    &draft,
                    message_id,
                    format!("<{}@mux.invalid>", operation.id.replace('_', "-")),
                    created_at,
                    None,
                    Vec::new(),
                    format!("mux-{}", operation.id.replace('_', "-")),
                    resolved_provider_kind.clone(),
                    resolved_remote_thread_id,
                )
            };
            let payload_json = serde_json::to_string(&payload)?;
            transaction.execute(
                "UPDATE operations SET payload_json = ?2 WHERE id = ?1",
                params![operation.id, payload_json],
            )?;
            let send_scope = if payload.provider_kind.as_deref() == Some("gmail") {
                crate::gmail::gmail_send_scope(&operation.id)
            } else {
                "outgoing:v1".into()
            };
            enqueue_in_transaction(
                &transaction,
                NewWorkItem {
                    id: work_id.clone(),
                    account_id: draft.account_id.clone(),
                    operation_id: Some(operation.id.clone()),
                    kind: WorkKind::Send,
                    scope: send_scope.clone(),
                    ordering_key: payload.submission_message_id.clone().ok_or_else(|| {
                        StoreError::Validation("Migrated send has no submission identity".into())
                    })?,
                    payload_json: payload_json.clone(),
                    priority: 100,
                    available_at: operation.not_before,
                    max_attempts: 8,
                },
                created_at,
            )
            .map_err(durable_work_error)?;
            if payload.provider_kind.as_deref() == Some("gmail") {
                enqueue_in_transaction(
                    &transaction,
                    crate::gmail::gmail_send_reconciliation_work(
                        &draft.account_id,
                        &operation.id,
                        &work_id,
                        &send_scope,
                        &payload_json,
                        operation.not_before.saturating_add(1_000),
                    )
                    .map_err(StoreError::Validation)?,
                    created_at.saturating_add(1),
                )
                .map_err(durable_work_error)?;
            }
        } else {
            let Some(thread_id) = operation.thread_id else {
                transaction.execute(
                    "UPDATE operations
                     SET state = 'failed', error = 'Thread was unavailable during worker migration'
                     WHERE id = ?1 AND state IN ('pending', 'retrying')",
                    [&operation.id],
                )?;
                continue;
            };
            let account_id = transaction
                .query_row(
                    "SELECT account_id FROM threads WHERE id = ?1",
                    [thread_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            let Some(account_id) = account_id else {
                transaction.execute(
                    "UPDATE operations
                     SET state = 'failed', error = 'Thread was unavailable during worker migration'
                     WHERE id = ?1 AND state IN ('pending', 'retrying')",
                    [&operation.id],
                )?;
                continue;
            };
            let payload_json = serde_json::to_string(&serde_json::json!({
                "operationId": operation.id.clone(),
                "threadId": thread_id,
                "field": operation.field.clone(),
                "value": operation.new_value.clone(),
            }))?;
            enqueue_in_transaction(
                &transaction,
                NewWorkItem {
                    id: work_id,
                    account_id,
                    operation_id: Some(operation.id.clone()),
                    kind: WorkKind::Mutation,
                    scope: format!("thread:v1:{thread_id}"),
                    ordering_key: operation.id,
                    payload_json,
                    priority: 50,
                    available_at: operation.not_before,
                    max_attempts: 8,
                },
                created_at,
            )
            .map_err(durable_work_error)?;
        }
    }
    transaction.commit()?;
    Ok(())
}

fn upgrade_legacy_send_work(transaction: &Transaction<'_>) -> Result<(), StoreError> {
    let rows = {
        let mut statement = transaction.prepare(
            "SELECT work.id, work.account_id, work.operation_id, work.scope,
                    work.ordering_key, work.payload_json, work.state, work.priority,
                    work.created_at, work.available_at, work.attempt_count,
                    work.max_attempts, work.cancel_requested, work.last_error_code,
                    work.auth_block_reason, work.retry_after_at,
                    operation.payload_json, operation.created_at
             FROM provider_work_items work
             JOIN operations operation ON operation.id = work.operation_id
             WHERE work.kind = 'send'
               AND work.state IN (
                 'queued', 'retry_wait', 'rate_limited', 'authentication_blocked'
               )
               AND operation.state IN ('pending', 'retrying')
             ORDER BY operation.created_at, work.id",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok(LegacySendWork {
                    id: row.get(0)?,
                    account_id: row.get(1)?,
                    operation_id: row.get(2)?,
                    scope: row.get(3)?,
                    ordering_key: row.get(4)?,
                    payload_json: row.get(5)?,
                    state: row.get(6)?,
                    priority: row.get(7)?,
                    created_at: row.get(8)?,
                    available_at: row.get(9)?,
                    attempt_count: row.get(10)?,
                    max_attempts: row.get(11)?,
                    cancel_requested: row.get(12)?,
                    last_error_code: row.get(13)?,
                    auth_block_reason: row.get(14)?,
                    retry_after_at: row.get(15)?,
                    operation_payload_json: row.get(16)?,
                    operation_created_at: row.get(17)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    for work in rows {
        let operation_payload_json = work.operation_payload_json.ok_or_else(|| {
            StoreError::Validation("Queued send operation payload is missing".into())
        })?;
        if work.payload_json != operation_payload_json {
            return Err(StoreError::Validation(
                "Queued send operation and work snapshots diverged".into(),
            ));
        }
        let mut payload: SendPayload = serde_json::from_str(&work.payload_json)?;
        if payload.snapshot_version == Some(2) {
            continue;
        }
        if !send_payload_has_complete_snapshot(&payload) {
            return Err(StoreError::Validation(
                "Queued legacy send snapshot is incomplete".into(),
            ));
        }
        send_projection_fields(transaction, &payload)?;
        payload.snapshot_version = Some(2);
        payload.queued_at_ms = Some(work.operation_created_at);
        let submission_message_id = payload.submission_message_id.as_deref().ok_or_else(|| {
            StoreError::Validation("Queued legacy send identity is missing".into())
        })?;
        let fingerprint = send_content_fingerprint_v2(
            submission_message_id,
            payload.account_id.as_deref().expect("complete snapshot"),
            payload.sender_email.as_deref().expect("complete snapshot"),
            payload.recipients.as_deref().expect("complete snapshot"),
            payload.cc_recipients.as_deref().expect("complete snapshot"),
            payload
                .bcc_recipients
                .as_deref()
                .expect("complete snapshot"),
            payload.subject.as_deref().expect("complete snapshot"),
            payload.body.as_deref().expect("complete snapshot"),
            payload.body_html.as_deref().expect("complete snapshot"),
            payload.reply_to_thread_id,
            payload.draft_revision.expect("complete snapshot"),
            work.operation_created_at,
            payload.in_reply_to.as_deref(),
            &payload.references,
        );
        payload.content_fingerprint_hex = Some(fingerprint);
        let upgraded_json = serde_json::to_string(&payload)?;
        if upgraded_json.len() > 1024 * 1024 {
            return Err(StoreError::Validation(
                "Upgraded send snapshot exceeds the durable work limit".into(),
            ));
        }
        let payload_fingerprint = Sha256::digest(upgraded_json.as_bytes());
        let operation_changed = transaction.execute(
            "UPDATE operations SET payload_json = ?2
             WHERE id = ?1 AND payload_json = ?3 AND state IN ('pending', 'retrying')",
            params![work.operation_id, upgraded_json, operation_payload_json],
        )?;
        if operation_changed != 1 {
            return Err(StoreError::Conflict(
                "Queued legacy send operation changed during upgrade".into(),
            ));
        }
        let deleted = transaction.execute(
            "DELETE FROM provider_work_items
             WHERE id = ?1 AND payload_json = ?2 AND state = ?3
               AND lease_owner IS NULL AND lease_token IS NULL AND lease_expires_at IS NULL",
            params![work.id, work.payload_json, work.state],
        )?;
        if deleted != 1 {
            return Err(StoreError::Conflict(
                "Queued legacy send changed during upgrade".into(),
            ));
        }
        transaction.execute(
            "INSERT INTO provider_work_items(
               id, account_id, operation_id, kind, scope, ordering_key, retry_safety,
               payload_json, payload_fingerprint, state, priority, created_at,
               available_at, attempt_count, max_attempts, cancel_requested,
               last_error_code, auth_block_reason, retry_after_at
             ) VALUES(
               ?1, ?2, ?3, 'send', ?4, ?5, 'non_idempotent_send', ?6, ?7, ?8,
               ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17
             )",
            params![
                work.id,
                work.account_id,
                work.operation_id,
                work.scope,
                work.ordering_key,
                upgraded_json,
                payload_fingerprint.as_slice(),
                work.state,
                work.priority,
                work.created_at,
                work.available_at,
                work.attempt_count,
                work.max_attempts,
                work.cancel_requested,
                work.last_error_code,
                work.auth_block_reason,
                work.retry_after_at,
            ],
        )?;
    }
    Ok(())
}

struct LegacySendWork {
    id: String,
    account_id: String,
    operation_id: String,
    scope: String,
    ordering_key: String,
    payload_json: String,
    state: String,
    priority: i64,
    created_at: i64,
    available_at: i64,
    attempt_count: i64,
    max_attempts: i64,
    cancel_requested: i64,
    last_error_code: Option<String>,
    auth_block_reason: Option<String>,
    retry_after_at: Option<i64>,
    operation_payload_json: Option<String>,
    operation_created_at: i64,
}
