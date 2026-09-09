/// Everything the Sync screen knows how to say, kept out of the component so it
/// can be checked against what the store actually does rather than eyeballed in
/// the interface. Two questions live here: what a journal row is really doing,
/// and how far behind an account is.
///
/// The journal's states are the store's vocabulary, not this screen's. Nothing
/// below invents one, and nothing below calls a row finished while the worker
/// still intends to pick it up again — mislabelling a retry as a failure is the
/// one mistake that would make this screen worse than no screen.

import type { AccountSummary, OperationActivitySummary } from './types';

/// Every string the store writes into `operations.state`. Listed so an unknown
/// state — one added to the store after this screen was written — surfaces as
/// unrecognized instead of being quietly sorted into the wrong lane.
export const OPERATION_STATES = [
  'pending',
  'executing',
  'retrying',
  'outcome_unknown',
  'confirmed',
  'cancelled',
  'failed',
  'conflicted'
] as const;

export type OperationState = (typeof OPERATION_STATES)[number];

export function isOperationState(state: string): state is OperationState {
  return (OPERATION_STATES as readonly string[]).includes(state);
}

/// Where a row belongs on this screen:
///
/// - `working` — the worker is going to act on it without being asked.
///   `pending` and `retrying` are equally eligible for a claim and differ only
///   in whether a previous attempt left an error behind; `executing` is leased
///   right now, and if Mux dies mid-flight the store puts it back to `retrying`.
/// - `attention` — nobody knows the outcome. `outcome_unknown` is the only
///   state like this, it only ever happens to a send, and it is terminal as far
///   as the worker is concerned: it means the send crossed the point of no
///   return without a usable answer, so retrying might send it twice. The draft
///   behind it stays locked. Gmail can sometimes settle one of these by going
///   looking for the message, but nothing else will, so it is shown as a
///   decision waiting to be made rather than as work in progress.
/// - `stopped` — the worker exhausted its retry budget (`failed`), or an undo
///   found the conversation already changed (`conflicted`). Both are terminal
///   with no way back; the only recovery is doing the thing again.
/// - `settled` — went through, or was called off.
/// - `other` — a state this screen does not recognize; shown verbatim.
export const QUEUE_LANES = ['attention', 'working', 'stopped', 'other', 'settled'] as const;

export type QueueLane = (typeof QUEUE_LANES)[number];

const LANE_BY_STATE: Record<OperationState, QueueLane> = {
  pending: 'working',
  executing: 'working',
  retrying: 'working',
  outcome_unknown: 'attention',
  confirmed: 'settled',
  cancelled: 'settled',
  failed: 'stopped',
  conflicted: 'stopped'
};

export const QUEUE_LANE_TITLES: Record<QueueLane, string> = {
  attention: 'Needs you',
  working: 'On its way',
  stopped: 'Stopped',
  other: 'Unrecognized',
  settled: 'Finished'
};

export function queueLane(state: string): QueueLane {
  return isOperationState(state) ? LANE_BY_STATE[state] : 'other';
}

/// How many tries a row gets before the worker calls it failed. The budget
/// actually lives on the provider work row rather than on the operation, but
/// every place that enqueues one asks for eight, so eight is what a reader
/// should be told — it is the difference between "still trying" and "one more
/// failure and this is over".
export const MAX_OPERATION_ATTEMPTS = 8;

/// What this screen can offer to do about a row. Both map onto an IPC command
/// the store will actually accept for that row; nothing is offered that would
/// only come back as a conflict.
export type QueueAction = 'unlock-draft' | 'call-off';

export type QueueEntry = {
  id: string;
  lane: QueueLane;
  title: string;
  /// The line that answers "is this one stuck?".
  status: string;
  /// Where it came from and how hard it has tried.
  detail: string;
  action: QueueAction | null;
};

export type QueueGroup = {
  lane: QueueLane;
  title: string;
  entries: QueueEntry[];
};

export type HealthTone = 'good' | 'working' | 'attention' | 'unknown';

export type Verdict = { tone: HealthTone; text: string };

const OPERATION_TITLES: Record<string, string> = {
  archive: 'Archive conversation',
  restore: 'Move to inbox',
  read: 'Mark as read',
  unread: 'Mark as unread',
  star: 'Star conversation',
  unstar: 'Remove star',
  delete: 'Move to trash',
  untrash: 'Restore from trash',
  label: 'Add label',
  unlabel: 'Remove label',
  snooze: 'Snooze conversation',
  rsvp: 'Invitation response',
  send: 'Send message'
};

/// Every command behind this screen returns its failure as a plain string, so a
/// thrown value is usually not an `Error` at all. The last clause matters most:
/// a caught value that stringifies to nothing would otherwise put an empty
/// alert on screen, which reads as "no problem" rather than "unexplained one".
export function describeSyncFailure(cause: unknown): string {
  const unexplained = 'Mux could not say what went wrong.';
  if (typeof cause === 'object' && cause !== null) {
    const message = (cause as { message?: unknown }).message;
    /// An object that stringifies to "[object Object]" tells a reader less than
    /// admitting there is no explanation, so it never reaches the screen.
    return typeof message === 'string' && message.trim() ? message : unexplained;
  }
  return String(cause ?? '').trim() || unexplained;
}

function readableKind(kind: string): string {
  const spaced = kind.replaceAll('_', ' ').trim();
  if (!spaced) return 'Unnamed change';
  return spaced[0].toUpperCase() + spaced.slice(1);
}

/// An undo is stored as its own row whose kind is `undo_<kind>`, so naming both
/// directions from one table keeps them from drifting apart.
export function operationTitle(kind: string): string {
  if (kind.startsWith('undo_') && kind.length > 5) {
    const undone = kind.slice(5);
    return `Undo — ${OPERATION_TITLES[undone] ?? readableKind(undone)}`;
  }
  return OPERATION_TITLES[kind] ?? readableKind(kind);
}

/// Only the largest unit. A queue is read at a glance, and "2m" carries every
/// decision a reader is about to make; "2m 14s" carries the same one, later.
export function durationPhrase(milliseconds: number): string {
  const seconds = Math.max(0, Math.round(milliseconds / 1_000));
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours}h`;
  return `${Math.round(hours / 24)}d`;
}

/// Deliberately not localized. These sit beside a clock that reticks every
/// second, and `Intl.RelativeTimeFormat` would change their width on every tick
/// in some locales while telling a queue reader nothing more than "2m ago"
/// already does. Absolute timestamps are the mailbox's job, not this screen's.
export function relativeMoment(at: number, now: number): string {
  const delta = at - now;
  if (Math.abs(delta) < 5_000) return 'just now';
  return delta > 0 ? `in ${durationPhrase(delta)}` : `${durationPhrase(-delta)} ago`;
}

/// The cadence read back as a sentence fragment, from whatever number the store
/// holds rather than from the five buttons the settings screen offers: an
/// account configured elsewhere still has to describe itself here.
export function describeCadence(refreshSeconds: number): string {
  if (!Number.isFinite(refreshSeconds) || refreshSeconds <= 0) return 'only when you ask';
  const seconds = Math.round(refreshSeconds);
  if (seconds < 60) return `every ${seconds} seconds`;
  const minutes = Math.round(seconds / 60);
  if (minutes === 1) return 'every minute';
  if (minutes < 60) return `every ${minutes} minutes`;
  const hours = Math.round(minutes / 60);
  return hours === 1 ? 'every hour' : `every ${hours} hours`;
}

/// A deadline about to arrive is not a wait. "in 0s" is a number to watch
/// rather than a state to act on, so the last second reads as now.
export function countdownPhrase(until: number, now: number): string {
  const remaining = until - now;
  return remaining < 1_000 ? 'now' : `in ${durationPhrase(remaining)}`;
}

/// `not_before` is a floor, not a schedule: the store stamps it ahead of the
/// row's creation to hold work back — 350ms of coalescing for a thread change,
/// the whole undo window for a send — and the worker pushes it further out on
/// each backoff. So a future `not_before` means "deliberately waiting", and a
/// past one means "eligible, and behind something else", which read very
/// differently to someone wondering why nothing has happened.
function waitingUntil(row: OperationActivitySummary, now: number): string | null {
  return row.notBefore > now ? countdownPhrase(row.notBefore, now) : null;
}

export function queueEntryStatus(row: OperationActivitySummary, now: number): string {
  const waiting = waitingUntil(row, now);
  switch (row.state) {
    case 'pending':
      return waiting ? `Goes out ${waiting}` : 'Waiting its turn';
    case 'executing':
      return 'Running now';
    case 'retrying':
      return waiting ? `Trying again ${waiting}` : 'Trying again now';
    case 'outcome_unknown':
      return 'Mux never learned whether this went out';
    case 'confirmed':
      return `Went through ${relativeMoment(row.confirmedAt ?? row.createdAt, now)}`;
    case 'cancelled':
      return 'Called off';
    case 'failed':
      /// The attempt count is the whole story here: eight tries and out.
      return row.attempts > 0
        ? `Gave up after ${plural(row.attempts, 'attempt', 'attempts')}`
        : 'Gave up';
    case 'conflicted':
      /// A row only reaches this state by having its undo refused, so it is the
      /// undo that failed, not the change — the change itself went through.
      return 'Could not be undone — the conversation changed first';
    default:
      return `Unrecognized state “${row.state}”`;
  }
}

export function queueEntryDetail(row: OperationActivitySummary, now: number): string {
  const parts = [`Queued ${relativeMoment(row.createdAt, now)}`];
  /// Attempts are counted as the worker picks a row up, so everything that has
  /// run at all is on its first. Saying so is noise; saying that it is on its
  /// third, out of eight, is the difference between a slow send and a doomed one.
  if (row.attempts > 1) parts.push(`attempt ${row.attempts} of ${MAX_OPERATION_ATTEMPTS}`);
  if (row.undoOf) parts.push('undoes an earlier change');
  return parts.join(' · ');
}

/// Calling work off is offered only where `undo_operation` will actually take
/// it, which is narrower for a send than for anything else: a send is droppable
/// only while `pending`, because once it has been tried at all the store cannot
/// promise it did not go out. A thread change is droppable while `pending` or
/// `retrying`.
///
/// `undo_operation` also undoes *confirmed* changes, but that queues a fresh
/// compensating operation — a new change to your mail, not a recovery, and one
/// that can leave the original `conflicted` if the conversation has moved on.
/// That belongs with the toast that offered it, not with a status screen.
///
/// A send in `outcome_unknown` gets the store's own escape hatch instead:
/// unlocking restores the draft and stops asking, without retrying a send that
/// may already have gone out.
export function queueEntryAction(row: OperationActivitySummary): QueueAction | null {
  if (row.field === 'send') {
    if (row.state === 'outcome_unknown') return 'unlock-draft';
    return row.state === 'pending' ? 'call-off' : null;
  }
  return row.state === 'pending' || row.state === 'retrying' ? 'call-off' : null;
}

export function describeQueueEntry(row: OperationActivitySummary, now: number): QueueEntry {
  return {
    id: row.id,
    lane: queueLane(row.state),
    title: operationTitle(row.kind),
    status: queueEntryStatus(row, now),
    detail: queueEntryDetail(row, now),
    action: queueEntryAction(row)
  };
}

/// Grouped in reading order, and empty lanes are dropped: a heading over
/// nothing reads as a problem the reader has to rule out.
export function groupQueue(rows: OperationActivitySummary[], now: number): QueueGroup[] {
  const entries = rows.map((row) => describeQueueEntry(row, now));
  return QUEUE_LANES.map((lane) => ({
    lane,
    title: QUEUE_LANE_TITLES[lane],
    entries: entries.filter((entry) => entry.lane === lane)
  })).filter((group) => group.entries.length > 0);
}

export function laneCounts(rows: OperationActivitySummary[]): Record<QueueLane, number> {
  const counts = { attention: 0, working: 0, stopped: 0, other: 0, settled: 0 };
  for (const row of rows) counts[queueLane(row.state)] += 1;
  return counts;
}

/// The top-bar badge is an interruption counter, not a history counter.
/// Confirmed and cancelled rows remain useful in the full journal but no
/// longer represent work or a decision waiting on the reader.
export function queueBadgeCount(rows: OperationActivitySummary[]): number {
  return rows.reduce((count, row) => count + Number(queueLane(row.state) !== 'settled'), 0);
}

function plural(count: number, one: string, many: string): string {
  return `${count} ${count === 1 ? one : many}`;
}

/// One line over the whole queue. Attention outranks movement, because a person
/// who has to decide something should not have to read past three healthy rows
/// to find out.
export function queueVerdict(rows: OperationActivitySummary[]): Verdict {
  if (!rows.length) return { tone: 'good', text: 'Nothing queued' };
  const counts = laneCounts(rows);
  if (counts.attention) {
    return { tone: 'attention', text: `${plural(counts.attention, 'change', 'changes')} needs you` };
  }
  if (counts.stopped) {
    return { tone: 'attention', text: `${plural(counts.stopped, 'change', 'changes')} stopped` };
  }
  if (counts.working) {
    return { tone: 'working', text: `${plural(counts.working, 'change', 'changes')} on the way` };
  }
  return { tone: 'good', text: 'Everything has gone through' };
}

/// Every value `provider_accounts.sync_state` is allowed to hold, taken from the
/// CHECK constraint that defines the column rather than from the handful of
/// values the current worker happens to write — a schema migration maps older
/// rows straight into `offline` and `authentication_blocked`, so those arrive
/// without any running code having produced them.
export const SYNC_STATES = [
  'never_synced',
  'idle',
  'scheduled',
  'syncing',
  'backoff',
  'authentication_blocked',
  'offline',
  'failed'
] as const;

export type SyncState = (typeof SYNC_STATES)[number];

export function isSyncState(value: string): value is SyncState {
  return (SYNC_STATES as readonly string[]).includes(value);
}

/// What the provider row says about an account, with no dates in it — the
/// timestamp is a separate fact and reads better beside this than inside it.
///
/// `null` is not "never synced": the three sync columns are all null when no
/// provider row is attached at all, which is every account in a local-only
/// mailbox. `never_synced` is the store's own word for an account that has a
/// provider and has not finished a cycle with it yet, and the two deserve
/// different sentences — one is nothing to fix, the other is waiting to work.
export function describeSyncState(syncState: string | null | undefined): Verdict {
  if (syncState === null || syncState === undefined) {
    return { tone: 'unknown', text: 'No provider is syncing this mailbox' };
  }
  if (!isSyncState(syncState)) {
    return { tone: 'unknown', text: `Unrecognized sync state “${syncState}”` };
  }
  switch (syncState) {
    case 'never_synced':
      return { tone: 'unknown', text: 'Has not finished a first sync yet' };
    case 'idle':
      return { tone: 'good', text: 'Up to date' };
    case 'scheduled':
      return { tone: 'working', text: 'A check is queued' };
    case 'syncing':
      return { tone: 'working', text: 'Syncing now' };
    /// Backoff is the provider's own retry, not a dead end: it will try again
    /// without being asked, which is why it does not read as a failure.
    case 'backoff':
      return { tone: 'working', text: 'Waiting to try again after a failure' };
    case 'authentication_blocked':
      return { tone: 'attention', text: 'Needs you to sign in again' };
    case 'offline':
      return { tone: 'attention', text: 'No usable sign-in for this account' };
    case 'failed':
      return { tone: 'attention', text: 'Its last sync failed' };
  }
}

/// The codes the store is known to write. It keeps a machine token here —
/// lowercase, underscored — and never a sentence, so an unfamiliar one is turned
/// into words instead of shown raw or dropped: a reason nobody recognizes is
/// still better than no reason.
const SYNC_ERROR_TEXT: Record<string, string> = {
  credential_unavailable: 'Mux could not read this account’s sign-in from the keychain',
  gmail_authority_invalid: 'Google rejected the stored authorization',
  gmail_reauthorization_required: 'Google wants you to authorize Mux again',
  gmail_cursor_invalid: 'Gmail retired the sync bookmark, so the mail needs re-downloading',
  imap_authority_invalid: 'The IMAP server rejected the stored sign-in',
  imap_reauthorization_required: 'The IMAP server wants you to sign in again',
  cancelled_by_request: 'The last sync was cancelled'
};

export function describeSyncError(code: string | null | undefined): string {
  if (!code) return '';
  return SYNC_ERROR_TEXT[code] ?? `The provider reported “${code.replaceAll('_', ' ')}”`;
}

/// A sync this screen asked for and is still waiting on.
///
/// The stored bookkeeping is the authority on when a sync last finished, but it
/// only reaches this screen when the mailbox reloads. Between pressing the
/// button and that reload there is a gap, and this covers it — nothing more.
export type SyncObservation = {
  /// Set when a request goes out. Still set with no `settledAt` means it has
  /// not come back.
  askedAt?: number;
  settledAt?: number;
  /// What `sync_account_now` returned: how many sync cycles it scheduled. Zero
  /// is not a refusal — it means the account was already mid-sync, so the ask
  /// was redundant rather than rejected.
  scheduled?: number;
  error?: string;
};

export type AccountHealth = {
  tone: HealthTone;
  /// The answer to "is this account up to date?", in one line.
  headline: string;
  detail: string;
  /// A sync is under way, from either source. Drives the label and the
  /// screen-wide verdict.
  syncing: boolean;
  /// This screen's own request has not come back. Only this disables the
  /// button: a `sync_state` stuck at `syncing` would otherwise take away the
  /// one control that could clear it.
  asking: boolean;
};

export function isAsking(observation: SyncObservation | undefined): boolean {
  return observation?.askedAt !== undefined && observation.settledAt === undefined;
}

export function accountHealth(
  account: AccountSummary,
  observation: SyncObservation | undefined,
  now: number
): AccountHealth {
  const cadence = `Mux checks ${describeCadence(account.refreshSeconds)} on its own.`;
  const stored = describeSyncState(account.syncState);
  const reason = describeSyncError(account.lastErrorCode);
  const asking = isAsking(observation);
  const syncing = asking || account.syncState === 'syncing';
  /// `last_sync_at` is stamped only when a cycle actually completes, and the
  /// error code is cleared in the same statement — so a date here means mail
  /// really did land, not that a request was accepted.
  const synced = typeof account.lastSyncAt === 'number'
    ? `Synced ${relativeMoment(account.lastSyncAt, now)}`
    : '';

  if (asking) {
    return {
      tone: 'working',
      syncing,
      asking,
      headline: 'Asking for new mail',
      detail: [synced, cadence].filter(Boolean).join(' · ')
    };
  }
  /// A request that just came back failed more recently than anything the
  /// provider row remembers, so it is the newer truth and outranks it.
  if (observation?.error) {
    return {
      tone: 'attention',
      syncing,
      asking,
      headline: 'That check did not go through',
      detail: observation.error
    };
  }
  /// Nothing scheduled means the account was already mid-sync when asked, which
  /// the provider row may not say yet.
  if (observation?.scheduled === 0 && stored.tone === 'good') {
    return {
      tone: 'working',
      syncing: true,
      asking,
      headline: 'Already mid-check',
      detail: [synced, cadence].filter(Boolean).join(' · ')
    };
  }

  /// A healthy account is best described by its date; every other state is
  /// better described by the state, with the date demoted to context.
  const headline = stored.tone === 'good' && synced ? synced : stored.text;
  const detail = [
    reason,
    stored.tone === 'good' || !synced ? '' : synced,
    account.syncState === null || account.syncState === undefined
      ? 'Mail here stays as it is until a provider is connected.'
      : cadence
  ].filter(Boolean).join(' · ');
  return { tone: stored.tone, syncing, asking, headline, detail };
}

/// One line over every account. "No accounts" is its own answer rather than a
/// cheerful one about nothing.
export function mailboxVerdict(
  accounts: AccountSummary[],
  observations: Record<string, SyncObservation>,
  now: number
): Verdict {
  if (!accounts.length) return { tone: 'unknown', text: 'No accounts connected' };
  const healths = accounts.map((account) => accountHealth(account, observations[account.id], now));
  if (healths.some((health) => health.syncing)) return { tone: 'working', text: 'Checking for new mail' };
  const needy = healths.filter((health) => health.tone === 'attention').length;
  if (needy) {
    return { tone: 'attention', text: `${plural(needy, 'account needs', 'accounts need')} attention` };
  }
  /// An account with no provider attached is not up to date and is not behind
  /// either, so it cannot be counted into a cheerful total — it is subtracted
  /// from one instead.
  const current = healths.filter((health) => health.tone === 'good').length;
  if (current === accounts.length) {
    return { tone: 'good', text: accounts.length === 1 ? 'Up to date' : 'All accounts up to date' };
  }
  if (current === 0) return { tone: 'unknown', text: 'Nothing here is syncing yet' };
  return { tone: 'unknown', text: `${current} of ${accounts.length} accounts up to date` };
}
