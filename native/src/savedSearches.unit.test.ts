import assert from 'node:assert/strict';
import { test } from 'vitest';

import {
  addSavedSearch,
  persistSavedSearches,
  readSavedSearches,
  removeSavedSearch,
  SAVED_SEARCHES_KEY
} from './savedSearches';

test('saving keeps the newest first and never duplicates a query', () => {
  let saved = addSavedSearch([], 'is:unread');
  saved = addSavedSearch(saved, 'category:Finance', 'Money');
  saved = addSavedSearch(saved, 'is:unread', 'Unread');
  assert.deepEqual(saved, [
    { name: 'Unread', query: 'is:unread' },
    { name: 'Money', query: 'category:Finance' }
  ]);
  assert.deepEqual(removeSavedSearch(saved, 'is:unread'), [{ name: 'Money', query: 'category:Finance' }]);
  // An empty query is not a search.
  assert.deepEqual(addSavedSearch([], '   '), []);
});

test('a saved search survives a round trip through storage', () => {
  persistSavedSearches(addSavedSearch([], 'from:alice', 'Alice'));
  assert.deepEqual(readSavedSearches(), [{ name: 'Alice', query: 'from:alice' }]);
});

test('storage is untrusted: anything unusable is dropped rather than shown', () => {
  window.localStorage.setItem(SAVED_SEARCHES_KEY, 'not json');
  assert.deepEqual(readSavedSearches(), []);
  window.localStorage.setItem(SAVED_SEARCHES_KEY, JSON.stringify({ query: 'is:unread' }));
  assert.deepEqual(readSavedSearches(), []);
  window.localStorage.setItem(SAVED_SEARCHES_KEY, JSON.stringify([
    { query: 'is:unread' },
    null,
    { name: 'no query' },
    { name: 'dupe', query: 'is:unread' },
    { name: 42, query: 'from:alice' }
  ]));
  assert.deepEqual(readSavedSearches(), [
    { name: 'is:unread', query: 'is:unread' },
    { name: 'from:alice', query: 'from:alice' }
  ]);
});

test('a hostile length is trimmed on the way in and on the way out', () => {
  const long = 'x'.repeat(4_000);
  const saved = addSavedSearch([], long, long);
  assert.equal(saved[0].query.length, 512);
  assert.equal(saved[0].name.length, 80);
  window.localStorage.setItem(SAVED_SEARCHES_KEY, JSON.stringify(
    Array.from({ length: 60 }, (_unused, index) => ({ name: `n${index}`, query: `q${index}` }))
  ));
  assert.equal(readSavedSearches().length, 24);
});
