# Start Here

Read `AGENTS.md`, `README.md`, `docs/ARCHITECTURE.md`, `docs/RUNTIME.md`, and `docs/KNOWN_LIMITATIONS.md`.

Before editing:

```bash
npm run verify
```

For UI work also run `npm run e2e`; for store/search/pagination/operation work also run `npm run benchmark`. Open the app with `npm run dev` and build the macOS bundle with `npm run build`.

There is one product implementation. Do not recreate the removed browser server/UI, use its external recovery archive as a fallback, add a real provider, or claim another platform without explicit acceptance criteria and compile-and-run evidence.
