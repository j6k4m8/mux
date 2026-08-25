# Tauri Command API

Mux has no product HTTP API. Svelte calls the allowlisted Rust commands registered in `native/src-tauri/src/lib.rs`; changes are announced through the typed `mux://mailbox-changed` event.

## Queries

| Command | Purpose |
| --- | --- |
| `mailbox_bootstrap` | Accounts, view counts, schema version, and header-only draft summaries |
| `list_threads` | Bounded keyset page for Inbox/Starred/Sent/Archive/Trash/All/Snoozed |
| `get_thread_summary` | Recover one selected row under the same view/search constraints |
| `get_thread_messages` | Adaptive bounded message page plus attachment metadata and optional invitation |
| `search_threads` | Bounded parsed search over the effective local projection |
| `read_attachment` | Read one validated local demo attachment through a typed base64 DTO |
| `get_draft` | Load one full local draft explicitly; draft bodies are never in bootstrap |
| `open_message_link` | Revalidate and open one normalized absolute HTTP(S) destination through the app-owned native opener |
| `list_operations` | Bounded payload-free operation activity summaries |
| `vault_status` | `absent`, `locked`, `unlocked`, or `unavailable`; never secret metadata |

## Mailbox and draft commands

| Command | Purpose |
| --- | --- |
| `save_draft` | Create or update a bounded local draft with revision protection |
| `delete_draft` | Delete an unlocked local draft |
| `queue_send` | Lock a draft and append the dedicated delayed-send operation/work item |
| `apply_thread_action` | Archive/restore, trash/untrash, read/unread, or star/unstar |
| `undo_operation` | Append a compensating operation after stale-state validation |
| `snooze_thread` | Set bounded Mux-owned wake metadata |
| `rsvp_thread` | Store `accepted`, `tentative`, `declined`, or `needsAction` locally |
| `resolve_outcome_unknown_send` | Validate the exact immutable send snapshot and unlock it without resending |

All mutating commands validate identifiers, input sizes, enum vocabularies, current effective state, and allowed transitions in Rust. Effective views expose the local result immediately; workers process durable work later.

## Vault lifecycle commands

| Command | Purpose |
| --- | --- |
| `vault_create` | Create the bounded authenticated vault envelope |
| `vault_unlock` | Derive the key and unlock Rust-only record access |
| `vault_lock` | Prevent new secret borrows and zeroize retained vault state |
| `vault_change_passphrase` | Re-encrypt with a fresh salt/nonce under the stable file lock |
| `vault_reset` | Explicit destructive recovery after confirmation |
| `gmail_oauth_begin` | Open one bounded installed-desktop Google authorization and return only connected account identity |
| `gmail_oauth_cancel` | Request cancellation of the active Google authorization attempt |

There is intentionally no generic credential `get`, `put`, `remove`, list, export, or provider-token command over IPC.

## Bounds and casing

Tauri serializes Rust snake_case fields to camelCase for the Svelte types. Exact DTOs live in `native/src/types.ts` and `native/src-tauri/src/store.rs`; tests cover their command payload shapes. Opaque cursors are versioned, integrity-checked, scope-bound, restart-stable, and at most 256 bytes. Actual serialized ceilings are 512 KiB for bootstrap, 3 MiB for thread/search pages, 16 MiB for message detail, 28 MiB for attachment content, and 256 KiB for activity; provider batches are limited to 32 MiB before projection. Message pages may contain fewer rows than requested to stay inside their aggregate ceiling. Attachment content is non-streaming, capped at 20 MiB raw, validated against stored metadata before and after the exact blob lookup, and revalidated in Svelte after base64 decoding. Recipient lists, bodies, external destinations, and vault inputs are separately bounded.

`open_message_link` is deliberately narrower than generic opener authority. The frontend has no opener or shell capability; Rust accepts only absolute normalized HTTP(S) URLs without credentials or raw/encoded control characters.

## Provider boundary

Provider work remains provider-neutral, but Gmail onboarding adds exactly two provider-specific lifecycle commands: begin and cancel. They expose no client value, authorization code, token, remote message identifier, or provider DTO. The Gmail sync/mutation core remains exercised only with hermetic Rust fixtures, and no live Google grant has run.
