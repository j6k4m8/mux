import crypto from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';

const root = path.resolve(import.meta.dirname, '..');
const manifestPath = path.join(root, 'MANIFEST.sha256');
const excludedDirectories = new Set([
  '.git',
  'node_modules',
  'data',
  'artifacts',
  'dist',
  'target'
]);
const excludedFiles = new Set(['MANIFEST.sha256', '.DS_Store']);

function walk(directory, output = []) {
  for (const entry of fs.readdirSync(directory, { withFileTypes: true })) {
    if (entry.isDirectory() && excludedDirectories.has(entry.name)) continue;
    const absolute = path.join(directory, entry.name);
    const relative = path.relative(root, absolute).split(path.sep).join('/');
    if (entry.isDirectory()) walk(absolute, output);
    else if (
      !excludedFiles.has(entry.name)
      && !relative.endsWith('.zip')
      && !relative.endsWith('.zip.sha256')
    ) output.push(relative);
  }
  return output;
}

function digest(filename) {
  return crypto.createHash('sha256').update(fs.readFileSync(filename)).digest('hex');
}

function expectedEntries() {
  return walk(root).sort().map((relative) => ({ relative, hash: digest(path.join(root, relative)) }));
}

function writeManifest() {
  const lines = expectedEntries().map(({ hash, relative }) => `${hash}  ${relative}`);
  fs.writeFileSync(manifestPath, `${lines.join('\n')}\n`);
  console.log(`Wrote ${lines.length} entries to MANIFEST.sha256.`);
}

function verifyManifest() {
  if (!fs.existsSync(manifestPath)) throw new Error('MANIFEST.sha256 is missing');
  const actual = new Map();
  for (const line of fs.readFileSync(manifestPath, 'utf8').split(/\r?\n/).filter(Boolean)) {
    const match = /^([a-f0-9]{64})  (.+)$/.exec(line);
    if (!match) throw new Error(`Malformed manifest line: ${line}`);
    if (actual.has(match[2])) throw new Error(`Duplicate manifest entry: ${match[2]}`);
    actual.set(match[2], match[1]);
  }

  const expected = expectedEntries();
  const expectedNames = new Set(expected.map(({ relative }) => relative));
  const missing = expected.filter(({ relative }) => !actual.has(relative)).map(({ relative }) => relative);
  const extra = [...actual.keys()].filter((relative) => !expectedNames.has(relative));
  if (missing.length || extra.length) {
    throw new Error(`Manifest file set mismatch; missing=[${missing.join(', ')}], extra=[${extra.join(', ')}]`);
  }
  for (const { relative, hash } of expected) {
    if (actual.get(relative) !== hash) throw new Error(`Manifest hash mismatch: ${relative}`);
  }
  console.log(`Verified ${expected.length} manifest entries.`);
}

if (process.argv.includes('--write')) writeManifest();
else verifyManifest();
