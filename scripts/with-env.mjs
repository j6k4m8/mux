// Loads .env from the repository root into the environment, then runs the given
// command. The Rust process needs MUX_GOOGLE_OAUTH_CLIENT_CONFIG at launch, and
// Vite's own .env handling only reaches the frontend.
import fs from 'node:fs';
import path from 'node:path';
import { spawn } from 'node:child_process';

const root = path.resolve(import.meta.dirname, '..');
const envFile = path.join(root, '.env');
if (fs.existsSync(envFile)) {
  for (const line of fs.readFileSync(envFile, 'utf8').split('\n')) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith('#')) continue;
    const separator = trimmed.indexOf('=');
    if (separator < 1) continue;
    const key = trimmed.slice(0, separator).trim();
    // An existing value wins, so one-off overrides on the command line still work.
    if (process.env[key] !== undefined) continue;
    process.env[key] = trimmed.slice(separator + 1).trim();
  }
}

const [command, ...args] = process.argv.slice(2);
if (!command) {
  console.error('with-env.mjs needs a command to run.');
  process.exit(1);
}
const child = spawn(command, args, { stdio: 'inherit', shell: false });
child.on('exit', (code, signal) => process.exit(signal ? 1 : (code ?? 0)));
