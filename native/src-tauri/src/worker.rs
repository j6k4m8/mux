use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::provider::{ProviderBatch, ProviderCapabilities, RemoteMessageIdentity};

const MAX_WORK_ID_BYTES: usize = 256;
const MAX_ACCOUNT_ID_BYTES: usize = 256;
const MAX_OWNER_BYTES: usize = 256;
const MAX_ERROR_CODE_BYTES: usize = 128;
const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;
const MAX_SCOPE_BYTES: usize = 4096;
const MAX_ORDERING_KEY_BYTES: usize = 2048;
const SEND_SUBMISSION_STARTED_CODE: &str = "send_submission_started";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ThreadMutationPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format_version: Option<u8>,
    pub operation_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    pub thread_id: i64,
    pub field: String,
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_container_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo_of: Option<String>,
}

#[derive(Debug, Error)]
pub(crate) enum WorkerError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("{0}")]
    Validation(String),
    #[error("{0}")]
    Conflict(String),
    #[error("The work lease is no longer owned by this worker")]
    LeaseLost,
}

#[derive(Debug, Clone)]
pub(crate) struct WorkerConfig {
    pub max_in_flight: usize,
    pub max_per_account: usize,
    pub lease_ms: i64,
    pub base_backoff_ms: i64,
    pub max_backoff_ms: i64,
    pub jitter_seed: u64,
    pub idle_poll_ms: u64,
    pub heartbeat_ms: u64,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            max_in_flight: 4,
            max_per_account: 2,
            lease_ms: 60_000,
            base_backoff_ms: 1_000,
            max_backoff_ms: 15 * 60_000,
            jitter_seed: 0x6d75_785f_776f_726b,
            idle_poll_ms: 5_000,
            heartbeat_ms: 10_000,
        }
    }
}

impl WorkerConfig {
    fn validate(&self) -> Result<(), WorkerError> {
        if !(1..=64).contains(&self.max_in_flight) {
            return Err(WorkerError::Validation(
                "Worker max_in_flight must be between 1 and 64".into(),
            ));
        }
        if self.max_per_account == 0 || self.max_per_account > self.max_in_flight {
            return Err(WorkerError::Validation(
                "Worker max_per_account must be between 1 and max_in_flight".into(),
            ));
        }
        if self.lease_ms < 1_000 || self.base_backoff_ms < 1 || self.max_backoff_ms < 1 {
            return Err(WorkerError::Validation(
                "Worker lease and retry delays must be positive and the lease at least one second"
                    .into(),
            ));
        }
        if self.base_backoff_ms > self.max_backoff_ms {
            return Err(WorkerError::Validation(
                "Worker base backoff cannot exceed maximum backoff".into(),
            ));
        }
        if self.idle_poll_ms == 0 {
            return Err(WorkerError::Validation(
                "Worker idle poll interval must be positive".into(),
            ));
        }
        let heartbeat_ms = i64::try_from(self.heartbeat_ms).map_err(|_| {
            WorkerError::Validation("Worker heartbeat interval is too large".into())
        })?;
        if heartbeat_ms < 25 || heartbeat_ms.saturating_mul(3) > self.lease_ms {
            return Err(WorkerError::Validation(
                "Worker heartbeat must be at least 25ms and no more than one third of the lease"
                    .into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkKind {
    Sync,
    Mutation,
    Send,
}

impl WorkKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Sync => "sync",
            Self::Mutation => "mutation",
            Self::Send => "send",
        }
    }

    fn parse(value: &str) -> Result<Self, WorkerError> {
        match value {
            "sync" => Ok(Self::Sync),
            "mutation" => Ok(Self::Mutation),
            "send" => Ok(Self::Send),
            _ => Err(WorkerError::Validation("Unknown durable work kind".into())),
        }
    }

    fn retry_safety(self) -> &'static str {
        match self {
            Self::Send => "non_idempotent_send",
            Self::Sync | Self::Mutation => "safe_retry",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)] // Provider adapters consume the complete durable state contract.
pub(crate) enum WorkState {
    Queued,
    Executing,
    RetryWait,
    RateLimited,
    AuthenticationBlocked,
    Succeeded,
    Failed,
    Cancelled,
    OutcomeUnknown,
}

#[allow(dead_code)]
impl WorkState {
    fn parse(value: &str) -> Result<Self, WorkerError> {
        match value {
            "queued" => Ok(Self::Queued),
            "executing" => Ok(Self::Executing),
            "retry_wait" => Ok(Self::RetryWait),
            "rate_limited" => Ok(Self::RateLimited),
            "authentication_blocked" => Ok(Self::AuthenticationBlocked),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "outcome_unknown" => Ok(Self::OutcomeUnknown),
            _ => Err(WorkerError::Validation("Unknown durable work state".into())),
        }
    }

    fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::OutcomeUnknown
        )
    }
}

#[derive(Debug, Clone)]
pub(crate) struct NewWorkItem {
    pub id: String,
    pub account_id: String,
    pub operation_id: Option<String>,
    pub kind: WorkKind,
    /// Serializes jobs that mutate the same durable provider scope.
    pub scope: String,
    /// Stable adapter identity (batch ID, remote object mutation ID, or logical send ID).
    pub ordering_key: String,
    pub payload_json: String,
    pub priority: i64,
    pub available_at: i64,
    pub max_attempts: i64,
}

/// The next bounded page in one provider synchronization scope. The payload is
/// provider-private durable state: it may contain opaque page/history cursors,
/// but never credentials. The projector converts it into a normal safe-retry
/// work item in the same transaction that commits the current page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderSyncContinuation {
    pub id: String,
    pub ordering_key: String,
    pub payload_json: String,
    pub priority: i64,
    pub available_at: i64,
    pub max_attempts: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ReconciliationObjectKind {
    Containers,
    Threads,
    Messages,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderReconciliationPage {
    pub generation_id: String,
    pub begin: bool,
    /// A final authoritative container snapshot can discard earlier seen marks
    /// before recording the current page, so labels deleted during a long
    /// backfill are still swept.
    pub reset_seen_containers: bool,
    /// Object inventories that are now authoritatively complete, including an
    /// explicitly empty inventory. Completeness is durable and independent for
    /// containers, threads, and messages.
    pub complete_kinds: BTreeSet<ReconciliationObjectKind>,
    /// Tombstone one bounded page only for these explicitly authorized kinds.
    /// A kind must already be durably complete (or become complete on this
    /// page), otherwise the projector fails closed.
    pub sweep_kinds: BTreeSet<ReconciliationObjectKind>,
    /// Bounded identities observed by an inventory-only provider query. This
    /// lets delta-capable adapters prove that unchanged messages still exist
    /// without downloading and re-projecting their MIME bodies. The projector
    /// records these marks in the same transaction as the page cursor.
    pub seen_remote_messages: Vec<RemoteMessageIdentity>,
}

/// Complete result of one bounded provider sync request.
///
/// Exactly one of `complete` and `continuation` is present. This prevents a
/// successful work acknowledgement from losing the next page or declaring a
/// partial projection idle. Capability replacement is optional so adapters can
/// publish their exercised contract on bootstrap without repeating it forever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderSyncPage {
    pub batch: Box<ProviderBatch>,
    pub restricted_message_content: Vec<RestrictedMessageContent>,
    pub continuation: Option<ProviderSyncContinuation>,
    pub complete: bool,
    pub capabilities: Option<ProviderCapabilities>,
    pub replace_memberships_for_upserted_messages: bool,
    pub derive_thread_state_from_messages: bool,
    pub reconciliation: Option<ProviderReconciliationPage>,
}

/// Sanitized render content produced inside the Rust MIME boundary.
///
/// This is intentionally not deserializable or part of `ProviderBatch`: ambient
/// provider JSON cannot claim that arbitrary HTML has already been sanitized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RestrictedMessageContent {
    pub identity: RemoteMessageIdentity,
    pub body_html: String,
    pub blocked_remote_resources: i64,
    pub remote_images: Vec<crate::content::RemoteImageCandidate>,
}

/// Exact provider evidence for one immutable non-idempotent submission.
/// The production projector rechecks every field against the durable send row
/// before it can confirm the operation or delete the frozen draft revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProviderSendAcceptance {
    pub provider_kind: String,
    pub original_work_id: String,
    pub operation_id: String,
    pub send_payload_fingerprint_hex: String,
    pub remote_message_id: String,
    pub remote_thread_id: String,
    pub accepted_at_ms: i64,
    /// Present only when bounded evidence, rather than the submission response,
    /// resolved an outcome-unknown send.
    pub reconciliation_work_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClaimedWork {
    pub id: String,
    pub account_id: String,
    pub operation_id: Option<String>,
    pub kind: WorkKind,
    pub scope: String,
    pub ordering_key: String,
    pub payload_json: String,
    pub payload_fingerprint_hex: String,
    pub attempt: i64,
    pub lease_token: String,
    pub lease_expires_at: i64,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // The fake adapter succeeds; real adapters map the other bounded outcomes.
pub(crate) enum WorkerOutcome {
    Succeeded {
        projection: WorkerProjection,
    },
    RetryableFailure {
        code: String,
    },
    RejectedBeforeSubmission {
        code: String,
    },
    RateLimited {
        code: String,
        retry_after_at: i64,
    },
    /// No usable credential was available before provider I/O began, so the
    /// account needs reauthorizing. Safe for non-idempotent sends because
    /// nothing was submitted, and it does not consume retry budget.
    CredentialUnavailable,
    AuthenticationExpired {
        code: String,
    },
    PermanentFailure {
        code: String,
    },
    Cancelled,
    OutcomeUnknown {
        code: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // Real provider adapters return batches; the fake adapter uses local operations.
pub(crate) enum WorkerProjection {
    LocalOperation,
    RemoteThreadAbsent { remote_thread_id: String },
    ProviderBatch(Box<ProviderBatch>),
    ProviderSyncPage(Box<ProviderSyncPage>),
    ProviderSendAcceptance(Box<ProviderSendAcceptance>),
}

#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkerCycleResult {
    pub claimed: usize,
    pub succeeded: usize,
    pub retry_scheduled: usize,
    pub rate_limited: usize,
    pub authentication_blocked: usize,
    pub failed: usize,
    pub cancelled: usize,
    pub outcome_unknown: usize,
    pub errors: Vec<String>,
}

impl WorkerCycleResult {
    pub fn changed(&self) -> bool {
        self.claimed > 0
            || self.succeeded > 0
            || self.retry_scheduled > 0
            || self.rate_limited > 0
            || self.authentication_blocked > 0
            || self.failed > 0
            || self.cancelled > 0
            || self.outcome_unknown > 0
            || !self.errors.is_empty()
    }

    fn record(&mut self, state: WorkState) {
        match state {
            WorkState::Succeeded => self.succeeded += 1,
            WorkState::RetryWait => self.retry_scheduled += 1,
            WorkState::RateLimited => self.rate_limited += 1,
            WorkState::AuthenticationBlocked => self.authentication_blocked += 1,
            WorkState::Failed => self.failed += 1,
            WorkState::Cancelled => self.cancelled += 1,
            WorkState::OutcomeUnknown => self.outcome_unknown += 1,
            WorkState::Queued | WorkState::Executing => {}
        }
    }

    fn record_recovery(&mut self, recovery: RecoveryResult) {
        self.retry_scheduled = self.retry_scheduled.saturating_add(recovery.safe_retried);
        self.cancelled = self.cancelled.saturating_add(recovery.cancelled);
        self.failed = self.failed.saturating_add(recovery.failed);
        self.outcome_unknown = self
            .outcome_unknown
            .saturating_add(recovery.send_outcome_unknown);
    }
}

#[derive(Clone)]
pub(crate) struct WorkerExecutionContext {
    cancelled: Arc<AtomicBool>,
}

impl WorkerExecutionContext {
    pub(crate) fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    #[allow(dead_code)] // Real adapters poll this around bounded provider I/O and safe cancellation points.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

pub(crate) trait WorkerAdapter {
    fn execute(&self, work: &ClaimedWork, context: &WorkerExecutionContext) -> WorkerOutcome;
}

impl<F> WorkerAdapter for F
where
    F: Fn(&ClaimedWork, &WorkerExecutionContext) -> WorkerOutcome,
{
    fn execute(&self, work: &ClaimedWork, context: &WorkerExecutionContext) -> WorkerOutcome {
        self(work, context)
    }
}

pub(crate) trait WorkerProjector {
    fn apply_success(
        &self,
        transaction: &Transaction<'_>,
        work: &ClaimedWork,
        projection: &WorkerProjection,
    ) -> Result<(), WorkerError>;
}

impl<F> WorkerProjector for F
where
    F: for<'a> Fn(&Transaction<'a>, &ClaimedWork, &WorkerProjection) -> Result<(), WorkerError>,
{
    fn apply_success(
        &self,
        transaction: &Transaction<'_>,
        work: &ClaimedWork,
        projection: &WorkerProjection,
    ) -> Result<(), WorkerError> {
        self(transaction, work, projection)
    }
}

pub(crate) trait WorkerClock {
    fn now_ms(&self) -> i64;
}

impl<F> WorkerClock for F
where
    F: Fn() -> i64,
{
    fn now_ms(&self) -> i64 {
        self()
    }
}

struct SystemWorkerClock;

impl WorkerClock for SystemWorkerClock {
    fn now_ms(&self) -> i64 {
        system_now_ms()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RecoveryResult {
    pub safe_retried: usize,
    pub cancelled: usize,
    pub failed: usize,
    pub send_outcome_unknown: usize,
}

#[derive(Debug, Default)]
struct ClaimBatch {
    claims: Vec<ClaimedWork>,
    recovery: RecoveryResult,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub(crate) struct WorkSnapshot {
    pub state: WorkState,
    pub available_at: i64,
    pub attempt_count: i64,
    pub cancel_requested: bool,
    pub last_error_code: Option<String>,
    pub auth_block_reason: Option<String>,
}

#[derive(Clone)]
pub(crate) struct DurableWorker {
    database_path: PathBuf,
    config: WorkerConfig,
}

#[allow(dead_code)]
impl DurableWorker {
    pub fn new(path: impl AsRef<Path>, config: WorkerConfig) -> Result<Self, WorkerError> {
        config.validate()?;
        Ok(Self {
            database_path: path.as_ref().to_path_buf(),
            config,
        })
    }

    fn connect(&self) -> Result<Connection, WorkerError> {
        let connection = Connection::open(&self.database_path)?;
        connection.busy_timeout(Duration::from_secs(2))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        Ok(connection)
    }

    fn next_wake_delay(&self, now: i64) -> Result<Duration, WorkerError> {
        let connection = self.connect()?;
        let next_due = connection.query_row(
            "SELECT MIN(deadline)
             FROM (
               SELECT MAX(
                        work.available_at,
                        COALESCE(operation.not_before, work.available_at)
                      ) AS deadline
                 FROM provider_work_items work
                 LEFT JOIN operations operation ON operation.id = work.operation_id
                 WHERE work.state IN ('queued', 'retry_wait', 'rate_limited')
                   AND work.cancel_requested = 0
                   AND (
                     work.operation_id IS NULL OR operation.state IN ('pending', 'retrying')
                   )
                   AND (
                     EXISTS (
                       SELECT 1 FROM provider_accounts account
                       WHERE account.account_id = work.account_id
                         AND account.auth_state = 'ready'
                         AND account.credential_ref IS NOT NULL
                     ) OR EXISTS (
                       SELECT 1 FROM accounts account
                       WHERE account.id = work.account_id
                         AND account.provider = 'fake'
                         AND NOT EXISTS (
                           SELECT 1 FROM provider_accounts provider_account
                           WHERE provider_account.account_id = account.id
                         )
                     )
                   )
                   AND NOT EXISTS (
                     SELECT 1 FROM provider_work_items earlier
                     WHERE earlier.account_id = work.account_id
                       AND earlier.scope = work.scope
                       AND earlier.state NOT IN (
                         'succeeded', 'failed', 'cancelled', 'outcome_unknown'
                       )
                       AND (
                         earlier.created_at < work.created_at OR
                         (earlier.created_at = work.created_at AND earlier.id < work.id)
                       )
                   )
               UNION ALL
               SELECT lease_expires_at AS deadline
                 FROM provider_work_items
                WHERE state = 'executing' AND lease_expires_at IS NOT NULL
             )",
            [],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        let idle = Duration::from_millis(self.config.idle_poll_ms);
        let Some(next_due) = next_due else {
            return Ok(idle);
        };
        let delay_ms = (next_due.saturating_sub(now).max(0) as u64).max(25);
        Ok(Duration::from_millis(delay_ms).min(idle))
    }

    pub fn enqueue(&self, item: NewWorkItem, created_at: i64) -> Result<(), WorkerError> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        enqueue_in_transaction(&transaction, item, created_at)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn claim_available(
        &self,
        now: i64,
        owner: &str,
        requested: usize,
    ) -> Result<Vec<ClaimedWork>, WorkerError> {
        Ok(self
            .claim_available_with_recovery(now, owner, requested)?
            .claims)
    }

    fn claim_available_with_recovery(
        &self,
        now: i64,
        owner: &str,
        requested: usize,
    ) -> Result<ClaimBatch, WorkerError> {
        bounded_nonempty(owner, "Worker owner", MAX_OWNER_BYTES)?;
        if now < 0 {
            return Err(WorkerError::Validation(
                "Worker clock cannot be negative".into(),
            ));
        }
        if requested == 0 {
            return Ok(ClaimBatch::default());
        }
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let recovery = recover_expired_transaction(&transaction, now, &self.config)?;
        reconcile_linked_terminal_operations(&transaction, now)?;

        let active_total = transaction.query_row(
            "SELECT COUNT(*) FROM provider_work_items
             WHERE state = 'executing' AND lease_expires_at > ?1",
            [now],
            |row| row.get::<_, i64>(0),
        )? as usize;
        let available_slots = self
            .config
            .max_in_flight
            .saturating_sub(active_total)
            .min(requested);
        if available_slots == 0 {
            transaction.commit()?;
            return Ok(ClaimBatch {
                claims: Vec::new(),
                recovery,
            });
        }

        let mut account_active = BTreeMap::<String, usize>::new();
        {
            let mut statement = transaction.prepare(
                "SELECT account_id, COUNT(*) FROM provider_work_items
                 WHERE state = 'executing' AND lease_expires_at > ?1
                 GROUP BY account_id",
            )?;
            let rows = statement.query_map([now], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize))
            })?;
            for row in rows {
                let (account_id, count) = row?;
                account_active.insert(account_id, count);
            }
        }

        #[derive(Debug)]
        struct Candidate {
            id: String,
            account_id: String,
            operation_id: Option<String>,
            kind: String,
            scope: String,
            ordering_key: String,
            payload_json: String,
            payload_fingerprint_hex: String,
            attempt_count: i64,
        }
        let candidates = {
            let mut statement = transaction.prepare(
                "SELECT work.id, work.account_id, work.operation_id, work.kind,
                        work.scope, work.ordering_key, work.payload_json,
                        lower(hex(work.payload_fingerprint)), work.attempt_count
                 FROM provider_work_items work
                 WHERE work.state IN ('queued', 'retry_wait', 'rate_limited')
                   AND work.available_at <= ?1 AND work.cancel_requested = 0
                   AND (
                     work.operation_id IS NULL OR EXISTS (
                       SELECT 1 FROM operations operation
                       WHERE operation.id = work.operation_id
                         AND operation.state IN ('pending', 'retrying')
                         AND operation.not_before <= ?1
                     )
                   )
                   AND (
                     EXISTS (
                       SELECT 1 FROM provider_accounts account
                       WHERE account.account_id = work.account_id
                         AND account.auth_state = 'ready'
                         AND account.credential_ref IS NOT NULL
                     ) OR EXISTS (
                       SELECT 1 FROM accounts account
                       WHERE account.id = work.account_id
                         AND account.provider = 'fake'
                         AND NOT EXISTS (
                           SELECT 1 FROM provider_accounts provider_account
                           WHERE provider_account.account_id = account.id
                         )
                     )
                   )
                   AND NOT EXISTS (
                     SELECT 1 FROM provider_work_items earlier
                     WHERE earlier.account_id = work.account_id
                       AND earlier.scope = work.scope
                       AND earlier.state NOT IN (
                         'succeeded', 'failed', 'cancelled', 'outcome_unknown'
                       )
                       AND (
                         earlier.created_at < work.created_at OR
                         (earlier.created_at = work.created_at AND earlier.id < work.id)
                       )
                   )
                 ORDER BY priority DESC, created_at, id
                 LIMIT 4096",
            )?;
            let rows = statement
                .query_map([now], |row| {
                    Ok(Candidate {
                        id: row.get(0)?,
                        account_id: row.get(1)?,
                        operation_id: row.get(2)?,
                        kind: row.get(3)?,
                        scope: row.get(4)?,
                        ordering_key: row.get(5)?,
                        payload_json: row.get(6)?,
                        payload_fingerprint_hex: row.get(7)?,
                        attempt_count: row.get(8)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };

        let lease_expires_at = now.saturating_add(self.config.lease_ms);
        let mut claimed = Vec::with_capacity(available_slots);
        for candidate in candidates {
            if claimed.len() >= available_slots {
                break;
            }
            let account_count = account_active
                .entry(candidate.account_id.clone())
                .or_default();
            if *account_count >= self.config.max_per_account {
                continue;
            }
            if let Some(operation_id) = &candidate.operation_id {
                let operation_claimed = transaction.execute(
                    "UPDATE operations
                     SET state = 'executing', attempts = attempts + 1, error = NULL
                     WHERE id = ?1 AND state IN ('pending', 'retrying')
                       AND not_before <= ?2",
                    params![operation_id, now],
                )?;
                if operation_claimed != 1 {
                    continue;
                }
            }
            let token = transaction.query_row("SELECT lower(hex(randomblob(16)))", [], |row| {
                row.get::<_, String>(0)
            })?;
            let changed = transaction.execute(
                "UPDATE provider_work_items
                 SET state = 'executing', attempt_count = attempt_count + 1,
                     lease_owner = ?2, lease_token = ?3, lease_expires_at = ?4,
                     last_error_code = NULL, auth_block_reason = NULL,
                     retry_after_at = NULL
                 WHERE id = ?1
                   AND state IN ('queued', 'retry_wait', 'rate_limited')
                   AND available_at <= ?5 AND cancel_requested = 0",
                params![candidate.id, owner, token, lease_expires_at, now],
            )?;
            if changed != 1 {
                return Err(WorkerError::Conflict(
                    "Durable work changed while its linked operation was claimed".into(),
                ));
            }
            *account_count += 1;
            claimed.push(ClaimedWork {
                id: candidate.id,
                account_id: candidate.account_id,
                operation_id: candidate.operation_id,
                kind: WorkKind::parse(&candidate.kind)?,
                scope: candidate.scope,
                ordering_key: candidate.ordering_key,
                payload_json: candidate.payload_json,
                payload_fingerprint_hex: candidate.payload_fingerprint_hex,
                attempt: candidate.attempt_count + 1,
                lease_token: token,
                lease_expires_at,
            });
        }
        transaction.commit()?;
        Ok(ClaimBatch {
            claims: claimed,
            recovery,
        })
    }

    pub fn renew_lease(
        &self,
        work_id: &str,
        lease_token: &str,
        now: i64,
    ) -> Result<i64, WorkerError> {
        bounded_nonempty(work_id, "Work ID", MAX_WORK_ID_BYTES)?;
        bounded_nonempty(lease_token, "Lease token", MAX_WORK_ID_BYTES)?;
        let lease_expires_at = now.saturating_add(self.config.lease_ms);
        let connection = self.connect()?;
        let changed = connection.execute(
            "UPDATE provider_work_items SET lease_expires_at = ?3
             WHERE id = ?1 AND state = 'executing' AND lease_token = ?2
               AND lease_expires_at > ?4",
            params![work_id, lease_token, lease_expires_at, now],
        )?;
        if changed != 1 {
            return Err(WorkerError::LeaseLost);
        }
        Ok(lease_expires_at)
    }

    fn cancellation_requested(&self, claim: &ClaimedWork) -> Result<bool, WorkerError> {
        let connection = self.connect()?;
        connection
            .query_row(
                "SELECT cancel_requested FROM provider_work_items
                 WHERE id = ?1 AND state = 'executing' AND lease_token = ?2",
                params![claim.id, claim.lease_token],
                |row| Ok(row.get::<_, i64>(0)? != 0),
            )
            .optional()?
            .ok_or(WorkerError::LeaseLost)
    }

    pub fn acknowledge(
        &self,
        claim: &ClaimedWork,
        outcome: WorkerOutcome,
        now: i64,
    ) -> Result<WorkState, WorkerError> {
        let outcome = match outcome {
            WorkerOutcome::Succeeded { .. } => {
                return Err(WorkerError::Conflict(
                    "Successful work requires the fenced projection path".into(),
                ));
            }
            other => other,
        };
        validate_claim_payload(claim)?;
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = transaction
            .query_row(
                "SELECT kind, retry_safety, attempt_count, max_attempts, cancel_requested,
                        scope, ordering_key, lower(hex(payload_fingerprint)), last_error_code
                 FROM provider_work_items
                 WHERE id = ?1 AND state = 'executing' AND lease_token = ?2
                   AND lease_expires_at > ?3
                   AND (
                     operation_id IS NULL OR EXISTS (
                       SELECT 1 FROM operations operation
                       WHERE operation.id = provider_work_items.operation_id
                         AND operation.state = 'executing'
                     )
                   )",
                params![claim.id, claim.lease_token, now],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)? != 0,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, Option<String>>(8)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            kind,
            retry_safety,
            attempts,
            max_attempts,
            cancel_requested,
            scope,
            ordering_key,
            payload_fingerprint_hex,
            cancellation_reason,
        )) = current
        else {
            return Err(WorkerError::LeaseLost);
        };
        if kind != claim.kind.as_str()
            || retry_safety != claim.kind.retry_safety()
            || scope != claim.scope
            || ordering_key != claim.ordering_key
            || payload_fingerprint_hex != claim.payload_fingerprint_hex
        {
            return Err(WorkerError::Conflict(
                "Durable work kind changed after it was claimed".into(),
            ));
        }

        // Cooperative cancellation is durable state, not an adapter outcome. Once requested,
        // every non-success acknowledgement for retry-safe work must terminalize instead of
        // allowing a racing provider error to put the item (and any linked operation) back into
        // a waiting or authentication-blocked state. Successful projection acknowledgements use
        // the separate fenced path below and executing sends are deliberately never cancellable.
        if cancel_requested && claim.kind != WorkKind::Send {
            let code = if cancellation_reason.as_deref() == Some("credentials_reset") {
                "credentials_reset"
            } else {
                "cancelled_by_request"
            };
            set_terminal(
                &transaction,
                &claim.id,
                WorkState::Cancelled,
                Some(code),
                now,
            )?;
            transaction.commit()?;
            return Ok(WorkState::Cancelled);
        }

        let state = match outcome {
            WorkerOutcome::Succeeded { .. } => {
                unreachable!("success uses the fenced projection path")
            }
            WorkerOutcome::Cancelled => {
                if claim.kind == WorkKind::Send {
                    return Err(WorkerError::Validation(
                        "An executing send cannot use a cancelled outcome; report a pre-submission rejection or outcome_unknown"
                            .into(),
                    ));
                }
                let code = cancel_requested.then_some("cancelled_by_request");
                set_terminal(&transaction, &claim.id, WorkState::Cancelled, code, now)?;
                WorkState::Cancelled
            }
            WorkerOutcome::PermanentFailure { code } => {
                validate_error_code(&code)?;
                set_terminal(&transaction, &claim.id, WorkState::Failed, Some(&code), now)?;
                WorkState::Failed
            }
            WorkerOutcome::CredentialUnavailable => {
                refund_pre_submission_attempt(&transaction, &claim.id)?;
                let account = transaction
                    .query_row(
                        "SELECT auth_state, credential_ref, auth_block_reason
                         FROM provider_accounts WHERE account_id = ?1",
                        [claim.account_id.as_str()],
                        |row| {
                            Ok((
                                row.get::<_, String>(0)?,
                                row.get::<_, Option<String>>(1)?,
                                row.get::<_, Option<String>>(2)?,
                            ))
                        },
                    )
                    .optional()?;
                match account {
                    Some((auth_state, Some(_), _)) if auth_state == "ready" => {
                        transaction.execute(
                            "UPDATE provider_accounts
                             SET auth_state = 'reauthorization_required',
                                 auth_block_reason = 'provider_reauthorization',
                                 sync_state = 'authentication_blocked',
                                 last_error_code = 'credential_unavailable', updated_at = ?2
                             WHERE account_id = ?1",
                            params![claim.account_id, now],
                        )?;
                        set_pre_submission_auth_block(
                            &transaction,
                            &claim.id,
                            now,
                            "provider_reauthorization",
                            "credential_unavailable",
                        )?;
                        WorkState::AuthenticationBlocked
                    }
                    Some((auth_state, _, reason))
                        if auth_state == "reauthorization_required"
                            && reason.as_deref() == Some("provider_reauthorization") =>
                    {
                        set_pre_submission_auth_block(
                            &transaction,
                            &claim.id,
                            now,
                            "provider_reauthorization",
                            "provider_reauthorization",
                        )?;
                        WorkState::AuthenticationBlocked
                    }
                    Some((auth_state, _, _)) => {
                        let code = match auth_state.as_str() {
                            "signed_out" => "provider_account_signed_out",
                            "unavailable" => "provider_account_unavailable",
                            _ => "provider_account_not_configured",
                        };
                        set_terminal(
                            &transaction,
                            &claim.id,
                            WorkState::Cancelled,
                            Some(code),
                            now,
                        )?;
                        WorkState::Cancelled
                    }
                    None => {
                        set_terminal(
                            &transaction,
                            &claim.id,
                            WorkState::Cancelled,
                            Some("provider_account_removed"),
                            now,
                        )?;
                        WorkState::Cancelled
                    }
                }
            }
            WorkerOutcome::AuthenticationExpired { code } => {
                validate_error_code(&code)?;
                transaction.execute(
                    "UPDATE provider_work_items
                     SET state = 'authentication_blocked', last_error_code = ?2,
                         auth_block_reason = 'provider_reauthorization',
                         lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL
                     WHERE id = ?1",
                    params![claim.id, code],
                )?;
                let account_changed = transaction.execute(
                    "UPDATE provider_accounts
                     SET auth_state = 'reauthorization_required',
                         auth_block_reason = 'provider_reauthorization',
                         sync_state = 'authentication_blocked', last_error_code = ?2,
                         updated_at = ?3
                     WHERE account_id = ?1",
                    params![claim.account_id, code, now],
                )?;
                if account_changed != 1 {
                    return Err(WorkerError::Conflict(
                        "Authentication expiry requires a provider account".into(),
                    ));
                }
                set_linked_operation_retry(&transaction, &claim.id, now, &code)?;
                WorkState::AuthenticationBlocked
            }
            WorkerOutcome::RateLimited {
                code,
                retry_after_at,
            } => {
                validate_error_code(&code)?;
                if retry_after_at < now {
                    return Err(WorkerError::Validation(
                        "Provider retry-after cannot be in the past".into(),
                    ));
                }
                if attempts >= max_attempts {
                    set_terminal(&transaction, &claim.id, WorkState::Failed, Some(&code), now)?;
                    WorkState::Failed
                } else {
                    let retry_floor =
                        now.saturating_add(backoff_ms(attempts, &claim.id, &self.config));
                    set_waiting(
                        &transaction,
                        &claim.id,
                        WorkState::RateLimited,
                        retry_after_at.max(retry_floor),
                        Some(retry_after_at),
                        &code,
                    )?;
                    WorkState::RateLimited
                }
            }
            WorkerOutcome::RejectedBeforeSubmission { code } => {
                validate_error_code(&code)?;
                schedule_retry_or_fail(
                    &transaction,
                    &claim.id,
                    attempts,
                    max_attempts,
                    now,
                    &code,
                    &self.config,
                )?
            }
            WorkerOutcome::RetryableFailure { code } => {
                validate_error_code(&code)?;
                if retry_safety == "non_idempotent_send" {
                    set_terminal(
                        &transaction,
                        &claim.id,
                        WorkState::OutcomeUnknown,
                        Some(&code),
                        now,
                    )?;
                    WorkState::OutcomeUnknown
                } else {
                    schedule_retry_or_fail(
                        &transaction,
                        &claim.id,
                        attempts,
                        max_attempts,
                        now,
                        &code,
                        &self.config,
                    )?
                }
            }
            WorkerOutcome::OutcomeUnknown { code } => {
                validate_error_code(&code)?;
                if retry_safety == "safe_retry" {
                    schedule_retry_or_fail(
                        &transaction,
                        &claim.id,
                        attempts,
                        max_attempts,
                        now,
                        &code,
                        &self.config,
                    )?
                } else {
                    set_terminal(
                        &transaction,
                        &claim.id,
                        WorkState::OutcomeUnknown,
                        Some(&code),
                        now,
                    )?;
                    WorkState::OutcomeUnknown
                }
            }
        };
        transaction.commit()?;
        Ok(state)
    }

    /// Fences the lease, applies a provider projection/operation confirmation, and marks the
    /// work succeeded in one SQLite transaction. Integration must use this for successful real
    /// adapter results so no UI-visible confirmation can commit for a stale claim.
    pub fn acknowledge_success_with_projection<F>(
        &self,
        claim: &ClaimedWork,
        now: i64,
        apply_projection: F,
    ) -> Result<WorkState, WorkerError>
    where
        F: FnOnce(&Transaction<'_>) -> Result<(), WorkerError>,
    {
        validate_claim_payload(claim)?;
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let fence = transaction
            .query_row(
                "SELECT kind, retry_safety, scope, ordering_key,
                        lower(hex(payload_fingerprint))
                 FROM provider_work_items
                 WHERE id = ?1 AND state = 'executing' AND lease_token = ?2
                   AND lease_expires_at > ?3
                   AND (
                     operation_id IS NULL OR EXISTS (
                       SELECT 1 FROM operations operation
                       WHERE operation.id = provider_work_items.operation_id
                         AND operation.state = 'executing'
                     )
                   )",
                params![claim.id, claim.lease_token, now],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((kind, retry_safety, scope, ordering_key, payload_fingerprint_hex)) = fence else {
            return Err(WorkerError::LeaseLost);
        };
        if kind != claim.kind.as_str()
            || retry_safety != claim.kind.retry_safety()
            || scope != claim.scope
            || ordering_key != claim.ordering_key
            || payload_fingerprint_hex != claim.payload_fingerprint_hex
        {
            return Err(WorkerError::Conflict(
                "Durable work identity changed after it was claimed".into(),
            ));
        }
        apply_projection(&transaction)?;
        set_terminal(&transaction, &claim.id, WorkState::Succeeded, None, now)?;
        transaction.commit()?;
        Ok(WorkState::Succeeded)
    }

    pub fn request_cancellation(&self, work_id: &str, now: i64) -> Result<WorkState, WorkerError> {
        bounded_nonempty(work_id, "Work ID", MAX_WORK_ID_BYTES)?;
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = transaction
            .query_row(
                "SELECT kind, state FROM provider_work_items WHERE id = ?1",
                [work_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .ok_or_else(|| WorkerError::Conflict("Durable work item was not found".into()))?;
        let state = WorkState::parse(&current.1)?;
        if state.is_terminal() {
            transaction.commit()?;
            return Ok(state);
        }
        if state == WorkState::Executing {
            if current.0 == "send" {
                return Err(WorkerError::Conflict(
                    "A send cannot be cancelled after provider submission starts".into(),
                ));
            }
            let changed = transaction.execute(
                "UPDATE provider_work_items
                 SET cancel_requested = 1, last_error_code = 'cancelled_by_request'
                 WHERE id = ?1 AND state = 'executing' AND kind <> 'send'",
                [work_id],
            )?;
            if changed != 1 {
                return Err(WorkerError::Conflict(
                    "Durable work state changed during cancellation".into(),
                ));
            }
            transaction.commit()?;
            return Ok(WorkState::Executing);
        }
        set_terminal(
            &transaction,
            work_id,
            WorkState::Cancelled,
            Some("cancelled_before_execution"),
            now,
        )?;
        transaction.commit()?;
        Ok(WorkState::Cancelled)
    }

    pub fn resume_after_auth(&self, account_id: &str, now: i64) -> Result<usize, WorkerError> {
        bounded_nonempty(account_id, "Account ID", MAX_ACCOUNT_ID_BYTES)?;
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let account_changed = transaction.execute(
            "UPDATE provider_accounts
             SET auth_state = 'ready', sync_state = 'scheduled', last_error_code = NULL,
                 auth_block_reason = NULL, updated_at = ?2
             WHERE account_id = ?1 AND auth_state = 'reauthorization_required'
               AND auth_block_reason = 'provider_reauthorization'",
            params![account_id, now],
        )?;
        if account_changed != 1 {
            return Err(WorkerError::Conflict(
                "Provider account is not awaiting provider reauthorization".into(),
            ));
        }
        clear_resumed_linked_operation_errors(
            &transaction,
            account_id,
            "provider_reauthorization",
            now,
        )?;
        let changed = transaction.execute(
            "UPDATE provider_work_items
             SET state = 'queued', available_at = ?2, last_error_code = NULL,
                 auth_block_reason = NULL
             WHERE account_id = ?1 AND state = 'authentication_blocked'
               AND auth_block_reason = 'provider_reauthorization'",
            params![account_id, now],
        )?;
        transaction.commit()?;
        Ok(changed)
    }

    pub fn recover_expired(&self, now: i64) -> Result<RecoveryResult, WorkerError> {
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = recover_expired_transaction(&transaction, now, &self.config)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn snapshot(&self, work_id: &str) -> Result<Option<WorkSnapshot>, WorkerError> {
        let connection = self.connect()?;
        connection
            .query_row(
                "SELECT state, available_at, attempt_count, cancel_requested, last_error_code,
                        auth_block_reason
                 FROM provider_work_items WHERE id = ?1",
                [work_id],
                |row| {
                    let state = row.get::<_, String>(0)?;
                    Ok((
                        state,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)? != 0,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<String>>(5)?,
                    ))
                },
            )
            .optional()?
            .map(
                |(
                    state,
                    available_at,
                    attempt_count,
                    cancel_requested,
                    last_error_code,
                    auth_block_reason,
                )| {
                    Ok(WorkSnapshot {
                        state: WorkState::parse(&state)?,
                        available_at,
                        attempt_count,
                        cancel_requested,
                        last_error_code,
                        auth_block_reason,
                    })
                },
            )
            .transpose()
    }

    pub fn run_cycle<A, P, C>(
        &self,
        owner: &str,
        adapter: &A,
        projector: &P,
        clock: &C,
    ) -> Result<WorkerCycleResult, WorkerError>
    where
        A: WorkerAdapter + Sync,
        P: WorkerProjector + Sync,
        C: WorkerClock + Sync,
    {
        let batch =
            self.claim_available_with_recovery(clock.now_ms(), owner, self.config.max_in_flight)?;
        let mut result = WorkerCycleResult {
            claimed: batch.claims.len(),
            ..WorkerCycleResult::default()
        };
        result.record_recovery(batch.recovery);
        let (completed, panicked) = execute_claims(batch.claims, adapter);
        result.errors.extend(
            (0..panicked).map(|_| {
                "Provider adapter execution panicked; lease left for safe recovery".into()
            }),
        );
        for completion in completed {
            let acknowledged = settle_completion(self, &completion, projector, clock.now_ms());
            match acknowledged {
                Ok(state) => result.record(state),
                Err(error) => result
                    .errors
                    .push(format!("{}: {error}", completion.claim.id)),
            }
        }
        Ok(result)
    }
}

/// Durably fences the exact point immediately before a non-idempotent request
/// may perform network I/O. A lease that expires before this marker is safe to
/// retry; after the marker it is conservatively outcome-unknown.
pub(crate) fn mark_send_submission_started(
    database_path: &Path,
    claim: &ClaimedWork,
    now: i64,
) -> Result<(), WorkerError> {
    if claim.kind != WorkKind::Send || now < 0 {
        return Err(WorkerError::Validation(
            "Only a valid claimed send can cross the submission boundary".into(),
        ));
    }
    validate_claim_payload(claim)?;
    let connection = Connection::open(database_path)?;
    connection.busy_timeout(Duration::from_secs(2))?;
    let changed = connection.execute(
        "UPDATE provider_work_items
         SET last_error_code = ?4
         WHERE id = ?1 AND state = 'executing' AND lease_token = ?2
           AND lease_expires_at > ?3 AND kind = 'send'
           AND lower(hex(payload_fingerprint)) = ?5
           AND operation_id = ?6
           AND EXISTS (
             SELECT 1 FROM operations operation
             WHERE operation.id = provider_work_items.operation_id
               AND operation.state = 'executing'
           )",
        params![
            claim.id,
            claim.lease_token,
            now,
            SEND_SUBMISSION_STARTED_CODE,
            claim.payload_fingerprint_hex,
            claim.operation_id,
        ],
    )?;
    if changed != 1 {
        return Err(WorkerError::LeaseLost);
    }
    Ok(())
}

/// Runtime manager for detached adapter calls.
///
/// Stop joins only the manager. A noncooperative adapter thread may remain until process exit,
/// but `WorkerExecutionContext` carries only a cancellation bit: no SQLite handle, credential,
/// provider client, or other ambient authority is exposed through the context.
pub(crate) struct WorkerController {
    control: Sender<ManagerEvent>,
    stop_requested: Arc<AtomicBool>,
    start_gate: Arc<Mutex<()>>,
    thread: Option<JoinHandle<()>>,
}

struct PendingCompletion {
    claim: ClaimedWork,
    outcome: WorkerOutcome,
}

struct ActiveExecution {
    claim: ClaimedWork,
    context: WorkerExecutionContext,
    next_heartbeat_at: i64,
}

struct RetainedCompletion {
    completion: PendingCompletion,
    next_ack_at: i64,
    next_heartbeat_at: i64,
    ack_failures: u32,
}

fn execute_claims<A: WorkerAdapter + Sync>(
    claims: Vec<ClaimedWork>,
    adapter: &A,
) -> (Vec<PendingCompletion>, usize) {
    // The claim transaction has committed and its connection is gone before any adapter
    // work starts. Each claimed scope gets a bounded execution thread; claim_available
    // caps the vector globally and per account.
    let completed = thread::scope(|scope| {
        let handles = claims
            .into_iter()
            .map(|claim| {
                let context = WorkerExecutionContext::new();
                scope.spawn(move || PendingCompletion {
                    outcome: adapter.execute(&claim, &context),
                    claim,
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| handle.join())
            .collect::<Vec<_>>()
    });
    let panicked = completed.iter().filter(|result| result.is_err()).count();
    (
        completed.into_iter().filter_map(Result::ok).collect(),
        panicked,
    )
}

fn settle_completion<P: WorkerProjector + Sync>(
    worker: &DurableWorker,
    completion: &PendingCompletion,
    projector: &P,
    now: i64,
) -> Result<WorkState, WorkerError> {
    match &completion.outcome {
        WorkerOutcome::Succeeded { projection } => {
            worker.acknowledge_success_with_projection(&completion.claim, now, |transaction| {
                projector.apply_success(transaction, &completion.claim, projection)
            })
        }
        other => worker.acknowledge(&completion.claim, other.clone(), now),
    }
}

enum ManagerEvent {
    Wake,
    Stop,
    AdapterCompleted {
        work_id: String,
        lease_token: String,
        outcome: WorkerOutcome,
    },
    AdapterPanicked {
        work_id: String,
        lease_token: String,
    },
}

const ACK_RETRY_BASE_DELAY_MS: i64 = 50;
const ACK_RETRY_MAX_DELAY_MS: i64 = 5_000;

fn is_transient_ack_error(error: &WorkerError) -> bool {
    matches!(
        error,
        WorkerError::Sqlite(rusqlite::Error::SqliteFailure(sqlite, _))
            if matches!(
                sqlite.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
    )
}

fn ack_retry_delay_ms(failures: u32) -> i64 {
    let exponent = failures.saturating_sub(1).min(16);
    ACK_RETRY_BASE_DELAY_MS
        .saturating_mul(1_i64 << exponent)
        .min(ACK_RETRY_MAX_DELAY_MS)
}

fn terminal_outcome_after_ack_rejection(kind: WorkKind) -> WorkerOutcome {
    if kind == WorkKind::Send {
        WorkerOutcome::OutcomeUnknown {
            code: "acknowledgement_unknown_after_send".into(),
        }
    } else {
        WorkerOutcome::PermanentFailure {
            code: "acknowledgement_rejected".into(),
        }
    }
}

impl WorkerController {
    pub fn start<A, P, O>(
        worker: DurableWorker,
        owner: String,
        adapter: A,
        projector: P,
        mut observer: O,
    ) -> Result<Self, WorkerError>
    where
        A: WorkerAdapter + Send + Sync + 'static,
        P: WorkerProjector + Send + Sync + 'static,
        O: FnMut(WorkerCycleResult) + Send + 'static,
    {
        bounded_nonempty(&owner, "Worker owner", MAX_OWNER_BYTES)?;
        let poll = Duration::from_millis(worker.config.idle_poll_ms);
        let (sender, receiver) = mpsc::channel();
        let stop_requested = Arc::new(AtomicBool::new(false));
        let manager_stop = Arc::clone(&stop_requested);
        let start_gate = Arc::new(Mutex::new(()));
        let manager_start_gate = Arc::clone(&start_gate);
        let adapter = Arc::new(adapter);
        let manager_sender = sender.clone();
        let thread = thread::Builder::new()
            .name("mux-provider-worker".into())
            .spawn(move || {
                let clock = SystemWorkerClock;
                let heartbeat_ms = worker.config.heartbeat_ms as i64;
                let mut active = BTreeMap::<String, ActiveExecution>::new();
                let mut pending = BTreeMap::<String, RetainedCompletion>::new();
                let mut deferred_errors = Vec::<String>::new();
                loop {
                    if manager_stop.load(Ordering::Acquire) {
                        for execution in active.values() {
                            execution.context.cancel();
                        }
                        break;
                    }
                    let mut result = WorkerCycleResult::default();
                    result.errors.append(&mut deferred_errors);
                    let now = clock.now_ms();

                    let mut lost = Vec::new();
                    for (work_id, execution) in &mut active {
                        match worker.cancellation_requested(&execution.claim) {
                            Ok(true) => execution.context.cancel(),
                            Ok(false) => {}
                            Err(WorkerError::LeaseLost) => {
                                execution.context.cancel();
                                lost.push(work_id.clone());
                            }
                            Err(error) => result
                                .errors
                                .push(format!("{} cancellation check: {error}", work_id)),
                        }
                        if now >= execution.next_heartbeat_at {
                            match worker.renew_lease(
                                &execution.claim.id,
                                &execution.claim.lease_token,
                                now,
                            ) {
                                Ok(expires_at) => {
                                    execution.claim.lease_expires_at = expires_at;
                                    execution.next_heartbeat_at = now.saturating_add(heartbeat_ms);
                                }
                                Err(WorkerError::LeaseLost) => {
                                    execution.context.cancel();
                                    lost.push(work_id.clone());
                                }
                                Err(error) => {
                                    result
                                        .errors
                                        .push(format!("{} heartbeat: {error}", work_id));
                                    execution.next_heartbeat_at =
                                        now.saturating_add((heartbeat_ms / 2).max(25));
                                }
                            }
                        }
                    }
                    lost.sort();
                    lost.dedup();
                    for work_id in lost {
                        active.remove(&work_id);
                    }

                    let mut expired_pending = Vec::new();
                    for (work_id, completion) in &mut pending {
                        if now >= completion.next_heartbeat_at {
                            match worker.renew_lease(
                                &completion.completion.claim.id,
                                &completion.completion.claim.lease_token,
                                now,
                            ) {
                                Ok(expires_at) => {
                                    completion.completion.claim.lease_expires_at = expires_at;
                                    completion.next_heartbeat_at = now.saturating_add(heartbeat_ms);
                                }
                                Err(WorkerError::LeaseLost) => {
                                    expired_pending.push(work_id.clone());
                                }
                                Err(error) => {
                                    result
                                        .errors
                                        .push(format!("{} completion heartbeat: {error}", work_id));
                                    completion.next_heartbeat_at =
                                        now.saturating_add((heartbeat_ms / 2).max(25));
                                }
                            }
                        }
                    }
                    for work_id in expired_pending {
                        pending.remove(&work_id);
                    }

                    let due_acks = pending
                        .iter()
                        .filter(|(_, completion)| now >= completion.next_ack_at)
                        .map(|(work_id, _)| work_id.clone())
                        .collect::<Vec<_>>();
                    for work_id in due_acks {
                        let Some(mut completion) = pending.remove(&work_id) else {
                            continue;
                        };
                        match settle_completion(
                            &worker,
                            &completion.completion,
                            &projector,
                            clock.now_ms(),
                        ) {
                            Ok(state) => result.record(state),
                            Err(WorkerError::LeaseLost) => result
                                .errors
                                .push(format!("{}: work lease was lost", work_id)),
                            Err(error) if is_transient_ack_error(&error) => {
                                result.errors.push(format!("{}: {error}", work_id));
                                completion.ack_failures = completion.ack_failures.saturating_add(1);
                                completion.next_ack_at = now.saturating_add(ack_retry_delay_ms(
                                    completion.ack_failures,
                                ));
                                pending.insert(work_id, completion);
                            }
                            Err(error) => {
                                if std::env::var_os("MUX_LOG_WORKER_ERRORS").is_some() {
                                    eprintln!("Mux worker acknowledgement {work_id}: {error}");
                                }
                                result.errors.push(format!("{}: {error}", work_id));
                                completion.completion.outcome =
                                    terminal_outcome_after_ack_rejection(
                                        completion.completion.claim.kind,
                                    );
                                match settle_completion(
                                    &worker,
                                    &completion.completion,
                                    &projector,
                                    clock.now_ms(),
                                ) {
                                    Ok(state) => result.record(state),
                                    Err(WorkerError::LeaseLost) => result.errors.push(format!(
                                        "{}: work lease was lost while recording acknowledgement rejection",
                                        work_id
                                    )),
                                    Err(fallback) if is_transient_ack_error(&fallback) => {
                                        result.errors.push(format!(
                                            "{} terminal acknowledgement: {fallback}",
                                            work_id
                                        ));
                                        completion.ack_failures =
                                            completion.ack_failures.saturating_add(1);
                                        completion.next_ack_at = now.saturating_add(
                                            ack_retry_delay_ms(completion.ack_failures),
                                        );
                                        pending.insert(work_id, completion);
                                    }
                                    Err(fallback) => result.errors.push(format!(
                                        "{} terminal acknowledgement was rejected: {fallback}; lease left for recovery",
                                        work_id
                                    )),
                                }
                            }
                        }
                    }

                    let tracked = active.len().saturating_add(pending.len());
                    let capacity = worker.config.max_in_flight.saturating_sub(tracked);
                    if capacity > 0 && !manager_stop.load(Ordering::Acquire) {
                        let claims = {
                            let _gate = manager_start_gate
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner());
                            if manager_stop.load(Ordering::Acquire) {
                                Ok(ClaimBatch::default())
                            } else {
                                worker.claim_available_with_recovery(
                                    clock.now_ms(),
                                    &owner,
                                    capacity,
                                )
                            }
                        };
                        match claims {
                            Ok(batch) => {
                                result.record_recovery(batch.recovery);
                                for claim in batch.claims {
                                    let gate = manager_start_gate
                                        .lock()
                                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                                    if manager_stop.load(Ordering::Acquire) {
                                        drop(gate);
                                        break;
                                    }
                                    let context = WorkerExecutionContext::new();
                                    let adapter = Arc::clone(&adapter);
                                    let events = manager_sender.clone();
                                    let adapter_claim = claim.clone();
                                    let adapter_context = context.clone();
                                    let work_id = claim.id.clone();
                                    let lease_token = claim.lease_token.clone();
                                    let spawned = thread::Builder::new()
                                        .name("mux-provider-adapter".into())
                                        .spawn(move || {
                                            let outcome = std::panic::catch_unwind(
                                                std::panic::AssertUnwindSafe(|| {
                                                    adapter
                                                        .execute(&adapter_claim, &adapter_context)
                                                }),
                                            );
                                            let event = match outcome {
                                                Ok(outcome) => ManagerEvent::AdapterCompleted {
                                                    work_id,
                                                    lease_token,
                                                    outcome,
                                                },
                                                Err(_) => ManagerEvent::AdapterPanicked {
                                                    work_id,
                                                    lease_token,
                                                },
                                            };
                                            let _ = events.send(event);
                                        });
                                    drop(gate);
                                    match spawned {
                                        Ok(_detached) => {
                                            result.claimed += 1;
                                            active.insert(
                                                claim.id.clone(),
                                                ActiveExecution {
                                                    claim,
                                                    context,
                                                    next_heartbeat_at: now
                                                        .saturating_add(heartbeat_ms),
                                                },
                                            );
                                        }
                                        Err(error) => {
                                            context.cancel();
                                            result.errors.push(format!(
                                                "Could not start provider adapter: {error}"
                                            ));
                                        }
                                    }
                                }
                            }
                            Err(error) => result.errors.push(error.to_string()),
                        }
                    }
                    if result.changed() {
                        observer(result);
                    }

                    let wait_now = clock.now_ms();
                    let mut wait = poll;
                    if active.len().saturating_add(pending.len()) < worker.config.max_in_flight {
                        wait = wait.min(worker.next_wake_delay(wait_now).unwrap_or(poll));
                    }
                    for deadline in active
                        .values()
                        .map(|execution| execution.next_heartbeat_at)
                        .chain(pending.values().flat_map(|completion| {
                            [completion.next_ack_at, completion.next_heartbeat_at]
                        }))
                    {
                        let delay = deadline.saturating_sub(wait_now).max(1) as u64;
                        wait = wait.min(Duration::from_millis(delay));
                    }
                    match receiver.recv_timeout(wait) {
                        Ok(ManagerEvent::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                            for execution in active.values() {
                                execution.context.cancel();
                            }
                            break;
                        }
                        Ok(ManagerEvent::Wake) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                        Ok(ManagerEvent::AdapterCompleted {
                            work_id,
                            lease_token,
                            outcome,
                        }) => {
                            let matches = active.get(&work_id).is_some_and(|execution| {
                                execution.claim.lease_token == lease_token
                            });
                            if matches {
                                let execution =
                                    active.remove(&work_id).expect("matched active execution");
                                let now = clock.now_ms();
                                pending.insert(
                                    work_id,
                                    RetainedCompletion {
                                        completion: PendingCompletion {
                                            claim: execution.claim,
                                            outcome,
                                        },
                                        next_ack_at: now,
                                        next_heartbeat_at: now.saturating_add(heartbeat_ms),
                                        ack_failures: 0,
                                    },
                                );
                            }
                        }
                        Ok(ManagerEvent::AdapterPanicked {
                            work_id,
                            lease_token,
                        }) => {
                            let matches = active.get(&work_id).is_some_and(|execution| {
                                execution.claim.lease_token == lease_token
                            });
                            if matches {
                                if let Some(execution) = active.remove(&work_id) {
                                    execution.context.cancel();
                                }
                                deferred_errors.push(format!(
                                    "{}: provider adapter panicked; lease left for safe recovery",
                                    work_id
                                ));
                            }
                        }
                    }
                }
            })
            .map_err(|error| WorkerError::Conflict(format!("Could not start worker: {error}")))?;
        Ok(Self {
            control: sender,
            stop_requested,
            start_gate,
            thread: Some(thread),
        })
    }

    pub fn wake(&self) -> Result<(), WorkerError> {
        self.control
            .send(ManagerEvent::Wake)
            .map_err(|_| WorkerError::Conflict("Provider worker has stopped".into()))
    }

    pub fn stop(mut self) -> Result<(), WorkerError> {
        {
            let _gate = self
                .start_gate
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.stop_requested.store(true, Ordering::Release);
        }
        let _ = self.control.send(ManagerEvent::Stop);
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| WorkerError::Conflict("Provider worker thread panicked".into()))?;
        }
        Ok(())
    }
}

impl Drop for WorkerController {
    fn drop(&mut self) {
        {
            let _gate = self
                .start_gate
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            self.stop_requested.store(true, Ordering::Release);
        }
        let _ = self.control.send(ManagerEvent::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(crate) fn enqueue_in_transaction(
    transaction: &Transaction<'_>,
    item: NewWorkItem,
    created_at: i64,
) -> Result<(), WorkerError> {
    bounded_nonempty(&item.id, "Work ID", MAX_WORK_ID_BYTES)?;
    bounded_nonempty(&item.account_id, "Account ID", MAX_ACCOUNT_ID_BYTES)?;
    if let Some(operation_id) = &item.operation_id {
        bounded_nonempty(operation_id, "Operation ID", MAX_WORK_ID_BYTES)?;
    }
    bounded_nonempty(&item.scope, "Work scope", MAX_SCOPE_BYTES)?;
    bounded_nonempty(
        &item.ordering_key,
        "Work ordering key",
        MAX_ORDERING_KEY_BYTES,
    )?;
    if item.payload_json.len() > MAX_PAYLOAD_BYTES {
        return Err(WorkerError::Validation(
            "Work payload exceeds the 1 MiB durable limit".into(),
        ));
    }
    serde_json::from_str::<serde_json::Value>(&item.payload_json)
        .map_err(|_| WorkerError::Validation("Work payload must be valid JSON".into()))?;
    if !(1..=100).contains(&item.max_attempts) {
        return Err(WorkerError::Validation(
            "Work max_attempts must be between 1 and 100".into(),
        ));
    }
    if !(-1000..=1000).contains(&item.priority) || created_at < 0 || item.available_at < 0 {
        return Err(WorkerError::Validation(
            "Work priority or timestamp is outside its durable bound".into(),
        ));
    }
    let payload_fingerprint = Sha256::digest(item.payload_json.as_bytes());
    transaction.execute(
        "INSERT INTO provider_work_items(
           id, account_id, operation_id, kind, scope, ordering_key, retry_safety,
           payload_json, payload_fingerprint, state, priority, created_at,
           available_at, max_attempts
         ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'queued', ?10, ?11, ?12, ?13)",
        params![
            item.id,
            item.account_id,
            item.operation_id,
            item.kind.as_str(),
            item.scope,
            item.ordering_key,
            item.kind.retry_safety(),
            item.payload_json,
            payload_fingerprint.as_slice(),
            item.priority,
            created_at,
            item.available_at,
            item.max_attempts,
        ],
    )?;
    Ok(())
}

fn recover_expired_transaction(
    transaction: &Transaction<'_>,
    now: i64,
    config: &WorkerConfig,
) -> Result<RecoveryResult, WorkerError> {
    let expired = {
        let mut statement = transaction.prepare(
            "SELECT id, retry_safety, attempt_count, max_attempts, cancel_requested,
                    last_error_code
             FROM provider_work_items
             WHERE state = 'executing' AND lease_expires_at <= ?1
             ORDER BY created_at, id",
        )?;
        let rows = statement
            .query_map([now], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)? != 0,
                    row.get::<_, Option<String>>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    let mut result = RecoveryResult::default();
    for (id, retry_safety, attempts, max_attempts, cancel_requested, cancellation_reason) in expired
    {
        if retry_safety == "non_idempotent_send"
            && cancellation_reason.as_deref() == Some(SEND_SUBMISSION_STARTED_CODE)
        {
            set_terminal(
                transaction,
                &id,
                WorkState::OutcomeUnknown,
                Some("worker_restarted_during_send"),
                now,
            )?;
            result.send_outcome_unknown += 1;
        } else if retry_safety == "non_idempotent_send" {
            if attempts >= max_attempts {
                set_terminal(
                    transaction,
                    &id,
                    WorkState::Failed,
                    Some("retry_budget_exhausted_before_submission"),
                    now,
                )?;
                result.failed += 1;
            } else {
                let available_at = now.saturating_add(backoff_ms(attempts, &id, config));
                set_waiting(
                    transaction,
                    &id,
                    WorkState::RetryWait,
                    available_at,
                    None,
                    "worker_restarted_before_submission",
                )?;
                result.safe_retried += 1;
            }
        } else if cancel_requested {
            let code = if cancellation_reason.as_deref() == Some("credentials_reset") {
                "credentials_reset"
            } else {
                "cancelled_by_request"
            };
            set_terminal(transaction, &id, WorkState::Cancelled, Some(code), now)?;
            result.cancelled += 1;
        } else if attempts >= max_attempts {
            set_terminal(
                transaction,
                &id,
                WorkState::Failed,
                Some("retry_budget_exhausted_after_restart"),
                now,
            )?;
            result.failed += 1;
        } else {
            let available_at = now.saturating_add(backoff_ms(attempts, &id, config));
            set_waiting(
                transaction,
                &id,
                WorkState::RetryWait,
                available_at,
                None,
                "worker_restarted_before_acknowledgement",
            )?;
            result.safe_retried += 1;
        }
    }
    Ok(result)
}

fn reconcile_linked_terminal_operations(
    transaction: &Transaction<'_>,
    now: i64,
) -> Result<(), WorkerError> {
    transaction.execute(
        "UPDATE provider_work_items
         SET state = CASE (
               SELECT operation.state FROM operations operation
               WHERE operation.id = provider_work_items.operation_id
             )
               WHEN 'confirmed' THEN 'succeeded'
               WHEN 'cancelled' THEN 'cancelled'
               WHEN 'outcome_unknown' THEN 'outcome_unknown'
               ELSE 'failed'
             END,
             completed_at = ?1,
             last_error_code = CASE (
               SELECT operation.state FROM operations operation
               WHERE operation.id = provider_work_items.operation_id
             )
               WHEN 'confirmed' THEN NULL
               WHEN 'cancelled' THEN 'linked_operation_cancelled'
               WHEN 'outcome_unknown' THEN 'linked_operation_outcome_unknown'
               WHEN 'conflicted' THEN 'linked_operation_conflicted'
               ELSE 'linked_operation_failed'
             END,
             lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL,
             retry_after_at = NULL, auth_block_reason = NULL
         WHERE operation_id IS NOT NULL
           AND state NOT IN ('succeeded', 'failed', 'cancelled', 'outcome_unknown')
           AND EXISTS (
             SELECT 1 FROM operations operation
             WHERE operation.id = provider_work_items.operation_id
               AND operation.state IN (
                 'confirmed', 'failed', 'conflicted', 'cancelled', 'outcome_unknown'
               )
           )",
        [now],
    )?;
    Ok(())
}

fn schedule_retry_or_fail(
    transaction: &Transaction<'_>,
    work_id: &str,
    attempts: i64,
    max_attempts: i64,
    now: i64,
    code: &str,
    config: &WorkerConfig,
) -> Result<WorkState, WorkerError> {
    if attempts >= max_attempts {
        set_terminal(transaction, work_id, WorkState::Failed, Some(code), now)?;
        return Ok(WorkState::Failed);
    }
    let available_at = now.saturating_add(backoff_ms(attempts, work_id, config));
    set_waiting(
        transaction,
        work_id,
        WorkState::RetryWait,
        available_at,
        None,
        code,
    )?;
    Ok(WorkState::RetryWait)
}

fn set_waiting(
    transaction: &Transaction<'_>,
    work_id: &str,
    state: WorkState,
    available_at: i64,
    retry_after_at: Option<i64>,
    code: &str,
) -> Result<(), WorkerError> {
    let state = match state {
        WorkState::RetryWait => "retry_wait",
        WorkState::RateLimited => "rate_limited",
        _ => {
            return Err(WorkerError::Validation(
                "Only retry states can be scheduled".into(),
            ))
        }
    };
    transaction.execute(
        "UPDATE provider_work_items
         SET state = ?2, available_at = ?3, retry_after_at = ?4,
             last_error_code = ?5, auth_block_reason = NULL,
             lease_owner = NULL, lease_token = NULL,
             lease_expires_at = NULL
         WHERE id = ?1",
        params![work_id, state, available_at, retry_after_at, code],
    )?;
    set_linked_operation_retry(transaction, work_id, available_at, code)?;
    Ok(())
}

fn set_terminal(
    transaction: &Transaction<'_>,
    work_id: &str,
    state: WorkState,
    code: Option<&str>,
    now: i64,
) -> Result<(), WorkerError> {
    let state = match state {
        WorkState::Succeeded => "succeeded",
        WorkState::Failed => "failed",
        WorkState::Cancelled => "cancelled",
        WorkState::OutcomeUnknown => "outcome_unknown",
        _ => {
            return Err(WorkerError::Validation(
                "Only terminal states can complete durable work".into(),
            ))
        }
    };
    transaction.execute(
        "UPDATE provider_work_items
         SET state = ?2, completed_at = ?3, last_error_code = ?4,
             auth_block_reason = NULL,
             lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL,
             retry_after_at = NULL
         WHERE id = ?1",
        params![work_id, state, now, code],
    )?;
    let operation_state = match state {
        "succeeded" => "confirmed",
        "failed" => "failed",
        "cancelled" => "cancelled",
        "outcome_unknown" => "outcome_unknown",
        _ => unreachable!(),
    };
    transaction.execute(
        "UPDATE operations
         SET state = ?2,
             confirmed_at = CASE WHEN ?2 = 'confirmed' THEN ?3 ELSE confirmed_at END,
             error = CASE WHEN ?2 = 'confirmed' THEN NULL ELSE ?4 END
         WHERE id = (
           SELECT operation_id FROM provider_work_items WHERE id = ?1
         ) AND state IN ('pending', 'executing', 'retrying', ?2)",
        params![work_id, operation_state, now, code],
    )?;
    Ok(())
}

fn set_linked_operation_retry(
    transaction: &Transaction<'_>,
    work_id: &str,
    not_before: i64,
    code: &str,
) -> Result<(), WorkerError> {
    transaction.execute(
        "UPDATE operations
         SET state = 'retrying', not_before = ?2, error = ?3
         WHERE id = (
           SELECT operation_id FROM provider_work_items WHERE id = ?1
         ) AND state IN ('executing', 'retrying')",
        params![work_id, not_before, code],
    )?;
    Ok(())
}

fn refund_pre_submission_attempt(
    transaction: &Transaction<'_>,
    work_id: &str,
) -> Result<(), WorkerError> {
    transaction.execute(
        "UPDATE provider_work_items SET attempt_count = MAX(attempt_count - 1, 0)
         WHERE id = ?1",
        [work_id],
    )?;
    transaction.execute(
        "UPDATE operations SET attempts = MAX(attempts - 1, 0)
         WHERE id = (
           SELECT operation_id FROM provider_work_items WHERE id = ?1
         )",
        [work_id],
    )?;
    Ok(())
}

fn set_pre_submission_auth_block(
    transaction: &Transaction<'_>,
    work_id: &str,
    not_before: i64,
    reason: &str,
    code: &str,
) -> Result<(), WorkerError> {
    transaction.execute(
        "UPDATE provider_work_items
         SET state = 'authentication_blocked', auth_block_reason = ?2,
             last_error_code = ?3,
             lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL,
             retry_after_at = NULL
         WHERE id = ?1",
        params![work_id, reason, code],
    )?;
    transaction.execute(
        "UPDATE operations
         SET state = 'retrying', not_before = ?2, error = ?3
         WHERE id = (
           SELECT operation_id FROM provider_work_items WHERE id = ?1
         ) AND state IN ('executing', 'retrying')",
        params![work_id, not_before, code],
    )?;
    Ok(())
}

fn clear_resumed_linked_operation_errors(
    transaction: &Transaction<'_>,
    account_id: &str,
    reason: &str,
    not_before: i64,
) -> Result<(), WorkerError> {
    transaction.execute(
        "UPDATE operations
         SET state = 'retrying', not_before = ?3, error = NULL
         WHERE id IN (
           SELECT operation_id FROM provider_work_items
           WHERE account_id = ?1 AND state = 'authentication_blocked'
             AND auth_block_reason = ?2 AND operation_id IS NOT NULL
         ) AND state = 'retrying'",
        params![account_id, reason, not_before],
    )?;
    Ok(())
}

fn backoff_ms(attempts: i64, work_id: &str, config: &WorkerConfig) -> i64 {
    let exponent = attempts.saturating_sub(1).clamp(0, 30) as u32;
    let exponential = config
        .base_backoff_ms
        .saturating_mul(1_i64 << exponent)
        .min(config.max_backoff_ms);
    let jitter_bound = (exponential / 4).max(1) as u64;
    let mut value = config.jitter_seed ^ attempts as u64;
    for byte in work_id.as_bytes() {
        value ^= *byte as u64;
        value = value.wrapping_mul(0x100000001b3);
    }
    exponential
        .saturating_add((value % (jitter_bound + 1)) as i64)
        .min(config.max_backoff_ms)
}

fn bounded_nonempty(value: &str, field: &str, maximum_bytes: usize) -> Result<(), WorkerError> {
    let length = value.len();
    if length == 0 || length > maximum_bytes {
        return Err(WorkerError::Validation(format!(
            "{field} must contain between 1 and {maximum_bytes} UTF-8 bytes"
        )));
    }
    Ok(())
}

fn validate_claim_payload(claim: &ClaimedWork) -> Result<(), WorkerError> {
    let actual = Sha256::digest(claim.payload_json.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if actual != claim.payload_fingerprint_hex {
        return Err(WorkerError::Conflict(
            "Claimed work payload no longer matches its durable fingerprint".into(),
        ));
    }
    Ok(())
}

fn validate_error_code(code: &str) -> Result<(), WorkerError> {
    bounded_nonempty(code, "Provider error code", MAX_ERROR_CODE_BYTES)?;
    if !code.bytes().enumerate().all(|(index, byte)| {
        byte.is_ascii_lowercase() || (index > 0 && (byte.is_ascii_digit() || byte == b'_'))
    }) {
        return Err(WorkerError::Validation(
            "Provider error code must be a lowercase bounded identifier".into(),
        ));
    }
    Ok(())
}

fn system_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::sync::{Arc, Barrier};

    use tempfile::tempdir;

    use super::*;
    use crate::store::MuxStore;

    fn test_config() -> WorkerConfig {
        WorkerConfig {
            max_in_flight: 3,
            max_per_account: 2,
            lease_ms: 1_000,
            base_backoff_ms: 100,
            max_backoff_ms: 10_000,
            jitter_seed: 7,
            idle_poll_ms: 60_000,
            heartbeat_ms: 200,
        }
    }

    fn configured_worker(accounts: &[&str]) -> (tempfile::TempDir, PathBuf, DurableWorker) {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("worker.db");
        drop(MuxStore::open(&path, false).expect("worker schema"));
        let connection = Connection::open(&path).expect("worker fixture connection");
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .unwrap();
        for account in accounts {
            connection
                .execute(
                    "INSERT INTO accounts(id, name, email, color, provider)
                     VALUES(?1, ?1, ?2, '#000000', 'jmap')",
                    params![account, format!("{account}@example.com")],
                )
                .unwrap();
            connection
                .execute(
                    "INSERT INTO provider_accounts(
                       account_id, provider_kind, remote_account_id, auth_state,
                       sync_state, credential_ref, created_at, updated_at
                     ) VALUES(?1, 'jmap', ?1, 'ready', 'scheduled', ?2, 0, 0)",
                    params![account, format!("vault:v1:{account}")],
                )
                .unwrap();
        }
        drop(connection);
        let worker = DurableWorker::new(&path, test_config()).unwrap();
        (directory, path, worker)
    }

    fn item(id: &str, account_id: &str, kind: WorkKind) -> NewWorkItem {
        NewWorkItem {
            id: id.into(),
            account_id: account_id.into(),
            operation_id: None,
            kind,
            scope: format!("test:v1:{id}"),
            ordering_key: format!("test-order:v1:{id}"),
            payload_json: "{}".into(),
            priority: 0,
            available_at: 0,
            max_attempts: 4,
        }
    }

    fn no_op_projector(
        _transaction: &Transaction<'_>,
        _work: &ClaimedWork,
        _projection: &WorkerProjection,
    ) -> Result<(), WorkerError> {
        Ok(())
    }

    fn local_success() -> WorkerOutcome {
        WorkerOutcome::Succeeded {
            projection: WorkerProjection::LocalOperation,
        }
    }

    fn sample_projection_batch() -> ProviderBatch {
        serde_json::from_value(serde_json::json!({
            "muxAccountId": "account-a",
            "batchId": "projection-batch",
            "expectedPriorCursor": null,
            "cursor": {
                "muxAccountId": "account-a",
                "scope": { "kind": "account" },
                "value": "projection-cursor"
            },
            "observedAt": 100,
            "threadUpserts": [],
            "messageUpserts": [],
            "containerUpserts": [],
            "membershipChanges": [],
            "tombstones": []
        }))
        .expect("valid projection batch")
    }

    #[test]
    fn claim_commits_before_adapter_io_and_ack_uses_a_new_short_transaction() {
        let (_directory, path, worker) = configured_worker(&["account-a"]);
        worker
            .enqueue(item("sync-1", "account-a", WorkKind::Sync), 0)
            .unwrap();
        let adapter = |_: &ClaimedWork, _: &WorkerExecutionContext| {
            let connection = Connection::open(&path).expect("parallel connection during I/O");
            connection.busy_timeout(Duration::from_millis(50)).unwrap();
            connection
                .execute(
                    "INSERT INTO meta(key, value) VALUES('adapter_io_marker', 'written')",
                    [],
                )
                .expect("no worker transaction is held across adapter I/O");
            local_success()
        };
        let result = worker
            .run_cycle("worker-a", &adapter, &no_op_projector, &|| 1)
            .unwrap();
        assert_eq!(result.claimed, 1);
        assert_eq!(result.succeeded, 1);
        let verify = Connection::open(path).unwrap();
        assert_eq!(
            verify
                .query_row(
                    "SELECT value FROM meta WHERE key = 'adapter_io_marker'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "written"
        );
    }

    #[test]
    fn leases_are_exclusive_and_unmarked_expired_work_is_safe_to_retry() {
        let (_directory, _path, worker) = configured_worker(&["account-a"]);
        worker
            .enqueue(item("safe", "account-a", WorkKind::Mutation), 0)
            .unwrap();
        worker
            .enqueue(item("send", "account-a", WorkKind::Send), 0)
            .unwrap();
        let first = worker.claim_available(10, "worker-a", 2).unwrap();
        assert_eq!(first.len(), 2);
        assert!(worker
            .claim_available(500, "worker-b", 2)
            .unwrap()
            .is_empty());

        let recovery = worker.recover_expired(1_010).unwrap();
        assert_eq!(recovery.safe_retried, 2);
        assert_eq!(recovery.send_outcome_unknown, 0);
        assert_eq!(
            worker.snapshot("send").unwrap().unwrap().state,
            WorkState::RetryWait
        );
        assert!(matches!(
            worker.acknowledge_success_with_projection(&first[0], 1_011, |_| Ok(())),
            Err(WorkerError::LeaseLost)
        ));
        assert!(worker
            .claim_available(1_109, "worker-b", 2)
            .unwrap()
            .is_empty());
        let retry_at = worker
            .snapshot("safe")
            .unwrap()
            .unwrap()
            .available_at
            .max(worker.snapshot("send").unwrap().unwrap().available_at);
        let reclaimed = worker.claim_available(retry_at, "worker-b", 2).unwrap();
        assert_eq!(reclaimed.len(), 2);
    }

    #[test]
    fn retry_rate_limit_auth_and_failure_transitions_are_deterministic() {
        let (_directory, _path, worker) = configured_worker(&["account-a"]);
        for (created_at, id) in ["retry", "rate", "auth", "failure"].into_iter().enumerate() {
            worker
                .enqueue(item(id, "account-a", WorkKind::Sync), created_at as i64)
                .unwrap();
        }

        let retry = worker.claim_available(10, "worker", 1).unwrap().remove(0);
        assert_eq!(
            worker
                .acknowledge(
                    &retry,
                    WorkerOutcome::RetryableFailure {
                        code: "temporary".into(),
                    },
                    20,
                )
                .unwrap(),
            WorkState::RetryWait
        );
        let retry_snapshot = worker.snapshot("retry").unwrap().unwrap();
        assert!((120..=145).contains(&retry_snapshot.available_at));
        assert_eq!(retry_snapshot.attempt_count, 1);

        let rate = worker.claim_available(10, "worker", 1).unwrap().remove(0);
        assert_eq!(rate.id, "rate");
        assert_eq!(
            worker
                .acknowledge(
                    &rate,
                    WorkerOutcome::RateLimited {
                        code: "quota".into(),
                        retry_after_at: 2_000,
                    },
                    20,
                )
                .unwrap(),
            WorkState::RateLimited
        );
        assert_eq!(
            worker.snapshot("rate").unwrap().unwrap().available_at,
            2_000
        );

        let auth = worker.claim_available(10, "worker", 1).unwrap().remove(0);
        assert_eq!(auth.id, "auth");
        assert_eq!(
            worker
                .acknowledge(
                    &auth,
                    WorkerOutcome::AuthenticationExpired {
                        code: "expired".into(),
                    },
                    20,
                )
                .unwrap(),
            WorkState::AuthenticationBlocked
        );
        assert_eq!(
            worker
                .snapshot("auth")
                .unwrap()
                .unwrap()
                .auth_block_reason
                .as_deref(),
            Some("provider_reauthorization")
        );
        assert!(
            worker
                .claim_available(10_000, "worker", 3)
                .unwrap()
                .is_empty(),
            "authentication expiry blocks every scope for the provider account"
        );
        assert_eq!(worker.resume_after_auth("account-a", 30).unwrap(), 1);
        assert_eq!(
            worker.snapshot("auth").unwrap().unwrap().state,
            WorkState::Queued
        );
        assert_eq!(
            worker.snapshot("auth").unwrap().unwrap().auth_block_reason,
            None
        );

        let failure = worker.claim_available(31, "worker", 3).unwrap();
        let failure = failure.iter().find(|work| work.id == "failure").unwrap();
        assert_eq!(
            worker
                .acknowledge(
                    failure,
                    WorkerOutcome::PermanentFailure {
                        code: "invalid".into(),
                    },
                    32,
                )
                .unwrap(),
            WorkState::Failed
        );
    }

    #[test]
    fn one_account_losing_its_credential_leaves_other_accounts_working() {
        let (_directory, path, worker) = configured_worker(&["account-a", "account-b"]);
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "UPDATE provider_accounts SET credential_ref = 'ref_' || account_id",
                [],
            )
            .unwrap();
        drop(connection);
        worker
            .enqueue(item("a-sync", "account-a", WorkKind::Sync), 0)
            .unwrap();
        worker
            .enqueue(item("b-sync", "account-b", WorkKind::Sync), 0)
            .unwrap();

        let claims = worker.claim_available(1, "worker", 4).unwrap();
        let blocked = claims
            .iter()
            .find(|claim| claim.account_id == "account-a")
            .expect("account-a work");
        assert_eq!(
            worker
                .acknowledge(blocked, WorkerOutcome::CredentialUnavailable, 2)
                .unwrap(),
            WorkState::AuthenticationBlocked
        );

        let connection = Connection::open(&path).unwrap();
        // Only the account that lost its credential is blocked.
        let states: Vec<(String, String)> = connection
            .prepare("SELECT account_id, auth_state FROM provider_accounts ORDER BY account_id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            states,
            vec![
                (
                    "account-a".to_string(),
                    "reauthorization_required".to_string()
                ),
                ("account-b".to_string(), "ready".to_string())
            ]
        );
        drop(connection);

        // The healthy account still takes acknowledgements normally.
        let healthy = claims
            .iter()
            .find(|claim| claim.account_id == "account-b")
            .expect("account-b work");
        assert_eq!(
            worker
                .acknowledge(
                    healthy,
                    WorkerOutcome::RetryableFailure {
                        code: "transient".into()
                    },
                    3
                )
                .unwrap(),
            WorkState::RetryWait
        );
        assert!(
            worker
                .snapshot("b-sync")
                .unwrap()
                .unwrap()
                .auth_block_reason
                .is_none(),
            "a healthy account must not inherit another account's auth block"
        );
    }

    #[test]
    fn credential_unavailable_send_is_pre_submission_safe_and_preserves_retry_budget() {
        let (_directory, path, worker) = configured_worker(&["account-a"]);
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "UPDATE provider_accounts SET credential_ref = 'vault_entry_account_a'
                 WHERE account_id = 'account-a'",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO operations(
                   id, field, kind, old_value, new_value, payload_json, state,
                   created_at, not_before
                 ) VALUES(
                   'locked-send-operation', 'send', 'send', 'draft', 'submitted',
                   '{\"accountId\":\"account-a\",\"messageId\":\"locked-message\"}',
                   'pending', 0, 0
                 )",
                [],
            )
            .unwrap();
        drop(connection);
        let mut send = item("locked-send", "account-a", WorkKind::Send);
        send.operation_id = Some("locked-send-operation".into());
        send.ordering_key = "send:v1:locked-message".into();
        send.max_attempts = 1;
        worker.enqueue(send, 0).unwrap();

        let claim = worker.claim_available(1, "worker", 1).unwrap().remove(0);
        assert_eq!(claim.kind, WorkKind::Send);
        assert_eq!(
            worker
                .acknowledge(&claim, WorkerOutcome::CredentialUnavailable, 2)
                .unwrap(),
            WorkState::AuthenticationBlocked
        );
        let blocked = worker.snapshot("locked-send").unwrap().unwrap();
        assert_eq!(blocked.attempt_count, 0);
        assert_eq!(
            blocked.last_error_code.as_deref(),
            Some("credential_unavailable")
        );
        assert_eq!(
            blocked.auth_block_reason.as_deref(),
            Some("provider_reauthorization")
        );
        let connection = Connection::open(&path).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT state, attempts, error FROM operations
                     WHERE id = 'locked-send-operation'",
                    [],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, Option<String>>(2)?,
                        ))
                    },
                )
                .unwrap(),
            ("retrying".into(), 0, Some("credential_unavailable".into()))
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT auth_state, auth_block_reason FROM provider_accounts
                     WHERE account_id = 'account-a'",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .unwrap(),
            (
                "reauthorization_required".into(),
                "provider_reauthorization".into()
            )
        );
        drop(connection);

        // Reauthorizing the account is the only way back, and it costs no attempt.
        assert_eq!(worker.resume_after_auth("account-a", 4).unwrap(), 1);
        let resumed = worker.snapshot("locked-send").unwrap().unwrap();
        assert_eq!(resumed.state, WorkState::Queued);
        assert_eq!(resumed.attempt_count, 0);
        assert_eq!(resumed.auth_block_reason, None);
        let second_claim = worker.claim_available(4, "worker", 1).unwrap();
        assert_eq!(
            second_claim.len(),
            1,
            "an unavailable credential consumed no attempt"
        );
        assert_eq!(second_claim[0].attempt, 1);
    }

    #[test]
    fn scheduling_is_bounded_globally_and_per_account() {
        let (_directory, _path, worker) = configured_worker(&["account-a", "account-b"]);
        for index in 0..5 {
            worker
                .enqueue(item(&format!("a-{index}"), "account-a", WorkKind::Sync), 0)
                .unwrap();
        }
        for index in 0..3 {
            worker
                .enqueue(item(&format!("b-{index}"), "account-b", WorkKind::Sync), 0)
                .unwrap();
        }
        let claims = worker.claim_available(1, "worker-a", 20).unwrap();
        assert_eq!(claims.len(), 3);
        let by_account = claims.iter().fold(BTreeMap::new(), |mut counts, work| {
            *counts.entry(work.account_id.as_str()).or_insert(0) += 1;
            counts
        });
        assert!(by_account.values().all(|count| *count <= 2));
        assert!(worker
            .claim_available(2, "worker-b", 20)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn distinct_scopes_execute_concurrently_within_the_claim_bound() {
        let (_directory, _path, worker) = configured_worker(&["account-a"]);
        worker
            .enqueue(item("parallel-a", "account-a", WorkKind::Sync), 0)
            .unwrap();
        worker
            .enqueue(item("parallel-b", "account-a", WorkKind::Sync), 0)
            .unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let adapter = {
            let barrier = Arc::clone(&barrier);
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            move |_: &ClaimedWork, _: &WorkerExecutionContext| {
                let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(current, Ordering::SeqCst);
                barrier.wait();
                active.fetch_sub(1, Ordering::SeqCst);
                local_success()
            }
        };
        let result = worker
            .run_cycle("worker", &adapter, &no_op_projector, &|| 1)
            .unwrap();
        assert_eq!(result.claimed, 2);
        assert_eq!(result.succeeded, 2);
        assert_eq!(maximum.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn stale_success_ack_cannot_commit_projection_or_completion() {
        let (_directory, path, worker) = configured_worker(&["account-a"]);
        worker
            .enqueue(item("stale", "account-a", WorkKind::Mutation), 0)
            .unwrap();
        let stale = worker
            .claim_available(1, "old-worker", 1)
            .unwrap()
            .remove(0);
        worker.recover_expired(1_001).unwrap();
        let result = worker.acknowledge_success_with_projection(&stale, 1_002, |transaction| {
            transaction.execute(
                "INSERT INTO meta(key, value) VALUES('stale_projection', 'visible')",
                [],
            )?;
            Ok(())
        });
        assert!(matches!(result, Err(WorkerError::LeaseLost)));
        let verify = Connection::open(path).unwrap();
        assert_eq!(
            verify
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM meta WHERE key = 'stale_projection')",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
        assert_eq!(
            worker.snapshot("stale").unwrap().unwrap().state,
            WorkState::RetryWait
        );
    }

    #[test]
    fn post_io_clock_fences_expired_success_before_projection() {
        let (_directory, _path, worker) = configured_worker(&["account-a"]);
        worker
            .enqueue(item("slow", "account-a", WorkKind::Sync), 0)
            .unwrap();
        let clock_value = Arc::new(std::sync::atomic::AtomicI64::new(1));
        let projection_calls = Arc::new(AtomicUsize::new(0));
        let adapter = {
            let clock_value = Arc::clone(&clock_value);
            move |_: &ClaimedWork, _: &WorkerExecutionContext| {
                clock_value.store(1_001, Ordering::SeqCst);
                local_success()
            }
        };
        let projector = {
            let projection_calls = Arc::clone(&projection_calls);
            move |_: &Transaction<'_>, _: &ClaimedWork, _: &WorkerProjection| {
                projection_calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        };
        let clock = || clock_value.load(Ordering::SeqCst);
        let result = worker
            .run_cycle("worker", &adapter, &projector, &clock)
            .unwrap();
        assert_eq!(result.claimed, 1);
        assert_eq!(result.errors.len(), 1);
        assert_eq!(projection_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            worker.snapshot("slow").unwrap().unwrap().state,
            WorkState::Executing
        );
    }

    #[test]
    fn linked_operation_claim_retry_and_projection_ack_are_atomic() {
        let (_directory, path, worker) = configured_worker(&["account-a", "account-b"]);
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .unwrap();
        connection
            .execute(
                "INSERT INTO threads(
                   id, account_id, subject, participants, snippet, latest_at, message_count,
                   remote_in_inbox, remote_unread, remote_starred, has_attachment, has_invite,
                   has_link, has_from_me, category, attachment_names
                 ) VALUES(1, 'account-a', 'Linked', 'A', '', 0, 0, 1, 1, 0, 0, 0, 0, 0, '', '')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO operations(
                   id, thread_id, field, kind, old_value, new_value, state,
                   created_at, not_before
                 ) VALUES('operation-1', 1, 'starred', 'star', '0', '1', 'pending', 0, 0)",
                [],
            )
            .unwrap();
        drop(connection);

        let mut work = item("linked", "account-a", WorkKind::Mutation);
        work.operation_id = Some("operation-1".into());
        worker.enqueue(work, 0).unwrap();
        let claim = worker.claim_available(1, "worker", 1).unwrap().remove(0);
        let verify = Connection::open(&path).unwrap();
        assert_eq!(
            verify
                .query_row(
                    "SELECT state, attempts FROM operations WHERE id = 'operation-1'",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .unwrap(),
            ("executing".into(), 1)
        );
        drop(verify);
        assert!(matches!(
            worker.acknowledge(&claim, local_success(), 2),
            Err(WorkerError::Conflict(_))
        ));
        assert_eq!(
            worker.snapshot("linked").unwrap().unwrap().state,
            WorkState::Executing,
            "linked success cannot bypass the fenced projection callback"
        );
        worker
            .acknowledge(
                &claim,
                WorkerOutcome::RetryableFailure {
                    code: "temporary".into(),
                },
                2,
            )
            .unwrap();
        let retry_at = worker.snapshot("linked").unwrap().unwrap().available_at;
        let verify = Connection::open(&path).unwrap();
        assert_eq!(
            verify
                .query_row(
                    "SELECT state, not_before FROM operations WHERE id = 'operation-1'",
                    [],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .unwrap(),
            ("retrying".into(), retry_at)
        );
        drop(verify);

        let claim = worker
            .claim_available(retry_at, "worker", 1)
            .unwrap()
            .remove(0);
        worker
            .acknowledge_success_with_projection(&claim, retry_at + 1, |transaction| {
                transaction.execute("UPDATE threads SET remote_starred = 1 WHERE id = 1", [])?;
                Ok(())
            })
            .unwrap();
        let verify = Connection::open(&path).unwrap();
        assert_eq!(
            verify
                .query_row(
                    "SELECT state FROM operations WHERE id = 'operation-1'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "confirmed"
        );
        assert_eq!(
            verify
                .query_row(
                    "SELECT remote_starred FROM threads WHERE id = 1",
                    [],
                    |row| { row.get::<_, i64>(0) }
                )
                .unwrap(),
            1
        );

        let mut mismatched = item("wrong-account", "account-b", WorkKind::Mutation);
        mismatched.operation_id = Some("operation-1".into());
        assert!(worker.enqueue(mismatched, 10).is_err());
    }

    #[test]
    fn linked_send_restart_marks_work_and_operation_outcome_unknown() {
        let (directory, path, worker) = configured_worker(&["account-a"]);
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "INSERT INTO operations(
                   id, field, kind, old_value, new_value, payload_json, state,
                   created_at, not_before
                 ) VALUES(
                   'send-operation', 'send', 'send', 'draft', 'submitted',
                   '{\"accountId\":\"account-a\",\"messageId\":\"stable-message\"}',
                   'pending', 0, 0
                 )",
                [],
            )
            .unwrap();
        drop(connection);
        let mut send = item("linked-send", "account-a", WorkKind::Send);
        send.operation_id = Some("send-operation".into());
        send.ordering_key = "send:v1:stable-message".into();
        worker.enqueue(send, 0).unwrap();
        let claim = worker.claim_available(1, "worker", 1).unwrap().remove(0);
        assert_eq!(claim.kind, WorkKind::Send);
        mark_send_submission_started(&path, &claim, 2).unwrap();
        assert_eq!(
            worker
                .snapshot("linked-send")
                .unwrap()
                .unwrap()
                .last_error_code
                .as_deref(),
            Some(SEND_SUBMISSION_STARTED_CODE)
        );
        drop(worker);

        drop(MuxStore::open(&path, false).expect("application restart recovers operation"));
        let restarted =
            DurableWorker::new(directory.path().join("worker.db"), test_config()).unwrap();
        let recovery = restarted.recover_expired(1_001).unwrap();
        assert_eq!(recovery.send_outcome_unknown, 1);
        assert_eq!(
            restarted.snapshot("linked-send").unwrap().unwrap().state,
            WorkState::OutcomeUnknown
        );
        let verify = Connection::open(path).unwrap();
        assert_eq!(
            verify
                .query_row(
                    "SELECT state FROM operations WHERE id = 'send-operation'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "outcome_unknown"
        );
        assert!(restarted
            .claim_available(10_000, "other-worker", 1)
            .unwrap()
            .is_empty());
    }

    struct FailOnceProjector {
        calls: Arc<AtomicUsize>,
    }

    impl WorkerProjector for FailOnceProjector {
        fn apply_success(
            &self,
            _transaction: &Transaction<'_>,
            _work: &ClaimedWork,
            _projection: &WorkerProjection,
        ) -> Result<(), WorkerError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(WorkerError::Sqlite(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                    Some("database is busy".into()),
                )));
            }
            Ok(())
        }
    }

    #[test]
    fn controller_retries_transient_ack_without_reinvoking_adapter() {
        let (_directory, _path, worker) = configured_worker(&["account-a"]);
        worker
            .enqueue(item("ack-retry", "account-a", WorkKind::Sync), 0)
            .unwrap();
        let adapter_calls = Arc::new(AtomicUsize::new(0));
        let projector_calls = Arc::new(AtomicUsize::new(0));
        let adapter = {
            let adapter_calls = Arc::clone(&adapter_calls);
            move |_: &ClaimedWork, _: &WorkerExecutionContext| {
                adapter_calls.fetch_add(1, Ordering::SeqCst);
                local_success()
            }
        };
        let (sender, receiver) = mpsc::channel();
        let controller = WorkerController::start(
            worker,
            "controller-retry".into(),
            adapter,
            FailOnceProjector {
                calls: Arc::clone(&projector_calls),
            },
            move |result| {
                let _ = sender.send(result);
            },
        )
        .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut claimed = false;
        let mut ack_failed = false;
        let mut succeeded = false;
        while std::time::Instant::now() < deadline && !succeeded {
            let result = receiver.recv_timeout(Duration::from_millis(500)).unwrap();
            claimed |= result.claimed == 1;
            ack_failed |= !result.errors.is_empty();
            succeeded |= result.succeeded == 1;
            if ack_failed {
                controller.wake().unwrap();
            }
        }
        assert!(claimed && ack_failed && succeeded);
        assert_eq!(adapter_calls.load(Ordering::SeqCst), 1);
        assert_eq!(projector_calls.load(Ordering::SeqCst), 2);
        controller.stop().unwrap();
    }

    #[test]
    fn deterministic_projection_rejection_does_not_spin_and_send_becomes_unknown() {
        let (_directory, _path, worker) = configured_worker(&["account-a"]);
        worker
            .enqueue(item("rejected-send", "account-a", WorkKind::Send), 0)
            .unwrap();
        let adapter_calls = Arc::new(AtomicUsize::new(0));
        let projector_calls = Arc::new(AtomicUsize::new(0));
        let adapter = {
            let adapter_calls = Arc::clone(&adapter_calls);
            move |_: &ClaimedWork, _: &WorkerExecutionContext| {
                adapter_calls.fetch_add(1, Ordering::SeqCst);
                local_success()
            }
        };
        let projector = {
            let projector_calls = Arc::clone(&projector_calls);
            move |_: &Transaction<'_>, _: &ClaimedWork, _: &WorkerProjection| {
                projector_calls.fetch_add(1, Ordering::SeqCst);
                Err(WorkerError::Conflict(
                    "deterministic projection rejection".into(),
                ))
            }
        };
        let worker_probe = worker.clone();
        let (sender, receiver) = mpsc::channel();
        let controller = WorkerController::start(
            worker,
            "rejected-ack-worker".into(),
            adapter,
            projector,
            move |result| {
                let _ = sender.send(result);
            },
        )
        .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut outcome_unknown = false;
        while std::time::Instant::now() < deadline && !outcome_unknown {
            match receiver.recv_timeout(Duration::from_millis(500)) {
                Ok(result) => outcome_unknown |= result.outcome_unknown == 1,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        assert!(outcome_unknown);
        assert_eq!(adapter_calls.load(Ordering::SeqCst), 1);
        assert_eq!(projector_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            worker_probe
                .snapshot("rejected-send")
                .unwrap()
                .unwrap()
                .state,
            WorkState::OutcomeUnknown
        );
        controller.stop().unwrap();
    }

    #[test]
    fn same_scope_is_head_of_line_ordered_and_payload_identity_is_immutable() {
        let (_directory, path, worker) = configured_worker(&["account-a"]);
        let mut first = item("first", "account-a", WorkKind::Mutation);
        first.scope = "thread:v1:remote-42".into();
        first.payload_json = r#"{"starred":true}"#.into();
        let mut second = item("second", "account-a", WorkKind::Mutation);
        second.scope = first.scope.clone();
        second.payload_json = r#"{"starred":false}"#.into();
        worker.enqueue(first, 10).unwrap();
        worker.enqueue(second, 11).unwrap();

        let claim = worker.claim_available(20, "worker", 3).unwrap();
        assert_eq!(claim.len(), 1);
        assert_eq!(claim[0].id, "first");
        assert_eq!(
            claim[0].payload_fingerprint_hex,
            "447389c1c246ed9e9fedc02f6f74a4108c8e481b506d8b3beb1ac2f61a8b02cf"
        );
        let tamper = Connection::open(path).unwrap().execute(
            "UPDATE provider_work_items SET payload_json = '{\"starred\":false}' WHERE id = 'first'",
            [],
        );
        assert!(tamper.is_err(), "claimed adapter input is immutable");
        worker
            .acknowledge_success_with_projection(&claim[0], 21, |_| Ok(()))
            .unwrap();
        let next = worker.claim_available(22, "worker", 3).unwrap();
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].id, "second");
    }

    #[test]
    fn generic_demo_account_can_queue_and_complete_without_provider_projection() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("demo-worker.db");
        drop(MuxStore::open(&path, false).unwrap());
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "INSERT INTO accounts(id, name, email, color, provider)
                 VALUES('demo', 'Demo', 'demo@example.com', '#000000', 'fake')",
                [],
            )
            .unwrap();
        drop(connection);
        let worker = DurableWorker::new(&path, test_config()).unwrap();
        worker
            .enqueue(item("demo-mutation", "demo", WorkKind::Mutation), 0)
            .unwrap();
        let claim = worker.claim_available(1, "worker", 1).unwrap().remove(0);
        worker
            .acknowledge_success_with_projection(&claim, 2, |_| Ok(()))
            .unwrap();
        assert_eq!(
            worker.snapshot("demo-mutation").unwrap().unwrap().state,
            WorkState::Succeeded
        );
    }

    #[test]
    fn cancellation_is_cooperative_but_never_interrupts_an_executing_send() {
        let (_directory, _path, worker) = configured_worker(&["account-a"]);
        worker
            .enqueue(item("queued", "account-a", WorkKind::Sync), 0)
            .unwrap();
        assert_eq!(
            worker.request_cancellation("queued", 1).unwrap(),
            WorkState::Cancelled
        );

        worker
            .enqueue(item("executing", "account-a", WorkKind::Mutation), 0)
            .unwrap();
        let mutation = worker.claim_available(2, "worker", 1).unwrap().remove(0);
        assert_eq!(
            worker.request_cancellation("executing", 3).unwrap(),
            WorkState::Executing
        );
        assert!(
            worker
                .snapshot("executing")
                .unwrap()
                .unwrap()
                .cancel_requested
        );
        assert_eq!(
            worker
                .acknowledge(
                    &mutation,
                    WorkerOutcome::RetryableFailure {
                        code: "raced_temporary_failure".into(),
                    },
                    4,
                )
                .unwrap(),
            WorkState::Cancelled
        );
        assert_eq!(
            worker
                .snapshot("executing")
                .unwrap()
                .unwrap()
                .last_error_code
                .as_deref(),
            Some("cancelled_by_request")
        );

        worker
            .enqueue(item("send", "account-a", WorkKind::Send), 0)
            .unwrap();
        let send = worker.claim_available(5, "worker", 1).unwrap().remove(0);
        assert!(matches!(
            worker.request_cancellation("send", 6),
            Err(WorkerError::Conflict(_))
        ));
        assert!(matches!(
            worker.acknowledge(&send, WorkerOutcome::Cancelled, 7),
            Err(WorkerError::Validation(_))
        ));
        assert_eq!(
            worker
                .acknowledge(
                    &send,
                    WorkerOutcome::OutcomeUnknown {
                        code: "connection_lost_after_submit".into(),
                    },
                    8,
                )
                .unwrap(),
            WorkState::OutcomeUnknown
        );
    }

    #[test]
    fn acknowledgements_reject_tampered_claim_payloads_and_unbounded_error_text() {
        let (_directory, _path, worker) = configured_worker(&["account-a"]);
        worker
            .enqueue(item("tampered", "account-a", WorkKind::Sync), 0)
            .unwrap();
        let mut claim = worker.claim_available(1, "worker", 1).unwrap().remove(0);
        claim.payload_json = r#"{"ambientSecret":"must-not-persist"}"#.into();
        assert!(matches!(
            worker.acknowledge_success_with_projection(&claim, 2, |_| Ok(())),
            Err(WorkerError::Conflict(_))
        ));
        assert_eq!(
            worker.snapshot("tampered").unwrap().unwrap().state,
            WorkState::Executing
        );

        worker
            .enqueue(item("unsafe-code", "account-a", WorkKind::Sync), 3)
            .unwrap();
        let unsafe_claim = worker.claim_available(3, "worker", 1).unwrap().remove(0);
        assert!(matches!(
            worker.acknowledge(
                &unsafe_claim,
                WorkerOutcome::PermanentFailure {
                    code: "Bearer token-value".into(),
                },
                4,
            ),
            Err(WorkerError::Validation(_))
        ));
        assert_eq!(
            worker.snapshot("unsafe-code").unwrap().unwrap().state,
            WorkState::Executing,
            "raw provider text must never reach durable error fields"
        );
    }

    #[test]
    fn non_idempotent_send_needs_explicit_pre_submission_rejection_to_retry() {
        let (_directory, _path, worker) = configured_worker(&["account-a"]);
        worker
            .enqueue(item("safe-reject", "account-a", WorkKind::Send), 0)
            .unwrap();
        worker
            .enqueue(item("ambiguous", "account-a", WorkKind::Send), 0)
            .unwrap();
        let claims = worker.claim_available(1, "worker", 2).unwrap();
        let safe_reject = claims.iter().find(|work| work.id == "safe-reject").unwrap();
        let ambiguous = claims.iter().find(|work| work.id == "ambiguous").unwrap();
        assert_eq!(
            worker
                .acknowledge(
                    safe_reject,
                    WorkerOutcome::RejectedBeforeSubmission {
                        code: "offline_preflight".into(),
                    },
                    2,
                )
                .unwrap(),
            WorkState::RetryWait
        );
        assert_eq!(
            worker
                .acknowledge(
                    ambiguous,
                    WorkerOutcome::RetryableFailure {
                        code: "socket_timeout".into(),
                    },
                    2,
                )
                .unwrap(),
            WorkState::OutcomeUnknown
        );
    }

    #[test]
    fn heartbeat_validation_and_runtime_renewal_cover_long_adapter_io() {
        let (_directory, _path, worker) = configured_worker(&["account-a"]);
        let mut invalid = test_config();
        invalid.heartbeat_ms = 334;
        assert!(matches!(
            DurableWorker::new(worker.database_path.clone(), invalid),
            Err(WorkerError::Validation(_))
        ));

        worker
            .enqueue(item("long-io", "account-a", WorkKind::Sync), 0)
            .unwrap();
        let adapter_calls = Arc::new(AtomicUsize::new(0));
        let adapter = {
            let adapter_calls = Arc::clone(&adapter_calls);
            move |_: &ClaimedWork, _: &WorkerExecutionContext| {
                adapter_calls.fetch_add(1, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(1_250));
                local_success()
            }
        };
        let worker_probe = worker.clone();
        let (sender, receiver) = mpsc::channel();
        let controller = WorkerController::start(
            worker,
            "heartbeat-worker".into(),
            adapter,
            no_op_projector,
            move |result| {
                let _ = sender.send(result);
            },
        )
        .unwrap();
        thread::sleep(Duration::from_millis(1_050));
        assert!(worker_probe
            .claim_available(system_now_ms(), "competing-worker", 1)
            .unwrap()
            .is_empty());
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut succeeded = false;
        while std::time::Instant::now() < deadline {
            match receiver.recv_timeout(Duration::from_millis(500)) {
                Ok(result) if result.succeeded == 1 => {
                    succeeded = true;
                    break;
                }
                Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        assert!(succeeded, "heartbeat keeps the fenced success lease alive");
        assert_eq!(adapter_calls.load(Ordering::SeqCst), 1);
        controller.stop().unwrap();
    }

    #[test]
    fn stop_is_prompt_for_noncooperative_adapter_and_cancels_context() {
        let (_directory, _path, worker) = configured_worker(&["account-a"]);
        worker
            .enqueue(item("blocked", "account-a", WorkKind::Sync), 0)
            .unwrap();
        let release = Arc::new(AtomicBool::new(false));
        let (context_sender, context_receiver) = mpsc::channel();
        let adapter = {
            let release = Arc::clone(&release);
            move |_: &ClaimedWork, context: &WorkerExecutionContext| {
                let _ = context_sender.send(context.clone());
                while !release.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(5));
                }
                local_success()
            }
        };
        let controller = WorkerController::start(
            worker,
            "stop-worker".into(),
            adapter,
            no_op_projector,
            |_| {},
        )
        .unwrap();
        let context = context_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("adapter receives its cancellation-only context");
        let started = std::time::Instant::now();
        controller.stop().unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "controller stop must not join a noncooperative adapter"
        );
        assert!(context.is_cancelled());
        release.store(true, Ordering::Release);
    }

    #[test]
    fn durable_cancellation_reaches_running_adapter_context() {
        let (_directory, _path, worker) = configured_worker(&["account-a"]);
        worker
            .enqueue(item("db-cancel", "account-a", WorkKind::Mutation), 0)
            .unwrap();
        let worker_handle = worker.clone();
        let (started_sender, started_receiver) = mpsc::channel();
        let adapter = move |_: &ClaimedWork, context: &WorkerExecutionContext| {
            let _ = started_sender.send(());
            while !context.is_cancelled() {
                thread::sleep(Duration::from_millis(5));
            }
            WorkerOutcome::Cancelled
        };
        let (sender, receiver) = mpsc::channel();
        let controller = WorkerController::start(
            worker,
            "cancel-worker".into(),
            adapter,
            no_op_projector,
            move |result| {
                let _ = sender.send(result);
            },
        )
        .unwrap();
        started_receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("adapter started");
        assert_eq!(
            worker_handle
                .request_cancellation("db-cancel", system_now_ms())
                .unwrap(),
            WorkState::Executing
        );
        controller.wake().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut cancelled = false;
        while std::time::Instant::now() < deadline {
            match receiver.recv_timeout(Duration::from_millis(500)) {
                Ok(result) if result.cancelled == 1 => {
                    cancelled = true;
                    break;
                }
                Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        assert!(cancelled);
        controller.stop().unwrap();
    }

    #[test]
    fn provider_batch_success_projection_reaches_projector() {
        let (_directory, _path, worker) = configured_worker(&["account-a"]);
        worker
            .enqueue(item("provider-batch", "account-a", WorkKind::Sync), 0)
            .unwrap();
        let seen = Arc::new(Mutex::new(None::<String>));
        let projector = {
            let seen = Arc::clone(&seen);
            move |_: &Transaction<'_>, _: &ClaimedWork, projection: &WorkerProjection| {
                let WorkerProjection::ProviderBatch(batch) = projection else {
                    return Err(WorkerError::Conflict(
                        "expected provider batch projection".into(),
                    ));
                };
                let serialized = serde_json::to_value(batch.as_ref())
                    .expect("provider projection remains serializable");
                *seen.lock().unwrap() = serialized["batchId"].as_str().map(str::to_owned);
                Ok(())
            }
        };
        let adapter = |_: &ClaimedWork, _: &WorkerExecutionContext| WorkerOutcome::Succeeded {
            projection: WorkerProjection::ProviderBatch(Box::new(sample_projection_batch())),
        };
        let result = worker
            .run_cycle("projection-worker", &adapter, &projector, &|| 1)
            .unwrap();
        assert_eq!(result.succeeded, 1);
        assert_eq!(seen.lock().unwrap().as_deref(), Some("projection-batch"));
    }

    #[test]
    fn durable_error_code_columns_reject_raw_text_at_the_database_boundary() {
        let (_directory, path, worker) = configured_worker(&["account-a"]);
        worker
            .enqueue(item("bounded-errors", "account-a", WorkKind::Sync), 0)
            .unwrap();
        let connection = Connection::open(&path).unwrap();
        assert!(connection
            .execute(
                "UPDATE provider_work_items
                 SET last_error_code = 'Bearer secret-value'
                 WHERE id = 'bounded-errors'",
                [],
            )
            .expect_err("raw work error text must be rejected")
            .to_string()
            .contains("provider work error code"));
        assert!(connection
            .execute(
                "UPDATE provider_accounts
                 SET last_error_code = 'OAuth token-value'
                 WHERE account_id = 'account-a'",
                [],
            )
            .expect_err("raw account error text must be rejected")
            .to_string()
            .contains("provider account error code"));
        connection
            .execute(
                "UPDATE provider_work_items
                 SET last_error_code = 'socket_timeout'
                 WHERE id = 'bounded-errors'",
                [],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE provider_accounts
                 SET last_error_code = 'reauthorization_required'
                 WHERE account_id = 'account-a'",
                [],
            )
            .unwrap();
        assert!(connection
            .execute(
                "UPDATE provider_accounts SET auth_state = 'reauthorization_required'
                 WHERE account_id = 'account-a'",
                [],
            )
            .expect_err("account block state requires matching provenance")
            .to_string()
            .contains("auth block provenance"));
        assert!(connection
            .execute(
                "UPDATE provider_work_items SET state = 'authentication_blocked'
                 WHERE id = 'bounded-errors'",
                [],
            )
            .expect_err("work block state requires matching provenance")
            .to_string()
            .contains("auth block provenance"));
    }

    #[test]
    fn reconciling_a_terminal_operation_never_copies_its_human_error_text() {
        let (_directory, path, worker) = configured_worker(&["account-a"]);
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .unwrap();
        connection
            .execute(
                "INSERT INTO threads(
                   id, account_id, subject, participants, snippet, latest_at, message_count,
                   remote_in_inbox, remote_unread, remote_starred, has_attachment, has_invite,
                   has_link, has_from_me, category, attachment_names
                 ) VALUES(1, 'account-a', 'Linked failure', 'A', '', 0, 0,
                          1, 1, 0, 0, 0, 0, 0, '', '')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO operations(
                   id, thread_id, field, kind, old_value, new_value, state,
                   created_at, not_before, error
                 ) VALUES(
                   'failed-operation', 1, 'starred', 'star', '0', '1', 'failed',
                   0, 0, 'Bearer secret-value from provider'
                 )",
                [],
            )
            .unwrap();
        drop(connection);
        let mut work = item("linked-failure", "account-a", WorkKind::Mutation);
        work.operation_id = Some("failed-operation".into());
        worker.enqueue(work, 0).unwrap();

        assert!(worker.claim_available(1, "worker", 1).unwrap().is_empty());
        let snapshot = worker.snapshot("linked-failure").unwrap().unwrap();
        assert_eq!(snapshot.state, WorkState::Failed);
        assert_eq!(
            snapshot.last_error_code.as_deref(),
            Some("linked_operation_failed")
        );
    }

    #[test]
    fn controller_wakes_at_an_orphaned_execution_lease_deadline() {
        let (_directory, _path, worker) = configured_worker(&["account-a"]);
        let now = system_now_ms();
        worker
            .enqueue(item("orphaned", "account-a", WorkKind::Sync), now)
            .unwrap();
        assert_eq!(
            worker
                .claim_available(now, "crashed-worker", 1)
                .unwrap()
                .len(),
            1
        );
        let (sender, receiver) = mpsc::channel();
        let controller = WorkerController::start(
            worker,
            "recovery-worker".into(),
            |_: &ClaimedWork, _: &WorkerExecutionContext| local_success(),
            no_op_projector,
            move |result| {
                let _ = sender.send(result);
            },
        )
        .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut succeeded = false;
        while std::time::Instant::now() < deadline && !succeeded {
            match receiver.recv_timeout(Duration::from_millis(500)) {
                Ok(result) => succeeded |= result.succeeded == 1,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        assert!(
            succeeded,
            "the manager must recover at the durable lease deadline, not its 60s idle poll"
        );
        controller.stop().unwrap();
    }

    #[test]
    fn orphaned_send_recovery_notifies_the_observer_after_outcome_unknown_commits() {
        let (_directory, path, worker) = configured_worker(&["account-a"]);
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .unwrap();
        connection
            .execute(
                "INSERT INTO operations(
                   id, field, kind, old_value, new_value, payload_json, state,
                   created_at, not_before
                 ) VALUES(
                   'orphaned-send-operation', 'send', 'send', 'draft', 'submitted',
                   '{\"accountId\":\"account-a\",\"messageId\":\"orphaned-message\"}',
                   'pending', 0, 0
                 )",
                [],
            )
            .unwrap();
        drop(connection);
        let mut send = item("orphaned-send", "account-a", WorkKind::Send);
        send.operation_id = Some("orphaned-send-operation".into());
        send.ordering_key = "send:v1:orphaned-message".into();
        let now = system_now_ms();
        worker.enqueue(send, now).unwrap();
        let claim = worker
            .claim_available(now, "crashed-send-worker", 1)
            .unwrap()
            .pop()
            .expect("orphaned send claim");
        mark_send_submission_started(&path, &claim, now).unwrap();
        let worker_probe = worker.clone();
        let adapter_calls = Arc::new(AtomicUsize::new(0));
        let adapter = {
            let adapter_calls = Arc::clone(&adapter_calls);
            move |_: &ClaimedWork, _: &WorkerExecutionContext| {
                adapter_calls.fetch_add(1, Ordering::SeqCst);
                local_success()
            }
        };
        let (sender, receiver) = mpsc::channel();
        let controller = WorkerController::start(
            worker,
            "send-recovery-worker".into(),
            adapter,
            no_op_projector,
            move |result| {
                let _ = sender.send(result);
            },
        )
        .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let mut notified = false;
        while std::time::Instant::now() < deadline && !notified {
            match receiver.recv_timeout(Duration::from_millis(500)) {
                Ok(result) => notified |= result.outcome_unknown == 1,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        assert!(
            notified,
            "lease recovery must emit a terminal worker result after the commit"
        );
        assert_eq!(adapter_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            worker_probe
                .snapshot("orphaned-send")
                .unwrap()
                .unwrap()
                .state,
            WorkState::OutcomeUnknown
        );
        let verify = Connection::open(&path).unwrap();
        assert_eq!(
            verify
                .query_row(
                    "SELECT state FROM operations WHERE id = 'orphaned-send-operation'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "outcome_unknown"
        );
        controller.stop().unwrap();
    }

    #[test]
    fn controller_sleeps_until_the_next_durable_deadline_without_ui_polling() {
        let (_directory, _path, worker) = configured_worker(&["account-a"]);
        let now = system_now_ms();
        let mut delayed = item("scheduled", "account-a", WorkKind::Sync);
        delayed.available_at = now + 300;
        worker.enqueue(delayed, now).unwrap();
        let (sender, receiver) = mpsc::channel();
        let controller = WorkerController::start(
            worker,
            "scheduled-worker".into(),
            |_: &ClaimedWork, _: &WorkerExecutionContext| local_success(),
            no_op_projector,
            move |result| {
                let _ = sender.send(result);
            },
        )
        .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut succeeded = false;
        while std::time::Instant::now() < deadline && !succeeded {
            let result = receiver
                .recv_timeout(Duration::from_millis(500))
                .expect("worker wakes for its journal deadline without a renderer timer");
            succeeded |= result.succeeded == 1;
        }
        assert!(succeeded);
        controller.stop().unwrap();
    }

    #[test]
    fn controller_wake_processes_work_and_stop_joins_cleanly() {
        let (_directory, _path, worker) = configured_worker(&["account-a"]);
        worker
            .enqueue(item("controller", "account-a", WorkKind::Sync), 0)
            .unwrap();
        let (sender, receiver) = mpsc::channel();
        let controller = WorkerController::start(
            worker,
            "controller-worker".into(),
            |_: &ClaimedWork, _: &WorkerExecutionContext| local_success(),
            no_op_projector,
            move |result| {
                let _ = sender.send(result);
            },
        )
        .unwrap();
        controller.wake().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut succeeded = false;
        while std::time::Instant::now() < deadline && !succeeded {
            let result = receiver
                .recv_timeout(Duration::from_millis(500))
                .expect("wake causes an observable worker cycle");
            succeeded |= result.succeeded == 1;
        }
        assert!(succeeded);
        controller.stop().unwrap();
    }
}
