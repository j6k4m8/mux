<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { listen } from '@tauri-apps/api/event';
  import type { UnlistenFn } from '@tauri-apps/api/event';
  import { onDestroy, onMount, tick } from 'svelte';
  import { flip } from 'svelte/animate';
  import Composer from './Composer.svelte';
  import Icon from './Icon.svelte';
  import InlineReply from './InlineReply.svelte';
  import { swipeGesture } from './swipeGesture';
  import type { SwipeSide } from './swipeGesture';
  import JumpDialog from './JumpDialog.svelte';
  import MailboxSidebar from './MailboxSidebar.svelte';
  import SearchField from './SearchField.svelte';
  import ShortcutSheet from './ShortcutSheet.svelte';
  import SettingsScreen from './SettingsScreen.svelte';
  import StatsScreen from './StatsScreen.svelte';
  import SyncScreen from './SyncScreen.svelte';
  import ThreadConversation from './ThreadConversation.svelte';
  import {
    applyAccentToRoot,
    applyAppearanceToRoot,
    densityChoices,
    DEFAULT_APPEARANCE,
    DEFAULT_FONT,
    persistAppearance,
    readSavedAppearance,
    refreshChoices,
    swipeChoices,
    swipeLabel,
    textSizes
  } from './appearance';
  import type { Appearance, Density, SwipeAction } from './appearance';
  import { replyRecipients } from './replyRecipients';
  import { chordShortcutFor, mailboxShortcutFor, shortcutLabel } from './shortcuts';
  import { trapModalKeydown } from './modalFocus';
  import { mailboxJumpRows } from './jumpDialog';
  import { moveDestinations } from './moveTargets';
  import { motionTiming, slideAway, slideReveal, toastMotion } from './motion';
  import {
    persistSidebarLayout,
    readSidebarLayout,
    toggleFolderAccount,
    toggleSavedSearches,
    toggleSmartViews,
    DEFAULT_SIDEBAR_LAYOUT
  } from './sidebarLayout';
  import type { SidebarLayout } from './sidebarLayout';
  import {
    persistThemePreference,
    readThemePreference,
    resolveTheme,
    watchSystemTheme
  } from './theme';
  import type { ThemePreference } from './theme';
  import { searchIsNarrowed, searchScopeView, searchViewFor, viewScopeTerm } from './searchQuery';
  import {
    confirmationNotice,
    failureNotice,
    noticeExpired,
    noticeOffersUndo,
    sampleNotice,
    undoDeadline,
    undoableNotice
  } from './notices';
  import type { Notice } from './notices';
  import { addSavedSearch, persistSavedSearches, readSavedSearches, removeSavedSearch } from './savedSearches';
  import type { SavedSearch } from './savedSearches';
  import {
    adjacentMailboxWindowStart,
    boundedRefreshRowTarget,
    collectPagedWindow,
    mailboxRenderWindow,
    mailboxWindowStartForIndex,
    recoverRequiredRow,
    threadPageOrder
  } from './pagedWindow';
  import {
    MAILBOX_CHANGED_EVENT,
    isMailboxChangedPayload,
    type MailboxChangedPayload
  } from './mailboxEvents';
  import type {
    AccountSummary,
    AttachmentSummary,
    ContainerSummary,
    DraftHeaderSummary,
    DraftSummary,
    InvitationSummary,
    MailboxBootstrap,
    MailboxView,
    MessagePage,
    MessagePageInput,
    MessageSummary,
    OperationActivitySummary,
    OperationSummary,
    SearchInput,
    SearchPage,
    SettingsSection,
    SmartView,
    Theme,
    ThreadLookupInput,
    ThreadPage,
    ThreadPageInput,
    ThreadSummary,
    ViewCountSummary
  } from './types';

  type PaletteCommandId = 'compose' | 'archive' | 'move' | 'snooze' | 'star' | 'read' | 'inbox' | 'starred' | 'shortcuts' | 'activity' | 'theme' | 'settings';
  type PaletteCommand = { id: PaletteCommandId; title: string; description: string; shortcut: string };

  let mailbox: MailboxBootstrap | null = null;
  let theme: Theme = 'light';
  let themePreference: ThemePreference = 'system';
  const themeWatchers: Array<() => void> = [];
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
  /// The account folder being read, when one is open instead of a fixed view.
  let selectedContainer: ContainerSummary | null = null;
  let sidebar: SidebarLayout = DEFAULT_SIDEBAR_LAYOUT;
  let statsOpen = false;
  let syncOpen = false;
  let searchField: SearchField;
  let savedSearches: SavedSearch[] = [];
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
  /// The thread whose quick reply has been handed its stored draft. Set once
  /// that read settles, cleared when the reader moves to another thread.
  let replyDraftThreadId: number | null = null;
  let notice: Notice | null = null;
  let snoozeDialogOpen = false;
  let customSnoozeValue = '';
  let customSnoozeError = '';
  let commandPaletteOpen = false;
  let commandReturnFocus: HTMLElement | null = null;
  let paletteCommands: PaletteCommand[] = [];
  let goToOpen = false;
  let goToScoped = false;
  let goToReturnFocus: HTMLElement | null = null;
  let moveOpen = false;
  let moveReturnFocus: HTMLElement | null = null;
  let shortcutSheetOpen = false;
  let shortcutReturnFocus: HTMLElement | null = null;
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
  const HIDDEN_ACCOUNTS_KEY = 'mux-hidden-accounts';
  let appearance: Appearance = { ...DEFAULT_APPEARANCE };
  /// True once Enter has stepped into the conversation; j/k then move messages.
  let readerFocused = false;
  /// Accounts hidden from the list. They keep syncing in the background.
  let hiddenAccounts: string[] = [];
  let settingsOpen = false;
  let settingsScreen: SettingsScreen | undefined;
  let blockingDialogOpen = false;

  /// The term that says which mailbox a search covers. It is seeded into the
  /// box so the reader can see the scope — and delete it to widen the search.
  $: scopeTerm = viewScopeTerm(selectedView);
  $: isSearching = searchIsNarrowed(filter, selectedView);
  $: visibleThreads = isSearching ? searchRows : threads;
  $: visibleDrafts = (mailbox?.drafts ?? []).filter((draft) => {
    if (selectedAccount !== null && draft.accountId !== selectedAccount) return false;
    if (hiddenAccounts.includes(draft.accountId)) return false;
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
  const VIEW_TITLES: Record<MailboxView, string> = {
    all: 'All mail',
    inbox: 'Inbox',
    archive: 'Archive',
    starred: 'Starred',
    snoozed: 'Snoozed',
    sent: 'Sent',
    trash: 'Trash',
    drafts: 'Drafts'
  };
  $: viewTitle = selectedContainer?.name || smartViewTitle || VIEW_TITLES[selectedView];
  /// Both headers read from these, so the topbar and the pane heading cannot
  /// describe two different lists. A folder is only ever read inside its own
  /// account, so its account is the scope whatever the account list says. A
  /// smart view is a search the reader chose by name, so it keeps that name;
  /// any other search is titled by the mailbox it still covers, which is the
  /// view it started from only while the seeded scope is in the box.
  $: headerAccountId = selectedContainer?.accountId ?? selectedAccount;
  $: headerScope = headerAccountId === null
    ? { name: 'All accounts', email: 'All accounts' }
    : mailbox?.accounts.find((account) => account.id === headerAccountId) ?? { name: headerAccountId, email: headerAccountId };
  $: headerTitle = isSearching && !selectedSmartView
    ? `Search ${VIEW_TITLES[searchScopeView(filter, selectedView)].toLocaleLowerCase()}`
    : viewTitle;
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
  $: selectedThreadTotal = selectedContainer
    ? selectedContainer.total
    : selectedView === 'drafts' ? visibleDrafts.length : selectedCounts[selectedView];
  $: offersUndo = noticeOffersUndo(notice, clock);
  $: motion = motionTiming(appearance.animation);
  $: railCollapsed = sidebar.collapsed && !compactNavigation;
  /// `mailbox` is named here so recolouring an account moves the accent at
  /// once: a `$:` statement follows the variables it names, and accountFor()
  /// reads the mailbox out of its sight.
  $: applyAccentToRoot(
    appearance.accent,
    theme,
    selectedThread && mailbox ? accountFor(selectedThread.accountId)?.color ?? null : null
  );
  /// Swapping mailbox, account, search, or page replaces every row at once.
  /// That is a new list rather than mail coming and going, so it is keyed: the
  /// row transitions below are local and stay out of it.
  $: threadListKey = `${selectedView}|${selectedAccount ?? ''}|${selectedSmartView}|${selectedContainer?.remoteId ?? ''}|${filter.trim()}|${renderedThreadWindow.start}`;
  $: blockingDialogOpen = commandPaletteOpen || snoozeDialogOpen || activityDialogOpen
    || composerOpen || goToOpen || moveOpen || shortcutSheetOpen;
  $: if (noticeExpired(notice, clock)) notice = null;
  $: {
    selectedThread;
    theme;
    paletteCommands = availablePaletteCommands();
  }
  $: paletteRows = paletteCommands.map((command) => ({
    id: command.id,
    title: command.title,
    subtitle: command.description,
    chip: command.shortcut
  }));
  /// Only ever the selected conversation's own account: the destinations are
  /// built from the thread itself, so there is no other account to reach.
  $: moveRows = selectedThread && moveOpen
    ? moveDestinations(selectedThread, {
        trashed: selectedView === 'trash',
        accountLabel: accountLabelFor(selectedThread.accountId),
        accountColor: accountFor(selectedThread.accountId)?.color
      })
    : [];
  /// ⇧G narrows the jump list to the account being read; G offers every one.
  $: goToRows = mailbox && goToOpen
    ? mailboxJumpRows({
        accounts: mailbox.accounts,
        containers: mailbox.containers,
        saved: savedSearches,
        selectedAccount,
        scoped: goToScoped,
        scopeAccountId: selectedAccount
      })
    : [];

  onMount(() => {
    destroyed = false;
    sidebar = readSidebarLayout();
    savedSearches = readSavedSearches();
    setTheme(readThemePreference(), false);
    const stopWatchingSystemTheme = watchSystemTheme(systemThemeChanged);
    themeWatchers.push(stopWatchingSystemTheme);
    hiddenAccounts = readHiddenAccounts();
    applyAppearance(readSavedAppearance(), false);
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
  });

  onDestroy(() => {
    destroyed = true;
    for (const stop of themeWatchers) stop();
    themeWatchers.length = 0;
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
  });

  function setTheme(next: ThemePreference, persist = true) {
    themePreference = next;
    theme = resolveTheme(next);
    document.documentElement.dataset.theme = theme;
    if (persist) persistThemePreference(next);
  }

  /// Following the system means the answer can change while Mux is open, and
  /// only then: a chosen light or dark stays put.
  function systemThemeChanged(prefersDark: boolean) {
    if (themePreference !== 'system') return;
    theme = prefersDark ? 'dark' : 'light';
    document.documentElement.dataset.theme = theme;
  }


  /// The narrow rail is a wide-window arrangement: below that the sidebar is
  /// already an overlay, and shrinking an overlay to icons helps nobody.
  function toggleRail() {
    sidebar = { ...sidebar, collapsed: !sidebar.collapsed };
    persistSidebarLayout(sidebar);
  }

  function toggleFolderSection(accountId: string) {
    sidebar = toggleFolderAccount(sidebar, accountId);
    persistSidebarLayout(sidebar);
  }

  function toggleSmartViewsSection() {
    sidebar = toggleSmartViews(sidebar);
    persistSidebarLayout(sidebar);
  }

  function toggleSavedSearchesSection() {
    sidebar = toggleSavedSearches(sidebar);
    persistSidebarLayout(sidebar);
  }

  function toggleAccountVisibility(accountId: string) {
    hiddenAccounts = hiddenAccounts.includes(accountId)
      ? hiddenAccounts.filter((id) => id !== accountId)
      : [...hiddenAccounts, accountId];
    try {
      window.localStorage.setItem(HIDDEN_ACCOUNTS_KEY, JSON.stringify(hiddenAccounts));
    } catch {
      // Visibility persistence is optional; the choice still applies this session.
    }
    // A visibility change re-scopes the cursor, so restart the page.
    clearThreadPage();
    resetSearch();
    if (selectedView !== 'drafts') void loadThreads(false, true);
  }

  function readHiddenAccounts(): string[] {
    try {
      const raw = window.localStorage.getItem(HIDDEN_ACCOUNTS_KEY);
      if (!raw) return [];
      const parsed: unknown = JSON.parse(raw);
      if (!Array.isArray(parsed)) return [];
      return parsed.filter((id): id is string => typeof id === 'string' && id.length > 0).slice(0, 64);
    } catch {
      return [];
    }
  }

  function applyAppearance(next: Appearance, persist = true) {
    const speedChanged = persist && next.animation !== appearance.animation;
    appearance = next;
    applyAppearanceToRoot(next);
    if (persist) persistAppearance(next);
    // The setting is about how things move, so the answer to picking one is a
    // thing moving at that speed.
    if (speedChanged) void showAnimationSample();
  }

  /// Clearing first so the toast leaves and arrives again: picking a second
  /// speed has to show that speed, not quietly reuse the toast already up.
  async function showAnimationSample() {
    notice = null;
    await tick();
    notice = sampleNotice('Like this!');
  }

  /// The palette's toggle answers the question it is asked — light or dark —
  /// which means it stops following the system.
  function toggleTheme() {
    setTheme(theme === 'light' ? 'dark' : 'light');
  }

  function availablePaletteCommands(): PaletteCommand[] {
    const items: PaletteCommand[] = [
      { id: 'compose', title: 'Compose message', description: 'Start a new local draft', shortcut: shortcutLabel('compose') },
      ...(selectedThread ? [
        { id: 'archive', title: selectedThread.inInbox ? 'Archive conversation' : 'Move conversation to inbox', description: 'Update the focused conversation locally', shortcut: shortcutLabel('archive') },
        { id: 'move', title: 'Move conversation', description: 'Move it within its own account', shortcut: shortcutLabel('move') },
        { id: 'snooze', title: 'Snooze conversation', description: 'Hide it until a chosen time', shortcut: shortcutLabel('snooze') },
        { id: 'star', title: selectedThread.starred ? 'Remove star' : 'Star conversation', description: 'Update the focused conversation', shortcut: shortcutLabel('toggle-star') },
        { id: 'read', title: selectedThread.unread ? 'Mark conversation read' : 'Mark conversation unread', description: 'Update the focused conversation', shortcut: shortcutLabel('toggle-unread') }
      ] satisfies PaletteCommand[] : []),
      { id: 'inbox', title: 'Go to Inbox', description: 'Open the current unified inbox', shortcut: shortcutLabel('go-to') },
      { id: 'starred', title: 'Go to Starred', description: 'Open starred conversations', shortcut: shortcutLabel('go-to') },
      { id: 'shortcuts', title: 'Keyboard shortcuts', description: 'Every key Mux answers to', shortcut: shortcutLabel('shortcuts') },
      { id: 'activity', title: 'Open activity', description: 'Inspect the local operation journal', shortcut: '' },
      { id: 'theme', title: 'Toggle appearance', description: `Switch to ${theme === 'light' ? 'dark' : 'light'} mode`, shortcut: '' },
      { id: 'settings', title: 'Open settings', description: 'Accounts, appearance, and mail data', shortcut: shortcutLabel('settings') }
    ];
    // The dialog does the filtering; this is the whole menu.
    return items;
  }

  function openCommandPalette() {
    commandReturnFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    commandPaletteOpen = true;
  }

  function closeCommandPalette(restoreFocus = true) {
    commandPaletteOpen = false;
    if (restoreFocus) restoreDialogFocus(commandReturnFocus);
  }

  function runPaletteCommand(id: string) {
    executePaletteCommand(paletteCommands.find((command) => command.id === id));
  }

  function executePaletteCommand(command: PaletteCommand | undefined) {
    if (!command) return;
    closeCommandPalette(false);
    if (command.id === 'compose') openComposer();
    else if (command.id === 'archive') void applyThreadAction(selectedThread?.inInbox ? 'archive' : 'restore', selectedThread?.inInbox ? 'Archived' : 'Restored to inbox');
    else if (command.id === 'move') openMove();
    else if (command.id === 'snooze') openSnoozeDialog();
    else if (command.id === 'star') void applyThreadAction(selectedThread?.starred ? 'unstar' : 'star', selectedThread?.starred ? 'Star removed' : 'Starred');
    else if (command.id === 'read') void applyThreadAction(selectedThread?.unread ? 'read' : 'unread', selectedThread?.unread ? 'Marked read' : 'Marked unread');
    else if (command.id === 'inbox') selectView('inbox');
    else if (command.id === 'starred') selectView('starred');
    else if (command.id === 'shortcuts') openShortcutSheet();
    else if (command.id === 'activity') void openActivity();
    else if (command.id === 'theme') toggleTheme();
    else if (command.id === 'settings') void openSettings();
  }

  function openGoTo(scoped: boolean) {
    goToReturnFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    goToScoped = scoped;
    goToOpen = true;
  }

  function closeGoTo(restoreFocus = true) {
    goToOpen = false;
    if (restoreFocus) restoreDialogFocus(goToReturnFocus);
  }

  /// One row is one destination. Setting the account before the view keeps this
  /// to a single mailbox load rather than one per axis.
  function jumpTo(id: string) {
    const row = goToRows.find((entry) => entry.id === id);
    closeGoTo(false);
    if (!row) return;
    if (row.target.kind === 'account') {
      selectAccount(row.target.accountId);
      return;
    }
    if (row.target.kind === 'container') {
      const target = row.target;
      const folder = mailbox?.containers.find((container) =>
        container.accountId === target.accountId && container.remoteId === target.remoteId
      );
      if (folder) selectContainer(folder);
      return;
    }
    selectedAccount = row.target.accountId;
    if (row.target.kind === 'view') selectView(row.target.view);
    else if (row.target.kind === 'smart') selectSmartView(row.target.smart, row.target.query);
    else selectSavedSearch(row.target);
  }

  function openMove() {
    if (!selectedThread) return;
    moveReturnFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    moveOpen = true;
  }

  function closeMove(restoreFocus = true) {
    moveOpen = false;
    if (restoreFocus) restoreDialogFocus(moveReturnFocus);
  }

  function runMove(id: string) {
    const destination = moveRows.find((row) => row.id === id);
    closeMove(false);
    if (!destination) return;
    void applyThreadAction(destination.action, destination.notice);
  }

  function openShortcutSheet() {
    shortcutReturnFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    shortcutSheetOpen = true;
  }

  function closeShortcutSheet(restoreFocus = true) {
    shortcutSheetOpen = false;
    if (restoreFocus) restoreDialogFocus(shortcutReturnFocus);
  }

  function saveSearch(query: string) {
    if (!query) return;
    savedSearches = addSavedSearch(savedSearches, query);
    persistSavedSearches(savedSearches);
    notice = confirmationNotice(`Saved “${savedSearches[0].name}”`);
  }

  function forgetSearch(query: string) {
    savedSearches = removeSavedSearch(savedSearches, query);
    persistSavedSearches(savedSearches);
  }

  /// A saved search is run by putting its query in the box and handing it to
  /// `filterChanged`, which is what picking the ★ row in the dropdown does
  /// through the box's binding and `oninput`. The sidebar row and the jump
  /// dialog come here, so the three cannot come apart.
  function selectSavedSearch(search: SavedSearch) {
    filter = search.query;
    filterChanged();
  }

  function restoreDialogFocus(target: HTMLElement | null) {
    if (!target) return;
    void tick().then(() => {
      if (target.isConnected) target.focus();
    });
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

  async function openSettings(section: SettingsSection = 'accounts') {
    navigationOpen = false;
    closeCommandPalette(false);
    statsOpen = false;
    syncOpen = false;
    settingsOpen = true;
    await tick();
    settingsScreen?.show(section);
  }

  function closeSettings() {
    settingsOpen = false;
  }

  /// Stats and sync are places rather than dialogs: they take the whole
  /// workspace the way settings does, and only one of the three is ever open.
  function openStats() {
    navigationOpen = false;
    closeCommandPalette(false);
    settingsOpen = false;
    syncOpen = false;
    statsOpen = true;
  }

  function openSync() {
    navigationOpen = false;
    closeCommandPalette(false);
    settingsOpen = false;
    statsOpen = false;
    syncOpen = true;
  }

  function closeStats() {
    statsOpen = false;
  }

  function closeSync() {
    syncOpen = false;
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
        await loadThreads(false, initial, !isSearching, threadWindowSize, messageWindowSize);
        if (request !== bootstrapRequest) return;
        if (isSearching) await runSearch(false, searchWindowSize, messageWindowSize);
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
    replyDraftThreadId = null;
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
    const containerId = selectedContainer?.remoteId ?? null;
    const requiredThreadId = !initial && selectAfterLoad ? selectedThreadId : null;
    const isCurrent = () => request === threadRequest
      && selectedAccount === accountId
      && selectedView === view
      && (selectedContainer?.remoteId ?? null) === containerId;
    const input: ThreadPageInput = {
      accountId,
      view,
      cursor: append ? threadCursor : null,
      limit: 50,
      hiddenAccountIds: hiddenAccounts,
      containerId
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
      // Re-reading the thread already on screen must not blank it. Clearing
      // here swaps in the loading placeholder and rebuilds the conversation
      // underneath it, which reloads every message body the reader is looking
      // at — and a background sync re-reads the open thread constantly.
      if (loadedThreadId !== thread.id) {
        selectedMessages = [];
        selectedAttachments = [];
        loadedThreadId = null;
        replyDraftThreadId = null;
      }
      messageCursor = null;
      messageHasMore = false;
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
      if (!older) {
        loadedThreadId = null;
        replyDraftThreadId = null;
      }
    } finally {
      if (request === messageRequest) messageLoading = false;
    }
  }

  async function loadReplyDraft(threadId: number) {
    const header = mailbox?.drafts.find((draft) => draft.replyToThreadId === threadId && !draft.locked) ?? null;
    const request = ++replyDraftRequest;
    /// A thread being opened waits for its stored reply, so the editor mounts
    /// holding it. A thread already showing its quick reply must not wait: the
    /// editor has the words being typed, and taking it down to show the
    /// store's copy hands back the last save — whose own autosave is what
    /// reloaded the mailbox, so the swap would repeat on every keystroke. For
    /// an open thread the stored copy is read quietly, for the next mount.
    const opening = replyDraftThreadId !== threadId;
    if (opening) {
      replyDraft = null;
      replyDraftError = '';
      replyDraftLoading = Boolean(header);
    }
    if (!header) {
      replyDraft = null;
      replyDraftThreadId = threadId;
      return;
    }
    try {
      const detail = await invoke<DraftSummary>('get_draft', { draftId: header.id });
      if (request !== replyDraftRequest || selectedThreadId !== threadId) return;
      replyDraft = detail;
      replyDraftThreadId = threadId;
    } catch (cause) {
      if (request !== replyDraftRequest || selectedThreadId !== threadId || !opening) return;
      replyDraftError = cause instanceof Error ? cause.message : String(cause);
    } finally {
      if (request === replyDraftRequest) replyDraftLoading = false;
    }
  }

  function selectAccount(accountId: string | null) {
    draftOpenRequest += 1;
    navigationOpen = false;
    mobileReaderOpen = false;
    selectedContainer = null;
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
    selectedContainer = null;
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
    selectedContainer = null;
    selectedSmartView = view;
    selectedView = 'all';
    filter = query;
    resetSearch();
    clearThreadPage();
    void runSearch(false);
  }

  /// A folder is read within its own account, so opening one selects that
  /// account too: it is the only place the folder exists.
  function selectContainer(container: ContainerSummary) {
    draftOpenRequest += 1;
    navigationOpen = false;
    mobileReaderOpen = false;
    selectedContainer = container;
    selectedAccount = container.accountId;
    selectedView = 'all';
    selectedSmartView = '';
    filter = '';
    resetSearch();
    clearThreadPage();
    void loadThreads(false, true);
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
    if (!searchIsNarrowed(filter, selectedView) || selectedView === 'drafts') {
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
    if (!searchIsNarrowed(filter, selectedView) || selectedView === 'drafts') return;
    // The query carries its own scope now, so the search covers the account and
    // the terms in the box do the narrowing. Trash is the one mailbox "all"
    // leaves out, so a query asking for it is run against trash instead.
    const view = searchViewFor(filter);
    const request = ++searchRequest;
    searching = true;
    searchError = '';
    const input: SearchInput = {
      query: filter.trim(),
      accountId: selectedAccount,
      view,
      cursor: append ? searchCursor : null,
      limit: 50,
      timezoneOffsetMinutes: -new Date().getTimezoneOffset(),
      hiddenAccountIds: hiddenAccounts
    };
    const requiredThreadId = selectedThreadId;
    const isCurrent = () => request === searchRequest
      && filter.trim() === input.query
      && selectedAccount === input.accountId
      && searchViewFor(filter) === input.view;
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
    // Choosing another conversation leaves message-level focus behind.
    exitReaderFocus();
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
    customSnoozeValue = '';
    customSnoozeError = '';
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

  /// A local datetime-local value, floored to the minute, must be in the future.
  function snoozeAtCustomTime() {
    customSnoozeError = '';
    const parsed = new Date(customSnoozeValue);
    const wakeAt = parsed.getTime();
    if (!customSnoozeValue || Number.isNaN(wakeAt)) {
      customSnoozeError = 'Pick a date and time.';
      return;
    }
    if (wakeAt <= Date.now()) {
      customSnoozeError = 'Pick a time in the future.';
      return;
    }
    void snoozeUntil(wakeAt);
  }

  async function snoozeUntil(wakeAt: number) {
    const threadId = selectedThread?.id;
    if (!threadId) return;
    closeSnoozeDialog();
    try {
      const operation = await invoke<OperationSummary>('snooze_thread', { threadId, wakeAt });
      notice = undoableNotice('Conversation snoozed', operation.id, undoDeadline());
      await queueMailboxRefresh();
    } catch (cause) {
      notice = failureNotice(cause);
    }
  }

  async function respondToInvitation(response: InvitationSummary['response']) {
    const threadId = selectedThread?.id;
    if (!threadId || !selectedInvitation) return;
    try {
      const operation = await invoke<OperationSummary>('rsvp_thread', { threadId, response });
      notice = undoableNotice('Invitation response updated', operation.id, undoDeadline());
      await queueMailboxRefresh();
    } catch (cause) {
      notice = failureNotice(cause);
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
      notice = confirmationNotice('Draft unlocked without retrying the uncertain send');
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
      notice = failureNotice(cause);
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
    notice = undoableNotice(composerReply ? 'Reply queued' : 'Message queued', event.detail.id, event.detail.notBefore);
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
    notice = undoableNotice('Reply queued', event.detail.id, event.detail.notBefore);
  }

  async function composerDeleted() {
    composerOpen = false;
    restoreDialogFocus(composerReturnFocus);
    notice = confirmationNotice('Draft deleted');
    await queueMailboxRefresh();
    inlineReplyKey += 1;
  }

  async function applyThreadAction(action: string, label: string, target?: ThreadSummary) {
    const thread = target ?? selectedThread;
    if (!thread) return;
    try {
      const operation = await invoke<OperationSummary>('apply_thread_action', {
        threadId: thread.id,
        action
      });
      await queueMailboxRefresh();
      notice = undoableNotice(label, operation.id, undoDeadline());
    } catch (cause) {
      notice = failureNotice(cause);
    }
  }

  async function undoNotice() {
    if (!notice?.operationId) return;
    const operationId = notice.operationId;
    try {
      await invoke('undo_operation', { operationId });
      notice = confirmationNotice('Undone');
      await queueMailboxRefresh();
    } catch (cause) {
      notice = failureNotice(cause);
    }
  }

  function handleKeydown(event: KeyboardEvent) {
    if (event.defaultPrevented) return;
    // Dialogs answer their own keys, including Escape, which they stop from
    // reaching this handler.
    if (commandPaletteOpen || goToOpen || moveOpen || shortcutSheetOpen) return;
    const chord = chordShortcutFor(event);
    if (chord === 'search' && !composerOpen && !settingsOpen) {
      event.preventDefault();
      closeCommandPalette(false);
      searchField?.focus();
      return;
    }
    if (chord === 'settings' && !composerOpen) {
      event.preventDefault();
      void openSettings();
      return;
    }
    if (chord === 'toggle-sidebar' && !composerOpen && !settingsOpen) {
      event.preventDefault();
      toggleRail();
      return;
    }
    if (chord === 'shortcuts' && !composerOpen) {
      event.preventDefault();
      openShortcutSheet();
      return;
    }
    if (chord === 'palette' && !composerOpen && !snoozeDialogOpen && !activityDialogOpen && !settingsOpen) {
      event.preventDefault();
      openCommandPalette();
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
    if (settingsOpen || statsOpen || syncOpen) {
      if (event.key === 'Escape') {
        event.preventDefault();
        closeSettings();
        closeStats();
        closeSync();
      }
      return;
    }
    if (composerOpen) return;
    if (readerFocused && event.key === 'Escape') {
      event.preventDefault();
      exitReaderFocus();
      return;
    }
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
        searchField?.blur();
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
    if (action === 'open-thread') {
      if (readerFocused || !selectedThread || selectedView === 'drafts') return;
      readerFocused = true;
      void threadConversation?.focusFirstMessage();
      return;
    }
    if (action === 'go-to' || action === 'go-to-account') {
      openGoTo(action === 'go-to-account');
      return;
    }
    if (action === 'shortcuts') {
      openShortcutSheet();
      return;
    }
    if (action === 'move') {
      openMove();
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
    if (readerFocused && (action === 'next-thread' || action === 'previous-thread')) {
      void threadConversation?.moveMessageFocus(action === 'next-thread' ? 1 : -1);
      return;
    }
    if (!visibleThreads.length) return;
    const currentIndex = Math.max(0, visibleThreads.findIndex((thread) => thread.id === selectedThread?.id));
    const direction = action === 'next-thread' ? 1 : -1;
    const nextIndex = Math.min(visibleThreads.length - 1, Math.max(0, currentIndex + direction));
    selectThread(visibleThreads[nextIndex]);
  }

  function exitReaderFocus() {
    readerFocused = false;
    threadConversation?.clearMessageFocus();
  }

  type ReaderAction = {
    id: string;
    label: string;
    icon: 'inbox' | 'archive' | 'trash' | 'clock' | 'mail' | 'reply' | 'replyAll' | 'forward' | 'unread';
    shortcut: string;
    run: () => void;
  };

  $: readerActions = ((): ReaderAction[] => {
    const thread = selectedThread;
    if (!thread) return [];
    const actions: ReaderAction[] = [];
    if (selectedView === 'trash') {
      actions.push({
        id: 'untrash',
        label: 'Remove from Trash',
        icon: 'inbox',
        shortcut: '',
        run: () => void applyThreadAction('untrash', 'Removed from Trash')
      });
    } else {
      actions.push({
        id: thread.inInbox ? 'archive' : 'restore',
        label: thread.inInbox ? 'Archive' : 'Move to inbox',
        icon: 'archive',
        shortcut: shortcutLabel('archive'),
        run: () => void applyThreadAction(
          thread.inInbox ? 'archive' : 'restore',
          thread.inInbox ? 'Archived' : 'Restored to inbox'
        )
      });
      actions.push({
        id: 'delete',
        label: 'Trash',
        icon: 'trash',
        shortcut: '',
        run: () => void applyThreadAction('delete', 'Moved to Trash')
      });
    }
    actions.push({ id: 'snooze', label: 'Snooze', icon: 'clock', shortcut: shortcutLabel('snooze'), run: openSnoozeDialog });
    actions.push({
      id: thread.unread ? 'read' : 'unread',
      label: thread.unread ? 'Mark read' : 'Mark unread',
      icon: 'mail',
      shortcut: shortcutLabel('toggle-unread'),
      run: () => void applyThreadAction(
        thread.unread ? 'read' : 'unread',
        thread.unread ? 'Marked read' : 'Marked unread'
      )
    });
    actions.push({ id: 'reply', label: 'Reply', icon: 'reply', shortcut: shortcutLabel('reply'), run: () => openReply('reply') });
    actions.push({ id: 'reply-all', label: 'Reply all', icon: 'replyAll', shortcut: shortcutLabel('reply-all'), run: () => openReply('replyAll') });
    actions.push({ id: 'forward', label: 'Forward', icon: 'forward', shortcut: shortcutLabel('forward'), run: openForward });
    if (thread.unread) {
      actions.push({
        id: 'jump-unread',
        label: 'Unread',
        icon: 'unread',
        shortcut: '',
        run: () => void threadConversation?.jumpToUnread()
      });
    }
    return actions;
  })();

  /// Which row is mid-swipe, and how far along. The gesture itself lives in the
  /// action; only these discrete facts reach the markup.
  let swipeRow: number | null = null;
  let swipeSide: SwipeSide = 'left';
  let swipeArmed = false;

  function swipeStateFor(thread: ThreadSummary) {
    return (state: { swiping: boolean; side: SwipeSide; armed: boolean }) => {
      swipeRow = state.swiping ? thread.id : null;
      swipeSide = state.side;
      swipeArmed = state.armed;
    };
  }

  function swipeIntent(
    thread: ThreadSummary,
    action: SwipeAction
  ): { label: string; icon: 'archive' | 'inbox' | 'trash' | 'clock' | 'star' | 'mail'; tone: string } | null {
    if (action === 'none') return null;
    if (action === 'archive') {
      return thread.inInbox
        ? { label: 'Archive', icon: 'archive', tone: 'archive' }
        : { label: 'Move to inbox', icon: 'inbox', tone: 'archive' };
    }
    if (action === 'delete') return { label: 'Trash', icon: 'trash', tone: 'delete' };
    if (action === 'snooze') return { label: 'Snooze', icon: 'clock', tone: 'snooze' };
    if (action === 'star') {
      return thread.starred
        ? { label: 'Remove star', icon: 'star', tone: 'star' }
        : { label: 'Star', icon: 'star', tone: 'star' };
    }
    return thread.unread
      ? { label: 'Mark read', icon: 'mail', tone: 'unread' }
      : { label: 'Mark unread', icon: 'mail', tone: 'unread' };
  }

  function runSwipeAction(thread: ThreadSummary, action: SwipeAction) {
    if (action === 'none') return;
    if (action === 'snooze') {
      selectThread(thread, false);
      void openSnoozeDialog();
      return;
    }
    if (action === 'star') {
      void applyThreadAction(thread.starred ? 'unstar' : 'star', thread.starred ? 'Star removed' : 'Starred', thread);
      return;
    }
    if (action === 'unread') {
      void applyThreadAction(thread.unread ? 'read' : 'unread', thread.unread ? 'Marked read' : 'Marked unread', thread);
      return;
    }
    if (action === 'archive') {
      void applyThreadAction(thread.inInbox ? 'archive' : 'restore', thread.inInbox ? 'Archived' : 'Restored to inbox', thread);
      return;
    }
    void applyThreadAction('delete', 'Moved to Trash', thread);
  }

  function accountFor(accountId: string): AccountSummary | undefined {
    return mailbox?.accounts.find((account) => account.id === accountId);
  }

  function accountLabelFor(accountId: string): string {
    const account = accountFor(accountId);
    return account ? `${account.name} · ${account.email}` : accountId;
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
  data-density={appearance.density}
  data-collapse-toolbar={appearance.toolbarCollapseNarrow ? 'true' : 'false'}
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
      data-action="toggle-navigation" title="Show or hide the mailbox list"
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
    <div class="workspace" data-testid="topbar-workspace">
      <span>{headerScope.name}</span>
      <small>{headerTitle}</small>
    </div>
    <SearchField
      bind:this={searchField}
      bind:value={filter}
      accounts={mailbox?.accounts ?? []}
      saved={savedSearches}
      seed={scopeTerm}
      placeholder="Search mail or use from:, after:, is:…"
      hint={shortcutLabel('search')}
      oninput={filterChanged}
      onsave={saveSearch}
      onforget={forgetSearch}
    />
    <div class="topbar-actions">
      <button
        class="topbar-icon-button"
        type="button"
        aria-label="Open sync status"
        title="Sync status and queue"
        data-action="open-sync"
        data-testid="sync-button"
        on:click={openSync}
      >
        <Icon name="sync" size={19} />
      </button>
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
    {#if settingsOpen}
      <SettingsScreen
        bind:this={settingsScreen}
        {mailbox}
        {appearance}
        {theme}
        {themePreference}
        {applyAppearance}
        {setTheme}
        refreshMailbox={() => refreshMailbox()}
        close={closeSettings}
      />
    {:else if statsOpen}
      <StatsScreen accounts={mailbox.accounts} close={closeStats} />
    {:else if syncOpen}
      <SyncScreen
        accounts={mailbox.accounts}
        refreshMailbox={() => refreshMailbox()}
        close={closeSync}
      />
    {:else}
      <div
        class="workspace-grid"
        class:rail-collapsed={railCollapsed}
        data-testid="mailbox-workspace"
        inert={blockingDialogOpen}
      >
        <MailboxSidebar
          {mailbox}
          counts={selectedCounts}
          {selectedView}
          {selectedSmartView}
          {selectedAccount}
          {hiddenAccounts}
          {filter}
          collapsed={compactNavigation}
          open={navigationOpen}
          statusError={threadError}
          liveUpdates={mailboxEventsAvailable}
          compose={() => openComposer()}
          {selectedContainer}
          railCollapsed={railCollapsed}
          openFolderAccounts={sidebar.openFolderAccounts}
          smartViewsOpen={sidebar.smartViewsOpen}
          savedSearchesOpen={sidebar.savedSearchesOpen}
          {savedSearches}
          {toggleRail}
          {toggleFolderSection}
          {toggleSmartViewsSection}
          {toggleSavedSearchesSection}
          {openStats}
          {selectView}
          {selectSmartView}
          {selectSavedSearch}
          {forgetSearch}
          {selectAccount}
          {selectContainer}
          {toggleAccountVisibility}
          openSettings={() => openSettings()}
          {openActivity}
        />

        <section class="thread-pane" aria-label={viewTitle}>
          <header class="pane-heading" data-testid="thread-list-header">
            <div><small>{headerScope.email}</small><h1>{headerTitle}</h1></div>
            <span>{searching ? 'Searching…' : `${isSearching && selectedView !== 'drafts' ? visibleThreads.length : selectedThreadTotal} ${selectedView === 'drafts' ? 'drafts' : 'threads'}`}</span>
          </header>

          <div class="thread-list" data-testid="thread-list">
            {#if selectedView === 'drafts'}
              {#if renderedDraftWindow.start > 0}
                <button class="search-more" type="button" title="Load more" on:click={() => moveDraftRenderWindow(-1)}>Show previous loaded drafts</button>
              {/if}
              {#each renderedDraftWindow.rows as draft (draft.id)}
                <button
                  class="thread-row draft-row"
                  data-testid="thread-row"
                  data-draft-id={draft.id}
                  animate:flip={{ duration: motion.base }}
                  in:slideReveal|local={{ duration: motion.base }}
                  out:slideAway|local={{ duration: motion.base }}
                  on:click={() => openSavedDraft(draft)}
                >
                  <span class="thread-accent" style:background={draft.accountColor}></span>
                  <span class="avatar" style:--avatar-color={draft.accountColor}>D</span>
                  <span class="thread-copy">
                    <span class="thread-line"><strong>{draft.recipients || 'No recipient'}</strong><time>{relativeTime(draft.updatedAt)}</time></span>
                    <span class="subject">{draft.subject || 'No subject'}</span>
                    <!-- Shares the snippet class but not the meaning: this line is where a
                         draft says it is mid-send, and losing that to a preview setting would
                         leave a sending draft looking like a saved one. -->
                    <span class="snippet">{draft.locked ? 'Sending…' : 'Saved locally'}</span>
                  </span>
                </button>
              {:else}
                <div class="empty"><strong>No drafts</strong><span>Compose a message and it will autosave here.</span></div>
              {/each}
              {#if renderedDraftWindow.end < visibleDrafts.length}
                <button class="search-more" type="button" title="Load more" on:click={() => moveDraftRenderWindow(1)}>Show next loaded drafts</button>
              {/if}
            {:else}
              {#if renderedThreadWindow.start > 0}
                <button class="search-more" type="button" title="Load more" on:click={() => moveThreadRenderWindow(-1)}>Show previous loaded threads</button>
              {/if}
              {#key threadListKey}
              {#each renderedThreadWindow.rows as thread (thread.id)}
                {@const swiping = swipeRow === thread.id}
                {@const pending = swiping
                  ? swipeIntent(thread, swipeSide === 'left' ? appearance.swipeLeft : appearance.swipeRight)
                  : null}
                <div
                  class="thread-swipe"
                  class:is-swiping={swiping}
                  animate:flip={{ duration: motion.base }}
                  in:slideReveal|local={{ duration: motion.base }}
                  out:slideAway|local={{ duration: motion.base }}
                  use:swipeGesture={{
                    enabled: appearance.swipeLeft !== 'none' || appearance.swipeRight !== 'none',
                    onstate: swipeStateFor(thread),
                    oncommit: (side) => runSwipeAction(
                      thread,
                      side === 'left' ? appearance.swipeLeft : appearance.swipeRight
                    )
                  }}
                >
                  {#if swiping && pending}
                    <div
                      class="thread-swipe-hint"
                      class:is-armed={swipeArmed}
                      data-side={swipeSide}
                      data-tone={pending.tone}
                      aria-hidden="true"
                    >
                      <Icon name={pending.icon} size={17} />
                      <span>{pending.label}</span>
                    </div>
                  {/if}
                <button
                  class="thread-row"
                  class:is-selected={thread.id === selectedThread?.id}
                  class:is-unread={thread.unread}
                  class:is-swiping={swiping}
                  data-testid="thread-row"
                  data-thread-id={thread.id}
                  title={`${thread.subject} — swipe left to ${swipeLabel(appearance.swipeLeft).toLocaleLowerCase()}, right to ${swipeLabel(appearance.swipeRight).toLocaleLowerCase()}`}
                  on:click={() => selectThread(thread)}
                  aria-pressed={thread.id === selectedThread?.id}
                >
                  <span class="thread-accent" style:background={accountFor(thread.accountId)?.color}></span>
                  <span class="avatar" style:--avatar-color={accountFor(thread.accountId)?.color}>{initials(thread.participants)}</span>
                  <span class="thread-copy">
                    <span class="thread-line"><strong>{thread.participants}</strong><time>{relativeTime(thread.latestAt)}</time></span>
                    <span class="subject">{thread.starred ? '★ ' : ''}{thread.subject}</span>
                    {#if appearance.listSnippet}<span class="snippet">{thread.snippet}</span>{/if}
                  </span>
                  {#if thread.unread}<span class="unread-dot" aria-label="Unread"></span>{/if}
                </button>
                </div>
              {:else}
                {#if threadLoading && !isSearching}
                  <div class="empty search-state"><strong>Loading mailbox</strong><span>Reading the next local page.</span></div>
                {:else if searching}
                  <div class="empty search-state"><strong>Searching mail</strong><span>Press Esc to clear.</span></div>
                {:else if searchError}
                  <div class="empty search-state has-error" role="alert"><strong>Search needs attention</strong><span>{searchError}</span></div>
                {:else}
                  <div class="empty"><strong>No {viewTitle.toLocaleLowerCase()} mail</strong><span>{filter ? 'Try a broader search.' : 'Choose another mailbox or account.'}</span></div>
                {/if}
              {/each}
              {/key}
              {#if threadError}
                <div class="empty search-state has-error" role="alert"><strong>Mailbox page could not load</strong><span>{threadError}</span></div>
              {/if}
              {#if renderedThreadWindow.end < visibleThreads.length}
                <button class="search-more" type="button" title="Load more" on:click={() => moveThreadRenderWindow(1)}>Show next loaded threads</button>
              {:else if isSearching && searchHasMore && !searching}
                <button class="search-more" type="button" title="Load more" on:click={() => runSearch(true)}>Load 50 more results</button>
              {:else if !isSearching && threadHasMore && !threadLoading}
                <button class="search-more" type="button" title="Load more" on:click={() => loadThreads(true)}>Load 50 more threads</button>
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
              {#each readerActions as action (action.id)}
                <button
                  data-action={action.id}
                  title={action.shortcut ? `${action.label} (${action.shortcut})` : action.label}
                  aria-label={action.label}
                  on:click={action.run}
                >
                  {#if appearance.toolbarIcons}<Icon name={action.icon} size={16} />{/if}
                  {#if appearance.toolbarText}<span>{action.label}</span>{/if}
                  {#if appearance.toolbarShortcuts && action.shortcut}<kbd>{action.shortcut}</kbd>{/if}
                </button>
              {/each}
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
                <p>{selectedThread.participants} · {selectedThread.messageCount} {selectedThread.messageCount === 1 ? 'message' : 'messages'} · {accountFor(selectedThread.accountId)?.name}</p>
              </div>

              {#if messageLoading && loadedThreadId !== selectedThread.id}
                <div class="empty search-state"><strong>Loading conversation</strong><span>Reading messages from the local store.</span></div>
              {:else if messageError && loadedThreadId !== selectedThread.id}
                <div class="empty search-state has-error" role="alert"><strong>Conversation needs attention</strong><span>{messageError}</span></div>
              {:else}
                {#if messageHasMore && !messageLoading}
                  <button class="search-more" type="button" title="Load more" on:click={() => loadThreadMessages(selectedThread, true)}>Show older</button>
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

                <!-- Keyed on the thread alone. Keying on the message list as well rebuilt
                     the whole conversation every time a refresh replaced it. -->
                {#key selectedThread.id}
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
    {/if}
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
    <div
      class="undo-toast"
      role="status"
      data-testid="undo-toast"
      inert={blockingDialogOpen}
      transition:toastMotion={{ duration: motion.base }}
    >
      <span>{notice.text}</span>
      {#if offersUndo}
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
    <JumpDialog
      title="Commands"
      placeholder="Type a command…"
      rows={paletteRows}
      emptyLabel="No matching command"
      testid="command-palette"
      onselect={runPaletteCommand}
      onclose={closeCommandPalette}
    />
  {/if}

  {#if goToOpen}
    <JumpDialog
      title={goToScoped ? 'Go to folder in this account' : 'Go to'}
      placeholder="Folder, smart view, saved search, or account…"
      rows={goToRows}
      emptyLabel="No matching folder"
      testid="go-to-dialog"
      onselect={jumpTo}
      onclose={closeGoTo}
    />
  {/if}

  {#if moveOpen && selectedThread}
    <JumpDialog
      title="Move to"
      placeholder="Inbox, Archive, Trash…"
      rows={moveRows}
      emptyLabel="Nowhere to move this"
      testid="move-dialog"
      onselect={runMove}
      onclose={closeMove}
    />
  {/if}

  {#if shortcutSheetOpen}
    <ShortcutSheet onclose={closeShortcutSheet} />
  {/if}

  {#if snoozeDialogOpen && selectedThread}
    <div class="modal-backdrop" role="presentation" on:click|self={() => closeSnoozeDialog()}>
      <div bind:this={snoozeDialog} class="modal-card snooze-dialog" role="dialog" aria-modal="true" aria-labelledby="snooze-title" data-testid="snooze-dialog" tabindex="-1" on:keydown={(event) => trapModalKeydown(event, snoozeDialog, closeSnoozeDialog)}>
        <header>
          <div><small>Bring it back</small><h1 id="snooze-title">Snooze conversation</h1></div>
          <button class="icon-button" type="button" aria-label="Close snooze options" on:click={() => closeSnoozeDialog()}>×</button>
        </header>
        <div class="modal-content snooze-options">
          {#each snoozePresets() as preset, index}
            <button type="button" title={`Snooze until ${preset.description}`} data-autofocus={index === 0 ? '' : undefined} on:click={() => snoozeUntil(preset.wakeAt)}>
              <span><strong>{preset.label}</strong><small>{preset.description}</small></span>
              <Icon name="chevron" size={16} />
            </button>
          {/each}
          <form class="snooze-custom" data-testid="custom-snooze" on:submit|preventDefault={snoozeAtCustomTime}>
            <label>
              <span>Pick a time</span>
              <input type="datetime-local" bind:value={customSnoozeValue} data-testid="custom-snooze-input" />
            </label>
            <button class="primary-button" type="submit" title="Snooze until the chosen time">Snooze</button>
          </form>
          {#if customSnoozeError}<p class="snooze-error" role="alert">{customSnoozeError}</p>{/if}
        </div>
      </div>
    </div>
  {/if}

  {#if activityDialogOpen}
    <div class="modal-backdrop" role="presentation" on:click|self={() => closeActivity()}>
      <div bind:this={activityDialog} class="modal-card activity-dialog" role="dialog" aria-modal="true" aria-labelledby="activity-title" data-testid="activity-dialog" tabindex="-1" on:keydown={(event) => trapModalKeydown(event, activityDialog, closeActivity)}>
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

</main>
