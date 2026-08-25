import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';

const root = path.resolve(import.meta.dirname, '..');
const bundle = path.join(root, 'native/src-tauri/target/release/bundle/macos/Mux.app');
const executables = path.join(bundle, 'Contents/MacOS');
const infoPlist = path.join(bundle, 'Contents/Info.plist');

if (!fs.existsSync(infoPlist) || !fs.existsSync(executables)) {
  console.error(`The expected macOS application bundle is missing: ${bundle}`);
  process.exit(1);
}

const entries = fs.readdirSync(executables, { withFileTypes: true })
  .filter((entry) => entry.isFile())
  .map((entry) => entry.name)
  .sort();
if (entries.length !== 1 || entries[0] !== 'mux-native') {
  console.error(`Mux.app must contain only the product executable; found: ${entries.join(', ') || '(none)'}`);
  process.exit(1);
}

const productExecutable = path.join(executables, 'mux-native');
if (!(fs.statSync(productExecutable).mode & 0o111)) {
  console.error('Mux.app product executable is not executable.');
  process.exit(1);
}

const plist = fs.readFileSync(infoPlist, 'utf8');
if (!/<key>CFBundleExecutable<\/key>\s*<string>mux-native<\/string>/u.test(plist)) {
  console.error('Mux.app Info.plist does not name mux-native as CFBundleExecutable.');
  process.exit(1);
}

const signature = spawnSync(
  '/usr/bin/codesign',
  ['--verify', '--deep', '--strict', '--verbose=4', bundle],
  { encoding: 'utf8' }
);
if (signature.status !== 0) {
  console.error('Mux.app does not have a structurally valid macOS signature.');
  process.stderr.write(signature.stderr || signature.stdout);
  process.exit(1);
}

const signatureDetails = spawnSync(
  '/usr/bin/codesign',
  ['--display', '--verbose=4', bundle],
  { encoding: 'utf8' }
);
const signatureOutput = `${signatureDetails.stdout || ''}\n${signatureDetails.stderr || ''}`;
if (
  signatureDetails.status !== 0 ||
  !signatureOutput.includes('Signature=adhoc') ||
  !/flags=.*\badhoc\b/u.test(signatureOutput)
) {
  console.error('Mux.app is validly signed, but not with the required explicit ad-hoc identity.');
  process.stderr.write(signatureOutput);
  process.exit(1);
}

console.log('Checked Mux.app: one executable, mux-native, matches Info.plist, explicit ad-hoc signature verified.');
