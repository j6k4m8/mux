# Mux — Codex Handoff

## Current state

Mux is a single native application. The former Node/SQLite HTTP server, plain browser UI, duplicate fake database, Python browser test, and stale screenshots were archived outside the workspace and deleted after replacement native gates passed.

The product path is:

- Svelte production components in `native/src`;
- typed Tauri commands/events in `native/src-tauri/src/lib.rs`;
- Rust domain, SQLite, search, work, content, provider-neutral, and vault modules in `native/src-tauri/src`;
- one root npm command surface delegating to `native/package.json`.

The light three-pane UI is canonical. Dark appearance is optional and token-only; it is not a second feature branch.

## Product contracts to preserve

1. UI navigation reads only the local effective projection.
2. Confirmed provider projection, pending local intent, and Mux-owned metadata stay distinct.
3. Mutations are transactionally journaled and render immediately.
4. Undo is a stale-checked compensating operation.
5. Sending is non-idempotent; outcome-unknown work is never blindly retried.
6. Provider-specific concepts stay behind Rust adapter contracts.
7. Arbitrary email content and provider input are hostile.
8. Credentials never enter SQLite, logs, screenshots, fixtures, generic IPC, or manifests.
9. `Tab` stays native to focus navigation; global shortcuts do not run inside interactive controls.
10. Platform/provider support is claimed only after compile-and-run evidence.

## Current native demo behavior

- unified/account Inbox, Starred, Snoozed, Sent, Drafts, Archive, Trash, All mail, and smart views;
- bounded mailbox, search, message paging, and long synthetic conversations;
- archive/restore, trash/untrash, read/unread, star/unstar, delayed undo, snooze, invitation RSVP, and activity;
- rich compose/reply/reply-all/forward, attachments, drafts, inline expansion after 100 characters, and popout;
- delayed send and exact-snapshot uncertain-send recovery;
- deterministic provider-neutral outgoing MIME with stable correlation/reply headers, envelope-only Bcc, attachment construction, and an explicit ASCII-only SMTPUTF8 policy;
- bounded standards MIME/charset ingestion, exact attachment bytes, HTML5-tree sanitization, and typed external links;
- deterministic search indexing across provider replay, replacement, rethreading, and deletion;
- light default, optional persisted dark appearance, responsive macOS window layouts;
- `j/k/e/h/s/u/r/a/f/c`, `/`, Escape hierarchy, and Command-K palette;
- passphrase credential-vault lifecycle independent of Keychain/Apple accounts.
- Gmail synchronization and mailbox-mutation core with bounded label/bootstrap/history pages, cursor-fenced continuation and rescan, mailbox-identity verification, vault-only credential lookup, and desired-state label/trash changes, exercised only by hermetic Rust fixtures.
- Bounded installed-desktop Google authorization lifecycle: typed connect/cancel commands, system browser, random state, PKCE S256, exact IPv4 loopback redirect, `gmail.modify`, profile identity binding, and vault-only authority persistence. The ignored Tidings registration has passed the real loader without exposing its values.

No live provider grant/account, provider send, AI, full calendar, or provider-facing extension is present. The authorization flow and mutation core have not touched a Google endpoint. Gmail is not a support claim until its production transport and consent flow run against a dedicated account.

## Verification commands

```bash
npm run verify
npm run e2e
npm run benchmark
npm run build
```

`npm run e2e` means production-Svelte component interactions with strict Tauri IPC mocks, not Playwright and not a second browser product. A macOS handoff must also launch and inspect the built `.app`.

## Gmail offline boundary

The first adapter choice is Gmail. The offline synchronization and mailbox-mutation core now answers and tests:

- authentication mechanism and exact vault records;
- bootstrap and incremental/delta synchronization;
- pagination and large-mailbox backfill;
- stable remote IDs, moves, deletions, and tombstones;
- rate-limit and transient/permanent/auth failure classification;
- exact read-only or modify-scoped vault record, profile identity binding, auth/rate/retry classification;
- 10,000-label pagination into 1,000-container transactions;
- bootstrap, one-record history paging, current-watermark churn, deletions, replay, and invalid-history full rescan;
- complete label replacement, derived thread state, and preservation of pending local intent;
- archive/restore, read/unread, star/unstar, custom-label changes, trash/untrash, stale-remote reconciliation, and atomic journal acknowledgement;
- keep the exercised MIME/HTML boundary between provider bytes and Svelte;
- deterministic adapter contract fixtures and integration tests.

The compiled fixed-origin HTTPS and refresh transports have not been run against Google or a hermetic HTTP server. Authorization construction, cancellation, parsing, persistence, and the real Tidings registration loader are exercised offline; granted-scope establishment, live delta behavior, and production request/response handling remain unproven. Permanent deletion is deliberately excluded, custom-label changes do not yet have a Svelte picker, and sending remains absent.

## Immediate next work

1. Exercise the implemented Google consent flow against a dedicated Gmail test account; verify the returned scope, profile binding, vault persistence, cancellation, and restart/unlock behavior without exposing tokens.
2. Exercise macOS bootstrap, delta, offline, retry, rate-limit, deletion, mailbox mutation, and large-mailbox behavior against that account before claiming support.
3. Exercise the mutation core against that account without weakening immediate local projection, durable retries, or stale-remote reconciliation; add send last with an explicit pre-submission and reconciliation contract.
4. Complete Developer ID/notarization and fresh-machine download work only if those release channels become available; the current local bundle is ad-hoc signed.

## Important workspace note

This workspace was supplied without `.git` metadata. No agent removed it. Restore repository metadata before attempting commits, branches, pushes, or pull requests.
