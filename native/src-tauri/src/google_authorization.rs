//! Installed-app Gmail OAuth onboarding.
//!
//! The WebView receives only bounded lifecycle results. Client registration,
//! authorization codes, access tokens, and refresh tokens remain in Rust and
//! the completed authority record is written only to the encrypted vault.

use std::fs;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rand_core::{OsRng, RngCore};
use reqwest::blocking::{Client, Response};
use reqwest::redirect::Policy;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::{Zeroize, Zeroizing};

use crate::gmail_access::{encode_modify_authority, GmailAuthorizedTokens};
use crate::keychain::{CredentialStore, CredentialStoreStatus};

const CLIENT_CONFIG_ENV: &str = "MUX_GOOGLE_OAUTH_CLIENT_CONFIG";
const AUTHORIZATION_ENDPOINT: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_ENDPOINT: &str = "https://oauth2.googleapis.com/token";
const PROFILE_ENDPOINT: &str =
    "https://gmail.googleapis.com/gmail/v1/users/me/profile?fields=emailAddress";
const GMAIL_MODIFY_SCOPE: &str = "https://www.googleapis.com/auth/gmail.modify";
const CALLBACK_PATH: &str = "/";
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const MAX_HTTP_REQUEST_BYTES: usize = 8 * 1024;
const MAX_PROVIDER_RESPONSE_BYTES: u64 = 64 * 1024;
const MAX_CALLBACK_ATTEMPTS: usize = 32;
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(180);
const SOCKET_READ_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GmailOAuthResult {
    pub state: &'static str,
    pub account_id: String,
    pub email: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct GmailOAuthCancellation {
    pub state: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GmailOAuthCommandError {
    pub code: &'static str,
    pub message: &'static str,
}

impl GmailOAuthCommandError {
    fn new(code: &'static str, message: &'static str) -> Self {
        Self { code, message }
    }

    pub(crate) fn keychain_unavailable() -> Self {
        Self::new(
            "keychain_unavailable",
            "Mux could not reach the system keychain.",
        )
    }

    fn configuration() -> Self {
        Self::new(
            "oauth_client_unavailable",
            "Google authorization is not configured for this build.",
        )
    }

    fn busy() -> Self {
        Self::new(
            "oauth_already_active",
            "A Google authorization is already in progress.",
        )
    }

    fn cancelled() -> Self {
        Self::new("oauth_cancelled", "Google authorization was cancelled.")
    }

    fn timed_out() -> Self {
        Self::new(
            "oauth_timed_out",
            "Google authorization timed out. Try connecting again.",
        )
    }

    fn denied() -> Self {
        Self::new("oauth_denied", "Google authorization was not granted.")
    }

    fn provider() -> Self {
        Self::new(
            "oauth_provider_error",
            "Google authorization could not be completed.",
        )
    }

    fn storage() -> Self {
        Self::new(
            "oauth_storage_error",
            "The authorized account could not be saved securely.",
        )
    }

    fn browser() -> Self {
        Self::new(
            "oauth_browser_error",
            "The system browser could not be opened for Google authorization.",
        )
    }
}

pub(crate) struct GmailOAuthCoordinator {
    active: Mutex<Option<ActiveFlow>>,
}

struct ActiveFlow {
    id: String,
    cancellation: Arc<AtomicBool>,
}

pub(crate) struct GmailOAuthReservation {
    coordinator: Arc<GmailOAuthCoordinator>,
    id: String,
    pub(crate) cancellation: Arc<AtomicBool>,
}

impl GmailOAuthCoordinator {
    pub(crate) fn new() -> Self {
        Self {
            active: Mutex::new(None),
        }
    }

    pub(crate) fn reserve(
        self: &Arc<Self>,
    ) -> Result<GmailOAuthReservation, GmailOAuthCommandError> {
        let id = random_token();
        let cancellation = Arc::new(AtomicBool::new(false));
        let mut active = self
            .active
            .lock()
            .map_err(|_| GmailOAuthCommandError::storage())?;
        if active.is_some() {
            return Err(GmailOAuthCommandError::busy());
        }
        *active = Some(ActiveFlow {
            id: id.clone(),
            cancellation: Arc::clone(&cancellation),
        });
        Ok(GmailOAuthReservation {
            coordinator: Arc::clone(self),
            id,
            cancellation,
        })
    }

    pub(crate) fn cancel(&self) -> Result<GmailOAuthCancellation, GmailOAuthCommandError> {
        let active = self
            .active
            .lock()
            .map_err(|_| GmailOAuthCommandError::storage())?;
        let state = if let Some(active) = active.as_ref() {
            active.cancellation.store(true, Ordering::SeqCst);
            "cancel_requested"
        } else {
            "idle"
        };
        Ok(GmailOAuthCancellation { state })
    }
}

impl Drop for GmailOAuthReservation {
    fn drop(&mut self) {
        if let Ok(mut active) = self.coordinator.active.lock() {
            if active.as_ref().is_some_and(|active| active.id == self.id) {
                *active = None;
            }
        }
    }
}

#[derive(Deserialize, Zeroize)]
#[zeroize(drop)]
struct ClientConfigRoot {
    installed: InstalledClientConfigWire,
}

#[derive(Deserialize, Zeroize)]
#[zeroize(drop)]
struct InstalledClientConfigWire {
    client_id: String,
    client_secret: Option<String>,
    #[serde(default)]
    redirect_uris: Vec<String>,
}

struct InstalledClientConfig {
    client_id: String,
    client_secret: Option<Zeroizing<String>>,
}

impl InstalledClientConfig {
    fn load_from_environment() -> Result<Self, GmailOAuthCommandError> {
        let path = std::env::var_os(CLIENT_CONFIG_ENV)
            .map(PathBuf::from)
            .ok_or_else(GmailOAuthCommandError::configuration)?;
        Self::load(&path)
    }

    fn load(path: &Path) -> Result<Self, GmailOAuthCommandError> {
        let metadata = fs::metadata(path).map_err(|_| GmailOAuthCommandError::configuration())?;
        if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_CONFIG_BYTES {
            return Err(GmailOAuthCommandError::configuration());
        }
        let mut bytes = Zeroizing::new(Vec::with_capacity(metadata.len() as usize));
        fs::File::open(path)
            .and_then(|file| file.take(MAX_CONFIG_BYTES + 1).read_to_end(&mut bytes))
            .map_err(|_| GmailOAuthCommandError::configuration())?;
        if bytes.len() as u64 > MAX_CONFIG_BYTES {
            return Err(GmailOAuthCommandError::configuration());
        }
        let wire = Zeroizing::new(
            serde_json::from_slice::<ClientConfigRoot>(&bytes)
                .map_err(|_| GmailOAuthCommandError::configuration())?,
        );
        if !valid_client_id(&wire.installed.client_id)
            || !wire
                .installed
                .redirect_uris
                .iter()
                .any(|redirect| redirect == "http://localhost" || redirect == "http://127.0.0.1")
            || wire.installed.client_secret.as_ref().is_some_and(|secret| {
                secret.is_empty()
                    || secret.len() > 16 * 1024
                    || secret.chars().any(char::is_control)
            })
        {
            return Err(GmailOAuthCommandError::configuration());
        }
        Ok(Self {
            client_id: wire.installed.client_id.clone(),
            client_secret: wire.installed.client_secret.clone().map(Zeroizing::new),
        })
    }
}

fn valid_client_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 16 * 1024
        && value.ends_with(".apps.googleusercontent.com")
        && !value.chars().any(char::is_control)
}

struct PkceMaterial {
    verifier: Zeroizing<String>,
    challenge: String,
    state: Zeroizing<String>,
}

fn pkce_material() -> PkceMaterial {
    let verifier = Zeroizing::new(random_token());
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    PkceMaterial {
        verifier,
        challenge,
        state: Zeroizing::new(random_token()),
    }
}

fn random_token() -> String {
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let value = URL_SAFE_NO_PAD.encode(bytes);
    bytes.zeroize();
    value
}

fn authorization_url(
    client_id: &str,
    redirect_uri: &str,
    pkce: &PkceMaterial,
) -> Result<String, GmailOAuthCommandError> {
    let mut url = reqwest::Url::parse(AUTHORIZATION_ENDPOINT)
        .map_err(|_| GmailOAuthCommandError::configuration())?;
    url.query_pairs_mut()
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", GMAIL_MODIFY_SCOPE)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &pkce.state)
        .append_pair("access_type", "offline")
        .append_pair("prompt", "consent")
        .append_pair("include_granted_scopes", "false");
    Ok(url.into())
}

fn loopback_redirect_uri(host: &str) -> String {
    format!("http://{host}")
}

pub(crate) struct AuthorizedMailbox {
    email: String,
    tokens: GmailAuthorizedTokens,
    client_id: String,
    client_secret: Option<Zeroizing<String>>,
}

pub(crate) fn authorize<F>(
    cancellation: &AtomicBool,
    open_browser: F,
    now_ms: i64,
) -> Result<AuthorizedMailbox, GmailOAuthCommandError>
where
    F: FnOnce(&str) -> Result<(), ()>,
{
    if cancellation.load(Ordering::SeqCst) {
        return Err(GmailOAuthCommandError::cancelled());
    }
    let client_config = InstalledClientConfig::load_from_environment()?;
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
        .map_err(|_| GmailOAuthCommandError::provider())?;
    listener
        .set_nonblocking(true)
        .map_err(|_| GmailOAuthCommandError::provider())?;
    let port = listener
        .local_addr()
        .map_err(|_| GmailOAuthCommandError::provider())?
        .port();
    let host = format!("127.0.0.1:{port}");
    let redirect_uri = loopback_redirect_uri(&host);
    let pkce = pkce_material();
    let authorize_url = authorization_url(&client_config.client_id, &redirect_uri, &pkce)?;
    open_browser(&authorize_url).map_err(|_| GmailOAuthCommandError::browser())?;

    let code = wait_for_callback(
        &listener,
        &host,
        &pkce.state,
        cancellation,
        CALLBACK_TIMEOUT,
    )?;
    if cancellation.load(Ordering::SeqCst) {
        return Err(GmailOAuthCommandError::cancelled());
    }
    let transport = GoogleOAuthTransport::new()?;
    let tokens =
        transport.exchange(&client_config, &code, &redirect_uri, &pkce.verifier, now_ms)?;
    if cancellation.load(Ordering::SeqCst) {
        return Err(GmailOAuthCommandError::cancelled());
    }
    let email = transport.profile_email(&tokens.access_value)?;
    Ok(AuthorizedMailbox {
        email,
        tokens,
        client_id: client_config.client_id,
        client_secret: client_config.client_secret,
    })
}

fn wait_for_callback(
    listener: &TcpListener,
    expected_host: &str,
    expected_state: &str,
    cancellation: &AtomicBool,
    timeout: Duration,
) -> Result<Zeroizing<String>, GmailOAuthCommandError> {
    let started = Instant::now();
    let mut attempts = 0;
    while started.elapsed() < timeout {
        if cancellation.load(Ordering::SeqCst) {
            return Err(GmailOAuthCommandError::cancelled());
        }
        match listener.accept() {
            Ok((mut stream, peer)) => {
                attempts += 1;
                if peer.ip() != Ipv4Addr::LOCALHOST || attempts > MAX_CALLBACK_ATTEMPTS {
                    let _ = write_callback_response(&mut stream, false);
                    if attempts > MAX_CALLBACK_ATTEMPTS {
                        return Err(GmailOAuthCommandError::provider());
                    }
                    continue;
                }
                let connection_deadline =
                    (Instant::now() + SOCKET_READ_TIMEOUT).min(started + timeout);
                let parsed = read_callback(
                    &mut stream,
                    expected_host,
                    expected_state,
                    cancellation,
                    connection_deadline,
                );
                match parsed {
                    Ok(Callback::Code(code)) => {
                        let _ = write_callback_response(&mut stream, true);
                        return Ok(code);
                    }
                    Ok(Callback::Denied) => {
                        let _ = write_callback_response(&mut stream, false);
                        return Err(GmailOAuthCommandError::denied());
                    }
                    Err(()) => {
                        let _ = write_callback_response(&mut stream, false);
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return Err(GmailOAuthCommandError::provider()),
        }
    }
    Err(GmailOAuthCommandError::timed_out())
}

enum Callback {
    Code(Zeroizing<String>),
    Denied,
}

fn read_callback(
    stream: &mut TcpStream,
    expected_host: &str,
    expected_state: &str,
    cancellation: &AtomicBool,
    deadline: Instant,
) -> Result<Callback, ()> {
    let request = read_callback_request(stream, |stream| {
        let budget = callback_read_budget(cancellation, deadline)?;
        stream
            .set_read_timeout(Some(budget.min(Duration::from_millis(250))))
            .map_err(|_| ())
    })?;
    parse_callback_request(&request, expected_host, expected_state)
}

fn callback_read_budget(cancellation: &AtomicBool, deadline: Instant) -> Result<Duration, ()> {
    if cancellation.load(Ordering::SeqCst) {
        return Err(());
    }
    deadline.checked_duration_since(Instant::now()).ok_or(())
}

fn parse_callback_request(
    request: &[u8],
    expected_host: &str,
    expected_state: &str,
) -> Result<Callback, ()> {
    let request = std::str::from_utf8(request).map_err(|_| ())?;
    let mut lines = request.split("\r\n");
    let request_line = lines.next().ok_or(())?.split(' ').collect::<Vec<_>>();
    let ["GET", target, "HTTP/1.1"] = request_line.as_slice() else {
        return Err(());
    };
    let mut host = None;
    for (name, value) in lines.filter_map(|line| line.split_once(':')) {
        if name.eq_ignore_ascii_case("host") && host.replace(value.trim()).is_some() {
            return Err(());
        }
    }
    if host != Some(expected_host) {
        return Err(());
    }
    parse_callback_target(target, expected_host, expected_state)
}

fn read_callback_request<R, F>(
    reader: &mut R,
    mut prepare_read: F,
) -> Result<Zeroizing<Vec<u8>>, ()>
where
    R: Read,
    F: FnMut(&mut R) -> Result<(), ()>,
{
    let mut request = Zeroizing::new(Vec::with_capacity(MAX_HTTP_REQUEST_BYTES + 1));
    let mut chunk = [0_u8; 1024];
    loop {
        prepare_read(reader)?;
        let count = reader.read(&mut chunk).map_err(|_| ())?;
        if count == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..count]);
        chunk[..count].zeroize();
        if request.len() > MAX_HTTP_REQUEST_BYTES {
            return Err(());
        }
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
    }
    chunk.zeroize();
    if request.is_empty() || !request.windows(4).any(|window| window == b"\r\n\r\n") {
        return Err(());
    }
    Ok(request)
}

fn parse_callback_target(
    target: &str,
    expected_host: &str,
    expected_state: &str,
) -> Result<Callback, ()> {
    let url = reqwest::Url::parse(&format!("http://{expected_host}{target}")).map_err(|_| ())?;
    if url.path() != CALLBACK_PATH || url.fragment().is_some() {
        return Err(());
    }
    let mut state = None;
    let mut code = None;
    let mut error = None;
    for (key, value) in url.query_pairs() {
        let slot = match key.as_ref() {
            "state" => &mut state,
            "code" => &mut code,
            "error" => &mut error,
            _ => continue,
        };
        if slot.replace(value.into_owned()).is_some() {
            return Err(());
        }
    }
    if state.as_deref() != Some(expected_state) {
        return Err(());
    }
    let code = match (error, code) {
        (Some(_), None) => return Ok(Callback::Denied),
        (None, Some(code)) => code,
        _ => return Err(()),
    };
    if code.is_empty() || code.len() > 16 * 1024 || code.chars().any(char::is_control) {
        return Err(());
    }
    Ok(Callback::Code(Zeroizing::new(code)))
}

fn write_callback_response(stream: &mut TcpStream, success: bool) -> std::io::Result<()> {
    let (status, body) = if success {
        ("200 OK", "Authorization complete. Return to Mux.")
    } else {
        (
            "400 Bad Request",
            "Authorization was not completed. Return to Mux.",
        )
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes())
}

struct GoogleOAuthTransport {
    client: Client,
}

impl GoogleOAuthTransport {
    fn new() -> Result<Self, GmailOAuthCommandError> {
        let client = Client::builder()
            .redirect(Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .user_agent("Mux/0.1")
            .build()
            .map_err(|_| GmailOAuthCommandError::provider())?;
        Ok(Self { client })
    }

    fn exchange(
        &self,
        config: &InstalledClientConfig,
        code: &str,
        redirect_uri: &str,
        verifier: &str,
        now_ms: i64,
    ) -> Result<GmailAuthorizedTokens, GmailOAuthCommandError> {
        let mut form = vec![
            ("client_id", config.client_id.as_str()),
            ("code", code),
            ("code_verifier", verifier),
            ("grant_type", "authorization_code"),
            ("redirect_uri", redirect_uri),
        ];
        if let Some(secret) = config.client_secret.as_deref() {
            form.push(("client_secret", secret));
        }
        let response = self
            .client
            .post(TOKEN_ENDPOINT)
            .form(&form)
            .send()
            .map_err(|_| GmailOAuthCommandError::provider())?;
        let status = response.status();
        let bytes = read_bounded(response)?;
        if !status.is_success() {
            return Err(GmailOAuthCommandError::provider());
        }
        decode_token_response(&bytes, now_ms)
    }

    fn profile_email(&self, access_token: &str) -> Result<String, GmailOAuthCommandError> {
        let response = self
            .client
            .get(PROFILE_ENDPOINT)
            .bearer_auth(access_token)
            .send()
            .map_err(|_| GmailOAuthCommandError::provider())?;
        if !response.status().is_success() {
            return Err(GmailOAuthCommandError::provider());
        }
        let bytes = read_bounded(response)?;
        let wire: ProfileWire =
            serde_json::from_slice(&bytes).map_err(|_| GmailOAuthCommandError::provider())?;
        let email = wire.email_address.trim().to_ascii_lowercase();
        if email.is_empty()
            || email.len() > 320
            || !email.contains('@')
            || email.chars().any(char::is_control)
        {
            return Err(GmailOAuthCommandError::provider());
        }
        Ok(email)
    }
}

fn read_bounded(response: Response) -> Result<Zeroizing<Vec<u8>>, GmailOAuthCommandError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_PROVIDER_RESPONSE_BYTES)
    {
        return Err(GmailOAuthCommandError::provider());
    }
    let mut bytes = Zeroizing::new(Vec::with_capacity(
        (MAX_PROVIDER_RESPONSE_BYTES + 1) as usize,
    ));
    response
        .take(MAX_PROVIDER_RESPONSE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| GmailOAuthCommandError::provider())?;
    if bytes.len() as u64 > MAX_PROVIDER_RESPONSE_BYTES {
        return Err(GmailOAuthCommandError::provider());
    }
    Ok(bytes)
}

#[derive(Deserialize, Zeroize)]
#[zeroize(drop)]
struct TokenResponseWire {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
    token_type: String,
    scope: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProfileWire {
    email_address: String,
}

fn decode_token_response(
    bytes: &[u8],
    now_ms: i64,
) -> Result<GmailAuthorizedTokens, GmailOAuthCommandError> {
    let mut wire = Zeroizing::new(
        serde_json::from_slice::<TokenResponseWire>(bytes)
            .map_err(|_| GmailOAuthCommandError::provider())?,
    );
    let granted_scopes = wire.scope.split_ascii_whitespace().collect::<Vec<_>>();
    if wire.access_token.is_empty()
        || wire.access_token.len() > 16 * 1024
        || wire.refresh_token.is_empty()
        || wire.refresh_token.len() > 16 * 1024
        || !wire.token_type.eq_ignore_ascii_case("bearer")
        || !(1..=86_400).contains(&wire.expires_in)
        || granted_scopes != [GMAIL_MODIFY_SCOPE]
    {
        return Err(GmailOAuthCommandError::provider());
    }
    let expires_at_ms = now_ms
        .checked_add(wire.expires_in.saturating_mul(1_000))
        .ok_or_else(GmailOAuthCommandError::provider)?;
    Ok(GmailAuthorizedTokens {
        refresh_value: Zeroizing::new(std::mem::take(&mut wire.refresh_token)),
        access_value: Zeroizing::new(std::mem::take(&mut wire.access_token)),
        expires_at_ms,
    })
}

pub(crate) fn persist_authorized_mailbox(
    database_path: &Path,
    credentials: &CredentialStore,
    mut authorized: AuthorizedMailbox,
    now_ms: i64,
) -> Result<GmailOAuthResult, GmailOAuthCommandError> {
    if credentials.status() != CredentialStoreStatus::Available || now_ms < 0 {
        return Err(GmailOAuthCommandError::keychain_unavailable());
    }
    authorized.email = authorized.email.trim().to_ascii_lowercase();
    let account_id = account_id_for_email(&authorized.email);
    let configured_ref = configured_record_ref(database_path, &authorized.email)?;
    let record_ref = configured_ref
        .clone()
        .unwrap_or_else(|| format!("gmail/account/{}", &account_id[6..]));
    let encoded = encode_modify_authority(
        &authorized.client_id,
        authorized.client_secret.as_deref().map(String::as_str),
        &authorized.email,
        authorized.tokens,
    )
    .map_err(|_| GmailOAuthCommandError::storage())?;
    credentials
        .put(&record_ref, encoded)
        .map_err(|_| GmailOAuthCommandError::storage())?;

    let database_result = persist_account_marker(
        database_path,
        &account_id,
        &authorized.email,
        &record_ref,
        now_ms,
    );
    let persisted_account_id = match database_result {
        Ok(account_id) => account_id,
        Err(_) => match persisted_account_id(database_path, &authorized.email, &record_ref, now_ms)
        {
            Ok(Some(account_id)) => account_id,
            Ok(None) => {
                if configured_ref.is_none() {
                    let _ = credentials.remove(&record_ref);
                }
                return Err(GmailOAuthCommandError::storage());
            }
            Err(()) => return Err(GmailOAuthCommandError::storage()),
        },
    };
    Ok(GmailOAuthResult {
        state: "connected",
        account_id: persisted_account_id,
        email: authorized.email,
    })
}

fn account_id_for_email(email: &str) -> String {
    let digest = Sha256::digest(email.as_bytes());
    format!("gmail:{}", &hex_lower(&digest)[..32])
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

fn configured_record_ref(
    database_path: &Path,
    email: &str,
) -> Result<Option<String>, GmailOAuthCommandError> {
    let connection =
        Connection::open(database_path).map_err(|_| GmailOAuthCommandError::storage())?;
    let matches = connection
        .query_row(
            "SELECT COUNT(*) FROM provider_accounts
             WHERE provider_kind = 'gmail' AND lower(remote_account_id) = lower(?1)",
            [email],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|_| GmailOAuthCommandError::storage())?;
    if matches > 1 {
        return Err(GmailOAuthCommandError::storage());
    }
    connection
        .query_row(
            "SELECT credential_ref FROM provider_accounts
             WHERE provider_kind = 'gmail' AND lower(remote_account_id) = lower(?1)",
            [email],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| GmailOAuthCommandError::storage())?
        .flatten()
        .map_or(Ok(None), |reference: String| {
            if reference.is_empty()
                || reference.len() > 256
                || reference.chars().any(char::is_control)
            {
                Err(GmailOAuthCommandError::storage())
            } else {
                Ok(Some(reference))
            }
        })
}

fn persist_account_marker(
    database_path: &Path,
    account_id: &str,
    email: &str,
    record_ref: &str,
    now_ms: i64,
) -> Result<String, rusqlite::Error> {
    let mut connection = Connection::open(database_path)?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let matching_accounts = transaction.query_row(
        "SELECT COUNT(*) FROM provider_accounts
         WHERE provider_kind = 'gmail' AND lower(remote_account_id) = lower(?1)",
        [email],
        |row| row.get::<_, i64>(0),
    )?;
    if matching_accounts > 1 {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let existing_account: Option<String> = transaction
        .query_row(
            "SELECT account_id FROM provider_accounts
             WHERE provider_kind = 'gmail' AND lower(remote_account_id) = lower(?1)",
            [email],
            |row| row.get(0),
        )
        .optional()?;
    let selected_id = existing_account.as_deref().unwrap_or(account_id);
    if existing_account.is_none() {
        transaction.execute(
            "INSERT INTO accounts(id, name, email, color, provider)
             VALUES(?1, ?2, ?2, ?3, 'gmail')",
            params![
                selected_id,
                email,
                crate::account_color::account_color_for(selected_id)
            ],
        )?;
        transaction.execute(
            "INSERT INTO provider_accounts(
               account_id, provider_kind, remote_account_id, auth_state,
               credential_ref, sync_state, created_at, updated_at
             ) VALUES(?1, 'gmail', ?2, 'ready', ?3, 'never_synced', ?4, ?4)",
            params![selected_id, email, record_ref, now_ms],
        )?;
    } else {
        transaction.execute(
            "UPDATE accounts SET email = ?2, provider = 'gmail' WHERE id = ?1",
            params![selected_id, email],
        )?;
        transaction.execute(
            "UPDATE operations
             SET state = 'retrying', not_before = ?2, error = NULL
             WHERE id IN (
               SELECT operation_id FROM provider_work_items
               WHERE account_id = ?1 AND state = 'authentication_blocked'
                 AND auth_block_reason IN ('provider_reauthorization', 'credential_locked')
                 AND operation_id IS NOT NULL
             ) AND state = 'retrying'",
            params![selected_id, now_ms],
        )?;
        transaction.execute(
            "UPDATE provider_work_items
             SET state = 'queued', available_at = ?2, last_error_code = NULL,
                 auth_block_reason = NULL
             WHERE account_id = ?1 AND state = 'authentication_blocked'
               AND auth_block_reason IN ('provider_reauthorization', 'credential_locked')",
            params![selected_id, now_ms],
        )?;
        transaction.execute(
            "UPDATE provider_accounts
             SET remote_account_id = ?2, auth_state = 'ready', credential_ref = ?3,
                 auth_block_reason = NULL,
                 sync_state = CASE
                   WHEN EXISTS(
                     SELECT 1 FROM provider_sync_cursors cursor
                     WHERE cursor.account_id = provider_accounts.account_id
                   ) THEN 'scheduled'
                   ELSE 'never_synced'
                 END,
                 last_error_code = NULL, updated_at = ?4
             WHERE account_id = ?1 AND provider_kind = 'gmail'",
            params![selected_id, email, record_ref, now_ms],
        )?;
    }
    transaction.commit()?;
    Ok(selected_id.to_owned())
}

fn persisted_account_id(
    database_path: &Path,
    email: &str,
    record_ref: &str,
    updated_at: i64,
) -> Result<Option<String>, ()> {
    let connection = Connection::open(database_path).map_err(|_| ())?;
    connection
        .query_row(
            "SELECT provider.account_id
             FROM provider_accounts provider
             JOIN accounts account ON account.id = provider.account_id
             WHERE provider.provider_kind = 'gmail'
               AND lower(provider.remote_account_id) = lower(?1)
               AND provider.auth_state = 'ready'
               AND provider.credential_ref = ?2
               AND provider.auth_block_reason IS NULL
               AND provider.updated_at = ?3
               AND account.provider = 'gmail'
               AND lower(account.email) = lower(?1)",
            params![email, record_ref, updated_at],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MuxStore;
    use tempfile::tempdir;

    fn synthetic_authorized(email: &str) -> AuthorizedMailbox {
        AuthorizedMailbox {
            email: email.into(),
            client_id: "synthetic.apps.googleusercontent.com".into(),
            client_secret: Some(Zeroizing::new("synthetic-client-value".into())),
            tokens: GmailAuthorizedTokens {
                refresh_value: Zeroizing::new("synthetic-refresh-value".into()),
                access_value: Zeroizing::new("synthetic-access-value".into()),
                expires_at_ms: 3_600_000,
            },
        }
    }

    #[test]
    fn gmail_oauth_authorization_url_is_exact_scope_state_and_pkce_without_client_value() {
        let pkce = PkceMaterial {
            verifier: Zeroizing::new("a".repeat(43)),
            challenge: "challenge".into(),
            state: Zeroizing::new("state".into()),
        };
        let redirect_uri = loopback_redirect_uri("127.0.0.1:43123");
        assert_eq!(redirect_uri, "http://127.0.0.1:43123");
        let url = authorization_url("synthetic.apps.googleusercontent.com", &redirect_uri, &pkce)
            .unwrap();
        let url = reqwest::Url::parse(&url).unwrap();
        let values = url
            .query_pairs()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(url.as_str().split('?').next(), Some(AUTHORIZATION_ENDPOINT));
        assert_eq!(
            values.get("scope").map(|v| v.as_ref()),
            Some(GMAIL_MODIFY_SCOPE)
        );
        assert_eq!(
            values.get("redirect_uri").map(|v| v.as_ref()),
            Some("http://127.0.0.1:43123")
        );
        assert_eq!(values.get("state").map(|v| v.as_ref()), Some("state"));
        assert_eq!(
            values.get("code_challenge").map(|v| v.as_ref()),
            Some("challenge")
        );
        assert_eq!(
            values.get("code_challenge_method").map(|v| v.as_ref()),
            Some("S256")
        );
        assert!(!values.contains_key("client_secret"));
        assert!(!values.contains_key("gmail.send"));
    }

    #[test]
    fn gmail_oauth_callback_rejects_wrong_state_path_duplicates_and_accepts_one_code() {
        assert!(matches!(
            parse_callback_target(
                "/?state=expected&code=one",
                "127.0.0.1:1234",
                "expected"
            ),
            Ok(Callback::Code(code)) if code.as_str() == "one"
        ));
        for target in [
            "/wrong?state=expected&code=one",
            "/?state=wrong&code=one",
            "/?state=expected&state=expected&code=one",
            "/?state=expected&code=one&code=two",
            "/?state=expected&error=access_denied&code=one",
        ] {
            assert!(parse_callback_target(target, "127.0.0.1:1234", "expected").is_err());
        }
        assert!(matches!(
            parse_callback_target(
                "/?state=expected&error=access_denied",
                "127.0.0.1:1234",
                "expected"
            ),
            Ok(Callback::Denied)
        ));

        let valid = b"GET /?state=expected&code=one HTTP/1.1\r\nHost: 127.0.0.1:1234\r\n\r\n";
        assert!(matches!(
            parse_callback_request(valid, "127.0.0.1:1234", "expected"),
            Ok(Callback::Code(code)) if code.as_str() == "one"
        ));
        for invalid in [
            b"POST /?state=expected&code=one HTTP/1.1\r\nHost: 127.0.0.1:1234\r\n\r\n".as_slice(),
            b"GET /?state=expected&code=one extra HTTP/1.1\r\nHost: 127.0.0.1:1234\r\n\r\n".as_slice(),
            b"GET /?state=expected&code=one HTTP/1.1\r\nHost: 127.0.0.1:1234\r\nHost: attacker.test\r\n\r\n".as_slice(),
        ] {
            assert!(parse_callback_request(invalid, "127.0.0.1:1234", "expected").is_err());
        }
    }

    #[test]
    fn gmail_oauth_callback_reads_fragmented_bounded_headers() {
        struct FragmentedReader {
            bytes: std::io::Cursor<Vec<u8>>,
            maximum_read: usize,
        }

        impl Read for FragmentedReader {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                let length = buffer.len().min(self.maximum_read);
                self.bytes.read(&mut buffer[..length])
            }
        }

        let request =
            b"GET /?state=expected&code=fragmented HTTP/1.1\r\nHost: 127.0.0.1:43123\r\n\r\n";
        let mut reader = FragmentedReader {
            bytes: std::io::Cursor::new(request.to_vec()),
            maximum_read: 7,
        };
        let collected = read_callback_request(&mut reader, |_| Ok(())).unwrap();
        assert_eq!(collected.as_slice(), request);
    }

    #[test]
    fn gmail_oauth_callback_read_budget_honors_deadline_and_cancellation() {
        let cancellation = AtomicBool::new(false);
        assert!(
            callback_read_budget(&cancellation, Instant::now() + Duration::from_secs(1)).is_ok()
        );
        assert!(
            callback_read_budget(&cancellation, Instant::now() - Duration::from_millis(1)).is_err()
        );
        cancellation.store(true, Ordering::SeqCst);
        assert!(
            callback_read_budget(&cancellation, Instant::now() + Duration::from_secs(1)).is_err()
        );
    }

    #[test]
    fn gmail_oauth_configuration_accepts_only_installed_loopback_registration() {
        let directory = tempdir().unwrap();
        let registration = directory.path().join("registration.json");
        fs::write(
            &registration,
            serde_json::json!({
                "installed": {
                    "client_id": "synthetic.apps.googleusercontent.com",
                    "client_secret": "synthetic-client-value",
                    "redirect_uris": ["http://localhost"]
                }
            })
            .to_string(),
        )
        .unwrap();
        let loaded = InstalledClientConfig::load(&registration).unwrap();
        assert_eq!(loaded.client_id, "synthetic.apps.googleusercontent.com");

        fs::write(
            &registration,
            serde_json::json!({
                "web": {
                    "client_id": "synthetic.apps.googleusercontent.com",
                    "redirect_uris": ["https://example.test/callback"]
                }
            })
            .to_string(),
        )
        .unwrap();
        assert!(InstalledClientConfig::load(&registration).is_err());
    }

    #[test]
    fn gmail_oauth_explicit_process_registration_check_is_bounded() {
        if std::env::var("MUX_RUN_GOOGLE_REGISTRATION_CHECK").as_deref() != Ok("1") {
            return;
        }
        let configured = InstalledClientConfig::load_from_environment()
            .expect("explicitly configured installed registration");
        assert!(valid_client_id(&configured.client_id));
    }

    #[test]
    fn gmail_oauth_token_response_requires_modify_only_and_refresh_authority() {
        let valid = serde_json::json!({
            "access_token": "synthetic-access-value",
            "refresh_token": "synthetic-refresh-value",
            "expires_in": 3600,
            "token_type": "Bearer",
            "scope": GMAIL_MODIFY_SCOPE
        });
        let tokens = decode_token_response(valid.to_string().as_bytes(), 1000).unwrap();
        assert_eq!(tokens.expires_at_ms, 3_601_000);

        let mut extra_scope = valid.clone();
        extra_scope["scope"] = serde_json::json!(format!("{GMAIL_MODIFY_SCOPE} openid"));
        assert!(decode_token_response(extra_scope.to_string().as_bytes(), 1000).is_err());
        let mut missing_refresh = valid;
        missing_refresh["refresh_token"] = serde_json::json!("");
        assert!(decode_token_response(missing_refresh.to_string().as_bytes(), 1000).is_err());
    }

    #[test]
    fn gmail_oauth_coordinator_is_single_flight_and_cancellable() {
        let coordinator = Arc::new(GmailOAuthCoordinator::new());
        let reservation = coordinator.reserve().unwrap();
        assert!(matches!(
            coordinator.reserve(),
            Err(error) if error == GmailOAuthCommandError::busy()
        ));
        assert_eq!(
            coordinator.cancel().unwrap(),
            GmailOAuthCancellation {
                state: "cancel_requested"
            }
        );
        assert!(reservation.cancellation.load(Ordering::SeqCst));
        drop(reservation);
        assert_eq!(
            coordinator.cancel().unwrap(),
            GmailOAuthCancellation { state: "idle" }
        );
        assert!(coordinator.reserve().is_ok());
    }

    #[test]
    fn gmail_oauth_persists_only_an_opaque_sqlite_marker_and_rebinds_same_mailbox() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("oauth.db");
        drop(MuxStore::open(&path, false).unwrap());
        // A throwaway keychain so the test never touches the login keychain.
        let credentials = CredentialStore::temporary(&directory.path().join("oauth.keychain"));

        let first = persist_authorized_mailbox(
            &path,
            &credentials,
            synthetic_authorized("reader@example.test"),
            1000,
        )
        .unwrap();
        let second = persist_authorized_mailbox(
            &path,
            &credentials,
            synthetic_authorized("READER@example.test"),
            2000,
        )
        .unwrap();
        assert_eq!(first.account_id, second.account_id);

        let connection = Connection::open(&path).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM provider_accounts WHERE provider_kind = 'gmail'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        let sqlite_dump = fs::read(&path).unwrap();
        for secret in [
            b"synthetic-client-value".as_slice(),
            b"synthetic-refresh-value".as_slice(),
            b"synthetic-access-value".as_slice(),
        ] {
            assert!(!sqlite_dump
                .windows(secret.len())
                .any(|window| window == secret));
        }
        let record_ref: String = connection
            .query_row("SELECT credential_ref FROM provider_accounts", [], |row| {
                row.get(0)
            })
            .unwrap();
        let stored = credentials.get(&record_ref).unwrap().unwrap();
        let record: serde_json::Value = serde_json::from_slice(&stored).unwrap();
        assert_eq!(record["remoteSubject"], "reader@example.test");
        assert_eq!(record["scopes"], serde_json::json!([GMAIL_MODIFY_SCOPE]));
    }
}
