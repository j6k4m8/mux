<script lang="ts">
  import Icon from './Icon.svelte';
  import type {
    AccountSummary,
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
  export let hiddenAccounts: string[] = [];
  export let filter = '';
  export let collapsed = false;
  export let open = false;
  export let statusError = '';
  export let liveUpdates = true;

  export let compose: () => void;
  export let selectView: (view: MailboxView) => void;
  export let selectSmartView: (view: Exclude<SmartView, ''>, query: string) => void;
  export let selectAccount: (accountId: string | null) => void;
  export let toggleAccountVisibility: (accountId: string) => void;
  export let openSettings: () => void;
  export let openActivity: () => void;

  /// Eight near-identical buttons were eight near-identical blocks of markup.
  /// The only things that differ are the icon, the label, and where the count
  /// comes from, so that is all this says.
  const views: Array<{
    id: MailboxView;
    icon: 'inbox' | 'star' | 'clock' | 'sent' | 'drafts' | 'archive' | 'trash' | 'allMail';
    label: string;
    title: string;
    count: (counts: ViewCountSummary) => number;
  }> = [
    { id: 'inbox', icon: 'inbox', label: 'Inbox', title: 'Inbox', count: (c) => c.inbox },
    { id: 'starred', icon: 'star', label: 'Starred', title: 'Starred conversations', count: (c) => c.starred },
    { id: 'snoozed', icon: 'clock', label: 'Snoozed', title: 'Snoozed conversations', count: (c) => c.snoozed },
    { id: 'sent', icon: 'sent', label: 'Sent', title: 'Sent mail', count: (c) => c.sent },
    { id: 'drafts', icon: 'drafts', label: 'Drafts', title: 'Local drafts', count: () => draftCount },
    { id: 'archive', icon: 'archive', label: 'Archive', title: 'Archived conversations', count: (c) => c.archive },
    { id: 'trash', icon: 'trash', label: 'Trash', title: 'Trash', count: (c) => c.trash },
    { id: 'all', icon: 'allMail', label: 'All mail', title: 'All mail', count: (c) => c.all }
  ];

  const smartViews: Array<{ id: Exclude<SmartView, ''>; label: string; dot: string; query: string; title: string }> = [
    { id: 'unread', label: 'Unread', dot: 'blue', query: 'is:unread', title: 'Unread conversations' },
    { id: 'attachments', label: 'Attachments', dot: 'violet', query: 'has:attachment', title: 'Conversations with attachments' },
    { id: 'invitations', label: 'Invitations', dot: 'green', query: 'has:invite', title: 'Conversations with invitations' },
    { id: 'finance', label: 'Finance', dot: 'amber', query: 'category:Finance', title: 'Finance conversations' }
  ];

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
  aria-label="Mailbox navigation"
  aria-hidden={collapsed && !open}
  inert={collapsed && !open}
  data-testid="mailbox-navigation"
>
  <button class="compose" data-action="compose" title="Compose a message (C)" data-testid="compose-button" on:click={() => compose()}>
    <Icon name="compose" size={18} /> Compose
  </button>

  <section class="nav-section">
    {#each views as view}
      <button
        class:is-active={isActive(view.id)}
        data-action={`view-${view.id}`}
        title={view.title}
        on:click={() => selectView(view.id)}
      >
        <span class="nav-icon"><Icon name={view.icon} size={17} /></span><strong>{view.label}</strong>
        <em>{view.count(counts)}</em>
      </button>
    {/each}
  </section>

  <p class="section-label">Smart views</p>
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

  <p class="section-label">Accounts</p>
  <section class="accounts">
    <button class:is-active={selectedAccount === null} data-action="account-all" title="Show every account together" on:click={() => selectAccount(null)}>
      <span class="account-dot all"></span><span>All accounts</span>
      <em>{totalUnread}</em>
    </button>
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

  <button class="settings-button" type="button" data-action="open-settings" title="Settings (⌘,)" on:click={() => openSettings()}>
    <span class="nav-icon"><Icon name="settings" size={16} /></span><strong>Settings</strong>
    <kbd>⌘,</kbd>
  </button>

  <button class="projection-status" type="button" data-action="open-activity" title="Open the local operation journal" on:click={openActivity} aria-haspopup="dialog">
    <span class:has-error={Boolean(statusError) || !liveUpdates}></span>
    <div>
      <strong>{statusError ? 'Mailbox needs attention' : liveUpdates ? 'Up to date' : 'Refreshes on focus'}</strong>
      <small>{statusError || (liveUpdates ? 'Changes save on this Mac' : 'Live updates are unavailable')}</small>
    </div>
  </button>
</nav>
