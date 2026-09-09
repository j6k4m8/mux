<script lang="ts">
  import Icon from './Icon.svelte';
  import { mailboxViews, smartViews } from './mailboxViews';
  import { folderSectionIsOpen, savedSearchesSectionIsOpen, smartViewsSectionIsOpen } from './sidebarLayout';
  import type { SavedSearch } from './savedSearches';
  import type {
    AccountSummary,
    ContainerSummary,
    MailboxBootstrap,
    MailboxView,
    SmartView,
    ViewCountSummary
  } from './types';

  export let mailbox: MailboxBootstrap;
  export let counts: ViewCountSummary;
  export let selectedView: MailboxView;
  export let selectedSmartView: SmartView;
  export let selectedAccount: string | null;
  export let selectedContainer: ContainerSummary | null = null;
  /// The narrow rail: icons only, no labels, no counts.
  export let railCollapsed = false;
  export let openFolderAccounts: string[] = [];
  export let smartViewsOpen = true;
  export let savedSearchesOpen = true;
  export let savedSearches: SavedSearch[] = [];
  export let hiddenAccounts: string[] = [];
  export let filter = '';
  export let collapsed = false;
  export let open = false;
  export let statusError = '';
  export let liveUpdates = true;

  export let compose: () => void;
  export let selectView: (view: MailboxView) => void;
  export let selectSmartView: (view: Exclude<SmartView, ''>, query: string) => void;
  export let selectSavedSearch: (search: SavedSearch) => void;
  export let forgetSearch: (query: string) => void;
  export let selectAccount: (accountId: string | null) => void;
  export let selectContainer: (container: ContainerSummary) => void;
  export let toggleRail: () => void;
  export let toggleFolderSection: (accountId: string, currentlyShown: boolean) => void;
  export let toggleSmartViewsSection: (currentlyShown: boolean) => void;
  export let toggleSavedSearchesSection: (currentlyShown: boolean) => void;
  export let toggleAccountVisibility: (accountId: string) => void;
  export let openSettings: () => void;
  export let openStats: () => void;
  export let openSync: () => void;

  /// Folders belong to one account each, so they are listed under the account
  /// they belong to — one foldable section per visible account that has any.
  $: folderSections = mailbox.accounts
    .filter((account) => !hiddenAccounts.includes(account.id))
    .map((account) => ({
      account,
      folders: mailbox.containers.filter((container) => container.accountId === account.id)
    }))
    .filter((section) => section.folders.length > 0);
  $: openFolderAccountId = selectedContainer?.accountId ?? null;
  $: smartViewsShown = smartViewsSectionIsOpen(smartViewsOpen, selectedSmartView);
  /// The saved search being run, if the box holds one of them. Queries are
  /// stored trimmed, so the box is compared trimmed.
  $: activeSavedQuery = savedSearches.find((search) => search.query === filter.trim())?.query ?? '';
  $: savedSearchesShown = savedSearchesSectionIsOpen(savedSearchesOpen, activeSavedQuery);

  $: draftCount = mailbox.drafts.filter(
    (draft) => selectedAccount === null || draft.accountId === selectedAccount
  ).length;
  $: totalUnread = mailbox.accounts.reduce((total, account) => total + account.unread, 0);

  /// Snoozed and All mail also read as inactive while a smart view or a search
  /// has taken over the list.
  function isActive(view: MailboxView): boolean {
    if (selectedView !== view) return false;
    if (view === 'snoozed') return !selectedSmartView;
    if (view === 'all') return !selectedSmartView && !filter;
    return true;
  }

  function accountLabel(account: AccountSummary, hidden: boolean): string {
    return hidden ? `Show ${account.name}` : `Hide ${account.name}`;
  }
</script>

<nav
  id="native-navigation"
  class="sidebar"
  class:is-rail={railCollapsed}
  aria-label="Mailbox navigation"
  aria-hidden={collapsed && !open}
  inert={collapsed && !open}
  data-testid="mailbox-navigation"
>
  <button
    class="rail-toggle"
    type="button"
    aria-label={railCollapsed ? 'Widen the sidebar' : 'Narrow the sidebar to icons'}
    aria-expanded={!railCollapsed}
    title={railCollapsed ? 'Widen the sidebar (⌘\\)' : 'Narrow the sidebar (⌘\\)'}
    data-action="toggle-rail"
    data-testid="rail-toggle"
    on:click={() => toggleRail()}
  >{railCollapsed ? '\u203A' : '\u2039'}</button>

  <button class="compose" data-action="compose" title="Compose a message (C)" data-testid="compose-button" on:click={() => compose()}>
    <Icon name="compose" size={18} /><span>Compose</span>
  </button>

  <section class="nav-section">
    {#each mailboxViews as view}
      <button
        class:is-active={isActive(view.id)}
        data-action={`view-${view.id}`}
        title={view.title}
        on:click={() => selectView(view.id)}
      >
        <span class="nav-icon"><Icon name={view.icon} size={17} /></span><strong>{view.label}</strong>
        <em>{view.count(counts, draftCount)}</em>
      </button>
    {/each}
  </section>

  <button
    class="section-label section-toggle"
    type="button"
    aria-expanded={smartViewsShown}
    data-action="toggle-smart-views"
    title={smartViewsShown ? 'Fold away smart views' : 'Show smart views'}
    on:click={() => toggleSmartViewsSection(smartViewsShown)}
  >
    <span>Smart views</span>
    <span class="folder-chevron" aria-hidden="true">{smartViewsShown ? '\u2304' : '\u203A'}</span>
  </button>
  <!-- The rail has no heading to fold from, so its dots stay whatever the fold
       says; they are what the rail has instead of the words. -->
  {#if smartViewsShown || railCollapsed}
    <section class="smart-views">
      {#each smartViews as view}
        <button
          class:is-active={selectedSmartView === view.id}
          data-action={`smart-${view.id}`}
          title={view.title}
          on:click={() => selectSmartView(view.id, view.query)}
        >
          <span class={`smart-dot ${view.dot}`}></span><span>{view.label}</span>
        </button>
      {/each}
    </section>
  {/if}

  {#if savedSearches.length}
    <button
      class="section-label section-toggle"
      type="button"
      aria-expanded={savedSearchesShown}
      data-action="toggle-saved-searches"
      title={savedSearchesShown ? 'Fold away saved searches' : 'Show saved searches'}
      on:click={() => toggleSavedSearchesSection(savedSearchesShown)}
    >
      <span>Saved searches</span>
      <span class="folder-chevron" aria-hidden="true">{savedSearchesShown ? '\u2304' : '\u203A'}</span>
    </button>
    {#if savedSearchesShown || railCollapsed}
      <section class="nav-section saved-searches">
        <!-- The forget control cannot live inside the row's own button, so the
             two sit side by side on one grid row, as an account's dot and name
             do. Rows are keyed by query, the one thing the list is unique on. -->
        {#each savedSearches as search (search.query)}
          <div class="saved-search-row">
            <button
              class:is-active={activeSavedQuery === search.query}
              data-action="select-saved-search"
              data-query={search.query}
              title={search.query}
              on:click={() => selectSavedSearch(search)}
            >
              <span class="nav-icon"><Icon name="search" size={15} /></span><strong>{search.name}</strong>
            </button>
            <button
              class="saved-search-forget"
              type="button"
              aria-label={`Forget “${search.name}”`}
              title="Forget this search"
              data-action="forget-saved-search"
              data-query={search.query}
              on:click={() => forgetSearch(search.query)}
            >✕</button>
          </div>
        {/each}
      </section>
    {/if}
  {/if}

  {#if folderSections.length}
    <p class="section-label">Folders</p>
    {#each folderSections as section (section.account.id)}
      {@const open = folderSectionIsOpen(openFolderAccounts, section.account.id, openFolderAccountId)}
      <button
        class="folder-section"
        class:is-open={open}
        type="button"
        aria-expanded={open}
        aria-label={`${open ? 'Fold away' : 'Show'} ${section.account.name}'s folders`}
        data-action="toggle-folder-section"
        data-account-id={section.account.id}
        title={`${open ? 'Fold away' : 'Show'} ${section.account.name}'s folders`}
        on:click={() => toggleFolderSection(section.account.id, open)}
      >
        <span class="account-dot" style:background={section.account.color}></span>
        <strong>{section.account.name}</strong>
        <em>{section.folders.length}</em>
        <span class="folder-chevron" aria-hidden="true">{open ? '\u2304' : '\u203A'}</span>
      </button>
      {#if open}
        <section class="nav-section folders">
          {#each section.folders as folder (folder.remoteId)}
            <button
              class:is-active={selectedContainer?.remoteId === folder.remoteId
                && selectedContainer?.accountId === folder.accountId}
              data-action="select-folder"
              data-folder-id={folder.remoteId}
              title={`${folder.name} — ${section.account.name}`}
              on:click={() => selectContainer(folder)}
            >
              <span class="nav-icon"><Icon name={folder.kind === 'label' ? 'star' : 'archive'} size={15} /></span>
              <strong>{folder.name}</strong>
              <em>{folder.unread || ''}</em>
            </button>
          {/each}
        </section>
      {/if}
    {/each}
  {/if}

  <p class="section-label">Accounts</p>
  <section class="accounts">
    <!-- All accounts is one more account row, so its dot and name sit in the
         same columns as the accounts under it. Nothing is shown or hidden for
         all of them at once, so its dot cell is only the dot. -->
    <div class="account-row all">
      <span class="account-mark" aria-hidden="true"><span class="account-dot all"></span></span>
      <button
        class="account-select"
        class:is-active={selectedAccount === null}
        type="button"
        data-action="account-all"
        title="Show every account together"
        on:click={() => selectAccount(null)}
      >
        <span>All accounts</span>
        <em>{totalUnread}</em>
      </button>
    </div>
    {#each mailbox.accounts as account}
      {@const hidden = hiddenAccounts.includes(account.id)}
      <div class="account-row" class:is-hidden={hidden}>
        <button
          class="account-visibility"
          type="button"
          data-action="toggle-account-visibility"
          data-account-id={account.id}
          aria-pressed={!hidden}
          title={accountLabel(account, hidden)}
          aria-label={accountLabel(account, hidden)}
          on:click={() => toggleAccountVisibility(account.id)}
        >
          <span class="account-dot" style:background={account.color}></span>
        </button>
        <button
          class="account-select"
          class:is-active={selectedAccount === account.id}
          type="button"
          data-action="select-account" title={`Show only ${account.name}`}
          data-account-id={account.id}
          on:click={() => selectAccount(account.id)}
        >
          <span>{account.name}</span>
          <em>{account.unread}</em>
        </button>
      </div>
    {/each}
  </section>

  <button class="settings-button" type="button" data-action="open-stats" data-testid="stats-button" title="Mail statistics" on:click={() => openStats()}>
    <span class="nav-icon"><Icon name="stats" size={16} /></span><strong>Stats</strong>
  </button>

  <button class="settings-button" type="button" data-action="open-settings" title="Settings (⌘,)" on:click={() => openSettings()}>
    <span class="nav-icon"><Icon name="settings" size={16} /></span><strong>Settings</strong>
    <kbd>⌘,</kbd>
  </button>

  <button class="projection-status" type="button" data-action="open-sync" title="Open sync status and queue" on:click={openSync}>
    <span class:has-error={Boolean(statusError) || !liveUpdates}></span>
    <div>
      <strong>{statusError ? 'Mailbox needs attention' : liveUpdates ? 'Up to date' : 'Refreshes on focus'}</strong>
      <small>{statusError || (liveUpdates ? 'Changes save on this Mac' : 'Live updates are unavailable')}</small>
    </div>
  </button>
</nav>
