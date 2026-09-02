# AGENTS.md — Mux project instructions

Mux is a local-first mail client for people who live in their inbox. One product implementation: Tauri 2, Svelte, Rust, SQLite. macOS is the only platform exercised; another platform is future work until it compiles, packages, launches, and stores mail.

The seeded demo mailbox is a fixture, not a second application. Preserve observable local-first behavior while extending it.

## Non-negotiable rules

1. Run the existing checks before changing code, and the full gate before calling something done.
2. Never claim a feature, provider, platform, test, benchmark, or security property that was not actually exercised. This is the rule the rest of the repository is built on; documentation that overstates the code is worse than no documentation.
3. Prefer small, gated changes to broad speculative scaffolding.
4. Keep the Svelte interface provider-agnostic. Provider calls belong behind Rust domain commands and adapters, and no provider type or remote identifier may become meaningful to the frontend.
5. Preserve the three layers: confirmed provider projection, durable pending local intent, and Mux-owned metadata. They are not interchangeable.
6. Render the local projection immediately. Ordinary navigation must never wait on a network request.
7. Sending is not idempotent. It keeps its own uncertain-outcome state machine, and an ambiguous submission is never retried automatically.
8. When replacing an implementation, keep the equivalent store, search, operation, and interaction contracts green.
9. Do not introduce event sourcing, CQRS, microcrates, a backend service, or a second product UI. `scripts/check.mjs` fails if the removed server and browser harness paths return.
10. Treat email HTML, MIME, attachments, and remote resources as hostile. Everything crosses the bounded Rust parse and sanitization boundary; nothing bypasses it.
11. Keep credentials out of SQLite, logs, fixtures, screenshots, IPC, and release archives. There is no command that reads, writes, or unlocks a credential, and none that accepts a password.
12. Invitations and RSVP are mail-centric and local. A real calendar API is separate work with its own permissions.
13. Preserve accessibility, keyboard operation, responsive behavior, and large-mailbox performance.
14. Before wiring up another provider, specify and test its credential, sync, retry, pagination, mutation, and send semantics. Gmail and IMAP each arrived that way.
15. Do not weaken a test or a defensive check to make a change pass.

## Required validation

```bash
npm run verify          # always
npm run e2e             # interface changes
npm run benchmark       # search, storage, pagination, or operation changes
```

Before a release archive, run `npm run manifest:write` and `npm run release:check`, then extract the archive into a fresh directory and run `npm run release:check` again.

A macOS claim requires `npm run build`, launching the resulting `.app`, and looking at the real WebView. Component tests are not a platform claim.

## Where the boundary currently sits

README.md describes what connects and sends today; `docs/KNOWN_LIMITATIONS.md` is the blunt version. Read both before writing anything that asserts a capability. If your change moves that boundary, move the prose with it in the same change — and only as far as the evidence reaches.

## Working style

Inspect the relevant files before editing. State exact acceptance criteria. Run the narrowest useful test while iterating and the full gate at the end. Report the commands you ran, what actually happened, and what remains untested.
