use std::collections::BTreeSet;

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use sha2::{Digest, Sha256};

use crate::provider::{
    ContainerKind, ContainerMembershipChange, ContainerRole, ProviderBatch,
    ProviderBatchApplyCounts, ProviderBatchApplyResult, ProviderBodyState, ProviderContainer,
    ProviderMessageUpsert, ProviderThreadUpsert, ProviderTombstone, RemoteContainerIdentity,
    RemoteMessageIdentity, RemoteThreadIdentity, SyncCursorScope, TombstoneTarget,
};
use crate::store::StoreError;
use crate::worker::ReconciliationObjectKind;

const WRITE_FINGERPRINT_VERSION: i64 = 2;
const LEGACY_FINGERPRINT_CONFLICT: &str =
    "Provider batch receipt uses unverifiable legacy fingerprint version 1";
const RECONCILIATION_SWEEP_PAGE: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderBatchFailpoint {
    None,
    AfterProjection,
    AfterCursor,
}

pub(crate) fn apply_provider_batch(
    connection: &mut Connection,
    batch: ProviderBatch,
    failpoint: ProviderBatchFailpoint,
) -> Result<ProviderBatchApplyResult, StoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let result = apply_provider_batch_in_transaction(&transaction, batch, failpoint)?;
    transaction.commit()?;
    Ok(result)
}

/// Applies the complete normalized provider projection, cursor, and replay receipt inside the
/// caller's transaction. The durable worker uses this entry point so the provider projection and
/// its fenced success acknowledgement commit or roll back as one unit.
pub(crate) fn apply_provider_batch_in_transaction(
    transaction: &Transaction<'_>,
    batch: ProviderBatch,
    failpoint: ProviderBatchFailpoint,
) -> Result<ProviderBatchApplyResult, StoreError> {
    apply_provider_batch_with_options_in_transaction(transaction, batch, failpoint, false, false)
}

/// Applies a normalized page with provider-declared snapshot semantics.
///
/// Gmail message resources carry the complete current `labelIds` set. For
/// those pages the confirmed membership rows for every upserted message are
/// replaced before the page's membership upserts are applied. Pending local
/// operations live in the separate operation overlay and are never deleted by
/// this confirmed-projection replacement.
pub(crate) fn apply_provider_batch_with_options_in_transaction(
    transaction: &Transaction<'_>,
    batch: ProviderBatch,
    failpoint: ProviderBatchFailpoint,
    replace_memberships_for_upserted_messages: bool,
    derive_thread_state_from_messages: bool,
) -> Result<ProviderBatchApplyResult, StoreError> {
    batch
        .validate()
        .map_err(|error| StoreError::Validation(error.to_string()))?;
    let fingerprint = fingerprint_v2(&batch);
    let cursor_scope = cursor_scope_key(&batch.cursor.scope);
    let account_id = batch.mux_account_id.as_str();
    let batch_id = batch.batch_id.as_str();
    let cursor = batch.cursor.value.as_str();
    let expected_prior_cursor = batch
        .expected_prior_cursor
        .as_ref()
        .map(|value| value.as_str());
    let observed_at = batch.observed_at.get();
    let counts = ProviderBatchApplyCounts::from(&batch);

    let account_exists = transaction.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM provider_accounts WHERE account_id = ?1
         )",
        [account_id],
        |row| row.get::<_, i64>(0),
    )? != 0;
    if !account_exists {
        return Err(StoreError::NotFound(
            "Provider account is not configured".into(),
        ));
    }

    let prior_receipt = transaction
        .query_row(
            "SELECT fingerprint, fingerprint_version FROM provider_applied_batches
             WHERE account_id = ?1 AND batch_id = ?2",
            params![account_id, batch_id],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    if let Some((prior_fingerprint, fingerprint_version)) = prior_receipt {
        match fingerprint_version {
            1 => return Err(StoreError::Conflict(LEGACY_FINGERPRINT_CONFLICT.into())),
            2 if prior_fingerprint == fingerprint.as_slice() => {}
            2 => {
                return Err(StoreError::Conflict(
                    "Provider batch ID was reused with different content".into(),
                ));
            }
            other => {
                return Err(StoreError::Conflict(format!(
                    "Provider batch receipt has unsupported fingerprint version {other}"
                )));
            }
        }
        return Ok(ProviderBatchApplyResult {
            applied: false,
            batch_id: batch.batch_id,
            cursor: batch.cursor,
            counts: zero_counts(),
        });
    }

    let durable_cursor = transaction
        .query_row(
            "SELECT cursor FROM provider_sync_cursors
             WHERE account_id = ?1 AND scope = ?2",
            params![account_id, &cursor_scope],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if durable_cursor.as_deref() != expected_prior_cursor {
        return Err(StoreError::Conflict(
            "Provider batch prior cursor no longer matches durable state".into(),
        ));
    }

    for container in &batch.container_upserts {
        upsert_container(transaction, container)?;
    }
    let mut affected_thread_ids = BTreeSet::new();
    for thread in &batch.thread_upserts {
        affected_thread_ids.insert(upsert_thread(transaction, thread)?);
    }
    for message in &batch.message_upserts {
        if let Some(thread_id) = local_thread_for_message(
            transaction,
            message.identity.mux_account_id.as_str(),
            message.identity.remote_message_id.as_str(),
        )? {
            affected_thread_ids.insert(thread_id);
        }
        let (_, thread_id) = upsert_message(transaction, message)?;
        affected_thread_ids.insert(thread_id);
    }
    if replace_memberships_for_upserted_messages {
        for message in &batch.message_upserts {
            transaction.execute(
                "DELETE FROM provider_container_memberships
                 WHERE account_id = ?1 AND remote_message_id = ?2",
                params![
                    message.identity.mux_account_id.as_str(),
                    message.identity.remote_message_id.as_str()
                ],
            )?;
        }
    }
    for change in &batch.membership_changes {
        apply_membership_change(transaction, change)?;
    }
    for tombstone in &batch.tombstones {
        affected_thread_ids.extend(affected_threads_for_tombstone(
            transaction,
            account_id,
            tombstone,
        )?);
        apply_tombstone(transaction, account_id, tombstone)?;
    }

    if derive_thread_state_from_messages {
        for thread_id in &affected_thread_ids {
            derive_thread_projection(transaction, *thread_id)?;
        }
    }

    for thread_id in affected_thread_ids {
        rebuild_search_index_for_thread(transaction, thread_id)?;
    }

    if failpoint == ProviderBatchFailpoint::AfterProjection {
        return Err(StoreError::Conflict(
            "Injected failure after provider projection".into(),
        ));
    }

    transaction.execute(
        "INSERT INTO provider_sync_cursors(account_id, scope, cursor, updated_at)
         VALUES(?1, ?2, ?3, ?4)
         ON CONFLICT(account_id, scope) DO UPDATE SET
           cursor = excluded.cursor,
           updated_at = excluded.updated_at",
        params![account_id, cursor_scope, cursor, observed_at],
    )?;

    if failpoint == ProviderBatchFailpoint::AfterCursor {
        return Err(StoreError::Conflict(
            "Injected failure after provider cursor".into(),
        ));
    }

    transaction.execute(
        "INSERT INTO provider_applied_batches(
           account_id, batch_id, fingerprint, fingerprint_version,
           cursor_scope, cursor, applied_at
         ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            account_id,
            batch_id,
            fingerprint.as_slice(),
            WRITE_FINGERPRINT_VERSION,
            cursor_scope,
            cursor,
            observed_at
        ],
    )?;
    Ok(ProviderBatchApplyResult {
        applied: true,
        batch_id: batch.batch_id,
        cursor: batch.cursor,
        counts,
    })
}

fn zero_counts() -> ProviderBatchApplyCounts {
    ProviderBatchApplyCounts {
        thread_upserts: 0,
        message_upserts: 0,
        container_upserts: 0,
        membership_changes: 0,
        tombstones: 0,
    }
}

/// Records one bounded full-reconciliation page and optionally sweeps one
/// bounded page of provider-confirmed objects that were not observed. Local
/// operations are never touched: tombstones update only the confirmed remote
/// projection underneath the pending-intent overlay.
///
/// Returns `true` when a requested sweep has no more unseen active objects and
/// the reconciliation run was removed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_reconciliation_page_in_transaction(
    transaction: &Transaction<'_>,
    batch: &ProviderBatch,
    work_scope: &str,
    generation_id: &str,
    begin: bool,
    reset_seen_containers: bool,
    complete_kinds: &BTreeSet<ReconciliationObjectKind>,
    sweep_kinds: &BTreeSet<ReconciliationObjectKind>,
    seen_remote_messages: &[crate::provider::RemoteMessageIdentity],
) -> Result<bool, StoreError> {
    if generation_id.is_empty()
        || generation_id.len() > 256
        || generation_id.chars().any(char::is_control)
    {
        return Err(StoreError::Validation(
            "Provider reconciliation generation is invalid".into(),
        ));
    }
    if work_scope.is_empty() || work_scope.len() > 4_096 {
        return Err(StoreError::Validation(
            "Provider reconciliation scope is invalid".into(),
        ));
    }
    let account_id = batch.mux_account_id.as_str();
    let current_run = transaction
        .query_row(
            "SELECT scope, generation_id FROM provider_reconciliation_runs
             WHERE account_id = ?1",
            [account_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    if begin {
        match current_run.as_ref() {
            Some((scope, _)) if scope != work_scope => {
                return Err(StoreError::Conflict(
                    "Provider reconciliation is already active under another work scope".into(),
                ));
            }
            Some((_, current_generation)) if current_generation == generation_id => {}
            Some(_) => {
                transaction.execute(
                    "DELETE FROM provider_reconciliation_runs WHERE account_id = ?1",
                    [account_id],
                )?;
                insert_reconciliation_run(
                    transaction,
                    account_id,
                    work_scope,
                    generation_id,
                    batch.observed_at.get(),
                )?;
            }
            None => insert_reconciliation_run(
                transaction,
                account_id,
                work_scope,
                generation_id,
                batch.observed_at.get(),
            )?,
        }
    } else if current_run
        .as_ref()
        .map(|(scope, generation)| (scope.as_str(), generation.as_str()))
        != Some((work_scope, generation_id))
    {
        return Err(StoreError::Conflict(
            "Provider reconciliation generation is not durable".into(),
        ));
    }

    let mut completeness = transaction.query_row(
        "SELECT containers_complete, threads_complete, messages_complete
         FROM provider_reconciliation_runs
         WHERE account_id = ?1 AND generation_id = ?2",
        params![account_id, generation_id],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        },
    )?;

    if reset_seen_containers {
        if complete_kinds.contains(&ReconciliationObjectKind::Containers) {
            return Err(StoreError::Conflict(
                "Provider reconciliation container reset requires a later completeness page".into(),
            ));
        }
        transaction.execute(
            "DELETE FROM provider_reconciliation_seen
             WHERE account_id = ?1 AND generation_id = ?2 AND object_kind = 'container'",
            params![account_id, generation_id],
        )?;
        transaction.execute(
            "UPDATE provider_reconciliation_runs SET containers_complete = 0
             WHERE account_id = ?1 AND generation_id = ?2",
            params![account_id, generation_id],
        )?;
        completeness.0 = 0;
    }
    if (completeness.0 == 1 && !batch.container_upserts.is_empty())
        || (completeness.1 == 1
            && (!batch.thread_upserts.is_empty()
                || !batch.message_upserts.is_empty()
                || !seen_remote_messages.is_empty()))
        || (completeness.2 == 1
            && (!batch.message_upserts.is_empty() || !seen_remote_messages.is_empty()))
    {
        return Err(StoreError::Conflict(
            "Provider reconciliation inventory is frozen after completeness".into(),
        ));
    }
    for container in &batch.container_upserts {
        mark_reconciliation_seen(
            transaction,
            account_id,
            generation_id,
            "container",
            container.identity.remote_container_id.as_str(),
        )?;
    }
    for thread in &batch.thread_upserts {
        mark_reconciliation_seen(
            transaction,
            account_id,
            generation_id,
            "thread",
            thread.identity.remote_thread_id.as_str(),
        )?;
    }
    for message in &batch.message_upserts {
        mark_reconciliation_seen(
            transaction,
            account_id,
            generation_id,
            "message",
            message.identity.remote_message_id.as_str(),
        )?;
    }
    for message in seen_remote_messages {
        if message.mux_account_id.as_str() != account_id {
            return Err(StoreError::Validation(
                "Provider reconciliation inventory crosses account scope".into(),
            ));
        }
        let existing = transaction
            .query_row(
                "SELECT reference.remote_thread_id
                 FROM provider_message_refs reference
                 JOIN messages message ON message.id = reference.message_id
                 WHERE reference.account_id = ?1
                   AND reference.remote_message_id = ?2
                   AND message.remote_deleted = 0
                   AND NOT EXISTS (
                     SELECT 1 FROM provider_tombstones tombstone
                     WHERE tombstone.account_id = reference.account_id
                       AND tombstone.object_kind = 'message'
                       AND tombstone.remote_id = reference.remote_message_id
                       AND tombstone.related_remote_id = ''
                   )",
                params![account_id, message.remote_message_id.as_str()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?;
        if let Some(remote_thread_id) = existing {
            mark_reconciliation_seen(
                transaction,
                account_id,
                generation_id,
                "message",
                message.remote_message_id.as_str(),
            )?;
            if let Some(remote_thread_id) = remote_thread_id {
                mark_reconciliation_seen(
                    transaction,
                    account_id,
                    generation_id,
                    "thread",
                    &remote_thread_id,
                )?;
            }
        }
    }

    for kind in complete_kinds {
        let column = reconciliation_complete_column(*kind);
        transaction.execute(
            &format!(
                "UPDATE provider_reconciliation_runs SET {column} = 1
                 WHERE account_id = ?1 AND generation_id = ?2"
            ),
            params![account_id, generation_id],
        )?;
        match kind {
            ReconciliationObjectKind::Containers => completeness.0 = 1,
            ReconciliationObjectKind::Threads => completeness.1 = 1,
            ReconciliationObjectKind::Messages => completeness.2 = 1,
        }
    }

    if sweep_kinds.is_empty() {
        return Ok(false);
    }

    for kind in sweep_kinds {
        let complete = match kind {
            ReconciliationObjectKind::Containers => completeness.0,
            ReconciliationObjectKind::Threads => completeness.1,
            ReconciliationObjectKind::Messages => completeness.2,
        };
        if complete != 1 {
            return Err(StoreError::Conflict(
                "Provider reconciliation cannot sweep an incomplete inventory".into(),
            ));
        }
    }

    let mut affected_threads = BTreeSet::new();
    let unseen_messages = if sweep_kinds.contains(&ReconciliationObjectKind::Messages) {
        unseen_reconciliation_ids(
            transaction,
            account_id,
            generation_id,
            "message",
            "provider_message_refs",
            "remote_message_id",
        )?
    } else {
        Vec::new()
    };
    for remote_message_id in unseen_messages {
        if let Some(thread_id) =
            local_thread_for_message(transaction, account_id, &remote_message_id)?
        {
            affected_threads.insert(thread_id);
        }
        let remote_thread_id = transaction
            .query_row(
                "SELECT remote_thread_id FROM provider_message_refs
                 WHERE account_id = ?1 AND remote_message_id = ?2",
                params![account_id, &remote_message_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let tombstone = ProviderTombstone {
            target: TombstoneTarget::Message {
                identity: RemoteMessageIdentity {
                    mux_account_id: batch.mux_account_id.clone(),
                    remote_message_id: crate::provider::RemoteMessageId::new(remote_message_id)
                        .map_err(|error| StoreError::Validation(error.to_string()))?,
                    remote_thread_id: remote_thread_id
                        .map(crate::provider::RemoteThreadId::new)
                        .transpose()
                        .map_err(|error| StoreError::Validation(error.to_string()))?,
                },
            },
            observed_at: batch.observed_at,
            cursor: Some(batch.cursor.value.clone()),
        };
        apply_tombstone(transaction, account_id, &tombstone)?;
    }
    let unseen_threads = if sweep_kinds.contains(&ReconciliationObjectKind::Threads) {
        unseen_reconciliation_ids(
            transaction,
            account_id,
            generation_id,
            "thread",
            "provider_thread_refs",
            "remote_thread_id",
        )?
    } else {
        Vec::new()
    };
    for remote_thread_id in unseen_threads {
        if let Some(thread_id) = transaction
            .query_row(
                "SELECT thread_id FROM provider_thread_refs
                 WHERE account_id = ?1 AND remote_thread_id = ?2",
                params![account_id, &remote_thread_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
        {
            affected_threads.insert(thread_id);
        }
        let tombstone = ProviderTombstone {
            target: TombstoneTarget::Thread {
                identity: RemoteThreadIdentity {
                    mux_account_id: batch.mux_account_id.clone(),
                    remote_thread_id: crate::provider::RemoteThreadId::new(remote_thread_id)
                        .map_err(|error| StoreError::Validation(error.to_string()))?,
                },
            },
            observed_at: batch.observed_at,
            cursor: Some(batch.cursor.value.clone()),
        };
        apply_tombstone(transaction, account_id, &tombstone)?;
    }
    let unseen_containers = if sweep_kinds.contains(&ReconciliationObjectKind::Containers) {
        unseen_reconciliation_ids(
            transaction,
            account_id,
            generation_id,
            "container",
            "provider_containers",
            "remote_id",
        )?
    } else {
        Vec::new()
    };
    for remote_container_id in unseen_containers {
        let tombstone = ProviderTombstone {
            target: TombstoneTarget::Container {
                identity: RemoteContainerIdentity {
                    mux_account_id: batch.mux_account_id.clone(),
                    remote_container_id: crate::provider::RemoteContainerId::new(
                        remote_container_id,
                    )
                    .map_err(|error| StoreError::Validation(error.to_string()))?,
                },
            },
            observed_at: batch.observed_at,
            cursor: Some(batch.cursor.value.clone()),
        };
        apply_tombstone(transaction, account_id, &tombstone)?;
    }
    for thread_id in affected_threads {
        derive_thread_projection(transaction, thread_id)?;
        rebuild_search_index_for_thread(transaction, thread_id)?;
    }

    let all_complete = completeness == (1, 1, 1);
    let remaining =
        !all_complete || reconciliation_has_unseen(transaction, account_id, generation_id)?;
    if !remaining {
        transaction.execute(
            "DELETE FROM provider_reconciliation_runs
             WHERE account_id = ?1 AND generation_id = ?2",
            params![account_id, generation_id],
        )?;
    }
    Ok(!remaining)
}

fn insert_reconciliation_run(
    transaction: &Transaction<'_>,
    account_id: &str,
    work_scope: &str,
    generation_id: &str,
    started_at: i64,
) -> Result<(), StoreError> {
    transaction.execute(
        "INSERT INTO provider_reconciliation_runs(
           account_id, scope, generation_id, started_at,
           containers_complete, threads_complete, messages_complete
         ) VALUES(?1, ?2, ?3, ?4, 0, 0, 0)",
        params![account_id, work_scope, generation_id, started_at],
    )?;
    Ok(())
}

fn reconciliation_complete_column(kind: ReconciliationObjectKind) -> &'static str {
    match kind {
        ReconciliationObjectKind::Containers => "containers_complete",
        ReconciliationObjectKind::Threads => "threads_complete",
        ReconciliationObjectKind::Messages => "messages_complete",
    }
}

fn mark_reconciliation_seen(
    transaction: &Transaction<'_>,
    account_id: &str,
    generation_id: &str,
    object_kind: &str,
    remote_id: &str,
) -> Result<(), StoreError> {
    transaction.execute(
        "INSERT INTO provider_reconciliation_seen(
           account_id, generation_id, object_kind, remote_id
         ) VALUES(?1, ?2, ?3, ?4)
         ON CONFLICT(account_id, generation_id, object_kind, remote_id) DO NOTHING",
        params![account_id, generation_id, object_kind, remote_id],
    )?;
    Ok(())
}

fn unseen_reconciliation_ids(
    transaction: &Transaction<'_>,
    account_id: &str,
    generation_id: &str,
    object_kind: &str,
    source_table: &str,
    id_column: &str,
) -> Result<Vec<String>, StoreError> {
    let statement = format!(
        "SELECT source.{id_column}
         FROM {source_table} source
         WHERE source.account_id = ?1
           AND NOT EXISTS (
             SELECT 1 FROM provider_reconciliation_seen seen
             WHERE seen.account_id = ?1 AND seen.generation_id = ?2
               AND seen.object_kind = ?3 AND seen.remote_id = source.{id_column}
           )
           AND NOT EXISTS (
             SELECT 1 FROM provider_tombstones tombstone
             WHERE tombstone.account_id = ?1 AND tombstone.object_kind = ?3
               AND tombstone.remote_id = source.{id_column}
               AND tombstone.related_remote_id = ''
           )
         ORDER BY source.{id_column}
         LIMIT {RECONCILIATION_SWEEP_PAGE}"
    );
    let mut statement = transaction.prepare(&statement)?;
    let values = statement
        .query_map(params![account_id, generation_id, object_kind], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::from)?;
    Ok(values)
}

fn reconciliation_has_unseen(
    transaction: &Transaction<'_>,
    account_id: &str,
    generation_id: &str,
) -> Result<bool, StoreError> {
    for (object_kind, source_table, id_column) in [
        ("message", "provider_message_refs", "remote_message_id"),
        ("thread", "provider_thread_refs", "remote_thread_id"),
        ("container", "provider_containers", "remote_id"),
    ] {
        if !unseen_reconciliation_ids(
            transaction,
            account_id,
            generation_id,
            object_kind,
            source_table,
            id_column,
        )?
        .is_empty()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn upsert_container(
    transaction: &Transaction<'_>,
    container: &ProviderContainer,
) -> Result<(), StoreError> {
    let account_id = container.identity.mux_account_id.as_str();
    let remote_id = container.identity.remote_container_id.as_str();
    transaction.execute(
        "INSERT INTO provider_containers(
           account_id, remote_id, name, kind, role, parent_remote_id,
           is_selectable, is_deleted
         ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)
         ON CONFLICT(account_id, remote_id) DO UPDATE SET
           name = excluded.name,
           kind = excluded.kind,
           role = excluded.role,
           parent_remote_id = excluded.parent_remote_id,
           is_selectable = excluded.is_selectable,
           is_deleted = 0",
        params![
            account_id,
            remote_id,
            container.display_name.as_str(),
            container_kind_key(container.kind),
            container_role_key(container.role),
            container
                .parent_remote_container_id
                .as_ref()
                .map(|identity| identity.as_str()),
            bool_i64(container.selectable)
        ],
    )?;
    transaction.execute(
        "DELETE FROM provider_tombstones
         WHERE account_id = ?1 AND object_kind = 'container'
           AND remote_id = ?2 AND related_remote_id = ''",
        params![account_id, remote_id],
    )?;
    Ok(())
}

fn upsert_thread(
    transaction: &Transaction<'_>,
    thread: &ProviderThreadUpsert,
) -> Result<i64, StoreError> {
    let account_id = thread.identity.mux_account_id.as_str();
    let remote_thread_id = thread.identity.remote_thread_id.as_str();
    let local_thread_id = transaction
        .query_row(
            "SELECT thread_id FROM provider_thread_refs
             WHERE account_id = ?1 AND remote_thread_id = ?2",
            params![account_id, remote_thread_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;

    let local_thread_id = if let Some(local_thread_id) = local_thread_id {
        transaction.execute(
            "UPDATE threads SET
               subject = ?1, participants = ?2, snippet = ?3, latest_at = ?4,
               message_count = ?5, remote_in_inbox = ?6, remote_unread = ?7,
               remote_starred = ?8, has_attachment = ?9, has_invite = ?10,
               has_link = ?11, has_from_me = ?12, category = ?13,
               remote_deleted = 0
             WHERE id = ?14 AND account_id = ?15",
            params![
                thread.subject.as_str(),
                thread.participants.as_str(),
                thread.snippet.as_str(),
                thread.latest_at.get(),
                i64::from(thread.message_count.get()),
                bool_i64(thread.in_inbox),
                bool_i64(thread.unread),
                bool_i64(thread.starred),
                bool_i64(thread.has_attachments),
                bool_i64(thread.has_invite),
                bool_i64(thread.has_links),
                bool_i64(thread.has_from_me),
                thread.category.as_str(),
                local_thread_id,
                account_id
            ],
        )?;
        local_thread_id
    } else {
        transaction.execute(
            "INSERT INTO threads(
               account_id, subject, participants, snippet, latest_at, message_count,
               remote_in_inbox, remote_unread, remote_starred, has_attachment,
               has_invite, has_link, has_from_me, category, attachment_names,
               remote_deleted
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                      ?11, ?12, ?13, ?14, '', 0)",
            params![
                account_id,
                thread.subject.as_str(),
                thread.participants.as_str(),
                thread.snippet.as_str(),
                thread.latest_at.get(),
                i64::from(thread.message_count.get()),
                bool_i64(thread.in_inbox),
                bool_i64(thread.unread),
                bool_i64(thread.starred),
                bool_i64(thread.has_attachments),
                bool_i64(thread.has_invite),
                bool_i64(thread.has_links),
                bool_i64(thread.has_from_me),
                thread.category.as_str()
            ],
        )?;
        let local_thread_id = transaction.last_insert_rowid();
        transaction.execute(
            "INSERT INTO provider_thread_refs(
               account_id, remote_thread_id, thread_id, revision
             ) VALUES(?1, ?2, ?3, ?4)",
            params![
                account_id,
                remote_thread_id,
                local_thread_id,
                thread.revision.as_ref().map(|value| value.as_str())
            ],
        )?;
        local_thread_id
    };

    transaction.execute(
        "UPDATE provider_thread_refs SET revision = ?3
         WHERE account_id = ?1 AND remote_thread_id = ?2",
        params![
            account_id,
            remote_thread_id,
            thread.revision.as_ref().map(|value| value.as_str())
        ],
    )?;
    transaction.execute(
        "DELETE FROM provider_tombstones
         WHERE account_id = ?1 AND object_kind = 'thread'
           AND remote_id = ?2 AND related_remote_id = ''",
        params![account_id, remote_thread_id],
    )?;
    Ok(local_thread_id)
}

fn upsert_message(
    transaction: &Transaction<'_>,
    message: &ProviderMessageUpsert,
) -> Result<(i64, i64), StoreError> {
    let account_id = message.identity.mux_account_id.as_str();
    let remote_message_id = message.identity.remote_message_id.as_str();
    let requested_remote_thread = message
        .identity
        .remote_thread_id
        .as_ref()
        .map(|identity| identity.as_str());
    let existing = transaction
        .query_row(
            "SELECT provider_message_refs.message_id, messages.thread_id,
                    provider_message_refs.remote_thread_id,
                    provider_message_refs.body_state,
                    provider_message_refs.body_is_truncated
             FROM provider_message_refs
             JOIN messages ON messages.id = provider_message_refs.message_id
             WHERE provider_message_refs.account_id = ?1
               AND provider_message_refs.remote_message_id = ?2",
            params![account_id, remote_message_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()?;

    let effective_remote_thread = requested_remote_thread.or_else(|| {
        existing
            .as_ref()
            .and_then(|(_, _, remote_thread_id, _, _)| remote_thread_id.as_deref())
    });
    let mapped_thread_id = match effective_remote_thread {
        Some(remote_thread_id) => Some(
            transaction
                .query_row(
                    "SELECT thread_id FROM provider_thread_refs
                     WHERE account_id = ?1 AND remote_thread_id = ?2",
                    params![account_id, remote_thread_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
                .ok_or_else(|| {
                    StoreError::Validation(
                        "Provider message references an unknown remote thread".into(),
                    )
                })?,
        ),
        None => existing.as_ref().map(|(_, thread_id, _, _, _)| *thread_id),
    };

    let local_thread_id = match mapped_thread_id {
        Some(local_thread_id) => local_thread_id,
        None => create_standalone_thread(transaction, message)?,
    };
    let should_replace_body =
        existing
            .as_ref()
            .is_none_or(|(_, _, _, body_state, body_is_truncated)| {
                should_replace_body(message.body_state, body_state, *body_is_truncated != 0)
            });
    let body_state = body_state_key(message.body_state);
    let body_is_truncated = bool_i64(message.body_state == ProviderBodyState::Truncated);
    let references_json = message
        .references
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;

    let local_message_id = if let Some((local_message_id, existing_thread_id, _, _, _)) = existing {
        if existing_thread_id != local_thread_id {
            transaction.execute(
                "UPDATE provider_message_refs SET remote_thread_id = NULL
                 WHERE account_id = ?1 AND remote_message_id = ?2",
                params![account_id, remote_message_id],
            )?;
        }
        transaction.execute(
            "UPDATE messages SET
               thread_id = ?1, sender_name = ?2, sender_email = ?3, recipients = ?4,
               cc_recipients = ?5, bcc_recipients = ?6, sent_at = ?7,
               body_text = CASE WHEN ?9 = 1 THEN ?8 ELSE body_text END,
               is_from_me = ?10, remote_deleted = 0,
               internet_message_id = COALESCE(?12, internet_message_id),
               in_reply_to = COALESCE(?13, in_reply_to),
               references_json = COALESCE(?14, references_json),
               provider_subject = ?15
             WHERE id = ?11",
            params![
                local_thread_id,
                message.sender_name.as_str(),
                message.sender_email.as_str(),
                message.recipients.as_str(),
                message.cc_recipients.as_str(),
                message.bcc_recipients.as_str(),
                message.sent_at.get(),
                message.body_text.as_str(),
                bool_i64(should_replace_body),
                bool_i64(message.is_from_me),
                local_message_id,
                message.internet_message_id.as_deref(),
                message.in_reply_to.as_deref(),
                references_json.as_deref(),
                message.subject.as_str(),
            ],
        )?;
        transaction.execute(
            "UPDATE provider_message_refs SET
               remote_thread_id = ?3,
               revision = ?4,
               body_state = CASE WHEN ?6 = 1 THEN ?5 ELSE body_state END,
               body_is_truncated = CASE
                 WHEN ?6 = 1 THEN ?7 ELSE body_is_truncated END
             WHERE account_id = ?1 AND remote_message_id = ?2",
            params![
                account_id,
                remote_message_id,
                effective_remote_thread,
                message.revision.as_ref().map(|value| value.as_str()),
                body_state,
                bool_i64(should_replace_body),
                body_is_truncated
            ],
        )?;
        local_message_id
    } else {
        transaction.execute(
            "INSERT INTO messages(
               thread_id, sender_name, sender_email, recipients, cc_recipients,
               bcc_recipients, sent_at, body_text, body_html,
               blocked_remote_resources, is_from_me, remote_deleted,
               internet_message_id, in_reply_to, references_json, provider_subject
             ) VALUES(
               ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, '', 0, ?9, 0, ?10, ?11, ?12, ?13
             )",
            params![
                local_thread_id,
                message.sender_name.as_str(),
                message.sender_email.as_str(),
                message.recipients.as_str(),
                message.cc_recipients.as_str(),
                message.bcc_recipients.as_str(),
                message.sent_at.get(),
                message.body_text.as_str(),
                bool_i64(message.is_from_me),
                message.internet_message_id.as_deref().unwrap_or_default(),
                message.in_reply_to.as_deref().unwrap_or_default(),
                references_json.as_deref().unwrap_or("[]"),
                message.subject.as_str(),
            ],
        )?;
        let local_message_id = transaction.last_insert_rowid();
        transaction.execute(
            "INSERT INTO provider_message_refs(
               account_id, remote_message_id, message_id, remote_thread_id,
               revision, body_state, body_is_truncated
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                account_id,
                remote_message_id,
                local_message_id,
                requested_remote_thread,
                message.revision.as_ref().map(|value| value.as_str()),
                body_state,
                body_is_truncated
            ],
        )?;
        local_message_id
    };

    transaction.execute(
        "DELETE FROM provider_message_keywords
         WHERE account_id = ?1 AND remote_message_id = ?2",
        params![account_id, remote_message_id],
    )?;
    for keyword in &message.keywords {
        transaction.execute(
            "INSERT INTO provider_message_keywords(account_id, remote_message_id, keyword)
             VALUES(?1, ?2, ?3)",
            params![account_id, remote_message_id, keyword.as_str()],
        )?;
    }
    transaction.execute(
        "DELETE FROM provider_tombstones
         WHERE account_id = ?1 AND object_kind = 'message'
           AND remote_id = ?2 AND related_remote_id = ''",
        params![account_id, remote_message_id],
    )?;
    Ok((local_message_id, local_thread_id))
}

fn local_thread_for_message(
    transaction: &Transaction<'_>,
    account_id: &str,
    remote_message_id: &str,
) -> Result<Option<i64>, StoreError> {
    transaction
        .query_row(
            "SELECT messages.thread_id
             FROM provider_message_refs
             JOIN messages ON messages.id = provider_message_refs.message_id
             WHERE provider_message_refs.account_id = ?1
               AND provider_message_refs.remote_message_id = ?2",
            params![account_id, remote_message_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(StoreError::from)
}

pub(crate) fn derive_thread_projection(
    transaction: &Transaction<'_>,
    thread_id: i64,
) -> Result<(), StoreError> {
    transaction.execute(
        "UPDATE threads SET
           subject = COALESCE((
             SELECT NULLIF(message.provider_subject, '')
             FROM messages message
             WHERE message.thread_id = ?1 AND message.remote_deleted = 0
             ORDER BY message.sent_at DESC, message.id DESC LIMIT 1
           ), subject),
           participants = COALESCE((
             SELECT CASE
               WHEN message.sender_name = '' THEN message.sender_email
               ELSE message.sender_name || ' <' || message.sender_email || '>'
             END
             FROM messages message
             WHERE message.thread_id = ?1 AND message.remote_deleted = 0
             ORDER BY message.sent_at DESC, message.id DESC LIMIT 1
           ), participants),
           snippet = COALESCE((
             SELECT substr(replace(message.body_text, char(10), ' '), 1, 2000)
             FROM messages message
             WHERE message.thread_id = ?1 AND message.remote_deleted = 0
             ORDER BY message.sent_at DESC, message.id DESC LIMIT 1
           ), ''),
           latest_at = COALESCE((
             SELECT MAX(message.sent_at) FROM messages message
             WHERE message.thread_id = ?1 AND message.remote_deleted = 0
           ), latest_at),
           message_count = (
             SELECT COUNT(*) FROM messages message
             WHERE message.thread_id = ?1 AND message.remote_deleted = 0
           ),
           remote_in_inbox = EXISTS(
             SELECT 1
             FROM provider_message_refs reference
             JOIN provider_container_memberships membership
               ON membership.account_id = reference.account_id
              AND membership.remote_message_id = reference.remote_message_id
             JOIN provider_containers container
               ON container.account_id = membership.account_id
              AND container.remote_id = membership.remote_container_id
             JOIN messages message ON message.id = reference.message_id
             WHERE message.thread_id = ?1 AND message.remote_deleted = 0
               AND container.role = 'inbox' AND container.is_deleted = 0
           ),
           remote_unread = EXISTS(
             SELECT 1
             FROM provider_message_refs reference
             JOIN provider_message_keywords keyword
               ON keyword.account_id = reference.account_id
              AND keyword.remote_message_id = reference.remote_message_id
             JOIN messages message ON message.id = reference.message_id
             WHERE message.thread_id = ?1 AND message.remote_deleted = 0
               AND keyword.keyword = 'unread'
           ),
           remote_starred = EXISTS(
             SELECT 1
             FROM provider_message_refs reference
             JOIN provider_message_keywords keyword
               ON keyword.account_id = reference.account_id
              AND keyword.remote_message_id = reference.remote_message_id
             JOIN messages message ON message.id = reference.message_id
             WHERE message.thread_id = ?1 AND message.remote_deleted = 0
               AND keyword.keyword = 'starred'
           ),
           remote_trashed = EXISTS(
             SELECT 1
             FROM provider_message_refs reference
             JOIN provider_container_memberships membership
               ON membership.account_id = reference.account_id
              AND membership.remote_message_id = reference.remote_message_id
             JOIN provider_containers container
               ON container.account_id = membership.account_id
              AND container.remote_id = membership.remote_container_id
             JOIN messages message ON message.id = reference.message_id
             WHERE message.thread_id = ?1 AND message.remote_deleted = 0
               AND container.role = 'trash' AND container.is_deleted = 0
           ),
           has_attachment = EXISTS(
             SELECT 1
             FROM provider_message_refs reference
             JOIN provider_message_keywords keyword
               ON keyword.account_id = reference.account_id
              AND keyword.remote_message_id = reference.remote_message_id
             JOIN messages message ON message.id = reference.message_id
             WHERE message.thread_id = ?1 AND message.remote_deleted = 0
               AND keyword.keyword = 'has_attachment'
           ),
           has_invite = EXISTS(
             SELECT 1
             FROM provider_message_refs reference
             JOIN provider_message_keywords keyword
               ON keyword.account_id = reference.account_id
              AND keyword.remote_message_id = reference.remote_message_id
             JOIN messages message ON message.id = reference.message_id
             WHERE message.thread_id = ?1 AND message.remote_deleted = 0
               AND keyword.keyword = 'has_invite'
           ),
           has_link = EXISTS(
             SELECT 1
             FROM provider_message_refs reference
             JOIN provider_message_keywords keyword
               ON keyword.account_id = reference.account_id
              AND keyword.remote_message_id = reference.remote_message_id
             JOIN messages message ON message.id = reference.message_id
             WHERE message.thread_id = ?1 AND message.remote_deleted = 0
               AND keyword.keyword = 'has_link'
           ),
           has_from_me = EXISTS(
             SELECT 1 FROM messages message
             WHERE message.thread_id = ?1 AND message.remote_deleted = 0
               AND message.is_from_me = 1
           ),
           remote_deleted = CASE WHEN EXISTS(
             SELECT 1 FROM messages message
             WHERE message.thread_id = ?1 AND message.remote_deleted = 0
           ) THEN 0 ELSE 1 END
         WHERE id = ?1",
        [thread_id],
    )?;
    Ok(())
}

fn affected_threads_for_tombstone(
    transaction: &Transaction<'_>,
    account_id: &str,
    tombstone: &crate::provider::ProviderTombstone,
) -> Result<Vec<i64>, StoreError> {
    let thread_id = match &tombstone.target {
        TombstoneTarget::Thread { identity } => transaction
            .query_row(
                "SELECT thread_id FROM provider_thread_refs
                 WHERE account_id = ?1 AND remote_thread_id = ?2",
                params![account_id, identity.remote_thread_id.as_str()],
                |row| row.get(0),
            )
            .optional()?,
        TombstoneTarget::Message { identity } => {
            local_thread_for_message(transaction, account_id, identity.remote_message_id.as_str())?
        }
        TombstoneTarget::Container { .. } | TombstoneTarget::Membership { .. } => None,
    };
    Ok(thread_id.into_iter().collect())
}

pub(crate) fn rebuild_search_index_for_thread(
    transaction: &Transaction<'_>,
    thread_id: i64,
) -> Result<(), StoreError> {
    transaction.execute("DELETE FROM messages_fts WHERE thread_id = ?1", [thread_id])?;
    transaction.execute(
        "INSERT INTO messages_fts(
           thread_id, subject, participants, body, attachment_names
         )
         SELECT t.id, t.subject, t.participants,
                COALESCE((
                  SELECT group_concat(ordered_messages.body_text, char(10))
                  FROM (
                    SELECT body_text
                    FROM messages
                    WHERE thread_id = ?1 AND remote_deleted = 0
                    ORDER BY sent_at, id
                  ) ordered_messages
                ), ''),
                t.attachment_names
         FROM threads t
         WHERE t.id = ?1 AND t.remote_deleted = 0",
        [thread_id],
    )?;
    Ok(())
}

pub(crate) fn rebuild_all_search_indexes(transaction: &Transaction<'_>) -> Result<(), StoreError> {
    transaction.execute("DELETE FROM messages_fts", [])?;
    // A migration must rebuild in one indexed pass. Calling the per-thread
    // helper here repeatedly makes each FTS DELETE scan the growing virtual
    // table and turns a populated mailbox migration quadratic.
    transaction.execute(
        "INSERT INTO messages_fts(
           thread_id, subject, participants, body, attachment_names
         )
         SELECT t.id, t.subject, t.participants,
                COALESCE(ordered_messages.body, ''),
                t.attachment_names
         FROM threads t
         LEFT JOIN (
           SELECT live_messages.thread_id,
                  group_concat(live_messages.body_text, char(10)) AS body
           FROM (
             SELECT thread_id, body_text
             FROM messages
             WHERE remote_deleted = 0
             ORDER BY thread_id, sent_at, id
           ) live_messages
           GROUP BY live_messages.thread_id
         ) ordered_messages ON ordered_messages.thread_id = t.id
         WHERE t.remote_deleted = 0
         ORDER BY t.id",
        [],
    )?;
    Ok(())
}

fn create_standalone_thread(
    transaction: &Transaction<'_>,
    message: &ProviderMessageUpsert,
) -> Result<i64, StoreError> {
    let participants = if message.recipients.as_str().is_empty() {
        message.sender_email.as_str()
    } else {
        message.recipients.as_str()
    };
    let snippet = message
        .body_text
        .as_str()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(180)
        .collect::<String>();
    transaction.execute(
        "INSERT INTO threads(
           account_id, subject, participants, snippet, latest_at, message_count,
           remote_in_inbox, remote_unread, remote_starred, has_attachment,
           has_invite, has_link, has_from_me, category, attachment_names,
           remote_deleted
         ) VALUES(?1, ?2, ?3, ?4, ?5, 1, 0, ?6, ?7, 0, 0, 0, ?8, '', '', 0)",
        params![
            message.identity.mux_account_id.as_str(),
            message.subject.as_str(),
            participants,
            snippet,
            message.sent_at.get(),
            bool_i64(has_keyword(message, "unread")),
            bool_i64(has_keyword(message, "starred")),
            bool_i64(message.is_from_me)
        ],
    )?;
    Ok(transaction.last_insert_rowid())
}

fn apply_membership_change(
    transaction: &Transaction<'_>,
    change: &ContainerMembershipChange,
) -> Result<(), StoreError> {
    let membership = change.membership();
    validate_message_thread_hint(transaction, &membership.message, true)?;
    let account_id = membership.message.mux_account_id.as_str();
    let message_id = membership.message.remote_message_id.as_str();
    let container_id = membership.container.remote_container_id.as_str();
    match change {
        ContainerMembershipChange::Upsert { .. } => {
            transaction.execute(
                "INSERT INTO provider_container_memberships(
                   account_id, remote_message_id, remote_container_id
                 ) VALUES(?1, ?2, ?3)
                 ON CONFLICT(account_id, remote_message_id, remote_container_id) DO NOTHING",
                params![account_id, message_id, container_id],
            )?;
            transaction.execute(
                "DELETE FROM provider_tombstones
                 WHERE account_id = ?1 AND object_kind = 'membership'
                   AND remote_id = ?2 AND related_remote_id = ?3",
                params![account_id, message_id, container_id],
            )?;
        }
        ContainerMembershipChange::Remove { .. } => {
            transaction.execute(
                "DELETE FROM provider_container_memberships
                 WHERE account_id = ?1 AND remote_message_id = ?2
                   AND remote_container_id = ?3",
                params![account_id, message_id, container_id],
            )?;
        }
    }
    Ok(())
}

fn apply_tombstone(
    transaction: &Transaction<'_>,
    account_id: &str,
    tombstone: &crate::provider::ProviderTombstone,
) -> Result<(), StoreError> {
    let (kind, remote_id, related_remote_id) = match &tombstone.target {
        TombstoneTarget::Thread { identity } => {
            if let Some(local_thread_id) = transaction
                .query_row(
                    "SELECT thread_id FROM provider_thread_refs
                     WHERE account_id = ?1 AND remote_thread_id = ?2",
                    params![account_id, identity.remote_thread_id.as_str()],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
            {
                transaction.execute(
                    "UPDATE threads SET
                       remote_deleted = 1, remote_in_inbox = 0,
                       remote_unread = 0, remote_starred = 0
                     WHERE id = ?1",
                    [local_thread_id],
                )?;
                transaction.execute(
                    "UPDATE messages SET remote_deleted = 1
                     WHERE id IN (
                       SELECT message_id FROM provider_message_refs
                       WHERE account_id = ?1 AND remote_thread_id = ?2
                     )",
                    params![account_id, identity.remote_thread_id.as_str()],
                )?;
                transaction.execute(
                    "DELETE FROM provider_container_memberships
                     WHERE account_id = ?1 AND remote_message_id IN (
                       SELECT remote_message_id FROM provider_message_refs
                       WHERE account_id = ?1 AND remote_thread_id = ?2
                     )",
                    params![account_id, identity.remote_thread_id.as_str()],
                )?;
                transaction.execute(
                    "DELETE FROM provider_message_keywords
                     WHERE account_id = ?1 AND remote_message_id IN (
                       SELECT remote_message_id FROM provider_message_refs
                       WHERE account_id = ?1 AND remote_thread_id = ?2
                     )",
                    params![account_id, identity.remote_thread_id.as_str()],
                )?;
            }
            ("thread", identity.remote_thread_id.as_str(), "")
        }
        TombstoneTarget::Message { identity } => {
            validate_message_thread_hint(transaction, identity, false)?;
            if let Some(local_message_id) = transaction
                .query_row(
                    "SELECT message_id FROM provider_message_refs
                     WHERE account_id = ?1 AND remote_message_id = ?2",
                    params![account_id, identity.remote_message_id.as_str()],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
            {
                transaction.execute(
                    "UPDATE messages SET remote_deleted = 1 WHERE id = ?1",
                    [local_message_id],
                )?;
            }
            transaction.execute(
                "DELETE FROM provider_container_memberships
                 WHERE account_id = ?1 AND remote_message_id = ?2",
                params![account_id, identity.remote_message_id.as_str()],
            )?;
            transaction.execute(
                "DELETE FROM provider_message_keywords
                 WHERE account_id = ?1 AND remote_message_id = ?2",
                params![account_id, identity.remote_message_id.as_str()],
            )?;
            ("message", identity.remote_message_id.as_str(), "")
        }
        TombstoneTarget::Container { identity } => {
            transaction.execute(
                "UPDATE provider_containers SET is_deleted = 1
                 WHERE account_id = ?1 AND remote_id = ?2",
                params![account_id, identity.remote_container_id.as_str()],
            )?;
            transaction.execute(
                "DELETE FROM provider_container_memberships
                 WHERE account_id = ?1 AND remote_container_id = ?2",
                params![account_id, identity.remote_container_id.as_str()],
            )?;
            ("container", identity.remote_container_id.as_str(), "")
        }
        TombstoneTarget::Membership { message, container } => {
            validate_message_thread_hint(transaction, message, false)?;
            transaction.execute(
                "DELETE FROM provider_container_memberships
                 WHERE account_id = ?1 AND remote_message_id = ?2
                   AND remote_container_id = ?3",
                params![
                    account_id,
                    message.remote_message_id.as_str(),
                    container.remote_container_id.as_str()
                ],
            )?;
            (
                "membership",
                message.remote_message_id.as_str(),
                container.remote_container_id.as_str(),
            )
        }
    };
    transaction.execute(
        "INSERT INTO provider_tombstones(
           account_id, object_kind, remote_id, related_remote_id,
           cursor, deleted_at
         ) VALUES(?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(account_id, object_kind, remote_id, related_remote_id)
         DO UPDATE SET cursor = excluded.cursor, deleted_at = excluded.deleted_at",
        params![
            account_id,
            kind,
            remote_id,
            related_remote_id,
            tombstone.cursor.as_ref().map(|value| value.as_str()),
            tombstone.observed_at.get()
        ],
    )?;
    Ok(())
}

fn validate_message_thread_hint(
    transaction: &Transaction<'_>,
    identity: &RemoteMessageIdentity,
    require_message: bool,
) -> Result<(), StoreError> {
    let mapped_thread = transaction
        .query_row(
            "SELECT remote_thread_id FROM provider_message_refs
             WHERE account_id = ?1 AND remote_message_id = ?2",
            params![
                identity.mux_account_id.as_str(),
                identity.remote_message_id.as_str()
            ],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?;
    let Some(mapped_thread) = mapped_thread else {
        if require_message {
            return Err(StoreError::Validation(
                "Provider membership references an unknown remote message".into(),
            ));
        }
        return Ok(());
    };
    if let Some(expected_thread) = &identity.remote_thread_id {
        if mapped_thread.as_deref() != Some(expected_thread.as_str()) {
            return Err(StoreError::Validation(
                "Provider message thread hint conflicts with its durable mapping".into(),
            ));
        }
    }
    Ok(())
}

fn cursor_scope_key(scope: &SyncCursorScope) -> String {
    match scope {
        SyncCursorScope::Account => "a:v1".into(),
        SyncCursorScope::Container {
            remote_container_id,
        } => format!("c:v1:{}", remote_container_id.as_str()),
    }
}

fn container_kind_key(kind: ContainerKind) -> &'static str {
    match kind {
        ContainerKind::Label => "label",
        ContainerKind::Folder => "folder",
        ContainerKind::Mailbox => "mailbox",
        ContainerKind::RetrievalState => "retrieval_state",
    }
}

fn container_role_key(role: Option<ContainerRole>) -> &'static str {
    match role {
        Some(ContainerRole::Inbox) => "inbox",
        Some(ContainerRole::Archive) => "archive",
        Some(ContainerRole::AllMail) => "all_mail",
        Some(ContainerRole::Drafts) => "drafts",
        Some(ContainerRole::Sent) => "sent",
        Some(ContainerRole::Trash) => "trash",
        Some(ContainerRole::Spam) => "spam",
        Some(ContainerRole::Starred) => "starred",
        Some(ContainerRole::Important) => "important",
        None => "custom",
    }
}

fn body_state_key(state: ProviderBodyState) -> &'static str {
    match state {
        ProviderBodyState::Complete | ProviderBodyState::Truncated => "normalized",
        ProviderBodyState::Unavailable => "unavailable",
    }
}

fn should_replace_body(
    incoming: ProviderBodyState,
    durable_state: &str,
    durable_is_truncated: bool,
) -> bool {
    match incoming {
        ProviderBodyState::Complete => true,
        ProviderBodyState::Truncated => durable_state != "normalized" || durable_is_truncated,
        ProviderBodyState::Unavailable => false,
    }
}

fn has_keyword(message: &ProviderMessageUpsert, expected: &str) -> bool {
    message
        .keywords
        .iter()
        .any(|keyword| keyword.as_str() == expected)
}

fn bool_i64(value: bool) -> i64 {
    i64::from(value)
}

#[cfg(test)]
fn fingerprint_v1(batch: &ProviderBatch) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash_text(&mut hash, "mux-provider-batch-fingerprint-v1");
    hash_text(&mut hash, batch.mux_account_id.as_str());
    hash_text(&mut hash, batch.batch_id.as_str());
    hash_text(&mut hash, &cursor_scope_key(&batch.cursor.scope));
    hash_text(&mut hash, batch.cursor.value.as_str());
    hash_i64(&mut hash, batch.observed_at.get());
    hash_batch_collections(&mut hash, batch);
    hash.finalize().into()
}

fn fingerprint_v2(batch: &ProviderBatch) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash_text(&mut hash, "mux-provider-batch-fingerprint-v2");
    hash_text(&mut hash, batch.mux_account_id.as_str());
    hash_text(&mut hash, batch.batch_id.as_str());
    hash_option_text(
        &mut hash,
        batch
            .expected_prior_cursor
            .as_ref()
            .map(|value| value.as_str()),
    );
    hash_text(&mut hash, &cursor_scope_key(&batch.cursor.scope));
    hash_text(&mut hash, batch.cursor.value.as_str());
    hash_i64(&mut hash, batch.observed_at.get());
    hash_batch_collections(&mut hash, batch);
    hash.finalize().into()
}

fn hash_batch_collections(hash: &mut Sha256, batch: &ProviderBatch) {
    let mut containers = batch.container_upserts.iter().collect::<Vec<_>>();
    containers.sort_by_key(|item| item.identity.remote_container_id.as_str());
    hash_u64(hash, containers.len() as u64);
    for container in containers {
        hash_text(hash, container.identity.remote_container_id.as_str());
        hash_text(hash, container.display_name.as_str());
        hash_text(hash, container_kind_key(container.kind));
        hash_text(hash, container_role_key(container.role));
        hash_option_text(
            hash,
            container
                .parent_remote_container_id
                .as_ref()
                .map(|value| value.as_str()),
        );
        hash_bool(hash, container.selectable);
    }

    let mut threads = batch.thread_upserts.iter().collect::<Vec<_>>();
    threads.sort_by_key(|item| item.identity.remote_thread_id.as_str());
    hash_u64(hash, threads.len() as u64);
    for thread in threads {
        hash_text(hash, thread.identity.remote_thread_id.as_str());
        hash_text(hash, thread.subject.as_str());
        hash_text(hash, thread.participants.as_str());
        hash_text(hash, thread.snippet.as_str());
        hash_i64(hash, thread.latest_at.get());
        hash_u64(hash, u64::from(thread.message_count.get()));
        for value in [
            thread.in_inbox,
            thread.unread,
            thread.starred,
            thread.has_attachments,
            thread.has_invite,
            thread.has_links,
            thread.has_from_me,
        ] {
            hash_bool(hash, value);
        }
        hash_text(hash, thread.category.as_str());
        hash_option_text(hash, thread.revision.as_ref().map(|value| value.as_str()));
    }

    let mut messages = batch.message_upserts.iter().collect::<Vec<_>>();
    messages.sort_by_key(|item| item.identity.remote_message_id.as_str());
    hash_u64(hash, messages.len() as u64);
    for message in messages {
        hash_text(hash, message.identity.remote_message_id.as_str());
        hash_option_text(
            hash,
            message
                .identity
                .remote_thread_id
                .as_ref()
                .map(|value| value.as_str()),
        );
        hash_text(hash, message.subject.as_str());
        hash_text(hash, message.sender_name.as_str());
        hash_text(hash, message.sender_email.as_str());
        hash_text(hash, message.recipients.as_str());
        hash_text(hash, message.cc_recipients.as_str());
        hash_text(hash, message.bcc_recipients.as_str());
        hash_i64(hash, message.sent_at.get());
        hash_text(hash, message.body_text.as_str());
        hash_text(hash, body_state_fingerprint_key(message.body_state));
        hash_bool(hash, message.is_from_me);
        hash_option_text(hash, message.revision.as_ref().map(|value| value.as_str()));
        hash_u64(hash, message.keywords.len() as u64);
        for keyword in &message.keywords {
            hash_text(hash, keyword.as_str());
        }
        if let Some(message_id) = message.internet_message_id.as_deref() {
            hash_text(hash, "internet-message-id");
            hash_text(hash, message_id);
        }
        if let Some(in_reply_to) = message.in_reply_to.as_deref() {
            hash_text(hash, "in-reply-to");
            hash_text(hash, in_reply_to);
        }
        if let Some(references) = message.references.as_ref() {
            hash_text(hash, "references");
            hash_u64(hash, references.len() as u64);
            for reference in references {
                hash_text(hash, reference);
            }
        }
    }

    let mut memberships = batch.membership_changes.iter().collect::<Vec<_>>();
    memberships.sort_by_key(|change| {
        let membership = change.membership();
        (
            membership.message.remote_message_id.as_str(),
            membership.container.remote_container_id.as_str(),
        )
    });
    hash_u64(hash, memberships.len() as u64);
    for change in memberships {
        hash_text(
            hash,
            match change {
                ContainerMembershipChange::Upsert { .. } => "upsert",
                ContainerMembershipChange::Remove { .. } => "remove",
            },
        );
        let membership = change.membership();
        hash_text(hash, membership.message.remote_message_id.as_str());
        hash_option_text(
            hash,
            membership
                .message
                .remote_thread_id
                .as_ref()
                .map(|value| value.as_str()),
        );
        hash_text(hash, membership.container.remote_container_id.as_str());
    }

    let mut tombstones = batch.tombstones.iter().collect::<Vec<_>>();
    tombstones.sort_by_key(|tombstone| tombstone_sort_key(&tombstone.target));
    hash_u64(hash, tombstones.len() as u64);
    for tombstone in tombstones {
        let (kind, remote_id, related_id) = tombstone_sort_key(&tombstone.target);
        hash_text(hash, kind);
        hash_text(hash, remote_id);
        hash_text(hash, related_id);
        if let TombstoneTarget::Message { identity }
        | TombstoneTarget::Membership {
            message: identity, ..
        } = &tombstone.target
        {
            hash_option_text(
                hash,
                identity
                    .remote_thread_id
                    .as_ref()
                    .map(|value| value.as_str()),
            );
        } else {
            hash_option_text(hash, None);
        }
        hash_i64(hash, tombstone.observed_at.get());
        hash_option_text(hash, tombstone.cursor.as_ref().map(|value| value.as_str()));
    }
}

fn tombstone_sort_key(target: &TombstoneTarget) -> (&'static str, &str, &str) {
    match target {
        TombstoneTarget::Thread { identity } => ("thread", identity.remote_thread_id.as_str(), ""),
        TombstoneTarget::Message { identity } => {
            ("message", identity.remote_message_id.as_str(), "")
        }
        TombstoneTarget::Container { identity } => {
            ("container", identity.remote_container_id.as_str(), "")
        }
        TombstoneTarget::Membership { message, container } => (
            "membership",
            message.remote_message_id.as_str(),
            container.remote_container_id.as_str(),
        ),
    }
}

fn body_state_fingerprint_key(state: ProviderBodyState) -> &'static str {
    match state {
        ProviderBodyState::Complete => "complete",
        ProviderBodyState::Truncated => "truncated",
        ProviderBodyState::Unavailable => "unavailable",
    }
}

fn hash_text(hash: &mut Sha256, value: &str) {
    hash_u64(hash, value.len() as u64);
    hash.update(value.as_bytes());
}

fn hash_option_text(hash: &mut Sha256, value: Option<&str>) {
    match value {
        Some(value) => {
            hash.update([1]);
            hash_text(hash, value);
        }
        None => hash.update([0]),
    }
}

fn hash_bool(hash: &mut Sha256, value: bool) {
    hash.update([u8::from(value)]);
}

fn hash_i64(hash: &mut Sha256, value: i64) {
    hash.update(value.to_be_bytes());
}

fn hash_u64(hash: &mut Sha256, value: u64) {
    hash.update(value.to_be_bytes());
}

#[cfg(test)]
pub(crate) fn provider_batch_fingerprint_v1(batch: &ProviderBatch) -> [u8; 32] {
    fingerprint_v1(batch)
}

#[cfg(test)]
pub(crate) fn provider_batch_fingerprint_v2(batch: &ProviderBatch) -> [u8; 32] {
    fingerprint_v2(batch)
}

#[cfg(test)]
mod reconciliation_tests {
    use rusqlite::Connection;
    use tempfile::tempdir;

    use super::*;
    use crate::store::MuxStore;

    fn empty_batch() -> ProviderBatch {
        serde_json::from_value(serde_json::json!({
            "muxAccountId": "account-a",
            "batchId": "bounded-reconciliation-page",
            "cursor": {
                "muxAccountId": "account-a",
                "scope": { "kind": "account" },
                "value": "bounded-reconciliation-cursor"
            },
            "observedAt": 10,
            "threadUpserts": [],
            "messageUpserts": [],
            "containerUpserts": [],
            "membershipChanges": [],
            "tombstones": []
        }))
        .expect("empty provider batch")
    }

    #[test]
    fn gmail_provider_conformance_reconciliation_sweeps_more_than_one_thousand_in_two_passes() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("bounded-reconciliation.db");
        drop(MuxStore::open(&path, false).expect("native schema"));
        let mut connection = Connection::open(&path).expect("fixture connection");
        connection
            .execute(
                "INSERT INTO accounts(id, name, email, color, provider)
                 VALUES('account-a', 'Account A', 'a@example.test', '#000000', 'gmail')",
                [],
            )
            .expect("account fixture");
        connection
            .execute(
                "INSERT INTO provider_accounts(
                   account_id, provider_kind, remote_account_id, auth_state,
                   credential_ref, sync_state, created_at, updated_at
                 ) VALUES(
                   'account-a', 'gmail', 'a@example.test', 'ready',
                   'gmail/account-a', 'scheduled', 0, 0
                 )",
                [],
            )
            .expect("provider account fixture");
        {
            let transaction = connection.transaction().expect("container transaction");
            for index in 0..1_001 {
                transaction
                    .execute(
                        "INSERT INTO provider_containers(
                           account_id, remote_id, name, kind, role
                         ) VALUES('account-a', ?1, ?1, 'label', 'custom')",
                        [format!("Label_{index:04}")],
                    )
                    .expect("container fixture");
            }
            transaction.commit().expect("container commit");
        }
        let batch = empty_batch();
        let none = BTreeSet::new();
        let all = [
            ReconciliationObjectKind::Containers,
            ReconciliationObjectKind::Threads,
            ReconciliationObjectKind::Messages,
        ]
        .into_iter()
        .collect::<BTreeSet<_>>();
        {
            let transaction = connection.transaction().expect("begin transaction");
            assert!(!apply_reconciliation_page_in_transaction(
                &transaction,
                &batch,
                "sync:gmail:account:v1",
                "generation-a",
                true,
                false,
                &none,
                &none,
                &[],
            )
            .expect("begin reconciliation"));
            transaction.commit().expect("begin commit");
        }
        {
            let transaction = connection.transaction().expect("first sweep transaction");
            assert!(!apply_reconciliation_page_in_transaction(
                &transaction,
                &batch,
                "sync:gmail:account:v1",
                "generation-a",
                false,
                false,
                &all,
                &all,
                &[],
            )
            .expect("first bounded sweep"));
            transaction.commit().expect("first sweep commit");
        }
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM provider_tombstones
                     WHERE account_id = 'account-a' AND object_kind = 'container'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("first tombstone count"),
            RECONCILIATION_SWEEP_PAGE as i64
        );
        {
            let transaction = connection.transaction().expect("second sweep transaction");
            assert!(apply_reconciliation_page_in_transaction(
                &transaction,
                &batch,
                "sync:gmail:account:v1",
                "generation-a",
                false,
                false,
                &all,
                &all,
                &[],
            )
            .expect("second bounded sweep"));
            transaction.commit().expect("second sweep commit");
        }
        let result: (i64, i64) = connection
            .query_row(
                "SELECT
                   (SELECT COUNT(*) FROM provider_tombstones
                    WHERE account_id = 'account-a' AND object_kind = 'container'),
                   (SELECT COUNT(*) FROM provider_reconciliation_runs
                    WHERE account_id = 'account-a')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("final reconciliation state");
        assert_eq!(result, (1_001, 0));
    }
}
