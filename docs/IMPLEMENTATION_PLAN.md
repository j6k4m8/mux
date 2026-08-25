# Mux Implementation Plan

This is the only active plan. It is intentionally short and evidence-gated.

## Current completed slice

One native application now owns the demo behavior:

- Tauri 2 shell and typed command/event allowlist;
- Svelte light-first mailbox UI with responsive macOS layouts;
- Rust/SQLite schema, projection, pending intent, metadata, search, paging, drafts, work, undo, send uncertainty, snooze, RSVP, activity, safe demo content, and credential vault;
- pure UI, production-component, Rust contract, native performance, and macOS bundle gates.

The old Node/browser product path is gone and must not return.

## Completed slice — hostile real-mail boundary

Acceptance criteria:

Completed and exercised: bounded MIME/charset/header/address parsing, nested-message handling, exact binary attachment preservation, HTML5-tree allowlisting, denied remote/active content, typed external links, hostile corpora, and full native gates. Provider-fetched attachment cache/preview/open policy remains in its dedicated roadmap task.

No provider may bypass this boundary.

## Completed offline slice — Gmail synchronization and mutation core

Gmail is the first adapter. Hermetic acceptance fixtures now cover:

- exact read-only or modify-scoped authority records behind the passphrase vault and profile identity binding;
- stable identifiers, bounded 10,000-label/bootstrap/history pages, and durable cursors;
- current-watermark churn, replacement, deletion/tombstones, invalid-history rescan, and multi-pass reconciliation;
- rate-limit, retry, auth, permanent-failure, raw-message, and metadata-fallback classification;
- provider-neutral atomic projection with pending local intent preserved.
- desired-state archive/restore, read/unread, star/unstar, custom-label, trash/untrash, and missing-remote reconciliation through the durable journal.

The production HTTP and refresh transports compile but have not run against Google or a hermetic HTTP server. No live provider claim follows from this slice.

## Completed offline slice — Gmail installed-desktop authorization

The Rust-only flow now provides typed connect/cancel commands, system-browser authorization, random state, PKCE S256, an exact random-port IPv4 loopback redirect, bounded requests/deadlines, `gmail.modify`, profile identity binding, and vault-only authority persistence. The ignored Tidings registration passes the bounded production loader. No client value, code, or token enters SQLite or IPC.

## Next slice — live Gmail conformance

Complete a real Google consent grant with a dedicated test account. Then test startup locked state, unlock/resume provenance, genuine reauthorization, reset/sign-out, concurrent lock/auth races, production request bounds, bootstrap, delta, pagination, deletion, offline restart, rate limiting, large-mailbox backfill, and the already-hermetic mailbox mutations on macOS before claiming support. Svelte continues to read only the local projection.

## Slice 5 — send

Add Gmail sending only after live synchronization and mutation conformance are stable. A provider send must identify its pre-submission boundary and reconciliation mechanism; automatic retry after ambiguous submission remains forbidden.

## Slice 6 — release work

After a real provider is exercised on macOS: signing/notarization strategy without assuming an available developer account, hosted-download/Gatekeeper documentation, update integrity, crash diagnostics without mail/credential leakage, and fresh-machine install tests.

## Deferred

Windows, Linux, Android, and iOS; full calendar APIs; real AI; provider-facing MCP; third-party plugins; and customizable shortcut infrastructure. None is a current support claim or parallel implementation target.
