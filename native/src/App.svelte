<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import type { UnlistenFn } from '@tauri-apps/api/event';
  import { onDestroy, onMount, tick } from 'svelte';
  import Composer from './Composer.svelte';
  import Icon from './Icon.svelte';
  import InlineReply from './InlineReply.svelte';
  import ThreadConversation from './ThreadConversation.svelte';
  import { replyRecipients } from './replyRecipients.mjs';
  import { mailboxShortcutFor } from './shortcuts.mjs';
  import {
    adjacentMailboxWindowStart,
    boundedRefreshRowTarget,
    collectPagedWindow,
    mailboxRenderWindow,
    mailboxWindowStartForIndex,
    recoverRequiredRow,
    threadPageOrder
  } from './pagedWindow.mjs';
  import {
    MAILBOX_CHANGED_EVENT,
    isMailboxChangedPayload,
    type MailboxChangedPayload
  } from './mailboxEvents';
  import type {
    AccountSummary,
    AttachmentSummary,
    DraftHeaderSummary,
    DraftSummary,
    InvitationSummary,
    MailboxBootstrap,
    MessagePage,
    MessagePageInput,
    MessageSummary,
    OperationActivitySummary,
    OperationSummary,
    SearchInput,
    SearchPage,
    ThreadPage,
    ThreadPageInput,
    ThreadLookupInput,
    ThreadSummary
  } from './types';

  type MailboxView = 'all' | 'inbox' | 'archive' | 'starred' | 'snoozed' | 'sent' | 'trash' | 'drafts';
  type SmartView = '' | 'unread' | 'attachments' | 'invitations' | 'finance';
  type Theme = 'light' | 'dark';
  type Notice = { text: string; operationId?: string; until?: number };
  type VaultState = 'absent' | 'locked' | 'unlocked' | 'unavailable';
  type VaultStatus = { state: VaultState };
  type GmailOAuthResult = { state: 'connected'; accountId: string; email: string };
  type PaletteCommandId = 'compose' | 'archive' | 'snooze' | 'star' | 'read' | 'inbox' | 'starred' | 'activity' | 'theme';
  type PaletteCommand = { id: PaletteCommandId; title: string; description: string; shortcut: string };

  let mailbox: MailboxBootstrap | null = null;
  let theme: Theme = 'light';
  let compactNavigation = false;
  let compactReader = false;
  let navigationOpen = false;
  let navigationMediaQuery: MediaQueryList | null = null;
  let readerMediaQuery: MediaQueryList | null = null;
  let mobileReaderOpen = false;
  let mobileMenuButton: HTMLButtonElement;
  let selectedAccount: string | null = null;
  let selectedView: MailboxView = 'inbox';
  let selectedSmartView: SmartView = '';
  let selectedThreadId: number | null = null;
  let threads: ThreadSummary[] = [];
  let threadCursor: string | null = null;
  let threadHasMore = false;
  let threadLoading = false;
  let threadError = '';
  let threadRequest = 0;
  let threadRenderStart = 0;
  let draftRenderStart = 0;
  let selectedMessages: MessageSummary[] = [];
  let selectedAttachments: AttachmentSummary[] = [];
  let selectedInvitation: InvitationSummary | null = null;
  let messageCursor: string | null = null;
  let messageHasMore = false;
  let messageLoading = false;
  let messageError = '';
  let messageRequest = 0;
  let loadedThreadId: number | null = null;
  let filter = '';
  let filterInput: HTMLInputElement;
  let searchRows: ThreadSummary[] = [];
  let searchCursor: string | null = null;
  let searchHasMore = false;
  let searchError = '';
  let searching = false;
  let searchTimer: number | undefined;
  let searchRequest = 0;
  let focusedReadTimer: number | undefined;
  let bootstrapRequest = 0;
  let loading = true;
  let error = '';
  let composerOpen = false;
  let composerDraft: DraftSummary | null = null;
  let composerReply: ThreadSummary | null = null;
  let composerForward: ThreadSummary | null = null;
  let composerQuote: MessageSummary | null = null;
  let composerReplyAll = false;
  let composerKey = 0;
  let composerReturnFocus: HTMLElement | null = null;
  let draftOpenRequest = 0;
  let inlineReply: InlineReply;
  let threadConversation: ThreadConversation;
  let inlineReplyKey = 0;
  let replyDraft: DraftSummary | null = null;
  let replyDraftLoading = false;
  let replyDraftError = '';
  let replyDraftRequest = 0;
  let notice: Notice | null = null;
  let snoozeDialogOpen = false;
  let commandPaletteOpen = false;
  let commandFilter = '';
  let commandIndex = 0;
  let commandInput: HTMLInputElement;
  let commandDialog: HTMLElement;
  let commandReturnFocus: HTMLElement | null = null;
  let paletteCommands: PaletteCommand[] = [];
  let snoozeDialog: HTMLElement;
  let snoozeReturnFocus: HTMLElement | null = null;
  let activityDialogOpen = false;
  let activityDialog: HTMLElement;
  let activityReturnFocus: HTMLElement | null = null;
  let activityLoading = false;
  let activityError = '';
  let activityRows: OperationActivitySummary[] = [];
  let clock = Date.now();
  let clockTimer: number | undefined;
  let unlistenMailbox: UnlistenFn | null = null;
  let mailboxEventsAvailable = true;
  let destroyed = false;
  let refreshQueued = false;
  let initialRefreshQueued = false;
  let refreshLoop: Promise<void> | null = null;
  let vaultState: VaultState = 'unavailable';
  let vaultDialogOpen = false;
  let vaultBusy = false;
  let vaultError = '';
  let vaultMessage = '';
  let vaultChangingPassphrase = false;
  let vaultResetting = false;
  let vaultPassphrase = '';
  let vaultConfirmation = '';
  let vaultCurrentPassphrase = '';
  let vaultNewPassphrase = '';
  let vaultNewConfirmation = '';
  let vaultResetConfirmation = '';
  let gmailOAuthBusy = false;
  let vaultDialog: HTMLElement;
  let vaultCloseButton: HTMLButtonElement;
  let vaultPassphraseInput: HTMLInputElement;
  let vaultCurrentPassphraseInput: HTMLInputElement;
  let vaultResetInput: HTMLInputElement;
  let vaultLockButton: HTMLButtonElement;
  let blockingDialogOpen = false;

  $: visibleThreads = filter.trim() ? searchRows : threads;
  $: visibleDrafts = (mailbox?.drafts ?? []).filter((draft) => {
    if (selectedAccount !== null && draft.accountId !== selectedAccount) return false;
    const query = filter.trim().toLocaleLowerCase();
    return !query || `${draft.subject} ${draft.recipients} ${draft.ccRecipients} ${draft.bccRecipients}`.toLocaleLowerCase().includes(query);
  });
  $: renderedThreadWindow = mailboxRenderWindow(visibleThreads, threadRenderStart);
  $: renderedDraftWindow = mailboxRenderWindow(visibleDrafts, draftRenderStart);
  $: selectedThread = selectedView === 'drafts'
    ? null
    : visibleThreads.find((thread) => thread.id === selectedThreadId) ?? null;
  $: selectedReplyDraftHeader = selectedThread
    ? mailbox?.drafts.find((draft) => draft.replyToThreadId === selectedThread?.id && !draft.locked) ?? null
    : null;
  $: ownAddresses = [...(mailbox?.accounts.map((account) => account.email) ?? []), 'jordan@mux.example'];
  $: singleReplyRecipients = replyRecipients(selectedMessages, ownAddresses, 'reply');
  $: allReplyRecipients = replyRecipients(selectedMessages, ownAddresses, 'replyAll');
  $: smartViewTitle = ({
    '': '',
    unread: 'Unread',
    attachments: 'Attachments',
    invitations: 'Invitations',
    finance: 'Finance'
  } as const)[selectedSmartView];
  $: viewTitle = smartViewTitle || ({
    all: 'All mail',
    inbox: 'Inbox',
    archive: 'Archive',
    starred: 'Starred',
    snoozed: 'Snoozed',
    sent: 'Sent',
    trash: 'Trash',
    drafts: 'Drafts'
  } as const)[selectedView];
  $: selectedCounts = mailbox?.viewCounts.find((counts) =>
    counts.accountId === selectedAccount
  ) ?? {
    accountId: selectedAccount,
    inbox: 0,
    archive: 0,
    starred: 0,
    snoozed: 0,
    sent: 0,
    all: 0,
    trash: 0
  };
  $: selectedThreadTotal = selectedView === 'drafts' ? visibleDrafts.length : selectedCounts[selectedView];
  $: remainingSeconds = notice?.until ? Math.max(0, Math.ceil((notice.until - clock) / 1_000)) : 0;
  $: blockingDialogOpen = commandPaletteOpen || snoozeDialogOpen || activityDialogOpen || vaultDialogOpen || composerOpen;
  $: if (notice?.until && clock >= notice.until) notice = null;
  $: vaultStateLabel = ({
    absent: 'Not set up',
    locked: 'Locked',
    unlocked: 'Unlocked',
    unavailable: 'Unavailable'
  } as const)[vaultState];
  $: {
    commandFilter;
    selectedThread;
    theme;
    paletteCommands = availablePaletteCommands();
  }

  onMount(() => {
    destroyed = false;
    let savedTheme: string | null = null;
    try {
      savedTheme = window.localStorage.getItem('mux-theme');
    } catch {
      // A denied storage read should not stop the local mailbox from opening.
    }
    setTheme(savedTheme === 'dark' ? 'dark' : 'light', false);
    navigationMediaQuery = window.matchMedia('(max-width: 980px)');
    readerMediaQuery = window.matchMedia('(max-width: 680px)');
    syncResponsiveLayout();
    clockTimer = window.setInterval(() => { clock = Date.now(); }, 200);
    window.addEventListener('focus', refreshWhenForegrounded);
    window.addEventListener('resize', syncResponsiveLayout);
    navigationMediaQuery.addEventListener('change', syncResponsiveLayout);
    readerMediaQuery.addEventListener('change', syncResponsiveLayout);
    document.addEventListener('visibilitychange', refreshWhenVisible);
    void subscribeToMailboxChanges();
    void refreshVaultStatus();
  });

  onDestroy(() => {
    destroyed = true;
    unlistenMailbox?.();
    unlistenMailbox = null;
    window.clearInterval(clockTimer);
    window.clearTimeout(searchTimer);
    window.clearTimeout(focusedReadTimer);
    window.removeEventListener('focus', refreshWhenForegrounded);
    window.removeEventListener('resize', syncResponsiveLayout);
    navigationMediaQuery?.removeEventListener('change', syncResponsiveLayout);
    readerMediaQuery?.removeEventListener('change', syncResponsiveLayout);
    navigationMediaQuery = null;
    readerMediaQuery = null;
    document.removeEventListener('visibilitychange', refreshWhenVisible);
    clearVaultSecrets();
  });

  function setTheme(nextTheme: Theme, persist = true) {
    theme = nextTheme;
    document.documentElement.dataset.theme = nextTheme;
    if (!persist) return;
    try {
      window.localStorage.setItem('mux-theme', nextTheme);
    } catch {
      // Theme persistence is optional; the selected appearance still applies.
    }
  }

  function toggleTheme() {
    setTheme(theme === 'light' ? 'dark' : 'light');
  }

  function availablePaletteCommands(): PaletteCommand[] {
    const items: PaletteCommand[] = [
      { id: 'compose', title: 'Compose message', description: 'Start a new local draft', shortcut: 'C' },
      ...(selectedThread ? [
        { id: 'archive', title: selectedThread.inInbox ? 'Archive conversation' : 'Move conversation to inbox', description: 'Update the focused conversation locally', shortcut: 'E' },
        { id: 'snooze', title: 'Snooze conversation', description: 'Hide it until a chosen time', shortcut: 'H' },
        { id: 'star', title: selectedThread.starred ? 'Remove star' : 'Star conversation', description: 'Update the focused conversation', shortcut: 'S' },
        { id: 'read', title: selectedThread.unread ? 'Mark conversation read' : 'Mark conversation unread', description: 'Update the focused conversation', shortcut: 'U' }
      ] satisfies PaletteCommand[] : []),
      { id: 'inbox', title: 'Go to Inbox', description: 'Open the current unified inbox', shortcut: 'G I' },
      { id: 'starred', title: 'Go to Starred', description: 'Open starred conversations', shortcut: 'G S' },
      { id: 'activity', title: 'Open activity', description: 'Inspect the local operation journal', shortcut: '' },
      { id: 'theme', title: 'Toggle appearance', description: `Switch to ${theme === 'light' ? 'dark' : 'light'} mode`, shortcut: '' }
    ];
    const query = commandFilter.trim().toLocaleLowerCase();
    return query
      ? items.filter((item) => `${item.title} ${item.description}`.toLocaleLowerCase().includes(query))
      : items;
  }

  async function openCommandPalette() {
    commandReturnFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    commandFilter = '';
    commandIndex = 0;
    commandPaletteOpen = true;
    await tick();
    commandInput?.focus();
  }

  function closeCommandPalette(restoreFocus = true) {
    commandPaletteOpen = false;
    commandFilter = '';
    commandIndex = 0;
    if (restoreFocus) restoreDialogFocus(commandReturnFocus);
  }

  function executePaletteCommand(command: PaletteCommand | undefined) {
    if (!command) return;
    closeCommandPalette(false);
    if (command.id === 'compose') openComposer();
    else if (command.id === 'archive') void applyThreadAction(selectedThread?.inInbox ? 'archive' : 'restore', selectedThread?.inInbox ? 'Archived' : 'Restored to inbox');
    else if (command.id === 'snooze') openSnoozeDialog();
    else if (command.id === 'star') void applyThreadAction(selectedThread?.starred ? 'unstar' : 'star', selectedThread?.starred ? 'Star removed' : 'Starred');
    else if (command.id === 'read') void applyThreadAction(selectedThread?.unread ? 'read' : 'unread', selectedThread?.unread ? 'Marked read' : 'Marked unread');
    else if (command.id === 'inbox') selectView('inbox');
    else if (command.id === 'starred') selectView('starred');
    else if (command.id === 'activity') void openActivity();
    else if (command.id === 'theme') toggleTheme();
  }

  function restoreDialogFocus(target: HTMLElement | null) {
    if (!target) return;
    void tick().then(() => {
      if (target.isConnected) target.focus();
    });
  }

  function handleModalKeydown(event: KeyboardEvent, dialog: HTMLElement, close: () => void) {
    if (event.key === 'Escape') {
      event.preventDefault();
      event.stopPropagation();
      close();
      return;
    }
    if (event.key !== 'Tab') return;
    const focusable = [...dialog.querySelectorAll<HTMLElement>(
      'button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), a[href], [tabindex]:not([tabindex="-1"])'
    )].filter((element) => !element.hasAttribute('hidden'));
    if (!focusable.length) {
      event.preventDefault();
      dialog.focus();
      return;
    }
    const first = focusable[0];
    const last = focusable.at(-1) ?? first;
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  }

  function syncResponsiveLayout() {
    compactNavigation = (navigationMediaQuery ?? window.matchMedia('(max-width: 980px)')).matches;
    compactReader = (readerMediaQuery ?? window.matchMedia('(max-width: 680px)')).matches;
    if (!compactNavigation) navigationOpen = false;
    if (!compactReader) mobileReaderOpen = false;
  }

  function toggleNavigation() {
    if (!compactNavigation) return;
    navigationOpen = !navigationOpen;
  }

  async function closeNavigation(restoreFocus = false) {
    navigationOpen = false;
    if (!restoreFocus) return;
    await tick();
    mobileMenuButton?.focus();
  }

  async function closeMobileReader(restoreFocus = false) {
    mobileReaderOpen = false;
    if (!restoreFocus || selectedThreadId === null) return;
    await tick();
    document.querySelector<HTMLButtonElement>(`[data-thread-id="${selectedThreadId}"]`)?.focus();
  }

  function isVaultState(value: unknown): value is VaultState {
    return value === 'absent' || value === 'locked' || value === 'unlocked' || value === 'unavailable';
  }

  function readVaultStatus(value: unknown): VaultStatus {
    if (typeof value === 'object' && value !== null && 'state' in value && isVaultState(value.state)) {
      return { state: value.state };
    }
    throw new Error('Mux returned an invalid credential status.');
  }

  function readGmailOAuthResult(value: unknown): GmailOAuthResult {
    if (
      typeof value === 'object' && value !== null &&
      'state' in value && value.state === 'connected' &&
      'accountId' in value && typeof value.accountId === 'string' && value.accountId.length > 0 &&
      'email' in value && typeof value.email === 'string' && value.email.length > 0
    ) {
      return { state: value.state, accountId: value.accountId, email: value.email };
    }
    throw new Error('Mux returned an invalid Google authorization result.');
  }

  function clearVaultSecrets() {
    vaultPassphrase = '';
    vaultConfirmation = '';
    vaultCurrentPassphrase = '';
    vaultNewPassphrase = '';
    vaultNewConfirmation = '';
  }

  function clearVaultPanel() {
    clearVaultSecrets();
    vaultResetConfirmation = '';
    vaultChangingPassphrase = false;
    vaultResetting = false;
    vaultError = '';
    vaultMessage = '';
  }

  function vaultErrorText(cause: unknown, sensitiveValues: string[] = []): string {
    let text = cause instanceof Error
      ? cause.message
      : typeof cause === 'object' && cause !== null && 'message' in cause && typeof cause.message === 'string'
        ? cause.message
        : String(cause);
    for (const value of sensitiveValues) {
      if (value) text = text.split(value).join('[redacted]');
    }
    return text || 'Credential settings could not be updated.';
  }

  async function focusVaultPrimaryControl() {
    if (!vaultDialogOpen) return;
    await tick();
    if (vaultResetting) vaultResetInput?.focus();
    else if (vaultChangingPassphrase) vaultCurrentPassphraseInput?.focus();
    else if (vaultState === 'absent' || vaultState === 'locked') vaultPassphraseInput?.focus();
    else if (vaultState === 'unlocked') vaultLockButton?.focus();
    else vaultCloseButton?.focus();
  }

  async function refreshVaultStatus(focusAfter = false) {
    try {
      const status = readVaultStatus(await invoke<unknown>('vault_status'));
      vaultState = status.state;
      if (vaultDialogOpen) vaultError = '';
    } catch (cause) {
      vaultState = 'unavailable';
      if (vaultDialogOpen) vaultError = vaultErrorText(cause);
    } finally {
      if (focusAfter) await focusVaultPrimaryControl();
    }
  }

  async function openVaultSettings() {
    navigationOpen = false;
    clearVaultPanel();
    vaultDialogOpen = true;
    if (vaultBusy) await focusVaultPrimaryControl();
    else await refreshVaultStatus(true);
  }

  function closeVaultSettings() {
    if (gmailOAuthBusy) void cancelGmailOAuth();
    clearVaultPanel();
    vaultDialogOpen = false;
  }

  function handleVaultDialogKeydown(event: KeyboardEvent) {
    if (event.key === 'Escape') {
      event.preventDefault();
      event.stopPropagation();
      closeVaultSettings();
      return;
    }
    if (event.key !== 'Tab' || !vaultDialog) return;
    const controls = [...vaultDialog.querySelectorAll<HTMLElement>(
      'button:not(:disabled), input:not(:disabled), [tabindex]:not([tabindex="-1"])'
    )].filter((control) => !control.hasAttribute('hidden'));
    if (!controls.length) return;
    const first = controls[0];
    const last = controls.at(-1)!;
    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  }

  async function createVault() {
    const passphrase = vaultPassphrase;
    const confirmation = vaultConfirmation;
    vaultError = '';
    vaultMessage = '';
    vaultBusy = true;
    try {
      if (!passphrase) throw new Error('Enter a passphrase.');
      if (passphrase !== confirmation) throw new Error('The passphrases do not match.');
      const status = readVaultStatus(await invoke<unknown>('vault_create', { passphrase }));
      vaultState = status.state;
      vaultMessage = 'Credential protection is ready for this session.';
    } catch (cause) {
      vaultError = vaultErrorText(cause, [passphrase, confirmation]);
    } finally {
      clearVaultSecrets();
      vaultBusy = false;
      await focusVaultPrimaryControl();
    }
  }

  async function unlockVault() {
    const passphrase = vaultPassphrase;
    vaultError = '';
    vaultMessage = '';
    vaultBusy = true;
    try {
      if (!passphrase) throw new Error('Enter your passphrase.');
      const status = readVaultStatus(await invoke<unknown>('vault_unlock', { passphrase }));
      vaultState = status.state;
      vaultMessage = 'Account credentials are available for this session.';
    } catch (cause) {
      vaultError = vaultErrorText(cause, [passphrase]);
    } finally {
      clearVaultSecrets();
      vaultBusy = false;
      await focusVaultPrimaryControl();
    }
  }

  async function lockVault() {
    vaultError = '';
    vaultMessage = '';
    vaultBusy = true;
    try {
      const status = readVaultStatus(await invoke<unknown>('vault_lock'));
      vaultState = status.state;
      vaultMessage = 'Account credentials are locked. Your local mailbox remains available.';
    } catch (cause) {
      vaultError = vaultErrorText(cause);
    } finally {
      clearVaultSecrets();
      vaultBusy = false;
      await focusVaultPrimaryControl();
    }
  }

  async function connectGmail() {
    vaultError = '';
    vaultMessage = '';
    gmailOAuthBusy = true;
    try {
      const result = readGmailOAuthResult(await invoke<unknown>('gmail_oauth_begin'));
      vaultMessage = `Connected ${result.email}.`;
    } catch (cause) {
      vaultError = vaultErrorText(cause);
    } finally {
      gmailOAuthBusy = false;
      await focusVaultPrimaryControl();
    }
  }

  async function cancelGmailOAuth() {
    try {
      await invoke<unknown>('gmail_oauth_cancel');
      vaultMessage = 'Cancelling Google authorization…';
    } catch (cause) {
      vaultError = vaultErrorText(cause);
    }
  }

  async function changeVaultPassphrase() {
    const currentPassphrase = vaultCurrentPassphrase;
    const newPassphrase = vaultNewPassphrase;
    const confirmation = vaultNewConfirmation;
    vaultError = '';
    vaultMessage = '';
    vaultBusy = true;
    try {
      if (!currentPassphrase || !newPassphrase) throw new Error('Enter your current and new passphrases.');
      if (newPassphrase !== confirmation) throw new Error('The new passphrases do not match.');
      const status = readVaultStatus(await invoke<unknown>('vault_change_passphrase', {
        currentPassphrase,
        newPassphrase
      }));
      vaultState = status.state;
      vaultChangingPassphrase = false;
      vaultMessage = 'Your passphrase was changed.';
    } catch (cause) {
      vaultError = vaultErrorText(cause, [currentPassphrase, newPassphrase, confirmation]);
    } finally {
      clearVaultSecrets();
      vaultBusy = false;
      await focusVaultPrimaryControl();
    }
  }

  async function resetVault() {
    const confirmation = vaultResetConfirmation;
    vaultError = '';
    vaultMessage = '';
    vaultBusy = true;
    try {
      if (confirmation !== 'RESET') throw new Error('Type RESET to confirm.');
      const status = readVaultStatus(await invoke<unknown>('vault_reset', { confirmation }));
      vaultState = status.state;
      vaultResetting = false;
      vaultMessage = 'Credential protection was reset. Add account credentials again before connecting.';
    } catch (cause) {
      vaultError = vaultErrorText(cause);
    } finally {
      clearVaultSecrets();
      vaultResetConfirmation = '';
      vaultBusy = false;
      await focusVaultPrimaryControl();
    }
  }

  async function showVaultPassphraseChange() {
    clearVaultSecrets();
    vaultError = '';
    vaultMessage = '';
    vaultChangingPassphrase = true;
    vaultResetting = false;
    await focusVaultPrimaryControl();
  }

  async function showVaultReset() {
    clearVaultSecrets();
    vaultResetConfirmation = '';
    vaultError = '';
    vaultMessage = '';
    vaultChangingPassphrase = false;
    vaultResetting = true;
    await focusVaultPrimaryControl();
  }

  async function cancelVaultSubpanel() {
    clearVaultPanel();
    await focusVaultPrimaryControl();
  }

  async function subscribeToMailboxChanges() {
    try {
      const unlisten = await listen<MailboxChangedPayload>(MAILBOX_CHANGED_EVENT, (event) => {
        if (destroyed) return;
        if (!isMailboxChangedPayload(event.payload)) {
          console.warn(`Ignored invalid ${MAILBOX_CHANGED_EVENT} payload`);
          return;
        }
        void queueMailboxRefresh();
      });
      if (destroyed) {
        unlisten();
        return;
      }
      unlistenMailbox = unlisten;
      mailboxEventsAvailable = true;
    } catch {
      mailboxEventsAvailable = false;
      console.error(`Could not subscribe to ${MAILBOX_CHANGED_EVENT}`);
    } finally {
      // Subscribe before the initial read so a commit cannot be missed between
      // loading the snapshot and beginning to observe changes.
      if (!destroyed) void queueMailboxRefresh(true);
    }
  }

  function refreshWhenForegrounded() {
    if (!destroyed) void queueMailboxRefresh();
  }

  function refreshWhenVisible() {
    if (document.visibilityState === 'visible') refreshWhenForegrounded();
  }

  function queueMailboxRefresh(initial = false): Promise<void> {
    refreshQueued = true;
    initialRefreshQueued ||= initial;
    if (!refreshLoop) refreshLoop = runMailboxRefreshes();
    return refreshLoop;
  }

  async function runMailboxRefreshes() {
    try {
      while (refreshQueued && !destroyed) {
        const initial = initialRefreshQueued;
        refreshQueued = false;
        initialRefreshQueued = false;
        await refreshMailbox(initial);
      }
    } finally {
      refreshLoop = null;
    }
  }

  async function refreshMailbox(initial = false) {
    const request = ++bootstrapRequest;
    const threadWindowSize = boundedRefreshRowTarget(threads.length);
    const messageWindowSize = boundedRefreshRowTarget(selectedMessages.length);
    const searchWindowSize = boundedRefreshRowTarget(searchRows.length);
    try {
      const nextMailbox = await invoke<MailboxBootstrap>('mailbox_bootstrap');
      if (request !== bootstrapRequest) return;
      mailbox = nextMailbox;
      error = '';
      if (selectedView === 'drafts') {
        clearThreadPage();
      } else {
        await loadThreads(false, initial, !filter.trim(), threadWindowSize, messageWindowSize);
        if (request !== bootstrapRequest) return;
        if (filter.trim()) await runSearch(false, searchWindowSize, messageWindowSize);
      }
    } catch (cause) {
      if (request !== bootstrapRequest) return;
      error = cause instanceof Error ? cause.message : String(cause);
    } finally {
      if (request === bootstrapRequest) loading = false;
    }
  }

  function mergeById<T extends { id: number | string }>(current: T[], incoming: T[]): T[] {
    const rows = new Map(current.map((row) => [row.id, row]));
    for (const row of incoming) rows.set(row.id, row);
    return [...rows.values()];
  }

  function clearMessages() {
    window.clearTimeout(focusedReadTimer);
    messageRequest += 1;
    selectedMessages = [];
    selectedAttachments = [];
    selectedInvitation = null;
    messageCursor = null;
    messageHasMore = false;
    messageLoading = false;
    messageError = '';
    loadedThreadId = null;
    replyDraftRequest += 1;
    replyDraft = null;
    replyDraftLoading = false;
    replyDraftError = '';
  }

  function clearThreadPage() {
    threadRequest += 1;
    threads = [];
    threadCursor = null;
    threadHasMore = false;
    threadLoading = false;
    threadError = '';
    threadRenderStart = 0;
    draftRenderStart = 0;
    selectedThreadId = null;
    clearMessages();
  }

  async function loadThreads(
    append: boolean,
    initial = false,
    selectAfterLoad = true,
    minimumRows = 50,
    detailMinimumRows = 50
  ) {
    if (selectedView === 'drafts') return;
    const request = ++threadRequest;
    const accountId = selectedAccount;
    const view = selectedView;
    const requiredThreadId = !initial && selectAfterLoad ? selectedThreadId : null;
    const isCurrent = () => request === threadRequest
      && selectedAccount === accountId
      && selectedView === view;
    const input: ThreadPageInput = {
      accountId,
      view,
      cursor: append ? threadCursor : null,
      limit: 50
    };
    threadLoading = true;
    threadError = '';
    try {
      const page = await collectPagedWindow({
        minimumRows: append ? threads.length + 50 : minimumRows,
        initialRows: append ? threads : [],
        initialCursor: append ? threadCursor : null,
        initialHasMore: append ? threadHasMore : true,
        fetchPage: async (cursor) => {
          const nextInput = { ...input, cursor };
          const next = await invoke<ThreadPage>('list_threads', { input: nextInput });
          return { rows: next.threads, nextCursor: next.nextCursor, hasMore: next.hasMore };
        },
        isCurrent,
        compareRows: threadPageOrder
      });
      if (!page) return;
      const restored = await recoverRequiredRow({
        page,
        requiredId: requiredThreadId,
        fetchRequired: async (id) => {
          const lookup: ThreadLookupInput = {
            threadId: Number(id),
            accountId,
            view,
            query: '',
            timezoneOffsetMinutes: -new Date().getTimezoneOffset()
          };
          return invoke<ThreadSummary | null>('get_thread_summary', { input: lookup });
        },
        isCurrent,
        compareRows: threadPageOrder
      });
      if (!restored) return;
      threads = restored.rows;
      threadRenderStart = mailboxWindowStartForIndex(
        Math.max(0, threads.findIndex((thread) => thread.id === selectedThreadId)),
        threads.length,
        threadRenderStart
      );
      threadCursor = restored.nextCursor;
      threadHasMore = restored.hasMore;
      if (!append && selectAfterLoad) {
        const selected = !initial ? threads.find((thread) => thread.id === selectedThreadId) : undefined;
        const next = selected ?? threads[0] ?? null;
        selectedThreadId = next?.id ?? null;
        if (next) await loadThreadMessages(next, false, detailMinimumRows);
        else clearMessages();
      }
    } catch (cause) {
      if (request !== threadRequest) return;
      threadError = cause instanceof Error ? cause.message : String(cause);
    } finally {
      if (request === threadRequest) threadLoading = false;
    }
  }

  async function loadThreadMessages(thread: ThreadSummary, older: boolean, minimumRows = 50) {
    const request = ++messageRequest;
    const input: MessagePageInput = {
      threadId: thread.id,
      cursor: older ? messageCursor : null,
      limit: 50
    };
    messageLoading = true;
    messageError = '';
    if (!older) {
      selectedMessages = [];
      selectedAttachments = [];
      messageCursor = null;
      messageHasMore = false;
      loadedThreadId = null;
    }
    try {
      const attachmentRows = new Map(
        (older ? selectedAttachments : []).map((attachment) => [attachment.id, attachment])
      );
      let invitation = older ? selectedInvitation : null;
      const page = await collectPagedWindow({
        minimumRows: older ? selectedMessages.length + 50 : minimumRows,
        initialRows: older ? selectedMessages : [],
        initialCursor: older ? messageCursor : null,
        initialHasMore: older ? messageHasMore : true,
        fetchPage: async (cursor) => {
          const nextInput = { ...input, cursor };
          const next = await invoke<MessagePage>('get_thread_messages', { input: nextInput });
          for (const attachment of next.attachments) attachmentRows.set(attachment.id, attachment);
          if (next.invitation) invitation = next.invitation;
          return { rows: next.messages, nextCursor: next.nextCursor, hasMore: next.hasMore };
        },
        isCurrent: () => request === messageRequest && selectedThreadId === thread.id
      });
      if (!page) return;
      selectedMessages = page.rows.sort((left, right) => left.sentAt - right.sentAt || left.id - right.id);
      selectedAttachments = [...attachmentRows.values()].sort((left, right) =>
        left.messageId - right.messageId || left.id.localeCompare(right.id)
      );
      selectedInvitation = invitation;
      messageCursor = page.nextCursor;
      messageHasMore = page.hasMore;
      loadedThreadId = thread.id;
      if (!older) await loadReplyDraft(thread.id);
    } catch (cause) {
      if (request !== messageRequest) return;
      messageError = cause instanceof Error ? cause.message : String(cause);
      if (!older) loadedThreadId = null;
    } finally {
      if (request === messageRequest) messageLoading = false;
    }
  }

  async function loadReplyDraft(threadId: number) {
    const header = mailbox?.drafts.find((draft) => draft.replyToThreadId === threadId && !draft.locked) ?? null;
    const request = ++replyDraftRequest;
    replyDraft = null;
    replyDraftError = '';
    replyDraftLoading = Boolean(header);
    if (!header) return;
    try {
      const detail = await invoke<DraftSummary>('get_draft', { draftId: header.id });
      if (request !== replyDraftRequest || selectedThreadId !== threadId) return;
      replyDraft = detail;
    } catch (cause) {
      if (request !== replyDraftRequest || selectedThreadId !== threadId) return;
      replyDraftError = cause instanceof Error ? cause.message : String(cause);
    } finally {
      if (request === replyDraftRequest) replyDraftLoading = false;
    }
  }

  function selectAccount(accountId: string | null) {
    draftOpenRequest += 1;
    navigationOpen = false;
    mobileReaderOpen = false;
    selectedAccount = accountId;
    selectedSmartView = '';
    filter = '';
    resetSearch();
    clearThreadPage();
    if (selectedView !== 'drafts') void loadThreads(false, true);
  }

  function selectView(view: MailboxView) {
    draftOpenRequest += 1;
    navigationOpen = false;
    mobileReaderOpen = false;
    selectedView = view;
    selectedSmartView = '';
    filter = '';
    resetSearch();
    clearThreadPage();
    if (view !== 'drafts') void loadThreads(false, true);
  }

  function selectSmartView(view: Exclude<SmartView, ''>, query: string) {
    draftOpenRequest += 1;
    navigationOpen = false;
    mobileReaderOpen = false;
    selectedSmartView = view;
    selectedView = 'all';
    filter = query;
    resetSearch();
    clearThreadPage();
    void runSearch(false);
  }

  function resetSearch() {
    window.clearTimeout(searchTimer);
    searchRows = [];
    searchCursor = null;
    searchHasMore = false;
    searchError = '';
    searching = false;
    searchRequest += 1;
  }

  function restoreThreadSelection() {
    if (selectedView === 'drafts') return;
    const thread = threads.find((row) => row.id === selectedThreadId) ?? threads[0] ?? null;
    if (thread) selectThread(thread, false);
    else clearMessages();
  }

  function filterChanged() {
    navigationOpen = false;
    if (compactReader) mobileReaderOpen = false;
    selectedSmartView = '';
    window.clearTimeout(searchTimer);
    searchRequest += 1;
    searchError = '';
    if (!filter.trim() || selectedView === 'drafts') {
      resetSearch();
      restoreThreadSelection();
      return;
    }
    searching = true;
    searchRows = [];
    searchCursor = null;
    searchHasMore = false;
    searchTimer = window.setTimeout(() => void runSearch(false), 220);
  }

  async function runSearch(append: boolean, minimumRows = 50, detailMinimumRows = 50) {
    if (!filter.trim() || selectedView === 'drafts') return;
    const view = selectedView;
    const request = ++searchRequest;
    searching = true;
    searchError = '';
    const input: SearchInput = {
      query: filter.trim(),
      accountId: selectedAccount,
      view,
      cursor: append ? searchCursor : null,
      limit: 50,
      timezoneOffsetMinutes: -new Date().getTimezoneOffset()
    };
    const requiredThreadId = selectedThreadId;
    const isCurrent = () => request === searchRequest
      && filter.trim() === input.query
      && selectedAccount === input.accountId
      && selectedView === input.view;
    try {
      const page = await collectPagedWindow({
        minimumRows: append ? searchRows.length + 50 : minimumRows,
        initialRows: append ? searchRows : [],
        initialCursor: append ? searchCursor : null,
        initialHasMore: append ? searchHasMore : true,
        fetchPage: async (cursor) => {
          const nextInput = { ...input, cursor };
          const next = await invoke<SearchPage>('search_threads', { input: nextInput });
          return { rows: next.rows, nextCursor: next.nextCursor, hasMore: next.hasMore };
        },
        isCurrent,
        compareRows: threadPageOrder
      });
      if (!page) return;
      const restored = await recoverRequiredRow({
        page,
        requiredId: requiredThreadId,
        fetchRequired: async (id) => {
          const lookup: ThreadLookupInput = {
            threadId: Number(id),
            accountId: input.accountId,
            view,
            query: input.query,
            timezoneOffsetMinutes: input.timezoneOffsetMinutes
          };
          return invoke<ThreadSummary | null>('get_thread_summary', { input: lookup });
        },
        isCurrent,
        compareRows: threadPageOrder
      });
      if (!restored) return;
      searchRows = restored.rows;
      threadRenderStart = mailboxWindowStartForIndex(
        Math.max(0, searchRows.findIndex((thread) => thread.id === selectedThreadId)),
        searchRows.length,
        threadRenderStart
      );
      searchCursor = restored.nextCursor;
      searchHasMore = restored.hasMore;
      if (!selectedThreadId || !searchRows.some((thread) => thread.id === selectedThreadId)) {
        selectedThreadId = searchRows[0]?.id ?? null;
      }
      const selected = searchRows.find((thread) => thread.id === selectedThreadId) ?? null;
      if (selected && (!append || loadedThreadId !== selected.id)) {
        await loadThreadMessages(selected, false, detailMinimumRows);
      }
      else if (!selected) clearMessages();
    } catch (cause) {
      if (request !== searchRequest) return;
      searchError = cause instanceof Error ? cause.message : String(cause);
    } finally {
      if (request === searchRequest) searching = false;
    }
  }

  function selectThread(thread: ThreadSummary, openReader = true) {
    navigationOpen = false;
    if (openReader && compactReader) mobileReaderOpen = true;
    window.clearTimeout(focusedReadTimer);
    if (thread.unread) {
      const threadId = thread.id;
      focusedReadTimer = window.setTimeout(() => void markFocusedThreadRead(threadId), 650);
    }
    if (selectedThreadId === thread.id && loadedThreadId === thread.id) return;
    selectedThreadId = thread.id;
    const rowIndex = visibleThreads.findIndex((row) => row.id === thread.id);
    threadRenderStart = mailboxWindowStartForIndex(rowIndex, visibleThreads.length, threadRenderStart);
    void loadThreadMessages(thread, false);
  }

  function moveThreadRenderWindow(direction: -1 | 1) {
    threadRenderStart = adjacentMailboxWindowStart(
      renderedThreadWindow.start,
      visibleThreads.length,
      direction
    );
  }

  function moveDraftRenderWindow(direction: -1 | 1) {
    draftRenderStart = adjacentMailboxWindowStart(
      renderedDraftWindow.start,
      visibleDrafts.length,
      direction
    );
  }

  async function markFocusedThreadRead(threadId: number) {
    if (selectedThreadId !== threadId || !selectedThread?.unread) return;
    try {
      await invoke<OperationSummary>('apply_thread_action', { threadId, action: 'read' });
      if (selectedThreadId === threadId) await queueMailboxRefresh();
    } catch {
      // A failed automatic read transition leaves the visible unread state unchanged.
    }
  }

  async function openSnoozeDialog() {
    if (!selectedThread) return;
    snoozeReturnFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    navigationOpen = false;
    snoozeDialogOpen = true;
    await tick();
    snoozeDialog?.querySelector<HTMLElement>('[data-autofocus]')?.focus();
  }

  function closeSnoozeDialog(restoreFocus = true) {
    snoozeDialogOpen = false;
    if (restoreFocus) restoreDialogFocus(snoozeReturnFocus);
  }

  function snoozePresets(): { label: string; description: string; wakeAt: number }[] {
    const now = new Date();
    const laterToday = new Date(now.getTime() + 3 * 60 * 60 * 1_000);
    const tomorrow = new Date(now);
    tomorrow.setDate(tomorrow.getDate() + 1);
    tomorrow.setHours(9, 0, 0, 0);
    const nextWeek = new Date(now);
    const daysUntilMonday = ((8 - nextWeek.getDay()) % 7) || 7;
    nextWeek.setDate(nextWeek.getDate() + daysUntilMonday);
    nextWeek.setHours(9, 0, 0, 0);
    return [
      { label: 'Later today', description: relativeTime(laterToday.getTime()), wakeAt: laterToday.getTime() },
      { label: 'Tomorrow morning', description: relativeTime(tomorrow.getTime()), wakeAt: tomorrow.getTime() },
      {
        label: 'Next week',
        description: new Intl.DateTimeFormat(undefined, { weekday: 'short', hour: 'numeric', minute: '2-digit' }).format(nextWeek),
        wakeAt: nextWeek.getTime()
      }
    ];
  }

  async function snoozeUntil(wakeAt: number) {
    const threadId = selectedThread?.id;
    if (!threadId) return;
    closeSnoozeDialog();
    try {
      const operation = await invoke<OperationSummary>('snooze_thread', { threadId, wakeAt });
      notice = { text: 'Conversation snoozed', operationId: operation.id };
      await queueMailboxRefresh();
    } catch (cause) {
      notice = { text: cause instanceof Error ? cause.message : String(cause) };
    }
  }

  async function respondToInvitation(response: InvitationSummary['response']) {
    const threadId = selectedThread?.id;
    if (!threadId || !selectedInvitation) return;
    try {
      const operation = await invoke<OperationSummary>('rsvp_thread', { threadId, response });
      notice = { text: 'Invitation response updated', operationId: operation.id };
      await queueMailboxRefresh();
    } catch (cause) {
      notice = { text: cause instanceof Error ? cause.message : String(cause) };
    }
  }

  function formatInvitationTime(invitation: InvitationSummary): string {
    try {
      const start = new Intl.DateTimeFormat(undefined, {
        weekday: 'short', month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit',
        timeZone: invitation.timezone
      }).format(invitation.startAt);
      const end = new Intl.DateTimeFormat(undefined, {
        hour: 'numeric', minute: '2-digit', timeZone: invitation.timezone
      }).format(invitation.endAt);
      return `${start} – ${end}`;
    } catch {
      return `${new Date(invitation.startAt).toLocaleString()} – ${new Date(invitation.endAt).toLocaleTimeString()}`;
    }
  }

  async function openActivity() {
    activityReturnFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    navigationOpen = false;
    activityDialogOpen = true;
    activityLoading = true;
    activityError = '';
    await tick();
    activityDialog?.querySelector<HTMLElement>('[data-autofocus]')?.focus();
    try {
      activityRows = await invoke<OperationActivitySummary[]>('list_operations', { input: { limit: 50 } });
    } catch (cause) {
      activityRows = [];
      activityError = cause instanceof Error ? cause.message : String(cause);
    } finally {
      activityLoading = false;
    }
  }

  function closeActivity(restoreFocus = true) {
    activityDialogOpen = false;
    if (restoreFocus) restoreDialogFocus(activityReturnFocus);
  }

  async function resolveUnknownSend(operationId: string) {
    try {
      await invoke<OperationSummary>('resolve_outcome_unknown_send', { operationId });
      notice = { text: 'Draft unlocked without retrying the uncertain send' };
      await queueMailboxRefresh();
      await openActivity();
    } catch (cause) {
      activityError = cause instanceof Error ? cause.message : String(cause);
    }
  }

  function operationTitle(operation: OperationActivitySummary): string {
    return ({
      archive: 'Archive conversation',
      restore: 'Move to inbox',
      read: 'Mark as read',
      unread: 'Mark as unread',
      star: 'Star conversation',
      unstar: 'Remove star',
      snooze: 'Snooze conversation',
      rsvp: 'Invitation response',
      send: 'Send message'
    } as Record<string, string>)[operation.kind] ?? operation.kind.replaceAll('_', ' ');
  }

  function openComposer(draft: DraftSummary | null = null) {
    draftOpenRequest += 1;
    composerReturnFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    navigationOpen = false;
    composerDraft = draft;
    composerReply = null;
    composerForward = null;
    composerQuote = null;
    composerReplyAll = false;
    composerKey += 1;
    composerOpen = true;
  }

  async function openSavedDraft(header: DraftHeaderSummary) {
    const request = ++draftOpenRequest;
    try {
      const draft = await invoke<DraftSummary>('get_draft', { draftId: header.id });
      if (request !== draftOpenRequest || selectedView !== 'drafts') return;
      openComposer(draft);
    } catch (cause) {
      if (request !== draftOpenRequest || selectedView !== 'drafts') return;
      notice = { text: cause instanceof Error ? cause.message : String(cause) };
    }
  }

  function openReply(mode: 'reply' | 'replyAll' = 'reply') {
    if (!selectedThread) return;
    void inlineReply?.focus(mode);
  }

  function openReplyModal(draft: DraftSummary | null, mode: 'reply' | 'replyAll') {
    if (!selectedThread) return;
    draftOpenRequest += 1;
    composerReturnFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    composerDraft = draft;
    composerReply = selectedThread;
    composerForward = null;
    composerQuote = selectedMessages.at(-1) ?? null;
    composerReplyAll = mode === 'replyAll';
    composerKey += 1;
    composerOpen = true;
  }

  function openForward() {
    if (!selectedThread) return;
    draftOpenRequest += 1;
    composerReturnFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    composerDraft = null;
    composerReply = null;
    composerForward = selectedThread;
    composerQuote = selectedMessages.at(-1) ?? null;
    composerReplyAll = false;
    composerKey += 1;
    composerOpen = true;
  }

  function updateDraftLocally(draft: DraftSummary) {
    if (!mailbox) return;
    const { body: _body, bodyHtml: _bodyHtml, ...header } = draft;
    const drafts = mailbox.drafts.some((existing) => existing.id === draft.id)
      ? mailbox.drafts.map((existing) => existing.id === draft.id ? header : existing)
      : [header, ...mailbox.drafts];
    mailbox = { ...mailbox, drafts };
    if (draft.replyToThreadId === selectedThreadId) replyDraft = draft;
  }

  async function composerQueued(event: CustomEvent<OperationSummary>) {
    composerOpen = false;
    restoreDialogFocus(composerReturnFocus);
    await queueMailboxRefresh();
    inlineReplyKey += 1;
    notice = { text: composerReply ? 'Reply queued' : 'Message queued', operationId: event.detail.id, until: event.detail.notBefore };
  }

  async function composerClosed() {
    composerOpen = false;
    restoreDialogFocus(composerReturnFocus);
    await queueMailboxRefresh();
    inlineReplyKey += 1;
  }

  async function inlineQueued(event: CustomEvent<OperationSummary>) {
    await queueMailboxRefresh();
    inlineReplyKey += 1;
    notice = { text: 'Reply queued', operationId: event.detail.id, until: event.detail.notBefore };
  }

  async function composerDeleted() {
    composerOpen = false;
    restoreDialogFocus(composerReturnFocus);
    notice = { text: 'Draft deleted' };
    await queueMailboxRefresh();
    inlineReplyKey += 1;
  }

  async function applyThreadAction(action: string, label: string) {
    if (!selectedThread) return;
    try {
      const operation = await invoke<OperationSummary>('apply_thread_action', {
        threadId: selectedThread.id,
        action
      });
      await queueMailboxRefresh();
      notice = { text: label, operationId: operation.id, until: Date.now() + 5_000 };
    } catch (cause) {
      notice = { text: cause instanceof Error ? cause.message : String(cause) };
    }
  }

  async function undoNotice() {
    if (!notice?.operationId) return;
    const operationId = notice.operationId;
    try {
      await invoke('undo_operation', { operationId });
      notice = { text: 'Undone' };
      await queueMailboxRefresh();
    } catch (cause) {
      notice = { text: cause instanceof Error ? cause.message : String(cause) };
    }
  }

  function handleKeydown(event: KeyboardEvent) {
    if (event.defaultPrevented) return;
    if (commandPaletteOpen) {
      if (event.key === 'Escape') {
        event.preventDefault();
        closeCommandPalette();
      } else if (event.key === 'ArrowDown') {
        event.preventDefault();
        commandIndex = Math.min(Math.max(0, paletteCommands.length - 1), commandIndex + 1);
      } else if (event.key === 'ArrowUp') {
        event.preventDefault();
        commandIndex = Math.max(0, commandIndex - 1);
      } else if (event.key === 'Enter') {
        event.preventDefault();
        executePaletteCommand(paletteCommands[commandIndex]);
      }
      return;
    }
    if ((event.metaKey || event.ctrlKey) && event.key.toLocaleLowerCase() === 'k' && !composerOpen && !snoozeDialogOpen && !activityDialogOpen && !vaultDialogOpen) {
      event.preventDefault();
      void openCommandPalette();
      return;
    }
    if (snoozeDialogOpen) {
      if (event.key === 'Escape') {
        event.preventDefault();
        closeSnoozeDialog();
      }
      return;
    }
    if (activityDialogOpen) {
      if (event.key === 'Escape') {
        event.preventDefault();
        closeActivity();
      }
      return;
    }
    if (vaultDialogOpen) {
      if (event.key === 'Escape') {
        event.preventDefault();
        closeVaultSettings();
      }
      return;
    }
    if (composerOpen) return;
    if (event.key === 'Escape') {
      if (navigationOpen) {
        event.preventDefault();
        void closeNavigation(true);
        return;
      }
      if (compactReader && mobileReaderOpen) {
        event.preventDefault();
        void closeMobileReader(true);
        return;
      }
      if (filter) {
        event.preventDefault();
        filter = '';
        selectedSmartView = '';
        filterInput?.blur();
        resetSearch();
        restoreThreadSelection();
      } else if (document.activeElement instanceof HTMLElement && document.activeElement !== document.body) {
        document.activeElement.blur();
      }
      return;
    }
    const action = mailboxShortcutFor(event);
    if (!action) return;
    event.preventDefault();
    if (action === 'focus-filter') {
      filterInput?.focus();
      return;
    }
    if (action === 'compose') {
      openComposer();
      return;
    }
    if (action === 'reply') {
      openReply('reply');
      return;
    }
    if (action === 'reply-all') {
      openReply('replyAll');
      return;
    }
    if (action === 'forward') {
      openForward();
      return;
    }
    if (action === 'archive') {
      void applyThreadAction(selectedThread?.inInbox ? 'archive' : 'restore', selectedThread?.inInbox ? 'Archived' : 'Restored to inbox');
      return;
    }
    if (action === 'toggle-star') {
      void applyThreadAction(selectedThread?.starred ? 'unstar' : 'star', selectedThread?.starred ? 'Star removed' : 'Starred');
      return;
    }
    if (action === 'snooze') {
      openSnoozeDialog();
      return;
    }
    if (action === 'toggle-unread') {
      void applyThreadAction(selectedThread?.unread ? 'read' : 'unread', selectedThread?.unread ? 'Marked read' : 'Marked unread');
      return;
    }
    if (!visibleThreads.length) return;
    const currentIndex = Math.max(0, visibleThreads.findIndex((thread) => thread.id === selectedThread?.id));
    const direction = action === 'next-thread' ? 1 : -1;
    const nextIndex = Math.min(visibleThreads.length - 1, Math.max(0, currentIndex + direction));
    selectThread(visibleThreads[nextIndex]);
  }

  function accountFor(accountId: string): AccountSummary | undefined {
    return mailbox?.accounts.find((account) => account.id === accountId);
  }

  function initials(name: string): string {
    return name.split(/\s+/u).map((part) => part[0] ?? '').join('').slice(0, 2).toLocaleUpperCase();
  }

  function relativeTime(timestamp: number): string {
    const delta = Date.now() - timestamp;
    if (delta < 86_400_000) {
      return new Intl.DateTimeFormat(undefined, { hour: 'numeric', minute: '2-digit' }).format(timestamp);
    }
    return new Intl.DateTimeFormat(undefined, { month: 'short', day: 'numeric' }).format(timestamp);
  }

</script>

<svelte:window on:keydown={handleKeydown} />

<main
  class="shell"
  class:is-loading={loading}
  class:nav-open={navigationOpen}
  class:reader-mobile-open={mobileReaderOpen}
  data-theme={theme}
  data-testid="mux-shell"
>
  <header class="topbar" inert={blockingDialogOpen}>
    <button
      bind:this={mobileMenuButton}
      class="mobile-menu-button"
      type="button"
      aria-label={navigationOpen ? 'Close mailbox navigation' : 'Open mailbox navigation'}
      aria-controls="native-navigation"
      aria-expanded={navigationOpen}
      data-action="toggle-navigation"
      data-testid="mobile-menu-button"
      disabled={!mailbox}
      on:click={toggleNavigation}
    >
      <Icon name="menu" size={19} />
    </button>
    <div class="brand" aria-label="Mux">
      <span class="mark" aria-hidden="true"><i></i><i></i><i></i></span>
      <strong>mux</strong>
    </div>
    <div class="workspace">
      <span>{selectedAccount === null ? 'All accounts' : accountFor(selectedAccount)?.name}</span>
      <small>{viewTitle}</small>
    </div>
    <label class="filter">
      <Icon name="search" size={17} />
      <input bind:this={filterInput} bind:value={filter} aria-label="Search mail" placeholder="Search mail or use from:, after:, is:…" data-testid="mailbox-search" on:input={filterChanged} />
      <kbd>/</kbd>
    </label>
    <div class="topbar-actions">
      <button
        class="topbar-icon-button"
        type="button"
        aria-label="Open command palette"
        title="Command palette (⌘K)"
        data-action="open-command-palette"
        on:click={openCommandPalette}
      >
        <Icon name="command" size={19} />
      </button>
      <button
        class="topbar-icon-button"
        type="button"
        aria-label={theme === 'light' ? 'Switch to dark appearance' : 'Switch to light appearance'}
        title={theme === 'light' ? 'Dark appearance' : 'Light appearance'}
        data-action="toggle-theme"
        on:click={toggleTheme}
      >
        <Icon name={theme === 'light' ? 'moon' : 'sun'} size={19} />
      </button>
    </div>
  </header>

  {#if loading}
    <section class="state-card">
      <div class="spinner" aria-hidden="true"></div>
      <h1>Opening Mux</h1>
      <p>Loading your local mail.</p>
    </section>
  {:else if error && !mailbox}
    <section class="state-card error" role="alert">
      <h1>Mux could not open</h1>
      <p>{error}</p>
    </section>
  {:else if mailbox}
    <div class="workspace-grid" data-testid="mailbox-workspace" inert={blockingDialogOpen}>
      <nav
        id="native-navigation"
        class="sidebar"
        aria-label="Mailbox navigation"
        aria-hidden={compactNavigation && !navigationOpen}
        inert={compactNavigation && !navigationOpen}
        data-testid="mailbox-navigation"
      >
        <button class="compose" data-action="compose" data-testid="compose-button" on:click={() => openComposer()}>
          <Icon name="compose" size={18} /> Compose
        </button>

        <section class="nav-section">
          <button class:is-active={selectedView === 'inbox'} data-action="view-inbox" on:click={() => selectView('inbox')}>
            <span class="nav-icon"><Icon name="inbox" size={17} /></span><strong>Inbox</strong>
            <em>{selectedCounts.inbox}</em>
          </button>
          <button class:is-active={selectedView === 'starred'} data-action="view-starred" on:click={() => selectView('starred')}>
            <span class="nav-icon"><Icon name="star" size={17} /></span><strong>Starred</strong>
            <em>{selectedCounts.starred}</em>
          </button>
          <button class:is-active={selectedView === 'snoozed' && !selectedSmartView} data-action="view-snoozed" on:click={() => selectView('snoozed')}>
            <span class="nav-icon"><Icon name="clock" size={17} /></span><strong>Snoozed</strong>
            <em>{selectedCounts.snoozed}</em>
          </button>
          <button class:is-active={selectedView === 'sent'} data-action="view-sent" on:click={() => selectView('sent')}>
            <span class="nav-icon"><Icon name="sent" size={17} /></span><strong>Sent</strong>
            <em>{selectedCounts.sent}</em>
          </button>
          <button class:is-active={selectedView === 'drafts'} data-action="view-drafts" on:click={() => selectView('drafts')}>
            <span class="nav-icon"><Icon name="drafts" size={17} /></span><strong>Drafts</strong>
            <em>{mailbox.drafts.filter((draft) => selectedAccount === null || draft.accountId === selectedAccount).length}</em>
          </button>
          <button class:is-active={selectedView === 'archive'} data-action="view-archive" on:click={() => selectView('archive')}>
            <span class="nav-icon"><Icon name="archive" size={17} /></span><strong>Archive</strong>
            <em>{selectedCounts.archive}</em>
          </button>
          <button class:is-active={selectedView === 'trash'} data-action="view-trash" on:click={() => selectView('trash')}>
            <span class="nav-icon"><Icon name="trash" size={17} /></span><strong>Trash</strong>
            <em>{selectedCounts.trash}</em>
          </button>
          <button class:is-active={selectedView === 'all' && !selectedSmartView && !filter} data-action="view-all" on:click={() => selectView('all')}>
            <span class="nav-icon"><Icon name="allMail" size={17} /></span><strong>All mail</strong>
            <em>{selectedCounts.all}</em>
          </button>
        </section>

        <p class="section-label">Smart views</p>
        <section class="smart-views">
          <button class:is-active={selectedSmartView === 'unread'} data-action="smart-unread" on:click={() => selectSmartView('unread', 'is:unread')}>
            <span class="smart-dot blue"></span><span>Unread</span>
          </button>
          <button class:is-active={selectedSmartView === 'attachments'} data-action="smart-attachments" on:click={() => selectSmartView('attachments', 'has:attachment')}>
            <span class="smart-dot violet"></span><span>Attachments</span>
          </button>
          <button class:is-active={selectedSmartView === 'invitations'} data-action="smart-invitations" on:click={() => selectSmartView('invitations', 'has:invite')}>
            <span class="smart-dot green"></span><span>Invitations</span>
          </button>
          <button class:is-active={selectedSmartView === 'finance'} data-action="smart-finance" on:click={() => selectSmartView('finance', 'category:Finance')}>
            <span class="smart-dot amber"></span><span>Finance</span>
          </button>
        </section>

        <p class="section-label">Accounts</p>
        <section class="accounts">
          <button class:is-active={selectedAccount === null} data-action="account-all" on:click={() => selectAccount(null)}>
            <span class="account-dot all"></span><span>All accounts</span>
            <em>{mailbox.accounts.reduce((total, account) => total + account.unread, 0)}</em>
          </button>
          {#each mailbox.accounts as account}
            <button class:is-active={selectedAccount === account.id} data-action="select-account" data-account-id={account.id} on:click={() => selectAccount(account.id)}>
              <span class="account-dot" style:background={account.color}></span>
              <span>{account.name}</span>
              <em>{account.unread}</em>
            </button>
          {/each}
        </section>

        <button class="vault-status-button" type="button" data-action="open-vault" on:click={openVaultSettings} aria-haspopup="dialog">
          <span class="vault-status-dot" class:is-absent={vaultState === 'absent'} class:is-locked={vaultState === 'locked'} class:is-unlocked={vaultState === 'unlocked'}></span>
          <span><strong>Account security</strong><small>{vaultStateLabel}</small></span>
          <span class="vault-status-arrow" aria-hidden="true"><Icon name="chevron" size={16} /></span>
        </button>

        <button class="projection-status" type="button" data-action="open-activity" on:click={openActivity} aria-haspopup="dialog">
          <span class:has-error={Boolean(threadError) || !mailboxEventsAvailable}></span>
          <div>
            <strong>{threadError ? 'Mailbox needs attention' : mailboxEventsAvailable ? 'Up to date' : 'Refreshes on focus'}</strong>
            <small>{threadError || (mailboxEventsAvailable ? 'Changes save on this Mac' : 'Live updates are unavailable')}</small>
          </div>
        </button>
      </nav>

      <section class="thread-pane" aria-label={viewTitle}>
        <header class="pane-heading">
          <div><small>{selectedAccount === null ? 'All accounts' : accountFor(selectedAccount)?.email}</small><h1>{filter ? `Search ${viewTitle.toLocaleLowerCase()}` : viewTitle}</h1></div>
          <span>{searching ? 'Searching…' : `${filter.trim() && selectedView !== 'drafts' ? visibleThreads.length : selectedThreadTotal} ${selectedView === 'drafts' ? 'drafts' : 'threads'}`}</span>
        </header>

        <div class="thread-list" data-testid="thread-list">
          {#if selectedView === 'drafts'}
            {#if renderedDraftWindow.start > 0}
              <button class="search-more" type="button" on:click={() => moveDraftRenderWindow(-1)}>Show previous loaded drafts</button>
            {/if}
            {#each renderedDraftWindow.rows as draft (draft.id)}
              <button class="thread-row draft-row" data-testid="thread-row" data-draft-id={draft.id} on:click={() => openSavedDraft(draft)}>
                <span class="thread-accent" style:background={draft.accountColor}></span>
                <span class="avatar" style:--avatar-color={draft.accountColor}>D</span>
                <span class="thread-copy">
                  <span class="thread-line"><strong>{draft.recipients || 'No recipient'}</strong><time>{relativeTime(draft.updatedAt)}</time></span>
                  <span class="subject">{draft.subject || 'No subject'}</span>
                  <span class="snippet">{draft.locked ? 'Sending…' : 'Saved locally'}</span>
                </span>
              </button>
            {:else}
              <div class="empty"><strong>No drafts</strong><span>Compose a message and it will autosave here.</span></div>
            {/each}
            {#if renderedDraftWindow.end < visibleDrafts.length}
              <button class="search-more" type="button" on:click={() => moveDraftRenderWindow(1)}>Show next loaded drafts</button>
            {/if}
          {:else}
            {#if renderedThreadWindow.start > 0}
              <button class="search-more" type="button" on:click={() => moveThreadRenderWindow(-1)}>Show previous loaded threads</button>
            {/if}
            {#each renderedThreadWindow.rows as thread (thread.id)}
              <button
                class="thread-row"
                class:is-selected={thread.id === selectedThread?.id}
                class:is-unread={thread.unread}
                data-testid="thread-row"
                data-thread-id={thread.id}
                on:click={() => selectThread(thread)}
                aria-pressed={thread.id === selectedThread?.id}
              >
                <span class="thread-accent" style:background={accountFor(thread.accountId)?.color}></span>
                <span class="avatar" style:--avatar-color={accountFor(thread.accountId)?.color}>{initials(thread.participants)}</span>
                <span class="thread-copy">
                  <span class="thread-line"><strong>{thread.participants}</strong><time>{relativeTime(thread.latestAt)}</time></span>
                  <span class="subject">{thread.starred ? '★ ' : ''}{thread.subject}</span>
                  <span class="snippet">{thread.snippet}</span>
                </span>
                {#if thread.unread}<span class="unread-dot" aria-label="Unread"></span>{/if}
              </button>
            {:else}
              {#if threadLoading && !filter.trim()}
                <div class="empty search-state"><strong>Loading mailbox</strong><span>Reading the next local page.</span></div>
              {:else if searching}
                <div class="empty search-state"><strong>Searching mail</strong><span>Press Esc to clear.</span></div>
              {:else if searchError}
                <div class="empty search-state has-error" role="alert"><strong>Search needs attention</strong><span>{searchError}</span></div>
              {:else}
                <div class="empty"><strong>No {viewTitle.toLocaleLowerCase()} mail</strong><span>{filter ? 'Try a broader search.' : 'Choose another mailbox or account.'}</span></div>
              {/if}
            {/each}
            {#if threadError}
              <div class="empty search-state has-error" role="alert"><strong>Mailbox page could not load</strong><span>{threadError}</span></div>
            {/if}
            {#if renderedThreadWindow.end < visibleThreads.length}
              <button class="search-more" type="button" on:click={() => moveThreadRenderWindow(1)}>Show next loaded threads</button>
            {:else if filter && searchHasMore && !searching}
              <button class="search-more" type="button" on:click={() => runSearch(true)}>Load 50 more results</button>
            {:else if !filter.trim() && threadHasMore && !threadLoading}
              <button class="search-more" type="button" on:click={() => loadThreads(true)}>Load 50 more threads</button>
            {/if}
          {/if}
        </div>
        {#if selectedView !== 'drafts'}
        <footer class="key-hint"><kbd>J</kbd><kbd>K</kbd><span>Navigate</span><kbd>R</kbd><span>Reply</span><kbd>A</kbd><span>All</span></footer>
        {/if}
      </section>

      <article
        class="reader"
        aria-live="polite"
        aria-hidden={compactReader && !mobileReaderOpen}
        inert={compactReader && !mobileReaderOpen}
        data-testid="reader"
      >
        {#if selectedThread}
          <header class="reader-toolbar">
            <button class="reader-back-button" type="button" data-action="reader-back" data-testid="reader-back-button" aria-label="Back to thread list" on:click={() => closeMobileReader(true)}>
              <Icon name="chevron" size={18} />
            </button>
            {#if selectedView === 'trash'}
              <button data-action="untrash" on:click={() => applyThreadAction('untrash', 'Removed from Trash')}>
                <Icon name="inbox" size={16} /> <span>Remove from Trash</span>
              </button>
            {:else}
              <button data-action={selectedThread.inInbox ? 'archive' : 'restore'} on:click={() => applyThreadAction(selectedThread.inInbox ? 'archive' : 'restore', selectedThread.inInbox ? 'Archived' : 'Restored to inbox')}>
                <Icon name="archive" size={16} /> <span>{selectedThread.inInbox ? 'Archive' : 'Move to inbox'}</span>
              </button>
              <button data-action="delete" on:click={() => applyThreadAction('delete', 'Moved to Trash')}>
                <Icon name="trash" size={16} /> <span>Trash</span>
              </button>
            {/if}
            <button data-action="snooze" on:click={openSnoozeDialog}>
              <Icon name="clock" size={16} /> <span>Snooze</span>
            </button>
            <button data-action={selectedThread.unread ? 'read' : 'unread'} on:click={() => applyThreadAction(selectedThread.unread ? 'read' : 'unread', selectedThread.unread ? 'Marked read' : 'Marked unread')}>
              <Icon name="mail" size={16} /> <span>{selectedThread.unread ? 'Mark read' : 'Mark unread'}</span>
            </button>
            <button data-action="reply" on:click={() => openReply('reply')}><Icon name="reply" size={16} /> <span>Reply</span></button>
            <button data-action="reply-all" on:click={() => openReply('replyAll')}><Icon name="replyAll" size={16} /> <span>Reply all</span></button>
            <button data-action="forward" on:click={openForward}><Icon name="forward" size={16} /> <span>Forward</span></button>
            {#if selectedThread.unread}
              <button data-action="jump-unread" on:click={() => threadConversation?.jumpToUnread()}><Icon name="unread" size={16} /> <span>Unread</span></button>
            {/if}
            <button data-action="jump-newest" on:click={() => threadConversation?.jumpToNewest()}><Icon name="newest" size={16} /> <span>Newest</span></button>
            <span class="toolbar-spacer"></span>
            <button class:is-starred={selectedThread.starred} data-action={selectedThread.starred ? 'unstar' : 'star'} aria-label={selectedThread.starred ? 'Remove star' : 'Star'} title={selectedThread.starred ? 'Remove star (S)' : 'Star (S)'} on:click={() => applyThreadAction(selectedThread.starred ? 'unstar' : 'star', selectedThread.starred ? 'Star removed' : 'Starred')}>
              <Icon name="star" size={18} filled={selectedThread.starred} />
            </button>
          </header>

          <div class="reader-scroll">
            <div class="subject-heading">
              <div class="subject-line">
                <h1 data-testid="reader-subject">{selectedThread.subject}</h1>
                <span class="category-pill">{selectedThread.category}</span>
              </div>
              <p>{selectedThread.participants} · {selectedThread.messageCount} messages · {accountFor(selectedThread.accountId)?.name}</p>
            </div>

            {#if messageLoading && loadedThreadId !== selectedThread.id}
              <div class="empty search-state"><strong>Loading conversation</strong><span>Reading messages from the local store.</span></div>
            {:else if messageError && loadedThreadId !== selectedThread.id}
              <div class="empty search-state has-error" role="alert"><strong>Conversation needs attention</strong><span>{messageError}</span></div>
            {:else}
              {#if messageHasMore && !messageLoading}
                <button class="search-more" type="button" on:click={() => loadThreadMessages(selectedThread, true)}>Show older</button>
              {/if}
              {#if messageError}
                <div class="empty search-state has-error" role="alert"><strong>Older messages could not load</strong><span>{messageError}</span></div>
              {/if}

              {#if selectedInvitation}
                <section class="invitation-card" data-testid="invitation-card" aria-label={`Invitation: ${selectedInvitation.title}`}>
                  <div class="invitation-icon" aria-hidden="true"><Icon name="calendar" size={22} /></div>
                  <div class="invitation-copy">
                    <h2>{selectedInvitation.title}</h2>
                    <p><strong>{formatInvitationTime(selectedInvitation)}</strong></p>
                    <p>{selectedInvitation.location} · Organized by {selectedInvitation.organizer}</p>
                    {#if selectedInvitation.conflictText}
                      <p class="invitation-conflict">{selectedInvitation.conflictText}</p>
                    {/if}
                    <div class="invitation-actions" aria-label="Respond to invitation">
                      <button class:is-selected={selectedInvitation.response === 'accepted'} type="button" on:click={() => respondToInvitation('accepted')}>Accept</button>
                      <button class:is-selected={selectedInvitation.response === 'tentative'} type="button" on:click={() => respondToInvitation('tentative')}>Maybe</button>
                      <button class:is-selected={selectedInvitation.response === 'declined'} type="button" on:click={() => respondToInvitation('declined')}>Decline</button>
                    </div>
                  </div>
                </section>
              {/if}

              {#key `${selectedThread.id}:${selectedMessages[0]?.id ?? 0}:${selectedMessages.length}`}
                <ThreadConversation
                  bind:this={threadConversation}
                  messages={selectedMessages}
                  attachments={selectedAttachments}
                  accountColor={accountFor(selectedThread.accountId)?.color}
                  threadUnread={selectedThread.unread}
                />
              {/key}
            {/if}
          </div>
          {#if loadedThreadId === selectedThread.id && !replyDraftLoading}
            <footer class="quick-reply-host" data-testid="inline-reply">
              {#if replyDraftError && selectedReplyDraftHeader}
                <div class="reply-draft-error" role="alert">
                  <span>Saved reply could not load: {replyDraftError}</span>
                  <button type="button" on:click={() => loadReplyDraft(selectedThread.id)}>Try again</button>
                </div>
              {:else}
                {#key `${selectedThread.id}:${inlineReplyKey}`}
                  <InlineReply
                    bind:this={inlineReply}
                    accounts={mailbox.accounts}
                    thread={selectedThread}
                    draft={replyDraft}
                    replyRecipient={singleReplyRecipients}
                    replyAllRecipients={allReplyRecipients}
                    on:saved={(event) => updateDraftLocally(event.detail)}
                    on:queued={inlineQueued}
                    on:popout={(event) => openReplyModal(event.detail.draft, event.detail.mode)}
                  />
                {/key}
              {/if}
            </footer>
          {/if}
        {:else if selectedView === 'drafts'}
          <div class="empty reader-empty"><strong>Your drafts</strong><span>Select a draft to keep writing, or start a new message.</span><button on:click={() => openComposer()}>Compose</button></div>
        {:else}
          <div class="empty reader-empty"><strong>Select a conversation</strong><span>Choose a message from the list.</span></div>
        {/if}
      </article>
    </div>
    <button
      class="mobile-scrim"
      type="button"
      aria-label="Close mailbox navigation"
      aria-hidden={!navigationOpen}
      tabindex={navigationOpen ? 0 : -1}
      data-action="close-navigation"
      data-testid="mobile-navigation-scrim"
      on:click={() => closeNavigation(true)}
    ></button>
  {/if}

  {#if notice}
    <div class="undo-toast" role="status" data-testid="undo-toast" inert={blockingDialogOpen}>
      <span>{notice.text}</span>
      {#if notice.operationId && (!notice.until || remainingSeconds > 0)}
        <button on:click={undoNotice}>Undo</button>
      {/if}
    </div>
  {/if}

  {#if composerOpen && mailbox}
    <div class="composer-mount" data-testid="composer">
      {#key composerKey}
        <Composer
          accounts={mailbox.accounts}
          draft={composerDraft}
          replyThread={composerReply}
          replyRecipient={composerReplyAll ? allReplyRecipients : singleReplyRecipients}
          replyAll={composerReplyAll}
          forwardThread={composerForward}
          quoteSource={composerQuote}
          on:close={composerClosed}
          on:saved={(event) => updateDraftLocally(event.detail)}
          on:deleted={composerDeleted}
          on:queued={composerQueued}
        />
      {/key}
    </div>
  {/if}

  {#if commandPaletteOpen}
    <div class="modal-backdrop command-backdrop" role="presentation" on:click|self={() => closeCommandPalette()}>
      <div bind:this={commandDialog} class="modal-card command-dialog" role="dialog" aria-modal="true" aria-label="Command palette" data-testid="command-palette" tabindex="-1" on:keydown={(event) => handleModalKeydown(event, commandDialog, closeCommandPalette)}>
        <label class="command-filter">
          <Icon name="search" size={17} />
          <input
            bind:this={commandInput}
            bind:value={commandFilter}
            aria-label="Command search"
            autocomplete="off"
            placeholder="Type a command…"
            on:input={() => { commandIndex = 0; }}
          />
          <kbd>esc</kbd>
        </label>
        <div class="command-list" role="listbox" aria-label="Available commands">
          {#if paletteCommands.length}
            {#each paletteCommands as command, index (command.id)}
              <button
                class:is-selected={commandIndex === index}
                type="button"
                role="option"
                aria-selected={commandIndex === index}
                on:mouseenter={() => { commandIndex = index; }}
                on:click={() => executePaletteCommand(command)}
              >
                <span><strong>{command.title}</strong><small>{command.description}</small></span>
                <kbd>{command.shortcut || 'enter'}</kbd>
              </button>
            {/each}
          {:else}
            <div class="empty"><strong>No matching command</strong></div>
          {/if}
        </div>
      </div>
    </div>
  {/if}

  {#if snoozeDialogOpen && selectedThread}
    <div class="modal-backdrop" role="presentation" on:click|self={() => closeSnoozeDialog()}>
      <div bind:this={snoozeDialog} class="modal-card snooze-dialog" role="dialog" aria-modal="true" aria-labelledby="snooze-title" data-testid="snooze-dialog" tabindex="-1" on:keydown={(event) => handleModalKeydown(event, snoozeDialog, closeSnoozeDialog)}>
        <header>
          <div><small>Bring it back</small><h1 id="snooze-title">Snooze conversation</h1></div>
          <button class="icon-button" type="button" aria-label="Close snooze options" on:click={() => closeSnoozeDialog()}>×</button>
        </header>
        <div class="modal-content snooze-options">
          {#each snoozePresets() as preset, index}
            <button type="button" data-autofocus={index === 0 ? '' : undefined} on:click={() => snoozeUntil(preset.wakeAt)}>
              <span><strong>{preset.label}</strong><small>{preset.description}</small></span>
              <Icon name="chevron" size={16} />
            </button>
          {/each}
        </div>
      </div>
    </div>
  {/if}

  {#if activityDialogOpen}
    <div class="modal-backdrop" role="presentation" on:click|self={() => closeActivity()}>
      <div bind:this={activityDialog} class="modal-card activity-dialog" role="dialog" aria-modal="true" aria-labelledby="activity-title" data-testid="activity-dialog" tabindex="-1" on:keydown={(event) => handleModalKeydown(event, activityDialog, closeActivity)}>
        <header>
          <div><small>Local operation journal</small><h1 id="activity-title">Activity</h1></div>
          <button class="icon-button" data-autofocus type="button" aria-label="Close activity" on:click={() => closeActivity()}>×</button>
        </header>
        <div class="modal-content activity-list" aria-live="polite">
          {#if activityLoading}
            <div class="empty"><strong>Loading activity</strong></div>
          {:else if activityError}
            <div class="empty search-state has-error" role="alert"><strong>Activity needs attention</strong><span>{activityError}</span></div>
          {:else if !activityRows.length}
            <div class="empty"><strong>No local activity yet</strong><span>Mailbox changes and sends will appear here.</span></div>
          {:else}
            {#each activityRows as operation (operation.id)}
              <article class="activity-row">
                <div>
                  <strong>{operationTitle(operation)}</strong>
                  <span>{relativeTime(operation.createdAt)} · {operation.state.replaceAll('_', ' ')}</span>
                </div>
                {#if operation.field === 'send' && operation.state === 'outcome_unknown'}
                  <button type="button" on:click={() => resolveUnknownSend(operation.id)}>Unlock draft</button>
                {/if}
              </article>
            {/each}
          {/if}
        </div>
      </div>
    </div>
  {/if}

  {#if vaultDialogOpen}
    <div class="vault-backdrop" role="presentation" on:click|self={closeVaultSettings}>
      <div
        class="vault-dialog"
        data-testid="vault-dialog"
        data-vault-state={vaultState}
        bind:this={vaultDialog}
        role="dialog"
        aria-modal="true"
        aria-labelledby="vault-title"
        aria-describedby="vault-description"
        tabindex="-1"
        on:keydown={handleVaultDialogKeydown}
      >
        <header>
          <div>
            <small>Account security</small>
            <h1 id="vault-title">Credential access</h1>
          </div>
          <button bind:this={vaultCloseButton} class="icon-button" type="button" aria-label="Close credential settings" on:click={closeVaultSettings}>×</button>
        </header>

        <div class="vault-content">
          <div class="vault-state-line">
            <span class="vault-status-dot" class:is-absent={vaultState === 'absent'} class:is-locked={vaultState === 'locked'} class:is-unlocked={vaultState === 'unlocked'}></span>
            <strong>{vaultStateLabel}</strong>
          </div>

          {#if vaultResetting}
            <div class="vault-panel vault-reset-panel">
              <h2>Reset credential protection?</h2>
              <p id="vault-description">This removes every saved account credential from this Mac. Your local mailbox is not deleted, but accounts cannot connect until credentials are added again.</p>
              <form on:submit|preventDefault={resetVault}>
                <label>
                  <span>Type RESET to confirm</span>
                  <input bind:this={vaultResetInput} bind:value={vaultResetConfirmation} autocomplete="off" spellcheck="false" disabled={vaultBusy} />
                </label>
                <div class="vault-actions">
                  <button type="button" disabled={vaultBusy} on:click={cancelVaultSubpanel}>Cancel</button>
                  <button class="vault-danger-button" type="submit" disabled={vaultBusy || vaultResetConfirmation !== 'RESET'}>{vaultBusy ? 'Resetting…' : 'Reset credentials'}</button>
                </div>
              </form>
            </div>
          {:else if vaultChangingPassphrase}
            <div class="vault-panel">
              <h2>Change passphrase</h2>
              <p id="vault-description">Use your current passphrase to set a new one.</p>
              <form on:submit|preventDefault={changeVaultPassphrase}>
                <label>
                  <span>Current passphrase</span>
                  <input bind:this={vaultCurrentPassphraseInput} bind:value={vaultCurrentPassphrase} type="password" autocomplete="current-password" disabled={vaultBusy} />
                </label>
                <label>
                  <span>New passphrase</span>
                  <input bind:value={vaultNewPassphrase} type="password" autocomplete="new-password" disabled={vaultBusy} />
                </label>
                <label>
                  <span>Confirm new passphrase</span>
                  <input bind:value={vaultNewConfirmation} type="password" autocomplete="new-password" disabled={vaultBusy} />
                </label>
                <div class="vault-actions">
                  <button type="button" disabled={vaultBusy} on:click={cancelVaultSubpanel}>Cancel</button>
                  <button class="vault-primary-button" type="submit" disabled={vaultBusy}>{vaultBusy ? 'Changing…' : 'Change passphrase'}</button>
                </div>
              </form>
            </div>
          {:else if vaultState === 'absent'}
            <div class="vault-panel">
              <h2>Protect account credentials</h2>
              <p id="vault-description">Choose a passphrase for credentials saved on this Mac. You will enter it again after restarting Mux.</p>
              <form on:submit|preventDefault={createVault}>
                <label>
                  <span>Passphrase</span>
                  <input bind:this={vaultPassphraseInput} bind:value={vaultPassphrase} type="password" autocomplete="new-password" disabled={vaultBusy} />
                </label>
                <label>
                  <span>Confirm passphrase</span>
                  <input bind:value={vaultConfirmation} type="password" autocomplete="new-password" disabled={vaultBusy} />
                </label>
                <div class="vault-actions">
                  <button class="vault-primary-button" type="submit" disabled={vaultBusy}>{vaultBusy ? 'Creating…' : 'Create'}</button>
                </div>
              </form>
            </div>
          {:else if vaultState === 'locked'}
            <div class="vault-panel">
              <h2>Unlock account credentials</h2>
              <p id="vault-description">Your local mailbox stays available while locked. Unlock only when Mux needs to connect or send.</p>
              <form on:submit|preventDefault={unlockVault}>
                <label>
                  <span>Passphrase</span>
                  <input bind:this={vaultPassphraseInput} bind:value={vaultPassphrase} type="password" autocomplete="current-password" disabled={vaultBusy} />
                </label>
                <div class="vault-actions">
                  <button class="vault-primary-button" type="submit" disabled={vaultBusy}>{vaultBusy ? 'Unlocking…' : 'Unlock'}</button>
                </div>
              </form>
            </div>
          {:else if vaultState === 'unlocked'}
            <div class="vault-panel">
              <h2>Credentials are available</h2>
              <p id="vault-description">Mux can use saved credentials during this session. They lock again when Mux quits.</p>
              <section class="vault-provider-card" aria-labelledby="gmail-connect-title">
                <div>
                  <h3 id="gmail-connect-title">Gmail</h3>
                  <p>Authorize this Mac in your browser.</p>
                </div>
                {#if gmailOAuthBusy}
                  <button type="button" on:click={cancelGmailOAuth}>Cancel</button>
                {:else}
                  <button class="vault-primary-button" type="button" disabled={vaultBusy} on:click={connectGmail}>Connect Gmail</button>
                {/if}
              </section>
              <div class="vault-actions vault-actions-split">
                <button type="button" disabled={vaultBusy || gmailOAuthBusy} on:click={showVaultPassphraseChange}>Change passphrase</button>
                <button bind:this={vaultLockButton} class="vault-primary-button" type="button" disabled={vaultBusy || gmailOAuthBusy} on:click={lockVault}>{vaultBusy ? 'Locking…' : 'Lock now'}</button>
              </div>
            </div>
          {:else}
            <div class="vault-panel">
              <h2>Credential settings are unavailable</h2>
              <p id="vault-description">This build could not open credential settings. Your local mailbox is still available.</p>
              <div class="vault-actions">
                <button type="button" on:click={() => refreshVaultStatus(true)}>Try again</button>
              </div>
            </div>
          {/if}

          {#if vaultError}<p class="vault-feedback has-error" role="alert">{vaultError}</p>{/if}
          {#if vaultMessage}<p class="vault-feedback" role="status">{vaultMessage}</p>{/if}
        </div>

        {#if !vaultResetting && vaultState !== 'absent'}
          <footer>
            <button class="vault-reset-link" type="button" disabled={vaultBusy || gmailOAuthBusy} on:click={showVaultReset}>Reset saved credentials…</button>
          </footer>
        {/if}
      </div>
    </div>
  {/if}
</main>
