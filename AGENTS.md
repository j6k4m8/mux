# AGENTS.md — Mux Project Instructions

## Mission

Mux is a modern, local-first email client for power users. This repository contains one product implementation: Tauri 2 + Svelte + Rust + SQLite. macOS is the only platform currently exercised. Other platforms remain future work until each one compiles and runs.

The deterministic fake-provider demo is a contract fixture, not a second application. Preserve observable local-first behavior while extending it.

## Non-negotiable engineering rules

1. Run the existing checks before modifying code.
2. Never claim a feature, provider, platform, test, benchmark, or security property unless it was actually exercised.
3. Prefer small, gated changes over broad speculative scaffolding.
4. Keep the Svelte UI provider-agnostic. Provider calls belong behind Rust domain commands and adapters.
5. Preserve the three-layer state model: confirmed provider projection, durable pending local intent, and Mux-owned metadata.
6. Render the local projection immediately. Normal UI navigation must not wait on network requests.
7. Sending is non-idempotent and retains its dedicated uncertain-outcome state machine.
8. Keep equivalent store/search/operation and interaction contracts green when replacing an implementation.
9. Do not introduce event sourcing, CQRS, microcrates, a backend service, or another product UI without evidence.
10. Treat arbitrary email HTML, MIME, attachments, remote resources, extension content, MCP content, and email text supplied to AI as hostile.
11. The AI model never receives ambient authority. Permissions and confirmations are enforced outside the model.
12. Calendar permissions are separate and lazy. Initial support is mail-centric invitation/RSVP, not a full calendar.
13. MCP, CLI, and rules precede a heavy plugin ecosystem.
14. Keep credentials out of SQLite, logs, fixtures, screenshots, IPC, and release archives.
15. Preserve accessibility, keyboard operation, responsive behavior, and large-mailbox performance.
16. Do not add a real provider until its credential, sync, retry, pagination, and send semantics have explicit acceptance tests.

## Required validation

Before changing anything:

```bash
npm run verify
```

For UI changes:

```bash
npm run e2e
```

For search, storage, pagination, or operation changes:

```bash
npm run benchmark
```

Before producing a release archive:

```bash
npm run manifest:write
npm run release:check
```

Also extract the archive into a fresh directory and rerun `npm run release:check` there.

For a macOS application claim, run `npm run build`, launch the resulting `.app`, and inspect the actual WebView. Component tests alone are not a platform claim.

## Current implementation boundary

The current deterministic native demo validates:

- Rust SQLite schema/migrations, local projection, pending-intent semantics, work journal, undo, and stale-operation rejection;
- bounded Rust search parsing/AST/FTS and native mailbox/message pagination;
- drafts, rich compose/reply/reply-all/forward, delayed send, uncertain-send recovery, snooze, and synthetic invitation/RSVP;
- Rust-only provider credentials in the macOS Keychain, with no credential command on the IPC surface;
- bounded standards-based MIME/charset ingestion and HTML5-tree sanitization;
- typed external-link opening plus an exact bundled/development WebView-navigation allowlist;
- replay/replacement/rethread/deletion-safe search indexing;
- production-Svelte interactions through typed Tauri IPC mocks;
- a native 100,000-message isolated performance gate;
- responsive desktop layouts and a loopback-only internal Vite development URL;
- remote-image consent: inert markers in place of blocked resources, view-only and persistent sender/exact-domain allows, and Rust-side URL/address validation. The fetch itself has not run against a live host.

It does not implement real providers, S/MIME/PGP, attachment quarantine/scanning, real calendar APIs, real AI, provider-facing MCP/plugins, background sync, or proven Windows/Linux/Android/iOS support.

## Next implementation order

1. Keep the native local vertical slice green.
2. Keep the hostile MIME/HTML boundary and replay-safe search contracts green.
3. Complete the native provider-core conformance/performance gate.
4. Specify one provider adapter's authentication, delta sync, pagination, mutation, and send contracts.
5. Implement and exercise that provider without leaking provider types into Svelte.
6. Prove packaging and runtime behavior on each platform separately before adding its support claim.

## Working style

- Inspect relevant files before editing.
- State exact acceptance criteria.
- Run the narrowest relevant tests during iteration and the full required gate before completion.
- Report commands, actual results, remaining risks, and untested assumptions.
- Do not weaken tests or defensive checks to make a change pass.
