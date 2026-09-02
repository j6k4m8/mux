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
  sampleNotice,
  SAMPLE_MS,
  undoableNotice,
  undoDeadline,
  UNDO_MS
} from './notices';
import { MOTION_BASE_MS, motionDuration } from './motion';

const now = 1_000_000;

test('a toast with nothing to undo always carries an expiry', () => {
  assert.deepEqual(confirmationNotice('Saved', now), { text: 'Saved', until: now + CONFIRMATION_MS });
  assert.deepEqual(failureNotice(new Error('nope'), now), { text: 'nope', until: now + FAILURE_MS });
  // Anything thrown reads as something, so a toast can never come up blank.
  assert.equal(failureNotice('plain string', now).text, 'plain string');
  assert.equal(failureNotice({ weird: true }, now).text, '[object Object]');
});

test('a sample is gone well before a confirmation would be', () => {
  assert.equal(sampleNotice('Like this!', now).until, now + SAMPLE_MS);
  assert.ok(SAMPLE_MS < CONFIRMATION_MS);
  // It still outlasts arriving and leaving at the slowest speed.
  assert.ok(SAMPLE_MS > 2 * motionDuration('slow', MOTION_BASE_MS, false));
});

test('undoable work expires only when its undo does', () => {
  const pushed = undoableNotice('Archived', 'op_1', undoDeadline(now));
  assert.equal(pushed.until, now + UNDO_MS);
  assert.ok(!noticeExpired(pushed, now + UNDO_MS - 1));
  assert.ok(noticeExpired(pushed, now + UNDO_MS));

  // Every undoable toast carries a deadline. A local journal entry stays
  // undoable long after its toast has gone, but the toast still goes: one that
  // waits forever is worse than a short window to press Undo in.
  const local = undoableNotice('Conversation snoozed', 'op_2', undoDeadline(now));
  assert.equal(local.until, now + UNDO_MS);
  assert.ok(noticeExpired(local, now + 60 * 60 * 1_000));
  assert.ok(!noticeOffersUndo(local, now + 60 * 60 * 1_000));
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
