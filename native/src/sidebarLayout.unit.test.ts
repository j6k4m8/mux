import assert from 'node:assert/strict';
import { test } from 'vitest';

import {
  DEFAULT_SIDEBAR_LAYOUT,
  folderSectionIsOpen,
  persistSidebarLayout,
  readSidebarLayout,
  SIDEBAR_KEY,
  smartViewsSectionIsOpen,
  toggleFolderAccount,
  toggleSmartViews
} from './sidebarLayout';

test('the rail starts wide with the smart views out and every folder section folded away', () => {
  assert.deepEqual(DEFAULT_SIDEBAR_LAYOUT, { collapsed: false, openFolderAccounts: [], smartViewsOpen: true });
  assert.deepEqual(readSidebarLayout(), DEFAULT_SIDEBAR_LAYOUT);
});

test('an arrangement survives a round trip through storage', () => {
  persistSidebarLayout({ collapsed: true, openFolderAccounts: ['acc_work'], smartViewsOpen: false });
  assert.deepEqual(readSidebarLayout(), { collapsed: true, openFolderAccounts: ['acc_work'], smartViewsOpen: false });
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
  assert.deepEqual(readSidebarLayout(), { collapsed: true, openFolderAccounts: ['acc_work', 'acc_home'], smartViewsOpen: true });
  window.localStorage.setItem(SIDEBAR_KEY, JSON.stringify({
    openFolderAccounts: Array.from({ length: 80 }, (_unused, index) => `acc_${index}`)
  }));
  assert.equal(readSidebarLayout().openFolderAccounts.length, 64);
});

test('smart views are folded only by a stored false: a layout from before they could fold shows them', () => {
  window.localStorage.setItem(SIDEBAR_KEY, JSON.stringify({ collapsed: false, openFolderAccounts: [] }));
  assert.equal(readSidebarLayout().smartViewsOpen, true);
  window.localStorage.setItem(SIDEBAR_KEY, JSON.stringify({ smartViewsOpen: false }));
  assert.equal(readSidebarLayout().smartViewsOpen, false);
  for (const garbage of ['false', 0, null, 'no', [], {}]) {
    window.localStorage.setItem(SIDEBAR_KEY, JSON.stringify({ smartViewsOpen: garbage }));
    assert.equal(readSidebarLayout().smartViewsOpen, true, `${JSON.stringify(garbage)} should read as open`);
  }
});

test('the smart views fold away and come back, leaving the rest alone', () => {
  const folded = toggleSmartViews({ ...DEFAULT_SIDEBAR_LAYOUT, openFolderAccounts: ['acc_work'] });
  assert.deepEqual(folded, { collapsed: false, openFolderAccounts: ['acc_work'], smartViewsOpen: false });
  assert.equal(toggleSmartViews(folded).smartViewsOpen, true);
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

test('the smart view being read keeps its section out regardless', () => {
  assert.equal(smartViewsSectionIsOpen(false, ''), false);
  assert.equal(smartViewsSectionIsOpen(false, 'unread'), true);
  assert.equal(smartViewsSectionIsOpen(true, ''), true);
});
