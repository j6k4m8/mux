import assert from 'node:assert/strict';
import { test } from 'vitest';

import {
  chordShortcutFor,
  isInteractiveShortcutTarget,
  mailboxShortcutFor,
  shortcutCatalog,
  shortcutChips,
  shortcutLabel
} from './shortcuts';
import type { ShortcutEvent } from './shortcuts';

function event(key: string, target: unknown = null, modifiers: Partial<ShortcutEvent> = {}): ShortcutEvent {
  return { key, target, metaKey: false, ctrlKey: false, altKey: false, shiftKey: false, ...modifiers };
}

test('native mailbox shortcuts never capture Tab', () => {
  assert.equal(mailboxShortcutFor(event('Tab')), null);
  assert.equal(mailboxShortcutFor(event('Tab', null, { shiftKey: true })), null);
});

test('native mailbox shortcuts are disabled for every normal interactive target', () => {
  for (const tagName of ['A', 'BUTTON', 'INPUT', 'SELECT', 'TEXTAREA']) {
    assert.equal(mailboxShortcutFor(event('e', { tagName })), null, tagName);
  }
  assert.equal(mailboxShortcutFor(event('j', { tagName: 'DIV', isContentEditable: true })), null);
  assert.equal(mailboxShortcutFor(event('k', { tagName: 'DIV', getAttribute: () => 'dialog' })), null);
});

test('native mailbox shortcuts ignore command, control, and option combinations', () => {
  assert.equal(mailboxShortcutFor(event('k', null, { metaKey: true })), null);
  assert.equal(mailboxShortcutFor(event('b', null, { ctrlKey: true })), null);
  assert.equal(mailboxShortcutFor(event('u', null, { altKey: true })), null);
});

test('native mailbox shortcuts map only the documented mailbox keys', () => {
  // Search moved to the platform chord, so a bare slash is just a character.
  assert.equal(mailboxShortcutFor(event('/')), null);
  assert.equal(mailboxShortcutFor(event('j')), 'next-thread');
  assert.equal(mailboxShortcutFor(event('k')), 'previous-thread');
  assert.equal(mailboxShortcutFor(event('Enter')), 'open-thread');
  assert.equal(mailboxShortcutFor(event('e')), 'archive');
  assert.equal(mailboxShortcutFor(event('s')), 'toggle-star');
  assert.equal(mailboxShortcutFor(event('h')), 'snooze');
  assert.equal(mailboxShortcutFor(event('u')), 'toggle-unread');
  assert.equal(mailboxShortcutFor(event('r')), 'reply');
  assert.equal(mailboxShortcutFor(event('a')), 'reply-all');
  assert.equal(mailboxShortcutFor(event('f')), 'forward');
  assert.equal(mailboxShortcutFor(event('c')), 'compose');
  assert.equal(mailboxShortcutFor(event('g')), 'go-to');
  assert.equal(mailboxShortcutFor(event('x')), null);
});

test('shift is part of the binding, not something to shrug at', () => {
  assert.equal(mailboxShortcutFor(event('G', null, { shiftKey: true })), 'go-to-account');
  assert.equal(mailboxShortcutFor(event('?', null, { shiftKey: true })), 'shortcuts');
  // Holding shift does not silently fire the unshifted action.
  assert.equal(mailboxShortcutFor(event('E', null, { shiftKey: true })), null);
});

test('command chords stay live where bare keys must not', () => {
  assert.equal(chordShortcutFor(event('f', { tagName: 'INPUT' }, { metaKey: true })), 'search');
  assert.equal(chordShortcutFor(event('k', null, { metaKey: true })), 'palette');
  assert.equal(chordShortcutFor(event('p', null, { ctrlKey: true })), 'palette');
  assert.equal(chordShortcutFor(event(',', null, { metaKey: true })), 'settings');
  assert.equal(chordShortcutFor(event('/', null, { metaKey: true })), 'shortcuts');
  assert.equal(chordShortcutFor(event('f')), null);
  assert.equal(chordShortcutFor(event('f', null, { metaKey: true, altKey: true })), null);
});

test('every documented binding reaches the action it is documented under', () => {
  for (const definition of shortcutCatalog) {
    for (const binding of definition.bindings) {
      const fired = binding.command
        ? chordShortcutFor(event(binding.key, null, { metaKey: true, shiftKey: Boolean(binding.shift) }))
        : mailboxShortcutFor(event(binding.key, null, { shiftKey: Boolean(binding.shift) }));
      assert.equal(fired, definition.action, `${definition.title} via ${shortcutChips(binding).join('')}`);
    }
  }
});

test('chips read the way the keys are pressed', () => {
  assert.deepEqual(shortcutChips({ key: 'k', command: true }), ['⌘', 'K']);
  assert.deepEqual(shortcutChips({ key: 'g', shift: true }), ['⇧', 'G']);
  assert.deepEqual(shortcutChips({ key: 'Enter' }), ['↩']);
  assert.equal(shortcutLabel('settings'), '⌘,');
  assert.equal(shortcutLabel('go-to'), 'G');
});

test('interactive target detection honors nested controls', () => {
  const nested = { tagName: 'SPAN', closest: (selector: string) => selector.includes('button') ? {} : null };
  assert.equal(isInteractiveShortcutTarget(nested), true);
});
