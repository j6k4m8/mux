import assert from 'node:assert/strict';
import { test } from 'vitest';

import {
  accentChoices,
  accentColorFor,
  accentSwatch,
  applyAccentToRoot,
  APPEARANCE_KEY,
  DEFAULT_APPEARANCE,
  isAccentChoice,
  persistAppearance,
  readSavedAppearance
} from './appearance';

test('following the message is the default, and takes the account color', () => {
  assert.equal(DEFAULT_APPEARANCE.accent, 'account');
  assert.equal(accentColorFor('account', 'light', '#12a58c'), '#12a58c');
  assert.equal(accentColorFor('account', 'dark', '#12a58c'), '#12a58c');
  // With nothing selected there is no color to follow, and the stylesheet's
  // own accent is already right for the theme.
  assert.equal(accentColorFor('account', 'light', null), null);
});

test('a fixed accent has a light form and a dark one', () => {
  for (const choice of accentChoices) {
    if (choice.value === 'account') continue;
    const light = accentColorFor(choice.value, 'light', null);
    const dark = accentColorFor(choice.value, 'dark', null);
    assert.match(String(light), /^#[0-9a-f]{6}$/u, choice.value);
    assert.match(String(dark), /^#[0-9a-f]{6}$/u, choice.value);
    assert.notEqual(light, dark, `${choice.value} needs a form for each theme`);
    assert.equal(accentSwatch(choice.value, 'dark'), dark);
  }
  assert.equal(accentSwatch('account', 'light'), '');
});

test('applying an accent sets one property, and clearing it removes it', () => {
  applyAccentToRoot('teal', 'light', null);
  assert.equal(document.documentElement.style.getPropertyValue('--accent'), accentColorFor('teal', 'light', null));
  applyAccentToRoot('account', 'light', null);
  assert.equal(document.documentElement.style.getPropertyValue('--accent'), '');
});

test('a stored accent is read back, and anything else falls to the default', () => {
  assert.ok(isAccentChoice('violet'));
  assert.ok(!isAccentChoice('chartreuse'));
  window.localStorage.setItem(APPEARANCE_KEY, JSON.stringify({ accent: 'rose' }));
  assert.equal(readSavedAppearance().accent, 'rose');
  window.localStorage.setItem(APPEARANCE_KEY, JSON.stringify({ accent: 'chartreuse' }));
  assert.equal(readSavedAppearance().accent, 'account');
});

test('the list body preview starts on, and hiding it survives a round trip', () => {
  assert.equal(DEFAULT_APPEARANCE.listSnippet, true);
  persistAppearance({ ...DEFAULT_APPEARANCE, listSnippet: false });
  assert.equal(readSavedAppearance().listSnippet, false);
  persistAppearance({ ...DEFAULT_APPEARANCE, listSnippet: true });
  assert.equal(readSavedAppearance().listSnippet, true);
});

test('a stored list preview that is not a boolean shows the preview', () => {
  window.localStorage.setItem(APPEARANCE_KEY, JSON.stringify({ listSnippet: 'yes please' }));
  const read = readSavedAppearance();
  assert.equal(read.listSnippet, true);
  // The two previews are separate settings and must not be read from each other.
  window.localStorage.setItem(APPEARANCE_KEY, JSON.stringify({ railPreview: false }));
  assert.equal(readSavedAppearance().listSnippet, true);
});
