# Mux Architecture

## One application boundary

Mux has one executable product path:

```text
Svelte UI
   │ typed invoke/events
Tauri command boundary
   │
Rust domain/store/workers
   │
SQLite projection + journal        encrypted credential vault
```

Vite exists only to build the embedded Svelte frontend and to serve it on `127.0.0.1` during `tauri dev`. There is no product HTTP server, browser UI, or Node store.

## Runtime modules

- `native/src`: production Svelte UI and pure UI helpers.
- `native/src-tauri/src/lib.rs`: typed Tauri command boundary and narrow allowlist.
- `store.rs`: SQLite migrations, queries, drafts, operations, demo projection, and local commands.
- `search.rs`: bounded parser, AST, SQL compilation, date semantics, and sender/recipient field predicates.
- `worker.rs`: durable work claiming, retries, ordering, and uncertain-send outcomes.
- `outgoing.rs`, `internet_message.rs`: provider-neutral RFC 5322/MIME construction plus bounded canonical Internet message identities and reply chains.
- `provider.rs`, `provider_ingest.rs`, `provider_schema.rs`: provider-neutral contracts, atomic projection, label replacement, and bounded full-reconciliation state.
- `gmail.rs`, `gmail_access.rs`, `google_authorization.rs`: a Gmail bootstrap/history and mailbox-mutation adapter, fixed-origin HTTPS transport, typed vault authority, and bounded installed-desktop authorization lifecycle. These are hermetically exercised; the existing ignored Tidings registration has passed the loader, but live Google execution is absent.
- `mime_ingest.rs`: bounded `mail-parser` trust boundary for MIME trees, transfer encodings, charsets, decoded headers, nested messages, and binary attachments.
- `content.rs`: HTML5-tree sanitization into the controlled rich-text subset with remote resources denied.
- `navigation.rs`: normalized HTTP(S)-only external-link policy and exact WebView-origin allowlist.
- `vault.rs`: passphrase vault lifecycle and Rust-only secret access.

## State model

### Confirmed provider projection

Provider-confirmed rows describe the last acknowledged remote state. UI code never mutates this layer directly.

### Durable pending intent

User actions append operations and work items. Effective SQLite views overlay pending operations on confirmed projection, so archive/read/star/send interactions render immediately and survive process restart.

Provider label state is explicitly three-valued at the Rust boundary: absent from every locally known remote message, present on all of them, or partial. A pending add/remove overlays that confirmed aggregate with its desired state. This allows a partially labelled Gmail thread to be normalized without pretending that its prior distribution was a Boolean.

### Mux-owned metadata

Drafts, snooze times, invitation responses, operation activity, and future local labels belong to Mux and do not pretend to be provider state.

## Command flow

1. Svelte invokes a typed Tauri command.
2. Rust validates size, enum, identifier, and state-transition bounds.
3. One SQLite transaction records the operation and durable work.
4. Effective views expose local intent immediately.
5. Workers claim eligible work by scope and ordering key.
6. The deterministic demo and hermetic Gmail adapter fixtures acknowledge, reconcile, retry, or fail work through the same state machine.
7. Tauri events trigger a projection refresh; ordinary navigation remains local.

Undo creates a compensating operation. It does not erase history. Stale operations are rejected rather than silently applied.

## Sending

Sending remains a special non-idempotent state machine. A draft is locked when queued. The v2 durable snapshot freezes Date, stable Message-ID/client correlation, recipients, bodies, and bounded reply headers before the undo window. Provider-neutral construction emits deterministic standards MIME, keeps Bcc in the transport envelope only, and rejects internationalized envelope addresses until an adapter proves SMTPUTF8 support. Pre-submission failure is retryable according to policy; a lost acknowledgement after submission becomes `outcome_unknown`. Recovery unlocks the exact validated draft snapshot without claiming failure and without resending. There is no generic automatic send replay.

Canonical Internet `Message-ID`, `In-Reply-To`, and bounded `References` values are stored with normalized provider and local messages. A reply prefers the latest persisted message identity, so the first response to received mail and later local responses build a real bounded thread chain. Existing pre-submission v1 work is upgraded atomically by replacing only non-executing work inside the startup transaction; the runtime immutable-work trigger remains enforced.

## Search and pagination

Search input is bounded by character count, token count, nesting, field vocabulary, and date validation before SQL compilation. Values use bound SQLite parameters. FTS backs free-text body matching. Provider batches rebuild each affected thread index deterministically, so replay, replacement, rethreading, and tombstones do not retain stale terms. `from:` reads sender fields; `to:` reads To/Cc/Bcc fields. Thread, message, and search APIs use restart-stable, signed, versioned keyset cursors bound to the exact account/view/query/timezone/thread scope and capped at 256 bytes. They are live-view cursors, not historical database snapshots.

Serialized Tauri responses have measured aggregate ceilings: 512 KiB bootstrap, 3 MiB thread/search pages, 16 MiB message detail, 28 MiB attachment content, and 256 KiB activity. Message pages split adaptively at the last complete row below the actual serialized ceiling and continue with an opaque cursor. Bootstrap carries draft headers only; full drafts and attachment bytes require exact typed lookups. Attachment reads are non-streaming and capped at 20 MiB raw with declared/stored/decoded length agreement. Provider batches are capped at 32 MiB before projection. The Svelte mailbox retains loaded rows but renders at most 120; conversations render at most 18 cards. Node/jsdom stress tests exercise 50,000 mailbox rows and 10,000 messages, while actual WebView frame timing remains a separate runtime check.

## Credentials

Secrets are not stored in SQLite or returned across IPC. Svelte can invoke vault lifecycle commands plus typed Gmail connect/cancel lifecycle commands; client values, authorization codes, and tokens remain in Rust. The installed-desktop flow uses the system browser, random state, PKCE S256, an exact random-port IPv4 loopback redirect with bounded reads/deadlines, fixed Google endpoints, `gmail.modify`, and Gmail profile identity binding. Completed authority is written to the vault while SQLite stores only mailbox identity and an opaque reference. The vault uses a stable lock file, authenticated versioned envelope, bounded Argon2id parameters, XChaCha20-Poly1305, atomic replacement, and zeroizing buffers. It is intentionally independent of Keychain and Apple signing. No live consent grant has run.

## Hostile-content boundary

Raw messages first pass through bounded standards-based MIME parsing. The boundary enforces raw, tree, header, decoded-text, attachment, address-envelope, and aggregate budgets; handles common charsets and encoded headers/parameters; and preserves accepted binary attachment bytes exactly. HTML is then parsed as an HTML5 tree and serialized only into controlled render nodes. Active/embedded/form/resource content is removed, remote resources are denied, links are normalized and revalidated in Rust, and the WebView cannot navigate away from the exact app/development origin.

This boundary is adversarially tested but is not a complete mail-security product. Remote-image consent, S/MIME/PGP, malware/quarantine policy, and provider-fetched attachment lifecycle remain future work.

## Verification layers

- Node's built-in test runner covers pure UI algorithms.
- Vitest/jsdom mounts the production Svelte components and uses strict typed Tauri IPC mocks that reject unexpected commands.
- Rust tests cover migrations, store/search/worker/provider-neutral contracts, content, vault behavior, and the hermetic Gmail synchronization and mutation state machines.
- The mandatory offline provider contract runs the production worker projector and Gmail adapter boundary without network access; fixture/source scanning rejects common secret shapes.
- The release benchmark uses the native schema/store against a temporary 100,000-message database, including populated migration and interrupted-operation recovery.
- `tauri build` plus an actual `.app` launch is required for a macOS runtime claim.
