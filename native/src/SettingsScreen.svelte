<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { onDestroy } from 'svelte';
  import Icon from './Icon.svelte';
  import { animationChoices } from './motion';
  import {
    densityChoices,
    DEFAULT_FONT,
    fontChoices,
    refreshChoices,
    swipeChoices,
    textSizes
  } from './appearance';
  import type { Appearance } from './appearance';
  import type { MailboxBootstrap, SettingsSection, Theme } from './types';

  type GmailOAuthResult = { state: 'connected'; accountId: string; email: string };

  export let mailbox: MailboxBootstrap;
  export let appearance: Appearance;
  export let theme: Theme;
  /// Applying appearance and theme stays with the application: both reach the
  /// document root, which this screen does not own.
  export let applyAppearance: (next: Appearance, persist?: boolean) => void;
  export let setTheme: (next: Theme, persist?: boolean) => void;
  /// Several of these settings change what the mailbox should show.
  export let refreshMailbox: () => Promise<void>;
  export let close: () => void;

  const settingsSections: Array<{ id: SettingsSection; title: string }> = [
    { id: 'accounts', title: 'Accounts' },
    { id: 'appearance', title: 'Appearance' },
    { id: 'mail', title: 'Mail' },
    { id: 'shortcuts', title: 'Shortcuts' }
  ];

  const shortcutReference: Array<{ keys: string; action: string }> = [
    { keys: '\u2318P', action: 'Open the command palette' },
    { keys: '\u2318,', action: 'Open settings' },
    { keys: 'C', action: 'Compose' },
    { keys: 'R / A / F', action: 'Reply, reply all, forward' },
    { keys: 'J / K', action: 'Next or previous conversation' },
    { keys: 'E', action: 'Archive or restore' },
    { keys: 'S', action: 'Star' },
    { keys: 'U', action: 'Toggle unread' },
    { keys: 'H', action: 'Snooze' },
    { keys: '\u2318F', action: 'Search' },
    { keys: 'Esc', action: 'Close or step back' }
  ];

  let settingsSection: SettingsSection = 'accounts';
  let settingsBusy = false;
  let settingsError = '';
  let settingsMessage = '';
  let resyncBusy = false;
  let gmailOAuthBusy = false;
  let syncingAccounts: string[] = [];
  let customFont = fontChoices.some((choice) => choice.value === appearance.font)
    ? ''
    : appearance.font;

  export function show(section: SettingsSection = 'accounts') {
    settingsSection = section;
    clearSettingsFeedback();
  }

  function clearSettingsFeedback() {
    settingsError = '';
    settingsMessage = '';
  }

  function settingsErrorText(cause: unknown): string {
    const text = cause instanceof Error
      ? cause.message
      : typeof cause === 'object' && cause !== null && 'message' in cause && typeof cause.message === 'string'
        ? cause.message
        : String(cause);
    return text || 'Mux could not update that setting.';
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

  async function syncAccountNow(accountId: string, name: string) {
    settingsError = '';
    settingsMessage = '';
    syncingAccounts = [...syncingAccounts, accountId];
    try {
      const scheduled = await invoke<number>('sync_account_now', { input: { accountId } });
      settingsMessage = scheduled > 0
        ? `Checking ${name} for new mail.`
        : `${name} is already syncing.`;
    } catch (cause) {
      settingsError = settingsErrorText(cause);
    } finally {
      syncingAccounts = syncingAccounts.filter((id) => id !== accountId);
      await refreshMailbox();
    }
  }

  async function setAccountRefresh(accountId: string, refreshSeconds: number) {
    settingsError = '';
    settingsMessage = '';
    try {
      await invoke('set_account_refresh', { input: { accountId, refreshSeconds } });
      await refreshMailbox();
      const label = refreshChoices.find((choice) => choice.value === refreshSeconds)?.label
        ?? `${refreshSeconds}s`;
      settingsMessage = `Checking for new mail every ${label}.`;
    } catch (cause) {
      settingsError = settingsErrorText(cause);
    }
  }

  async function connectGmail() {
    settingsError = '';
    settingsMessage = '';
    gmailOAuthBusy = true;
    try {
      const result = readGmailOAuthResult(await invoke<unknown>('gmail_oauth_begin'));
      settingsMessage = `Connected ${result.email}.`;
    } catch (cause) {
      settingsError = settingsErrorText(cause);
    } finally {
      gmailOAuthBusy = false;
    }
  }

  async function cancelGmailOAuth() {
    try {
      await invoke<unknown>('gmail_oauth_cancel');
      settingsMessage = 'Cancelling\u2026';
    } catch (cause) {
      settingsError = settingsErrorText(cause);
    }
  }

  async function resyncAllMail() {
    settingsError = '';
    settingsMessage = '';
    resyncBusy = true;
    try {
      const requested = await invoke<{ accountsReset: number }>('resync_all_mail');
      settingsMessage = requested.accountsReset > 0
        ? 'Re-downloading every message from your provider. Nothing was removed.'
        : 'No provider account is connected yet, so there is nothing to re-download.';
      await refreshMailbox();
    } catch (cause) {
      settingsError = settingsErrorText(cause);
    } finally {
      resyncBusy = false;
    }
  }

  function closeSettings() {
    close();
  }

  // Leaving with an authorization half-open would strand it.
  onDestroy(() => {
    if (gmailOAuthBusy) void cancelGmailOAuth();
  });
</script>

  <section class="settings-screen" data-testid="settings-screen" data-section={settingsSection}>
    <nav class="settings-nav" aria-label="Settings sections">
      <button class="settings-back" type="button" title="Back to mail (Esc)" data-testid="settings-close" on:click={closeSettings}>
        <span aria-hidden="true"><Icon name="chevron" size={15} /></span>Back to mail
      </button>
      <h1>Settings</h1>
      {#each settingsSections as section}
        <button
          class:is-active={settingsSection === section.id}
          type="button"
          data-section={section.id}
          aria-current={settingsSection === section.id ? 'page' : undefined}
          on:click={() => { settingsSection = section.id; clearSettingsFeedback(); }}
        >{section.title}</button>
      {/each}
    </nav>

    <div class="settings-body">
      {#if settingsSection === 'accounts'}
        <header class="settings-heading">
          <h2>Accounts</h2>
          <p>Mux keeps each account's sign-in in your Mac's keychain, so it is ready whenever you are.</p>
        </header>

        {#if mailbox.accounts.length}
          <ul class="settings-account-list">
            {#each mailbox.accounts as account (account.id)}
              <li>
                <div class="settings-account-head">
                  <span class="settings-account-dot" style:--avatar-color={account.color}></span>
                  <span><strong>{account.name}</strong><small>{account.email}</small></span>
                  <em>{account.total} messages</em>
                </div>
                <div class="settings-account-refresh">
                  <span>Check for new mail</span>
                  <button
                    class="settings-sync-now"
                    type="button"
                    data-action="sync-now"
                    data-account-id={account.id}
                    title={`Check ${account.name} for new mail right now`}
                    disabled={syncingAccounts.includes(account.id)}
                    on:click={() => syncAccountNow(account.id, account.name)}
                  >{syncingAccounts.includes(account.id) ? 'Checking…' : 'Sync now'}</button>
                  <div class="settings-choice-row" role="group" aria-label={`Refresh interval for ${account.name}`} data-testid="refresh-interval" data-account-id={account.id}>
                    {#each refreshChoices as choice}
                      <button
                        class:is-active={account.refreshSeconds === choice.value}
                        type="button"
                        on:click={() => setAccountRefresh(account.id, choice.value)}
                      >{choice.label}</button>
                    {/each}
                  </div>
                </div>
              </li>
            {/each}
          </ul>
        {:else}
          <p class="settings-empty">No accounts yet.</p>
        {/if}

        <section class="settings-card">
          <h3>Add another account</h3>
          <p class="settings-hint">
            {mailbox.accounts.length === 1
              ? 'Your account above is already connected and syncing. This adds a second mailbox.'
              : 'Connect an additional mailbox alongside the ones above.'}
          </p>
          <div class="provider-card" aria-labelledby="gmail-connect-title">
            <div>
              <h3 id="gmail-connect-title">Gmail</h3>
              <p>Sign in through your browser. Mux never sees your Google password.</p>
            </div>
            {#if gmailOAuthBusy}
              <button type="button" on:click={cancelGmailOAuth}>Cancel</button>
            {:else}
              <button class="primary-button" type="button" title="Authorize Gmail in your browser" on:click={connectGmail}>Add Gmail account</button>
            {/if}
          </div>
        </section>
      {:else if settingsSection === 'appearance'}
        <header class="settings-heading">
          <h2>Appearance</h2>
          <p>Mux follows this choice on every launch.</p>
        </header>
        <section class="settings-card">
          <h3>Theme</h3>
          <div class="settings-choice-row" role="group" aria-label="Theme">
          <button class:is-active={theme === 'light'} type="button" data-action="theme-light" title="Use the light appearance" on:click={() => setTheme('light')}>
            <span aria-hidden="true"><Icon name="sun" size={16} /></span>Light
          </button>
            <button class:is-active={theme === 'dark'} type="button" data-action="theme-dark" title="Use the dark appearance" on:click={() => setTheme('dark')}>
              <span aria-hidden="true"><Icon name="moon" size={16} /></span>Dark
            </button>
          </div>
        </section>

        <section class="settings-card">
          <h3>Text and typeface</h3>
          <p class="settings-hint">Scales the whole interface, not only message text.</p>
          <div class="settings-choice-row" role="group" aria-label="Text size" data-testid="text-size">
          {#each textSizes as size}
            <button
              class:is-active={appearance.scale === size.value}
              type="button"
              on:click={() => applyAppearance({ ...appearance, scale: size.value })}
            >{size.label}</button>
          {/each}
        </div>

          <div class="settings-choice-row settings-choice-spaced" role="group" aria-label="Typeface" data-testid="typeface">
          {#each fontChoices as choice}
            <button
              class:is-active={appearance.font === choice.value}
              type="button"
              style:font-family={choice.value}
              on:click={() => { customFont = ''; applyAppearance({ ...appearance, font: choice.value }); }}
            >{choice.label}</button>
          {/each}
        </div>
        <label class="settings-inline-field">
          <span>Or name a font installed on this Mac</span>
          <input
            bind:value={customFont}
            data-testid="custom-font"
            placeholder="Iosevka"
            spellcheck="false"
            on:change={() => applyAppearance({ ...appearance, font: customFont.trim() || DEFAULT_FONT })}
          />
        </label>

        </section>

        <section class="settings-card">
          <h3>Thread actions</h3>
          <p class="settings-hint">What the buttons above a conversation show.</p>
          <div class="settings-toggle-row" data-testid="toolbar-display">
          <label><input type="checkbox" bind:checked={appearance.toolbarIcons} on:change={() => applyAppearance(appearance)} /><span>Icon</span></label>
          <label><input type="checkbox" bind:checked={appearance.toolbarText} on:change={() => applyAppearance(appearance)} /><span>Text</span></label>
          <label><input type="checkbox" bind:checked={appearance.toolbarShortcuts} on:change={() => applyAppearance(appearance)} /><span>Key shortcut</span></label>
          <label><input type="checkbox" bind:checked={appearance.toolbarCollapseNarrow} on:change={() => applyAppearance(appearance)} /><span>Collapse to icons on narrow screens</span></label>
          <label><input type="checkbox" bind:checked={appearance.railPreview} on:change={() => applyAppearance(appearance)} /><span>Preview a message when hovering its dot</span></label>
        </div>

        </section>

        <section class="settings-card">
          <h3>Swipe actions</h3>
          <p class="settings-hint">Drag a conversation sideways in the list.</p>
        <label class="settings-inline-field">
          <span>Swipe left</span>
          <select bind:value={appearance.swipeLeft} data-testid="swipe-left" on:change={() => applyAppearance(appearance)}>
            {#each swipeChoices as choice}<option value={choice.value}>{choice.label}</option>{/each}
          </select>
        </label>
        <label class="settings-inline-field">
          <span>Swipe right</span>
          <select bind:value={appearance.swipeRight} data-testid="swipe-right" on:change={() => applyAppearance(appearance)}>
            {#each swipeChoices as choice}<option value={choice.value}>{choice.label}</option>{/each}
          </select>
        </label>

        </section>

        <section class="settings-card">
          <h3>Animation</h3>
          <p>How fast the list, the dialogs, and the toasts move. Your Mac's reduce-motion setting turns them off whatever this says.</p>
          <div class="settings-choice-row" role="group" aria-label="Animation speed" data-testid="animation-speed">
          {#each animationChoices as choice}
            <button
              class:is-active={appearance.animation === choice.value}
              type="button"
              data-animation-option={choice.value}
              on:click={() => applyAppearance({ ...appearance, animation: choice.value })}
            >{choice.label}</button>
          {/each}
          </div>
        </section>

        <section class="settings-card">
          <h3>Density</h3>
          <div class="settings-choice-row" role="group" aria-label="Density" data-testid="density">
          {#each densityChoices as choice}
            <button
              class:is-active={appearance.density === choice.value}
              type="button"
              data-density-option={choice.value}
              on:click={() => applyAppearance({ ...appearance, density: choice.value })}
            >{choice.label}</button>
          {/each}
          </div>
        </section>
      {:else if settingsSection === 'mail'}
        <header class="settings-heading">
          <h2>Mail</h2>
          <p>Mux keeps a copy of your mail on this Mac so it opens instantly.</p>
        </header>
        <section class="settings-card">
          <h3>Re-download all mail</h3>
          <p>Fetches every message from your provider again and refreshes the local copy in place. Nothing is removed, here or on the server. Useful if a message looks wrong or incomplete.</p>
          <div class="settings-actions">
            <button class="primary-button" type="button" data-action="resync-all" title="Re-download every message from your provider" data-testid="resync-all" disabled={settingsBusy || resyncBusy || gmailOAuthBusy} on:click={resyncAllMail}>{resyncBusy ? 'Re-downloading…' : 'Re-download all mail'}</button>
          </div>
        </section>
      {:else}
        <header class="settings-heading">
          <h2>Shortcuts</h2>
          <p>These work whenever the message list has focus.</p>
        </header>
        <section class="settings-card">
          <ul class="settings-shortcut-list">
          {#each shortcutReference as row}
            <li><kbd>{row.keys}</kbd><span>{row.action}</span></li>
            {/each}
          </ul>
        </section>
      {/if}

      {#if settingsError}<p class="settings-feedback has-error" role="alert">{settingsError}</p>{/if}
      {#if settingsMessage}<p class="settings-feedback" role="status">{settingsMessage}</p>{/if}
    </div>
  </section>
