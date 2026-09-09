import { mockIPC } from '@tauri-apps/api/mocks';
import { render, screen, waitFor, within } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { describe, expect, test, vi } from 'vitest';
import SyncScreen from './SyncScreen.svelte';
import type { AccountSummary, OperationActivitySummary } from './types';

const now = Date.now();

const work: AccountSummary = {
  id: 'acc_work',
  name: 'Work',
  email: 'jordan@acme.example',
  color: '#5168f4',
  signature: 'Jordan',
  unread: 3,
  total: 41,
  refreshSeconds: 300,
  lastSyncAt: now - 240_000,
  syncState: 'idle',
  lastErrorCode: null
};

const home: AccountSummary = {
  ...work,
  id: 'acc_home',
  name: 'Home',
  email: 'jordan@home.example',
  unread: 0,
  total: 12,
  refreshSeconds: 60,
  lastSyncAt: now - 30_000
};

function row(overrides: Partial<OperationActivitySummary> = {}): OperationActivitySummary {
  return {
    id: 'op_1',
    threadId: 7,
    field: 'in_inbox',
    kind: 'archive',
    state: 'pending',
    createdAt: now - 4_000,
    notBefore: now - 3_650,
    confirmedAt: null,
    undoOf: null,
    attempts: 1,
    ...overrides
  };
}

type IpcCall = { command: string; payload: unknown };
type Handlers = {
  operations?: OperationActivitySummary[];
  syncAccountNow?: (accountId: string) => number;
  undoOperation?: (operationId: string) => unknown;
  resolveUnknown?: (operationId: string) => unknown;
};

function installSyncIpc(handlers: Handlers = {}): IpcCall[] {
  const calls: IpcCall[] = [];
  mockIPC((command, payload) => {
    calls.push({ command, payload });
    if (command === 'list_operations') return handlers.operations ?? [];
    if (command === 'sync_account_now') {
      const accountId = String((payload as { input: { accountId: string } }).input.accountId);
      return handlers.syncAccountNow ? handlers.syncAccountNow(accountId) : 1;
    }
    if (command === 'undo_operation') {
      const operationId = String((payload as { operationId: string }).operationId);
      if (handlers.undoOperation) return handlers.undoOperation(operationId);
      return { id: operationId, threadId: 7, field: 'in_inbox', kind: 'archive', state: 'cancelled', notBefore: now };
    }
    if (command === 'resolve_outcome_unknown_send') {
      const operationId = String((payload as { operationId: string }).operationId);
      if (handlers.resolveUnknown) return handlers.resolveUnknown(operationId);
      return { id: operationId, threadId: null, field: 'send', kind: 'send', state: 'cancelled', notBefore: now };
    }
    throw new Error(`Unexpected IPC command: ${command}`);
  }, { shouldMockEvents: true });
  return calls;
}

async function renderSync(accounts: AccountSummary[], handlers: Handlers = {}) {
  const calls = installSyncIpc(handlers);
  const close = vi.fn();
  const refreshMailbox = vi.fn(() => Promise.resolve());
  const signIn = vi.fn();
  const user = userEvent.setup();
  render(SyncScreen, { close, accounts, refreshMailbox, signIn });
  await waitFor(() => expect(screen.queryByTestId('queue-loading')).toBeNull());
  return { calls, close, refreshMailbox, signIn, user };
}

function queueEntry(operationId: string): HTMLElement {
  const entry = screen
    .getAllByTestId('queue-entry')
    .find((element) => element.dataset.operationId === operationId);
  if (!entry) throw new Error(`No queue entry for ${operationId}`);
  return entry;
}

function accountRow(accountId: string): HTMLElement {
  const row = screen
    .getAllByTestId('sync-account')
    .find((element) => element.dataset.accountId === accountId);
  if (!row) throw new Error(`No account row for ${accountId}`);
  return row;
}

describe('sync status screen', () => {
  test('reads the journal once on mount and shows every account with its cadence', async () => {
    const { calls } = await renderSync([work, home], { operations: [] });

    expect(screen.getByTestId('sync-screen')).toBeTruthy();
    expect(calls.filter((call) => call.command === 'list_operations')).toHaveLength(1);
    expect(calls[0].payload).toEqual({ input: { limit: 100 } });

    const rendered = screen.getAllByTestId('sync-account');
    expect(rendered.map((element) => element.dataset.accountId)).toEqual(['acc_work', 'acc_home']);
    expect(within(rendered[0]).getByText(/every 5 minutes/u)).toBeTruthy();
    expect(within(rendered[1]).getByText(/every minute/u)).toBeTruthy();
    expect(within(rendered[0]).getByText('3 unread of 41')).toBeTruthy();
    // Each account is dated from its own provider row, not from being asked.
    expect(within(rendered[0]).getByTestId('sync-account-headline').textContent).toBe('Synced 4m ago');
    expect(within(rendered[1]).getByTestId('sync-account-headline').textContent).toBe('Synced 30s ago');
    expect(screen.getByTestId('sync-verdict').textContent).toBe('All accounts up to date');
    expect(screen.getByTestId('queue-verdict').textContent).toBe('Nothing queued');
    expect(screen.getByTestId('queue-empty')).toBeTruthy();
  });

  test('answers with the empty cases rather than a cheerful nothing', async () => {
    await renderSync([], { operations: [] });

    expect(screen.getByTestId('sync-verdict').textContent).toBe('No accounts connected');
    expect(screen.getByTestId('sync-accounts-empty')).toBeTruthy();
    expect(screen.queryAllByTestId('sync-account')).toHaveLength(0);
    // With nothing to sync, the button that would sync it is not offered.
    expect(screen.getByTestId<HTMLButtonElement>('sync-all').disabled).toBe(true);
  });

  test('an erroring account says so on its own row', async () => {
    const blocked = { ...work, syncState: 'authentication_blocked', lastErrorCode: 'imap_reauthorization_required' };
    const { calls, signIn, user } = await renderSync([
      blocked
    ], { operations: [] });

    const account = screen.getByTestId('sync-account');
    expect(account.dataset.tone).toBe('attention');
    expect(account.dataset.syncState).toBe('authentication_blocked');
    expect(within(account).getByTestId('sync-account-headline').textContent).toBe('Needs you to sign in again');
    expect(within(account).getByText(/The IMAP server wants you to sign in again/u)).toBeTruthy();
    expect(screen.getByTestId('sync-verdict').textContent).toBe('1 account needs attention');
    expect(within(account).queryByRole('button', { name: 'Sync now' })).toBeNull();
    await user.click(within(account).getByRole('button', { name: 'Sign in' }));
    expect(signIn).toHaveBeenCalledWith(blocked);
    expect(calls.some((call) => call.command === 'sync_account_now')).toBe(false);
  });

  test('an account with no provider is not reported as behind', async () => {
    await renderSync([
      { ...work, lastSyncAt: null, syncState: null, lastErrorCode: null }
    ], { operations: [] });

    const account = screen.getByTestId('sync-account');
    expect(account.dataset.tone).toBe('unknown');
    expect(account.dataset.syncState).toBe('none');
    expect(within(account).getByTestId('sync-account-headline').textContent)
      .toBe('No provider is syncing this mailbox');
    expect(within(account).queryByRole('button', { name: 'Sync now' })).toBeNull();
    expect(screen.getByTestId<HTMLButtonElement>('sync-all').disabled).toBe(true);
    expect(screen.getByTestId('sync-verdict').textContent).toBe('Nothing here is syncing yet');
  });

  test('a sync the provider is already running leaves the button usable', async () => {
    await renderSync([{ ...work, syncState: 'syncing' }], { operations: [] });

    const account = screen.getByTestId('sync-account');
    expect(within(account).getByTestId('sync-account-headline').textContent).toBe('Syncing now');
    // Nothing of this screen's is outstanding, so the one control that could
    // clear a stuck sync stays pressable.
    expect(within(account).getByRole<HTMLButtonElement>('button', { name: 'Checking…' }).disabled).toBe(false);
    expect(screen.getByTestId<HTMLButtonElement>('sync-all').disabled).toBe(false);
  });

  test('syncing one account asks for that account and reloads the mailbox', async () => {
    const { calls, refreshMailbox, user } = await renderSync([work, home], { operations: [] });

    await user.click(within(accountRow('acc_home')).getByRole('button', { name: 'Sync now' }));

    await waitFor(() => expect(refreshMailbox).toHaveBeenCalledTimes(1));
    const asks = calls.filter((call) => call.command === 'sync_account_now');
    expect(asks).toHaveLength(1);
    expect(asks[0].payload).toEqual({ input: { accountId: 'acc_home' } });
    // The journal is read again afterwards, because a sync can confirm queued work.
    expect(calls.filter((call) => call.command === 'list_operations')).toHaveLength(2);

    // Once the request is answered the provider row is the authority again, so
    // the row goes back to reporting its own last completed sync.
    const account = accountRow('acc_home');
    await waitFor(() => expect(account.dataset.tone).toBe('good'));
    expect(within(account).getByTestId('sync-account-headline').textContent).toBe('Synced 30s ago');
  });

  test('syncing everything asks every account once and reloads the mailbox once', async () => {
    const { calls, refreshMailbox, user } = await renderSync([work, home], { operations: [] });

    await user.click(screen.getByTestId('sync-all'));

    await waitFor(() => expect(refreshMailbox).toHaveBeenCalledTimes(1));
    const asks = calls.filter((call) => call.command === 'sync_account_now');
    expect(asks.map((call) => (call.payload as { input: { accountId: string } }).input.accountId))
      .toEqual(['acc_work', 'acc_home']);
  });

  test('syncing everything skips local mailboxes and accounts waiting for sign-in', async () => {
    const local = { ...home, id: 'acc_local', lastSyncAt: null, syncState: null, lastErrorCode: null };
    const blocked = { ...home, id: 'acc_blocked', syncState: 'authentication_blocked' };
    const { calls, refreshMailbox, user } = await renderSync([work, local, blocked], { operations: [] });

    await user.click(screen.getByTestId('sync-all'));

    await waitFor(() => expect(refreshMailbox).toHaveBeenCalledTimes(1));
    const asks = calls.filter((call) => call.command === 'sync_account_now');
    expect(asks.map((call) => (call.payload as { input: { accountId: string } }).input.accountId))
      .toEqual(['acc_work']);
  });

  test('an account that was already syncing is not reported as a refusal', async () => {
    const { user } = await renderSync([work], { operations: [], syncAccountNow: () => 0 });

    await user.click(screen.getByTestId('sync-all'));

    const account = screen.getByTestId('sync-account');
    await waitFor(() => expect(within(account).getByTestId('sync-account-headline').textContent).toBe('Already mid-check'));
    expect(account.dataset.tone).toBe('working');
  });

  test('a failed sync names the reason against the account that failed', async () => {
    const { user } = await renderSync([work, home], {
      operations: [],
      syncAccountNow: (accountId) => {
        if (accountId === 'acc_home') throw new Error('Manual synchronization is not implemented for pop accounts');
        return 1;
      }
    });

    await user.click(screen.getByTestId('sync-all'));

    const failing = accountRow('acc_home');
    await waitFor(() => expect(failing.dataset.tone).toBe('attention'));
    expect(within(failing).getByText(/not implemented for pop accounts/u)).toBeTruthy();
    expect(accountRow('acc_work').dataset.tone).toBe('good');
    expect(screen.getByTestId('sync-verdict').textContent).toBe('1 account needs attention');
  });

  test('a retrying send reads as work in progress, not as a failure', async () => {
    await renderSync([work], {
      operations: [
        row({
          id: 'op_send',
          field: 'send',
          kind: 'send',
          threadId: null,
          state: 'retrying',
          attempts: 5,
          createdAt: now - 600_000,
          notBefore: now + 120_000
        })
      ]
    });

    const entry = queueEntry('op_send');
    expect(entry.dataset.lane).toBe('working');
    expect(within(entry).getByText('Send message')).toBeTruthy();
    expect(within(entry).getByText('Trying again in 2m')).toBeTruthy();
    expect(within(entry).getByText(/attempt 5 of 8/u)).toBeTruthy();
    expect(screen.getByTestId('queue-verdict').textContent).toBe('1 change on the way');
    // A send that has already been tried cannot be called off, so no button.
    expect(entry.querySelector('button')).toBeNull();
  });

  test('a send whose outcome is unknown is separated out and can be unlocked', async () => {
    const { calls, refreshMailbox, user } = await renderSync([work], {
      operations: [
        row({ id: 'op_stuck', field: 'send', kind: 'send', threadId: null, state: 'outcome_unknown', attempts: 3 }),
        row({ id: 'op_done', state: 'confirmed', confirmedAt: now - 30_000 })
      ]
    });

    expect(screen.getByTestId('queue-verdict').textContent).toBe('1 change needs you');
    const lanes = screen.getAllByTestId('sync-lane');
    expect(lanes.map((lane) => lane.dataset.lane)).toEqual(['attention', 'settled']);

    const stuck = queueEntry('op_stuck');
    expect(stuck.dataset.lane).toBe('attention');
    expect(within(stuck).getByText('Mux never learned whether this went out')).toBeTruthy();

    await user.click(within(stuck).getByRole('button', { name: 'Unlock draft' }));

    await waitFor(() => expect(screen.getByTestId('sync-notice')).toBeTruthy());
    const resolved = calls.filter((call) => call.command === 'resolve_outcome_unknown_send');
    expect(resolved).toHaveLength(1);
    expect(resolved[0].payload).toEqual({ operationId: 'op_stuck' });
    expect(refreshMailbox).toHaveBeenCalledTimes(1);
  });

  test('a queued send can be called off, and the store gets the operation id', async () => {
    const { calls, user } = await renderSync([work], {
      operations: [
        row({ id: 'op_outgoing', field: 'send', kind: 'send', threadId: null, state: 'pending', attempts: 0, notBefore: now + 5_000 })
      ]
    });

    const entry = queueEntry('op_outgoing');
    expect(within(entry).getByText(/^Goes out /u)).toBeTruthy();

    await user.click(within(entry).getByRole('button', { name: 'Call it off' }));

    await waitFor(() => expect(calls.some((call) => call.command === 'undo_operation')).toBe(true));
    expect(calls.find((call) => call.command === 'undo_operation')?.payload)
      .toEqual({ operationId: 'op_outgoing' });
    await waitFor(() => expect(screen.getByTestId('sync-notice').textContent).toBe('Called off before it went out.'));
  });

  test('work the worker already claimed says so instead of silently doing nothing', async () => {
    const { user } = await renderSync([work], {
      operations: [row({ id: 'op_race', state: 'pending' })],
      undoOperation: () => {
        throw new Error('Operation started before it could be undone');
      }
    });

    await user.click(within(queueEntry('op_race')).getByRole('button', { name: 'Call it off' }));

    await waitFor(() => expect(screen.getByTestId('sync-error').textContent).toBe('Operation started before it could be undone'));
  });

  test('terminal work is grouped apart from work that is still moving', async () => {
    await renderSync([work], {
      operations: [
        row({ id: 'op_gone', state: 'failed', attempts: 8 }),
        row({ id: 'op_conflict', state: 'conflicted' }),
        row({ id: 'op_moving', state: 'pending' }),
        row({ id: 'op_odd', state: 'teleported' })
      ]
    });

    expect(screen.getAllByTestId('sync-lane').map((lane) => lane.dataset.lane))
      .toEqual(['working', 'stopped', 'other']);
    expect(within(queueEntry('op_gone')).getByText('Gave up after 8 attempts')).toBeTruthy();
    expect(within(queueEntry('op_conflict')).getByText(/Could not be undone/u)).toBeTruthy();
    // A state this build has never heard of is shown, not hidden or renamed.
    expect(within(queueEntry('op_odd')).getByText(/Unrecognized state/u)).toBeTruthy();
    // Nothing terminal offers an action, because nothing would accept one.
    for (const id of ['op_gone', 'op_conflict', 'op_odd']) {
      expect(queueEntry(id).querySelector('button')).toBeNull();
    }
    expect(screen.getByTestId('queue-verdict').textContent).toBe('2 changes stopped');
  });

  test('a journal that cannot be read says so and offers to try again', async () => {
    let attempt = 0;
    const { calls, user } = await renderSync([work], {
      get operations() {
        attempt += 1;
        if (attempt === 1) throw new Error('Native mailbox state is unavailable');
        return [row({ id: 'op_later', state: 'confirmed', confirmedAt: now })];
      }
    });

    expect(screen.getByTestId('queue-error').textContent).toBe('Native mailbox state is unavailable');

    await user.click(screen.getByTestId('reload-queue'));

    await waitFor(() => expect(screen.queryByTestId('queue-error')).toBeNull());
    expect(queueEntry('op_later')).toBeTruthy();
    expect(calls.filter((call) => call.command === 'list_operations')).toHaveLength(2);
  });

  test('going back to mail is the caller’s decision, not this screen’s', async () => {
    const { close, user } = await renderSync([work], { operations: [] });

    await user.click(screen.getByTestId('sync-close'));

    expect(close).toHaveBeenCalledTimes(1);
  });
});
