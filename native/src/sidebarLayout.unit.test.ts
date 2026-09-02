import assert from 'node:assert/strict';
import { test } from 'vitest';

import {
  DEFAULT_SIDEBAR_LAYOUT,
  folderSectionIsOpen,
  persistSidebarLayout,
  readSidebarLayout,
  SIDEBAR_KEY,
  toggleFolderAccount
} from './sidebarLayout';

test('the rail starts wide with every folder section folded away', () => {
  assert.deepEqual(DEFAULT_SIDEBAR_LAYOUT, { collapsed: false, openFolderAccounts: [] });
  assert.deepEqual(readSidebarLayout(), DEFAULT_SIDEBAR_LAYOUT);
});

test('an arrangement survives a round trip through storage', () => {
  persistSidebarLayout({ collapsed: true, openFolderAccounts: ['acc_work'] });
  assert.deepEqual(readSidebarLayout(), { collapsed: true, openFolderAccounts: ['acc_work'] });
});

test('storage is untrusted: anything unusable falls back rather than showing up', () => {
  window.localStorage.setItem(SIDEBAR_KEY, 'not json');
  assert.deepEqual(readSidebarLayout(), DEFAULT_SIDEBAR_LAYOUT);
  window.localStorage.setItem(SIDEBAR_KEY, JSON.stringify({ collapsed: 'yes', openFolderAccounts: 'acc' }));
  assert.deepEqual(readSidebarLayout(), DEFAULT_SIDEBAR_LAYOUT);
  window.localStorage.setItem(SIDEBAR_KEY, JSON.stringify({
    collapsed: true,
    openFolderAccounts: ['acc_work', '', 42, 'acc_work', 'x'.repeat(400), 'acc_home']
  }));
  assert.deepEqual(readSidebarLayout(), { collapsed: true, openFolderAccounts: ['acc_work', 'acc_home'] });
  window.localStorage.setItem(SIDEBAR_KEY, JSON.stringify({
    openFolderAccounts: Array.from({ length: 80 }, (_unused, index) => `acc_${index}`)
  }));
  assert.equal(readSidebarLayout().openFolderAccounts.length, 64);
});

test('a section opens and folds back, and the rest are left alone', () => {
  let layout = toggleFolderAccount(DEFAULT_SIDEBAR_LAYOUT, 'acc_work');
  assert.deepEqual(layout.openFolderAccounts, ['acc_work']);
  layout = toggleFolderAccount(layout, 'acc_home');
  assert.deepEqual(layout.openFolderAccounts, ['acc_work', 'acc_home']);
  layout = toggleFolderAccount(layout, 'acc_work');
  assert.deepEqual(layout.openFolderAccounts, ['acc_home']);
});

test('the section holding the folder being read is open regardless', () => {
  const folded = DEFAULT_SIDEBAR_LAYOUT.openFolderAccounts;
  assert.equal(folderSectionIsOpen(folded, 'acc_work', null), false);
  assert.equal(folderSectionIsOpen(folded, 'acc_work', 'acc_work'), true);
  assert.equal(folderSectionIsOpen(folded, 'acc_home', 'acc_work'), false);
  const opened = toggleFolderAccount(DEFAULT_SIDEBAR_LAYOUT, 'acc_home').openFolderAccounts;
  assert.equal(folderSectionIsOpen(opened, 'acc_home', null), true);
});
