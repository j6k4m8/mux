//! Typed IMAP connection records held in the system keychain.
//!
//! SQLite retains only an opaque lookup marker and mailbox identity. Host,
//! port, username, authentication material, and TLS policy are decoded only
//! inside Rust immediately before a bounded IMAP session is created.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::imap::{ImapAccessError, ImapAccessGrant, ImapAccessSource};
use crate::keychain::{CredentialError, CredentialStore, CredentialStoreStatus};
use crate::worker::WorkerError;

const RECORD_VERSION: u8 = 1;
const MAX_RECORD_BYTES: usize = 64 * 1024;
const MAX_HOST_BYTES: usize = 253;
const MAX_IDENTITY_BYTES: usize = 2_048;
const MAX_SECRET_BYTES: usize = 16 * 1024;
const TLS_MODE: &str = "implicit";
const AUTH_MECHANISM: &str = "password";

pub(crate) struct ImapKeychainAccess {
    database_path: PathBuf,
    credentials: CredentialStore,
}

impl ImapKeychainAccess {
    pub(crate) fn new(database_path: &Path, credentials: CredentialStore) -> Self {
        Self {
            database_path: database_path.to_owned(),
            credentials,
        }
    }
}

impl ImapAccessSource for ImapKeychainAccess {
    fn access_for_account(&self, account_id: &str) -> Result<ImapAccessGrant, ImapAccessError> {
        let configured = read_configured_account(&self.database_path, account_id)?;
        match configured.auth_state.as_str() {
            "credential_locked" => return Err(ImapAccessError::CredentialUnavailable),
            "reauthorization_required" | "signed_out" => {
                return Err(ImapAccessError::ReauthorizationRequired)
            }
            "ready" => {}
            _ => return Err(ImapAccessError::Permanent),
        }
        let reference = configured
            .record_ref
            .ok_or(ImapAccessError::ReauthorizationRequired)?;
        let bytes = self
            .credentials
            .get(&reference)
            .map_err(map_credential_error)?
            .ok_or(ImapAccessError::ReauthorizationRequired)?;
        let record = ImapAuthorityRecord::decode(&bytes)?;
        record.validate_for_subject(&configured.remote_subject)?;
        Ok(ImapAccessGrant {
            host: record.host,
            port: record.port,
            username: record.username,
            password: record.password,
            remote_account_id: record.remote_subject,
        })
    }
}

/// Revalidates every configured IMAP lookup marker against the keychain at
/// startup. A missing or wrong-subject record is a typed reauthorization block;
/// it can never fall through to network I/O.
pub(crate) fn reconcile_credential_records(
    database_path: &Path,
    credentials: &CredentialStore,
    now_ms: i64,
) -> Result<usize, WorkerError> {
    if now_ms < 0 || credentials.status() != CredentialStoreStatus::Available {
        return Err(WorkerError::Conflict(
            "IMAP authority cannot be reconciled while the keychain is unavailable".into(),
        ));
    }
    let mut connection = Connection::open(database_path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let accounts = {
        let mut statement = transaction.prepare(
            "SELECT account_id, remote_account_id, credential_ref
             FROM provider_accounts
             WHERE provider_kind = 'imap' AND credential_ref IS NOT NULL
               AND auth_state IN ('credential_locked', 'ready')
             ORDER BY account_id",
        )?;
        let values = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        values
    };
    let mut invalid = 0;
    for (account_id, remote_subject, reference) in accounts {
        let valid = match credentials.get(&reference) {
            Ok(Some(bytes)) => ImapAuthorityRecord::decode(&bytes)
                .and_then(|record| record.validate_for_subject(&remote_subject))
                .is_ok(),
            Ok(None) => false,
            Err(CredentialError::Unavailable) => {
                return Err(WorkerError::Conflict(
                    "IMAP authority reconciliation could not read the keychain".into(),
                ))
            }
            Err(_) => false,
        };
        if valid {
            continue;
        }
        invalid += 1;
        transaction.execute(
            "UPDATE provider_accounts
             SET auth_state = 'reauthorization_required',
                 auth_block_reason = 'provider_reauthorization',
                 sync_state = 'authentication_blocked',
                 last_error_code = 'imap_authority_invalid', updated_at = ?2
             WHERE account_id = ?1",
            params![&account_id, now_ms],
        )?;
        transaction.execute(
            "UPDATE provider_work_items
             SET state = 'authentication_blocked',
                 auth_block_reason = 'provider_reauthorization',
                 last_error_code = 'imap_reauthorization_required',
                 lease_owner = NULL, lease_token = NULL, lease_expires_at = NULL,
                 retry_after_at = NULL
             WHERE account_id = ?1
               AND state IN (
                 'queued', 'retry_wait', 'rate_limited', 'authentication_blocked'
               )",
            [&account_id],
        )?;
    }
    transaction.commit()?;
    Ok(invalid)
}

struct ConfiguredAccount {
    remote_subject: String,
    auth_state: String,
    record_ref: Option<String>,
}

fn read_configured_account(
    database_path: &Path,
    account_id: &str,
) -> Result<ConfiguredAccount, ImapAccessError> {
    let connection = Connection::open(database_path).map_err(|_| ImapAccessError::Retryable)?;
    connection
        .query_row(
            "SELECT remote_account_id, auth_state, credential_ref
             FROM provider_accounts
             WHERE account_id = ?1 AND provider_kind = 'imap'",
            [account_id],
            |row| {
                Ok(ConfiguredAccount {
                    remote_subject: row.get(0)?,
                    auth_state: row.get(1)?,
                    record_ref: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(|_| ImapAccessError::Retryable)?
        .ok_or(ImapAccessError::ReauthorizationRequired)
}

struct ImapAuthorityRecord {
    host: String,
    port: u16,
    username: String,
    password: Zeroizing<String>,
    remote_subject: String,
}

#[derive(Deserialize, Zeroize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ImapAuthorityRecordWire {
    version: u8,
    tls_mode: String,
    auth_mechanism: String,
    host: String,
    port: u16,
    username: String,
    password: String,
    remote_subject: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(not(test), allow(dead_code))]
struct ImapAuthorityRecordWrite<'a> {
    version: u8,
    tls_mode: &'static str,
    auth_mechanism: &'static str,
    host: &'a str,
    port: u16,
    username: &'a str,
    password: &'a str,
    remote_subject: &'a str,
}

impl ImapAuthorityRecord {
    fn decode(bytes: &[u8]) -> Result<Self, ImapAccessError> {
        if bytes.is_empty() || bytes.len() > MAX_RECORD_BYTES {
            return Err(ImapAccessError::Permanent);
        }
        let wire = Zeroizing::new(
            serde_json::from_slice::<ImapAuthorityRecordWire>(bytes)
                .map_err(|_| ImapAccessError::Permanent)?,
        );
        if wire.version != RECORD_VERSION
            || wire.tls_mode != TLS_MODE
            || wire.auth_mechanism != AUTH_MECHANISM
            || wire.port == 0
        {
            return Err(ImapAccessError::Permanent);
        }
        let host = normalize_hostname(&wire.host)?;
        validate_identity(&wire.username)?;
        validate_identity(&wire.remote_subject)?;
        validate_secret(&wire.password)?;
        Ok(Self {
            host,
            port: wire.port,
            username: wire.username.clone(),
            password: Zeroizing::new(wire.password.clone()),
            remote_subject: wire.remote_subject.clone(),
        })
    }

    fn validate_for_subject(&self, expected_subject: &str) -> Result<(), ImapAccessError> {
        if self.remote_subject != expected_subject {
            return Err(ImapAccessError::ReauthorizationRequired);
        }
        Ok(())
    }

    fn encode(&self) -> Result<Zeroizing<Vec<u8>>, ImapAccessError> {
        serde_json::to_vec(&ImapAuthorityRecordWrite {
            version: RECORD_VERSION,
            tls_mode: TLS_MODE,
            auth_mechanism: AUTH_MECHANISM,
            host: &self.host,
            port: self.port,
            username: &self.username,
            password: &self.password,
            remote_subject: &self.remote_subject,
        })
        .map(Zeroizing::new)
        .map_err(|_| ImapAccessError::Permanent)
    }
}

/// Internal provisioning boundary for a future typed account flow. The
/// returned bytes are secret-bearing and may only be written to the keychain.
/// Writes one IMAP authority record. Exercised by this module's tests; the
/// command that adds an IMAP account from the interface does not exist yet, so
/// there is no production caller.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn encode_imap_authority(
    host: &str,
    port: u16,
    username: &str,
    password: Zeroizing<String>,
    remote_subject: &str,
) -> Result<Zeroizing<Vec<u8>>, ImapAccessError> {
    let record = ImapAuthorityRecord {
        host: normalize_hostname(host)?,
        port,
        username: username.to_owned(),
        password,
        remote_subject: remote_subject.to_owned(),
    };
    if record.port == 0 {
        return Err(ImapAccessError::Permanent);
    }
    validate_identity(&record.username)?;
    validate_identity(&record.remote_subject)?;
    validate_secret(&record.password)?;
    record.encode()
}

fn normalize_hostname(value: &str) -> Result<String, ImapAccessError> {
    if value.is_empty() || value.len() > MAX_HOST_BYTES || !value.is_ascii() || value.ends_with('.')
    {
        return Err(ImapAccessError::Permanent);
    }
    let normalized = value.to_ascii_lowercase();
    if normalized.split('.').any(|label| {
        label.is_empty()
            || label.len() > 63
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || !label
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            || !label
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
    }) {
        return Err(ImapAccessError::Permanent);
    }
    Ok(normalized)
}

fn validate_identity(value: &str) -> Result<(), ImapAccessError> {
    if value.is_empty() || value.len() > MAX_IDENTITY_BYTES || value.chars().any(char::is_control) {
        return Err(ImapAccessError::Permanent);
    }
    Ok(())
}

fn validate_secret(value: &str) -> Result<(), ImapAccessError> {
    if value.is_empty() || value.len() > MAX_SECRET_BYTES || value.contains('\0') {
        return Err(ImapAccessError::Permanent);
    }
    Ok(())
}

fn map_credential_error(error: CredentialError) -> ImapAccessError {
    match error {
        CredentialError::Unavailable => ImapAccessError::CredentialUnavailable,
        CredentialError::InvalidIdentifier | CredentialError::LimitExceeded(_) => {
            ImapAccessError::Permanent
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MuxStore;
    use tempfile::tempdir;

    fn configured_fixture() -> (tempfile::TempDir, PathBuf, CredentialStore, String) {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("imap-access.db");
        drop(MuxStore::open(&path, false).expect("native schema"));
        let credentials = CredentialStore::temporary(&directory.path().join("imap.keychain-db"));
        let reference = "imap:v1:fixture-account".to_string();
        let encoded = encode_imap_authority(
            "mail.example.test",
            993,
            "reader@example.test",
            Zeroizing::new("fixture-imap-passphrase".to_string()),
            "reader@example.test",
        )
        .expect("encode authority");
        credentials
            .put(&reference, encoded)
            .expect("write authority");
        let connection = Connection::open(&path).expect("fixture connection");
        connection
            .execute(
                "INSERT INTO accounts(id, name, email, color, provider)
                 VALUES('imap-account', 'IMAP', 'reader@example.test', '#000000', 'imap')",
                [],
            )
            .expect("account fixture");
        connection
            .execute(
                "INSERT INTO provider_accounts(
                   account_id, provider_kind, remote_account_id, auth_state,
                   credential_ref, sync_state, created_at, updated_at
                 ) VALUES(
                   'imap-account', 'imap', 'reader@example.test', 'ready', ?1,
                   'never_synced', 0, 0
                 )",
                [&reference],
            )
            .expect("provider fixture");
        (directory, path, credentials, reference)
    }

    #[test]
    fn imap_authority_is_strict_normalized_and_secret_bearing_only_in_keychain() {
        let encoded = encode_imap_authority(
            "MAIL.Example.Test",
            993,
            "reader@example.test",
            Zeroizing::new("fixture-imap-passphrase".into()),
            "reader@example.test",
        )
        .expect("valid authority");
        let record = ImapAuthorityRecord::decode(&encoded).expect("decode authority");
        assert_eq!(record.host, "mail.example.test");
        assert!(encode_imap_authority(
            "mail.example.test\r\nINJECT",
            993,
            "reader@example.test",
            Zeroizing::new("fixture-imap-passphrase".into()),
            "reader@example.test",
        )
        .is_err());
        assert!(encode_imap_authority(
            "mail.example.test",
            0,
            "reader@example.test",
            Zeroizing::new("fixture-imap-passphrase".into()),
            "reader@example.test",
        )
        .is_err());
        let wrong_policy = serde_json::json!({
            "version": 1,
            "tlsMode": "starttls",
            "authMechanism": "password",
            "host": "mail.example.test",
            "port": 143,
            "username": "reader@example.test",
            "password": "fixture-imap-passphrase",
            "remoteSubject": "reader@example.test"
        });
        assert!(ImapAuthorityRecord::decode(
            &serde_json::to_vec(&wrong_policy).expect("serialize wrong policy")
        )
        .is_err());
    }

    #[test]
    fn imap_keychain_access_fails_closed_and_sqlite_contains_only_an_opaque_marker() {
        let (_directory, path, credentials, reference) = configured_fixture();
        let access = ImapKeychainAccess::new(&path, credentials);
        let grant = access
            .access_for_account("imap-account")
            .expect("available grant");
        assert_eq!(grant.host, "mail.example.test");
        assert_eq!(grant.port, 993);
        assert_eq!(grant.username, "reader@example.test");
        assert_eq!(grant.remote_account_id, "reader@example.test");
        assert_eq!(grant.password.as_str(), "fixture-imap-passphrase");

        let sqlite_bytes = std::fs::read(&path).expect("read SQLite fixture");
        let sqlite_text = String::from_utf8_lossy(&sqlite_bytes);
        assert!(!sqlite_text.contains("fixture-imap-passphrase"));
        assert!(!sqlite_text.contains("mail.example.test"));
        assert!(sqlite_text.contains(&reference));

        let connection = Connection::open(&path).expect("fixture connection");
        connection
            .execute(
                "UPDATE provider_accounts SET credential_ref = 'imap:v1:missing',
                   auth_state = 'ready' WHERE account_id = 'imap-account'",
                [],
            )
            .expect("replace marker");
        drop(connection);
        assert!(matches!(
            access.access_for_account("imap-account"),
            Err(ImapAccessError::ReauthorizationRequired)
        ));
    }

    #[test]
    fn imap_startup_reconciliation_blocks_wrong_subject_without_exposing_record() {
        let (_directory, path, credentials, _reference) = configured_fixture();
        let connection = Connection::open(&path).expect("fixture connection");
        connection
            .execute(
                "UPDATE provider_accounts SET remote_account_id = 'other@example.test'
                 WHERE account_id = 'imap-account'",
                [],
            )
            .expect("change subject");
        drop(connection);
        assert_eq!(
            reconcile_credential_records(&path, &credentials, 10).expect("reconcile"),
            1
        );
        let connection = Connection::open(path).expect("verification connection");
        assert_eq!(
            connection
                .query_row(
                    "SELECT auth_state || ':' || auth_block_reason || ':' || last_error_code
                     FROM provider_accounts WHERE account_id = 'imap-account'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("blocked state"),
            "reauthorization_required:provider_reauthorization:imap_authority_invalid"
        );
    }
}
