<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { onMount } from 'svelte';
  import Icon from './Icon.svelte';
  import {
    accountHealth,
    describeSyncFailure,
    groupQueue,
    mailboxVerdict,
    queueVerdict
  } from './syncStatus';
  import type { SyncObservation } from './syncStatus';
  import type { AccountSummary, OperationActivitySummary } from './types';

  export let close: () => void;
  export let accounts: AccountSummary[];
  /// A sync changes what the mailbox should show, and the mailbox is not this
  /// screen's to reload.
  export let refreshMailbox: () => Promise<void>;

  /// The store will not hand over more than a hundred journal rows, and a whole
  /// screen has room for all of them — the dialog this replaces asked for fifty
  /// because it was a dialog.
  const QUEUE_LIMIT = 100;

  let rows: OperationActivitySummary[] = [];
  let queueLoading = true;
  let queueError = '';
  let actionError = '';
  let actionNotice = '';
  let busyOperationId = '';
  let observations: Record<string, SyncObservation> = {};
  /// Read once per tick rather than per expression, so every relative time on
  /// the screen is measured from the same moment.
  let now = Date.now();

  $: accountRows = accounts.map((account) => ({
    account,
    health: accountHealth(account, observations[account.id], now)
  }));
  $: overall = mailboxVerdict(accounts, observations, now);
  $: queueGroups = groupQueue(rows, now);
  $: queueSummary = queueVerdict(rows);
  /// Only an unanswered request of this screen's own disables the buttons. A
  /// stored state that says `syncing` shows as such but must not lock the one
  /// control that could get it moving again.
  $: anyAsking = accountRows.some((entry) => entry.health.asking);

  async function loadQueue() {
    queueLoading = true;
    queueError = '';
    try {
      rows = await invoke<OperationActivitySummary[]>('list_operations', {
        input: { limit: QUEUE_LIMIT }
      });
    } catch (cause) {
      rows = [];
      queueError = describeSyncFailure(cause);
    } finally {
      queueLoading = false;
      now = Date.now();
    }
  }

  /// Records what came back without reloading anything, so a batch can settle
  /// before the mailbox is asked to catch up once.
  async function askAccount(account: AccountSummary) {
    const askedAt = Date.now();
    observations = { ...observations, [account.id]: { askedAt } };
    try {
      const scheduled = await invoke<number>('sync_account_now', {
        input: { accountId: account.id }
      });
      observations = {
        ...observations,
        [account.id]: { askedAt, settledAt: Date.now(), scheduled }
      };
    } catch (cause) {
      observations = {
        ...observations,
        [account.id]: { askedAt, settledAt: Date.now(), error: describeSyncFailure(cause) }
      };
    }
  }

  async function afterSync() {
    now = Date.now();
    await refreshMailbox();
    await loadQueue();
  }

  async function syncOne(account: AccountSummary) {
    actionError = '';
    actionNotice = '';
    await askAccount(account);
    await afterSync();
  }

  /// Every account at once, then one reload: the mailbox does not become more
  /// correct for being reloaded once per account.
  async function syncAll() {
    actionError = '';
    actionNotice = '';
    await Promise.all(accounts.map(askAccount));
    await afterSync();
  }

  async function unlockDraft(operationId: string) {
    actionError = '';
    actionNotice = '';
    busyOperationId = operationId;
    try {
      await invoke('resolve_outcome_unknown_send', { operationId });
      actionNotice = 'Draft unlocked. Mux will not try that send again.';
      await afterSync();
    } catch (cause) {
      actionError = describeSyncFailure(cause);
    } finally {
      busyOperationId = '';
    }
  }

  async function callOff(operationId: string) {
    actionError = '';
    actionNotice = '';
    busyOperationId = operationId;
    try {
      await invoke('undo_operation', { operationId });
      actionNotice = 'Called off before it went out.';
      await afterSync();
    } catch (cause) {
      /// The worker may have claimed the row between the button appearing and
      /// being pressed, and the store says so rather than pretending.
      actionError = describeSyncFailure(cause);
    } finally {
      busyOperationId = '';
    }
  }

  onMount(() => {
    void loadQueue();
    /// This screen is mostly countdowns — a send's undo window, a retry's
    /// backoff — and a countdown that only moves when you press something is
    /// worse than no countdown at all.
    const tick = window.setInterval(() => { now = Date.now(); }, 1_000);
    return () => window.clearInterval(tick);
  });
</script>

<section class="sync-screen" data-testid="sync-screen">
  <header class="sync-head">
    <button class="sync-back" type="button" title="Back to mail" data-testid="sync-close" on:click={close}>
      <span aria-hidden="true"><Icon name="chevron" size={15} /></span>Back to mail
    </button>
    <div class="sync-head-title">
      <h1>Sync</h1>
      <p class="sync-verdict" data-tone={overall.tone} data-testid="sync-verdict">{overall.text}</p>
    </div>
    <div class="sync-head-actions">
      <button
        class="sync-primary"
        type="button"
        data-action="sync-all"
        data-testid="sync-all"
        title="Check every account for new mail right now"
        disabled={!accounts.length || anyAsking}
        on:click={syncAll}
      >{anyAsking ? 'Checking…' : 'Sync all accounts'}</button>
      <button
        class="sync-secondary"
        type="button"
        data-action="reload-queue"
        data-testid="reload-queue"
        title="Read the operation journal again"
        disabled={queueLoading}
        on:click={loadQueue}
      >Reload queue</button>
    </div>
  </header>

  <div class="sync-body">
    <section class="sync-card" aria-labelledby="sync-accounts-title">
      <h2 id="sync-accounts-title">Accounts</h2>
      {#if !accounts.length}
        <p class="sync-empty" data-testid="sync-accounts-empty">
          No accounts are connected yet, so there is nothing to sync. Add one in Settings.
        </p>
      {:else}
        <ul class="sync-account-list">
          {#each accountRows as entry (entry.account.id)}
            <li
              data-testid="sync-account"
              data-account-id={entry.account.id}
              data-tone={entry.health.tone}
              data-sync-state={entry.account.syncState ?? 'none'}
            >
              <span class="sync-account-dot" style:--avatar-color={entry.account.color}></span>
              <div class="sync-account-text">
                <strong>{entry.account.name}</strong>
                <small>{entry.account.email}</small>
                <span class="sync-account-headline" data-testid="sync-account-headline">{entry.health.headline}</span>
                <small class="sync-account-detail">{entry.health.detail}</small>
              </div>
              <div class="sync-account-side">
                <em>{entry.account.unread} unread of {entry.account.total}</em>
                <button
                  class="sync-secondary"
                  type="button"
                  data-action="sync-account"
                  data-account-id={entry.account.id}
                  title={`Check ${entry.account.name} for new mail right now`}
                  disabled={entry.health.asking}
                  on:click={() => syncOne(entry.account)}
                >{entry.health.syncing ? 'Checking…' : 'Sync now'}</button>
              </div>
            </li>
          {/each}
        </ul>
      {/if}
    </section>

    <section class="sync-card" aria-labelledby="sync-queue-title">
      <h2 id="sync-queue-title">Queue</h2>
      <p class="sync-card-hint">
        Every change you make is written down here first, then sent. Nothing is lost if Mux closes
        or the network drops.
      </p>
      <p class="sync-verdict" data-tone={queueSummary.tone} data-testid="queue-verdict">{queueSummary.text}</p>

      {#if actionNotice}<p class="sync-feedback" role="status" data-testid="sync-notice">{actionNotice}</p>{/if}
      {#if actionError}<p class="sync-feedback has-error" role="alert" data-testid="sync-error">{actionError}</p>{/if}

      <div class="sync-queue" aria-live="polite">
        {#if queueLoading}
          <p class="sync-empty" data-testid="queue-loading">Reading the journal…</p>
        {:else if queueError}
          <p class="sync-empty has-error" role="alert" data-testid="queue-error">{queueError}</p>
        {:else if !rows.length}
          <p class="sync-empty" data-testid="queue-empty">
            Nothing is queued. Changes you make to your mail will appear here on their way out.
          </p>
        {:else}
          {#each queueGroups as group (group.lane)}
            <section class="sync-lane" data-lane={group.lane} data-testid="sync-lane">
              <h3>{group.title}<span>{group.entries.length}</span></h3>
              <ul>
                {#each group.entries as entry (entry.id)}
                  <li data-testid="queue-entry" data-operation-id={entry.id} data-lane={entry.lane}>
                    <div class="sync-entry-text">
                      <strong>{entry.title}</strong>
                      <span class="sync-entry-status">{entry.status}</span>
                      <small>{entry.detail}</small>
                    </div>
                    {#if entry.action === 'unlock-draft'}
                      <button
                        class="sync-secondary"
                        type="button"
                        data-action="unlock-draft"
                        title="Put the message back in your drafts and stop asking"
                        disabled={busyOperationId === entry.id}
                        on:click={() => unlockDraft(entry.id)}
                      >Unlock draft</button>
                    {:else if entry.action === 'call-off'}
                      <button
                        class="sync-secondary"
                        type="button"
                        data-action="call-off"
                        title="Drop this change before it is sent"
                        disabled={busyOperationId === entry.id}
                        on:click={() => callOff(entry.id)}
                      >Call it off</button>
                    {/if}
                  </li>
                {/each}
              </ul>
            </section>
          {/each}
        {/if}
      </div>
    </section>
  </div>
</section>

<style>
  /* Scoped rather than in styles.css only because that file is being edited
     elsewhere right now; nothing here needs to escape the component. */
  .sync-screen {
    height: calc(100% - var(--topbar));
    display: grid;
    grid-template-rows: auto minmax(0, 1fr);
    overflow: hidden;
  }

  .sync-head {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto;
    grid-template-areas: 'back back' 'title actions';
    align-items: end;
    gap: .6em 1em;
    padding: var(--gap-lg) clamp(24px, 4vw, 56px) var(--gap-lg);
    border-bottom: 1px solid var(--border);
    background: var(--surface-muted);
  }
  .sync-back {
    grid-area: back;
    height: 1.8em;
    display: inline-flex;
    align-items: center;
    gap: .5em;
    justify-self: start;
    padding: 0 .5em 0 0;
    color: var(--text-faint);
    background: transparent;
    cursor: pointer;
    font-size: var(--text-0);
    font-weight: 620;
  }
  .sync-back:hover { color: var(--text); }
  .sync-back span { display: inline-flex; transform: rotate(180deg); }
  .sync-head-title { grid-area: title; min-width: 0; }
  .sync-head-title h1 { margin: 0; color: var(--text); font-size: var(--text-4); letter-spacing: -.03em; }
  .sync-head-actions { grid-area: actions; display: flex; flex-wrap: wrap; gap: .5em; }

  .sync-verdict {
    margin: .4em 0 0;
    color: var(--text-soft);
    font-size: var(--text-1);
    font-weight: 700;
  }
  /* Tone is an attribute rather than a class so the same four words describe an
     account, the queue, and the screen as a whole. */
  .sync-verdict[data-tone='good'] { color: var(--success); }
  .sync-verdict[data-tone='working'] { color: var(--accent-strong); }
  .sync-verdict[data-tone='attention'] { color: var(--warning); }
  .sync-verdict[data-tone='unknown'] { color: var(--text-soft); }

  .sync-primary,
  .sync-secondary {
    height: 2em;
    flex: 0 0 auto;
    padding: 0 .8em;
    border: 1px solid var(--border-strong);
    border-radius: 8px;
    cursor: pointer;
    font-size: var(--text-1);
    font-weight: 700;
  }
  .sync-primary { border-color: var(--accent); color: var(--accent-strong); background: var(--accent-soft); }
  .sync-secondary { color: var(--text-soft); background: var(--surface); }
  .sync-primary:hover:enabled,
  .sync-secondary:hover:enabled { color: var(--accent-strong); background: var(--accent-soft); }
  .sync-primary:disabled,
  .sync-secondary:disabled { color: var(--text-faint); background: var(--surface-muted); cursor: progress; }

  .sync-body { min-height: 0; padding: var(--gap-xl) clamp(24px, 4vw, 56px) calc(var(--gap-xl) + var(--gap-lg)); overflow-y: auto; }

  .sync-card {
    max-width: calc(640px * var(--ui-scale));
    margin-bottom: var(--gap-lg);
    padding: var(--gap-lg) 1.1em;
    border: 1px solid var(--border);
    border-radius: 12px;
    background: var(--surface);
  }
  .sync-card h2 { margin: 0; color: var(--text); font-size: var(--text-2); }
  .sync-card-hint { margin: .4em 0 0; color: var(--text-soft); font-size: var(--text-1); line-height: 1.55; }

  .sync-empty { margin: .75em 0 0; color: var(--text-soft); font-size: var(--text-1); line-height: 1.55; }
  .sync-empty.has-error { color: var(--danger); }
  .sync-feedback { margin: .65em 0 0; color: var(--text-soft); font-size: var(--text-1); }
  .sync-feedback.has-error { color: var(--danger); }

  .sync-account-list { margin: var(--gap-md) 0 0; padding: 0; list-style: none; display: grid; gap: var(--gap-sm); }
  .sync-account-list li {
    display: grid;
    grid-template-columns: auto minmax(0, 1fr) auto;
    align-items: start;
    gap: .7em;
    padding: var(--gap-md) .75em;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: var(--surface-muted);
  }
  .sync-account-dot {
    width: .6em;
    height: .6em;
    margin-top: .25em;
    border-radius: 50%;
    background: var(--avatar-color, var(--accent));
  }
  .sync-account-text { min-width: 0; display: flex; flex-direction: column; gap: 2px; }
  .sync-account-text strong { overflow: hidden; color: var(--text); font-size: var(--text-1); text-overflow: ellipsis; white-space: nowrap; }
  .sync-account-text small { color: var(--text-faint); font-size: var(--text-0); }
  .sync-account-headline { margin-top: .25em; color: var(--text-soft); font-size: var(--text-1); font-weight: 700; }
  .sync-account-detail { line-height: 1.5; }
  .sync-account-list li[data-tone='good'] .sync-account-headline { color: var(--success); }
  .sync-account-list li[data-tone='working'] .sync-account-headline { color: var(--accent-strong); }
  .sync-account-list li[data-tone='attention'] .sync-account-headline { color: var(--warning); }
  .sync-account-side { display: grid; justify-items: end; gap: .45em; }
  .sync-account-side em { color: var(--text-faint); font-size: var(--text-0); font-style: normal; white-space: nowrap; }

  .sync-queue { margin-top: var(--gap-xs); }
  .sync-lane { margin-top: var(--gap-lg); }
  .sync-lane h3 {
    display: flex;
    align-items: center;
    gap: .45em;
    margin: 0 0 .45em;
    color: var(--text-faint);
    font-size: var(--text-0);
    font-weight: 800;
    letter-spacing: .13em;
    text-transform: uppercase;
  }
  /* The count takes the heading's size; only the tracking comes off. */
  .sync-lane h3 span {
    min-width: 1.2em;
    padding: .05em .35em;
    border-radius: 6px;
    color: var(--text-soft);
    background: var(--surface-muted);
    font-weight: 700;
    letter-spacing: 0;
    text-align: center;
  }
  .sync-lane ul { margin: 0; padding: 0; list-style: none; display: grid; gap: var(--gap-xs); }
  .sync-lane li {
    display: grid;
    grid-template-columns: minmax(0, 1fr) auto;
    align-items: center;
    gap: .7em;
    padding: var(--gap-sm) .75em;
    border: 1px solid var(--border);
    border-left: 3px solid var(--border-strong);
    border-radius: 9px;
    background: var(--surface-muted);
  }
  /* The stripe is the only thing that has to be readable from across the room:
     which of these rows is a problem. */
  .sync-lane[data-lane='attention'] li { border-left-color: var(--warning); }
  .sync-lane[data-lane='working'] li { border-left-color: var(--accent); }
  .sync-lane[data-lane='stopped'] li { border-left-color: var(--danger); }
  .sync-lane[data-lane='settled'] li { border-left-color: var(--border-strong); }
  .sync-entry-text { min-width: 0; display: flex; flex-direction: column; gap: 2px; }
  .sync-entry-text strong { color: var(--text); font-size: var(--text-1); }
  .sync-entry-text small { color: var(--text-faint); font-size: var(--text-0); }
  .sync-entry-status { color: var(--text-soft); font-size: var(--text-1); }
  .sync-lane[data-lane='attention'] .sync-entry-status { color: var(--warning); font-weight: 700; }
  .sync-lane[data-lane='stopped'] .sync-entry-status { color: var(--danger); font-weight: 700; }
  .sync-lane[data-lane='working'] .sync-entry-status { color: var(--accent-strong); }

  @media (max-width: 720px) {
    .sync-head { grid-template-columns: minmax(0, 1fr); grid-template-areas: 'back' 'title' 'actions'; align-items: start; }
    .sync-account-list li { grid-template-columns: auto minmax(0, 1fr); }
    .sync-account-side { grid-column: 1 / -1; justify-items: start; }
  }
</style>
