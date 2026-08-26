import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';

const root = path.resolve(import.meta.dirname, '..');
const requiredFiles = [
  'native/package.json',
  'native/index.html',
  'native/src/App.svelte',
  'native/src/App.interaction.test.ts',
  'native/src/RichEditor.interaction.test.ts',
  'native/src-tauri/Cargo.toml',
  'native/src-tauri/src/lib.rs',
  'native/src-tauri/src/store.rs',
  'native/src-tauri/src/bin/mux-benchmark.rs',
  'native/src-tauri/tauri.conf.json',
  'native/src-tauri/capabilities/default.json',
  'scripts/check-bundle.mjs',
  'scripts/check-provider-fixtures.mjs'
];
const forbiddenFiles = [
  'server.mjs',
  'public',
  'src/server',
  'e2e',
  'data',
  'scripts/seed.mjs',
  'scripts/benchmark.mjs',
  'tests/search.test.mjs',
  'tests/server.test.mjs',
  'tests/store.test.mjs'
];

const missing = requiredFiles.filter((filename) => !fs.existsSync(path.join(root, filename)));
const stale = forbiddenFiles.filter((filename) => fs.existsSync(path.join(root, filename)));
if (missing.length || stale.length) {
  if (missing.length) console.error(`Required native files are missing: ${missing.join(', ')}`);
  if (stale.length) console.error(`Stale browser-harness paths must not return: ${stale.join(', ')}`);
  process.exit(1);
}

const rootPackage = JSON.parse(fs.readFileSync(path.join(root, 'package.json'), 'utf8'));
const nativePackage = JSON.parse(fs.readFileSync(path.join(root, 'native/package.json'), 'utf8'));
const tauri = JSON.parse(fs.readFileSync(path.join(root, 'native/src-tauri/tauri.conf.json'), 'utf8'));
const capability = JSON.parse(fs.readFileSync(path.join(root, 'native/src-tauri/capabilities/default.json'), 'utf8'));
const cargoManifest = fs.readFileSync(path.join(root, 'native/src-tauri/Cargo.toml'), 'utf8');
const viteConfig = fs.readFileSync(path.join(root, 'native/vite.config.ts'), 'utf8');

let failed = false;
function expect(condition, message) {
  if (condition) return;
  failed = true;
  console.error(message);
}

expect(rootPackage.name === 'mux' && nativePackage.name === 'mux', 'Both package manifests must identify the one Mux application.');
expect(rootPackage.version === nativePackage.version && nativePackage.version === tauri.version, 'Root, frontend, and Tauri versions must match.');
expect(rootPackage.scripts?.dev === 'npm --prefix native run dev', 'Root npm run dev must open the native application.');
expect(rootPackage.scripts?.e2e === 'npm --prefix native run test:ui', 'Root npm run e2e must exercise the production Svelte interaction gate.');
expect(rootPackage.scripts?.['test:provider-contract'] === 'npm --prefix native run test:provider-contract', 'The offline provider contract gate must be available from the root package.');
expect(nativePackage.scripts?.['test:provider-contract'] === 'cargo test --manifest-path src-tauri/Cargo.toml provider_conformance -- --nocapture', 'The provider contract command must remain an offline native Rust test gate.');
expect(tauri.build?.beforeDevCommand === 'npm run frontend:dev', 'Tauri development must start only the internal Vite frontend.');
expect(tauri.build?.beforeBuildCommand === 'npm run frontend:build', 'Tauri builds must compile the single Svelte frontend.');
expect(tauri.build?.devUrl === 'http://127.0.0.1:1420', 'The internal Vite development URL must remain loopback-only.');
expect(/host:\s*'127\.0\.0\.1'/u.test(viteConfig) && !viteConfig.includes('TAURI_DEV_HOST'), 'Vite must not accept a non-loopback development host.');
expect(tauri.build?.frontendDist === '../dist', 'Tauri must bundle native/dist.');
expect(tauri.bundle?.targets?.length === 1 && tauri.bundle.targets[0] === 'app', 'The only claimed bundle target is the macOS .app exercised by this project.');
// Keychain items are bound to the code identity that created them, so a stable
// signature is what stops macOS re-prompting after every rebuild. A development
// certificate is enough for that; Developer ID would be a distribution claim
// this project has not earned.
expect(
  tauri.bundle?.macOS?.signingIdentity === '-'
    || /^Apple Development: /u.test(tauri.bundle?.macOS?.signingIdentity ?? ''),
  'The macOS bundle must be ad-hoc signed or signed with an Apple Development certificate.'
);
expect(
  !/Developer ID/u.test(tauri.bundle?.macOS?.signingIdentity ?? ''),
  'This project does not claim Developer ID distribution or notarization.'
);
expect(/^default-run = "mux-native"$/mu.test(cargoManifest), 'Cargo must explicitly bundle mux-native rather than a utility binary.');
expect(tauri.app?.security?.csp?.includes("default-src 'self'"), 'Tauri must retain an explicit self-only default CSP.');
expect(JSON.stringify(capability.permissions) === JSON.stringify(['core:default']), 'The main window capability must not accumulate broad plugin permissions.');

const ignored = new Set(['.git', 'node_modules', 'dist', 'target', 'gen']);
const modules = [];
function walk(directory) {
  for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
    if (ignored.has(entry.name)) continue;
    const filename = path.join(directory, entry.name);
    if (entry.isDirectory()) walk(filename);
    else if (/\.mjs$/u.test(entry.name)) modules.push(filename);
  }
}
walk(root);
for (const filename of modules.sort()) {
  const result = spawnSync(process.execPath, ['--check', filename], { encoding: 'utf8' });
  if (result.status === 0) continue;
  failed = true;
  process.stderr.write(result.stderr || result.stdout);
}

if (failed) process.exit(1);
console.log(`Checked the single native product boundary and ${modules.length} JavaScript modules.`);
