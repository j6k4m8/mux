import assert from 'node:assert/strict';
import { test } from 'vitest';

import { fuzzyMatches, fuzzyScore, rankByFuzzy } from './fuzzy';

test('matching is a subsequence, not a substring', () => {
  assert.ok(fuzzyMatches('inwk', 'Inbox · work@example.com'));
  assert.ok(fuzzyMatches('', 'anything'));
  assert.ok(!fuzzyMatches('zz', 'Inbox'));
  assert.ok(!fuzzyMatches('xobni', 'Inbox'));
});

test('a closer match scores higher than a scattered one', () => {
  const tight = fuzzyScore('inbox', 'Inbox');
  const scattered = fuzzyScore('inbox', 'Invitations before the box arrives');
  assert.ok(tight !== null && scattered !== null);
  assert.ok(tight > scattered);
});

test('ranking puts the obvious answer first and keeps ties in the given order', () => {
  const rows = [
    { id: 'all', title: 'All mail' },
    { id: 'archive', title: 'Archive' },
    { id: 'inbox', title: 'Inbox' }
  ];
  assert.deepEqual(rankByFuzzy('inb', rows, (row) => row.title).map((row) => row.id), ['inbox']);
  assert.deepEqual(rankByFuzzy('a', rows, (row) => row.title).map((row) => row.id), ['all', 'archive']);
  assert.deepEqual(rankByFuzzy('', rows, (row) => row.title).map((row) => row.id), ['all', 'archive', 'inbox']);
});

test('a row can be found by either half of how it reads', () => {
  const rows = [{ id: 'archive', title: 'Archive', subtitle: 'Work · jordan@acme.example' }];
  const texts = (row: (typeof rows)[number]) => [`${row.title} ${row.subtitle}`, `${row.subtitle} ${row.title}`];
  assert.deepEqual(rankByFuzzy('archwork', rows, texts).map((row) => row.id), ['archive']);
  assert.deepEqual(rankByFuzzy('workarch', rows, texts).map((row) => row.id), ['archive']);
});

test('an equal match prefers the account being read', () => {
  const rows = [
    { id: 'other', title: 'Inbox', mine: false },
    { id: 'mine', title: 'Inbox', mine: true }
  ];
  assert.deepEqual(
    rankByFuzzy('inbox', rows, (row) => row.title, (row) => row.mine).map((row) => row.id),
    ['mine', 'other']
  );
});
