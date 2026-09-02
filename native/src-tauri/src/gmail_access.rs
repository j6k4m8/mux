//! Typed Gmail authority records held in the system keychain.
//!
//! This is deliberately not an IPC surface. A configured SQLite row contains
//! only an opaque keychain lookup reference; every adapter execution revalidates
//! the referenced record, account subject, scope, and expiry before I/O.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::blocking::Client;
use reqwest::redirect::Policy;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use crate::gmail::{GmailAccessError, GmailAccessGrant, GmailAccessSource};
use crate::keychain::{CredentialError, CredentialStore, CredentialStoreStatus};
use crate::worker::WorkerError;

const RECORD_VERSION: u8 = 1;
const READONLY_SCOPE: &str = "https://www.googleapis.com/auth/gmail.readonly";
const MODIFY_SCOPE: &str = "https://www.googleapis.com/auth/gmail.modify";
const REFRESH_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
const MAX_RECORD_BYTES: usize = 64 * 1024;
const MAX_FIELD_BYTES: usize = 16 * 1024;
const MAX_REFRESH_RESPONSE_BYTES: u64 = 64 * 1024;
// One Gmail page can make a profile request, a label request, a message-list
// request, and ten sequential message fetches. Each oversized raw fetch can
// consume its 45-second request budget before a second bounded metadata fetch,
// for a current maximum of 23 requests / 17m15s. Thirty minutes keeps cached
// and newly refreshed grants comfortably beyond that whole-page boundary.
const MIN_GRANT_LIFETIME_SECONDS: i64 = 30 * 60;
const MIN_GRANT_LIFETIME_MS: i64 = MIN_GRANT_LIFETIME_SECONDS * 1_000;

pub(crate) struct GmailAuthorizedTokens {
    pub(crate) refresh_value: Zeroizing<String>,
    pub(crate) access_value: Zeroizing<String>,
    pub(crate) expires_at_ms: i64,
}

pub(crate) struct RefreshedGrant {
    access_value: Zeroizing<String>,
    expires_at_ms: i64,
}

pub(crate) trait GmailGrantRefresher {
    fn refresh(
        &self,
        client_id: &str,
        client_value: Option<&str>,
        refresh_value: &str,
        now_ms: i64,
    ) -> Result<RefreshedGrant, GmailAccessError>;
}

pub(crate) struct GmailKeychainAccess<R> {
    database_path: PathBuf,
    credentials: CredentialStore,
    refresher: R,
}

impl<R> GmailKeychainAccess<R> {
    pub(crate) fn new(database_path: &Path, credentials: CredentialStore, refresher: R) -> Self {
        Self {
            database_path: database_path.to_owned(),
            credentials,
            refresher,
        }
    }
}

/// Validates every configured Gmail lookup marker against the keychain at
/// startup. Missing, malformed, wrong-version, wrong-scope, and wrong-subject
/// records are left provider-reauthorization blocked.
pub(crate) fn reconcile_credential_records(
    database_path: &Path,
    credentials: &CredentialStore,
    now_ms: i64,
) -> Result<usize, WorkerError> {
    if now_ms < 0 || credentials.status() != CredentialStoreStatus::Available {
        return Err(WorkerError::Conflict(
            "Gmail authority cannot be reconciled while the keychain is unavailable".into(),
        ));
    }
    let mut connection = Connection::open(database_path)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let accounts = {
        let mut statement = transaction.prepare(
            "SELECT account_id, remote_account_id, credential_ref
             FROM provider_accounts
             WHERE provider_kind = 'gmail' AND credential_ref IS NOT NULL
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
            Ok(Some(bytes)) => GmailAuthorityRecord::decode(&bytes)
                .and_then(|record| record.validate_for_subject(&remote_subject))
                .is_ok(),
            Ok(None) => false,
            Err(CredentialError::Unavailable) => {
                return Err(WorkerError::Conflict(
                    "Gmail authority reconciliation could not read the keychain".into(),
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
                 last_error_code = 'gmail_authority_invalid', updated_at = ?2
             WHERE account_id = ?1",
            params![&account_id, now_ms],
        )?;
        transaction.execute(
            "UPDATE provider_work_items
             SET state = 'authentication_blocked',
                 auth_block_reason = 'provider_reauthorization',
                 last_error_code = 'gmail_reauthorization_required',
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

impl<R> GmailKeychainAccess<R>
where
    R: GmailGrantRefresher + Sync,
{
    fn access_with_requirement(
        &self,
        account_id: &str,
        now_ms: i64,
        requires_modify: bool,
    ) -> Result<GmailAccessGrant, GmailAccessError> {
        let configured = read_configured_account(&self.database_path, account_id)?;
        match configured.auth_state.as_str() {
            "credential_locked" => return Err(GmailAccessError::CredentialUnavailable),
            "reauthorization_required" | "signed_out" => {
                return Err(GmailAccessError::ReauthorizationRequired)
            }
            "ready" => {}
            _ => return Err(GmailAccessError::Permanent),
        }
        let reference = configured
            .record_ref
            .ok_or(GmailAccessError::ReauthorizationRequired)?;
        let bytes = self
            .credentials
            .get(&reference)
            .map_err(map_credential_error)?
            .ok_or(GmailAccessError::ReauthorizationRequired)?;
        let mut record = GmailAuthorityRecord::decode(bytes.as_slice())?;
        record.validate_for_subject(&configured.remote_subject)?;
        if requires_modify && !record.can_mutate() {
            return Err(GmailAccessError::ReauthorizationRequired);
        }
        if record
            .access_value
            .as_ref()
            .zip(record.expires_at_ms)
            .is_some_and(|(_, expires_at)| {
                expires_at > now_ms.saturating_add(MIN_GRANT_LIFETIME_MS)
            })
        {
            return Ok(GmailAccessGrant {
                access_value: record
                    .access_value
                    .take()
                    .expect("validated access value exists"),
                remote_account_id: record.remote_subject.clone(),
            });
        }

        let refreshed = self.refresher.refresh(
            &record.client_id,
            record.client_value.as_deref().map(String::as_str),
            &record.refresh_value,
            now_ms,
        )?;
        record.access_value = Some(refreshed.access_value);
        record.expires_at_ms = Some(refreshed.expires_at_ms);
        let encoded = record.encode()?;

        // Re-read the non-secret reference after network I/O. Reauthorization
        // or sign-out that won the race must not be overwritten by this refresh.
        let current = read_configured_account(&self.database_path, account_id)?;
        if current.auth_state != "ready"
            || current.record_ref.as_deref() != Some(reference.as_str())
            || current.remote_subject != configured.remote_subject
        {
            return Err(GmailAccessError::ReauthorizationRequired);
        }
        self.credentials
            .put(&reference, encoded)
            .map_err(map_credential_error)?;
        Ok(GmailAccessGrant {
            access_value: record
                .access_value
                .take()
                .expect("refreshed access value exists"),
            remote_account_id: record.remote_subject.clone(),
        })
    }
}

impl<R> GmailAccessSource for GmailKeychainAccess<R>
where
    R: GmailGrantRefresher + Sync,
{
    fn access_for_account(
        &self,
        account_id: &str,
        now_ms: i64,
    ) -> Result<GmailAccessGrant, GmailAccessError> {
        self.access_with_requirement(account_id, now_ms, false)
    }

    fn access_for_mutation(
        &self,
        account_id: &str,
        now_ms: i64,
    ) -> Result<GmailAccessGrant, GmailAccessError> {
        self.access_with_requirement(account_id, now_ms, true)
    }
}

struct ConfiguredAccount {
    remote_subject: String,
    auth_state: String,
    record_ref: Option<String>,
}

fn read_configured_account(
    database_path: &Path,
    account_id: &str,
) -> Result<ConfiguredAccount, GmailAccessError> {
    let connection = Connection::open(database_path)
        .map_err(|_| GmailAccessError::RetryableBecause("database"))?;
    connection
        .query_row(
            "SELECT remote_account_id, auth_state, credential_ref
             FROM provider_accounts
             WHERE account_id = ?1 AND provider_kind = 'gmail'",
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
        .map_err(|_| GmailAccessError::RetryableBecause("database"))?
        .ok_or(GmailAccessError::ReauthorizationRequired)
}

struct GmailAuthorityRecord {
    client_id: String,
    client_value: Option<Zeroizing<String>>,
    remote_subject: String,
    scopes: Vec<String>,
    refresh_value: Zeroizing<String>,
    access_value: Option<Zeroizing<String>>,
    expires_at_ms: Option<i64>,
}

#[derive(Deserialize, Zeroize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GmailAuthorityRecordWire {
    version: u8,
    client_id: String,
    client_value: Option<String>,
    remote_subject: String,
    scopes: Vec<String>,
    refresh_value: String,
    access_value: Option<String>,
    expires_at_ms: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GmailAuthorityRecordWrite<'a> {
    version: u8,
    client_id: &'a str,
    client_value: Option<&'a str>,
    remote_subject: &'a str,
    scopes: &'a [String],
    refresh_value: &'a str,
    access_value: Option<&'a str>,
    expires_at_ms: Option<i64>,
}

impl GmailAuthorityRecord {
    fn decode(bytes: &[u8]) -> Result<Self, GmailAccessError> {
        if bytes.is_empty() || bytes.len() > MAX_RECORD_BYTES {
            return Err(GmailAccessError::Permanent);
        }
        let wire = Zeroizing::new(
            serde_json::from_slice::<GmailAuthorityRecordWire>(bytes)
                .map_err(|_| GmailAccessError::Permanent)?,
        );
        if wire.version != RECORD_VERSION
            || !matches!(wire.scopes.as_slice(), [scope] if scope == READONLY_SCOPE || scope == MODIFY_SCOPE)
            || wire.expires_at_ms.is_some_and(|value| value < 0)
        {
            return Err(GmailAccessError::Permanent);
        }
        for value in [
            wire.client_id.as_str(),
            wire.remote_subject.as_str(),
            wire.refresh_value.as_str(),
        ] {
            validate_field(value)?;
        }
        if let Some(value) = wire.client_value.as_deref() {
            validate_field(value)?;
        }
        if let Some(value) = wire.access_value.as_deref() {
            validate_field(value)?;
        }
        for scope in &wire.scopes {
            validate_field(scope)?;
        }
        Ok(Self {
            client_id: wire.client_id.clone(),
            client_value: wire.client_value.clone().map(Zeroizing::new),
            remote_subject: wire.remote_subject.clone(),
            scopes: wire.scopes.clone(),
            refresh_value: Zeroizing::new(wire.refresh_value.clone()),
            access_value: wire.access_value.clone().map(Zeroizing::new),
            expires_at_ms: wire.expires_at_ms,
        })
    }

    fn validate_for_subject(&self, expected_subject: &str) -> Result<(), GmailAccessError> {
        if self.remote_subject != expected_subject
            || !matches!(self.scopes.as_slice(), [scope] if scope == READONLY_SCOPE || scope == MODIFY_SCOPE)
        {
            return Err(GmailAccessError::ReauthorizationRequired);
        }
        Ok(())
    }

    fn can_mutate(&self) -> bool {
        self.scopes.as_slice() == [MODIFY_SCOPE]
    }

    fn encode(&self) -> Result<Zeroizing<Vec<u8>>, GmailAccessError> {
        serde_json::to_vec(&GmailAuthorityRecordWrite {
            version: RECORD_VERSION,
            client_id: &self.client_id,
            client_value: self.client_value.as_deref().map(String::as_str),
            remote_subject: &self.remote_subject,
            scopes: &self.scopes,
            refresh_value: &self.refresh_value,
            access_value: self.access_value.as_deref().map(String::as_str),
            expires_at_ms: self.expires_at_ms,
        })
        .map(Zeroizing::new)
        .map_err(|_| GmailAccessError::Permanent)
    }
}

pub(crate) fn encode_modify_authority(
    client_id: &str,
    client_value: Option<&str>,
    remote_subject: &str,
    tokens: GmailAuthorizedTokens,
) -> Result<Zeroizing<Vec<u8>>, GmailAccessError> {
    for value in [
        client_id,
        remote_subject,
        tokens.refresh_value.as_str(),
        tokens.access_value.as_str(),
    ] {
        validate_field(value)?;
    }
    if let Some(value) = client_value {
        validate_field(value)?;
    }
    if tokens.expires_at_ms < 0 {
        return Err(GmailAccessError::Permanent);
    }
    GmailAuthorityRecord {
        client_id: client_id.to_owned(),
        client_value: client_value.map(|value| Zeroizing::new(value.to_owned())),
        remote_subject: remote_subject.to_owned(),
        scopes: vec![MODIFY_SCOPE.to_owned()],
        refresh_value: tokens.refresh_value,
        access_value: Some(tokens.access_value),
        expires_at_ms: Some(tokens.expires_at_ms),
    }
    .encode()
}

fn validate_field(value: &str) -> Result<(), GmailAccessError> {
    if value.is_empty() || value.len() > MAX_FIELD_BYTES || value.chars().any(char::is_control) {
        return Err(GmailAccessError::Permanent);
    }
    Ok(())
}

fn map_credential_error(error: CredentialError) -> GmailAccessError {
    match error {
        // A keychain that cannot answer is a transient condition, not a decision.
        CredentialError::Unavailable => GmailAccessError::RetryableBecause("keychain"),
        CredentialError::InvalidIdentifier | CredentialError::LimitExceeded(_) => {
            GmailAccessError::Permanent
        }
    }
}

pub(crate) struct GoogleGrantRefresher {
    client: Client,
}

impl GoogleGrantRefresher {
    pub(crate) fn new() -> Result<Self, WorkerError> {
        let client = Client::builder()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .user_agent("Mux/0.1")
            .build()
            .map_err(|_| {
                WorkerError::Conflict("Could not initialize Gmail authorization transport".into())
            })?;
        Ok(Self { client })
    }
}

impl GmailGrantRefresher for GoogleGrantRefresher {
    fn refresh(
        &self,
        client_id: &str,
        client_value: Option<&str>,
        refresh_value: &str,
        now_ms: i64,
    ) -> Result<RefreshedGrant, GmailAccessError> {
        let mut form = vec![
            ("client_id", client_id),
            ("refresh_token", refresh_value),
            ("grant_type", "refresh_token"),
        ];
        if let Some(value) = client_value {
            form.push(("client_secret", value));
        }
        let response = self
            .client
            .post(REFRESH_ENDPOINT)
            .form(&form)
            .send()
            .map_err(classify_transport_error)?;
        let status = response.status();
        let retry_after_at = bounded_retry_after(response.headers(), now_ms);
        let bytes = read_bounded(response, MAX_REFRESH_RESPONSE_BYTES)?;
        decode_refresh_response(status, retry_after_at, bytes, now_ms)
    }
}

fn decode_refresh_response(
    status: reqwest::StatusCode,
    retry_after_at: i64,
    bytes: Zeroizing<Vec<u8>>,
    now_ms: i64,
) -> Result<RefreshedGrant, GmailAccessError> {
    if status.as_u16() == 429 {
        return Err(GmailAccessError::RateLimited { retry_after_at });
    }
    if status.is_server_error() {
        return Err(GmailAccessError::RetryableBecause("provider_5xx"));
    }
    if !status.is_success() {
        let error: RefreshErrorWire =
            serde_json::from_slice(&bytes).unwrap_or(RefreshErrorWire { error: None });
        return if error.error.as_deref() == Some("invalid_grant") {
            Err(GmailAccessError::ReauthorizationRequired)
        } else {
            Err(GmailAccessError::Permanent)
        };
    }
    let mut wire: RefreshSuccessWire =
        serde_json::from_slice(&bytes).map_err(|_| GmailAccessError::Permanent)?;
    validate_field(&wire.access_token)?;
    if !wire.token_type.eq_ignore_ascii_case("bearer")
        || !(MIN_GRANT_LIFETIME_SECONDS..=86_400).contains(&wire.expires_in)
    {
        return Err(GmailAccessError::Permanent);
    }
    let expires_at_ms = now_ms
        .checked_add(wire.expires_in.saturating_mul(1_000))
        .ok_or(GmailAccessError::Permanent)?;
    Ok(RefreshedGrant {
        access_value: Zeroizing::new(std::mem::take(&mut wire.access_token)),
        expires_at_ms,
    })
}

#[derive(Deserialize, Zeroize)]
#[zeroize(drop)]
struct RefreshSuccessWire {
    access_token: String,
    expires_in: i64,
    token_type: String,
}

#[derive(Deserialize)]
struct RefreshErrorWire {
    error: Option<String>,
}

fn read_bounded(
    response: reqwest::blocking::Response,
    max_bytes: u64,
) -> Result<Zeroizing<Vec<u8>>, GmailAccessError> {
    if response
        .content_length()
        .is_some_and(|length| length > max_bytes)
    {
        return Err(GmailAccessError::Permanent);
    }
    let read_limit = max_bytes.saturating_add(1);
    let capacity = usize::try_from(read_limit).map_err(|_| GmailAccessError::Permanent)?;
    // Allocate the complete bounded buffer under Zeroizing before the first
    // read so growth cannot leave credential-bearing response copies behind.
    let mut bytes = Zeroizing::new(Vec::with_capacity(capacity));
    response
        .take(read_limit)
        .read_to_end(&mut bytes)
        .map_err(|_| GmailAccessError::RetryableBecause("response_read"))?;
    if bytes.len() as u64 > max_bytes {
        return Err(GmailAccessError::Permanent);
    }
    Ok(bytes)
}

fn classify_transport_error(error: reqwest::Error) -> GmailAccessError {
    if error.is_timeout() || error.is_connect() {
        GmailAccessError::RetryableBecause("network")
    } else {
        GmailAccessError::Permanent
    }
}

fn bounded_retry_after(headers: &reqwest::header::HeaderMap, now_ms: i64) -> i64 {
    let seconds = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(60)
        .clamp(1, 24 * 60 * 60);
    now_ms.saturating_add(seconds.saturating_mul(1_000))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rusqlite::params;
    use tempfile::tempdir;

    use super::*;
    use crate::store::MuxStore;

    struct FixtureRefresher {
        calls: AtomicUsize,
    }

    impl GmailGrantRefresher for FixtureRefresher {
        fn refresh(
            &self,
            _client_id: &str,
            _client_value: Option<&str>,
            _refresh_value: &str,
            now_ms: i64,
        ) -> Result<RefreshedGrant, GmailAccessError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(RefreshedGrant {
                access_value: Zeroizing::new("synthetic-refreshed-value".into()),
                expires_at_ms: now_ms + 3_600_000,
            })
        }
    }

    fn authority_record(subject: &str, expires_at_ms: i64) -> Vec<u8> {
        authority_record_with_scope(subject, expires_at_ms, READONLY_SCOPE)
    }

    fn authority_record_with_scope(subject: &str, expires_at_ms: i64, scope: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "clientId": "desktop-client.example.invalid",
            "clientValue": null,
            "remoteSubject": subject,
            "scopes": [scope],
            "refreshValue": "synthetic-refresh-value",
            "accessValue": "synthetic-access-value",
            "expiresAtMs": expires_at_ms
        }))
        .unwrap()
    }

    /// Each fixture gets its own keychain service, and removes what it wrote.
    struct FixtureCredentials {
        store: CredentialStore,
        identifiers: Vec<&'static str>,
    }

    impl Drop for FixtureCredentials {
        fn drop(&mut self) {
            for identifier in &self.identifiers {
                let _ = self.store.remove(identifier);
            }
        }
    }

    static FIXTURE_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    fn configured_fixture() -> (tempfile::TempDir, PathBuf, FixtureCredentials) {
        let directory = tempdir().expect("temporary directory");
        let path = directory.path().join("gmail-access.db");
        drop(MuxStore::open(&path, false).expect("native store"));
        let connection = Connection::open(&path).expect("fixture connection");
        connection
            .execute(
                "INSERT INTO accounts(id, name, email, color, provider)
                 VALUES('gmail-account', 'Gmail', 'reader@example.test', '#000000', 'gmail')",
                [],
            )
            .expect("account fixture");
        connection
            .execute(
                "INSERT INTO provider_accounts(
                   account_id, provider_kind, remote_account_id, auth_state,
                   credential_ref, sync_state, created_at, updated_at
                 ) VALUES(?1, 'gmail', ?2, 'ready', ?3, 'never_synced', 0, 0)",
                params!["gmail-account", "subject-1", "gmail/fixture/a"],
            )
            .expect("provider account fixture");
        drop(connection);
        let sequence = FIXTURE_SEQUENCE.fetch_add(1, Ordering::SeqCst);
        // A throwaway keychain per fixture: no login-keychain access, no prompts.
        let store = CredentialStore::temporary(
            &directory
                .path()
                .join(format!("gmail-access-{sequence}.keychain")),
        );
        store
            .put(
                "gmail/fixture/a",
                Zeroizing::new(authority_record("subject-1", 4_000_000)),
            )
            .expect("put authority record");
        (
            directory,
            path,
            FixtureCredentials {
                store,
                identifiers: vec!["gmail/fixture/a"],
            },
        )
    }

    #[test]
    fn gmail_provider_conformance_refresh_transport_classifies_bounded_statuses() {
        let now_ms = 50_000;
        let success = serde_json::json!({
            "access_token": "synthetic-live-access",
            "expires_in": MIN_GRANT_LIFETIME_SECONDS,
            "token_type": "Bearer"
        })
        .to_string();
        let refreshed = decode_refresh_response(
            reqwest::StatusCode::OK,
            now_ms,
            Zeroizing::new(success.into_bytes()),
            now_ms,
        )
        .expect("bounded successful refresh");
        assert_eq!(refreshed.access_value.as_str(), "synthetic-live-access");
        assert_eq!(refreshed.expires_at_ms, now_ms + MIN_GRANT_LIFETIME_MS);

        let too_short = serde_json::json!({
            "access_token": "synthetic-short-access",
            "expires_in": MIN_GRANT_LIFETIME_SECONDS - 1,
            "token_type": "Bearer"
        })
        .to_string();
        assert!(matches!(
            decode_refresh_response(
                reqwest::StatusCode::OK,
                now_ms,
                Zeroizing::new(too_short.into_bytes()),
                now_ms,
            ),
            Err(GmailAccessError::Permanent)
        ));

        assert!(matches!(
            decode_refresh_response(
                reqwest::StatusCode::BAD_REQUEST,
                now_ms,
                Zeroizing::new(br#"{"error":"invalid_grant"}"#.to_vec()),
                now_ms,
            ),
            Err(GmailAccessError::ReauthorizationRequired)
        ));
        assert!(matches!(
            decode_refresh_response(
                reqwest::StatusCode::TOO_MANY_REQUESTS,
                now_ms + 7_000,
                Zeroizing::new(br#"{}"#.to_vec()),
                now_ms,
            ),
            Err(GmailAccessError::RateLimited {
                retry_after_at
            }) if retry_after_at == now_ms + 7_000
        ));
        assert!(matches!(
            decode_refresh_response(
                reqwest::StatusCode::SERVICE_UNAVAILABLE,
                now_ms,
                Zeroizing::new(br#"{}"#.to_vec()),
                now_ms,
            ),
            Err(GmailAccessError::RetryableBecause("provider_5xx"))
        ));
    }

    #[test]
    fn gmail_provider_conformance_refresh_transport_rejects_oversize_response() {
        let response: reqwest::blocking::Response = tauri::http::Response::builder()
            .body(vec![b'x'; MAX_REFRESH_RESPONSE_BYTES as usize + 1])
            .expect("oversize response")
            .into();
        assert!(matches!(
            read_bounded(response, MAX_REFRESH_RESPONSE_BYTES),
            Err(GmailAccessError::Permanent)
        ));
    }

    #[test]
    fn gmail_provider_conformance_typed_record_rejects_wrong_subject_and_scope() {
        let valid = serde_json::json!({
            "version": 1,
            "clientId": "desktop-client.example.invalid",
            "clientValue": null,
            "remoteSubject": "subject-1",
            "scopes": [READONLY_SCOPE],
            "refreshValue": "synthetic-refresh-value",
            "accessValue": "synthetic-access-value",
            "expiresAtMs": 5000
        });
        let record =
            GmailAuthorityRecord::decode(valid.to_string().as_bytes()).expect("bounded record");
        assert!(record.validate_for_subject("subject-1").is_ok());
        assert!(matches!(
            record.validate_for_subject("subject-2"),
            Err(GmailAccessError::ReauthorizationRequired)
        ));

        let mut wrong_scope = valid;
        wrong_scope["scopes"] = serde_json::json!(["openid"]);
        assert!(GmailAuthorityRecord::decode(wrong_scope.to_string().as_bytes()).is_err());
    }

    #[test]
    fn gmail_provider_conformance_mutations_require_modify_authority() {
        let (_directory, path, credentials) = configured_fixture();
        let source = GmailKeychainAccess::new(
            &path,
            credentials.store.clone(),
            FixtureRefresher {
                calls: AtomicUsize::new(0),
            },
        );
        assert!(source.access_for_account("gmail-account", 1_000).is_ok());
        assert!(matches!(
            source.access_for_mutation("gmail-account", 1_000),
            Err(GmailAccessError::ReauthorizationRequired)
        ));
        credentials
            .store
            .put(
                "gmail/fixture/a",
                Zeroizing::new(authority_record_with_scope(
                    "subject-1",
                    4_000_000,
                    MODIFY_SCOPE,
                )),
            )
            .unwrap();
        let grant = source
            .access_for_mutation("gmail-account", 1_000)
            .expect("modify scope authorizes a mutation grant");
        assert_eq!(grant.remote_account_id, "subject-1");
    }

    #[test]
    fn gmail_provider_conformance_record_encoding_contains_no_debug_surface() {
        let value = serde_json::json!({
            "version": 1,
            "clientId": "desktop-client.example.invalid",
            "clientValue": null,
            "remoteSubject": "subject-1",
            "scopes": [READONLY_SCOPE],
            "refreshValue": "synthetic-refresh-value",
            "accessValue": "synthetic-access-value",
            "expiresAtMs": 5000
        });
        let record =
            GmailAuthorityRecord::decode(value.to_string().as_bytes()).expect("bounded record");
        let encoded = record.encode().expect("encode record");
        assert!(encoded.len() <= MAX_RECORD_BYTES);
        assert!(serde_json::from_slice::<serde_json::Value>(&encoded).is_ok());
    }

    #[test]
    fn gmail_provider_conformance_keychain_record_is_verified_before_access_or_refresh() {
        let (_directory, path, credentials) = configured_fixture();
        let source = GmailKeychainAccess::new(
            &path,
            credentials.store.clone(),
            FixtureRefresher {
                calls: AtomicUsize::new(0),
            },
        );
        assert_eq!(
            source
                .access_for_account("gmail-account", 1_000)
                .expect("unexpired access")
                .access_value
                .as_str(),
            "synthetic-access-value"
        );
        assert_eq!(source.refresher.calls.load(Ordering::SeqCst), 0);

        // A record that disappears must stop access rather than reuse a cached grant.
        credentials
            .store
            .remove("gmail/fixture/a")
            .expect("remove authority record");
        assert!(matches!(
            source.access_for_account("gmail-account", 1_000),
            Err(GmailAccessError::ReauthorizationRequired)
        ));
        assert_eq!(source.refresher.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn gmail_provider_conformance_near_expiry_access_refreshes_before_a_whole_page() {
        let (_directory, path, credentials) = configured_fixture();
        let now_ms = 1_000;
        credentials
            .store
            .put(
                "gmail/fixture/a",
                Zeroizing::new(authority_record(
                    "subject-1",
                    now_ms + MIN_GRANT_LIFETIME_MS,
                )),
            )
            .expect("replace near-expiry authority record");
        let source = GmailKeychainAccess::new(
            &path,
            credentials.store.clone(),
            FixtureRefresher {
                calls: AtomicUsize::new(0),
            },
        );
        assert_eq!(
            source
                .access_for_account("gmail-account", now_ms)
                .expect("refresh grant before page boundary")
                .access_value
                .as_str(),
            "synthetic-refreshed-value"
        );
        assert_eq!(source.refresher.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn gmail_provider_conformance_expired_access_refreshes_and_rewrites_only_the_vault() {
        let (_directory, path, credentials) = configured_fixture();
        credentials
            .store
            .put(
                "gmail/fixture/a",
                Zeroizing::new(authority_record("subject-1", 100)),
            )
            .expect("replace expired authority record");
        let source = GmailKeychainAccess::new(
            &path,
            credentials.store.clone(),
            FixtureRefresher {
                calls: AtomicUsize::new(0),
            },
        );
        assert_eq!(
            source
                .access_for_account("gmail-account", 1_000)
                .expect("refresh expired access")
                .access_value
                .as_str(),
            "synthetic-refreshed-value"
        );
        assert_eq!(source.refresher.calls.load(Ordering::SeqCst), 1);
        let stored = credentials
            .store
            .get("gmail/fixture/a")
            .expect("read refreshed record")
            .expect("refreshed record exists");
        let stored: serde_json::Value = serde_json::from_slice(&stored).expect("record JSON");
        assert_eq!(stored["accessValue"], "synthetic-refreshed-value");

        let connection = Connection::open(path).expect("verification connection");
        assert_eq!(
            connection
                .query_row(
                    "SELECT auth_state || ':' || credential_ref
                     FROM provider_accounts WHERE account_id = 'gmail-account'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("provider marker"),
            "ready:gmail/fixture/a"
        );
    }

    #[test]
    fn gmail_provider_conformance_subject_mismatch_never_refreshes() {
        let (_directory, path, credentials) = configured_fixture();
        credentials
            .store
            .put(
                "gmail/fixture/a",
                Zeroizing::new(authority_record("different-subject", 100)),
            )
            .expect("replace mismatched record");
        let source = GmailKeychainAccess::new(
            &path,
            credentials.store.clone(),
            FixtureRefresher {
                calls: AtomicUsize::new(0),
            },
        );
        assert!(matches!(
            source.access_for_account("gmail-account", 1_000),
            Err(GmailAccessError::ReauthorizationRequired)
        ));
        assert_eq!(source.refresher.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn gmail_provider_conformance_reconciliation_blocks_missing_records_at_startup() {
        let (_directory, path, credentials) = configured_fixture();
        credentials
            .store
            .remove("gmail/fixture/a")
            .expect("remove authority record");
        let connection = Connection::open(&path).expect("fixture connection");
        connection
            .execute(
                "UPDATE provider_accounts SET auth_state = 'ready', sync_state = 'idle'
                 WHERE account_id = 'gmail-account'",
                [],
            )
            .expect("account believed usable");
        drop(connection);

        assert_eq!(
            reconcile_credential_records(&path, &credentials.store, 5_000,)
                .expect("reconcile missing record"),
            1
        );
        let connection = Connection::open(path).expect("verification connection");
        assert_eq!(
            connection
                .query_row(
                    "SELECT auth_state || ':' || auth_block_reason || ':' || sync_state
                     FROM provider_accounts WHERE account_id = 'gmail-account'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("blocked account state"),
            "reauthorization_required:provider_reauthorization:authentication_blocked"
        );
    }
}
