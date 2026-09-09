//! Typed IMAP connection records held in the system keychain.
//!
//! SQLite retains only an opaque lookup marker and mailbox identity. Host,
//! port, username, authentication material, and TLS policy are decoded only
//! inside Rust immediately before a bounded IMAP session is created.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::account_color::account_color_for;
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
               AND auth_state = 'ready'
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

/// Everything the interface supplies to add a mailbox. Secret bearing, so the
/// caller holds it in `Zeroizing` and a rejected attempt leaves no password in
/// freed memory. The derive supplies the wipe; `Zeroizing` is what calls it.
#[derive(Zeroize)]
pub(crate) struct ImapProvisionRequest {
    pub display_name: String,
    pub email: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

/// A validated request, ready to be proved against the server and then written
/// down. Preparing and persisting are separate so the order cannot be got
/// wrong: nothing reaches the keychain or the database until a session has
/// actually been opened with these details.
pub(crate) struct PreparedImapAccount {
    pub grant: ImapAccessGrant,
    pub account_id: String,
    pub record_ref: String,
    pub display_name: String,
    pub email: String,
}

const MAX_DISPLAY_BYTES: usize = 200;
const MAX_EMAIL_BYTES: usize = 320;

pub(crate) fn prepare_imap_account(
    request: &ImapProvisionRequest,
) -> Result<PreparedImapAccount, ImapAccessError> {
    let host = normalize_hostname(&request.host)?;
    if request.port == 0 {
        return Err(ImapAccessError::Permanent);
    }
    let username = request.username.trim().to_owned();
    validate_identity(&username)?;
    validate_secret(&request.password)?;
    let email = request.email.trim().to_ascii_lowercase();
    validate_bounded_text(&email, MAX_EMAIL_BYTES)?;
    let display_name = request.display_name.trim();
    let display_name = if display_name.is_empty() {
        email.clone()
    } else {
        display_name.to_owned()
    };
    validate_bounded_text(&display_name, MAX_DISPLAY_BYTES)?;

    // One mailbox is one host, port and login. Hashing the three of them means
    // re-adding the same mailbox corrects it in place rather than making a
    // second copy, and a NUL cannot appear in any of them, so the join is
    // unambiguous.
    let digest = Sha256::digest(format!("{host}\0{}\0{username}", request.port).as_bytes());
    let fingerprint = hex_lower(&digest)[..32].to_owned();
    Ok(PreparedImapAccount {
        grant: ImapAccessGrant {
            host,
            port: request.port,
            username: username.clone(),
            password: Zeroizing::new(request.password.clone()),
            remote_account_id: username,
        },
        account_id: format!("imap:{fingerprint}"),
        record_ref: format!("imap/account/{fingerprint}"),
        display_name,
        email,
    })
}

/// Writes the authority to the keychain and the marker to the database, in that
/// order: a failed database write leaves a keychain item nothing points at,
/// which the next attempt overwrites, while the reverse would leave a mailbox
/// whose credentials do not exist.
pub(crate) fn persist_imap_account(
    database_path: &Path,
    credentials: &CredentialStore,
    prepared: &PreparedImapAccount,
    now_ms: i64,
) -> Result<(), ImapAccessError> {
    if credentials.status() != CredentialStoreStatus::Available || now_ms < 0 {
        return Err(ImapAccessError::CredentialUnavailable);
    }
    let encoded = encode_imap_authority(
        &prepared.grant.host,
        prepared.grant.port,
        &prepared.grant.username,
        Zeroizing::new(prepared.grant.password.to_string()),
        &prepared.grant.remote_account_id,
    )?;
    credentials
        .put(&prepared.record_ref, encoded)
        .map_err(map_credential_error)?;
    persist_account_marker(database_path, prepared, now_ms).map_err(|_| ImapAccessError::Retryable)
}

fn persist_account_marker(
    database_path: &Path,
    prepared: &PreparedImapAccount,
    now_ms: i64,
) -> Result<(), rusqlite::Error> {
    let mut connection = Connection::open(database_path)?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    // Scoped to the kind the update below writes: a row of another kind under
    // this id would otherwise take the update path and match nothing, leaving
    // the mailbox half-written.
    let exists = transaction
        .query_row(
            "SELECT 1 FROM provider_accounts
             WHERE account_id = ?1 AND provider_kind = 'imap'",
            [&prepared.account_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some();
    if exists {
        transaction.execute(
            "UPDATE accounts SET name = ?2, email = ?3, provider = 'imap' WHERE id = ?1",
            params![prepared.account_id, prepared.display_name, prepared.email],
        )?;
        // Whatever stalled for want of credentials can now be tried again.
        transaction.execute(
            "UPDATE provider_work_items
             SET state = 'queued', available_at = ?2, last_error_code = NULL,
                 auth_block_reason = NULL
             WHERE account_id = ?1 AND state = 'authentication_blocked'",
            params![prepared.account_id, now_ms],
        )?;
        // The mailbox was just reached and signed into, so whatever had stopped
        // it syncing is over; a sync in flight, or one already waiting, is left
        // to finish.
        transaction.execute(
            "UPDATE provider_accounts
             SET remote_account_id = ?2, auth_state = 'ready', credential_ref = ?3,
                 auth_block_reason = NULL, last_error_code = NULL, updated_at = ?4,
                 sync_state = CASE
                   WHEN sync_state IN ('authentication_blocked', 'backoff', 'offline', 'failed')
                   THEN 'idle' ELSE sync_state END
             WHERE account_id = ?1 AND provider_kind = 'imap'",
            params![
                prepared.account_id,
                prepared.grant.remote_account_id,
                prepared.record_ref,
                now_ms
            ],
        )?;
    } else {
        let color = account_color_for(&prepared.account_id);
        transaction.execute(
            "INSERT INTO accounts(id, name, email, color, provider)
             VALUES(?1, ?2, ?3, ?4, 'imap')",
            params![
                prepared.account_id,
                prepared.display_name,
                prepared.email,
                color
            ],
        )?;
        transaction.execute(
            "INSERT INTO provider_accounts(
               account_id, provider_kind, remote_account_id, auth_state,
               credential_ref, sync_state, created_at, updated_at
             ) VALUES(?1, 'imap', ?2, 'ready', ?3, 'never_synced', ?4, ?4)",
            params![
                prepared.account_id,
                prepared.grant.remote_account_id,
                prepared.record_ref,
                now_ms
            ],
        )?;
    }
    transaction.commit()
}

fn validate_bounded_text(value: &str, maximum: usize) -> Result<(), ImapAccessError> {
    if value.is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        return Err(ImapAccessError::Permanent);
    }
    Ok(())
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        value.push(HEX[(byte >> 4) as usize] as char);
        value.push(HEX[(byte & 0x0f) as usize] as char);
    }
    value
}

/// Writes one IMAP authority record. The returned bytes are secret-bearing and
/// may only be written to the keychain.
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
    use crate::account_color::ACCOUNT_COLORS;
    use crate::store::MuxStore;
    use tempfile::tempdir;

    fn provision_request(port: u16, password: &str) -> ImapProvisionRequest {
        ImapProvisionRequest {
            display_name: "  Mail  ".into(),
            email: "  Reader@Example.Test ".into(),
            host: "Mail.Example.Test".into(),
            port,
            username: " reader@example.test ".into(),
            password: password.into(),
        }
    }

    #[test]
    fn one_mailbox_is_one_host_port_and_login() {
        let prepared = prepare_imap_account(&provision_request(993, "swordfish")).expect("prepare");
        // Normalized on the way in, so the same mailbox typed differently is
        // still the same mailbox.
        assert_eq!(prepared.grant.host, "mail.example.test");
        assert_eq!(prepared.grant.username, "reader@example.test");
        assert_eq!(prepared.email, "reader@example.test");
        assert_eq!(prepared.display_name, "Mail");
        assert!(prepared.account_id.starts_with("imap:"));
        assert!(prepared.record_ref.starts_with("imap/account/"));
        assert_eq!(prepared.account_id.len(), "imap:".len() + 32);

        let again = prepare_imap_account(&provision_request(993, "swordfish")).expect("prepare");
        assert_eq!(prepared.account_id, again.account_id);
        assert_eq!(prepared.record_ref, again.record_ref);
        // A different port is a different mailbox, and the identity says so.
        let other = prepare_imap_account(&provision_request(143, "swordfish")).expect("prepare");
        assert_ne!(prepared.account_id, other.account_id);
        assert_ne!(prepared.record_ref, other.record_ref);
    }

    #[test]
    fn unusable_mailbox_details_are_refused_before_anything_is_written() {
        let cases: Vec<(&str, ImapProvisionRequest)> = vec![
            (
                "empty host",
                ImapProvisionRequest {
                    host: String::new(),
                    ..provision_request(993, "s")
                },
            ),
            ("port zero", provision_request(0, "s")),
            ("empty password", provision_request(993, "")),
            (
                "trailing dot host",
                ImapProvisionRequest {
                    host: "mail.example.test.".into(),
                    ..provision_request(993, "s")
                },
            ),
            (
                "non-ascii host",
                ImapProvisionRequest {
                    host: "mäil.example.test".into(),
                    ..provision_request(993, "s")
                },
            ),
            (
                "empty username",
                ImapProvisionRequest {
                    username: "   ".into(),
                    ..provision_request(993, "s")
                },
            ),
            (
                "empty email",
                ImapProvisionRequest {
                    email: " ".into(),
                    ..provision_request(993, "s")
                },
            ),
            (
                "control in email",
                ImapProvisionRequest {
                    email: "reader\u{7f}@example.test".into(),
                    ..provision_request(993, "s")
                },
            ),
            (
                "oversized email",
                ImapProvisionRequest {
                    email: format!("{}@example.test", "e".repeat(400)),
                    ..provision_request(993, "s")
                },
            ),
            (
                "oversized password",
                provision_request(993, &"p".repeat(MAX_SECRET_BYTES + 1)),
            ),
            ("nul in password", provision_request(993, "pass\0word")),
        ];
        for (label, request) in cases {
            assert!(prepare_imap_account(&request).is_err(), "accepted {label}");
        }
    }

    #[test]
    fn a_provisioned_mailbox_reads_back_as_a_usable_grant() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("provision.db");
        drop(MuxStore::open(&path, false).expect("native schema"));
        let credentials =
            CredentialStore::temporary(&directory.path().join("provision.keychain-db"));
        let prepared = prepare_imap_account(&provision_request(993, "swordfish")).expect("prepare");
        persist_imap_account(&path, &credentials, &prepared, 5_000).expect("persist");

        // The whole point: what was written down is what the sync path reads.
        let access = ImapKeychainAccess::new(&path, credentials.clone());
        let grant = access
            .access_for_account(&prepared.account_id)
            .expect("grant");
        assert_eq!(grant.host, "mail.example.test");
        assert_eq!(grant.port, 993);
        assert_eq!(grant.username, "reader@example.test");
        assert_eq!(grant.password.as_str(), "swordfish");
        assert_eq!(grant.remote_account_id, "reader@example.test");

        let connection = Connection::open(&path).expect("connection");
        let (kind, auth, sync, reference): (String, String, String, String) = connection
            .query_row(
                "SELECT provider_kind, auth_state, sync_state, credential_ref
                 FROM provider_accounts WHERE account_id = ?1",
                [&prepared.account_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("provider row");
        assert_eq!(
            (kind.as_str(), auth.as_str(), sync.as_str()),
            ("imap", "ready", "never_synced")
        );
        assert_eq!(reference, prepared.record_ref);
        let (name, email, provider): (String, String, String) = connection
            .query_row(
                "SELECT name, email, provider FROM accounts WHERE id = ?1",
                [&prepared.account_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("account row");
        assert_eq!(
            (name.as_str(), email.as_str(), provider.as_str()),
            ("Mail", "reader@example.test", "imap")
        );
        let _ = credentials.remove(&prepared.record_ref);
    }

    #[test]
    fn adding_the_same_mailbox_again_corrects_it_and_releases_blocked_work() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("reprovision.db");
        drop(MuxStore::open(&path, false).expect("native schema"));
        let credentials =
            CredentialStore::temporary(&directory.path().join("reprovision.keychain-db"));
        let first = prepare_imap_account(&provision_request(993, "old-secret")).expect("prepare");
        persist_imap_account(&path, &credentials, &first, 5_000).expect("persist");

        let connection = Connection::open(&path).expect("connection");
        connection
            .execute(
                "UPDATE provider_accounts
                 SET auth_state = 'reauthorization_required',
                     auth_block_reason = 'provider_reauthorization',
                     sync_state = 'authentication_blocked'
                 WHERE account_id = ?1",
                [&first.account_id],
            )
            .expect("block the account");
        connection
            .execute(
                "INSERT INTO provider_work_items(
                   id, account_id, kind, scope, ordering_key, retry_safety,
                   payload_json, payload_fingerprint, state, priority, created_at,
                   available_at, attempt_count, max_attempts, auth_block_reason
                 ) VALUES('work-1', ?1, 'sync', 'scope', 'order', 'safe_retry',
                          '{}', '0123456789abcdef0123456789abcdef', 'authentication_blocked', 0, 0,
                          0, 1, 8, 'provider_reauthorization')",
                [&first.account_id],
            )
            .expect("blocked work fixture");

        let second = prepare_imap_account(&provision_request(993, "new-secret")).expect("prepare");
        assert_eq!(first.account_id, second.account_id);
        persist_imap_account(&path, &credentials, &second, 9_000).expect("reprovision");

        let access = ImapKeychainAccess::new(&path, credentials.clone());
        let grant = access
            .access_for_account(&second.account_id)
            .expect("grant after correction");
        assert_eq!(grant.password.as_str(), "new-secret");
        let (auth, sync, state): (String, String, String) = connection
            .query_row(
                "SELECT provider.auth_state, provider.sync_state, work.state
                 FROM provider_accounts provider
                 JOIN provider_work_items work ON work.account_id = provider.account_id
                 WHERE provider.account_id = ?1",
                [&second.account_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("rows after correction");
        assert_eq!(
            (auth.as_str(), sync.as_str(), state.as_str()),
            ("ready", "idle", "queued")
        );
        // Nothing is left holding the mailbox back: sync now takes it.
        assert_eq!(
            crate::imap::sync_account_now(&path, &second.account_id, 9_500).expect("schedule"),
            1
        );
        // One mailbox, not two.
        let accounts: i64 = connection
            .query_row("SELECT COUNT(*) FROM accounts", [], |row| row.get(0))
            .expect("account count");
        assert_eq!(accounts, 1);
        let _ = credentials.remove(&second.record_ref);
    }

    #[test]
    fn a_new_mailbox_is_queued_for_its_first_sync() {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("first-sync.db");
        drop(MuxStore::open(&path, false).expect("native schema"));
        let credentials =
            CredentialStore::temporary(&directory.path().join("first-sync.keychain-db"));
        let prepared = prepare_imap_account(&provision_request(993, "secret")).expect("prepare");
        persist_imap_account(&path, &credentials, &prepared, 5_000).expect("persist");

        // The row starts never_synced, and "sync now" is what the command calls
        // next: it has to take a mailbox with no history.
        assert_eq!(
            crate::imap::sync_account_now(&path, &prepared.account_id, 6_000).expect("schedule"),
            1
        );
        let connection = Connection::open(&path).expect("connection");
        let (sync_state, queued): (String, i64) = connection
            .query_row(
                "SELECT provider.sync_state,
                        (SELECT COUNT(*) FROM provider_work_items work
                         WHERE work.account_id = provider.account_id
                           AND work.kind = 'sync' AND work.state = 'queued')
                 FROM provider_accounts provider
                 WHERE provider.account_id = ?1",
                [&prepared.account_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("row after scheduling");
        assert_eq!((sync_state.as_str(), queued), ("scheduled", 1));
        let _ = credentials.remove(&prepared.record_ref);
    }

    #[test]
    fn the_locked_credential_state_cannot_be_written() {
        let (_directory, path, _credentials, _reference) = configured_fixture();
        let connection = Connection::open(&path).expect("connection");
        // The schema retired it, so nothing reading auth_state needs an arm for it.
        let refused = connection.execute(
            "UPDATE provider_accounts SET auth_state = 'credential_locked'
             WHERE account_id = 'imap-account'",
            [],
        );
        assert!(refused.is_err());
    }

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

    /// Settings offers the six colours new mailboxes are dealt, and the
    /// interface keeps its own copy of the list so it can name them. This holds
    /// the two together: a colour changed on one side fails here until the
    /// other follows.
    #[test]
    fn the_interface_offers_exactly_the_colours_new_mailboxes_are_dealt() {
        let source = include_str!("../../src/accountColor.ts");
        let palette = source
            .split("accountColorChoices")
            .nth(1)
            .and_then(|tail| tail.split("];").next())
            .expect("palette source");
        let offered = palette
            .split('\'')
            .filter(|literal| literal.starts_with('#'))
            .collect::<Vec<_>>();
        assert_eq!(offered, ACCOUNT_COLORS);
    }
}
