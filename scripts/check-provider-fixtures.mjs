import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const root = path.resolve(import.meta.dirname, '..');
const ignored = new Set(['.git', '.vite', 'bundle', 'dist', 'gen', 'node_modules', 'target']);
const MAX_SCANNED_FILE_BYTES = 10 * 1024 * 1024;
const credentialFilenames = /(?:^|[._-])(credentials?|oauth|refresh[._-]?token|secrets?|token)(?:[._-]|$)/iu;
const secretShapes = [
  ['private key', /-----BEGIN (?:EC |OPENSSH |RSA )?PRIVATE KEY-----/u],
  ['AWS access key', /\bAKIA[0-9A-Z]{16}\b/u],
  ['OAuth access token', /\bya29\.[A-Za-z0-9_-]{20,}\b/u],
  ['OAuth refresh token', /\b1\/\/[A-Za-z0-9_-]{20,}\b/u],
  ['Google OAuth client value', /\bGOCSPX-[A-Za-z0-9_-]{20,}\b/u],
  ['GitHub token', /\bgh[pousr]_[A-Za-z0-9]{30,}\b/u],
  ['Slack token', /\bxox[baprs]-[A-Za-z0-9-]{20,}\b/u],
  ['Tapestry token', /\btap_(?:dlg_)?[A-Za-z0-9_-]{24,}\b/u],
  ['JWT', /\beyJ[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]{20,}\.[A-Za-z0-9_-]{20,}\b/u]
];

function inspect(filename, scanRoot, failures) {
  const relative = path.relative(scanRoot, filename);
  if (credentialFilenames.test(path.basename(filename))) {
    failures.push(`${relative}: credential-shaped filename`);
  }
  const stat = fs.statSync(filename);
  if (stat.size > MAX_SCANNED_FILE_BYTES) {
    failures.push(`${relative}: fixture/source file exceeds the 10 MiB scanner limit`);
    return;
  }
  const value = fs.readFileSync(filename).toString('utf8');
  for (const [label, pattern] of secretShapes) {
    if (pattern.test(value)) failures.push(`${relative}: contains a ${label} shape`);
  }
}

function walk(directory, scanRoot, failures) {
  if (!fs.existsSync(directory)) return;
  for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
    if (ignored.has(entry.name)) continue;
    const filename = path.join(directory, entry.name);
    if (entry.isDirectory()) walk(filename, scanRoot, failures);
    else if (entry.isFile()) inspect(filename, scanRoot, failures);
  }
}

function scan(directory) {
  const failures = [];
  walk(directory, directory, failures);
  return failures;
}

function assertScannerRejectsSecretEnv() {
  const temporary = fs.mkdtempSync(path.join(os.tmpdir(), 'mux-provider-fixture-guard-'));
  try {
    const fixtureDirectory = path.join(temporary, 'native', 'tests');
    fs.mkdirSync(fixtureDirectory, { recursive: true });
    const syntheticSecret = ['ya29.', 'A'.repeat(24)].join('');
    const syntheticGoogleClientValue = ['GOCSPX-', 'B'.repeat(24)].join('');
    fs.writeFileSync(
      path.join(fixtureDirectory, 'provider.env'),
      `ACCESS_TOKEN=${syntheticSecret}\nCLIENT_VALUE=${syntheticGoogleClientValue}\n`
    );
    fs.writeFileSync(path.join(temporary, 'codex-to-delete-when-added.token.tapestry'), '');
    const failures = scan(temporary);
    if (!failures.some((failure) => failure.includes('OAuth access token'))) {
      throw new Error('Provider fixture/source guard self-test did not reject a secret-shaped .env file');
    }
    if (!failures.some((failure) => failure.includes('Google OAuth client value'))) {
      throw new Error('Provider fixture/source guard self-test did not reject a Google client value');
    }
    if (!failures.some((failure) => failure.includes('codex-to-delete-when-added.token.tapestry: credential-shaped filename'))) {
      throw new Error('Provider fixture/source guard self-test did not reject a generic token filename');
    }
  } finally {
    fs.rmSync(temporary, { recursive: true, force: true });
  }
}

assertScannerRejectsSecretEnv();
const failures = scan(root);

if (failures.length) {
  console.error('Provider fixture/source guard rejected secret-shaped test data:');
  for (const failure of failures) console.error(`- ${failure}`);
  console.error('Use deterministic example.test/example.invalid values and synthetic tokens.');
  process.exit(1);
}

console.log('Provider fixture/source guard found no secret-shaped test data.');
