import assert from 'node:assert/strict';
import { test } from 'vitest';

import { moveDestinations } from './moveTargets';

const context = { trashed: false, accountLabel: 'Work · jordan@acme.example' };

test('an inbox conversation can be archived or trashed, and nothing else', () => {
  const destinations = moveDestinations({ accountId: 'acc_work', inInbox: true }, context);
  assert.deepEqual(destinations.map((destination) => [destination.title, destination.action]), [
    ['Archive', 'archive'],
    ['Trash', 'delete']
  ]);
});

test('an archived conversation can go back to the inbox or on to the trash', () => {
  const destinations = moveDestinations({ accountId: 'acc_work', inInbox: false }, context);
  assert.deepEqual(destinations.map((destination) => [destination.title, destination.action]), [
    ['Inbox', 'restore'],
    ['Trash', 'delete']
  ]);
});

test('a trashed conversation offers the one place lifting the flag puts it', () => {
  const trashed = { ...context, trashed: true };
  assert.deepEqual(
    moveDestinations({ accountId: 'acc_work', inInbox: true }, trashed)
      .map((destination) => [destination.title, destination.action]),
    [['Inbox', 'untrash']]
  );
  assert.deepEqual(
    moveDestinations({ accountId: 'acc_work', inInbox: false }, trashed)
      .map((destination) => [destination.title, destination.action]),
    [['Archive', 'untrash']]
  );
});

test('every destination is in the conversation’s own account and takes one operation', () => {
  const actions = new Set(['restore', 'archive', 'delete', 'untrash']);
  for (const trashed of [false, true]) {
    for (const inInbox of [false, true]) {
      const destinations = moveDestinations({ accountId: 'acc_work', inInbox }, { ...context, trashed });
      assert.ok(destinations.length > 0);
      for (const destination of destinations) {
        assert.equal(destination.subtitle, context.accountLabel);
        // Only flag changes on this thread, which cannot reach another account.
        assert.ok(actions.has(destination.action), destination.action);
      }
    }
  }
});
