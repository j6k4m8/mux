import assert from 'node:assert/strict';
import { test } from 'vitest';

import {
  accountHealth,
  countdownPhrase,
  describeCadence,
  describeQueueEntry,
  describeSyncError,
  describeSyncFailure,
  describeSyncState,
  durationPhrase,
  groupQueue,
  isAsking,
  laneCounts,
  mailboxVerdict,
  MAX_OPERATION_ATTEMPTS,
  operationTitle,
  OPERATION_STATES,
  queueEntryAction,
  queueEntryStatus,
  queueBadgeCount,
  queueLane,
  queueVerdict,
  relativeMoment,
  SYNC_STATES
} from './syncStatus';
import type { SyncObservation } from './syncStatus';
import type { AccountSummary, OperationActivitySummary } from './types';

const now = 1_700_000_000_000;

function account(overrides: Partial<AccountSummary> = {}): AccountSummary {
  return {
    id: 'acc_work',
    name: 'Work',
    email: 'jordan@acme.example',
    color: '#5168f4',
    signature: 'Jordan',
    unread: 1,
    total: 42,
    refreshSeconds: 300,
    lastSyncAt: now - 240_000,
    syncState: 'idle',
    lastErrorCode: null,
    ...overrides
  };
}

function row(overrides: Partial<OperationActivitySummary> = {}): OperationActivitySummary {
  return {
    id: 'op_1',
    threadId: 7,
    field: 'in_inbox',
    kind: 'archive',
    state: 'pending',
    createdAt: now - 10_000,
    notBefore: now - 9_650,
    confirmedAt: null,
    undoOf: null,
    attempts: 1,
    ...overrides
  };
}

test('every state the store can write lands in a lane, and only the right one', () => {
  // The store asks for exactly these four whenever it wants to know whether a
  // thread still has local changes in flight, so none of them may read as done.
  for (const state of ['pending', 'executing', 'retrying']) {
    assert.equal(queueLane(state), 'working');
  }
  assert.equal(queueLane('outcome_unknown'), 'attention');
  assert.equal(queueLane('confirmed'), 'settled');
  assert.equal(queueLane('cancelled'), 'settled');
  assert.equal(queueLane('failed'), 'stopped');
  assert.equal(queueLane('conflicted'), 'stopped');
  // A state added to the store later must not be smuggled into a lane it does
  // not belong in.
  assert.equal(queueLane('teleported'), 'other');
  assert.equal(queueLane(''), 'other');
  for (const state of OPERATION_STATES) assert.notEqual(queueLane(state), 'other');
});

test('a retry never reads as a failure', () => {
  const retrying = row({ state: 'retrying', attempts: 4, notBefore: now + 120_000 });
  assert.equal(queueLane(retrying.state), 'working');
  assert.equal(queueEntryStatus(retrying, now), 'Trying again in 2m');
  assert.equal(describeQueueEntry(retrying, now).detail, 'Queued 10s ago · attempt 4 of 8');
  // Backoff that has already elapsed is a row waiting its turn, not a stall.
  assert.equal(queueEntryStatus(row({ state: 'retrying', notBefore: now - 1 }), now), 'Trying again now');
});

test('the attempt budget is what separates slow from doomed', () => {
  assert.equal(MAX_OPERATION_ATTEMPTS, 8);
  // Attempts are counted as the worker picks a row up, so a first try is the
  // normal case and saying so would be noise.
  assert.equal(describeQueueEntry(row({ attempts: 0 }), now).detail, 'Queued 10s ago');
  assert.equal(describeQueueEntry(row({ attempts: 1 }), now).detail, 'Queued 10s ago');
  assert.equal(
    describeQueueEntry(row({ state: 'retrying', attempts: MAX_OPERATION_ATTEMPTS - 1 }), now).detail,
    'Queued 10s ago · attempt 7 of 8'
  );
  // Failure is the end of the budget, and the count is the reason to believe it.
  assert.equal(queueEntryStatus(row({ state: 'failed', attempts: 8 }), now), 'Gave up after 8 attempts');
  assert.equal(queueEntryStatus(row({ state: 'failed', attempts: 1 }), now), 'Gave up after 1 attempt');
  assert.equal(queueEntryStatus(row({ state: 'failed', attempts: 0 }), now), 'Gave up');
});

test('a conflict is a refused undo, not a change that never happened', () => {
  const conflicted = row({ state: 'conflicted', attempts: 1 });
  assert.equal(queueLane(conflicted.state), 'stopped');
  assert.equal(queueEntryStatus(conflicted, now), 'Could not be undone — the conversation changed first');
  assert.equal(queueEntryAction(conflicted), null);
});

test('a queued send says when it goes out, and a stuck one says it is stuck', () => {
  const queued = row({
    field: 'send',
    kind: 'send',
    threadId: null,
    state: 'pending',
    createdAt: now,
    notBefore: now + 5_000
  });
  assert.equal(queueEntryStatus(queued, now), 'Goes out in 5s');
  assert.equal(queueEntryStatus({ ...queued, notBefore: now + 400 }, now), 'Goes out now');
  // Past its floor, a send is eligible and simply behind something else — the
  // worker runs one scope at a time per account.
  assert.equal(queueEntryStatus({ ...queued, notBefore: now - 1 }, now), 'Waiting its turn');

  const unknown = { ...queued, state: 'outcome_unknown', attempts: 3 };
  assert.equal(queueLane(unknown.state), 'attention');
  assert.equal(queueEntryStatus(unknown, now), 'Mux never learned whether this went out');

  const sent = { ...queued, state: 'confirmed', confirmedAt: now - 60_000 };
  assert.equal(queueEntryStatus(sent, now), 'Went through 1m ago');
  // A confirmed row with no confirmation stamp still has to say something.
  assert.equal(
    queueEntryStatus({ ...sent, confirmedAt: null, createdAt: now - 3_600_000 }, now),
    'Went through 1h ago'
  );
});

test('only work the store would actually accept is offered an action', () => {
  // A send is undoable strictly while pending — a *retrying* send has already
  // been tried, so the store refuses to promise it did not go out, and the
  // button must not be there.
  const send = row({ field: 'send', kind: 'send' });
  assert.equal(queueEntryAction(send), 'call-off');
  assert.equal(queueEntryAction({ ...send, state: 'executing' }), null);
  assert.equal(queueEntryAction({ ...send, state: 'retrying' }), null);
  assert.equal(queueEntryAction({ ...send, state: 'outcome_unknown' }), 'unlock-draft');
  // Unlocking is only for sends; nothing else has a locked draft behind it.
  assert.equal(queueEntryAction(row({ state: 'outcome_unknown' })), null);

  // A thread change can be called off until it is confirmed.
  assert.equal(queueEntryAction(row({ state: 'pending' })), 'call-off');
  assert.equal(queueEntryAction(row({ state: 'retrying' })), 'call-off');
  // Undoing a confirmed change queues a new change instead of dropping one, so
  // this screen does not offer it.
  assert.equal(queueEntryAction(row({ state: 'confirmed' })), null);
  for (const state of ['failed', 'conflicted', 'cancelled']) {
    assert.equal(queueEntryAction(row({ state })), null);
  }
});

test('the queue groups in reading order and drops empty headings', () => {
  const groups = groupQueue(
    [
      row({ id: 'a', state: 'confirmed', confirmedAt: now - 1_000 }),
      row({ id: 'b', state: 'retrying' }),
      row({ id: 'c', field: 'send', kind: 'send', state: 'outcome_unknown' }),
      row({ id: 'd', state: 'pending' })
    ],
    now
  );
  assert.deepEqual(groups.map((group) => group.lane), ['attention', 'working', 'settled']);
  assert.deepEqual(groups.map((group) => group.title), ['Needs you', 'On its way', 'Finished']);
  assert.deepEqual(groups[1].entries.map((entry) => entry.id), ['b', 'd']);
  assert.deepEqual(groupQueue([], now), []);
  assert.deepEqual(laneCounts([row({ state: 'failed' }), row({ state: 'nonsense' })]), {
    attention: 0,
    working: 0,
    stopped: 1,
    other: 1,
    settled: 0
  });
});

test('the queue verdict puts a decision ahead of healthy traffic', () => {
  assert.deepEqual(queueVerdict([]), { tone: 'good', text: 'Nothing queued' });
  assert.deepEqual(queueVerdict([row({ state: 'confirmed' })]), {
    tone: 'good',
    text: 'Everything has gone through'
  });
  assert.deepEqual(queueVerdict([row({ state: 'pending' }), row({ state: 'retrying' })]), {
    tone: 'working',
    text: '2 changes on the way'
  });
  assert.deepEqual(queueVerdict([row({ state: 'failed' })]), { tone: 'attention', text: '1 change stopped' });
  assert.deepEqual(
    queueVerdict([row({ state: 'outcome_unknown' }), row({ state: 'failed' }), row({ state: 'pending' })]),
    { tone: 'attention', text: '1 change needs you' }
  );
});

test('the top-bar badge excludes history but includes work and unresolved states', () => {
  assert.equal(queueBadgeCount([]), 0);
  assert.equal(queueBadgeCount([
    row({ id: 'pending', state: 'pending' }),
    row({ id: 'unknown', state: 'outcome_unknown' }),
    row({ id: 'failed', state: 'failed' }),
    row({ id: 'confirmed', state: 'confirmed' }),
    row({ id: 'cancelled', state: 'cancelled' })
  ]), 3);
});

test('undo rows name what they are undoing', () => {
  assert.equal(operationTitle('archive'), 'Archive conversation');
  assert.equal(operationTitle('undo_archive'), 'Undo — Archive conversation');
  assert.equal(operationTitle('unlabel'), 'Remove label');
  // Kinds this screen has never heard of still read as words.
  assert.equal(operationTitle('bulk_relabel'), 'Bulk relabel');
  assert.equal(operationTitle('undo_bulk_relabel'), 'Undo — Bulk relabel');
  // Nothing after the prefix leaves nothing to name, so it stays a plain word
  // rather than becoming a dangling "Undo — ".
  assert.equal(operationTitle('undo_'), 'Undo');
  assert.equal(operationTitle(''), 'Unnamed change');
  assert.equal(
    describeQueueEntry(row({ kind: 'undo_archive', undoOf: 'op_0' }), now).detail,
    'Queued 10s ago · undoes an earlier change'
  );
});

test('spans read as one unit and never round up into a bigger one', () => {
  assert.equal(durationPhrase(0), '0s');
  assert.equal(durationPhrase(59_000), '59s');
  assert.equal(durationPhrase(60_000), '1m');
  assert.equal(durationPhrase(3_599_000), '1h');
  assert.equal(durationPhrase(3_600_000), '1h');
  assert.equal(durationPhrase(82_800_000), '23h');
  assert.equal(durationPhrase(86_400_000), '1d');
  assert.equal(durationPhrase(-5_000), '0s');

  assert.equal(relativeMoment(now, now), 'just now');
  assert.equal(relativeMoment(now - 4_999, now), 'just now');
  assert.equal(relativeMoment(now - 30_000, now), '30s ago');
  assert.equal(relativeMoment(now + 120_000, now), 'in 2m');

  // A countdown reads as a wait right up until it is not one. "Goes out in 0s"
  // would be a number to watch instead of a state to act on.
  assert.equal(countdownPhrase(now + 5_000, now), 'in 5s');
  assert.equal(countdownPhrase(now + 1_000, now), 'in 1s');
  assert.equal(countdownPhrase(now + 999, now), 'now');
  assert.equal(countdownPhrase(now, now), 'now');
});

test('the cadence describes whatever number the store holds', () => {
  assert.equal(describeCadence(30), 'every 30 seconds');
  assert.equal(describeCadence(60), 'every minute');
  assert.equal(describeCadence(300), 'every 5 minutes');
  assert.equal(describeCadence(3_600), 'every hour');
  assert.equal(describeCadence(7_200), 'every 2 hours');
  // A cadence of zero is a real possibility and is not "every 0 seconds".
  assert.equal(describeCadence(0), 'only when you ask');
  assert.equal(describeCadence(Number.NaN), 'only when you ask');
});

test('a healthy account is dated from the store, not from being asked', () => {
  const healthy = accountHealth(account(), undefined, now);
  assert.equal(healthy.tone, 'good');
  assert.equal(healthy.headline, 'Synced 4m ago');
  assert.match(healthy.detail, /every 5 minutes/u);
  assert.ok(!healthy.syncing);
  assert.ok(!healthy.asking);

  // The date survives a state that is not idle; it just stops being the
  // headline, because what is happening now matters more than when it last did.
  const queued = accountHealth(account({ syncState: 'scheduled' }), undefined, now);
  assert.equal(queued.tone, 'working');
  assert.equal(queued.headline, 'A check is queued');
  assert.match(queued.detail, /Synced 4m ago/u);
});

test('every state the schema allows says something, and none is invented', () => {
  const expected: Array<[string, string, string]> = [
    ['never_synced', 'unknown', 'Has not finished a first sync yet'],
    ['idle', 'good', 'Up to date'],
    ['scheduled', 'working', 'A check is queued'],
    ['syncing', 'working', 'Syncing now'],
    // Backoff is the provider's own retry, so it must not read as a failure.
    ['backoff', 'working', 'Waiting to try again after a failure'],
    ['authentication_blocked', 'attention', 'Needs you to sign in again'],
    ['offline', 'attention', 'No usable sign-in for this account'],
    ['failed', 'attention', 'Its last sync failed']
  ];
  assert.deepEqual(expected.map(([state]) => state), [...SYNC_STATES]);
  for (const [state, tone, text] of expected) {
    assert.deepEqual(describeSyncState(state), { tone, text });
  }
  // A state added to the schema later is shown, not guessed at.
  assert.deepEqual(describeSyncState('quarantined'), {
    tone: 'unknown',
    text: 'Unrecognized sync state “quarantined”'
  });
});

test('no provider row is not the same as never having synced', () => {
  // All three columns are null for a local-only mailbox: nothing is syncing it,
  // and nothing is wrong with it either.
  const local = accountHealth(
    account({ lastSyncAt: null, syncState: null, lastErrorCode: null }),
    undefined,
    now
  );
  assert.equal(local.tone, 'unknown');
  assert.equal(local.headline, 'No provider is syncing this mailbox');
  assert.equal(local.detail, 'Mail here stays as it is until a provider is connected.');
  // Absent fields read the same as explicit nulls, since the type allows both.
  const absent = accountHealth(
    { ...account(), lastSyncAt: undefined, syncState: undefined, lastErrorCode: undefined },
    undefined,
    now
  );
  assert.deepEqual(absent, local);

  // An account with a provider that has not finished a cycle is a different
  // sentence, and its cadence still applies.
  const first = accountHealth(account({ lastSyncAt: null, syncState: 'never_synced' }), undefined, now);
  assert.equal(first.tone, 'unknown');
  assert.equal(first.headline, 'Has not finished a first sync yet');
  assert.match(first.detail, /every 5 minutes/u);
});

test('a stored error code says why on the account itself', () => {
  const blocked = accountHealth(
    account({ syncState: 'authentication_blocked', lastErrorCode: 'imap_reauthorization_required' }),
    undefined,
    now
  );
  assert.equal(blocked.tone, 'attention');
  assert.equal(blocked.headline, 'Needs you to sign in again');
  assert.match(blocked.detail, /^The IMAP server wants you to sign in again/u);
  // The date is still there as context, behind the reason.
  assert.match(blocked.detail, /Synced 4m ago/u);

  assert.equal(describeSyncError(null), '');
  assert.equal(describeSyncError(undefined), '');
  assert.equal(describeSyncError(''), '');
  assert.equal(describeSyncError('credential_unavailable'), 'Mux could not read this account’s sign-in from the keychain');
  // An unfamiliar code is still a reason, so it is spoken rather than dropped.
  assert.equal(describeSyncError('imap_greeting_refused'), 'The provider reported “imap greeting refused”');
});

test('an in-flight request covers the gap before the store catches up', () => {
  const asked: SyncObservation = { askedAt: now - 2_000 };
  assert.ok(isAsking(asked));
  const asking = accountHealth(account({ syncState: 'idle' }), asked, now);
  assert.equal(asking.tone, 'working');
  assert.equal(asking.headline, 'Asking for new mail');
  assert.ok(asking.syncing && asking.asking);

  const done: SyncObservation = { askedAt: now - 61_000, settledAt: now - 60_000, scheduled: 1 };
  assert.ok(!isAsking(done));
  // Once the request is answered the store is the authority again.
  assert.equal(accountHealth(account(), done, now).headline, 'Synced 4m ago');

  // Zero scheduled cycles means it was already mid-sync, which is a success the
  // provider row may not have caught up to yet.
  const redundant = accountHealth(account(), { askedAt: now - 1_000, settledAt: now, scheduled: 0 }, now);
  assert.equal(redundant.tone, 'working');
  assert.equal(redundant.headline, 'Already mid-check');
  assert.ok(redundant.syncing && !redundant.asking);

  // A request that just failed is newer than anything the row remembers.
  const failed = accountHealth(account(), { askedAt: now, settledAt: now, error: 'IMAP said no' }, now);
  assert.equal(failed.tone, 'attention');
  assert.equal(failed.detail, 'IMAP said no');
});

test('a sync the provider is running shows without locking the button', () => {
  const running = accountHealth(account({ syncState: 'syncing' }), undefined, now);
  assert.ok(running.syncing);
  // Nothing of this screen's is outstanding, so the one control that could
  // clear a stuck `syncing` stays available.
  assert.ok(!running.asking);
});

test('a failure always says something, whatever shape it arrived in', () => {
  // The commands behind this screen return their errors as plain strings.
  assert.equal(describeSyncFailure('Manual synchronization is not implemented'), 'Manual synchronization is not implemented');
  assert.equal(describeSyncFailure(new Error('Operation was not found')), 'Operation was not found');
  assert.equal(describeSyncFailure({ message: 'Native mailbox state is unavailable' }), 'Native mailbox state is unavailable');
  // An empty alert reads as "no problem", which is the opposite of the truth.
  for (const blank of [new Error(''), '', '   ', null, undefined, { message: '' }]) {
    assert.equal(describeSyncFailure(blank), 'Mux could not say what went wrong.');
  }
});

test('no accounts is its own answer, not a cheerful one about nothing', () => {
  assert.deepEqual(mailboxVerdict([], {}, now), { tone: 'unknown', text: 'No accounts connected' });
  assert.deepEqual(mailboxVerdict([account()], {}, now), { tone: 'good', text: 'Up to date' });
  assert.deepEqual(mailboxVerdict([account(), account({ id: 'acc_home' })], {}, now), {
    tone: 'good',
    text: 'All accounts up to date'
  });
  assert.deepEqual(
    mailboxVerdict([account(), account({ id: 'acc_home' })], { acc_home: { askedAt: now } }, now),
    { tone: 'working', text: 'Checking for new mail' }
  );
  assert.deepEqual(
    mailboxVerdict(
      [account(), account({ id: 'acc_home' })],
      { acc_home: { askedAt: now - 1, settledAt: now, error: 'nope' } },
      now
    ),
    { tone: 'attention', text: '1 account needs attention' }
  );
  // An account nothing syncs cannot be counted into a cheerful total, so it is
  // subtracted from one instead.
  assert.deepEqual(
    mailboxVerdict([account(), account({ id: 'acc_local', syncState: null, lastSyncAt: null })], {}, now),
    { tone: 'unknown', text: '1 of 2 accounts up to date' }
  );
  assert.deepEqual(
    mailboxVerdict([account({ syncState: null, lastSyncAt: null })], {}, now),
    { tone: 'unknown', text: 'Nothing here is syncing yet' }
  );
});
