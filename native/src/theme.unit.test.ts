import assert from 'node:assert/strict';
import { test } from 'vitest';

import {
  isThemePreference,
  persistThemePreference,
  readThemePreference,
  resolveTheme,
  THEME_KEY,
  themeChoices
} from './theme';

test('what was chosen and what is on screen are not the same question', () => {
  assert.equal(resolveTheme('light', true), 'light');
  assert.equal(resolveTheme('dark', false), 'dark');
  assert.equal(resolveTheme('system', true), 'dark');
  assert.equal(resolveTheme('system', false), 'light');
});

test('every offered choice is a choice, and nothing else is', () => {
  for (const choice of themeChoices) assert.ok(isThemePreference(choice.value), choice.value);
  assert.ok(!isThemePreference('sepia'));
  assert.ok(!isThemePreference(null));
});

test('a choice survives storage, and an older Mux stored a valid one', () => {
  persistThemePreference('dark');
  assert.equal(readThemePreference(), 'dark');
  // Mux only ever wrote 'light' or 'dark' here before there was a third option.
  window.localStorage.setItem(THEME_KEY, 'light');
  assert.equal(readThemePreference(), 'light');
  window.localStorage.setItem(THEME_KEY, 'sepia');
  assert.equal(readThemePreference(), 'system');
  window.localStorage.removeItem(THEME_KEY);
  assert.equal(readThemePreference(), 'system');
});
