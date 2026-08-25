import assert from 'node:assert/strict';
import test from 'node:test';

import { isInteractiveShortcutTarget, mailboxShortcutFor } from '../src/shortcuts.mjs';

function event(key, target = null, modifiers = {}) {
  return { key, target, metaKey: false, ctrlKey: false, altKey: false, ...modifiers };
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
  assert.equal(mailboxShortcutFor(event('/')), 'focus-filter');
  assert.equal(mailboxShortcutFor(event('j')), 'next-thread');
  assert.equal(mailboxShortcutFor(event('k')), 'previous-thread');
  assert.equal(mailboxShortcutFor(event('e')), 'archive');
  assert.equal(mailboxShortcutFor(event('s')), 'toggle-star');
  assert.equal(mailboxShortcutFor(event('h')), 'snooze');
  assert.equal(mailboxShortcutFor(event('u')), 'toggle-unread');
  assert.equal(mailboxShortcutFor(event('r')), 'reply');
  assert.equal(mailboxShortcutFor(event('a')), 'reply-all');
  assert.equal(mailboxShortcutFor(event('f')), 'forward');
  assert.equal(mailboxShortcutFor(event('c')), 'compose');
  assert.equal(mailboxShortcutFor(event('x')), null);
});

test('interactive target detection honors nested controls', () => {
  const nested = { tagName: 'SPAN', closest: (selector) => selector.includes('button') ? {} : null };
  assert.equal(isInteractiveShortcutTarget(nested), true);
});
