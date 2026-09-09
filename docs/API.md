# Tauri command API

Mux has no product HTTP API. Svelte calls the allowlisted Rust commands registered in `native/src-tauri/src/lib.rs`, and changes are announced back through the typed `mux://mailbox-changed` event.

## Queries

| Command | Purpose |
| --- | --- |
| `mailbox_bootstrap` | Accounts, view counts, per-account folders and labels, schema version, and header-only draft summaries |
| `list_threads` | Bounded keyset page for Inbox/Starred/Snoozed/Sent/Archive/Trash/All |
| `get_thread_summary` | Recover one selected row under the same view and search constraints |
| `get_thread_messages` | Adaptive bounded message page plus attachment metadata and any invitation |
| `search_threads` | Bounded parsed search over the effective local projection |
| `mailbox_stats` | Daily volume, top senders and recipients, and totals over a bounded day window |
| `read_attachment` | Read one validated attachment through a typed base64 DTO |
| `get_draft` | Load one full draft explicitly; draft bodies are never in bootstrap |
| `list_operations` | Bounded payload-free operation activity summaries |
| `open_message_link` | Revalidate and open one normalized absolute HTTP(S) destination through the app-owned native opener |
| `load_remote_image` | Fetch one consented remote image in Rust and return it as a bounded `data:` URL; the WebView never receives the origin URL |

## Mutations

| Command | Purpose |
| --- | --- |
| `save_draft` | Create or update a bounded draft with revision protection |
| `delete_draft` | Delete an unlocked draft |
| `queue_send` | Lock a draft and append the dedicated delayed-send operation and work item |
| `apply_thread_action` | Archive/restore, trash/untrash, read/unread, or star/unstar |
| `undo_operation` | Append a compensating operation after stale-state validation |
| `snooze_thread` | Set bounded Mux-owned wake metadata |
| `rsvp_thread` | Store `accepted`, `tentative`, `declined`, or `needsAction` locally |
| `resolve_outcome_unknown_send` | Validate the exact immutable send snapshot and unlock it without resending |
| `allow_remote_content_sender` | Persist a remote-content allow for that message's sender, scoped to its account |
| `allow_remote_content_domain` | Persist a remote-content allow for one exact domain, validated against that message's own blocked candidates |

## Accounts and sync

| Command | Purpose |
| --- | --- |
| `gmail_oauth_begin` | Start the installed-desktop authorization and resolve once an account is bound |
| `gmail_oauth_cancel` | Abandon an in-flight authorization and release its loopback listener |
| `imap_account_add` | Accept only non-secret IMAP/SMTP endpoint fields, collect both passwords in native macOS secure fields, authenticate both transports, then bind the mailbox and write one combined authority record to the Keychain. Cancellation returns no account; no credential crosses IPC or is returned |
| `account_remove` | After interface confirmation, atomically remove one account's local projection, drafts, and durable work while recording any required account-bound Keychain cleanup. Refuses an executing sync/send, resumes credential cleanup after a crash, and never mutates provider mail |
| `set_account_refresh` | Set one account's refresh cadence, and wake the worker so a shorter one takes effect now |
| `set_account_color` | Recolor one account. Exactly one `#rrggbb` is accepted and stored lowercased: the value is rendered into inline styles, so nothing looser gets in. Serialized with removal and the other account-lifecycle changes |
| `sync_account_now` | Schedule an immediate sync for one account, routed by its stored provider kind |
| `resync_all_mail` | Rewind provider sync bookmarks so the next cycle re-downloads and re-projects every message in place; removes nothing |

Every mutating command validates identifiers, input sizes, enum vocabularies, the current effective state, and the allowed transitions in Rust. Effective views expose the local result immediately; workers process the durable work later.

## Bounds and casing

Tauri serializes Rust snake_case fields to camelCase. The exact DTOs live in `native/src/types.ts` and in `native/src-tauri/src/store.rs` and `store/stats.rs`; tests cover their command payload shapes.

Opaque cursors are versioned, integrity-checked, scope-bound, restart-stable, and at most 256 bytes. Serialized ceilings are 512 KiB for bootstrap, 3 MiB for thread and search pages, 16 MiB for message detail, 28 MiB for attachment content, and 256 KiB for activity, with provider batches limited to 32 MiB before projection; they are declared together in `ipc_boundary.rs`. A message page may contain fewer rows than requested to stay inside its aggregate ceiling. Attachment content is non-streaming, capped at 20 MiB raw, validated against stored metadata before and after the blob lookup, and revalidated in Svelte after base64 decoding. Recipient lists, bodies, and external destinations are bounded separately.

`load_remote_image` and `open_message_link` are both deliberately narrower than the authority they replace. The frontend never holds a remote image URL, and it has no opener or shell capability: Rust accepts only absolute normalized HTTP(S) URLs with no credentials and no raw or encoded control characters.
