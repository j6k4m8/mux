use rusqlite::{params, OptionalExtension, Transaction};

use crate::ipc_boundary::{ensure_serialized_budget, IpcPayloadKind};
use crate::provider::{
    ProviderBodyState, ProviderCapabilities, ProviderCapability, SyncCursorScope,
};
use crate::provider_ingest::{
    apply_provider_batch_in_transaction, apply_provider_batch_with_options_in_transaction,
    apply_reconciliation_page_in_transaction, ProviderBatchFailpoint,
};
use crate::store::StoreError;
use crate::worker::{
    enqueue_in_transaction, ClaimedWork, NewWorkItem, ProviderSyncPage, WorkKind, WorkerError,
    WorkerProjection,
};

/// Production projector shared by the native worker controller.
///
/// The current projection matrix is intentionally closed: sync work applies one normalized
/// provider batch; mutation and send work apply their exact linked local-operation snapshot.
/// Provider adapters may not substitute a cursor-only batch for a mutation or non-idempotent send.
/// Projection, cursor, replay receipt, local operation, and fenced worker success all use the
/// transaction supplied by the durable worker, so a crash or rejection cannot commit partially.
pub(crate) fn apply_worker_projection(
    transaction: &Transaction<'_>,
    work: &ClaimedWork,
    projection: &WorkerProjection,
) -> Result<(), WorkerError> {
    match (work.kind, projection) {
        (WorkKind::Mutation | WorkKind::Send, WorkerProjection::LocalOperation) => {
            crate::store::apply_worker_projection(transaction, work, projection)
        }
        (WorkKind::Mutation, WorkerProjection::RemoteThreadAbsent { remote_thread_id }) => {
            crate::store::apply_missing_remote_thread_projection(
                transaction,
                work,
                remote_thread_id,
            )
        }
        (WorkKind::Send | WorkKind::Sync, WorkerProjection::ProviderSendAcceptance(acceptance)) => {
            crate::store::apply_provider_send_acceptance(transaction, work, acceptance.as_ref())
        }
        (WorkKind::Sync, WorkerProjection::ProviderBatch(batch)) => {
            if batch.mux_account_id.as_str() != work.account_id {
                return Err(WorkerError::Conflict(
                    "Provider batch account does not match the claimed work account".into(),
                ));
            }
            if batch.batch_id.as_str() != work.ordering_key {
                return Err(WorkerError::Conflict(
                    "Provider sync batch ID does not match the claimed ordering identity".into(),
                ));
            }
            ensure_serialized_budget(batch.as_ref(), IpcPayloadKind::ProviderBatch)
                .map_err(|error| WorkerError::Conflict(error.to_string()))?;
            apply_provider_batch_in_transaction(
                transaction,
                batch.as_ref().clone(),
                ProviderBatchFailpoint::None,
            )
            .map(|_| ())
            .map_err(worker_projection_error)
        }
        (WorkKind::Sync, WorkerProjection::ProviderSyncPage(page)) => {
            apply_provider_sync_page(transaction, work, page)
        }
        (
            WorkKind::Sync,
            WorkerProjection::LocalOperation | WorkerProjection::RemoteThreadAbsent { .. },
        ) => Err(WorkerError::Conflict(
            "Provider sync work requires a normalized provider batch".into(),
        )),
        (
            WorkKind::Mutation,
            WorkerProjection::ProviderBatch(_)
            | WorkerProjection::ProviderSyncPage(_)
            | WorkerProjection::ProviderSendAcceptance(_),
        ) => Err(WorkerError::Conflict(
            "Provider mutation work requires its exact linked local operation".into(),
        )),
        (
            WorkKind::Send,
            WorkerProjection::RemoteThreadAbsent { .. }
            | WorkerProjection::ProviderBatch(_)
            | WorkerProjection::ProviderSyncPage(_),
        ) => Err(WorkerError::Conflict(
            "Provider send work requires its exact immutable send snapshot".into(),
        )),
    }
}

fn apply_provider_sync_page(
    transaction: &Transaction<'_>,
    work: &ClaimedWork,
    page: &ProviderSyncPage,
) -> Result<(), WorkerError> {
    const MAX_RECONCILIATION_SEEN_MESSAGES: usize = 1_000;
    if page.complete == page.continuation.is_some() {
        return Err(WorkerError::Conflict(
            "Provider sync page must be final or contain exactly one continuation".into(),
        ));
    }
    if page.reconciliation.is_some() && page.complete {
        return Err(WorkerError::Conflict(
            "Provider reconciliation pages require a bounded continuation".into(),
        ));
    }
    if page.reconciliation.as_ref().is_some_and(|reconciliation| {
        reconciliation.seen_remote_messages.len() > MAX_RECONCILIATION_SEEN_MESSAGES
    }) {
        return Err(WorkerError::Conflict(
            "Provider reconciliation inventory exceeds its bounded page".into(),
        ));
    }
    let batch = page.batch.as_ref();
    if batch.mux_account_id.as_str() != work.account_id {
        return Err(WorkerError::Conflict(
            "Provider batch account does not match the claimed work account".into(),
        ));
    }
    if batch.batch_id.as_str() != work.ordering_key {
        return Err(WorkerError::Conflict(
            "Provider sync batch ID does not match the claimed ordering identity".into(),
        ));
    }
    if let Some(reconciliation) = &page.reconciliation {
        let account_only = reconciliation.begin
            || reconciliation.reset_seen_containers
            || !reconciliation.complete_kinds.is_empty()
            || !reconciliation.sweep_kinds.is_empty();
        let inventory_only_container =
            matches!(batch.cursor.scope, SyncCursorScope::Container { .. }) && !account_only;
        if batch.cursor.scope != SyncCursorScope::Account && !inventory_only_container {
            return Err(WorkerError::Conflict(
                "Provider reconciliation lifecycle changes require an account-scoped cursor".into(),
            ));
        }
    }
    let inventory = page
        .reconciliation
        .as_ref()
        .map(|reconciliation| reconciliation.seen_remote_messages.as_slice())
        .unwrap_or_default();
    let mut restricted_identities = std::collections::BTreeSet::new();
    let mut restricted_replacements = Vec::with_capacity(page.restricted_message_content.len());
    for content in &page.restricted_message_content {
        if content.identity.mux_account_id.as_str() != work.account_id {
            return Err(WorkerError::Conflict(
                "Restricted message content account does not match the claimed work account".into(),
            ));
        }
        if !restricted_identities.insert((
            content.identity.mux_account_id.as_str(),
            content.identity.remote_message_id.as_str(),
        )) {
            return Err(WorkerError::Conflict(
                "Restricted message content contains a duplicate message identity".into(),
            ));
        }
        let message = batch
            .message_upserts
            .iter()
            .find(|message| message.identity == content.identity)
            .ok_or_else(|| {
                WorkerError::Conflict(
                    "Restricted message content is not paired with a provider message upsert"
                        .into(),
                )
            })?;
        if message.body_state == ProviderBodyState::Unavailable {
            return Err(WorkerError::Conflict(
                "Unavailable provider bodies cannot carry restricted render content".into(),
            ));
        }
        crate::content::validate_remote_images(
            &content.body_html,
            content.blocked_remote_resources,
            &content.remote_images,
        )
        .map_err(WorkerError::Conflict)?;
        let durable = transaction
            .query_row(
                "SELECT body_state, body_is_truncated
                 FROM provider_message_refs
                 WHERE account_id = ?1 AND remote_message_id = ?2",
                params![
                    content.identity.mux_account_id.as_str(),
                    content.identity.remote_message_id.as_str()
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        let should_replace = durable.is_none_or(|(state, truncated)| match message.body_state {
            ProviderBodyState::Complete => true,
            ProviderBodyState::Truncated => state != "normalized" || truncated != 0,
            ProviderBodyState::Unavailable => false,
        });
        restricted_replacements.push((content, should_replace));
    }
    ensure_serialized_budget(
        &(batch, inventory, &page.restricted_message_content),
        IpcPayloadKind::ProviderBatch,
    )
    .map_err(|error| WorkerError::Conflict(error.to_string()))?;
    let mut seen_inventory = std::collections::BTreeSet::new();
    for message in inventory {
        if message.mux_account_id.as_str() != work.account_id {
            return Err(WorkerError::Conflict(
                "Provider reconciliation inventory account does not match the claimed work account"
                    .into(),
            ));
        }
        if !seen_inventory.insert(message.remote_message_id.as_str()) {
            return Err(WorkerError::Conflict(
                "Provider reconciliation inventory contains a duplicate message identity".into(),
            ));
        }
    }
    let applied = apply_provider_batch_with_options_in_transaction(
        transaction,
        batch.clone(),
        ProviderBatchFailpoint::None,
        page.replace_memberships_for_upserted_messages,
        page.derive_thread_state_from_messages,
    )
    .map_err(worker_projection_error)?;
    if !applied.applied {
        return Err(WorkerError::Conflict(
            "Provider sync page batch was already applied outside this claimed work".into(),
        ));
    }
    for (content, should_replace) in restricted_replacements {
        if !should_replace {
            continue;
        }
        let changed = transaction.execute(
            "UPDATE messages
             SET body_html = ?3, blocked_remote_resources = ?4
             WHERE id = (
               SELECT message_id FROM provider_message_refs
               WHERE account_id = ?1 AND remote_message_id = ?2
             )",
            params![
                content.identity.mux_account_id.as_str(),
                content.identity.remote_message_id.as_str(),
                &content.body_html,
                content.blocked_remote_resources
            ],
        )?;
        if changed != 1 {
            return Err(WorkerError::Conflict(
                "Restricted message content did not resolve to its projected message".into(),
            ));
        }
        let message_id = transaction.query_row(
            "SELECT message_id FROM provider_message_refs
             WHERE account_id = ?1 AND remote_message_id = ?2",
            params![
                content.identity.mux_account_id.as_str(),
                content.identity.remote_message_id.as_str()
            ],
            |row| row.get::<_, i64>(0),
        )?;
        transaction.execute(
            "DELETE FROM message_remote_images WHERE message_id = ?1",
            [message_id],
        )?;
        for image in &content.remote_images {
            transaction.execute(
                "INSERT INTO message_remote_images(
                   message_id, resource_id, url, domain, alt_text
                 ) VALUES(?1, ?2, ?3, ?4, ?5)",
                params![
                    message_id,
                    image.resource_id,
                    &image.url,
                    &image.domain,
                    &image.alt_text
                ],
            )?;
        }
    }

    let reconciliation_complete = if let Some(reconciliation) = &page.reconciliation {
        apply_reconciliation_page_in_transaction(
            transaction,
            batch,
            &work.scope,
            &reconciliation.generation_id,
            reconciliation.begin,
            reconciliation.reset_seen_containers,
            &reconciliation.complete_kinds,
            &reconciliation.sweep_kinds,
            &reconciliation.seen_remote_messages,
        )
        .map_err(worker_projection_error)?
    } else {
        false
    };
    let effective_complete = page.complete || reconciliation_complete;

    if let Some(capabilities) = &page.capabilities {
        replace_capabilities(transaction, &work.account_id, capabilities)?;
    }

    if !effective_complete {
        let continuation = page
            .continuation
            .as_ref()
            .ok_or_else(|| WorkerError::Conflict("Provider sync continuation is missing".into()))?;
        enqueue_in_transaction(
            transaction,
            NewWorkItem {
                id: continuation.id.clone(),
                account_id: work.account_id.clone(),
                operation_id: None,
                kind: WorkKind::Sync,
                scope: work.scope.clone(),
                ordering_key: continuation.ordering_key.clone(),
                payload_json: continuation.payload_json.clone(),
                priority: continuation.priority,
                available_at: continuation.available_at,
                max_attempts: continuation.max_attempts,
            },
            batch.observed_at.get(),
        )?;
    }

    transaction.execute(
        "UPDATE provider_accounts
         SET sync_state = ?2,
             last_sync_at = CASE WHEN ?3 = 1 THEN ?4 ELSE last_sync_at END,
             last_error_code = NULL,
             updated_at = ?4
         WHERE account_id = ?1",
        params![
            &work.account_id,
            if effective_complete {
                "idle"
            } else {
                "scheduled"
            },
            i64::from(effective_complete),
            batch.observed_at.get()
        ],
    )?;
    Ok(())
}

fn replace_capabilities(
    transaction: &Transaction<'_>,
    account_id: &str,
    capabilities: &ProviderCapabilities,
) -> Result<(), WorkerError> {
    transaction.execute(
        "DELETE FROM provider_capabilities WHERE account_id = ?1",
        [account_id],
    )?;
    for capability in &capabilities.supported {
        transaction.execute(
            "INSERT INTO provider_capabilities(account_id, capability, enabled)
             VALUES(?1, ?2, 1)",
            params![account_id, capability_key(*capability)],
        )?;
    }
    Ok(())
}

fn capability_key(capability: ProviderCapability) -> &'static str {
    match capability {
        ProviderCapability::DeltaSync => "delta_sync",
        ProviderCapability::RemoteThreads => "remote_threads",
        ProviderCapability::MultiContainerMembership => "multi_container_membership",
        ProviderCapability::RemoteDrafts => "remote_drafts",
        ProviderCapability::Mutations => "mutations",
        ProviderCapability::OutgoingMail => "outgoing_mail",
        ProviderCapability::AttachmentFetch => "attachment_fetch",
        ProviderCapability::ServerSearch => "server_search",
    }
}

fn worker_projection_error(error: StoreError) -> WorkerError {
    match error {
        StoreError::Sqlite(error) => WorkerError::Sqlite(error),
        error => WorkerError::Conflict(format!("Provider projection was rejected: {error}")),
    }
}

/// Compile-time boundary for future live adapter contract tests. Enabling the feature alone never
/// authorizes network or mailbox access: a provider-specific harness must call this guard and the
/// operator must opt in explicitly through process state. No credential values are accepted here.
#[cfg(feature = "live-provider-contract")]
#[allow(dead_code)] // Consumed only after a real provider-specific live harness is registered.
pub(crate) fn require_live_provider_contract_opt_in() -> Result<(), &'static str> {
    match std::env::var("MUX_RUN_LIVE_PROVIDER_CONTRACT").as_deref() {
        Ok("1") => Ok(()),
        _ => Err(
            "live provider contracts are disabled; set MUX_RUN_LIVE_PROVIDER_CONTRACT=1 explicitly",
        ),
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;

    use rusqlite::{params, Connection, OptionalExtension};
    use tempfile::tempdir;

    use super::*;
    use crate::content::RemoteImageCandidate;
    use crate::provider::{
        MuxAccountId, ProviderBatch, RemoteContainerId, RemoteMessageId, RemoteMessageIdentity,
        RemoteThreadId,
    };
    use crate::store::MuxStore;
    use crate::worker::{
        DurableWorker, NewWorkItem, ProviderReconciliationPage, ProviderSyncContinuation,
        ProviderSyncPage, ReconciliationObjectKind, RestrictedMessageContent, WorkState,
        WorkerAdapter, WorkerConfig, WorkerExecutionContext, WorkerOutcome,
    };

    #[cfg(feature = "live-provider-contract")]
    #[test]
    fn live_provider_feature_still_requires_explicit_process_opt_in() {
        if std::env::var("MUX_RUN_LIVE_PROVIDER_CONTRACT").as_deref() == Ok("1") {
            assert!(require_live_provider_contract_opt_in().is_ok());
        } else {
            assert!(require_live_provider_contract_opt_in().is_err());
        }
    }

    struct OfflineBatchAdapter;

    impl WorkerAdapter for OfflineBatchAdapter {
        fn execute(&self, work: &ClaimedWork, _context: &WorkerExecutionContext) -> WorkerOutcome {
            match serde_json::from_str::<ProviderBatch>(&work.payload_json) {
                Ok(batch) => WorkerOutcome::Succeeded {
                    projection: WorkerProjection::ProviderBatch(Box::new(batch)),
                },
                Err(_) => WorkerOutcome::PermanentFailure {
                    code: "invalid_provider_batch".into(),
                },
            }
        }
    }

    fn test_config() -> WorkerConfig {
        WorkerConfig {
            max_in_flight: 3,
            max_per_account: 2,
            lease_ms: 1_000,
            base_backoff_ms: 100,
            max_backoff_ms: 10_000,
            jitter_seed: 17,
            idle_poll_ms: 60_000,
            heartbeat_ms: 200,
        }
    }

    fn configured_worker(accounts: &[&str]) -> (tempfile::TempDir, PathBuf, DurableWorker) {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("provider-conformance.db");
        drop(MuxStore::open(&path, false).expect("native store schema"));
        let connection = Connection::open(&path).expect("provider fixture connection");
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .expect("foreign keys enabled");
        for account in accounts {
            connection
                .execute(
                    "INSERT INTO accounts(id, name, email, color, provider)
                     VALUES(?1, ?1, ?2, '#000000', 'jmap')",
                    params![account, format!("{account}@example.com")],
                )
                .expect("account fixture");
            connection
                .execute(
                    "INSERT INTO provider_accounts(
                       account_id, provider_kind, remote_account_id, auth_state,
                       sync_state, credential_ref, created_at, updated_at
                     ) VALUES(?1, 'jmap', ?1, 'ready', 'scheduled', ?2, 0, 0)",
                    params![account, format!("vault:v1:{account}")],
                )
                .expect("provider account fixture");
        }
        drop(connection);
        let worker = DurableWorker::new(&path, test_config()).expect("durable worker");
        (directory, path, worker)
    }

    fn batch(
        account_id: &str,
        batch_id: &str,
        expected_prior_cursor: Option<&str>,
        cursor: &str,
        observed_at: i64,
    ) -> ProviderBatch {
        serde_json::from_value(serde_json::json!({
            "muxAccountId": account_id,
            "batchId": batch_id,
            "expectedPriorCursor": expected_prior_cursor,
            "cursor": {
                "muxAccountId": account_id,
                "scope": { "kind": "account" },
                "value": cursor
            },
            "observedAt": observed_at,
            "threadUpserts": [],
            "messageUpserts": [],
            "containerUpserts": [],
            "membershipChanges": [],
            "tombstones": []
        }))
        .expect("valid provider batch")
    }

    fn gmail_data_batch(
        account_id: &str,
        batch_id: &str,
        expected_prior_cursor: Option<&str>,
        cursor: &str,
    ) -> ProviderBatch {
        serde_json::from_value(serde_json::json!({
            "muxAccountId": account_id,
            "batchId": batch_id,
            "expectedPriorCursor": expected_prior_cursor,
            "cursor": {
                "muxAccountId": account_id,
                "scope": { "kind": "account" },
                "value": cursor
            },
            "observedAt": 10,
            "threadUpserts": [{
                "identity": { "muxAccountId": account_id, "remoteThreadId": "gmail-thread" },
                "subject": "Confirmed subject",
                "participants": "Sender <sender@example.test>",
                "snippet": "Confirmed snippet",
                "latestAt": 10,
                "messageCount": 1,
                "inInbox": true,
                "unread": false,
                "starred": false,
                "hasAttachments": false,
                "hasInvite": false,
                "hasLinks": false,
                "hasFromMe": false,
                "category": "",
                "revision": "thread-r1"
            }],
            "messageUpserts": [{
                "identity": {
                    "muxAccountId": account_id,
                    "remoteMessageId": "gmail-message",
                    "remoteThreadId": "gmail-thread"
                },
                "subject": "Confirmed subject",
                "senderName": "Sender",
                "senderEmail": "sender@example.test",
                "recipients": "reader@example.test",
                "ccRecipients": "",
                "bccRecipients": "",
                "sentAt": 10,
                "bodyText": "Confirmed body",
                "bodyState": "complete",
                "isFromMe": false,
                "revision": "message-r1",
                "keywords": []
            }],
            "containerUpserts": [{
                "identity": { "muxAccountId": account_id, "remoteContainerId": "INBOX" },
                "displayName": "Inbox",
                "kind": "label",
                "role": "inbox",
                "parentRemoteContainerId": null,
                "selectable": true
            }],
            "membershipChanges": [{
                "kind": "upsert",
                "membership": {
                    "message": {
                        "muxAccountId": account_id,
                        "remoteMessageId": "gmail-message",
                        "remoteThreadId": "gmail-thread"
                    },
                    "container": {
                        "muxAccountId": account_id,
                        "remoteContainerId": "INBOX"
                    }
                }
            }],
            "tombstones": []
        }))
        .expect("valid Gmail projection fixture")
    }

    fn container_only_batch(
        account_id: &str,
        batch_id: &str,
        expected_prior_cursor: Option<&str>,
        cursor: &str,
    ) -> ProviderBatch {
        serde_json::from_value(serde_json::json!({
            "muxAccountId": account_id,
            "batchId": batch_id,
            "expectedPriorCursor": expected_prior_cursor,
            "cursor": {
                "muxAccountId": account_id,
                "scope": { "kind": "account" },
                "value": cursor
            },
            "observedAt": 50,
            "threadUpserts": [],
            "messageUpserts": [],
            "containerUpserts": [{
                "identity": { "muxAccountId": account_id, "remoteContainerId": "INBOX" },
                "displayName": "Inbox",
                "kind": "label",
                "role": "inbox",
                "parentRemoteContainerId": null,
                "selectable": true
            }],
            "membershipChanges": [],
            "tombstones": []
        }))
        .expect("valid container-only provider batch")
    }

    fn batch_work(id: &str, account_id: &str, batch: &ProviderBatch) -> NewWorkItem {
        NewWorkItem {
            id: id.into(),
            account_id: account_id.into(),
            operation_id: None,
            kind: WorkKind::Sync,
            scope: "sync:account".into(),
            ordering_key: batch.batch_id.as_str().into(),
            payload_json: serde_json::to_string(batch).expect("serialize provider batch"),
            priority: 0,
            available_at: 0,
            max_attempts: 4,
        }
    }

    fn oversized_batch(account_id: &str) -> ProviderBatch {
        large_batch(account_id, 17, "oversized-batch", "oversized-cursor")
    }

    fn large_batch(
        account_id: &str,
        message_count: usize,
        batch_id: &str,
        cursor: &str,
    ) -> ProviderBatch {
        let body = "x".repeat(2 * 1024 * 1024);
        let messages = (0..message_count)
            .map(|index| {
                serde_json::json!({
                    "identity": {
                        "muxAccountId": account_id,
                        "remoteMessageId": format!("large-message-{index}"),
                        "remoteThreadId": "large-thread"
                    },
                    "subject": "Aggregate budget fixture",
                    "senderName": "Provider Sender",
                    "senderEmail": "sender@example.com",
                    "recipients": "recipient@example.com",
                    "ccRecipients": "",
                    "bccRecipients": "",
                    "sentAt": 100,
                    "bodyText": body,
                    "bodyState": "complete",
                    "isFromMe": false,
                    "revision": format!("message-r{index}"),
                    "keywords": []
                })
            })
            .collect::<Vec<_>>();
        serde_json::from_value(serde_json::json!({
            "muxAccountId": account_id,
            "batchId": batch_id,
            "expectedPriorCursor": null,
            "cursor": {
                "muxAccountId": account_id,
                "scope": { "kind": "account" },
                "value": cursor
            },
            "observedAt": 100,
            "threadUpserts": [{
                "identity": {
                    "muxAccountId": account_id,
                    "remoteThreadId": "large-thread"
                },
                "subject": "Aggregate budget fixture",
                "participants": "sender@example.com, recipient@example.com",
                "snippet": "Bounded provider batch",
                "latestAt": 100,
                "messageCount": message_count,
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
            "messageUpserts": messages,
            "containerUpserts": [],
            "membershipChanges": [],
            "tombstones": []
        }))
        .expect("valid item-bounded provider batch")
    }

    fn item(id: &str, account_id: &str, kind: WorkKind) -> NewWorkItem {
        NewWorkItem {
            id: id.into(),
            account_id: account_id.into(),
            operation_id: None,
            kind,
            scope: format!("contract:{id}"),
            ordering_key: format!("contract-order:{id}"),
            payload_json: "{}".into(),
            priority: 0,
            available_at: 0,
            max_attempts: 4,
        }
    }

    fn insert_linked_operation(path: &std::path::Path, id: &str, field: &str, account_id: &str) {
        let connection = Connection::open(path).expect("linked operation connection");
        let (thread_id, payload) = if field == "send" {
            (None, serde_json::json!({ "accountId": account_id }))
        } else {
            connection
                .execute(
                    "INSERT INTO threads(
                       account_id, subject, participants, snippet, latest_at, message_count,
                       remote_in_inbox, remote_unread, remote_starred, has_attachment,
                       has_invite, has_link, has_from_me
                     ) VALUES(?1, 'Contract', '', '', 0, 0, 1, 0, 0, 0, 0, 0, 0)",
                    [account_id],
                )
                .expect("linked operation thread");
            (Some(connection.last_insert_rowid()), serde_json::json!({}))
        };
        connection
            .execute(
                "INSERT INTO operations(
                   id, thread_id, field, kind, old_value, new_value, payload_json,
                   state, created_at, not_before, attempts
                 ) VALUES(?1, ?2, ?3, 'provider_contract', NULL, NULL, ?4,
                          'pending', 0, 0, 0)",
                params![id, thread_id, field, payload.to_string()],
            )
            .expect("linked operation fixture");
    }

    fn operation_state(path: &std::path::Path, id: &str) -> (String, Option<String>) {
        Connection::open(path)
            .expect("operation verification connection")
            .query_row(
                "SELECT state, error FROM operations WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("linked operation state")
    }

    fn insert_draft(path: &std::path::Path, id: &str, account_id: &str) {
        Connection::open(path)
            .expect("draft fixture connection")
            .execute(
                "INSERT INTO drafts(
                   id, account_id, recipients, subject, body, body_html, updated_at, revision
                 ) VALUES(?1, ?2, 'recipient@example.com', 'Unsent draft',
                          'must remain durable', '<p>must remain durable</p>', 0, 1)",
                params![id, account_id],
            )
            .expect("draft fixture");
    }

    fn assert_no_provider_projection(path: &std::path::Path, account_id: &str) {
        let verify = Connection::open(path).expect("projection verification connection");
        assert_eq!(cursor(&verify, account_id), None);
        assert_eq!(
            verify
                .query_row("SELECT COUNT(*) FROM provider_applied_batches", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("receipt count"),
            0
        );
    }

    fn cursor(connection: &Connection, account_id: &str) -> Option<String> {
        connection
            .query_row(
                "SELECT cursor FROM provider_sync_cursors
                 WHERE account_id = ?1 AND scope = 'a:v1'",
                [account_id],
                |row| row.get(0),
            )
            .optional()
            .expect("cursor query")
    }

    /// Reusable adapter-level exercise: an implementation that can consume the normalized
    /// offline fixture must project through the same production worker boundary as a future real
    /// adapter. Provider-specific test modules can call this with their own `WorkerAdapter`.
    pub(crate) fn assert_offline_batch_adapter_contract<A: WorkerAdapter + Sync>(adapter: &A) {
        let (_directory, path, worker) = configured_worker(&["account-a"]);
        let initial = batch("account-a", "batch-1", None, "cursor-1", 100);
        worker
            .enqueue(batch_work("initial", "account-a", &initial), 0)
            .expect("enqueue initial batch");
        let initial_result = worker
            .run_cycle("contract-worker", adapter, &apply_worker_projection, &|| 1)
            .expect("initial worker cycle");
        assert_eq!(initial_result.succeeded, 1);
        assert!(initial_result.errors.is_empty());

        let replay = batch("account-a", "batch-2", Some("cursor-1"), "cursor-2", 101);
        MuxStore::open(&path, false)
            .expect("replay fixture store")
            .apply_provider_batch(replay.clone())
            .expect("pre-apply the exact replay fixture");
        worker
            .enqueue(batch_work("replay", "account-a", &replay), 2)
            .expect("enqueue exact replay");
        let replay_result = worker
            .run_cycle("contract-worker", adapter, &apply_worker_projection, &|| 3)
            .expect("replay worker cycle");
        assert_eq!(replay_result.succeeded, 1);
        assert!(replay_result.errors.is_empty());

        let verify = Connection::open(path).expect("verification connection");
        assert_eq!(cursor(&verify, "account-a").as_deref(), Some("cursor-2"));
        assert_eq!(
            verify
                .query_row(
                    "SELECT COUNT(*) FROM provider_applied_batches
                     WHERE account_id = 'account-a'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("receipt count"),
            2,
            "exact replay is a no-op and cannot duplicate its durable receipt"
        );
        for id in ["initial", "replay"] {
            assert_eq!(
                worker
                    .snapshot(id)
                    .expect("work snapshot")
                    .expect("work row")
                    .state,
                WorkState::Succeeded
            );
        }
    }

    #[test]
    fn offline_provider_adapter_contract_commits_and_exactly_replays() {
        assert_offline_batch_adapter_contract(&OfflineBatchAdapter);
    }

    #[test]
    fn production_projector_rejects_changed_batch_identity_and_stale_prior_cursor_atomically() {
        let (_directory, path, worker) = configured_worker(&["account-a"]);
        let initial = batch("account-a", "batch-1", None, "cursor-1", 100);
        worker
            .enqueue(batch_work("initial", "account-a", &initial), 0)
            .expect("enqueue initial batch");
        assert_eq!(
            worker
                .run_cycle(
                    "contract-worker",
                    &OfflineBatchAdapter,
                    &apply_worker_projection,
                    &|| 1,
                )
                .expect("initial cycle")
                .succeeded,
            1
        );

        let receipt_fixture = batch("account-a", "batch-2", Some("cursor-1"), "cursor-2", 101);
        MuxStore::open(&path, false)
            .expect("changed identity fixture store")
            .apply_provider_batch(receipt_fixture)
            .expect("pre-apply changed identity fixture");
        let changed_identity = batch(
            "account-a",
            "batch-2",
            Some("cursor-2"),
            "cursor-changed",
            102,
        );
        worker
            .enqueue(
                batch_work("changed-identity", "account-a", &changed_identity),
                2,
            )
            .expect("enqueue changed identity");
        let changed_result = worker
            .run_cycle(
                "contract-worker",
                &OfflineBatchAdapter,
                &apply_worker_projection,
                &|| 3,
            )
            .expect("changed identity cycle");
        assert_eq!(changed_result.succeeded, 0);
        assert_eq!(changed_result.errors.len(), 1);

        let stale_prior = batch(
            "account-a",
            "batch-3",
            Some("stale-cursor"),
            "cursor-3",
            103,
        );
        let stale_claim = {
            let mut stale_work = batch_work("stale-prior", "account-a", &stale_prior);
            stale_work.scope = "sync:stale-prior".into();
            worker.enqueue(stale_work, 4).expect("enqueue stale prior");
            worker
                .claim_available(5, "stale-worker", 1)
                .expect("claim stale prior")
                .pop()
                .expect("stale prior claim")
        };
        let stale_error = worker
            .acknowledge_success_with_projection(&stale_claim, 6, |transaction| {
                apply_worker_projection(
                    transaction,
                    &stale_claim,
                    &WorkerProjection::ProviderBatch(Box::new(stale_prior.clone())),
                )
            })
            .expect_err("stale prior cursor must be rejected");
        assert!(matches!(stale_error, WorkerError::Conflict(_)));

        let verify = Connection::open(path).expect("verification connection");
        assert_eq!(cursor(&verify, "account-a").as_deref(), Some("cursor-2"));
        assert_eq!(
            verify
                .query_row("SELECT COUNT(*) FROM provider_applied_batches", [], |row| {
                    row.get::<_, i64>(0)
                },)
                .expect("receipt count"),
            2,
            "rejected batches cannot commit a cursor or receipt"
        );
        assert_eq!(
            worker
                .snapshot("changed-identity")
                .expect("changed snapshot")
                .expect("changed row")
                .state,
            WorkState::Executing,
            "a direct cycle leaves deterministic acknowledgement rejection fenced for manager fallback or lease recovery"
        );
        assert_eq!(
            worker
                .snapshot("stale-prior")
                .expect("stale snapshot")
                .expect("stale row")
                .state,
            WorkState::Executing
        );
    }

    #[test]
    fn production_projector_binds_batch_account_and_sync_identity_to_the_claim() {
        let (_directory, path, worker) = configured_worker(&["account-a", "account-b"]);
        let wrong_account = batch("account-b", "wrong-account", None, "cursor-b", 100);
        let mut work = batch_work("wrong-account-work", "account-a", &wrong_account);
        work.ordering_key = "wrong-account".into();
        worker
            .enqueue(work, 0)
            .expect("enqueue wrong account batch");
        let result = worker
            .run_cycle(
                "contract-worker",
                &OfflineBatchAdapter,
                &apply_worker_projection,
                &|| 1,
            )
            .expect("wrong account cycle");
        assert_eq!(result.succeeded, 0);
        assert_eq!(result.errors.len(), 1);

        let wrong_identity = batch("account-a", "actual-batch", None, "cursor-a", 101);
        let mut work = batch_work("wrong-identity-work", "account-a", &wrong_identity);
        work.scope = "sync:other".into();
        work.ordering_key = "claimed-batch".into();
        worker
            .enqueue(work, 2)
            .expect("enqueue wrong identity batch");
        let result = worker
            .run_cycle(
                "contract-worker",
                &OfflineBatchAdapter,
                &apply_worker_projection,
                &|| 3,
            )
            .expect("wrong identity cycle");
        assert_eq!(result.succeeded, 0);
        assert_eq!(result.errors.len(), 1);

        let verify = Connection::open(path).expect("verification connection");
        assert_eq!(cursor(&verify, "account-a"), None);
        assert_eq!(cursor(&verify, "account-b"), None);
        assert_eq!(
            verify
                .query_row("SELECT COUNT(*) FROM provider_applied_batches", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("receipt count"),
            0
        );
    }

    #[test]
    fn send_cannot_confirm_from_a_provider_batch_or_delete_its_draft() {
        let (_directory, path, worker) = configured_worker(&["account-a"]);
        insert_draft(&path, "draft-send", "account-a");
        insert_linked_operation(&path, "send-operation", "send", "account-a");
        let mut work = item("send-work", "account-a", WorkKind::Send);
        work.operation_id = Some("send-operation".into());
        worker
            .enqueue(work, 0)
            .expect("enqueue send counterexample");
        let claim = worker
            .claim_available(1, "matrix-worker", 1)
            .expect("claim send counterexample")
            .pop()
            .expect("send claim");
        let projected = batch("account-a", "send-batch", None, "send-cursor", 100);
        let error = worker
            .acknowledge_success_with_projection(&claim, 2, |transaction| {
                apply_worker_projection(
                    transaction,
                    &claim,
                    &WorkerProjection::ProviderBatch(Box::new(projected.clone())),
                )
            })
            .expect_err("send provider batch must be rejected");
        assert!(matches!(error, WorkerError::Conflict(_)));
        assert_eq!(
            worker
                .snapshot("send-work")
                .expect("send work snapshot")
                .expect("send work row")
                .state,
            WorkState::Executing
        );
        assert_eq!(
            operation_state(&path, "send-operation"),
            ("executing".into(), None),
            "the linked send cannot be confirmed without its immutable snapshot"
        );
        assert_eq!(
            Connection::open(&path)
                .expect("draft verification connection")
                .query_row(
                    "SELECT COUNT(*) FROM drafts WHERE id = 'draft-send'",
                    [],
                    |row| { row.get::<_, i64>(0) }
                )
                .expect("draft count"),
            1,
            "a rejected projection cannot consume the unsent draft"
        );
        assert_no_provider_projection(&path, "account-a");
    }

    #[test]
    fn mutation_cannot_confirm_from_a_provider_batch() {
        let (_directory, path, worker) = configured_worker(&["account-a"]);
        insert_linked_operation(
            &path,
            "mutation-operation",
            "provider_mutation",
            "account-a",
        );
        let mut work = item("mutation-work", "account-a", WorkKind::Mutation);
        work.operation_id = Some("mutation-operation".into());
        worker
            .enqueue(work, 0)
            .expect("enqueue mutation counterexample");
        let claim = worker
            .claim_available(1, "matrix-worker", 1)
            .expect("claim mutation counterexample")
            .pop()
            .expect("mutation claim");
        let projected = batch("account-a", "mutation-batch", None, "mutation-cursor", 100);
        let error = worker
            .acknowledge_success_with_projection(&claim, 2, |transaction| {
                apply_worker_projection(
                    transaction,
                    &claim,
                    &WorkerProjection::ProviderBatch(Box::new(projected.clone())),
                )
            })
            .expect_err("mutation provider batch must be rejected");
        assert!(matches!(error, WorkerError::Conflict(_)));
        assert_eq!(
            worker
                .snapshot("mutation-work")
                .expect("mutation work snapshot")
                .expect("mutation work row")
                .state,
            WorkState::Executing
        );
        assert_eq!(
            operation_state(&path, "mutation-operation"),
            ("executing".into(), None)
        );
        assert_no_provider_projection(&path, "account-a");
    }

    #[test]
    fn sync_cannot_succeed_from_a_local_operation_projection() {
        let (_directory, path, worker) = configured_worker(&["account-a"]);
        worker
            .enqueue(item("sync-work", "account-a", WorkKind::Sync), 0)
            .expect("enqueue sync counterexample");
        let claim = worker
            .claim_available(1, "matrix-worker", 1)
            .expect("claim sync counterexample")
            .pop()
            .expect("sync claim");
        let error = worker
            .acknowledge_success_with_projection(&claim, 2, |transaction| {
                apply_worker_projection(transaction, &claim, &WorkerProjection::LocalOperation)
            })
            .expect_err("sync local operation must be rejected");
        assert!(matches!(error, WorkerError::Conflict(_)));
        assert_eq!(
            worker
                .snapshot("sync-work")
                .expect("sync work snapshot")
                .expect("sync work row")
                .state,
            WorkState::Executing
        );
        assert_no_provider_projection(&path, "account-a");
    }

    #[test]
    fn cursor_projection_failpoint_rolls_back_with_worker_success_acknowledgement() {
        let (_directory, path, worker) = configured_worker(&["account-a"]);
        let projected = batch("account-a", "crash-batch", None, "cursor-crash", 100);
        worker
            .enqueue(batch_work("crash-work", "account-a", &projected), 0)
            .expect("enqueue crash fixture");
        let claim = worker
            .claim_available(1, "crash-worker", 1)
            .expect("claim crash fixture")
            .pop()
            .expect("crash claim");
        let error = worker
            .acknowledge_success_with_projection(&claim, 2, |transaction| {
                apply_provider_batch_in_transaction(
                    transaction,
                    projected.clone(),
                    ProviderBatchFailpoint::AfterCursor,
                )
                .map(|_| ())
                .map_err(worker_projection_error)
            })
            .expect_err("injected cursor crash must reject acknowledgement");
        assert!(matches!(error, WorkerError::Conflict(_)));

        let verify = Connection::open(path).expect("verification connection");
        assert_eq!(cursor(&verify, "account-a"), None);
        assert_eq!(
            verify
                .query_row("SELECT COUNT(*) FROM provider_applied_batches", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("receipt count"),
            0
        );
        assert_eq!(
            worker
                .snapshot("crash-work")
                .expect("crash snapshot")
                .expect("crash row")
                .state,
            WorkState::Executing
        );
    }

    #[test]
    fn production_worker_rejects_a_provider_batch_over_the_aggregate_byte_budget() {
        let (_directory, path, worker) = configured_worker(&["account-a"]);
        let projected = oversized_batch("account-a");
        assert!(
            ensure_serialized_budget(&projected, IpcPayloadKind::ProviderBatch).is_err(),
            "the fixture must exceed the aggregate 32 MiB provider batch budget"
        );
        let mut work = item("oversized-work", "account-a", WorkKind::Sync);
        work.ordering_key = projected.batch_id.as_str().into();
        worker.enqueue(work, 0).expect("enqueue bounded work item");
        let claim = worker
            .claim_available(1, "budget-worker", 1)
            .expect("claim bounded work item")
            .pop()
            .expect("budget claim");
        let error = worker
            .acknowledge_success_with_projection(&claim, 2, |transaction| {
                apply_worker_projection(
                    transaction,
                    &claim,
                    &WorkerProjection::ProviderBatch(Box::new(projected.clone())),
                )
            })
            .expect_err("oversized aggregate batch must be rejected");
        assert!(matches!(error, WorkerError::Conflict(_)));
        assert_eq!(
            worker
                .snapshot("oversized-work")
                .expect("budget snapshot")
                .expect("budget row")
                .state,
            WorkState::Executing,
            "rejected projection must not commit the fenced success state"
        );
        let verify = Connection::open(path).expect("verification connection");
        assert_eq!(cursor(&verify, "account-a"), None);
        assert_eq!(
            verify
                .query_row("SELECT COUNT(*) FROM provider_applied_batches", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("receipt count"),
            0
        );
    }

    #[test]
    fn expired_lease_retries_safe_mutation_but_never_retries_an_uncertain_send() {
        let (_sync_directory, sync_path, worker) = configured_worker(&["account-a"]);
        insert_linked_operation(
            &sync_path,
            "safe-operation",
            "provider_mutation",
            "account-a",
        );
        let mut safe_work = item("safe-mutation", "account-a", WorkKind::Mutation);
        safe_work.operation_id = Some("safe-operation".into());
        worker.enqueue(safe_work, 0).expect("enqueue safe sync");
        worker
            .claim_available(1, "lease-worker", 1)
            .expect("claim safe sync");
        let recovery = worker
            .recover_expired(1_001)
            .expect("recover safe sync lease");
        assert_eq!(recovery.safe_retried, 1);
        assert_eq!(recovery.send_outcome_unknown, 0);
        assert_eq!(
            worker
                .snapshot("safe-mutation")
                .expect("safe snapshot")
                .expect("safe row")
                .state,
            WorkState::RetryWait
        );
        assert_eq!(
            operation_state(&sync_path, "safe-operation"),
            (
                "retrying".into(),
                Some("worker_restarted_before_acknowledgement".into())
            )
        );

        let (_send_directory, send_path, send_worker) = configured_worker(&["account-a"]);
        insert_linked_operation(&send_path, "send-operation", "send", "account-a");
        let mut send_work = item("uncertain-send", "account-a", WorkKind::Send);
        send_work.operation_id = Some("send-operation".into());
        send_worker.enqueue(send_work, 2_000).expect("enqueue send");
        let send_claim = send_worker
            .claim_available(2_001, "lease-worker", 1)
            .expect("claim send")
            .pop()
            .expect("send claim");
        crate::worker::mark_send_submission_started(&send_path, &send_claim, 2_002)
            .expect("mark the exact submission boundary");
        let recovery = send_worker
            .recover_expired(3_001)
            .expect("recover send lease");
        assert_eq!(recovery.safe_retried, 0);
        assert_eq!(recovery.send_outcome_unknown, 1);
        let send = send_worker
            .snapshot("uncertain-send")
            .expect("send snapshot")
            .expect("send row");
        assert_eq!(send.state, WorkState::OutcomeUnknown);
        assert_eq!(
            send.last_error_code.as_deref(),
            Some("worker_restarted_during_send")
        );
        assert_eq!(
            operation_state(&send_path, "send-operation"),
            (
                "outcome_unknown".into(),
                Some("worker_restarted_during_send".into())
            )
        );
    }

    #[test]
    fn gmail_provider_conformance_page_commits_cursor_continuation_and_capabilities_atomically() {
        let (_directory, path, worker) = configured_worker(&["account-a"]);
        let first = batch("account-a", "gmail-page-1", None, "gmail-cursor-1", 10);
        worker
            .enqueue(batch_work("gmail-work-1", "account-a", &first), 0)
            .expect("enqueue first Gmail page");
        let claim = worker
            .claim_available(1, "gmail-worker", 1)
            .expect("claim first Gmail page")
            .pop()
            .expect("Gmail page claim");
        let page = ProviderSyncPage {
            batch: Box::new(first),
            restricted_message_content: Vec::new(),
            continuation: Some(ProviderSyncContinuation {
                id: "gmail-work-2".into(),
                ordering_key: "gmail-page-2".into(),
                payload_json: r#"{"version":1,"phase":"history"}"#.into(),
                priority: 5,
                available_at: 10,
                max_attempts: 8,
            }),
            complete: false,
            capabilities: Some(ProviderCapabilities::new([
                ProviderCapability::DeltaSync,
                ProviderCapability::RemoteThreads,
                ProviderCapability::MultiContainerMembership,
            ])),
            replace_memberships_for_upserted_messages: true,
            derive_thread_state_from_messages: true,
            reconciliation: Some(ProviderReconciliationPage {
                generation_id: "gmail-scan-1".into(),
                begin: true,
                reset_seen_containers: false,
                complete_kinds: Default::default(),
                sweep_kinds: Default::default(),
                seen_remote_messages: Vec::new(),
            }),
        };
        worker
            .acknowledge_success_with_projection(&claim, 11, |transaction| {
                apply_worker_projection(
                    transaction,
                    &claim,
                    &WorkerProjection::ProviderSyncPage(Box::new(page.clone())),
                )
            })
            .expect("commit first Gmail page");

        let verify = Connection::open(path).expect("verification connection");
        assert_eq!(cursor(&verify, "account-a"), Some("gmail-cursor-1".into()));
        assert_eq!(
            worker
                .snapshot("gmail-work-1")
                .expect("first work snapshot")
                .expect("first work row")
                .state,
            WorkState::Succeeded
        );
        assert_eq!(
            worker
                .snapshot("gmail-work-2")
                .expect("continuation snapshot")
                .expect("continuation row")
                .state,
            WorkState::Queued
        );
        assert_eq!(
            verify
                .query_row(
                    "SELECT sync_state FROM provider_accounts WHERE account_id = 'account-a'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("sync state"),
            "scheduled"
        );
        assert_eq!(
            verify
                .query_row(
                    "SELECT COUNT(*) FROM provider_capabilities
                     WHERE account_id = 'account-a' AND enabled = 1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("capability count"),
            3
        );
        assert_eq!(
            verify
                .query_row(
                    "SELECT generation_id FROM provider_reconciliation_runs
                     WHERE account_id = 'account-a' AND scope = 'sync:account'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("reconciliation run"),
            "gmail-scan-1"
        );
    }

    #[test]
    fn gmail_provider_conformance_persists_restricted_html_with_the_fenced_page() {
        let (_directory, path, worker) = configured_worker(&["account-a"]);
        let projected = gmail_data_batch("account-a", "gmail-html-page", None, "gmail-html-cursor");
        let identity = projected.message_upserts[0].identity.clone();
        worker
            .enqueue(batch_work("gmail-html-work", "account-a", &projected), 0)
            .expect("enqueue HTML page");
        let claim = worker
            .claim_available(1, "gmail-html-worker", 1)
            .expect("claim HTML page")
            .pop()
            .expect("HTML page claim");
        let page = ProviderSyncPage {
            batch: Box::new(projected),
            restricted_message_content: vec![RestrictedMessageContent {
                identity,
                body_html: "<p><strong>Confirmed</strong> <a href=\"https://example.test/plan\">plan</a><mux-remote-image data-id=\"1\"></mux-remote-image></p>".into(),
                blocked_remote_resources: 1,
                remote_images: vec![RemoteImageCandidate {
                    resource_id: 1,
                    url: "https://images.example.test/tracker.png?private=1".into(),
                    domain: "images.example.test".into(),
                    alt_text: "Receipt".into(),
                }],
            }],
            continuation: None,
            complete: true,
            capabilities: None,
            replace_memberships_for_upserted_messages: true,
            derive_thread_state_from_messages: true,
            reconciliation: None,
        };
        worker
            .acknowledge_success_with_projection(&claim, 11, |transaction| {
                apply_worker_projection(
                    transaction,
                    &claim,
                    &WorkerProjection::ProviderSyncPage(Box::new(page.clone())),
                )
            })
            .expect("commit restricted HTML page");

        let verify = Connection::open(path).expect("verification connection");
        let persisted = verify
            .query_row(
                "SELECT message.body_text, message.body_html,
                        message.blocked_remote_resources
                 FROM messages message
                 JOIN provider_message_refs reference ON reference.message_id = message.id
                 WHERE reference.account_id = 'account-a'
                   AND reference.remote_message_id = 'gmail-message'",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .expect("persisted restricted HTML");
        assert_eq!(persisted.0, "Confirmed body");
        assert!(persisted.1.contains("<strong>Confirmed</strong>"));
        assert_eq!(persisted.2, 1);
        assert_eq!(
            verify
                .query_row(
                    "SELECT resource_id, domain, alt_text, url FROM message_remote_images",
                    [],
                    |row| Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?
                    )),
                )
                .expect("persisted remote image sidecar"),
            (
                1,
                "images.example.test".into(),
                "Receipt".into(),
                "https://images.example.test/tracker.png?private=1".into()
            )
        );
        assert_eq!(
            cursor(&verify, "account-a"),
            Some("gmail-html-cursor".into())
        );

        let blocked = gmail_data_batch(
            "account-a",
            "gmail-blocked-page",
            Some("gmail-html-cursor"),
            "gmail-blocked-cursor",
        );
        let blocked_identity = blocked.message_upserts[0].identity.clone();
        worker
            .enqueue(batch_work("gmail-blocked-work", "account-a", &blocked), 20)
            .expect("enqueue blocked-only HTML replacement");
        let blocked_claim = worker
            .claim_available(21, "gmail-html-worker", 1)
            .expect("claim blocked-only HTML replacement")
            .pop()
            .expect("blocked-only HTML replacement claim");
        let blocked_page = ProviderSyncPage {
            batch: Box::new(blocked),
            restricted_message_content: vec![RestrictedMessageContent {
                identity: blocked_identity,
                body_html: String::new(),
                blocked_remote_resources: 1,
                remote_images: Vec::new(),
            }],
            continuation: None,
            complete: true,
            capabilities: None,
            replace_memberships_for_upserted_messages: true,
            derive_thread_state_from_messages: true,
            reconciliation: None,
        };
        worker
            .acknowledge_success_with_projection(&blocked_claim, 22, |transaction| {
                apply_worker_projection(
                    transaction,
                    &blocked_claim,
                    &WorkerProjection::ProviderSyncPage(Box::new(blocked_page.clone())),
                )
            })
            .expect("commit blocked-only HTML replacement");
        assert_eq!(
            verify
                .query_row(
                    "SELECT message.body_html, message.blocked_remote_resources
                     FROM messages message
                     JOIN provider_message_refs reference ON reference.message_id = message.id
                     WHERE reference.account_id = 'account-a'
                       AND reference.remote_message_id = 'gmail-message'",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .unwrap(),
            (String::new(), 1)
        );
        assert_eq!(
            verify
                .query_row("SELECT COUNT(*) FROM message_remote_images", [], |row| row
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            0
        );

        let plain = gmail_data_batch(
            "account-a",
            "gmail-plain-page",
            Some("gmail-blocked-cursor"),
            "gmail-plain-cursor",
        );
        let plain_identity = plain.message_upserts[0].identity.clone();
        worker
            .enqueue(batch_work("gmail-plain-work", "account-a", &plain), 30)
            .expect("enqueue complete plain replacement");
        let plain_claim = worker
            .claim_available(31, "gmail-html-worker", 1)
            .expect("claim complete plain replacement")
            .pop()
            .expect("plain replacement claim");
        let plain_page = ProviderSyncPage {
            batch: Box::new(plain),
            restricted_message_content: vec![RestrictedMessageContent {
                identity: plain_identity,
                body_html: String::new(),
                blocked_remote_resources: 0,
                remote_images: Vec::new(),
            }],
            continuation: None,
            complete: true,
            capabilities: None,
            replace_memberships_for_upserted_messages: true,
            derive_thread_state_from_messages: true,
            reconciliation: None,
        };
        worker
            .acknowledge_success_with_projection(&plain_claim, 32, |transaction| {
                apply_worker_projection(
                    transaction,
                    &plain_claim,
                    &WorkerProjection::ProviderSyncPage(Box::new(plain_page.clone())),
                )
            })
            .expect("commit complete plain replacement");
        assert_eq!(
            verify
                .query_row(
                    "SELECT message.body_html, message.blocked_remote_resources
                     FROM messages message
                     JOIN provider_message_refs reference ON reference.message_id = message.id
                     WHERE reference.account_id = 'account-a'
                       AND reference.remote_message_id = 'gmail-message'",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .expect("plain replacement"),
            (String::new(), 0)
        );
    }

    #[test]
    fn gmail_provider_conformance_rejects_uncanonical_html_before_any_page_commit() {
        let (_directory, path, worker) = configured_worker(&["account-a"]);
        let projected = gmail_data_batch(
            "account-a",
            "gmail-hostile-html-page",
            None,
            "gmail-hostile-html-cursor",
        );
        let identity = projected.message_upserts[0].identity.clone();
        worker
            .enqueue(
                batch_work("gmail-hostile-html-work", "account-a", &projected),
                0,
            )
            .expect("enqueue hostile HTML page");
        let claim = worker
            .claim_available(1, "gmail-html-worker", 1)
            .expect("claim hostile HTML page")
            .pop()
            .expect("hostile HTML page claim");
        let page = ProviderSyncPage {
            batch: Box::new(projected),
            restricted_message_content: vec![RestrictedMessageContent {
                identity,
                body_html: "<script>ambientAuthority()</script>".into(),
                blocked_remote_resources: 0,
                remote_images: Vec::new(),
            }],
            continuation: None,
            complete: true,
            capabilities: None,
            replace_memberships_for_upserted_messages: true,
            derive_thread_state_from_messages: true,
            reconciliation: None,
        };
        let rejection = worker.acknowledge_success_with_projection(&claim, 11, |transaction| {
            apply_worker_projection(
                transaction,
                &claim,
                &WorkerProjection::ProviderSyncPage(Box::new(page.clone())),
            )
        });
        assert!(matches!(rejection, Err(WorkerError::Conflict(_))));
        let verify = Connection::open(path).expect("verification connection");
        assert_eq!(cursor(&verify, "account-a"), None);
        assert_eq!(
            verify
                .query_row("SELECT COUNT(*) FROM provider_message_refs", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            0
        );
    }

    #[test]
    fn gmail_provider_conformance_rejected_page_rolls_back_projection_and_continuation() {
        let (_directory, path, worker) = configured_worker(&["account-a"]);
        let projected = batch("account-a", "gmail-invalid", None, "cursor-invalid", 10);
        worker
            .enqueue(batch_work("gmail-invalid-work", "account-a", &projected), 0)
            .expect("enqueue invalid page fixture");
        let claim = worker
            .claim_available(1, "gmail-worker", 1)
            .expect("claim invalid page fixture")
            .pop()
            .expect("invalid page claim");
        let invalid = ProviderSyncPage {
            batch: Box::new(projected),
            restricted_message_content: Vec::new(),
            continuation: Some(ProviderSyncContinuation {
                id: "must-not-commit".into(),
                ordering_key: "must-not-commit".into(),
                payload_json: "{}".into(),
                priority: 0,
                available_at: 10,
                max_attempts: 8,
            }),
            complete: true,
            capabilities: None,
            replace_memberships_for_upserted_messages: false,
            derive_thread_state_from_messages: false,
            reconciliation: None,
        };
        assert!(worker
            .acknowledge_success_with_projection(&claim, 11, |transaction| {
                apply_worker_projection(
                    transaction,
                    &claim,
                    &WorkerProjection::ProviderSyncPage(Box::new(invalid.clone())),
                )
            })
            .is_err());
        let verify = Connection::open(path).expect("verification connection");
        assert_eq!(cursor(&verify, "account-a"), None);
        assert!(worker
            .snapshot("must-not-commit")
            .expect("continuation lookup")
            .is_none());
        assert_eq!(
            worker
                .snapshot("gmail-invalid-work")
                .expect("current work lookup")
                .expect("current work row")
                .state,
            WorkState::Executing
        );
    }

    #[test]
    fn gmail_provider_conformance_rescan_sweep_is_bounded_and_preserves_pending_intent() {
        let (_directory, path, worker) = configured_worker(&["account-a"]);
        let mut store = MuxStore::open(&path, false).expect("open provider store");
        store
            .apply_provider_batch(gmail_data_batch(
                "account-a",
                "gmail-baseline",
                None,
                "gmail-baseline-cursor",
            ))
            .expect("apply baseline projection");
        drop(store);
        let connection = Connection::open(&path).expect("fixture connection");
        let thread_id: i64 = connection
            .query_row(
                "SELECT thread_id FROM provider_thread_refs
                 WHERE account_id = 'account-a' AND remote_thread_id = 'gmail-thread'",
                [],
                |row| row.get(0),
            )
            .expect("local thread identity");
        connection
            .execute(
                "INSERT INTO operations(
                   id, thread_id, field, kind, old_value, new_value, state,
                   created_at, not_before, attempts
                 ) VALUES('pending-unread', ?1, 'unread', 'mark_unread', '0', '1',
                          'pending', 20, 20, 0)",
                [thread_id],
            )
            .expect("pending local intent");
        drop(connection);

        let begin = batch(
            "account-a",
            "gmail-rescan-begin",
            Some("gmail-baseline-cursor"),
            "gmail-rescan-cursor",
            30,
        );
        worker
            .enqueue(batch_work("gmail-rescan-work", "account-a", &begin), 20)
            .expect("enqueue rescan begin");
        let begin_claim = worker
            .claim_available(21, "gmail-worker", 1)
            .expect("claim rescan begin")
            .pop()
            .expect("rescan begin claim");
        let begin_page = ProviderSyncPage {
            batch: Box::new(begin),
            restricted_message_content: Vec::new(),
            continuation: Some(ProviderSyncContinuation {
                id: "gmail-sweep-work".into(),
                ordering_key: "gmail-sweep-batch".into(),
                payload_json: "{}".into(),
                priority: 0,
                available_at: 30,
                max_attempts: 8,
            }),
            complete: false,
            capabilities: None,
            replace_memberships_for_upserted_messages: true,
            derive_thread_state_from_messages: true,
            reconciliation: Some(ProviderReconciliationPage {
                generation_id: "gmail-rescan-generation".into(),
                begin: true,
                reset_seen_containers: false,
                complete_kinds: Default::default(),
                sweep_kinds: Default::default(),
                seen_remote_messages: Vec::new(),
            }),
        };
        worker
            .acknowledge_success_with_projection(&begin_claim, 31, |transaction| {
                apply_worker_projection(
                    transaction,
                    &begin_claim,
                    &WorkerProjection::ProviderSyncPage(Box::new(begin_page.clone())),
                )
            })
            .expect("commit rescan begin");

        let sweep_claim = worker
            .claim_available(31, "gmail-worker", 1)
            .expect("claim sweep")
            .pop()
            .expect("sweep claim");
        let sweep_batch = batch(
            "account-a",
            "gmail-sweep-batch",
            Some("gmail-rescan-cursor"),
            "gmail-idle-cursor",
            40,
        );
        let sweep_page = ProviderSyncPage {
            batch: Box::new(sweep_batch),
            restricted_message_content: Vec::new(),
            continuation: Some(ProviderSyncContinuation {
                id: "gmail-sweep-again".into(),
                ordering_key: "gmail-sweep-again-batch".into(),
                payload_json: "{}".into(),
                priority: 0,
                available_at: 40,
                max_attempts: 8,
            }),
            complete: false,
            capabilities: None,
            replace_memberships_for_upserted_messages: true,
            derive_thread_state_from_messages: true,
            reconciliation: Some(ProviderReconciliationPage {
                generation_id: "gmail-rescan-generation".into(),
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
        };
        worker
            .acknowledge_success_with_projection(&sweep_claim, 41, |transaction| {
                apply_worker_projection(
                    transaction,
                    &sweep_claim,
                    &WorkerProjection::ProviderSyncPage(Box::new(sweep_page.clone())),
                )
            })
            .expect("commit bounded sweep");

        assert!(worker
            .snapshot("gmail-sweep-again")
            .expect("suppressed continuation lookup")
            .is_none());
        let verify = Connection::open(path).expect("verification connection");
        let effective: (i64, i64, i64) = verify
            .query_row(
                "SELECT remote_deleted, unread, message_count
                 FROM thread_effective WHERE id = ?1",
                [thread_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("effective thread remains durable");
        assert_eq!(effective, (1, 1, 0));
        assert_eq!(
            verify
                .query_row(
                    "SELECT state FROM operations WHERE id = 'pending-unread'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("pending operation survives"),
            "pending"
        );
        assert_eq!(
            verify
                .query_row(
                    "SELECT COUNT(*) FROM provider_reconciliation_runs
                     WHERE account_id = 'account-a'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("reconciliation run count"),
            0
        );
    }

    #[test]
    fn inventory_only_reconciliation_is_bounded_account_scoped_and_lease_fenced() {
        let (_directory, path, worker) = configured_worker(&["account-a", "account-b"]);
        let mut store = MuxStore::open(&path, false).expect("open provider store");
        store
            .apply_provider_batch(gmail_data_batch(
                "account-a",
                "inventory-baseline",
                None,
                "inventory-baseline-cursor",
            ))
            .expect("apply inventory baseline");
        drop(store);

        let inventory_batch = batch(
            "account-a",
            "inventory-page",
            Some("inventory-baseline-cursor"),
            "inventory-cursor",
            50,
        );
        worker
            .enqueue(
                batch_work("inventory-work", "account-a", &inventory_batch),
                40,
            )
            .expect("enqueue inventory page");
        let claim = worker
            .claim_available(41, "inventory-worker", 1)
            .expect("claim inventory page")
            .pop()
            .expect("inventory claim");
        let identity = |account: &str, message: &str| RemoteMessageIdentity {
            mux_account_id: MuxAccountId::new(account).expect("account identity"),
            remote_message_id: RemoteMessageId::new(message).expect("message identity"),
            remote_thread_id: None,
        };
        let page = ProviderSyncPage {
            batch: Box::new(inventory_batch),
            restricted_message_content: Vec::new(),
            continuation: Some(ProviderSyncContinuation {
                id: "inventory-sweep-again".into(),
                ordering_key: "inventory-sweep-again-batch".into(),
                payload_json: "{}".into(),
                priority: 0,
                available_at: 50,
                max_attempts: 4,
            }),
            complete: false,
            capabilities: None,
            replace_memberships_for_upserted_messages: false,
            derive_thread_state_from_messages: true,
            reconciliation: Some(ProviderReconciliationPage {
                generation_id: "inventory-generation".into(),
                begin: true,
                reset_seen_containers: false,
                complete_kinds: [
                    ReconciliationObjectKind::Threads,
                    ReconciliationObjectKind::Messages,
                ]
                .into_iter()
                .collect(),
                sweep_kinds: [
                    ReconciliationObjectKind::Threads,
                    ReconciliationObjectKind::Messages,
                ]
                .into_iter()
                .collect(),
                seen_remote_messages: vec![
                    identity("account-a", "gmail-message"),
                    identity("account-a", "unknown-message"),
                ],
            }),
        };
        worker
            .acknowledge_success_with_projection(&claim, 51, |transaction| {
                apply_worker_projection(
                    transaction,
                    &claim,
                    &WorkerProjection::ProviderSyncPage(Box::new(page.clone())),
                )
            })
            .expect("commit inventory-only page");

        let verify = Connection::open(&path).expect("verification connection");
        assert_eq!(
            verify
                .query_row(
                    "SELECT COUNT(*) FROM provider_message_refs
                     WHERE account_id = 'account-a'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("message count"),
            1,
            "an unknown inventory identity never creates a projection row"
        );
        assert_eq!(
            verify
                .query_row(
                    "SELECT COUNT(*) FROM provider_tombstones
                     WHERE account_id = 'account-a'
                       AND object_kind IN ('message', 'thread')",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("tombstone count"),
            0,
            "the matching message and its durable thread were marked seen"
        );
        assert_eq!(
            verify
                .query_row(
                    "SELECT COUNT(*) FROM provider_reconciliation_seen
                     WHERE account_id = 'account-a' AND generation_id = 'inventory-generation'
                       AND remote_id = 'unknown-message'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("unknown seen count"),
            0,
            "unknown inventory identities create no durable seen marks"
        );
        assert_eq!(
            verify
                .query_row(
                    "SELECT COUNT(*) FROM provider_tombstones
                     WHERE account_id = 'account-a'
                       AND object_kind = 'container' AND remote_id = 'INBOX'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("container tombstone count"),
            0,
            "a message/thread inventory page cannot tombstone an incomplete container inventory"
        );
        assert_eq!(
            verify
                .query_row(
                    "SELECT COUNT(*) FROM provider_reconciliation_runs
                     WHERE account_id = 'account-a'
                       AND generation_id = 'inventory-generation'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("incomplete reconciliation run count"),
            1,
            "the run remains durable until the container inventory completes"
        );
        assert!(worker
            .snapshot("inventory-sweep-again")
            .expect("inventory continuation lookup")
            .is_some());
        drop(verify);

        let sweep_claim = worker
            .claim_available(51, "inventory-worker", 1)
            .expect("claim inventory sweep")
            .pop()
            .expect("inventory sweep claim");
        let sweep_batch = container_only_batch(
            "account-a",
            "inventory-sweep-again-batch",
            Some("inventory-cursor"),
            "inventory-finished-cursor",
        );
        let sweep_page = ProviderSyncPage {
            batch: Box::new(sweep_batch),
            restricted_message_content: Vec::new(),
            continuation: Some(ProviderSyncContinuation {
                id: "inventory-sweep-next".into(),
                ordering_key: "inventory-sweep-next-batch".into(),
                payload_json: "{}".into(),
                priority: 0,
                available_at: 52,
                max_attempts: 4,
            }),
            complete: false,
            capabilities: None,
            replace_memberships_for_upserted_messages: false,
            derive_thread_state_from_messages: true,
            reconciliation: Some(ProviderReconciliationPage {
                generation_id: "inventory-generation".into(),
                begin: false,
                reset_seen_containers: false,
                complete_kinds: [ReconciliationObjectKind::Containers].into_iter().collect(),
                sweep_kinds: [ReconciliationObjectKind::Containers].into_iter().collect(),
                seen_remote_messages: Vec::new(),
            }),
        };
        worker
            .acknowledge_success_with_projection(&sweep_claim, 53, |transaction| {
                apply_worker_projection(
                    transaction,
                    &sweep_claim,
                    &WorkerProjection::ProviderSyncPage(Box::new(sweep_page.clone())),
                )
            })
            .expect("commit inventory sweep");
        assert!(worker
            .snapshot("inventory-sweep-next")
            .expect("suppressed continuation lookup")
            .is_none());
        let verify = Connection::open(&path).expect("completed reconciliation verification");
        assert_eq!(
            verify
                .query_row(
                    "SELECT COUNT(*) FROM provider_reconciliation_runs
                     WHERE account_id = 'account-a'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("completed reconciliation run count"),
            0,
            "the final bounded sweep removes the complete reconciliation run atomically"
        );
        assert_eq!(
            verify
                .query_row(
                    "SELECT COUNT(*) FROM provider_tombstones
                     WHERE account_id = 'account-a' AND remote_id IN ('gmail-message', 'gmail-thread', 'INBOX')",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("surviving projection tombstone count"),
            0
        );
        drop(verify);

        let replay = worker.acknowledge_success_with_projection(&claim, 54, |transaction| {
            apply_worker_projection(
                transaction,
                &claim,
                &WorkerProjection::ProviderSyncPage(Box::new(page.clone())),
            )
        });
        assert!(matches!(replay, Err(WorkerError::LeaseLost)));

        for (label, expected_error, messages) in [
            (
                "cross-account",
                "inventory account does not match",
                vec![identity("account-b", "foreign-message")],
            ),
            (
                "duplicate",
                "duplicate message identity",
                vec![
                    identity("account-a", "duplicate-message"),
                    identity("account-a", "duplicate-message"),
                ],
            ),
        ] {
            let (_invalid_directory, invalid_path, invalid_worker) =
                configured_worker(&["account-a", "account-b"]);
            let invalid_batch = batch(
                "account-a",
                &format!("inventory-{label}-batch"),
                None,
                &format!("inventory-{label}-cursor"),
                60,
            );
            let invalid_work = batch_work(
                &format!("inventory-{label}-work"),
                "account-a",
                &invalid_batch,
            );
            invalid_worker
                .enqueue(invalid_work, 59)
                .expect("enqueue invalid page");
            let invalid_claim = invalid_worker
                .claim_available(60, "inventory-worker", 1)
                .expect("claim invalid page")
                .pop()
                .expect("invalid claim");
            let invalid_page = ProviderSyncPage {
                batch: Box::new(invalid_batch),
                restricted_message_content: Vec::new(),
                continuation: Some(ProviderSyncContinuation {
                    id: format!("inventory-{label}-continuation"),
                    ordering_key: format!("inventory-{label}-continuation-order"),
                    payload_json: "{}".into(),
                    priority: 0,
                    available_at: 61,
                    max_attempts: 4,
                }),
                complete: false,
                capabilities: None,
                replace_memberships_for_upserted_messages: false,
                derive_thread_state_from_messages: true,
                reconciliation: Some(ProviderReconciliationPage {
                    generation_id: format!("inventory-{label}-generation"),
                    begin: true,
                    reset_seen_containers: false,
                    complete_kinds: Default::default(),
                    sweep_kinds: Default::default(),
                    seen_remote_messages: messages,
                }),
            };
            let rejected = invalid_worker.acknowledge_success_with_projection(
                &invalid_claim,
                61,
                |transaction| {
                    apply_worker_projection(
                        transaction,
                        &invalid_claim,
                        &WorkerProjection::ProviderSyncPage(Box::new(invalid_page.clone())),
                    )
                },
            );
            match rejected {
                Err(WorkerError::Conflict(message)) => assert!(
                    message.contains(expected_error),
                    "{label} rejection named the wrong invariant: {message}"
                ),
                other => panic!("{label} inventory should fail with a named conflict: {other:?}"),
            }
            let verify = Connection::open(&invalid_path).expect("rejection verification");
            assert_eq!(cursor(&verify, "account-a"), None);
            assert!(invalid_worker
                .snapshot(&format!("inventory-{label}-continuation"))
                .expect("invalid continuation lookup")
                .is_none());
        }

        let (_overflow_directory, overflow_path, overflow_worker) =
            configured_worker(&["account-a"]);
        let overflow_batch = batch(
            "account-a",
            "inventory-overflow-batch",
            None,
            "inventory-overflow-cursor",
            70,
        );
        overflow_worker
            .enqueue(
                batch_work("inventory-overflow-work", "account-a", &overflow_batch),
                69,
            )
            .expect("enqueue overflow page");
        let overflow_claim = overflow_worker
            .claim_available(70, "inventory-worker", 1)
            .expect("claim overflow page")
            .pop()
            .expect("overflow claim");
        let overflow_page = ProviderSyncPage {
            batch: Box::new(overflow_batch),
            restricted_message_content: Vec::new(),
            continuation: Some(ProviderSyncContinuation {
                id: "inventory-overflow-continuation".into(),
                ordering_key: "inventory-overflow-continuation-order".into(),
                payload_json: "{}".into(),
                priority: 0,
                available_at: 71,
                max_attempts: 4,
            }),
            complete: false,
            capabilities: None,
            replace_memberships_for_upserted_messages: false,
            derive_thread_state_from_messages: true,
            reconciliation: Some(ProviderReconciliationPage {
                generation_id: "inventory-overflow-generation".into(),
                begin: true,
                reset_seen_containers: false,
                complete_kinds: Default::default(),
                sweep_kinds: Default::default(),
                seen_remote_messages: (0..=1_000)
                    .map(|index| identity("account-a", &format!("message-{index}")))
                    .collect(),
            }),
        };
        let overflow_rejected = overflow_worker.acknowledge_success_with_projection(
            &overflow_claim,
            71,
            |transaction| {
                apply_worker_projection(
                    transaction,
                    &overflow_claim,
                    &WorkerProjection::ProviderSyncPage(Box::new(overflow_page.clone())),
                )
            },
        );
        match overflow_rejected {
            Err(WorkerError::Conflict(message)) => assert!(
                message.contains("exceeds its bounded page"),
                "overflow rejection named the wrong invariant: {message}"
            ),
            other => panic!("overflow inventory should fail with a named conflict: {other:?}"),
        }
        let verify = Connection::open(overflow_path).expect("overflow verification");
        assert_eq!(cursor(&verify, "account-a"), None);
        assert!(overflow_worker
            .snapshot("inventory-overflow-continuation")
            .expect("overflow continuation lookup")
            .is_none());
    }

    #[test]
    fn reconciliation_requires_account_cursor_and_fences_account_global_generations() {
        let (_scoped_directory, scoped_path, scoped_worker) = configured_worker(&["account-a"]);
        let mut scoped_batch = batch(
            "account-a",
            "container-scoped-reconciliation",
            None,
            "container-scoped-cursor",
            10,
        );
        scoped_batch.cursor.scope = SyncCursorScope::Container {
            remote_container_id: RemoteContainerId::new("INBOX").expect("container identity"),
        };
        scoped_worker
            .enqueue(
                batch_work(
                    "container-scoped-reconciliation-work",
                    "account-a",
                    &scoped_batch,
                ),
                0,
            )
            .expect("enqueue container-scoped reconciliation");
        let scoped_claim = scoped_worker
            .claim_available(1, "scope-worker", 1)
            .expect("claim container-scoped reconciliation")
            .pop()
            .expect("container-scoped claim");
        let scoped_page = ProviderSyncPage {
            batch: Box::new(scoped_batch),
            restricted_message_content: Vec::new(),
            continuation: Some(ProviderSyncContinuation {
                id: "container-scoped-continuation".into(),
                ordering_key: "container-scoped-continuation-order".into(),
                payload_json: "{}".into(),
                priority: 0,
                available_at: 2,
                max_attempts: 4,
            }),
            complete: false,
            capabilities: None,
            replace_memberships_for_upserted_messages: false,
            derive_thread_state_from_messages: true,
            reconciliation: Some(ProviderReconciliationPage {
                generation_id: "container-scoped-generation".into(),
                begin: true,
                reset_seen_containers: false,
                complete_kinds: Default::default(),
                sweep_kinds: Default::default(),
                seen_remote_messages: Vec::new(),
            }),
        };
        let scoped_rejection =
            scoped_worker.acknowledge_success_with_projection(&scoped_claim, 2, |transaction| {
                apply_worker_projection(
                    transaction,
                    &scoped_claim,
                    &WorkerProjection::ProviderSyncPage(Box::new(scoped_page.clone())),
                )
            });
        match scoped_rejection {
            Err(WorkerError::Conflict(message)) => assert!(message.contains("account-scoped")),
            other => panic!("container-scoped reconciliation should be rejected: {other:?}"),
        }
        let scoped_verify = Connection::open(scoped_path).expect("scope rejection verification");
        assert_eq!(cursor(&scoped_verify, "account-a"), None);
        assert_eq!(
            scoped_verify
                .query_row(
                    "SELECT COUNT(*) FROM provider_reconciliation_runs",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("scope run count"),
            0
        );
        assert!(scoped_worker
            .snapshot("container-scoped-continuation")
            .expect("scope continuation lookup")
            .is_none());

        let (_directory, path, worker) = configured_worker(&["account-a"]);
        let first = gmail_data_batch("account-a", "scope-a-page", None, "scope-a-cursor");
        worker
            .enqueue(batch_work("scope-a-work", "account-a", &first), 0)
            .expect("enqueue scope A");
        let first_claim = worker
            .claim_available(1, "scope-worker", 1)
            .expect("claim scope A")
            .pop()
            .expect("scope A claim");
        let first_page = ProviderSyncPage {
            batch: Box::new(first),
            restricted_message_content: Vec::new(),
            continuation: Some(ProviderSyncContinuation {
                id: "scope-a-later".into(),
                ordering_key: "scope-a-later-order".into(),
                payload_json: "{}".into(),
                priority: 0,
                available_at: 999,
                max_attempts: 4,
            }),
            complete: false,
            capabilities: None,
            replace_memberships_for_upserted_messages: false,
            derive_thread_state_from_messages: true,
            reconciliation: Some(ProviderReconciliationPage {
                generation_id: "generation-a".into(),
                begin: true,
                reset_seen_containers: false,
                complete_kinds: Default::default(),
                sweep_kinds: Default::default(),
                seen_remote_messages: Vec::new(),
            }),
        };
        worker
            .acknowledge_success_with_projection(&first_claim, 2, |transaction| {
                apply_worker_projection(
                    transaction,
                    &first_claim,
                    &WorkerProjection::ProviderSyncPage(Box::new(first_page.clone())),
                )
            })
            .expect("commit scope A");

        let scope_b_batch = batch(
            "account-a",
            "scope-b-page",
            Some("scope-a-cursor"),
            "scope-b-cursor",
            20,
        );
        let mut scope_b_work = batch_work("scope-b-work", "account-a", &scope_b_batch);
        scope_b_work.scope = "sync:folder:INBOX".into();
        worker.enqueue(scope_b_work, 10).expect("enqueue scope B");
        let scope_b_claim = worker
            .claim_available(11, "scope-worker", 1)
            .expect("claim scope B")
            .pop()
            .expect("scope B claim");
        let scope_b_page = ProviderSyncPage {
            batch: Box::new(scope_b_batch),
            restricted_message_content: Vec::new(),
            continuation: Some(ProviderSyncContinuation {
                id: "scope-b-later".into(),
                ordering_key: "scope-b-later-order".into(),
                payload_json: "{}".into(),
                priority: 0,
                available_at: 999,
                max_attempts: 4,
            }),
            complete: false,
            capabilities: None,
            replace_memberships_for_upserted_messages: false,
            derive_thread_state_from_messages: true,
            reconciliation: Some(ProviderReconciliationPage {
                generation_id: "generation-b".into(),
                begin: true,
                reset_seen_containers: false,
                complete_kinds: Default::default(),
                sweep_kinds: Default::default(),
                seen_remote_messages: Vec::new(),
            }),
        };
        let scope_b_rejection =
            worker.acknowledge_success_with_projection(&scope_b_claim, 12, |transaction| {
                apply_worker_projection(
                    transaction,
                    &scope_b_claim,
                    &WorkerProjection::ProviderSyncPage(Box::new(scope_b_page.clone())),
                )
            });
        match scope_b_rejection {
            Err(WorkerError::Conflict(message)) => assert!(message.contains("another work scope")),
            other => panic!("a second reconciliation scope should be rejected: {other:?}"),
        }
        let verify = Connection::open(&path).expect("scope fencing verification");
        assert_eq!(cursor(&verify, "account-a"), Some("scope-a-cursor".into()));
        assert_eq!(
            verify
                .query_row(
                    "SELECT generation_id FROM provider_reconciliation_runs
                     WHERE account_id = 'account-a'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("active generation"),
            "generation-a"
        );
        drop(verify);

        Connection::open(&path)
            .expect("same-scope restart connection")
            .execute(
                "UPDATE provider_work_items
                 SET state = 'cancelled', completed_at = 19
                 WHERE id = 'scope-a-later' AND state = 'queued'",
                [],
            )
            .expect("retire the abandoned continuation before a same-scope restart");

        let replacement = batch(
            "account-a",
            "scope-a-restart-page",
            Some("scope-a-cursor"),
            "scope-a-restart-cursor",
            30,
        );
        worker
            .enqueue(
                batch_work("scope-a-restart-work", "account-a", &replacement),
                20,
            )
            .expect("enqueue same-scope restart");
        let replacement_claim = worker
            .claim_available(21, "scope-worker", 1)
            .expect("claim same-scope restart")
            .pop()
            .expect("same-scope restart claim");
        let replacement_page = ProviderSyncPage {
            batch: Box::new(replacement),
            restricted_message_content: Vec::new(),
            continuation: Some(ProviderSyncContinuation {
                id: "scope-a-restart-later".into(),
                ordering_key: "scope-a-restart-later-order".into(),
                payload_json: "{}".into(),
                priority: 0,
                available_at: 999,
                max_attempts: 4,
            }),
            complete: false,
            capabilities: None,
            replace_memberships_for_upserted_messages: false,
            derive_thread_state_from_messages: true,
            reconciliation: Some(ProviderReconciliationPage {
                generation_id: "generation-restarted".into(),
                begin: true,
                reset_seen_containers: false,
                complete_kinds: Default::default(),
                sweep_kinds: Default::default(),
                seen_remote_messages: Vec::new(),
            }),
        };
        worker
            .acknowledge_success_with_projection(&replacement_claim, 22, |transaction| {
                apply_worker_projection(
                    transaction,
                    &replacement_claim,
                    &WorkerProjection::ProviderSyncPage(Box::new(replacement_page.clone())),
                )
            })
            .expect("same canonical scope may replace a stale generation");
        let verify = Connection::open(path).expect("restart verification");
        assert_eq!(
            verify
                .query_row(
                    "SELECT generation_id FROM provider_reconciliation_runs
                     WHERE account_id = 'account-a'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("replacement generation"),
            "generation-restarted"
        );
        assert_eq!(
            verify
                .query_row(
                    "SELECT COUNT(*) FROM provider_reconciliation_seen
                     WHERE account_id = 'account-a' AND generation_id = 'generation-a'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("stale generation marks"),
            0,
            "replacement atomically cascades stale generation inventory"
        );
    }

    #[test]
    fn reconciliation_inventory_uses_durable_identity_and_ignores_tombstoned_or_unknown_hints() {
        let (_directory, path, worker) = configured_worker(&["account-a"]);
        let mut store = MuxStore::open(&path, false).expect("open overlap store");
        store
            .apply_provider_batch(gmail_data_batch(
                "account-a",
                "overlap-baseline",
                None,
                "overlap-baseline-cursor",
            ))
            .expect("apply overlap baseline");
        drop(store);
        let overlap = gmail_data_batch(
            "account-a",
            "overlap-page",
            Some("overlap-baseline-cursor"),
            "overlap-cursor",
        );
        worker
            .enqueue(batch_work("overlap-work", "account-a", &overlap), 0)
            .expect("enqueue overlap page");
        let overlap_claim = worker
            .claim_available(1, "identity-worker", 1)
            .expect("claim overlap page")
            .pop()
            .expect("overlap claim");
        let overlap_page = ProviderSyncPage {
            batch: Box::new(overlap),
            restricted_message_content: Vec::new(),
            continuation: Some(ProviderSyncContinuation {
                id: "overlap-continuation".into(),
                ordering_key: "overlap-continuation-order".into(),
                payload_json: "{}".into(),
                priority: 0,
                available_at: 99,
                max_attempts: 4,
            }),
            complete: false,
            capabilities: None,
            replace_memberships_for_upserted_messages: false,
            derive_thread_state_from_messages: true,
            reconciliation: Some(ProviderReconciliationPage {
                generation_id: "overlap-generation".into(),
                begin: true,
                reset_seen_containers: false,
                complete_kinds: [
                    ReconciliationObjectKind::Threads,
                    ReconciliationObjectKind::Messages,
                ]
                .into_iter()
                .collect(),
                sweep_kinds: [
                    ReconciliationObjectKind::Threads,
                    ReconciliationObjectKind::Messages,
                ]
                .into_iter()
                .collect(),
                seen_remote_messages: vec![RemoteMessageIdentity {
                    mux_account_id: MuxAccountId::new("account-a").expect("account identity"),
                    remote_message_id: RemoteMessageId::new("gmail-message")
                        .expect("message identity"),
                    remote_thread_id: Some(
                        RemoteThreadId::new("stale-provider-hint").expect("thread hint"),
                    ),
                }],
            }),
        };
        worker
            .acknowledge_success_with_projection(&overlap_claim, 2, |transaction| {
                apply_worker_projection(
                    transaction,
                    &overlap_claim,
                    &WorkerProjection::ProviderSyncPage(Box::new(overlap_page.clone())),
                )
            })
            .expect("batch upsert and inventory overlap is idempotent");
        let verify = Connection::open(&path).expect("overlap verification");
        let marks = verify
            .prepare(
                "SELECT object_kind, remote_id FROM provider_reconciliation_seen
                 WHERE account_id = 'account-a' AND generation_id = 'overlap-generation'
                 ORDER BY object_kind, remote_id",
            )
            .expect("prepare overlap marks")
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .expect("query overlap marks")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect overlap marks");
        assert!(marks.contains(&("message".into(), "gmail-message".into())));
        assert!(marks.contains(&("thread".into(), "gmail-thread".into())));
        assert!(!marks
            .iter()
            .any(|(_, remote_id)| remote_id == "stale-provider-hint"));
        drop(verify);

        let (_deleted_directory, deleted_path, deleted_worker) = configured_worker(&["account-a"]);
        let mut deleted_store = MuxStore::open(&deleted_path, false).expect("open deletion store");
        deleted_store
            .apply_provider_batch(gmail_data_batch(
                "account-a",
                "deleted-baseline",
                None,
                "deleted-baseline-cursor",
            ))
            .expect("apply deletion baseline");
        let deletion: ProviderBatch = serde_json::from_value(serde_json::json!({
            "muxAccountId": "account-a",
            "batchId": "deleted-message-batch",
            "expectedPriorCursor": "deleted-baseline-cursor",
            "cursor": {
                "muxAccountId": "account-a",
                "scope": { "kind": "account" },
                "value": "deleted-message-cursor"
            },
            "observedAt": 20,
            "threadUpserts": [],
            "messageUpserts": [],
            "containerUpserts": [],
            "membershipChanges": [],
            "tombstones": [{
                "target": {
                    "kind": "message",
                    "identity": {
                        "muxAccountId": "account-a",
                        "remoteMessageId": "gmail-message",
                        "remoteThreadId": "gmail-thread"
                    }
                },
                "observedAt": 20,
                "cursor": "deleted-message-cursor"
            }]
        }))
        .expect("valid deletion batch");
        deleted_store
            .apply_provider_batch(deletion)
            .expect("apply message deletion");
        drop(deleted_store);
        let deleted_inventory = batch(
            "account-a",
            "deleted-inventory-page",
            Some("deleted-message-cursor"),
            "deleted-inventory-cursor",
            30,
        );
        deleted_worker
            .enqueue(
                batch_work("deleted-inventory-work", "account-a", &deleted_inventory),
                0,
            )
            .expect("enqueue deleted inventory");
        let deleted_claim = deleted_worker
            .claim_available(1, "identity-worker", 1)
            .expect("claim deleted inventory")
            .pop()
            .expect("deleted inventory claim");
        let deleted_page = ProviderSyncPage {
            batch: Box::new(deleted_inventory),
            restricted_message_content: Vec::new(),
            continuation: Some(ProviderSyncContinuation {
                id: "deleted-inventory-continuation".into(),
                ordering_key: "deleted-inventory-continuation-order".into(),
                payload_json: "{}".into(),
                priority: 0,
                available_at: 99,
                max_attempts: 4,
            }),
            complete: false,
            capabilities: None,
            replace_memberships_for_upserted_messages: false,
            derive_thread_state_from_messages: true,
            reconciliation: Some(ProviderReconciliationPage {
                generation_id: "deleted-inventory-generation".into(),
                begin: true,
                reset_seen_containers: false,
                complete_kinds: Default::default(),
                sweep_kinds: Default::default(),
                seen_remote_messages: vec![RemoteMessageIdentity {
                    mux_account_id: MuxAccountId::new("account-a").expect("account identity"),
                    remote_message_id: RemoteMessageId::new("gmail-message")
                        .expect("message identity"),
                    remote_thread_id: Some(
                        RemoteThreadId::new("gmail-thread").expect("thread identity"),
                    ),
                }],
            }),
        };
        deleted_worker
            .acknowledge_success_with_projection(&deleted_claim, 2, |transaction| {
                apply_worker_projection(
                    transaction,
                    &deleted_claim,
                    &WorkerProjection::ProviderSyncPage(Box::new(deleted_page.clone())),
                )
            })
            .expect("commit tombstoned inventory page");
        let verify = Connection::open(deleted_path).expect("deleted inventory verification");
        assert_eq!(
            verify
                .query_row(
                    "SELECT message.remote_deleted
                     FROM provider_message_refs reference
                     JOIN messages message ON message.id = reference.message_id
                     WHERE reference.account_id = 'account-a'
                       AND reference.remote_message_id = 'gmail-message'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("deleted message state"),
            1
        );
        assert_eq!(
            verify
                .query_row(
                    "SELECT COUNT(*) FROM provider_tombstones
                     WHERE account_id = 'account-a' AND object_kind = 'message'
                       AND remote_id = 'gmail-message'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("durable message tombstone"),
            1
        );
        assert_eq!(
            verify
                .query_row(
                    "SELECT COUNT(*) FROM provider_reconciliation_seen
                     WHERE account_id = 'account-a'
                       AND generation_id = 'deleted-inventory-generation'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("tombstoned inventory marks"),
            0,
            "inventory cannot resurrect or mark a tombstoned message/thread"
        );
    }

    #[test]
    fn reconciliation_combined_budget_counts_inventory_and_rolls_back_atomically() {
        use crate::ipc_boundary::{ensure_serialized_budget, IpcPayloadKind};

        let (_directory, path, worker) = configured_worker(&["account-a"]);
        let projected = large_batch(
            "account-a",
            15,
            "combined-budget-batch",
            "combined-budget-cursor",
        );
        assert!(ensure_serialized_budget(&projected, IpcPayloadKind::ProviderBatch).is_ok());
        let account = MuxAccountId::new("account-a").expect("account identity");
        let inventory = (0..1_000)
            .map(|index| {
                let prefix = format!("inventory-{index:04}-");
                RemoteMessageIdentity {
                    mux_account_id: account.clone(),
                    remote_message_id: RemoteMessageId::new(format!(
                        "{prefix}{}",
                        "x".repeat(2_048 - prefix.len())
                    ))
                    .expect("maximum bounded identity"),
                    remote_thread_id: None,
                }
            })
            .collect::<Vec<_>>();
        assert!(ensure_serialized_budget(
            &(&projected, inventory.as_slice()),
            IpcPayloadKind::ProviderBatch
        )
        .is_err());

        let mut work = item("combined-budget-work", "account-a", WorkKind::Sync);
        work.scope = "sync:account".into();
        work.ordering_key = projected.batch_id.as_str().into();
        worker
            .enqueue(work, 0)
            .expect("enqueue combined budget work");
        let claim = worker
            .claim_available(1, "budget-worker", 1)
            .expect("claim combined budget work")
            .pop()
            .expect("combined budget claim");
        let page = ProviderSyncPage {
            batch: Box::new(projected),
            restricted_message_content: Vec::new(),
            continuation: Some(ProviderSyncContinuation {
                id: "combined-budget-continuation".into(),
                ordering_key: "combined-budget-continuation-order".into(),
                payload_json: "{}".into(),
                priority: 0,
                available_at: 2,
                max_attempts: 4,
            }),
            complete: false,
            capabilities: None,
            replace_memberships_for_upserted_messages: false,
            derive_thread_state_from_messages: true,
            reconciliation: Some(ProviderReconciliationPage {
                generation_id: "combined-budget-generation".into(),
                begin: true,
                reset_seen_containers: false,
                complete_kinds: Default::default(),
                sweep_kinds: Default::default(),
                seen_remote_messages: inventory,
            }),
        };
        assert!(matches!(
            worker.acknowledge_success_with_projection(&claim, 2, |transaction| {
                apply_worker_projection(
                    transaction,
                    &claim,
                    &WorkerProjection::ProviderSyncPage(Box::new(page.clone())),
                )
            }),
            Err(WorkerError::Conflict(_))
        ));
        let verify = Connection::open(path).expect("combined budget verification");
        assert_eq!(cursor(&verify, "account-a"), None);
        assert_eq!(
            verify
                .query_row(
                    "SELECT COUNT(*) FROM provider_reconciliation_runs",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("run count"),
            0
        );
        assert!(worker
            .snapshot("combined-budget-continuation")
            .expect("continuation lookup")
            .is_none());
    }

    #[test]
    fn preapplied_sync_page_receipt_cannot_bypass_reconciliation_fencing() {
        let (_directory, path, worker) = configured_worker(&["account-a"]);
        let preapplied = batch(
            "account-a",
            "preapplied-page-batch",
            None,
            "preapplied-page-cursor",
            10,
        );
        let mut store = MuxStore::open(&path, false).expect("open provider store");
        store
            .apply_provider_batch(preapplied.clone())
            .expect("preapply batch outside worker");
        drop(store);

        let mut work = item("preapplied-page-work", "account-a", WorkKind::Sync);
        work.scope = "sync:account".into();
        work.ordering_key = preapplied.batch_id.as_str().into();
        worker
            .enqueue(work, 10)
            .expect("enqueue fresh claimed work");
        let claim = worker
            .claim_available(11, "receipt-worker", 1)
            .expect("claim work")
            .pop()
            .expect("fresh claim");
        let all: std::collections::BTreeSet<_> = [
            ReconciliationObjectKind::Containers,
            ReconciliationObjectKind::Threads,
            ReconciliationObjectKind::Messages,
        ]
        .into_iter()
        .collect();
        let malicious = ProviderSyncPage {
            batch: Box::new(preapplied),
            restricted_message_content: Vec::new(),
            continuation: Some(ProviderSyncContinuation {
                id: "receipt-malicious-continuation".into(),
                ordering_key: "receipt-malicious-order".into(),
                payload_json: "{}".into(),
                priority: 0,
                available_at: 12,
                max_attempts: 4,
            }),
            complete: false,
            capabilities: None,
            replace_memberships_for_upserted_messages: false,
            derive_thread_state_from_messages: true,
            reconciliation: Some(ProviderReconciliationPage {
                generation_id: "receipt-malicious-generation".into(),
                begin: true,
                reset_seen_containers: false,
                complete_kinds: all.clone(),
                sweep_kinds: all,
                seen_remote_messages: Vec::new(),
            }),
        };
        assert!(matches!(
            worker.acknowledge_success_with_projection(&claim, 12, |transaction| {
                apply_worker_projection(
                    transaction,
                    &claim,
                    &WorkerProjection::ProviderSyncPage(Box::new(malicious.clone())),
                )
            }),
            Err(WorkerError::Conflict(_))
        ));
        let verify = Connection::open(path).expect("receipt verification");
        assert_eq!(
            cursor(&verify, "account-a"),
            Some("preapplied-page-cursor".into())
        );
        assert_eq!(
            verify
                .query_row(
                    "SELECT COUNT(*) FROM provider_reconciliation_runs",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("run count"),
            0
        );
        assert!(worker
            .snapshot("receipt-malicious-continuation")
            .expect("malicious continuation lookup")
            .is_none());
        assert_eq!(
            worker
                .snapshot("preapplied-page-work")
                .expect("work lookup")
                .expect("work row")
                .state,
            WorkState::Executing
        );
    }

    #[test]
    fn gmail_provider_conformance_complete_labels_replace_stale_membership_and_rederive_thread() {
        let (_directory, path, _worker) = configured_worker(&["account-a"]);
        let mut store = MuxStore::open(&path, false).expect("open provider store");
        store
            .apply_provider_batch(gmail_data_batch(
                "account-a",
                "gmail-label-baseline",
                None,
                "gmail-label-cursor-1",
            ))
            .expect("apply baseline projection");
        drop(store);
        let mut connection = Connection::open(&path).expect("fixture connection");
        let thread_id: i64 = connection
            .query_row(
                "SELECT thread_id FROM provider_thread_refs
                 WHERE account_id = 'account-a' AND remote_thread_id = 'gmail-thread'",
                [],
                |row| row.get(0),
            )
            .expect("thread identity");
        connection
            .execute(
                "INSERT INTO operations(
                   id, thread_id, field, kind, old_value, new_value, state,
                   created_at, not_before, attempts
                 ) VALUES(
                   'pending-unread-after-relabel', ?1, 'unread', 'mark_unread',
                   '0', '1', 'pending', 15, 15, 0
                 )",
                [thread_id],
            )
            .expect("pending unread intent");
        let refresh: ProviderBatch = serde_json::from_value(serde_json::json!({
            "muxAccountId": "account-a",
            "batchId": "gmail-label-refresh",
            "expectedPriorCursor": "gmail-label-cursor-1",
            "cursor": {
                "muxAccountId": "account-a",
                "scope": { "kind": "account" },
                "value": "gmail-label-cursor-2"
            },
            "observedAt": 20,
            "threadUpserts": [],
            "messageUpserts": [{
                "identity": {
                    "muxAccountId": "account-a",
                    "remoteMessageId": "gmail-message",
                    "remoteThreadId": "gmail-thread"
                },
                "subject": "Confirmed subject",
                "senderName": "Sender",
                "senderEmail": "sender@example.test",
                "recipients": "reader@example.test",
                "ccRecipients": "",
                "bccRecipients": "",
                "sentAt": 20,
                "bodyText": "Relabeled body",
                "bodyState": "complete",
                "isFromMe": false,
                "revision": "message-r2",
                "keywords": ["starred"]
            }],
            "containerUpserts": [{
                "identity": {
                    "muxAccountId": "account-a",
                    "remoteContainerId": "STARRED"
                },
                "displayName": "Starred",
                "kind": "label",
                "role": "starred",
                "parentRemoteContainerId": null,
                "selectable": true
            }],
            "membershipChanges": [{
                "kind": "upsert",
                "membership": {
                    "message": {
                        "muxAccountId": "account-a",
                        "remoteMessageId": "gmail-message",
                        "remoteThreadId": "gmail-thread"
                    },
                    "container": {
                        "muxAccountId": "account-a",
                        "remoteContainerId": "STARRED"
                    }
                }
            }],
            "tombstones": []
        }))
        .expect("valid relabel batch");
        {
            let transaction = connection.transaction().expect("relabel transaction");
            apply_provider_batch_with_options_in_transaction(
                &transaction,
                refresh,
                ProviderBatchFailpoint::None,
                true,
                true,
            )
            .expect("apply complete Gmail labels");
            transaction.commit().expect("relabel commit");
        }
        let memberships: String = connection
            .query_row(
                "SELECT group_concat(remote_container_id, ',')
                 FROM provider_container_memberships
                 WHERE account_id = 'account-a' AND remote_message_id = 'gmail-message'",
                [],
                |row| row.get(0),
            )
            .expect("confirmed memberships");
        let remote: (i64, i64, i64) = connection
            .query_row(
                "SELECT remote_in_inbox, remote_unread, remote_starred
                 FROM threads WHERE id = ?1",
                [thread_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("derived remote state");
        let effective_unread: i64 = connection
            .query_row(
                "SELECT unread FROM thread_effective WHERE id = ?1",
                [thread_id],
                |row| row.get(0),
            )
            .expect("effective pending intent");
        assert_eq!(memberships, "STARRED");
        assert_eq!(remote, (0, 0, 1));
        assert_eq!(effective_unread, 1);
        assert_eq!(
            connection
                .query_row(
                    "SELECT state FROM operations
                     WHERE id = 'pending-unread-after-relabel'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("pending operation state"),
            "pending"
        );
    }
}
