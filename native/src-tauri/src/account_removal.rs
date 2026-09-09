//! Account removal is a local destructive operation with one external edge:
//! the account's Keychain item. Keep those two stores coordinated so a failed
//! SQLite delete neither strands a usable account without its credential nor
//! removes another account's credential through a corrupt marker.

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::keychain::{CredentialError, CredentialStore, CredentialStoreStatus};

const MAX_ACCOUNT_ID_BYTES: usize = 200;
const PENDING_CREDENTIAL_PREFIX: &str = "account_removal/credential/";
const PENDING_CREDENTIAL_VERSION: u8 = 1;

#[derive(Debug, Error)]
pub(crate) enum AccountRemovalError {
    #[error("That account identifier is not valid")]
    Validation,
    #[error("That account is no longer connected")]
    NotFound,
    #[error("That account is busy. Wait for its current sync or send to finish, then try again")]
    Busy,
    #[error("Mux could not safely identify that account's saved sign-in")]
    UnsafeCredentialReference,
    #[error("Mux could not update the macOS Keychain")]
    CredentialUnavailable,
    #[error("Mux could not remove that account")]
    Storage,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RemovedAccount {
    pub(crate) account_id: String,
    pub(crate) keychain_cleanup_pending: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PendingCredentialDeletion {
    version: u8,
    account_id: String,
    provider_kind: String,
    credential_ref: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RemovalFailpoint {
    None,
    #[cfg(test)]
    AfterLocalCommit,
}

/// Removes one account's complete local projection and its bound Keychain
/// authority. Provider mail is never mutated. An immediate transaction closes
/// the race with a worker claim; already-executing work is refused explicitly.
pub(crate) fn remove_account(
    database_path: &Path,
    credentials: &CredentialStore,
    account_id: &str,
) -> Result<RemovedAccount, AccountRemovalError> {
    remove_account_with_failpoint(
        database_path,
        credentials,
        account_id,
        RemovalFailpoint::None,
    )
}

fn remove_account_with_failpoint(
    database_path: &Path,
    credentials: &CredentialStore,
    account_id: &str,
    failpoint: RemovalFailpoint,
) -> Result<RemovedAccount, AccountRemovalError> {
    validate_account_id(account_id)?;
    let mut connection =
        Connection::open(database_path).map_err(|_| AccountRemovalError::Storage)?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(|_| AccountRemovalError::Storage)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|_| AccountRemovalError::Storage)?;

    let account = transaction
        .query_row(
            "SELECT p.provider_kind, p.credential_ref
             FROM accounts a
             LEFT JOIN provider_accounts p ON p.account_id = a.id
             WHERE a.id = ?1",
            [account_id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .optional()
        .map_err(|_| AccountRemovalError::Storage)?
        .ok_or(AccountRemovalError::NotFound)?;

    let executing = transaction
        .query_row(
            "SELECT EXISTS(
               SELECT 1 FROM provider_work_items
               WHERE account_id = ?1 AND state = 'executing'
             )",
            [account_id],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|_| AccountRemovalError::Storage)?
        != 0;
    if executing {
        return Err(AccountRemovalError::Busy);
    }

    let pending_credential = match (&account.0, &account.1) {
        (_, None) => None,
        (Some(provider), Some(reference))
            if credential_reference_belongs_to_account(provider, account_id, reference) =>
        {
            if credentials.status() != CredentialStoreStatus::Available {
                return Err(AccountRemovalError::CredentialUnavailable);
            }
            Some(PendingCredentialDeletion {
                version: PENDING_CREDENTIAL_VERSION,
                account_id: account_id.to_owned(),
                provider_kind: provider.clone(),
                credential_ref: reference.clone(),
            })
        }
        _ => return Err(AccountRemovalError::UnsafeCredentialReference),
    };

    if let Some(pending) = &pending_credential {
        let value = serde_json::to_string(pending)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
            .map_err(|_| AccountRemovalError::Storage)?;
        transaction
            .execute(
                "INSERT INTO meta(key, value) VALUES(?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![pending_credential_key(account_id), value],
            )
            .map_err(|_| AccountRemovalError::Storage)?;
    }

    delete_local_account(&transaction, account_id)
        .and_then(|()| transaction.commit())
        .map_err(|_| AccountRemovalError::Storage)?;

    let keychain_cleanup_pending = match pending_credential {
        None => false,
        Some(_) if failpoint != RemovalFailpoint::None => true,
        Some(pending) => complete_pending_credential_deletion(
            database_path,
            credentials,
            &pending_credential_key(account_id),
            &pending,
        )
        .is_err(),
    };

    Ok(RemovedAccount {
        account_id: account_id.to_owned(),
        keychain_cleanup_pending,
    })
}

/// Finishes crash-interrupted Keychain cleanup. The durable marker is committed
/// atomically with local account deletion, so the only restart states are a
/// complete account or a removed account whose credential is still queued here.
pub(crate) fn reconcile_pending_credential_deletions(
    database_path: &Path,
    credentials: &CredentialStore,
) -> Result<usize, AccountRemovalError> {
    if credentials.status() != CredentialStoreStatus::Available {
        return Err(AccountRemovalError::CredentialUnavailable);
    }
    let connection = Connection::open(database_path).map_err(|_| AccountRemovalError::Storage)?;
    let pending = connection
        .prepare(
            "SELECT key, value FROM meta
             WHERE substr(key, 1, length(?1)) = ?1
             ORDER BY key",
        )
        .map_err(|_| AccountRemovalError::Storage)?
        .query_map([PENDING_CREDENTIAL_PREFIX], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|_| AccountRemovalError::Storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| AccountRemovalError::Storage)?;
    let mut completed = 0;
    for (key, value) in pending {
        let pending: PendingCredentialDeletion =
            serde_json::from_str(&value).map_err(|_| AccountRemovalError::Storage)?;
        if pending.version != PENDING_CREDENTIAL_VERSION
            || key != pending_credential_key(&pending.account_id)
            || !credential_reference_belongs_to_account(
                &pending.provider_kind,
                &pending.account_id,
                &pending.credential_ref,
            )
        {
            return Err(AccountRemovalError::UnsafeCredentialReference);
        }
        complete_pending_credential_deletion(database_path, credentials, &key, &pending)?;
        completed += 1;
    }
    Ok(completed)
}

fn complete_pending_credential_deletion(
    database_path: &Path,
    credentials: &CredentialStore,
    key: &str,
    pending: &PendingCredentialDeletion,
) -> Result<(), AccountRemovalError> {
    credentials
        .remove(&pending.credential_ref)
        .map_err(map_credential_error)?;
    let connection = Connection::open(database_path).map_err(|_| AccountRemovalError::Storage)?;
    connection
        .execute("DELETE FROM meta WHERE key = ?1", [key])
        .map_err(|_| AccountRemovalError::Storage)?;
    Ok(())
}

fn pending_credential_key(account_id: &str) -> String {
    format!("{PENDING_CREDENTIAL_PREFIX}{account_id}")
}

fn delete_local_account(
    transaction: &Transaction<'_>,
    account_id: &str,
) -> Result<(), rusqlite::Error> {
    let operation_ids = transaction
        .prepare(
            "SELECT operation_id FROM provider_work_items
             WHERE account_id = ?1 AND operation_id IS NOT NULL
             ORDER BY operation_id",
        )?
        .query_map([account_id], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let operation_ids = serde_json::to_string(&operation_ids)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;

    // Work holds a restrictive reference to its operation, so remove work
    // first, then every operation reached through that work or the account's
    // threads. The payload clause covers old completed sends whose thread was
    // deliberately null.
    transaction.execute(
        "DELETE FROM provider_work_items WHERE account_id = ?1",
        [account_id],
    )?;
    transaction.execute(
        "DELETE FROM operations
         WHERE thread_id IN (SELECT id FROM threads WHERE account_id = ?1)
            OR id IN (SELECT value FROM json_each(?2))
            OR (payload_json IS NOT NULL AND json_extract(payload_json, '$.accountId') = ?1)",
        params![account_id, operation_ids],
    )?;
    transaction.execute("DELETE FROM drafts WHERE account_id = ?1", [account_id])?;
    // FTS5 is intentionally not a foreign-key table.
    transaction.execute(
        "DELETE FROM messages_fts
         WHERE thread_id IN (SELECT id FROM threads WHERE account_id = ?1)",
        [account_id],
    )?;
    let deleted = transaction.execute("DELETE FROM accounts WHERE id = ?1", [account_id])?;
    if deleted != 1 {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    }
    Ok(())
}

fn validate_account_id(account_id: &str) -> Result<(), AccountRemovalError> {
    if account_id.is_empty()
        || account_id.len() > MAX_ACCOUNT_ID_BYTES
        || account_id.chars().any(char::is_control)
    {
        return Err(AccountRemovalError::Validation);
    }
    Ok(())
}

fn credential_reference_belongs_to_account(
    provider: &str,
    account_id: &str,
    reference: &str,
) -> bool {
    let Some(fingerprint) = account_id.strip_prefix(&format!("{provider}:")) else {
        return false;
    };
    if fingerprint.len() != 32
        || !fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return false;
    }
    let base = format!("{provider}/account/{fingerprint}");
    match provider {
        "gmail" => reference == base,
        "imap" => {
            reference == base
                || reference
                    .strip_prefix(&(base + "/"))
                    .is_some_and(|generation| {
                        generation.len() == 32
                            && generation
                                .bytes()
                                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
                    })
        }
        _ => false,
    }
}

fn map_credential_error(_error: CredentialError) -> AccountRemovalError {
    AccountRemovalError::CredentialUnavailable
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;
    use tempfile::tempdir;
    use zeroize::Zeroizing;

    use super::*;
    use crate::store::MuxStore;

    const ACCOUNT_A: &str = "gmail:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const ACCOUNT_B: &str = "gmail:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const REF_A: &str = "gmail/account/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const REF_B: &str = "gmail/account/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn fixture() -> (tempfile::TempDir, std::path::PathBuf, CredentialStore) {
        let directory = tempdir().expect("temporary directory");
        let database_path = directory.path().join("account-removal.db");
        drop(MuxStore::open(&database_path, false).expect("store"));
        let credentials =
            CredentialStore::temporary(&directory.path().join("account-removal.keychain"));
        (directory, database_path, credentials)
    }

    fn insert_gmail_account(connection: &Connection, account_id: &str, reference: &str) {
        connection
            .execute(
                "INSERT INTO accounts(id, name, email, color, provider)
                 VALUES(?1, 'Mailbox', ?2, '#000000', 'gmail')",
                params![account_id, format!("{account_id}@example.test")],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO provider_accounts(
                   account_id, provider_kind, remote_account_id, auth_state,
                   credential_ref, sync_state, created_at, updated_at
                 ) VALUES(?1, 'gmail', ?2, 'ready', ?3, 'idle', 1, 1)",
                params![account_id, format!("{account_id}@example.test"), reference],
            )
            .unwrap();
    }

    #[test]
    fn removal_deletes_only_the_selected_local_projection_and_credential() {
        let (_directory, path, credentials) = fixture();
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .unwrap();
        insert_gmail_account(&connection, ACCOUNT_A, REF_A);
        insert_gmail_account(&connection, ACCOUNT_B, REF_B);
        credentials
            .put(REF_A, Zeroizing::new(b"authority-a".to_vec()))
            .unwrap();
        credentials
            .put(REF_B, Zeroizing::new(b"authority-b".to_vec()))
            .unwrap();
        connection
            .execute(
                "INSERT INTO threads(
               id, account_id, subject, participants, snippet, latest_at, message_count,
               remote_in_inbox, remote_unread, remote_starred, has_attachment,
               has_invite, has_link, has_from_me
             ) VALUES(10, ?1, 'Subject', 'Person', 'Snippet', 1, 1, 1, 1, 0, 0, 0, 0, 0)",
                [ACCOUNT_A],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO messages(
               id, thread_id, sender_name, sender_email, recipients, sent_at,
               body_text, is_from_me
             ) VALUES(20, 10, 'Person', 'person@example.test', 'reader@example.test', 1,
                      'body', 0)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO messages_fts(thread_id, subject, participants, body, attachment_names)
             VALUES(10, 'Subject', 'Person', 'body', '')",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO drafts(id, account_id, recipients, subject, body, updated_at)
             VALUES('draft-a', ?1, 'person@example.test', 'Draft', 'body', 1)",
                [ACCOUNT_A],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO operations(
               id, thread_id, field, kind, old_value, new_value, payload_json,
               state, created_at, not_before
             ) VALUES('op-a', 10, 'unread', 'mutation', '1', '0', '{}', 'pending', 1, 1)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO provider_work_items(
               id, account_id, operation_id, kind, scope, ordering_key, retry_safety,
               payload_json, payload_fingerprint, state, created_at, available_at
             ) VALUES('work-a', ?1, 'op-a', 'mutation', 'account:a', 'op-a',
                      'safe_retry', '{}', zeroblob(32), 'queued', 1, 1)",
                [ACCOUNT_A],
            )
            .unwrap();
        drop(connection);

        let removed = remove_account(&path, &credentials, ACCOUNT_A).unwrap();
        assert_eq!(removed.account_id, ACCOUNT_A);
        assert!(!removed.keychain_cleanup_pending);
        let connection = Connection::open(&path).unwrap();
        for table in [
            "accounts",
            "threads",
            "messages",
            "drafts",
            "operations",
            "provider_work_items",
            "messages_fts",
        ] {
            let selected: i64 = connection
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE 1"),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let expected = i64::from(table == "accounts");
            assert_eq!(selected, expected, "unexpected rows in {table}");
        }
        assert!(credentials.get(REF_A).unwrap().is_none());
        assert_eq!(
            credentials.get(REF_B).unwrap().unwrap().as_slice(),
            b"authority-b"
        );
    }

    #[test]
    fn executing_work_blocks_removal_without_touching_local_or_keychain_state() {
        let (_directory, path, credentials) = fixture();
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .unwrap();
        insert_gmail_account(&connection, ACCOUNT_A, REF_A);
        credentials
            .put(REF_A, Zeroizing::new(b"authority-a".to_vec()))
            .unwrap();
        connection
            .execute(
                "INSERT INTO provider_work_items(
               id, account_id, kind, scope, ordering_key, retry_safety, payload_json,
               payload_fingerprint, state, created_at, available_at, attempt_count,
               lease_owner, lease_token, lease_expires_at
             ) VALUES('work-a', ?1, 'sync', 'account:a', 'sync-a', 'safe_retry', '{}',
                      zeroblob(32), 'executing', 1, 1, 1, 'worker', 'lease', 100)",
                [ACCOUNT_A],
            )
            .unwrap();
        drop(connection);

        assert!(matches!(
            remove_account(&path, &credentials, ACCOUNT_A),
            Err(AccountRemovalError::Busy)
        ));
        let connection = Connection::open(&path).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM accounts WHERE id = ?1",
                    [ACCOUNT_A],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        assert!(credentials.get(REF_A).unwrap().is_some());
    }

    #[test]
    fn a_crossed_credential_reference_cannot_delete_another_accounts_secret() {
        let (_directory, path, credentials) = fixture();
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .unwrap();
        insert_gmail_account(&connection, ACCOUNT_A, REF_B);
        credentials
            .put(REF_B, Zeroizing::new(b"authority-b".to_vec()))
            .unwrap();
        drop(connection);

        assert!(matches!(
            remove_account(&path, &credentials, ACCOUNT_A),
            Err(AccountRemovalError::UnsafeCredentialReference)
        ));
        assert_eq!(
            credentials.get(REF_B).unwrap().unwrap().as_slice(),
            b"authority-b"
        );
        let connection = Connection::open(&path).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM accounts WHERE id = ?1",
                    [ACCOUNT_A],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
    }

    #[test]
    fn database_failure_leaves_the_credential_account_and_cleanup_marker_untouched() {
        let (_directory, path, credentials) = fixture();
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .unwrap();
        insert_gmail_account(&connection, ACCOUNT_A, REF_A);
        connection
            .execute_batch(
                "CREATE TRIGGER fail_account_removal
             BEFORE DELETE ON accounts
             BEGIN SELECT RAISE(ABORT, 'forced removal failure'); END;",
            )
            .unwrap();
        credentials
            .put(REF_A, Zeroizing::new(b"authority-a".to_vec()))
            .unwrap();
        drop(connection);

        assert!(matches!(
            remove_account(&path, &credentials, ACCOUNT_A),
            Err(AccountRemovalError::Storage)
        ));
        assert_eq!(
            credentials.get(REF_A).unwrap().unwrap().as_slice(),
            b"authority-a"
        );
        let connection = Connection::open(&path).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM accounts WHERE id = ?1",
                    [ACCOUNT_A],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM meta WHERE key = ?1",
                    [pending_credential_key(ACCOUNT_A)],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn crash_after_local_commit_is_reconciled_without_reviving_the_account() {
        let (_directory, path, credentials) = fixture();
        let connection = Connection::open(&path).unwrap();
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .unwrap();
        insert_gmail_account(&connection, ACCOUNT_A, REF_A);
        credentials
            .put(REF_A, Zeroizing::new(b"authority-a".to_vec()))
            .unwrap();
        drop(connection);

        let removed = remove_account_with_failpoint(
            &path,
            &credentials,
            ACCOUNT_A,
            RemovalFailpoint::AfterLocalCommit,
        )
        .unwrap();
        assert!(removed.keychain_cleanup_pending);
        let connection = Connection::open(&path).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM accounts WHERE id = ?1",
                    [ACCOUNT_A],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM meta WHERE key = ?1",
                    [pending_credential_key(ACCOUNT_A)],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        drop(connection);
        assert!(credentials.get(REF_A).unwrap().is_some());

        assert_eq!(
            reconcile_pending_credential_deletions(&path, &credentials).unwrap(),
            1
        );
        assert!(credentials.get(REF_A).unwrap().is_none());
        let connection = Connection::open(&path).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM meta WHERE key = ?1",
                    [pending_credential_key(ACCOUNT_A)],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn removing_every_demo_account_does_not_reseed_them_on_reopen() {
        let directory = tempdir().expect("temporary directory");
        let database_path = directory.path().join("removed-demo-accounts.db");
        drop(MuxStore::open(&database_path, true).expect("seeded store"));
        let credentials =
            CredentialStore::temporary(&directory.path().join("removed-demo-accounts.keychain"));

        for account_id in ["acc_personal", "acc_research", "acc_work"] {
            remove_account(&database_path, &credentials, account_id).expect("demo account removed");
        }

        drop(MuxStore::open(&database_path, true).expect("removed store reopens"));
        let connection = Connection::open(&database_path).expect("removed database reopens");
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM accounts", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM threads", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn a_local_only_fixture_account_needs_no_keychain_record_to_be_removed() {
        let (_directory, path, credentials) = fixture();
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "INSERT INTO accounts(id, name, email, color, provider)
                 VALUES('local-account', 'Local', 'local@example.test', '#000000', 'demo')",
                [],
            )
            .unwrap();
        drop(connection);

        let removed = remove_account(&path, &credentials, "local-account").unwrap();
        assert_eq!(removed.account_id, "local-account");
        assert!(!removed.keychain_cleanup_pending);
        let connection = Connection::open(&path).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM accounts WHERE id = 'local-account'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }
}
