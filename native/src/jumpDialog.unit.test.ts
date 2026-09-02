import assert from 'node:assert/strict';
import { test } from 'vitest';

import { mailboxJumpRows } from './jumpDialog';
import type { AccountSummary } from './types';

const accounts: AccountSummary[] = [
  { id: 'acc_work', name: 'Work', email: 'jordan@acme.example', color: '#5168f4', signature: '', unread: 1, total: 2, refreshSeconds: 60 },
  { id: 'acc_home', name: 'Home', email: 'jordan@home.example', color: '#12a58c', signature: '', unread: 0, total: 4, refreshSeconds: 60 }
];

test('every folder in every account is reachable, and so is every account', () => {
  const rows = mailboxJumpRows({ accounts, selectedAccount: null });
  const inboxes = rows.filter((row) => row.target.kind === 'view' && row.target.view === 'inbox');
  assert.deepEqual(
    inboxes.map((row) => (row.target.kind === 'view' ? row.target.accountId : null)),
    [null, 'acc_work', 'acc_home']
  );
  assert.deepEqual(
    rows.filter((row) => row.target.kind === 'account').map((row) => row.title),
    ['All accounts', 'Work', 'Home']
  );
  // Smart views are destinations too, and carry the query that defines them.
  const unread = rows.find((row) => row.id === 'smart:acc_work:unread');
  assert.deepEqual(unread?.target, { kind: 'smart', smart: 'unread', query: 'is:unread', accountId: 'acc_work' });
  assert.equal(unread?.subtitle, 'Work · jordan@acme.example');
});

test('rows for the account being read are marked so they sort first', () => {
  const rows = mailboxJumpRows({ accounts, selectedAccount: 'acc_home' });
  assert.ok(rows.filter((row) => row.id.includes('acc_home')).every((row) => row.priority));
  assert.ok(rows.filter((row) => row.id.includes('acc_work')).every((row) => !row.priority));
});

test('the scoped list is one account and nothing else', () => {
  const rows = mailboxJumpRows({ accounts, selectedAccount: 'acc_work', scoped: true, scopeAccountId: 'acc_work' });
  assert.ok(rows.every((row) => row.target.kind !== 'account'));
  assert.ok(rows.every((row) => row.subtitle === 'Work · jordan@acme.example'));
  assert.equal(rows.length, 12);
});

test('scoping to nothing in particular scopes to every account together', () => {
  const rows = mailboxJumpRows({ accounts, selectedAccount: null, scoped: true, scopeAccountId: null });
  assert.ok(rows.every((row) => row.subtitle === 'All accounts'));
});
