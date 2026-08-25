# Native Runtime

Mux is no longer in a dual browser/native port phase. Tauri 2, Svelte, Rust, and SQLite are the only product runtime in this repository.

## Development path

`npm run dev` delegates to `native/package.json`, starts the loopback-only Vite asset server required by `tauri dev`, compiles the Rust shell, and opens the macOS application. `npm run build` produces the `.app` bundle. Vite alone is not a supported product entry point because its Tauri commands are absent.

## Consolidation acceptance gate

The browser harness was removed only after the native path had replacement coverage for:

- schema migration and runtime opening;
- local projection, durable operations, undo, stale rejection, drafts, send uncertainty, snooze, and RSVP;
- bounded search parsing and FTS queries;
- long-thread message paging and UI window helpers;
- Svelte mailbox, theme, Tab/Escape, reply/reply-all, command palette, and safe-link interactions;
- a deterministic 100,000-message Rust/SQLite performance run;
- Tauri bundling, signature verification, and executable inspection.

The archived pre-collapse harness is intentionally outside the repository and is not a supported fallback.

## Exercised macOS acceptance gate

The August 23, 2026 local release check required all of the following:

- an existing schema-v12 application database opens without deletion, migrates atomically to v15, and passes SQLite integrity checks;
- `npm run build` produces one `.app` whose executable matches `Info.plist` and whose explicit ad-hoc signature passes `codesign --verify --deep --strict`;
- the release bundle launches on macOS and renders the light `tauri://localhost` WebView;
- the running WebView displays a 36-message conversation and exercises inline reply, expansion after 100 characters, the rich-formatting controls and link popover, reply/reply-all recipients, Escape, pop-out compose, and native Tab focus order;
- a real narrow macOS window keeps the CSS breakpoint, drawer accessibility state, open/close control, and Escape focus restoration synchronized;
- no Developer ID, notarization, hosted-download, other-Mac Gatekeeper, or non-macOS support claim is inferred from this local run.

The launch check exposed an exact upgrade-only defect: v12 receipt tables lacked the later `fingerprint_version` column. The v14 migration maps those receipts to the conservative legacy-v1 state, and v15 adds bounded Internet threading headers. Regression fixtures cover populated v12 data and preserve the existing no-blind-replay behavior through the current schema.

## Product boundary

Do not recreate a second product server, browser interface, provider-specific UI, or duplicate fixture database. Browser technology remains an implementation detail of the embedded WebView. New behavior belongs in production Svelte components, typed Tauri commands/events, and Rust contracts.

## Platform claims

Only macOS is currently exercised. Responsive CSS is useful on a small macOS window but is not proof of Android, iOS, Windows, or Linux support. Each future platform needs its own compile, package, launch, storage, lifecycle, and background-work evidence.
