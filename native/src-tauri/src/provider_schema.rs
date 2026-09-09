use rusqlite::{Connection, OptionalExtension, Transaction};

use crate::store::StoreError;

const PROVIDER_ACCOUNTS_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS provider_accounts (
       account_id TEXT PRIMARY KEY CHECK(
         length(CAST(account_id AS BLOB)) BETWEEN 1 AND 256
       ) REFERENCES accounts(id) ON DELETE CASCADE,
       provider_kind TEXT NOT NULL CHECK(provider_kind IN ('gmail', 'imap', 'jmap', 'pop')),
       remote_account_id TEXT NOT NULL CHECK(
         length(CAST(remote_account_id AS BLOB)) BETWEEN 1 AND 2048
       ),
       auth_state TEXT NOT NULL DEFAULT 'signed_out' CHECK(auth_state IN (
         'signed_out', 'ready', 'reauthorization_required', 'unavailable'
       )),
       credential_ref TEXT CHECK(
         credential_ref IS NULL OR
         length(CAST(credential_ref AS BLOB)) BETWEEN 1 AND 256
       ),
       auth_block_reason TEXT CHECK(
         auth_block_reason IS NULL OR auth_block_reason = 'provider_reauthorization'
       ),
       sync_state TEXT NOT NULL DEFAULT 'never_synced' CHECK(sync_state IN (
         'never_synced', 'idle', 'scheduled', 'syncing', 'backoff',
         'authentication_blocked', 'offline', 'failed'
       )),
       refresh_seconds INTEGER NOT NULL DEFAULT 60 CHECK(
         refresh_seconds BETWEEN 15 AND 86400
       ),
       last_error_code TEXT CHECK(last_error_code IS NULL OR length(last_error_code) <= 200),
       last_sync_at INTEGER CHECK(last_sync_at IS NULL OR last_sync_at >= 0),
       created_at INTEGER NOT NULL CHECK(created_at >= 0),
       updated_at INTEGER NOT NULL CHECK(updated_at >= 0),
       CHECK(
         auth_state NOT IN ('ready', 'reauthorization_required') OR
         credential_ref IS NOT NULL
       )
     );";

pub(crate) fn provider_accounts_need_rebuild(
    connection: &Connection,
    stored_schema_version: i64,
) -> Result<bool, StoreError> {
    let schema: Option<String> = connection
        .query_row(
            "SELECT sql FROM sqlite_master
             WHERE type = 'table' AND name = 'provider_accounts'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(schema) = schema else {
        return Ok(false);
    };
    if stored_schema_version < 12 {
        return Ok(true);
    }

    let normalized = schema.split_whitespace().collect::<Vec<_>>().join(" ");
    Ok(![
        "account_id TEXT PRIMARY KEY CHECK( length(CAST(account_id AS BLOB)) BETWEEN 1 AND 256 ) REFERENCES accounts(id) ON DELETE CASCADE",
        "provider_kind TEXT NOT NULL CHECK(provider_kind IN ('gmail', 'imap', 'jmap', 'pop'))",
        "auth_state TEXT NOT NULL DEFAULT 'signed_out' CHECK(auth_state IN ( 'signed_out', 'ready', 'reauthorization_required', 'unavailable' ))",
        "credential_ref TEXT CHECK( credential_ref IS NULL OR length(CAST(credential_ref AS BLOB)) BETWEEN 1 AND 256 )",
        "auth_block_reason TEXT CHECK( auth_block_reason IS NULL OR auth_block_reason = 'provider_reauthorization' )",
        "sync_state TEXT NOT NULL DEFAULT 'never_synced' CHECK(sync_state IN ( 'never_synced', 'idle', 'scheduled', 'syncing', 'backoff', 'authentication_blocked', 'offline', 'failed' ))",
        "refresh_seconds INTEGER NOT NULL DEFAULT 60 CHECK( refresh_seconds BETWEEN 15 AND 86400 )",
        "CHECK( auth_state NOT IN ('ready', 'reauthorization_required') OR credential_ref IS NOT NULL )",
    ]
    .iter()
    .all(|fragment| normalized.contains(fragment)))
}

pub(crate) fn provider_containers_need_rebuild(
    connection: &Connection,
    stored_schema_version: i64,
) -> Result<bool, StoreError> {
    if stored_schema_version >= 20 {
        return Ok(false);
    }
    Ok(connection
        .query_row(
            "SELECT 1 FROM sqlite_master
             WHERE type = 'table' AND name = 'provider_containers'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

pub(crate) fn migrate(
    transaction: &Transaction<'_>,
    stored_schema_version: i64,
    rebuild_provider_accounts: bool,
    rebuild_provider_containers: bool,
) -> Result<(), StoreError> {
    if rebuild_provider_accounts {
        rebuild_provider_accounts_table(transaction)?;
    }
    if rebuild_provider_containers {
        rebuild_provider_containers_table(transaction)?;
    }
    transaction.execute_batch(PROVIDER_ACCOUNTS_SCHEMA)?;
    transaction.execute_batch(
         "CREATE TABLE IF NOT EXISTS provider_capabilities (
           account_id TEXT NOT NULL REFERENCES provider_accounts(account_id) ON DELETE CASCADE,
           capability TEXT NOT NULL CHECK(length(capability) BETWEEN 1 AND 200),
           enabled INTEGER NOT NULL CHECK(enabled IN (0, 1)),
           PRIMARY KEY(account_id, capability)
         ) WITHOUT ROWID;

         CREATE TABLE IF NOT EXISTS provider_containers (
           account_id TEXT NOT NULL REFERENCES provider_accounts(account_id) ON DELETE CASCADE,
           remote_id TEXT NOT NULL CHECK(
             length(CAST(remote_id AS BLOB)) BETWEEN 1 AND 2048
           ),
           name TEXT NOT NULL CHECK(length(CAST(name AS BLOB)) BETWEEN 1 AND 512),
           kind TEXT NOT NULL CHECK(kind IN (
             'mailbox', 'folder', 'label', 'retrieval_state'
           )),
           role TEXT NOT NULL DEFAULT 'custom' CHECK(role IN (
             'inbox', 'archive', 'all_mail', 'drafts', 'sent', 'trash', 'spam',
             'starred', 'important', 'custom'
           )),
           parent_remote_id TEXT CHECK(
             parent_remote_id IS NULL OR (
               length(CAST(parent_remote_id AS BLOB)) BETWEEN 1 AND 2048
               AND parent_remote_id <> remote_id
             )
           ),
           sort_order INTEGER NOT NULL DEFAULT 0,
           is_selectable INTEGER NOT NULL DEFAULT 1 CHECK(is_selectable IN (0, 1)),
           is_deleted INTEGER NOT NULL DEFAULT 0 CHECK(is_deleted IN (0, 1)),
           revision TEXT CHECK(revision IS NULL OR length(revision) <= 2000),
           PRIMARY KEY(account_id, remote_id),
           FOREIGN KEY(account_id, parent_remote_id)
             REFERENCES provider_containers(account_id, remote_id)
             DEFERRABLE INITIALLY DEFERRED
         ) WITHOUT ROWID;

         CREATE TABLE IF NOT EXISTS provider_thread_refs (
           account_id TEXT NOT NULL REFERENCES provider_accounts(account_id) ON DELETE CASCADE,
           remote_thread_id TEXT NOT NULL CHECK(
             length(CAST(remote_thread_id AS BLOB)) BETWEEN 1 AND 2048
           ),
           thread_id INTEGER NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
           revision TEXT CHECK(revision IS NULL OR length(revision) <= 2000),
           PRIMARY KEY(account_id, remote_thread_id),
           UNIQUE(account_id, thread_id)
         ) WITHOUT ROWID;

         CREATE TABLE IF NOT EXISTS provider_message_refs (
           account_id TEXT NOT NULL REFERENCES provider_accounts(account_id) ON DELETE CASCADE,
           remote_message_id TEXT NOT NULL CHECK(
             length(CAST(remote_message_id AS BLOB)) BETWEEN 1 AND 2048
           ),
           message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
           remote_thread_id TEXT CHECK(
             remote_thread_id IS NULL OR
             length(CAST(remote_thread_id AS BLOB)) BETWEEN 1 AND 2048
           ),
           revision TEXT CHECK(revision IS NULL OR length(revision) <= 2000),
           body_state TEXT NOT NULL DEFAULT 'unavailable' CHECK(body_state IN (
             'unavailable', 'metadata', 'normalized', 'failed'
           )),
           body_is_truncated INTEGER NOT NULL DEFAULT 0 CHECK(body_is_truncated IN (0, 1)),
           client_correlation_id TEXT CHECK(
             client_correlation_id IS NULL OR
             length(CAST(client_correlation_id AS BLOB)) BETWEEN 1 AND 256
           ),
           PRIMARY KEY(account_id, remote_message_id),
           UNIQUE(account_id, message_id),
           FOREIGN KEY(account_id, remote_thread_id)
             REFERENCES provider_thread_refs(account_id, remote_thread_id)
             DEFERRABLE INITIALLY DEFERRED
         ) WITHOUT ROWID;

         CREATE TABLE IF NOT EXISTS provider_container_memberships (
           account_id TEXT NOT NULL,
           remote_message_id TEXT NOT NULL,
           remote_container_id TEXT NOT NULL,
           PRIMARY KEY(account_id, remote_message_id, remote_container_id),
           FOREIGN KEY(account_id, remote_message_id)
             REFERENCES provider_message_refs(account_id, remote_message_id) ON DELETE CASCADE,
           FOREIGN KEY(account_id, remote_container_id)
             REFERENCES provider_containers(account_id, remote_id) ON DELETE CASCADE
         ) WITHOUT ROWID;

         CREATE TABLE IF NOT EXISTS provider_message_keywords (
           account_id TEXT NOT NULL,
           remote_message_id TEXT NOT NULL,
           keyword TEXT NOT NULL CHECK(length(keyword) BETWEEN 1 AND 500),
           PRIMARY KEY(account_id, remote_message_id, keyword),
           FOREIGN KEY(account_id, remote_message_id)
             REFERENCES provider_message_refs(account_id, remote_message_id) ON DELETE CASCADE
         ) WITHOUT ROWID;

         CREATE TABLE IF NOT EXISTS provider_sync_cursors (
           account_id TEXT NOT NULL REFERENCES provider_accounts(account_id) ON DELETE CASCADE,
           scope TEXT NOT NULL CHECK(length(CAST(scope AS BLOB)) BETWEEN 1 AND 4096),
           cursor TEXT NOT NULL CHECK(length(CAST(cursor AS BLOB)) BETWEEN 1 AND 16384),
           updated_at INTEGER NOT NULL CHECK(updated_at >= 0),
           PRIMARY KEY(account_id, scope)
         ) WITHOUT ROWID;

         CREATE TABLE IF NOT EXISTS provider_applied_batches (
           account_id TEXT NOT NULL REFERENCES provider_accounts(account_id) ON DELETE CASCADE,
           batch_id TEXT NOT NULL CHECK(
             length(CAST(batch_id AS BLOB)) BETWEEN 1 AND 256
           ),
           fingerprint BLOB NOT NULL CHECK(length(fingerprint) = 32),
           fingerprint_version INTEGER NOT NULL CHECK(fingerprint_version IN (1, 2)),
           cursor_scope TEXT NOT NULL CHECK(
             length(CAST(cursor_scope AS BLOB)) BETWEEN 1 AND 4096
           ),
           cursor TEXT NOT NULL CHECK(length(CAST(cursor AS BLOB)) BETWEEN 1 AND 16384),
           applied_at INTEGER NOT NULL CHECK(applied_at >= 0),
           PRIMARY KEY(account_id, batch_id)
         ) WITHOUT ROWID;

         CREATE TABLE IF NOT EXISTS provider_work_items (
           id TEXT PRIMARY KEY CHECK(length(CAST(id AS BLOB)) BETWEEN 1 AND 256),
           account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
           operation_id TEXT REFERENCES operations(id) ON DELETE RESTRICT,
           kind TEXT NOT NULL CHECK(kind IN ('sync', 'mutation', 'send')),
           scope TEXT NOT NULL CHECK(length(CAST(scope AS BLOB)) BETWEEN 1 AND 4096),
           ordering_key TEXT NOT NULL CHECK(
             length(CAST(ordering_key AS BLOB)) BETWEEN 1 AND 2048
           ),
           retry_safety TEXT NOT NULL CHECK(retry_safety IN (
             'safe_retry', 'non_idempotent_send'
           )),
           payload_json TEXT NOT NULL CHECK(
             length(CAST(payload_json AS BLOB)) <= 1048576 AND json_valid(payload_json)
           ),
           payload_fingerprint BLOB NOT NULL CHECK(length(payload_fingerprint) = 32),
           state TEXT NOT NULL CHECK(state IN (
             'queued', 'executing', 'retry_wait', 'rate_limited',
             'authentication_blocked', 'succeeded', 'failed', 'cancelled',
             'outcome_unknown'
           )),
           priority INTEGER NOT NULL DEFAULT 0 CHECK(priority BETWEEN -1000 AND 1000),
           created_at INTEGER NOT NULL CHECK(created_at >= 0),
           available_at INTEGER NOT NULL CHECK(available_at >= 0),
           attempt_count INTEGER NOT NULL DEFAULT 0 CHECK(attempt_count >= 0),
           max_attempts INTEGER NOT NULL DEFAULT 8 CHECK(max_attempts BETWEEN 1 AND 100),
           lease_owner TEXT CHECK(
             lease_owner IS NULL OR length(CAST(lease_owner AS BLOB)) BETWEEN 1 AND 256
           ),
           lease_token TEXT CHECK(
             lease_token IS NULL OR length(CAST(lease_token AS BLOB)) BETWEEN 1 AND 256
           ),
           lease_expires_at INTEGER CHECK(lease_expires_at IS NULL OR lease_expires_at >= 0),
           cancel_requested INTEGER NOT NULL DEFAULT 0 CHECK(cancel_requested IN (0, 1)),
           last_error_code TEXT CHECK(
             last_error_code IS NULL OR length(CAST(last_error_code AS BLOB)) BETWEEN 1 AND 512
           ),
           auth_block_reason TEXT CHECK(
             auth_block_reason IS NULL OR auth_block_reason = 'provider_reauthorization'
           ),
           retry_after_at INTEGER CHECK(retry_after_at IS NULL OR retry_after_at >= 0),
           completed_at INTEGER CHECK(completed_at IS NULL OR completed_at >= 0),
           CHECK(
             (kind = 'send' AND retry_safety = 'non_idempotent_send') OR
             (kind <> 'send' AND retry_safety = 'safe_retry')
           ),
           CHECK(
             (state = 'executing' AND lease_owner IS NOT NULL AND lease_token IS NOT NULL
               AND lease_expires_at IS NOT NULL) OR
             (state <> 'executing' AND lease_owner IS NULL AND lease_token IS NULL
               AND lease_expires_at IS NULL)
           ),
           CHECK(
             (state IN ('succeeded', 'failed', 'cancelled', 'outcome_unknown')
               AND completed_at IS NOT NULL) OR
             (state NOT IN ('succeeded', 'failed', 'cancelled', 'outcome_unknown')
               AND completed_at IS NULL)
           )
         );

         CREATE TABLE IF NOT EXISTS provider_tombstones (
           account_id TEXT NOT NULL REFERENCES provider_accounts(account_id) ON DELETE CASCADE,
           object_kind TEXT NOT NULL CHECK(object_kind IN (
             'container', 'thread', 'message', 'membership'
           )),
           remote_id TEXT NOT NULL CHECK(
             length(CAST(remote_id AS BLOB)) BETWEEN 1 AND 2048
           ),
           related_remote_id TEXT NOT NULL DEFAULT '' CHECK(
             (object_kind = 'membership' AND
              length(CAST(related_remote_id AS BLOB)) BETWEEN 1 AND 2048)
             OR
             (object_kind <> 'membership' AND related_remote_id = '')
           ),
           revision TEXT CHECK(revision IS NULL OR length(revision) <= 2000),
           cursor TEXT CHECK(
             cursor IS NULL OR length(CAST(cursor AS BLOB)) BETWEEN 1 AND 16384
           ),
           deleted_at INTEGER NOT NULL CHECK(deleted_at >= 0),
           PRIMARY KEY(account_id, object_kind, remote_id, related_remote_id)
         ) WITHOUT ROWID;

         CREATE TABLE IF NOT EXISTS provider_reconciliation_runs (
           account_id TEXT NOT NULL REFERENCES provider_accounts(account_id) ON DELETE CASCADE,
           scope TEXT NOT NULL CHECK(length(CAST(scope AS BLOB)) BETWEEN 1 AND 4096),
           generation_id TEXT NOT NULL CHECK(
             length(CAST(generation_id AS BLOB)) BETWEEN 1 AND 256
           ),
           started_at INTEGER NOT NULL CHECK(started_at >= 0),
           containers_complete INTEGER NOT NULL DEFAULT 0
             CHECK(containers_complete IN (0, 1)),
           threads_complete INTEGER NOT NULL DEFAULT 0
             CHECK(threads_complete IN (0, 1)),
           messages_complete INTEGER NOT NULL DEFAULT 0
             CHECK(messages_complete IN (0, 1)),
           PRIMARY KEY(account_id, scope),
           UNIQUE(account_id, generation_id)
         ) WITHOUT ROWID;

         CREATE TABLE IF NOT EXISTS provider_reconciliation_seen (
           account_id TEXT NOT NULL,
           generation_id TEXT NOT NULL,
           object_kind TEXT NOT NULL CHECK(object_kind IN ('container', 'thread', 'message')),
           remote_id TEXT NOT NULL CHECK(
             length(CAST(remote_id AS BLOB)) BETWEEN 1 AND 2048
           ),
           PRIMARY KEY(account_id, generation_id, object_kind, remote_id),
           FOREIGN KEY(account_id, generation_id)
             REFERENCES provider_reconciliation_runs(account_id, generation_id)
             ON DELETE CASCADE
         ) WITHOUT ROWID;

         CREATE INDEX IF NOT EXISTS provider_containers_active_idx
           ON provider_containers(account_id, is_deleted, sort_order, remote_id);
         CREATE INDEX IF NOT EXISTS provider_thread_refs_local_idx
           ON provider_thread_refs(thread_id);
         CREATE INDEX IF NOT EXISTS provider_message_refs_local_idx
           ON provider_message_refs(message_id);
         CREATE INDEX IF NOT EXISTS provider_memberships_container_idx
           ON provider_container_memberships(account_id, remote_container_id, remote_message_id);
         CREATE INDEX IF NOT EXISTS provider_tombstones_deleted_idx
           ON provider_tombstones(account_id, deleted_at);
         CREATE INDEX IF NOT EXISTS provider_reconciliation_seen_sweep_idx
           ON provider_reconciliation_seen(account_id, generation_id, object_kind, remote_id);
         CREATE INDEX IF NOT EXISTS provider_batches_applied_idx
           ON provider_applied_batches(account_id, applied_at, batch_id);
         CREATE INDEX IF NOT EXISTS provider_work_due_idx
           ON provider_work_items(state, available_at, priority DESC, created_at, id);
         CREATE INDEX IF NOT EXISTS provider_work_scope_order_idx
           ON provider_work_items(account_id, scope, created_at, id, state);
         CREATE INDEX IF NOT EXISTS provider_work_account_active_idx
           ON provider_work_items(account_id, state, lease_expires_at);
         CREATE UNIQUE INDEX IF NOT EXISTS provider_work_operation_idx
           ON provider_work_items(operation_id) WHERE operation_id IS NOT NULL;
         CREATE UNIQUE INDEX IF NOT EXISTS provider_work_ordering_key_idx
           ON provider_work_items(account_id, kind, ordering_key);

         CREATE TRIGGER IF NOT EXISTS provider_work_identity_immutable
         BEFORE UPDATE OF account_id, operation_id, kind, scope, ordering_key,
                          retry_safety, payload_json, payload_fingerprint
         ON provider_work_items
         BEGIN
           SELECT RAISE(ABORT, 'provider work identity and payload are immutable');
         END;

         CREATE TRIGGER IF NOT EXISTS provider_work_error_code_insert
         BEFORE INSERT ON provider_work_items
         WHEN NEW.last_error_code IS NOT NULL AND (
           length(NEW.last_error_code) NOT BETWEEN 1 AND 128 OR
           NEW.last_error_code <> lower(NEW.last_error_code) OR
           NEW.last_error_code GLOB '*[^a-z0-9_]*' OR
           substr(NEW.last_error_code, 1, 1) NOT GLOB '[a-z]'
         )
         BEGIN
           SELECT RAISE(ABORT, 'provider work error code must be a bounded identifier');
         END;

         CREATE TRIGGER IF NOT EXISTS provider_work_error_code_update
         BEFORE UPDATE OF last_error_code ON provider_work_items
         WHEN NEW.last_error_code IS NOT NULL AND (
           length(NEW.last_error_code) NOT BETWEEN 1 AND 128 OR
           NEW.last_error_code <> lower(NEW.last_error_code) OR
           NEW.last_error_code GLOB '*[^a-z0-9_]*' OR
           substr(NEW.last_error_code, 1, 1) NOT GLOB '[a-z]'
         )
         BEGIN
           SELECT RAISE(ABORT, 'provider work error code must be a bounded identifier');
         END;

         CREATE TRIGGER IF NOT EXISTS provider_account_error_code_insert
         BEFORE INSERT ON provider_accounts
         WHEN NEW.last_error_code IS NOT NULL AND (
           length(NEW.last_error_code) NOT BETWEEN 1 AND 128 OR
           NEW.last_error_code <> lower(NEW.last_error_code) OR
           NEW.last_error_code GLOB '*[^a-z0-9_]*' OR
           substr(NEW.last_error_code, 1, 1) NOT GLOB '[a-z]'
         )
         BEGIN
           SELECT RAISE(ABORT, 'provider account error code must be a bounded identifier');
         END;

         CREATE TRIGGER IF NOT EXISTS provider_account_error_code_update
         BEFORE UPDATE OF last_error_code ON provider_accounts
         WHEN NEW.last_error_code IS NOT NULL AND (
           length(NEW.last_error_code) NOT BETWEEN 1 AND 128 OR
           NEW.last_error_code <> lower(NEW.last_error_code) OR
           NEW.last_error_code GLOB '*[^a-z0-9_]*' OR
           substr(NEW.last_error_code, 1, 1) NOT GLOB '[a-z]'
         )
         BEGIN
           SELECT RAISE(ABORT, 'provider account error code must be a bounded identifier');
         END;

         CREATE TRIGGER IF NOT EXISTS provider_work_operation_insert
         BEFORE INSERT ON provider_work_items
         WHEN NEW.operation_id IS NOT NULL AND NOT EXISTS (
           SELECT 1
           FROM operations operation
           LEFT JOIN threads thread ON thread.id = operation.thread_id
           WHERE operation.id = NEW.operation_id
             AND (
               (
                 NEW.kind = 'mutation' AND operation.field <> 'send'
                 AND thread.account_id = NEW.account_id
               ) OR (
                 NEW.kind = 'send' AND operation.field = 'send'
                 AND json_extract(operation.payload_json, '$.accountId') = NEW.account_id
               )
             )
         )
         BEGIN
           SELECT RAISE(ABORT, 'provider work operation/account mismatch');
         END;

         CREATE TRIGGER IF NOT EXISTS provider_thread_refs_account_insert
         BEFORE INSERT ON provider_thread_refs
         WHEN NOT EXISTS (
           SELECT 1 FROM threads
           WHERE id = NEW.thread_id AND account_id = NEW.account_id
         )
         BEGIN
           SELECT RAISE(ABORT, 'provider thread account mismatch');
         END;

         CREATE TRIGGER IF NOT EXISTS provider_thread_refs_account_update
         BEFORE UPDATE OF account_id, thread_id ON provider_thread_refs
         WHEN NOT EXISTS (
           SELECT 1 FROM threads
           WHERE id = NEW.thread_id AND account_id = NEW.account_id
         )
         BEGIN
           SELECT RAISE(ABORT, 'provider thread account mismatch');
         END;

         CREATE TRIGGER IF NOT EXISTS provider_message_refs_account_insert
         BEFORE INSERT ON provider_message_refs
         WHEN NOT EXISTS (
           SELECT 1
           FROM messages
           JOIN threads ON threads.id = messages.thread_id
           WHERE messages.id = NEW.message_id AND threads.account_id = NEW.account_id
         )
         BEGIN
           SELECT RAISE(ABORT, 'provider message account mismatch');
         END;

         CREATE TRIGGER IF NOT EXISTS provider_message_refs_account_update
         BEFORE UPDATE OF account_id, message_id ON provider_message_refs
         WHEN NOT EXISTS (
           SELECT 1
           FROM messages
           JOIN threads ON threads.id = messages.thread_id
           WHERE messages.id = NEW.message_id AND threads.account_id = NEW.account_id
         )
         BEGIN
           SELECT RAISE(ABORT, 'provider message account mismatch');
         END;

         CREATE TRIGGER IF NOT EXISTS provider_message_refs_thread_insert
         BEFORE INSERT ON provider_message_refs
         WHEN NEW.remote_thread_id IS NOT NULL AND NOT EXISTS (
           SELECT 1
           FROM messages
           JOIN provider_thread_refs ON
             provider_thread_refs.account_id = NEW.account_id AND
             provider_thread_refs.remote_thread_id = NEW.remote_thread_id AND
             provider_thread_refs.thread_id = messages.thread_id
           WHERE messages.id = NEW.message_id
         )
         BEGIN
           SELECT RAISE(ABORT, 'provider message thread mismatch');
         END;

         CREATE TRIGGER IF NOT EXISTS provider_message_refs_thread_update
         BEFORE UPDATE OF account_id, message_id, remote_thread_id ON provider_message_refs
         WHEN NEW.remote_thread_id IS NOT NULL AND NOT EXISTS (
           SELECT 1
           FROM messages
           JOIN provider_thread_refs ON
             provider_thread_refs.account_id = NEW.account_id AND
             provider_thread_refs.remote_thread_id = NEW.remote_thread_id AND
             provider_thread_refs.thread_id = messages.thread_id
           WHERE messages.id = NEW.message_id
         )
         BEGIN
           SELECT RAISE(ABORT, 'provider message thread mismatch');
         END;

         CREATE TRIGGER IF NOT EXISTS provider_thread_refs_local_update
         BEFORE UPDATE OF account_id, remote_thread_id, thread_id ON provider_thread_refs
         WHEN EXISTS (
           SELECT 1
           FROM provider_message_refs
           JOIN messages ON messages.id = provider_message_refs.message_id
           WHERE provider_message_refs.account_id = OLD.account_id
             AND provider_message_refs.remote_thread_id = OLD.remote_thread_id
             AND messages.thread_id <> NEW.thread_id
         )
         BEGIN
           SELECT RAISE(ABORT, 'provider thread has local message references');
         END;

         CREATE TRIGGER IF NOT EXISTS threads_provider_account_update
         BEFORE UPDATE OF account_id ON threads
         WHEN NEW.account_id <> OLD.account_id AND (
           EXISTS (
             SELECT 1 FROM provider_thread_refs
             WHERE provider_thread_refs.thread_id = OLD.id
           ) OR EXISTS (
             SELECT 1
             FROM messages
             JOIN provider_message_refs ON provider_message_refs.message_id = messages.id
             WHERE messages.thread_id = OLD.id
           )
         )
         BEGIN
           SELECT RAISE(ABORT, 'thread account is referenced by provider state');
         END;

         CREATE TRIGGER IF NOT EXISTS messages_provider_thread_update
         BEFORE UPDATE OF thread_id ON messages
         WHEN EXISTS (
           SELECT 1
           FROM provider_message_refs
           JOIN threads ON threads.id = NEW.thread_id
           WHERE provider_message_refs.message_id = OLD.id
             AND (
               provider_message_refs.account_id <> threads.account_id OR
               (
                 provider_message_refs.remote_thread_id IS NOT NULL AND NOT EXISTS (
                   SELECT 1 FROM provider_thread_refs
                   WHERE provider_thread_refs.account_id = provider_message_refs.account_id
                     AND provider_thread_refs.remote_thread_id = provider_message_refs.remote_thread_id
                     AND provider_thread_refs.thread_id = NEW.thread_id
                 )
               )
             )
         )
         BEGIN
           SELECT RAISE(ABORT, 'message thread is referenced by provider state');
         END;",
    )?;
    if stored_schema_version < 14 {
        rebuild_provider_applied_batches_v14(transaction)?;
    }
    if !column_exists(transaction, "provider_message_refs", "body_is_truncated")? {
        transaction.execute_batch(
            "ALTER TABLE provider_message_refs
               ADD COLUMN body_is_truncated INTEGER NOT NULL DEFAULT 0
               CHECK(body_is_truncated IN (0, 1));",
        )?;
    }
    if !column_exists(
        transaction,
        "provider_message_refs",
        "client_correlation_id",
    )? {
        transaction.execute_batch(
            "ALTER TABLE provider_message_refs
               ADD COLUMN client_correlation_id TEXT
               CHECK(
                 client_correlation_id IS NULL OR
                 length(CAST(client_correlation_id AS BLOB)) BETWEEN 1 AND 256
               );",
        )?;
    }
    for column in [
        "containers_complete",
        "threads_complete",
        "messages_complete",
    ] {
        if !column_exists(transaction, "provider_reconciliation_runs", column)? {
            transaction.execute_batch(&format!(
                "ALTER TABLE provider_reconciliation_runs
                   ADD COLUMN {column} INTEGER NOT NULL DEFAULT 0
                   CHECK({column} IN (0, 1));"
            ))?;
        }
    }
    if stored_schema_version < 19 {
        // Pre-v19 allowed more than one reconciliation scope per account.
        // Keep the newest deterministic run and let exact-generation fencing
        // reject any continuation belonging to a removed stale run.
        transaction.execute_batch(
            "DELETE FROM provider_reconciliation_seen AS stale_seen
             WHERE EXISTS (
               SELECT 1
               FROM provider_reconciliation_runs AS stale_run
               JOIN provider_reconciliation_runs AS newer
                 ON newer.account_id = stale_run.account_id
                AND (
                  newer.started_at > stale_run.started_at OR
                  (newer.started_at = stale_run.started_at AND newer.scope > stale_run.scope)
                )
               WHERE stale_run.account_id = stale_seen.account_id
                 AND stale_run.generation_id = stale_seen.generation_id
             );
             DELETE FROM provider_reconciliation_runs AS stale
             WHERE EXISTS (
               SELECT 1 FROM provider_reconciliation_runs AS newer
               WHERE newer.account_id = stale.account_id
                 AND (
                   newer.started_at > stale.started_at OR
                   (newer.started_at = stale.started_at AND newer.scope > stale.scope)
                 )
             );",
        )?;
    }
    transaction.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS provider_reconciliation_one_per_account_idx
           ON provider_reconciliation_runs(account_id);",
    )?;
    if !column_exists(transaction, "provider_tombstones", "cursor")? {
        transaction.execute_batch(
            "ALTER TABLE provider_tombstones
               ADD COLUMN cursor TEXT CHECK(
                 cursor IS NULL OR length(CAST(cursor AS BLOB)) BETWEEN 1 AND 16384
               );",
        )?;
    }
    if !column_exists(transaction, "provider_accounts", "credential_ref")? {
        transaction.execute_batch(
            "ALTER TABLE provider_accounts
               ADD COLUMN credential_ref TEXT CHECK(
                 credential_ref IS NULL OR
                 length(CAST(credential_ref AS BLOB)) BETWEEN 1 AND 256
               );",
        )?;
    }
    if !column_exists(transaction, "provider_accounts", "auth_block_reason")? {
        transaction.execute_batch(
            "ALTER TABLE provider_accounts
               ADD COLUMN auth_block_reason TEXT CHECK(
                 auth_block_reason IS NULL OR auth_block_reason = 'provider_reauthorization'
               );",
        )?;
    }
    if !column_exists(transaction, "provider_work_items", "auth_block_reason")? {
        transaction.execute_batch(
            "ALTER TABLE provider_work_items
               ADD COLUMN auth_block_reason TEXT CHECK(
                 auth_block_reason IS NULL OR auth_block_reason = 'provider_reauthorization'
               );",
        )?;
    }
    if stored_schema_version < 11 {
        transaction.execute_batch(
            "UPDATE provider_accounts
               SET auth_block_reason = CASE auth_state
                 WHEN 'credential_locked' THEN 'provider_reauthorization'
                 WHEN 'reauthorization_required' THEN 'provider_reauthorization'
                 ELSE NULL
               END;
             UPDATE provider_work_items
               SET auth_block_reason = CASE
                 WHEN state <> 'authentication_blocked' THEN NULL
                 ELSE 'provider_reauthorization'
               END;",
        )?;
    }
    if !column_exists(transaction, "provider_accounts", "refresh_seconds")? {
        transaction.execute_batch(
            "ALTER TABLE provider_accounts
               ADD COLUMN refresh_seconds INTEGER NOT NULL DEFAULT 60 CHECK(
                 refresh_seconds BETWEEN 15 AND 86400
               );",
        )?;
    }
    if stored_schema_version < 22 {
        // The locked-credential state is gone. Anything still wearing it needs
        // reauthorizing, which is the only way back now.
        transaction.execute_batch(
            "UPDATE provider_work_items
               SET auth_block_reason = 'provider_reauthorization'
             WHERE auth_block_reason = 'credential_locked';
             UPDATE provider_accounts
               SET auth_state = 'reauthorization_required',
                   auth_block_reason = 'provider_reauthorization'
             WHERE auth_state = 'credential_locked' AND credential_ref IS NOT NULL;
             UPDATE provider_accounts
               SET auth_state = 'signed_out', auth_block_reason = NULL
             WHERE auth_state = 'credential_locked';
             UPDATE provider_accounts
               SET auth_block_reason = 'provider_reauthorization'
             WHERE auth_block_reason = 'credential_locked';",
        )?;
    }
    transaction.execute_batch(
        // Recreated rather than IF NOT EXISTS, so an existing database actually
        // loses the old provenance rules instead of keeping them alongside.
        "DROP TRIGGER IF EXISTS provider_account_auth_block_insert;
         DROP TRIGGER IF EXISTS provider_account_auth_block_update;
         DROP TRIGGER IF EXISTS provider_work_auth_block_insert;
         DROP TRIGGER IF EXISTS provider_work_auth_block_update;

         CREATE TRIGGER provider_account_auth_block_insert
         BEFORE INSERT ON provider_accounts
         WHEN NOT (
           (NEW.auth_state = 'reauthorization_required' AND
             NEW.auth_block_reason IS 'provider_reauthorization') OR
           (NEW.auth_state <> 'reauthorization_required' AND
             NEW.auth_block_reason IS NULL)
         )
         BEGIN
           SELECT RAISE(ABORT, 'provider account auth block provenance mismatch');
         END;

         CREATE TRIGGER provider_account_auth_block_update
         BEFORE UPDATE OF auth_state, auth_block_reason ON provider_accounts
         WHEN NOT (
           (NEW.auth_state = 'reauthorization_required' AND
             NEW.auth_block_reason IS 'provider_reauthorization') OR
           (NEW.auth_state <> 'reauthorization_required' AND
             NEW.auth_block_reason IS NULL)
         )
         BEGIN
           SELECT RAISE(ABORT, 'provider account auth block provenance mismatch');
         END;

         CREATE TRIGGER provider_work_auth_block_insert
         BEFORE INSERT ON provider_work_items
         WHEN NOT (
           (NEW.state = 'authentication_blocked' AND
             NEW.auth_block_reason IS 'provider_reauthorization') OR
           (NEW.state <> 'authentication_blocked' AND NEW.auth_block_reason IS NULL)
         )
         BEGIN
           SELECT RAISE(ABORT, 'provider work auth block provenance mismatch');
         END;

         CREATE TRIGGER provider_work_auth_block_update
         BEFORE UPDATE OF state, auth_block_reason ON provider_work_items
         WHEN NOT (
           (NEW.state = 'authentication_blocked' AND
             NEW.auth_block_reason IS 'provider_reauthorization') OR
           (NEW.state <> 'authentication_blocked' AND NEW.auth_block_reason IS NULL)
         )
         BEGIN
           SELECT RAISE(ABORT, 'provider work auth block provenance mismatch');
         END;",
    )?;
    if stored_schema_version == 8 {
        transaction.execute_batch(
            "UPDATE provider_message_refs
               SET body_is_truncated = 1
               WHERE body_state = 'normalized';
             UPDATE provider_tombstones
               SET cursor = revision
               WHERE cursor IS NULL AND revision IS NOT NULL;",
        )?;
    }
    Ok(())
}

fn rebuild_provider_applied_batches_v14(transaction: &Transaction<'_>) -> Result<(), StoreError> {
    let source_fingerprint_version = if column_exists(
        transaction,
        "provider_applied_batches",
        "fingerprint_version",
    )? {
        "fingerprint_version"
    } else {
        // Schemas through v12 stored only the original receipt encoding.
        "1"
    };
    transaction.execute_batch(&format!(
        "DROP TABLE IF EXISTS provider_applied_batches_v14;
         CREATE TABLE provider_applied_batches_v14 (
           account_id TEXT NOT NULL REFERENCES provider_accounts(account_id) ON DELETE CASCADE,
           batch_id TEXT NOT NULL CHECK(
             length(CAST(batch_id AS BLOB)) BETWEEN 1 AND 256
           ),
           fingerprint BLOB NOT NULL CHECK(length(fingerprint) = 32),
           fingerprint_version INTEGER NOT NULL CHECK(fingerprint_version IN (1, 2)),
           cursor_scope TEXT NOT NULL CHECK(
             length(CAST(cursor_scope AS BLOB)) BETWEEN 1 AND 4096
           ),
           cursor TEXT NOT NULL CHECK(length(CAST(cursor AS BLOB)) BETWEEN 1 AND 16384),
           applied_at INTEGER NOT NULL CHECK(applied_at >= 0),
           PRIMARY KEY(account_id, batch_id)
         ) WITHOUT ROWID;
         INSERT INTO provider_applied_batches_v14(
           account_id, batch_id, fingerprint, fingerprint_version,
           cursor_scope, cursor, applied_at
         )
         SELECT account_id, batch_id, fingerprint, {source_fingerprint_version},
                cursor_scope, cursor, applied_at
         FROM provider_applied_batches;
         DROP TABLE provider_applied_batches;
         ALTER TABLE provider_applied_batches_v14 RENAME TO provider_applied_batches;
         CREATE INDEX provider_batches_applied_idx
           ON provider_applied_batches(account_id, applied_at, batch_id);"
    ))?;
    Ok(())
}

fn rebuild_provider_accounts_table(transaction: &Transaction<'_>) -> Result<(), StoreError> {
    let credential_ref = if column_exists(transaction, "provider_accounts", "credential_ref")? {
        "credential_ref"
    } else {
        "NULL"
    };
    let refresh_seconds = if column_exists(transaction, "provider_accounts", "refresh_seconds")? {
        "refresh_seconds"
    } else {
        "60"
    };

    transaction.execute_batch(
        "DROP TABLE IF EXISTS provider_accounts_v12;
         CREATE TABLE provider_accounts_v12 (
           account_id TEXT PRIMARY KEY CHECK(
             length(CAST(account_id AS BLOB)) BETWEEN 1 AND 256
           ) REFERENCES accounts(id) ON DELETE CASCADE,
           provider_kind TEXT NOT NULL CHECK(provider_kind IN ('gmail', 'imap', 'jmap', 'pop')),
           remote_account_id TEXT NOT NULL CHECK(
             length(CAST(remote_account_id AS BLOB)) BETWEEN 1 AND 2048
           ),
           auth_state TEXT NOT NULL DEFAULT 'signed_out' CHECK(auth_state IN (
             'signed_out', 'ready', 'reauthorization_required', 'unavailable'
           )),
           credential_ref TEXT CHECK(
             credential_ref IS NULL OR
             length(CAST(credential_ref AS BLOB)) BETWEEN 1 AND 256
           ),
           auth_block_reason TEXT CHECK(
             auth_block_reason IS NULL OR auth_block_reason = 'provider_reauthorization'
           ),
           sync_state TEXT NOT NULL DEFAULT 'never_synced' CHECK(sync_state IN (
             'never_synced', 'idle', 'scheduled', 'syncing', 'backoff',
             'authentication_blocked', 'offline', 'failed'
           )),
           refresh_seconds INTEGER NOT NULL DEFAULT 60 CHECK(
             refresh_seconds BETWEEN 15 AND 86400
           ),
           last_error_code TEXT CHECK(last_error_code IS NULL OR length(last_error_code) <= 200),
           last_sync_at INTEGER CHECK(last_sync_at IS NULL OR last_sync_at >= 0),
           created_at INTEGER NOT NULL CHECK(created_at >= 0),
           updated_at INTEGER NOT NULL CHECK(updated_at >= 0),
           CHECK(
             auth_state NOT IN ('ready', 'reauthorization_required') OR
             credential_ref IS NOT NULL
           )
         );",
    )?;

    transaction.execute_batch(&format!(
        "INSERT INTO provider_accounts_v12(
           account_id, provider_kind, remote_account_id, auth_state,
           credential_ref, auth_block_reason, sync_state, refresh_seconds,
           last_error_code, last_sync_at, created_at, updated_at
         )
         SELECT account_id, provider_kind, remote_account_id,
           CASE auth_state
             WHEN 'locked' THEN CASE WHEN {credential_ref} IS NULL
               THEN 'signed_out' ELSE 'reauthorization_required' END
             WHEN 'signed_out' THEN 'signed_out'
             WHEN 'credential_locked' THEN CASE WHEN {credential_ref} IS NULL
               THEN 'signed_out' ELSE 'reauthorization_required' END
             WHEN 'ready' THEN CASE WHEN {credential_ref} IS NULL
               THEN 'signed_out' ELSE 'ready' END
             WHEN 'expired' THEN CASE WHEN {credential_ref} IS NULL
               THEN 'signed_out' ELSE 'reauthorization_required' END
             WHEN 'requires_action' THEN CASE WHEN {credential_ref} IS NULL
               THEN 'signed_out' ELSE 'reauthorization_required' END
             WHEN 'reauthorization_required' THEN CASE WHEN {credential_ref} IS NULL
               THEN 'signed_out' ELSE 'reauthorization_required' END
             WHEN 'unavailable' THEN 'unavailable'
             ELSE 'unavailable'
           END,
           {credential_ref},
           CASE
             WHEN {credential_ref} IS NOT NULL AND auth_state IN (
               'locked', 'credential_locked', 'expired', 'requires_action',
               'reauthorization_required'
             )
               THEN 'provider_reauthorization'
             ELSE NULL
           END,
           CASE
             WHEN {credential_ref} IS NOT NULL AND (
               auth_state IN (
                 'locked', 'credential_locked', 'expired', 'requires_action',
                 'reauthorization_required'
               )
             )
               THEN 'authentication_blocked'
             WHEN {credential_ref} IS NULL AND auth_state IN (
               'locked', 'signed_out', 'credential_locked', 'ready', 'expired',
               'requires_action', 'reauthorization_required'
             ) THEN 'offline'
             WHEN sync_state = 'error' THEN 'failed'
             WHEN sync_state IN (
               'never_synced', 'idle', 'scheduled', 'syncing', 'backoff',
               'authentication_blocked', 'offline', 'failed'
             ) THEN sync_state
             ELSE 'failed'
           END,
           {refresh_seconds},
           last_error_code, last_sync_at, created_at, updated_at
         FROM provider_accounts;
         DROP TABLE provider_accounts;
         ALTER TABLE provider_accounts_v12 RENAME TO provider_accounts;"
    ))?;
    Ok(())
}

fn rebuild_provider_containers_table(transaction: &Transaction<'_>) -> Result<(), StoreError> {
    transaction.execute_batch(
        "DROP VIEW IF EXISTS provider_thread_label_effective;
         DROP TABLE IF EXISTS provider_containers_v20;
         CREATE TABLE provider_containers_v20 (
           account_id TEXT NOT NULL REFERENCES provider_accounts(account_id) ON DELETE CASCADE,
           remote_id TEXT NOT NULL CHECK(
             length(CAST(remote_id AS BLOB)) BETWEEN 1 AND 2048
           ),
           name TEXT NOT NULL CHECK(length(CAST(name AS BLOB)) BETWEEN 1 AND 512),
           kind TEXT NOT NULL CHECK(kind IN (
             'mailbox', 'folder', 'label', 'retrieval_state'
           )),
           role TEXT NOT NULL DEFAULT 'custom' CHECK(role IN (
             'inbox', 'archive', 'all_mail', 'drafts', 'sent', 'trash', 'spam',
             'starred', 'important', 'custom'
           )),
           parent_remote_id TEXT CHECK(
             parent_remote_id IS NULL OR (
               length(CAST(parent_remote_id AS BLOB)) BETWEEN 1 AND 2048
               AND parent_remote_id <> remote_id
             )
           ),
           sort_order INTEGER NOT NULL DEFAULT 0,
           is_selectable INTEGER NOT NULL DEFAULT 1 CHECK(is_selectable IN (0, 1)),
           is_deleted INTEGER NOT NULL DEFAULT 0 CHECK(is_deleted IN (0, 1)),
           revision TEXT CHECK(revision IS NULL OR length(revision) <= 2000),
           PRIMARY KEY(account_id, remote_id),
           FOREIGN KEY(account_id, parent_remote_id)
             REFERENCES provider_containers(account_id, remote_id)
             DEFERRABLE INITIALLY DEFERRED
         ) WITHOUT ROWID;
         INSERT INTO provider_containers_v20(
           account_id, remote_id, name, kind, role, parent_remote_id,
           sort_order, is_selectable, is_deleted, revision
         )
         SELECT account_id, remote_id, name,
                CASE kind
                  WHEN 'virtual' THEN 'retrieval_state'
                  WHEN 'import_state' THEN 'retrieval_state'
                  ELSE kind
                END,
                CASE role WHEN 'all' THEN 'all_mail' ELSE role END,
                parent_remote_id, sort_order, is_selectable, is_deleted, revision
         FROM provider_containers;
         DROP TABLE provider_containers;
         ALTER TABLE provider_containers_v20 RENAME TO provider_containers;",
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
