pub mod content;
mod gmail;
mod gmail_access;
#[path = "google_authorization.rs"]
mod gmail_oauth;
mod keychain;
// IMAP remains an unshipped work-in-progress. Keep it out of the product build
// until its adapter compiles and passes the provider contract gate.
mod internet_message;
mod ipc_boundary;
mod mime_ingest;
mod navigation;
mod outgoing;
pub mod provider;
mod provider_conformance;
mod provider_ingest;
mod provider_schema;
mod remote_content;
mod search;
pub mod store;
mod worker;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::OptionalExtension;
use serde::Serialize;
use store::{
    AttachmentContent, DraftSummary, ListOperationsInput, MailboxBootstrap, MessagePage,
    MessagePageInput, MuxStore, OperationActivitySummary, OperationSummary, SaveDraftInput,
    SearchInput, SearchPage, ThreadLookupInput, ThreadPage, ThreadPageInput, ThreadSummary,
};
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_opener::OpenerExt;
use worker::{
    ClaimedWork, DurableWorker, WorkerAdapter, WorkerConfig, WorkerController, WorkerCycleResult,
    WorkerExecutionContext, WorkerOutcome, WorkerProjection,
};

const MAILBOX_CHANGED_EVENT: &str = "mux://mailbox-changed";
/// How often the refresh timer asks which accounts are due. Individual accounts
/// keep their own cadence; this only bounds how precisely it is honoured.
const REFRESH_TICK_SECONDS: u64 = 5;
/// Multiplier ceiling for the refresh tick after repeated failures.
const REFRESH_MAX_BACKOFF: u32 = 24;

#[derive(Clone, Copy, Serialize)]
struct MailboxChangedPayload {
    source: &'static str,
}

struct AppState {
    database_path: PathBuf,
    store: Mutex<MuxStore>,
    credentials: keychain::CredentialStore,
    gmail_oauth: Arc<gmail_oauth::GmailOAuthCoordinator>,
    worker: Mutex<Option<WorkerController>>,
    /// Set on exit so the refresh timer stops instead of outliving the window.
    refresh_stop: Arc<AtomicBool>,
}

struct NativeWorkerAdapter<G> {
    gmail: G,
    database_path: PathBuf,
}

impl<G> WorkerAdapter for NativeWorkerAdapter<G>
where
    G: WorkerAdapter + Sync,
{
    fn execute(&self, work: &ClaimedWork, context: &WorkerExecutionContext) -> WorkerOutcome {
        if gmail::is_gmail_owned_work(work) {
            return self.gmail.execute(work, context);
        }
        let provider_kind =
            rusqlite::Connection::open(&self.database_path).and_then(|connection| {
                connection
                    .query_row(
                        "SELECT provider_kind FROM provider_accounts WHERE account_id = ?1",
                        [work.account_id.as_str()],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
            });
        match provider_kind {
            Ok(Some(provider))
                if provider == "gmail" && work.kind == worker::WorkKind::Mutation =>
            {
                self.gmail.execute(work, context)
            }
            Ok(Some(provider)) => WorkerOutcome::PermanentFailure {
                code: format!("{provider}_work_not_implemented"),
            },
            Ok(None) => WorkerOutcome::Succeeded {
                projection: WorkerProjection::LocalOperation,
            },
            Err(_) => WorkerOutcome::RetryableFailure {
                code: "provider_routing_unavailable".into(),
            },
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn wake_worker_state(state: &AppState) {
    if let Ok(worker) = state.worker.lock() {
        if let Some(worker) = worker.as_ref() {
            let _ = worker.wake();
        }
    }
}

fn wake_worker(state: &State<'_, AppState>) {
    wake_worker_state(state);
}

#[tauri::command]
async fn gmail_oauth_begin(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<gmail_oauth::GmailOAuthResult, gmail_oauth::GmailOAuthCommandError> {
    let reservation = state.gmail_oauth.reserve()?;
    let cancellation = Arc::clone(&reservation.cancellation);
    let database_path = state.database_path.clone();
    let credentials = state.credentials.clone();
    let browser = app.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let authorized = gmail_oauth::authorize(
            &cancellation,
            |url| browser.opener().open_url(url, None::<&str>).map_err(|_| ()),
            now_ms(),
        )?;
        if cancellation.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(gmail_oauth::GmailOAuthCommandError {
                code: "oauth_cancelled",
                message: "Google authorization was cancelled.",
            });
        }
        gmail_oauth::persist_authorized_mailbox(&database_path, &credentials, authorized, now_ms())
    })
    .await
    .map_err(|_| gmail_oauth::GmailOAuthCommandError {
        code: "oauth_runtime_error",
        message: "Google authorization could not be completed.",
    })??;
    drop(reservation);

    let scheduled_at = now_ms();
    let _ = gmail::schedule_initial_syncs(&state.database_path, scheduled_at);
    let _ = gmail::schedule_resumable_syncs(&state.database_path, scheduled_at);
    wake_worker(&state);
    let _ = app.emit(
        MAILBOX_CHANGED_EVENT,
        MailboxChangedPayload {
            source: "gmail-onboarding",
        },
    );
    Ok(result)
}

#[tauri::command]
fn gmail_oauth_cancel(
    state: State<'_, AppState>,
) -> Result<gmail_oauth::GmailOAuthCancellation, gmail_oauth::GmailOAuthCommandError> {
    state.gmail_oauth.cancel()
}

/// Rewinds provider sync bookmarks and wakes the worker. This re-downloads and
/// re-projects every message onto the rows that already exist; it removes nothing.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AccountRefreshInput {
    account_id: String,
    refresh_seconds: i64,
}

#[tauri::command]
fn set_account_refresh(
    state: State<'_, AppState>,
    input: AccountRefreshInput,
) -> Result<(), String> {
    {
        let mut store = state
            .store
            .lock()
            .map_err(|_| "Native mailbox state is unavailable".to_string())?;
        store
            .set_account_refresh_seconds(&input.account_id, input.refresh_seconds)
            .map_err(|error| error.to_string())?;
    }
    // A shorter cadence should take effect now, not after the old one elapses.
    wake_worker(&state);
    Ok(())
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AccountIdInput {
    account_id: String,
}

/// Asks one account for new mail immediately rather than waiting for its cadence.
#[tauri::command]
fn sync_account_now(state: State<'_, AppState>, input: AccountIdInput) -> Result<usize, String> {
    let scheduled = gmail::sync_account_now(&state.database_path, &input.account_id, now_ms())
        .map_err(|error| error.to_string())?;
    wake_worker(&state);
    Ok(scheduled)
}

#[tauri::command]
fn resync_all_mail(state: State<'_, AppState>) -> Result<store::FullResyncRequest, String> {
    let requested = {
        let mut store = state
            .store
            .lock()
            .map_err(|_| "Native mailbox state is unavailable".to_string())?;
        store
            .request_full_resync()
            .map_err(|error| error.to_string())?
    };
    wake_worker(&state);
    Ok(requested)
}

fn worker_changed_mailbox(result: &WorkerCycleResult) -> bool {
    result.succeeded > 0 || result.failed > 0 || result.cancelled > 0 || result.outcome_unknown > 0
}

#[tauri::command]
fn mailbox_bootstrap(state: State<'_, AppState>) -> Result<MailboxBootstrap, String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "Native mailbox state is unavailable".to_string())?;
    store.bootstrap().map_err(|error| error.to_string())
}

#[tauri::command]
fn list_threads(state: State<'_, AppState>, input: ThreadPageInput) -> Result<ThreadPage, String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "Native mailbox state is unavailable".to_string())?;
    store.list_threads(input).map_err(|error| error.to_string())
}

#[tauri::command]
fn get_thread_summary(
    state: State<'_, AppState>,
    input: ThreadLookupInput,
) -> Result<Option<ThreadSummary>, String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "Native mailbox state is unavailable".to_string())?;
    store
        .get_thread_summary(input)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn get_thread_messages(
    state: State<'_, AppState>,
    input: MessagePageInput,
) -> Result<MessagePage, String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "Native mailbox state is unavailable".to_string())?;
    store
        .get_thread_messages(input)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn search_threads(state: State<'_, AppState>, input: SearchInput) -> Result<SearchPage, String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "Native mailbox state is unavailable".to_string())?;
    store
        .search_threads(input)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn read_attachment(
    state: State<'_, AppState>,
    attachment_id: String,
) -> Result<AttachmentContent, String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "Native mailbox state is unavailable".to_string())?;
    store
        .read_attachment(&attachment_id)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn get_draft(state: State<'_, AppState>, draft_id: String) -> Result<DraftSummary, String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "Native mailbox state is unavailable".to_string())?;
    store
        .get_draft(&draft_id)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Draft was not found".to_string())
}

#[tauri::command]
fn open_message_link(
    app: AppHandle,
    destination: String,
) -> Result<navigation::ExternalLinkDestination, String> {
    navigation::open_external_destination(&destination, |normalized| {
        app.opener()
            .open_url(normalized, None::<&str>)
            .map_err(|_| ())
    })
}

#[tauri::command]
async fn load_remote_image(
    state: State<'_, AppState>,
    input: remote_content::LoadRemoteImageInput,
) -> Result<remote_content::LoadedRemoteImage, String> {
    if input.message_id <= 0 || input.resource_id <= 0 {
        return Err("Remote image identity is invalid".into());
    }
    let image = {
        let store = state
            .store
            .lock()
            .map_err(|_| "Native mailbox state is unavailable".to_string())?;
        store
            .stored_remote_image(input.message_id, input.resource_id)
            .map_err(|error| error.to_string())?
    };
    tauri::async_runtime::spawn_blocking(move || remote_content::fetch_remote_image(image))
        .await
        .map_err(|_| "Remote image could not be loaded".to_string())?
}

#[tauri::command]
fn allow_remote_content_sender(
    state: State<'_, AppState>,
    input: remote_content::MessageRemoteContentInput,
) -> Result<(), String> {
    let mut store = state
        .store
        .lock()
        .map_err(|_| "Native mailbox state is unavailable".to_string())?;
    store
        .allow_remote_content_sender(input.message_id)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn allow_remote_content_domain(
    state: State<'_, AppState>,
    input: remote_content::DomainRemoteContentInput,
) -> Result<(), String> {
    let mut store = state
        .store
        .lock()
        .map_err(|_| "Native mailbox state is unavailable".to_string())?;
    store
        .allow_remote_content_domain(input.message_id, &input.domain)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn save_draft(state: State<'_, AppState>, input: SaveDraftInput) -> Result<DraftSummary, String> {
    let mut store = state
        .store
        .lock()
        .map_err(|_| "Native mailbox state is unavailable".to_string())?;
    store.save_draft(input).map_err(|error| error.to_string())
}

#[tauri::command]
fn delete_draft(state: State<'_, AppState>, draft_id: String) -> Result<(), String> {
    let mut store = state
        .store
        .lock()
        .map_err(|_| "Native mailbox state is unavailable".to_string())?;
    store
        .delete_draft(&draft_id)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn queue_send(
    state: State<'_, AppState>,
    draft_id: String,
    undo_window_ms: Option<i64>,
) -> Result<OperationSummary, String> {
    let operation = {
        let mut store = state
            .store
            .lock()
            .map_err(|_| "Native mailbox state is unavailable".to_string())?;
        store
            .queue_send(&draft_id, undo_window_ms.unwrap_or(5_000))
            .map_err(|error| error.to_string())?
    };
    wake_worker(&state);
    Ok(operation)
}

#[tauri::command]
fn apply_thread_action(
    state: State<'_, AppState>,
    thread_id: i64,
    action: String,
) -> Result<OperationSummary, String> {
    let operation = {
        let mut store = state
            .store
            .lock()
            .map_err(|_| "Native mailbox state is unavailable".to_string())?;
        store
            .apply_thread_action(thread_id, &action)
            .map_err(|error| error.to_string())?
    };
    wake_worker(&state);
    Ok(operation)
}

#[tauri::command]
fn undo_operation(
    state: State<'_, AppState>,
    operation_id: String,
) -> Result<OperationSummary, String> {
    let operation = {
        let mut store = state
            .store
            .lock()
            .map_err(|_| "Native mailbox state is unavailable".to_string())?;
        store
            .undo_operation(&operation_id)
            .map_err(|error| error.to_string())?
    };
    wake_worker(&state);
    Ok(operation)
}

#[tauri::command]
fn snooze_thread(
    state: State<'_, AppState>,
    thread_id: i64,
    wake_at: i64,
) -> Result<OperationSummary, String> {
    let mut store = state
        .store
        .lock()
        .map_err(|_| "Native mailbox state is unavailable".to_string())?;
    store
        .snooze_thread(thread_id, wake_at)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn rsvp_thread(
    state: State<'_, AppState>,
    thread_id: i64,
    response: String,
) -> Result<OperationSummary, String> {
    let mut store = state
        .store
        .lock()
        .map_err(|_| "Native mailbox state is unavailable".to_string())?;
    store
        .rsvp_thread(thread_id, &response)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn list_operations(
    state: State<'_, AppState>,
    input: ListOperationsInput,
) -> Result<Vec<OperationActivitySummary>, String> {
    let store = state
        .store
        .lock()
        .map_err(|_| "Native mailbox state is unavailable".to_string())?;
    store
        .list_operations(input)
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn resolve_outcome_unknown_send(
    state: State<'_, AppState>,
    operation_id: String,
) -> Result<OperationSummary, String> {
    let mut store = state
        .store
        .lock()
        .map_err(|_| "Native mailbox state is unavailable".to_string())?;
    store
        .resolve_outcome_unknown_send(&operation_id)
        .map_err(|error| error.to_string())
}

fn database_path(app: &tauri::App) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(path) = std::env::var_os("MUX_NATIVE_DB") {
        return Ok(PathBuf::from(path));
    }

    let directory = app.path().app_data_dir()?;
    std::fs::create_dir_all(&directory)?;
    Ok(directory.join("mux.db"))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(
            tauri::plugin::Builder::<tauri::Wry>::new("mux-navigation-policy")
                .on_navigation(|_webview, url| navigation::allows_webview_navigation(url))
                .build(),
        )
        .plugin(
            tauri_plugin_opener::Builder::new()
                .open_js_links_on_click(false)
                .build(),
        )
        .setup(|app| {
            let path = database_path(app)?;
            let store = MuxStore::open(&path, true)?;
            let worker = DurableWorker::new(&path, WorkerConfig::default())?;
            let credentials = keychain::CredentialStore::for_database(&path)?;
            gmail::schedule_initial_syncs(&path, now_ms())?;
            // Accounts whose keychain record is missing or invalid are marked for
            // reauthorization once at launch rather than waiting for an unlock.
            let _ = gmail_access::reconcile_credential_records(&path, &credentials, now_ms());
            let gmail_adapter = gmail::GmailAdapter::new(
                gmail_access::GmailKeychainAccess::new(
                    &path,
                    credentials.clone(),
                    gmail_access::GoogleGrantRefresher::new()?,
                ),
                gmail::GoogleGmailApi::new()?,
                now_ms as fn() -> i64,
            )
            .with_database_path(&path);
            let app_handle = app.handle().clone();
            let controller = WorkerController::start(
                worker,
                format!("mux-native-{}", std::process::id()),
                NativeWorkerAdapter {
                    gmail: gmail_adapter,
                    database_path: path.clone(),
                },
                provider_conformance::apply_worker_projection,
                move |result| {
                    if std::env::var_os("MUX_LOG_WORKER_ERRORS").is_some() {
                        for error in &result.errors {
                            eprintln!("Mux worker: {error}");
                        }
                    }
                    if worker_changed_mailbox(&result) {
                        let _ = app_handle.emit(
                            MAILBOX_CHANGED_EVENT,
                            MailboxChangedPayload {
                                source: "operation-worker",
                            },
                        );
                    }
                },
            )?;
            // Refresh timer: each account is asked for new mail on its own cadence.
            // The scheduler decides what is actually due, so this tick stays cheap.
            let refresh_path = path.clone();
            let refresh_handle = app.handle().clone();
            let refresh_stop = Arc::new(AtomicBool::new(false));
            let refresh_stop_thread = Arc::clone(&refresh_stop);
            std::thread::spawn(move || {
                let mut backoff = 1_u32;
                while !refresh_stop_thread.load(Ordering::SeqCst) {
                    // Sleep in slices so quitting does not wait out a whole tick.
                    let target = REFRESH_TICK_SECONDS * u64::from(backoff);
                    for _ in 0..target {
                        if refresh_stop_thread.load(Ordering::SeqCst) {
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                    match gmail::schedule_due_syncs(&refresh_path, now_ms()) {
                        Ok(scheduled) => {
                            backoff = 1;
                            if scheduled > 0 {
                                wake_worker_state(&refresh_handle.state::<AppState>());
                            }
                        }
                        Err(error) => {
                            // A failing database should not be retried every few
                            // seconds forever; back off to a couple of minutes.
                            backoff = (backoff * 2).min(REFRESH_MAX_BACKOFF);
                            if std::env::var_os("MUX_LOG_WORKER_ERRORS").is_some() {
                                eprintln!("Mux refresh: {error}");
                            }
                        }
                    }
                }
            });
            app.manage(AppState {
                database_path: path,
                store: Mutex::new(store),
                credentials,
                gmail_oauth: Arc::new(gmail_oauth::GmailOAuthCoordinator::new()),
                worker: Mutex::new(Some(controller)),
                refresh_stop,
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            mailbox_bootstrap,
            list_threads,
            get_thread_summary,
            get_thread_messages,
            search_threads,
            read_attachment,
            get_draft,
            open_message_link,
            load_remote_image,
            allow_remote_content_sender,
            allow_remote_content_domain,
            save_draft,
            delete_draft,
            queue_send,
            apply_thread_action,
            undo_operation,
            snooze_thread,
            rsvp_thread,
            list_operations,
            resolve_outcome_unknown_send,
            gmail_oauth_begin,
            gmail_oauth_cancel,
            resync_all_mail,
            set_account_refresh,
            sync_account_now
        ])
        .build(tauri::generate_context!())
        .expect("error while building Mux");
    app.run(|app_handle, event| {
        if matches!(event, tauri::RunEvent::Exit) {
            app_handle
                .state::<AppState>()
                .refresh_stop
                .store(true, Ordering::SeqCst);
            let _ = app_handle.state::<AppState>().gmail_oauth.cancel();
            if let Ok(mut worker) = app_handle.state::<AppState>().worker.lock() {
                if let Some(controller) = worker.take() {
                    let _ = controller.stop();
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct RecordingGmailAdapter {
        calls: AtomicUsize,
    }

    impl WorkerAdapter for RecordingGmailAdapter {
        fn execute(&self, _work: &ClaimedWork, _context: &WorkerExecutionContext) -> WorkerOutcome {
            self.calls.fetch_add(1, Ordering::SeqCst);
            WorkerOutcome::Succeeded {
                projection: WorkerProjection::LocalOperation,
            }
        }
    }

    fn routed_claim(account_id: &str, kind: worker::WorkKind) -> ClaimedWork {
        ClaimedWork {
            id: format!("work-{account_id}"),
            account_id: account_id.into(),
            operation_id: Some(format!("op-{account_id}")),
            kind,
            scope: crate::gmail::GMAIL_ACCOUNT_SCOPE.into(),
            ordering_key: format!("op-{account_id}"),
            payload_json: "{}".into(),
            payload_fingerprint_hex: "00".repeat(32),
            attempt: 1,
            lease_token: "lease".into(),
            lease_expires_at: 1_000,
        }
    }

    #[test]
    fn native_worker_routes_gmail_mutations_and_never_fake_confirms_provider_send() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("provider-routing.db");
        drop(MuxStore::open(&path, false).unwrap());
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "INSERT INTO accounts(id, name, email, color, provider) VALUES
                   ('gmail-account', 'Gmail', 'g@example.test', '#000', 'gmail'),
                   ('fake-account', 'Demo', 'f@example.test', '#000', 'fake');
                 INSERT INTO provider_accounts(
                   account_id, provider_kind, remote_account_id, auth_state,
                   credential_ref, sync_state, created_at, updated_at
                 ) VALUES(
                   'gmail-account', 'gmail', 'remote-gmail', 'ready',
                   'vault/gmail', 'idle', 0, 0
                 );",
            )
            .unwrap();
        let adapter = NativeWorkerAdapter {
            gmail: RecordingGmailAdapter {
                calls: AtomicUsize::new(0),
            },
            database_path: path,
        };
        assert!(matches!(
            adapter.execute(
                &routed_claim("gmail-account", worker::WorkKind::Mutation),
                &WorkerExecutionContext::new()
            ),
            WorkerOutcome::Succeeded { .. }
        ));
        assert_eq!(adapter.gmail.calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            adapter.execute(
                &routed_claim("gmail-account", worker::WorkKind::Send),
                &WorkerExecutionContext::new()
            ),
            WorkerOutcome::PermanentFailure { .. }
        ));
        assert_eq!(adapter.gmail.calls.load(Ordering::SeqCst), 1);
        assert!(matches!(
            adapter.execute(
                &routed_claim("fake-account", worker::WorkKind::Mutation),
                &WorkerExecutionContext::new()
            ),
            WorkerOutcome::Succeeded {
                projection: WorkerProjection::LocalOperation
            }
        ));
    }

    #[test]
    fn mailbox_event_is_minimal_and_only_follows_visible_worker_commits() {
        let payload = serde_json::to_value(MailboxChangedPayload {
            source: "operation-worker",
        })
        .unwrap();
        assert_eq!(payload, serde_json::json!({ "source": "operation-worker" }));

        let retry = WorkerCycleResult {
            claimed: 1,
            retry_scheduled: 1,
            errors: vec!["internal-only".into()],
            ..WorkerCycleResult::default()
        };
        assert!(!worker_changed_mailbox(&retry));
        let committed = WorkerCycleResult {
            succeeded: 1,
            ..WorkerCycleResult::default()
        };
        assert!(worker_changed_mailbox(&committed));
    }

    #[test]
    fn tauri_exposes_no_credential_access_of_any_kind() {
        let source = include_str!("lib.rs");
        let handler = source
            .split(".invoke_handler(tauri::generate_handler![")
            .nth(1)
            .and_then(|tail| tail.split("])").next())
            .expect("invoke handler source");
        // Credentials live in the keychain and are read only inside Rust. No
        // command may read, write, or unlock them, and none may ask for a password.
        for forbidden in [
            "vault",
            "credential_value",
            "passphrase",
            "password",
            "keychain_get",
            "keychain_put",
            "secret",
            "token",
        ] {
            assert!(!handler.contains(forbidden), "exposed {forbidden}");
        }
    }

    #[test]
    fn tauri_handler_is_an_exact_typed_mailbox_allowlist() {
        let source = include_str!("lib.rs");
        let handler = source
            .split(".invoke_handler(tauri::generate_handler![")
            .nth(1)
            .and_then(|tail| tail.split("])").next())
            .expect("invoke handler source");
        let commands = handler
            .split(',')
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(
            commands,
            vec![
                "mailbox_bootstrap",
                "list_threads",
                "get_thread_summary",
                "get_thread_messages",
                "search_threads",
                "read_attachment",
                "get_draft",
                "open_message_link",
                "load_remote_image",
                "allow_remote_content_sender",
                "allow_remote_content_domain",
                "save_draft",
                "delete_draft",
                "queue_send",
                "apply_thread_action",
                "undo_operation",
                "snooze_thread",
                "rsvp_thread",
                "list_operations",
                "resolve_outcome_unknown_send",
                "gmail_oauth_begin",
                "gmail_oauth_cancel",
                "resync_all_mail",
                "set_account_refresh",
                "sync_account_now",
            ]
        );
    }

    #[test]
    fn message_links_have_one_typed_command_and_no_generic_frontend_opener_authority() {
        let source = include_str!("lib.rs");
        assert!(source.contains("open_message_link"));
        assert!(source.contains("open_js_links_on_click(false)"));
        assert!(source.contains("allows_webview_navigation(url)"));

        let capability = include_str!("../capabilities/default.json");
        assert!(!capability.contains("opener:"));
        assert!(!capability.contains("shell:"));
    }
}
