# Mux 0.1.0 Consolidation Test Report

Date: 2026-08-23

## Environment

```text
macOS 26.2 (25C56), arm64
Node v26.5.0
npm 11.17.0
rustc/cargo 1.91.1
```

The project declares Node 22.13 or newer. This report records only the environment actually exercised.

## Pre-consolidation baseline

Before modifying the dual implementation, the former root `npm run verify` initially hit a sandbox loopback-permission error and then passed outside that restriction: 63/63 tests. The former `npm run e2e` initially reported its missing Python browser dependency; using the already-cached compatible dependency made its desktop/mobile browser-harness checks pass. No dependency was installed into or retained by the final product.

The former `npm run benchmark` passed against 100,000 messages/50,000 threads:

```text
seed 786.12 ms
inbox 12.81 ms
structured search 22.88 ms
FTS search 32.61 ms
thread detail 0.07 ms
archive 0.27 ms
```

These were comparison results, not final native claims.

## Final `npm run verify`

Passed with exit code 0:

- product-boundary check: one native path; 16 JavaScript modules syntax-checked; provider fixture/source secret-shape guard passed;
- Svelte check: 0 errors, 0 warnings;
- pure UI algorithms: 27/27;
- production-Svelte interaction tests: 21/21;
- Vite production build: 131 modules;
- Rust library tests: 186/186;
- Rust doc/binary tests: passed;
- `cargo check`: passed;
- `cargo fmt --check`: passed;
- `cargo clippy --all-targets -- -D warnings`: passed.

The Rust suite covers migrations, projection/intent semantics, worker leases/retries/send uncertainty, provider-neutral batches, replay-safe search, long demo threads, drafts/recipients, snooze/RSVP/activity, bounded MIME/charset/header/address parsing, exact attachments, deterministic outgoing MIME, canonical Message-ID reconciliation, received/local reply threading, HTML5-tree sanitization, navigation/opener policy, vault lifecycle/concurrency/tamper/failpoints, and the Tauri command allowlist.

## Final `npm run e2e`

Passed: 4 files, 21 production-component interaction tests.

Coverage includes light-shell bootstrap, mailbox rows/reader, persisted appearance, ordinary Tab order, modal/composer focus cycle and restoration, Escape hierarchy, `r` versus `a`, `h` snooze, Command-K palette execution, toolbar/snooze/RSVP typed command dispatch, activity, untouched-forward persistence, selected-unread auto-read, safe/unsafe message-link routing, keyboard-operable rich-text formatting, exact 100/101-character inline-reply expansion, pop-out body/mode preservation, media-query-driven navigation state, bounded mailbox/conversation render windows, stale asynchronous draft-load rejection, literal account ID `all` versus unified `null`, and the explicit full-draft lookup. IPC mocks are strict and throw on unexpected Tauri commands.

This is not a WebView pixel test and makes no Playwright/browser-product claim.

## Final `npm run benchmark`

Passed against an isolated native Rust/SQLite database that was removed with its WAL/SHM sidecars:

```text
fixture: 100000 messages across 50000 threads
fresh migration: 9.24 ms (limit 1000 ms)
populated v12→v15 migration: 139.19 ms (limit 5000 ms)
seed: 303.36 ms (limit 15000 ms)
inbox: 0.21 ms (limit 250 ms)
structured search: 18.87 ms (limit 500 ms)
FTS body search: 0.14 ms (limit 750 ms)
thread detail: 0.06 ms (limit 100 ms)
archive mutation: 0.23 ms (limit 100 ms)
interrupted operation recovery: 0.98 ms (limit 1000 ms)
total: 904.93 ms (limit 25000 ms)
```

Correctness assertions accompany every timing, including exact fixture counts, a populated stale-FTS v12 migration through v15 using the released receipt table without `fingerprint_version`, schema/bootstrap, integrity/foreign keys, pagination, search cardinality, message detail, durable archive work, interrupted-operation recovery, and immediate local projection. The first populated-migration run exposed a quadratic rebuild at 94,738.31 ms; the bulk ordered rebuild above is the measured fixed result.

## Offline provider-core contract

`npm run test:provider-contract` passed 9/9 reusable adapter-contract tests. It exercises the production worker projector, the exact sync/mutation/send projection-kind matrix, atomic projection/cursor/receipt/fenced-success commit, exact replay, changed-batch and stale-prior conflicts, cross-account/identity rejection, failpoint rollback, aggregate batch rejection, retry-safe lease recovery, and non-idempotent send uncertainty. Wrong projection variants cannot complete work, advance cursors, write receipts, confirm operations, or delete send drafts. No provider, network request, credential, or real mailbox fixture was used.

## macOS bundle

The first consolidated command exposed an npm wrapper error and failed before bundling. After correction, inspection found that Cargo/Tauri had selected the new benchmark binary as the application executable. The build was rejected.

The utility is now feature-gated, Cargo declares `default-run = "mux-native"`, the generated bundle was deleted, and a clean `npm run build` passed. A permanent post-build check verifies:

```text
Mux.app contains exactly one file in Contents/MacOS: mux-native
CFBundleExecutable: mux-native
codesign structural verification: passed
signature kind: explicit ad-hoc
```

One-time inspection of the artifact built on the exercised machine also identified the product executable as an arm64 Mach-O. Architecture is recorded as observed evidence, not enforced as a cross-machine build requirement.

The release `.app` launched successfully and rendered the canonical light interface at the bundled `tauri://localhost` origin. The current schema-v15 build opened the existing on-disk demo projection successfully and displayed the bounded recent window of a 36-message thread. The inline reply was observed compact and expanded beyond 100 characters; `r` and `a` selected the expected recipients; formatting controls and the link popover opened; Escape closed the popover and composer; and pop-out preserved the reply-all recipients and body. A real narrow macOS window also exercised the navigation drawer open/close state and Escape focus restoration. That run exposed and fixed stale responsive state when macOS changed media-query width without the expected generic resize event.

The app is ad-hoc signed for local use. No Developer ID, notarization, hosted-download, or other-Mac Gatekeeper claim is made.

## Deletion and recovery

Before deletion, the complete former harness and fake workspace database were archived and its table of contents verified at:

```text
/private/tmp/mux-browser-harness-pre-collapse-20260822.tar.gz
```

The archive is recoverable but intentionally outside the product repository. The Node server/UI, browser tests, duplicate seed/benchmark, workspace database, and stale screenshots are absent, and `scripts/check.mjs` fails if their product paths return.

## Unproven boundaries

No real mail provider, real-provider exercise of the bounded MIME/HTML pipeline, remote-resource consent, attachment quarantine, Developer ID signing/notarization, hosted distribution, or non-macOS platform is claimed.
