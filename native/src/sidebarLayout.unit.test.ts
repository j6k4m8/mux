import assert from 'node:assert/strict';
import { test } from 'vitest';

import {
  DEFAULT_SIDEBAR_LAYOUT,
  folderSectionIsOpen,
  persistSidebarLayout,
  readSidebarLayout,
  savedSearchesSectionIsOpen,
  SIDEBAR_KEY,
  smartViewsSectionIsOpen,
  toggleFolderAccount,
  toggleSavedSearches,
  toggleSmartViews
} from './sidebarLayout';

test('the rail starts wide with the smart views and saved searches out and every folder section folded away', () => {
  assert.deepEqual(DEFAULT_SIDEBAR_LAYOUT, {
    collapsed: false,
    openFolderAccounts: [],
    smartViewsOpen: true,
    savedSearchesOpen: true
  });
  assert.deepEqual(readSidebarLayout(), DEFAULT_SIDEBAR_LAYOUT);
});

test('an arrangement survives a round trip through storage', () => {
  const arrangement = { collapsed: true, openFolderAccounts: ['acc_work'], smartViewsOpen: false, savedSearchesOpen: false };
  persistSidebarLayout(arrangement);
  assert.deepEqual(readSidebarLayout(), arrangement);
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
  assert.deepEqual(readSidebarLayout(), {
    collapsed: true,
    openFolderAccounts: ['acc_work', 'acc_home'],
    smartViewsOpen: true,
    savedSearchesOpen: true
  });
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

test('saved searches are folded only by a stored false: a layout from before they had a section shows them', () => {
  window.localStorage.setItem(SIDEBAR_KEY, JSON.stringify({ collapsed: false, openFolderAccounts: [], smartViewsOpen: false }));
  assert.equal(readSidebarLayout().savedSearchesOpen, true);
  window.localStorage.setItem(SIDEBAR_KEY, JSON.stringify({ savedSearchesOpen: false }));
  assert.equal(readSidebarLayout().savedSearchesOpen, false);
  for (const garbage of ['false', 0, null, 'no', [], {}]) {
    window.localStorage.setItem(SIDEBAR_KEY, JSON.stringify({ savedSearchesOpen: garbage }));
    assert.equal(readSidebarLayout().savedSearchesOpen, true, `${JSON.stringify(garbage)} should read as open`);
  }
});

test('the smart views fold away and come back, leaving the rest alone', () => {
  const folded = toggleSmartViews({ ...DEFAULT_SIDEBAR_LAYOUT, openFolderAccounts: ['acc_work'] }, true);
  assert.deepEqual(folded, { collapsed: false, openFolderAccounts: ['acc_work'], smartViewsOpen: false, savedSearchesOpen: true });
  assert.equal(toggleSmartViews(folded, false).smartViewsOpen, true);
});

test('the saved searches fold away and come back, leaving the rest alone', () => {
  const folded = toggleSavedSearches({ ...DEFAULT_SIDEBAR_LAYOUT, smartViewsOpen: false }, true);
  assert.deepEqual(folded, { collapsed: false, openFolderAccounts: [], smartViewsOpen: false, savedSearchesOpen: false });
  assert.equal(toggleSavedSearches(folded, false).savedSearchesOpen, true);
});

test('a click always folds or shows what the reader can see, not the raw stored bit', () => {
  // A section forced open by an active selection reads as shown even when the
  // stored preference is folded — clicking it must fold the preference, not
  // flip it further open just because the stored bit itself said "folded".
  const forcedOpen = toggleSmartViews({ ...DEFAULT_SIDEBAR_LAYOUT, smartViewsOpen: false }, true);
  assert.equal(forcedOpen.smartViewsOpen, false);
  // Clicking again while it is still forced open is a no-op on the stored bit:
  // there is nothing further to fold, so the preference does not oscillate.
  assert.equal(toggleSmartViews(forcedOpen, true).smartViewsOpen, false);
  const forcedSavedOpen = toggleSavedSearches({ ...DEFAULT_SIDEBAR_LAYOUT, savedSearchesOpen: false }, true);
  assert.equal(forcedSavedOpen.savedSearchesOpen, false);
});

test('a section opens and folds back, and the rest are left alone', () => {
  let layout = toggleFolderAccount(DEFAULT_SIDEBAR_LAYOUT, 'acc_work', false);
  assert.deepEqual(layout.openFolderAccounts, ['acc_work']);
  layout = toggleFolderAccount(layout, 'acc_home', false);
  assert.deepEqual(layout.openFolderAccounts, ['acc_work', 'acc_home']);
  layout = toggleFolderAccount(layout, 'acc_work', true);
  assert.deepEqual(layout.openFolderAccounts, ['acc_home']);
});

test('folding a section forced open by the folder being read stores the fold, not another open', () => {
  // The section is shown (forced by openFolderAccountId elsewhere), but its
  // own stored bit is already folded — clicking must not flip that bit open.
  const layout = toggleFolderAccount(DEFAULT_SIDEBAR_LAYOUT, 'acc_work', true);
  assert.deepEqual(layout.openFolderAccounts, []);
});

test('the section holding the folder being read is open regardless', () => {
  const folded = DEFAULT_SIDEBAR_LAYOUT.openFolderAccounts;
  assert.equal(folderSectionIsOpen(folded, 'acc_work', null), false);
  assert.equal(folderSectionIsOpen(folded, 'acc_work', 'acc_work'), true);
  assert.equal(folderSectionIsOpen(folded, 'acc_home', 'acc_work'), false);
  const opened = toggleFolderAccount(DEFAULT_SIDEBAR_LAYOUT, 'acc_home', false).openFolderAccounts;
  assert.equal(folderSectionIsOpen(opened, 'acc_home', null), true);
});

test('the smart view being read keeps its section out regardless', () => {
  assert.equal(smartViewsSectionIsOpen(false, ''), false);
  assert.equal(smartViewsSectionIsOpen(false, 'unread'), true);
  assert.equal(smartViewsSectionIsOpen(true, ''), true);
});

test('the saved search being run keeps its section out regardless', () => {
  assert.equal(savedSearchesSectionIsOpen(false, ''), false);
  assert.equal(savedSearchesSectionIsOpen(false, 'from:alice'), true);
  assert.equal(savedSearchesSectionIsOpen(true, ''), true);
});
