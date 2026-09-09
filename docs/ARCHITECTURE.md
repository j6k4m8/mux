# Architecture

One executable path:

```text
Svelte UI
   │ typed invoke/events
Tauri command boundary
   │
Rust domain/store/workers
   │
SQLite projection + journal        macOS Keychain credentials
```

Vite exists only to build the embedded frontend and to serve it on `127.0.0.1` during `tauri dev`. There is no product HTTP server, browser UI, or Node store, and `scripts/check.mjs` fails if the paths of the removed ones come back.

## Modules worth knowing about

Most of `native/src-tauri/src` is named after what it does. These are the ones whose responsibility is not obvious from the filename:

- `provider.rs`, `provider_ingest.rs`, `provider_schema.rs` — the provider-neutral contract every adapter is written against: atomic projection, container replacement, reconciliation state, and the triggers that keep work identity and error codes honest at the SQL level.
- `worker.rs` — durable work claiming, leases, retries, ordering, and the uncertain-send outcome.
- `internet_message.rs` — canonical `Message-ID`, `In-Reply-To`, and bounded `References` values for both received and locally composed mail, which is what makes reply chains real rather than subject-matched.
- `mime_ingest.rs` — the bounded `mail-parser` trust boundary. Every byte of mail passes through it.
- `content.rs` — HTML5-tree sanitization into the controlled render subset.
- `navigation.rs` — the HTTP(S)-only external-link policy and the exact WebView-origin allowlist.
- `ipc_boundary.rs` — the serialized response ceilings, in one place so no command can quietly pick its own.
- `provider_conformance.rs` — the projection matrix the worker applies, exercised offline as its own gate.

## State model

Three layers, and they are not interchangeable.

**Confirmed provider projection** describes the last state the provider acknowledged. UI code never mutates it directly.

**Durable pending intent** is what the user asked for. Actions append an operation and a work item; effective SQLite views overlay pending operations on the confirmed projection, so archive, read, star, and send render immediately and survive a restart.

Container membership is recorded per message, so a thread is in a folder or carries a label either entirely, not at all, or partially. A pending change overlays that aggregate with the desired end state. This lets a partially labeled thread be normalized without pretending its prior distribution was a boolean — and it is why a confirmed normalization has no exact inverse: the per-message distribution is not recoverable from a thread-level undo. Cancelling before execution is still safe.

**Mux-owned metadata** — drafts, snooze times, invitation responses, operation activity — belongs to Mux and does not pretend to be provider state.

## Command flow

Svelte invokes a typed command. Rust validates sizes, enums, identifiers, and allowed state transitions. One SQLite transaction records the operation and its durable work. Effective views expose the intent immediately. Workers later claim eligible work by scope and ordering key, and an adapter acknowledges, reconciles, retries, or fails it through the same state machine. A `mux://mailbox-changed` event triggers a refresh; ordinary navigation stays local throughout.

Undo appends a compensating operation rather than erasing history. A stale operation is rejected, not silently applied.

## Sending

Sending is the one non-idempotent path and has its own state machine. Queuing locks the draft and freezes a durable snapshot — Date, stable Message-ID and client correlation, recipients, bodies, and bounded reply headers — before the undo window opens; the snapshot is fingerprinted, and the current version is 3, with earlier versions still validated for rows written by earlier builds. Provider-neutral construction emits deterministic standards MIME, keeps Bcc in the transport envelope only, and rejects internationalized envelope addresses until an adapter proves SMTPUTF8 support.

Failure before submission is retryable by policy. A lost acknowledgement after submission becomes `outcome_unknown`, and recovery unlocks the exact validated snapshot without claiming failure and without resending. There is no generic automatic replay.

Gmail submits the validated snapshot as raw MIME through its API. Generic IMAP accounts use a paired SMTP authority: implicit TLS or STARTTLS, bounded EHLO/authentication/reply parsing, exact envelope routing, and a fence immediately before `DATA`. Explicit pre-acceptance failures retain their retry classification; any ambiguous post-`DATA` result becomes `outcome_unknown` and is never automatically replayed. Acceptance confirms the local Sent projection. IMAP later adopts an exact Message-ID plus client-correlation match instead of inserting a duplicate, but Mux does not issue `APPEND` and cannot guarantee a server-side Sent copy.

## Search and pagination

Search input is bounded by character count, token count, nesting depth, field vocabulary, and date validation before any SQL is compiled, and values stay bound parameters. FTS backs free-text body matching. `from:` reads the sender fields; `to:` reads To, Cc, and Bcc. Provider batches rebuild each affected thread's index deterministically, so replay, replacement, rethreading, and tombstones cannot leave stale terms behind.

Thread, message, and search pages use restart-stable signed keyset cursors, versioned and bound to the exact account, view, query, timezone, and thread scope, capped at 256 bytes. They are cursors over the live view, not snapshots of the database.

Serialized responses have measured aggregate ceilings: 512 KiB for bootstrap, 3 MiB for a thread or search page, 16 MiB for message detail, 28 MiB for attachment content, 256 KiB for activity, and 32 MiB for a provider batch before projection. A message page splits at the last complete row below its actual serialized size and continues with a cursor. Bootstrap carries draft headers only; a full draft and attachment bytes each need their own explicit lookup. Attachment reads are non-streaming and capped at 20 MiB raw with declared, stored, and decoded lengths required to agree.

The mailbox keeps every loaded row but renders at most 120 of them.

## Credentials

Secrets are never stored in SQLite and never cross IPC. Gmail client values, authorization codes, and tokens stay in Rust. For generic mail, Svelte submits only non-secret endpoint configuration; an AppKit dialog gathers IMAP and SMTP passwords in native secure fields, and Rust zeroizes the borrowed values after verification and Keychain persistence. The command shape rejects unknown fields. There is no command that reads, returns, or unlocks a credential, and none accepts a password over IPC.

The installed-desktop flow uses the system browser, random state, PKCE S256, an exact random-port IPv4 loopback redirect with bounded reads and deadlines, fixed Google endpoints, `gmail.modify`, and Gmail profile identity binding. Completed authority goes to the macOS Keychain; SQLite keeps only the normalized mailbox identity and an opaque reference. At startup every stored reference is revalidated against the keychain, and an account whose record is missing or malformed is marked for reauthorization.

Generic-mail connection records contain independent IMAP and SMTP endpoints, usernames, passwords, and TLS policies in one account-bound Keychain value. They are decoded in Rust immediately before a bounded transport session and nowhere else; SQLite retains only mailbox identity, provider state, capabilities, and the opaque Keychain reference.

## Hostile-content boundary

Raw messages pass through bounded standards-based MIME parsing first. The boundary enforces raw, tree, header, decoded-text, attachment, address-envelope, and aggregate budgets, handles common charsets and encoded headers and parameters, and preserves accepted binary attachment bytes exactly.

HTML is then parsed as an HTML5 tree and serialized only into controlled render nodes. Active, embedded, form, and resource content is removed, remote resources are denied, and links are normalized and revalidated in Rust. The result renders inside a frame denied `allow-scripts`, and the WebView cannot navigate away from the exact app or development origin.

A blocked remote resource leaves an inert `<mux-remote-image data-id="N">` marker carrying no URL. Consent is explicit — once for this view, or persisted for the sender or one exact domain — and on consent Rust resolves and fetches the image itself: HTTPS only, no credentials or custom ports, DNS pinned to a pre-validated public address, same-host redirects only, a bitmap content-type allowlist, and a byte ceiling. It comes back as a bounded `data:` URL. The WebView is never granted `https:` image authority.

This boundary is adversarially tested. It is not a complete mail-security product; see [Known Limitations](KNOWN_LIMITATIONS.md).

## Verification layers

- Vitest under jsdom covers the pure helpers and mounts the production Svelte components behind strict typed Tauri IPC mocks that throw on an unexpected command.
- Rust tests cover migrations, the store, search, the worker, the provider-neutral contracts, content sanitization, keychain storage, and the Gmail and IMAP synchronization and mutation state machines.
- The offline provider contract runs the production worker projector and the adapter boundaries with no network; a repository scan rejects credential-shaped filenames and common secret shapes.
- The benchmark runs the real schema and store against a temporary 100,000-message database, including a populated migration from the oldest released schema and an interrupted-operation recovery.
- `npm run build` plus an actual `.app` launch is required before claiming anything about macOS.
