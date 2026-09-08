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

test('a folder is offered only under the account it belongs to', () => {
  const containers = [
    { accountId: 'acc_work', remoteId: 'Label_17', name: 'Zoomie Cycle', kind: 'label' as const, role: 'custom', unread: 1, total: 3 },
    { accountId: 'acc_home', remoteId: 'INBOX/Bills', name: 'Bills', kind: 'folder' as const, role: 'custom', unread: 0, total: 9 }
  ];
  const rows = mailboxJumpRows({ accounts, containers, selectedAccount: 'acc_work' });
  const folders = rows.filter((row) => row.target.kind === 'container');
  assert.deepEqual(folders.map((row) => row.id), [
    'folder:acc_work:Label_17',
    'folder:acc_home:INBOX/Bills'
  ]);
  // Under All accounts there is no account to open it in, so it is not offered.
  assert.ok(folders.every((row) => !row.id.startsWith('folder:all')));
  assert.deepEqual(folders[0].target, { kind: 'container', accountId: 'acc_work', remoteId: 'Label_17' });
  assert.equal(folders[0].subtitle, 'Work · jordan@acme.example');
  assert.equal(folders[0].priority, true);
  assert.equal(folders[1].priority, false);
});

test('a saved search is offered under every scope, after the views, carrying the query it runs', () => {
  const saved = [
    { name: 'Alice', query: 'from:alice' },
    { name: 'Alice', query: 'from:alice has:attachment' }
  ];
  const rows = mailboxJumpRows({ accounts, saved, selectedAccount: 'acc_home' });
  const alice = rows.filter((row) => row.target.kind === 'saved' && row.target.query === 'from:alice');
  assert.deepEqual(
    alice.map((row) => (row.target.kind === 'saved' ? row.target.accountId : undefined)),
    [null, 'acc_work', 'acc_home']
  );
  assert.deepEqual(alice[2].target, { kind: 'saved', name: 'Alice', query: 'from:alice', accountId: 'acc_home' });
  assert.equal(alice[2].title, 'Alice');
  assert.equal(alice[2].subtitle, 'Home · jordan@home.example');
  assert.equal(alice[2].chip, 'from:alice');
  assert.equal(alice[2].priority, true);
  assert.equal(alice[0].priority, false);
  // Two searches may share a name; their rows are still told apart.
  assert.equal(new Set(rows.map((row) => row.id)).size, rows.length);
  // Within a scope the fixed views and smart views come first; the reader's own
  // searches follow them, and the next scope starts after.
  const ids = rows.map((row) => row.id);
  assert.ok(ids.indexOf('smart:all:finance') < ids.indexOf('saved:all:from:alice'));
  assert.ok(ids.indexOf('saved:all:from:alice') < ids.indexOf('view:acc_work:inbox'));
  // With nothing saved, nothing is offered.
  assert.ok(mailboxJumpRows({ accounts, selectedAccount: null }).every((row) => row.target.kind !== 'saved'));
});

test('the scoped list keeps the saved searches under that one account', () => {
  const saved = [{ name: 'Alice', query: 'from:alice' }];
  const rows = mailboxJumpRows({ accounts, saved, selectedAccount: 'acc_work', scoped: true, scopeAccountId: 'acc_work' });
  assert.deepEqual(
    rows.filter((row) => row.target.kind === 'saved').map((row) => row.target),
    [{ kind: 'saved', name: 'Alice', query: 'from:alice', accountId: 'acc_work' }]
  );
  assert.ok(rows.every((row) => row.subtitle === 'Work · jordan@acme.example'));
});

test('the scoped list keeps that account folders and no others', () => {
  const containers = [
    { accountId: 'acc_work', remoteId: 'Label_17', name: 'Zoomie Cycle', kind: 'label' as const, role: 'custom', unread: 1, total: 3 },
    { accountId: 'acc_home', remoteId: 'INBOX/Bills', name: 'Bills', kind: 'folder' as const, role: 'custom', unread: 0, total: 9 }
  ];
  const rows = mailboxJumpRows({
    accounts,
    containers,
    selectedAccount: 'acc_work',
    scoped: true,
    scopeAccountId: 'acc_work'
  });
  assert.deepEqual(
    rows.filter((row) => row.target.kind === 'container').map((row) => row.title),
    ['Zoomie Cycle']
  );
});
