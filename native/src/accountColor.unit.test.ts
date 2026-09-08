import assert from 'node:assert/strict';
import { test } from 'vitest';

import { accountColorChoices, describeAccountColor, normalizeAccountColor } from './accountColor';

test('a six-digit hex colour is accepted and lowercased', () => {
  assert.equal(normalizeAccountColor('#AbCdEf'), '#abcdef');
  assert.equal(normalizeAccountColor('#5168f4'), '#5168f4');
});

test('anything that is not exactly #rrggbb is refused', () => {
  const refused: unknown[] = [
    '#abc', 'red', '#abcdefg', 'javascript:', ' #abcdef', '#abcdef ', 'abcdef', '#abcdeg',
    '#abcdef;background:url(x)', '', null, undefined, 0xabcdef, { toString: () => '#abcdef' }
  ];
  for (const value of refused) assert.equal(normalizeAccountColor(value), null, String(value));
});

test('the offered colours are valid, distinct, and named', () => {
  assert.equal(new Set(accountColorChoices.map((choice) => choice.value)).size, 6);
  for (const choice of accountColorChoices) {
    assert.equal(normalizeAccountColor(choice.value), choice.value, choice.label);
    assert.equal(describeAccountColor(choice.value), choice.label.toLocaleLowerCase());
  }
  // A colour Settings does not offer is described by its value.
  assert.equal(describeAccountColor('#abcdef'), '#abcdef');
});
