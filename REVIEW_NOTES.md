# Consolidation Review Notes

## Outcome

The light interface is now the canonical native Svelte product. The dark native screenshot was not a second newer feature set; it was a divergent UI branch. Its useful native interactions and Rust contracts were preserved while the light shell, navigation, invitation card, message cards, and reply placement became canonical.

The duplicate Node server, public browser UI, workspace fake database, Python/Playwright path, browser-only store/search tests, duplicate seed/benchmark scripts, and all stale UI screenshots were archived outside the repository and removed.

## Defects caught during convergence

- Normal Tab order changed when the command-palette control was inserted; the production component test failed and the expected accessible order was updated.
- Rich-editor autofocus could run after the component unmounted; its timer is now cancelled.
- Global keyboard dispatch previously lacked native snooze; `h` is now implemented and tested.
- Search relative dates used UTC boundaries; the typed command now carries a bounded local offset for consistent search and selected-row recovery, with the DST-span approximation documented.
- Recipient queueing now accepts quoted display names containing commas while rejecting header injection and malformed structure before creating operations.
- Plain draft bodies again enforce the exact Unicode-aware 50,000-character reference limit; formatted HTML retains its separate justified bound.
- The actual narrow macOS window exposed stale responsive state when CSS crossed a breakpoint without the expected generic resize event; media-query change listeners now keep the drawer state and accessibility contract synchronized, with a component regression and a repeated `.app` runtime check.
- The first release launch exposed an upgrade-only schema defect because released v12 provider receipts lacked `fingerprint_version`; the atomic v14 migration now maps those populated receipts to conservative legacy-v1 state and reopens cleanly.
- The bundle is now explicitly ad-hoc signed, and the post-build gate verifies both structural signature validity and the actual `Signature=adhoc` details.

## Preserved native behavior

Rich compose/reply/reply-all/forward, inline reply expansion/popout, attachments, long-thread paging, Rust search, delayed send/undo, uncertain-send recovery, snooze/RSVP/activity, safe rich text, durable operations, and the credential vault all remain on the single path.

The former demo MIME scanner and HTML scanner are also gone. Standards-based bounded MIME ingestion, HTML5-tree sanitization, typed link opening, exact WebView-origin policy, and deterministic affected-thread search rebuilds now own those contracts.

## Deliberately removed rather than ported

The fake AI sidebar, disabled/decorative controls, Node HTTP hardening tests, browser-only canned content, and implementation-status badges were not copied into the native app. There is no provider or platform support claim beyond what is compiled and run.

## Remaining review boundary

The component interaction suite is not a WebView pixel test. The built `.app` has been launched and its normal and narrow light layouts and key reply/navigation interactions inspected at `tauri://localhost`; the typed external opener itself was not exercised against a live external destination. MIME/HTML is adversarially exercised without a real provider, and real providers remain gated work. See `docs/KNOWN_LIMITATIONS.md`.
