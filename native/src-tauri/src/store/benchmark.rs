//! Deterministic native-store performance gate.
//!
//! This module deliberately benchmarks the same migrated SQLite schema and
//! `MuxStore` calls used by the application. It creates its database under the
//! operating system temporary directory and removes the database plus SQLite
//! sidecars when the run ends, including error paths.

use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{params, TransactionBehavior};

use super::{
    MessagePageInput, MuxStore, SearchInput, StoreError, ThreadLookupInput, ThreadPageInput,
};

const THREAD_COUNT: i64 = 50_000;
const MESSAGE_COUNT: i64 = THREAD_COUNT * 2;
const TARGET_THREAD_ID: i64 = 4_242;

const FRESH_MIGRATION_LIMIT: Duration = Duration::from_secs(1);
const POPULATED_MIGRATION_LIMIT: Duration = Duration::from_secs(5);
const SEED_LIMIT: Duration = Duration::from_secs(15);
const INBOX_LIMIT: Duration = Duration::from_millis(250);
const STRUCTURED_SEARCH_LIMIT: Duration = Duration::from_millis(500);
const FTS_SEARCH_LIMIT: Duration = Duration::from_millis(750);
const THREAD_DETAIL_LIMIT: Duration = Duration::from_millis(100);
const MUTATION_LIMIT: Duration = Duration::from_millis(100);
const RECOVERY_LIMIT: Duration = Duration::from_secs(1);
const WHOLE_RUN_LIMIT: Duration = Duration::from_secs(25);

#[derive(Debug, Clone)]
pub struct BenchmarkReport {
    pub threads: i64,
    pub messages: i64,
    pub fresh_migration_ms: f64,
    pub populated_v12_migration_ms: f64,
    pub seed_ms: f64,
    pub inbox_ms: f64,
    pub structured_search_ms: f64,
    pub fts_search_ms: f64,
    pub thread_detail_ms: f64,
    pub archive_mutation_ms: f64,
    pub interrupted_operation_recovery_ms: f64,
    pub total_ms: f64,
}

impl Display for BenchmarkReport {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        writeln!(formatter, "Mux native SQLite benchmark")?;
        writeln!(
            formatter,
            "fixture: {} messages across {} threads",
            self.messages, self.threads
        )?;
        writeln!(
            formatter,
            "fresh migration: {:.2} ms (limit 1000 ms)",
            self.fresh_migration_ms
        )?;
        writeln!(
            formatter,
            "populated v12→v{} migration: {:.2} ms (limit 5000 ms)",
            super::SCHEMA_VERSION,
            self.populated_v12_migration_ms
        )?;
        writeln!(formatter, "seed: {:.2} ms (limit 15000 ms)", self.seed_ms)?;
        writeln!(formatter, "inbox: {:.2} ms (limit 250 ms)", self.inbox_ms)?;
        writeln!(
            formatter,
            "structured search: {:.2} ms (limit 500 ms)",
            self.structured_search_ms
        )?;
        writeln!(
            formatter,
            "FTS body search: {:.2} ms (limit 750 ms)",
            self.fts_search_ms
        )?;
        writeln!(
            formatter,
            "thread detail: {:.2} ms (limit 100 ms)",
            self.thread_detail_ms
        )?;
        writeln!(
            formatter,
            "archive mutation: {:.2} ms (limit 100 ms)",
            self.archive_mutation_ms
        )?;
        writeln!(
            formatter,
            "interrupted operation recovery: {:.2} ms (limit 1000 ms)",
            self.interrupted_operation_recovery_ms
        )?;
        write!(formatter, "total: {:.2} ms (limit 25000 ms)", self.total_ms)
    }
}

/// Runs the release-sized native benchmark in an isolated temporary database.
///
/// The fixture contains exactly two messages in each of 50,000 threads. Every
/// operation below has a correctness assertion in addition to its time limit;
/// a fast wrong answer fails the benchmark.
pub fn run_native_benchmark() -> Result<BenchmarkReport, StoreError> {
    let mut temporary_database = TemporaryDatabase::new("mux-native-benchmark");
    let result = run_at_path(temporary_database.path(), THREAD_COUNT, true);
    let cleanup = temporary_database.cleanup();
    match (result, cleanup) {
        (Ok(report), Ok(())) => Ok(report),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(benchmark_error), Err(_)) => Err(StoreError::Validation(format!(
            "{benchmark_error}; native benchmark temporary database cleanup also failed"
        ))),
    }
}

fn run_at_path(
    path: &Path,
    thread_count: i64,
    enforce_release_shape: bool,
) -> Result<BenchmarkReport, StoreError> {
    let whole_started = Instant::now();

    let fresh_migration_started = Instant::now();
    let mut store = MuxStore::open(path, false)?;
    let fresh_migration_elapsed = fresh_migration_started.elapsed();
    enforce_limit(
        "fresh migration",
        fresh_migration_elapsed,
        FRESH_MIGRATION_LIMIT,
    )?;

    let seed_started = Instant::now();
    seed_fixture(&mut store, thread_count)?;
    let seed_elapsed = seed_started.elapsed();
    enforce_limit("seed", seed_elapsed, SEED_LIMIT)?;

    let expected_messages = thread_count * 2;
    assert_database_shape(&store, thread_count, expected_messages)?;

    prepare_populated_v12_fixture(&mut store)?;
    drop(store);
    let populated_migration_started = Instant::now();
    let mut store = MuxStore::open(path, false)?;
    let populated_migration_elapsed = populated_migration_started.elapsed();
    enforce_limit(
        "populated v12 migration",
        populated_migration_elapsed,
        POPULATED_MIGRATION_LIMIT,
    )?;
    assert_database_shape(&store, thread_count, expected_messages)?;

    let inbox_started = Instant::now();
    let inbox = store.list_threads(ThreadPageInput {
        account_id: None,
        view: Some("inbox".into()),
        cursor: None,
        limit: Some(50),
        hidden_account_ids: Vec::new(),
    })?;
    let inbox_elapsed = inbox_started.elapsed();
    enforce_limit("inbox", inbox_elapsed, INBOX_LIMIT)?;
    benchmark_assert(inbox.threads.len() == 50, "inbox did not return 50 rows")?;
    benchmark_assert(inbox.has_more, "inbox did not report its next page")?;
    benchmark_assert(
        inbox.next_cursor.is_some(),
        "inbox did not return a page cursor",
    )?;

    let structured_started = Instant::now();
    let structured = store.search_threads(SearchInput {
        query: format!("from:sender{TARGET_THREAD_ID}@example.test is:unread"),
        account_id: None,
        view: Some("all".into()),
        cursor: None,
        limit: Some(50),
        timezone_offset_minutes: 0,
        hidden_account_ids: Vec::new(),
    })?;
    let structured_elapsed = structured_started.elapsed();
    enforce_limit(
        "structured search",
        structured_elapsed,
        STRUCTURED_SEARCH_LIMIT,
    )?;
    benchmark_assert(
        structured.rows.len() == 1 && structured.rows[0].id == TARGET_THREAD_ID,
        "structured search returned the wrong thread",
    )?;
    benchmark_assert(!structured.has_more, "structured search had extra rows")?;

    let fts_started = Instant::now();
    let fts = store.search_threads(SearchInput {
        query: format!("quartzneedle{TARGET_THREAD_ID}"),
        account_id: None,
        view: Some("all".into()),
        cursor: None,
        limit: Some(50),
        timezone_offset_minutes: 0,
        hidden_account_ids: Vec::new(),
    })?;
    let fts_elapsed = fts_started.elapsed();
    enforce_limit("FTS body search", fts_elapsed, FTS_SEARCH_LIMIT)?;
    benchmark_assert(
        fts.rows.len() == 1 && fts.rows[0].id == TARGET_THREAD_ID,
        "FTS body search returned the wrong thread",
    )?;
    benchmark_assert(!fts.has_more, "FTS body search had extra rows")?;

    let detail_started = Instant::now();
    let detail = store.get_thread_messages(MessagePageInput {
        thread_id: TARGET_THREAD_ID,
        cursor: None,
        limit: Some(50),
    })?;
    let detail_elapsed = detail_started.elapsed();
    enforce_limit("thread detail", detail_elapsed, THREAD_DETAIL_LIMIT)?;
    benchmark_assert(
        detail.messages.len() == 2,
        "thread detail did not return two messages",
    )?;
    benchmark_assert(!detail.has_more, "two-message thread reported another page")?;
    benchmark_assert(
        detail.next_cursor.is_none(),
        "two-message thread returned a cursor",
    )?;
    benchmark_assert(
        detail.attachments.is_empty(),
        "fixture unexpectedly returned attachments",
    )?;

    let mutation_started = Instant::now();
    let archive = store.apply_thread_action(TARGET_THREAD_ID, "archive")?;
    let mutation_elapsed = mutation_started.elapsed();
    enforce_limit("archive mutation", mutation_elapsed, MUTATION_LIMIT)?;
    benchmark_assert(
        archive.kind == "archive",
        "archive operation had the wrong kind",
    )?;
    benchmark_assert(
        archive.state == "pending",
        "archive operation was not durable pending intent",
    )?;
    benchmark_assert(
        archive.thread_id == Some(TARGET_THREAD_ID),
        "archive operation lost its thread identity",
    )?;
    let inbox_lookup = store.get_thread_summary(ThreadLookupInput {
        thread_id: TARGET_THREAD_ID,
        query: None,
        account_id: None,
        view: Some("inbox".into()),
        timezone_offset_minutes: 0,
    })?;
    let archive_lookup = store.get_thread_summary(ThreadLookupInput {
        thread_id: TARGET_THREAD_ID,
        query: None,
        account_id: None,
        view: Some("archive".into()),
        timezone_offset_minutes: 0,
    })?;
    benchmark_assert(
        inbox_lookup.is_none() && archive_lookup.is_some(),
        "archive pending intent was not reflected in the local projection",
    )?;
    let queued_work: i64 = store.connection.query_row(
        "SELECT COUNT(*) FROM provider_work_items WHERE operation_id = ?1 AND state = 'queued'",
        [&archive.id],
        |row| row.get(0),
    )?;
    benchmark_assert(
        queued_work == 1,
        "archive did not enqueue exactly one durable work item",
    )?;

    // Reproduce a process exit after the local mutation began executing but
    // before provider acknowledgement. Reopening must recover a safe mutation
    // to retry without losing its durable local projection.
    store.connection.execute(
        "UPDATE operations SET state = 'executing' WHERE id = ?1",
        [&archive.id],
    )?;
    drop(store);
    let recovery_started = Instant::now();
    let store = MuxStore::open(path, false)?;
    let recovery_elapsed = recovery_started.elapsed();
    enforce_limit(
        "interrupted operation recovery",
        recovery_elapsed,
        RECOVERY_LIMIT,
    )?;
    let recovered_state: String = store.connection.query_row(
        "SELECT state FROM operations WHERE id = ?1",
        [&archive.id],
        |row| row.get(0),
    )?;
    benchmark_assert(
        recovered_state == "retrying",
        "an interrupted retry-safe mutation was not recovered for retry",
    )?;
    let recovered_archive = store.get_thread_summary(ThreadLookupInput {
        thread_id: TARGET_THREAD_ID,
        query: None,
        account_id: None,
        view: Some("archive".into()),
        timezone_offset_minutes: 0,
    })?;
    benchmark_assert(
        recovered_archive.is_some(),
        "interrupted-operation recovery lost the immediate local projection",
    )?;

    let total_elapsed = whole_started.elapsed();
    enforce_limit("whole benchmark", total_elapsed, WHOLE_RUN_LIMIT)?;
    if enforce_release_shape {
        benchmark_assert(
            thread_count == THREAD_COUNT && expected_messages == MESSAGE_COUNT,
            "release benchmark fixture size changed",
        )?;
    }

    Ok(BenchmarkReport {
        threads: thread_count,
        messages: expected_messages,
        fresh_migration_ms: milliseconds(fresh_migration_elapsed),
        populated_v12_migration_ms: milliseconds(populated_migration_elapsed),
        seed_ms: milliseconds(seed_elapsed),
        inbox_ms: milliseconds(inbox_elapsed),
        structured_search_ms: milliseconds(structured_elapsed),
        fts_search_ms: milliseconds(fts_elapsed),
        thread_detail_ms: milliseconds(detail_elapsed),
        archive_mutation_ms: milliseconds(mutation_elapsed),
        interrupted_operation_recovery_ms: milliseconds(recovery_elapsed),
        total_ms: milliseconds(total_elapsed),
    })
}

/// Converts the populated current fixture into the exact released v12 shape
/// that preceded the deterministic v13 FTS rebuild. In particular, v12
/// provider receipts did not have a fingerprint-version column. The following
/// open therefore exercises the populated v12-to-current upgrade path rather than
/// merely editing the schema-version marker.
fn prepare_populated_v12_fixture(store: &mut MuxStore) -> Result<(), StoreError> {
    let transaction = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "DELETE FROM messages_fts;
         INSERT INTO messages_fts(thread_id, subject, participants, body, attachment_names)
           VALUES(-1, 'stale benchmark row', 'stale', 'must disappear', '');
         DROP INDEX provider_batches_applied_idx;
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
           account_id, batch_id, fingerprint, cursor_scope, cursor, applied_at
         )
         SELECT account_id, batch_id, fingerprint, cursor_scope, cursor, applied_at
         FROM provider_applied_batches_source
         WHERE fingerprint_version = 1;
         DROP TABLE provider_applied_batches_source;
         CREATE INDEX provider_batches_applied_idx
           ON provider_applied_batches(account_id, applied_at, batch_id);
         UPDATE meta SET value = '12' WHERE key = 'schema_version';",
    )?;
    transaction.commit()?;
    Ok(())
}

fn seed_fixture(store: &mut MuxStore, thread_count: i64) -> Result<(), StoreError> {
    benchmark_assert(
        thread_count >= TARGET_THREAD_ID,
        "benchmark fixture must include the target thread",
    )?;
    let transaction = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute(
        "INSERT INTO accounts(id, name, email, color, provider, signature)
         VALUES('benchmark', 'Benchmark', 'benchmark@mux.example', '#5067f2', 'fake', '')",
        [],
    )?;

    {
        let mut insert_thread = transaction.prepare(
            "INSERT INTO threads(
               id, account_id, subject, participants, snippet, latest_at, message_count,
               remote_in_inbox, remote_unread, remote_starred, has_attachment, has_invite,
               has_link, has_from_me, category, attachment_names
             ) VALUES(?1, 'benchmark', ?2, ?3, ?4, ?5, 2, ?6, ?7, ?8, 0, 0, 0, 1, ?9, '')",
        )?;
        let mut insert_message = transaction.prepare(
            "INSERT INTO messages(
               id, thread_id, sender_name, sender_email, recipients, sent_at, body_text,
               is_from_me
             ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?;
        let mut insert_fts = transaction.prepare(
            "INSERT INTO messages_fts(thread_id, subject, participants, body, attachment_names)
             VALUES(?1, ?2, ?3, ?4, '')",
        )?;

        for thread_id in 1..=thread_count {
            let subject = format!("Deterministic benchmark thread {thread_id}");
            let participant = format!("Sender {thread_id}");
            let sender_email = format!("sender{thread_id}@example.test");
            let incoming_body = if thread_id == TARGET_THREAD_ID {
                format!("Benchmark incoming body for thread {thread_id}. quartzneedle{thread_id}")
            } else {
                format!("Benchmark incoming body for thread {thread_id}.")
            };
            let outgoing_body = format!("Deterministic reply for thread {thread_id}.");
            let latest_at = 1_800_000_000_000_i64 - thread_id * 1_000;
            let in_inbox = i64::from(thread_id % 5 != 0);
            let unread = i64::from(thread_id % 3 == 0);
            let starred = i64::from(thread_id % 11 == 0);
            let category = if thread_id % 2 == 0 {
                "Work"
            } else {
                "Personal"
            };

            insert_thread.execute(params![
                thread_id,
                subject,
                participant,
                incoming_body,
                latest_at,
                in_inbox,
                unread,
                starred,
                category,
            ])?;
            insert_message.execute(params![
                thread_id * 2 - 1,
                thread_id,
                participant,
                sender_email,
                "Benchmark <benchmark@mux.example>",
                latest_at - 1_000,
                incoming_body,
                0,
            ])?;
            insert_message.execute(params![
                thread_id * 2,
                thread_id,
                "Benchmark",
                "benchmark@mux.example",
                format!("sender{thread_id}@example.test"),
                latest_at,
                outgoing_body,
                1,
            ])?;
            insert_fts.execute(params![
                thread_id,
                subject,
                format!("{participant} {sender_email}"),
                format!("{incoming_body}\n{outgoing_body}"),
            ])?;
        }
    }
    transaction.commit()?;
    Ok(())
}

fn assert_database_shape(
    store: &MuxStore,
    expected_threads: i64,
    expected_messages: i64,
) -> Result<(), StoreError> {
    let thread_count: i64 =
        store
            .connection
            .query_row("SELECT COUNT(*) FROM threads", [], |row| row.get(0))?;
    let message_count: i64 =
        store
            .connection
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))?;
    let fts_count: i64 =
        store
            .connection
            .query_row("SELECT COUNT(*) FROM messages_fts", [], |row| row.get(0))?;
    benchmark_assert(
        thread_count == expected_threads,
        "thread count was incorrect",
    )?;
    benchmark_assert(
        message_count == expected_messages,
        "message count was incorrect",
    )?;
    benchmark_assert(fts_count == expected_threads, "FTS row count was incorrect")?;

    let bootstrap = store.bootstrap()?;
    benchmark_assert(
        bootstrap.schema_version == super::SCHEMA_VERSION,
        "schema was not current",
    )?;
    benchmark_assert(
        bootstrap.accounts.len() == 1,
        "fixture account count was incorrect",
    )?;
    benchmark_assert(
        bootstrap.accounts[0].total == expected_threads,
        "bootstrap thread count was incorrect",
    )?;
    benchmark_assert(
        bootstrap.view_counts[0].all == expected_threads,
        "all-mail count was incorrect",
    )?;
    benchmark_assert(
        bootstrap.view_counts[0].inbox == expected_threads - expected_threads / 5,
        "inbox count was incorrect",
    )?;

    let foreign_key_violation: Option<String> = store
        .connection
        .prepare("PRAGMA foreign_key_check")?
        .query_map([], |row| row.get(0))?
        .next()
        .transpose()?;
    benchmark_assert(
        foreign_key_violation.is_none(),
        "fixture violated a foreign key",
    )?;
    let integrity: String = store
        .connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    benchmark_assert(integrity == "ok", "SQLite integrity check failed")?;
    Ok(())
}

fn benchmark_assert(condition: bool, message: &str) -> Result<(), StoreError> {
    if condition {
        Ok(())
    } else {
        Err(StoreError::Validation(format!(
            "Native benchmark assertion failed: {message}"
        )))
    }
}

fn enforce_limit(label: &str, elapsed: Duration, limit: Duration) -> Result<(), StoreError> {
    if elapsed <= limit {
        Ok(())
    } else {
        Err(StoreError::Validation(format!(
            "Native benchmark {label} took {:.2} ms; limit is {:.2} ms",
            milliseconds(elapsed),
            milliseconds(limit)
        )))
    }
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

struct TemporaryDatabase {
    path: PathBuf,
    cleaned: bool,
}

impl TemporaryDatabase {
    fn new(prefix: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("{prefix}-{}-{nonce}.sqlite3", std::process::id()));
        Self {
            path,
            cleaned: false,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn cleanup(&mut self) -> Result<(), StoreError> {
        if self.cleaned {
            return Ok(());
        }
        for path in [
            self.path.clone(),
            PathBuf::from(format!("{}-wal", self.path.display())),
            PathBuf::from(format!("{}-shm", self.path.display())),
        ] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    return Err(StoreError::Validation(
                        "Could not remove native benchmark temporary database".into(),
                    ));
                }
            }
        }
        self.cleaned = true;
        Ok(())
    }
}

impl Drop for TemporaryDatabase {
    fn drop(&mut self) {
        if self.cleanup().is_err() {
            eprintln!("could not remove native benchmark temporary database");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaled_native_benchmark_exercises_real_store_contracts_and_cleans_up() {
        let temporary_database = TemporaryDatabase::new("mux-native-benchmark-test");
        let database_path = temporary_database.path().to_path_buf();
        let report = run_at_path(&database_path, TARGET_THREAD_ID + 8, false).unwrap();
        assert_eq!(report.threads, TARGET_THREAD_ID + 8);
        assert_eq!(report.messages, (TARGET_THREAD_ID + 8) * 2);
        assert!(database_path.exists());
        drop(temporary_database);
        assert!(!database_path.exists());
        assert!(!PathBuf::from(format!("{}-wal", database_path.display())).exists());
        assert!(!PathBuf::from(format!("{}-shm", database_path.display())).exists());
    }
}
