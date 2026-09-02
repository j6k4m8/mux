import assert from 'node:assert/strict';
import { test } from 'vitest';

import {
  CONFIRMATION_MS,
  confirmationNotice,
  failureNotice,
  FAILURE_MS,
  noticeExpired,
  noticeOffersUndo,
  noticeRemainingSeconds,
  undoableNotice,
  undoDeadline,
  UNDO_MS
} from './notices';

const now = 1_000_000;

test('a toast with nothing to undo always carries an expiry', () => {
  assert.deepEqual(confirmationNotice('Saved', now), { text: 'Saved', until: now + CONFIRMATION_MS });
  assert.deepEqual(failureNotice(new Error('nope'), now), { text: 'nope', until: now + FAILURE_MS });
  // Anything thrown reads as something, so a toast can never come up blank.
  assert.equal(failureNotice('plain string', now).text, 'plain string');
  assert.equal(failureNotice({ weird: true }, now).text, '[object Object]');
});

test('undoable work expires only when its undo does', () => {
  const pushed = undoableNotice('Archived', 'op_1', undoDeadline(now));
  assert.equal(pushed.until, now + UNDO_MS);
  assert.ok(!noticeExpired(pushed, now + UNDO_MS - 1));
  assert.ok(noticeExpired(pushed, now + UNDO_MS));

  // Local journals stay undoable until something supersedes them, so hiding
  // the toast would take away the only Undo button there is.
  const local = undoableNotice('Conversation snoozed', 'op_2');
  assert.equal(local.until, undefined);
  assert.ok(!noticeExpired(local, now + 60 * 60 * 1_000));
  assert.ok(noticeOffersUndo(local, now + 60 * 60 * 1_000));
});

test('the undo button is offered exactly while the undo is still good', () => {
  assert.ok(!noticeOffersUndo(null, now));
  assert.ok(!noticeOffersUndo(confirmationNotice('Undone', now), now));
  const queued = undoableNotice('Message queued', 'op_3', now + 9_000);
  assert.ok(noticeOffersUndo(queued, now));
  assert.equal(noticeRemainingSeconds(queued, now), 9);
  assert.ok(!noticeOffersUndo(queued, now + 9_000));
  assert.equal(noticeRemainingSeconds(queued, now + 9_000), 0);
});
