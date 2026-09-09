<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { onDestroy, tick } from 'svelte';
  import Icon from './Icon.svelte';
  import ThreadRowSample from './ThreadRowSample.svelte';
  import { animationChoices } from './motion';
  import { themeChoices } from './theme';
  import type { ThemePreference } from './theme';
  import { shortcutCatalog, shortcutChips, visibleBindings } from './shortcuts';
  import {
    accentChoices,
    accentSwatch,
    densityChoices,
    DEFAULT_FONT,
    fontChoices,
    refreshChoices,
    swipeChoices,
    textSizes
  } from './appearance';
  import type { Appearance } from './appearance';
  import { accountColorChoices, describeAccountColor, normalizeAccountColor } from './accountColor';
  import type { MailboxBootstrap, SettingsSection, Theme } from './types';

  type GmailOAuthResult = { state: 'connected'; accountId: string; email: string };
  type ImapAccountAdded = { accountId: string; email: string; folders: number };
  type RemovedAccount = { accountId: string; keychainCleanupPending: boolean };
  type AccountWizardStep = 'email' | 'mailbox-type' | 'gmail' | 'imap-incoming' | 'imap-outgoing' | 'imap-review';
  type ImapForm = {
    displayName: string;
    email: string;
    host: string;
    port: number;
    username: string;
    smtpHost: string;
    smtpPort: number;
    smtpTlsMode: 'starttls' | 'implicit';
    smtpUsername: string;
  };

  /// Mux proves these details against the server before saving anything, so
  /// this form is where a typo gets caught rather than the sync queue.
  const emptyImapForm = (): ImapForm => ({
    displayName: '', email: '', host: '', port: 993, username: '',
    smtpHost: '', smtpPort: 587, smtpTlsMode: 'starttls', smtpUsername: ''
  });
  let accountWizardStep: AccountWizardStep = 'email';
  let accountEmail = '';
  let accountEmailInput: HTMLInputElement | null = null;
  let wizardStepHeading: HTMLElement | null = null;
  let imapForm = emptyImapForm();
  let imapBusy = false;
  $: wizardEmailReady = validMailboxEmail(accountEmail);
  $: imapIncomingReady = Boolean(
    imapForm.host.trim() && imapForm.username.trim()
      && validPort(imapForm.port)
  );
  $: imapOutgoingReady = Boolean(
    imapForm.smtpHost.trim() && imapForm.smtpUsername.trim()
      && validPort(imapForm.smtpPort)
  );

  export let mailbox: MailboxBootstrap;
  export let appearance: Appearance;
  export let theme: Theme;
  export let themePreference: ThemePreference;
  /// Applying appearance and theme stays with the application: both reach the
  /// document root, which this screen does not own.
  export let applyAppearance: (next: Appearance, persist?: boolean) => void;
  export let setTheme: (next: ThemePreference, persist?: boolean) => void;
  /// Several of these settings change what the mailbox should show.
  export let refreshMailbox: () => Promise<void>;
  export let close: () => void;

  const settingsSections: Array<{ id: SettingsSection; title: string }> = [
    { id: 'accounts', title: 'Accounts' },
    { id: 'appearance', title: 'Appearance' },
    { id: 'mail', title: 'Mail' },
    { id: 'shortcuts', title: 'Shortcuts' }
  ];

  /// Read from the shortcut catalog rather than typed out again: a list of
  /// keys kept by hand is a list of keys that goes stale.
  const shortcutReference = shortcutCatalog.map((definition) => ({
    keys: visibleBindings(definition).map((binding) => shortcutChips(binding).join(' ')).join('  or  '),
    action: definition.title
  }));

  let settingsSection: SettingsSection = 'accounts';
  let settingsBusy = false;
  let settingsError = '';
  let settingsMessage = '';
  let resyncBusy = false;
  let gmailOAuthBusy = false;
  let syncingAccounts: string[] = [];
  let settingAccountColors: string[] = [];
  let removingAccountId = '';
  let removeConfirmationId = '';
  let removalCancelButton: HTMLButtonElement | null = null;
  let removalTriggerButton: HTMLButtonElement | null = null;
  let customFont = fontChoices.some((choice) => choice.value === appearance.font)
    ? ''
    : appearance.font;

  export function show(section: SettingsSection = 'accounts', signInEmail = '') {
    settingsSection = section;
    clearSettingsFeedback();
    if (section === 'accounts' && validMailboxEmail(signInEmail)) {
      accountWizardStep = 'email';
      accountEmail = signInEmail;
      imapForm = emptyImapForm();
      // Let the reactive validity flag observe the prefill before advancing.
      void tick().then(startAccountWizard);
    }
  }

  function clearSettingsFeedback() {
    settingsError = '';
    settingsMessage = '';
  }

  function validMailboxEmail(value: string): boolean {
    const email = value.trim();
    const at = email.indexOf('@');
    return email.length > 2 && email.length <= 320 && at > 0 && at === email.lastIndexOf('@')
      && at < email.length - 1 && !/\s|[\u0000-\u001f\u007f]/u.test(email);
  }

  function validPort(value: number): boolean {
    return Number.isInteger(value) && value > 0 && value <= 65535;
  }

  function normalizedAccountEmail(): string {
    const email = accountEmail.trim();
    const at = email.lastIndexOf('@');
    return `${email.slice(0, at)}@${email.slice(at + 1).toLowerCase()}`;
  }

  async function moveAccountWizard(step: AccountWizardStep) {
    accountWizardStep = step;
    clearSettingsFeedback();
    await tick();
    if (step === 'email') accountEmailInput?.focus();
    else wizardStepHeading?.focus();
  }

  async function resetAccountWizard() {
    accountWizardStep = 'email';
    accountEmail = '';
    imapForm = emptyImapForm();
    await tick();
    accountEmailInput?.focus();
  }

  async function startAccountWizard() {
    if (!wizardEmailReady || gmailOAuthBusy || imapBusy || removingAccountId) return;
    removeConfirmationId = '';
    accountEmail = normalizedAccountEmail();
    if (accountEmail.endsWith('@gmail.com')) {
      await moveAccountWizard('gmail');
      void connectGmail();
    } else {
      await moveAccountWizard('mailbox-type');
    }
  }

  async function chooseImapMailbox() {
    if (removingAccountId) return;
    const email = normalizedAccountEmail();
    imapForm = { ...emptyImapForm(), email, username: email, smtpUsername: email };
    await moveAccountWizard('imap-incoming');
  }

  async function chooseGoogleMailbox() {
    if (removingAccountId) return;
    await moveAccountWizard('gmail');
    void connectGmail();
  }

  async function advanceImapWizard() {
    if (removingAccountId) return;
    if (accountWizardStep === 'imap-incoming' && imapIncomingReady) {
      await moveAccountWizard('imap-outgoing');
    } else if (accountWizardStep === 'imap-outgoing' && imapOutgoingReady) {
      await moveAccountWizard('imap-review');
    } else if (accountWizardStep === 'imap-review') {
      await addImapAccount();
    }
  }

  async function backFromImapWizard() {
    const step = accountWizardStep === 'imap-incoming'
      ? 'mailbox-type'
      : accountWizardStep === 'imap-outgoing' ? 'imap-incoming' : 'imap-outgoing';
    await moveAccountWizard(step);
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

  function readImapAccountAdded(value: unknown): ImapAccountAdded | null {
    if (value === null) return null;
    if (
      typeof value === 'object' && value !== null &&
      'accountId' in value && typeof value.accountId === 'string' && value.accountId.length > 0 &&
      'email' in value && typeof value.email === 'string' && value.email.length > 0 &&
      'folders' in value && typeof value.folders === 'number' && Number.isFinite(value.folders)
    ) {
      return { accountId: value.accountId, email: value.email, folders: value.folders };
    }
    throw new Error('Mux returned an invalid mailbox result.');
  }

  function readRemovedAccount(value: unknown): RemovedAccount {
    if (
      typeof value === 'object' && value !== null &&
      'accountId' in value && typeof value.accountId === 'string' && value.accountId.length > 0 &&
      'keychainCleanupPending' in value && typeof value.keychainCleanupPending === 'boolean'
    ) {
      return { accountId: value.accountId, keychainCleanupPending: value.keychainCleanupPending };
    }
    throw new Error('Mux returned an invalid account-removal result.');
  }

  async function addImapAccount() {
    // The submit button is disabled without these, but a form also submits on
    // Enter, and a half-filled credential is not worth sending anywhere.
    if (imapBusy || removingAccountId || accountWizardStep !== 'imap-review' || !imapIncomingReady || !imapOutgoingReady) return;
    settingsError = '';
    settingsMessage = '';
    imapBusy = true;
    try {
      const result = readImapAccountAdded(await invoke<unknown>('imap_account_add', {
        input: {
          displayName: imapForm.displayName.trim(),
          email: imapForm.email.trim(),
          host: imapForm.host.trim(),
          port: imapForm.port,
          username: imapForm.username.trim(),
          smtpHost: imapForm.smtpHost.trim(),
          smtpPort: imapForm.smtpPort,
          smtpTlsMode: imapForm.smtpTlsMode,
          smtpUsername: imapForm.smtpUsername.trim()
        }
      }));
      if (!result) return;
      await resetAccountWizard();
      const folders = result.folders === 1 ? '1 folder' : `${result.folders} folders`;
      settingsMessage = `Connected ${result.email} — ${folders}.`;
      await refreshMailbox();
    } catch (cause) {
      settingsError = settingsErrorText(cause);
    } finally {
      imapBusy = false;
    }
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

  async function setAccountColor(accountId: string, name: string, value: string) {
    if (settingAccountColors.includes(accountId) || removingAccountId) return;
    settingsError = '';
    settingsMessage = '';
    // Checked here as well as in the store: the colour is written into inline
    // styles, and the interface should not offer one the store would refuse.
    const color = normalizeAccountColor(value);
    if (!color) {
      settingsError = 'Mux only takes a colour written as #rrggbb.';
      return;
    }
    settingAccountColors = [...settingAccountColors, accountId];
    try {
      await invoke('set_account_color', { input: { accountId, color } });
      await refreshMailbox();
      settingsMessage = `${name} is now ${describeAccountColor(color)}.`;
    } catch (cause) {
      settingsError = settingsErrorText(cause);
    } finally {
      settingAccountColors = settingAccountColors.filter((id) => id !== accountId);
    }
  }

  async function connectGmail() {
    if (gmailOAuthBusy || removingAccountId) return;
    settingsError = '';
    settingsMessage = '';
    gmailOAuthBusy = true;
    try {
      const result = readGmailOAuthResult(await invoke<unknown>('gmail_oauth_begin'));
      settingsMessage = `Connected ${result.email}.`;
      await resetAccountWizard();
      await refreshMailbox();
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

  async function removeAccount(accountId: string, accountName: string) {
    if (gmailOAuthBusy || imapBusy || settingAccountColors.length || removingAccountId || removeConfirmationId !== accountId) return;
    settingsError = '';
    settingsMessage = '';
    removingAccountId = accountId;
    try {
      const removed = readRemovedAccount(await invoke<unknown>('account_remove', {
        input: { accountId }
      }));
      if (removed.accountId !== accountId) throw new Error('Mux removed a different account than requested.');
      removeConfirmationId = '';
      settingsMessage = removed.keychainCleanupPending
        ? `Removed ${accountName}. Saved sign-in cleanup will finish when Mux can next reach the Keychain.`
        : `Removed ${accountName} from this Mac. Mail on the server was not changed.`;
      await refreshMailbox();
    } catch (cause) {
      settingsError = settingsErrorText(cause);
    } finally {
      removingAccountId = '';
    }
  }

  async function requestAccountRemoval(accountId: string, trigger: HTMLButtonElement) {
    if (gmailOAuthBusy || imapBusy || settingAccountColors.length || removingAccountId) return;
    removalTriggerButton = trigger;
    removeConfirmationId = accountId;
    clearSettingsFeedback();
    await tick();
    removalCancelButton?.focus();
  }

  async function cancelAccountRemoval() {
    removeConfirmationId = '';
    await tick();
    removalTriggerButton?.focus();
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
              {@const currentColor = normalizeAccountColor(account.color)}
              <li>
                <div class="settings-account-head">
                  <span class="settings-account-dot" style:--avatar-color={account.color}></span>
                  <span><strong>{account.name}</strong><small>{account.email}</small></span>
                  <em>{account.total} messages</em>
                </div>
                <div class="settings-account-refresh">
                  <div class="settings-account-control-head">
                    <span>Check for new mail</span>
                    <div class="settings-account-buttons">
                      <button
                        class="settings-sync-now"
                        type="button"
                        data-action="sync-now"
                        data-account-id={account.id}
                        title={`Check ${account.name} for new mail right now`}
                        disabled={syncingAccounts.includes(account.id) || removingAccountId === account.id}
                        on:click={() => syncAccountNow(account.id, account.name)}
                      >{syncingAccounts.includes(account.id) ? 'Checking…' : 'Sync now'}</button>
                      <button
                        class="settings-remove-account"
                        type="button"
                        data-testid="remove-account"
                        data-account-id={account.id}
                        disabled={gmailOAuthBusy || imapBusy || settingAccountColors.length > 0 || Boolean(removingAccountId)}
                        on:click={(event) => { void requestAccountRemoval(account.id, event.currentTarget); }}
                      >Remove</button>
                    </div>
                  </div>
                  <div class="settings-choice-row" role="group" aria-label={`Refresh interval for ${account.name}`} data-testid="refresh-interval" data-account-id={account.id}>
                    {#each refreshChoices as choice}
                      <button
                        class:is-active={account.refreshSeconds === choice.value}
                        type="button"
                        disabled={removingAccountId === account.id}
                        on:click={() => setAccountRefresh(account.id, choice.value)}
                      >{choice.label}</button>
                    {/each}
                  </div>
                </div>
                <div class="settings-account-color">
                  <span>Colour</span>
                  <div class="settings-choice-row settings-accents" role="group" aria-label={`Colour for ${account.name}`} data-testid="account-color" data-account-id={account.id}>
                    {#each accountColorChoices as choice}
                      <button
                        class:is-active={currentColor === choice.value}
                        type="button"
                        data-account-color={choice.value}
                        aria-pressed={currentColor === choice.value}
                        aria-label={`${choice.label} for ${account.name}`}
                        title={`Use ${choice.label.toLocaleLowerCase()} for ${account.name}`}
                        disabled={settingAccountColors.includes(account.id) || Boolean(removingAccountId)}
                        on:click={() => setAccountColor(account.id, account.name, choice.value)}
                      >
                        <span class="accent-swatch" style:background={choice.value} aria-hidden="true"></span>{choice.label}
                      </button>
                    {/each}
                    <!-- A label rather than a button, so the native picker can sit in
                         the row: a button may not hold an input. -->
                    <label
                      class="settings-color-custom"
                      class:is-active={!accountColorChoices.some((choice) => choice.value === currentColor)}
                      title={`Pick any colour for ${account.name}`}
                    >
                      <input
                        type="color"
                        value={currentColor ?? '#000000'}
                        aria-label={`Custom colour for ${account.name}`}
                        data-testid="account-color-custom"
                        disabled={settingAccountColors.includes(account.id) || Boolean(removingAccountId)}
                        on:change={(event) => setAccountColor(account.id, account.name, event.currentTarget.value)}
                      />Custom
                    </label>
                  </div>
                </div>
                {#if removeConfirmationId === account.id}
                  <div
                    class="settings-remove-confirmation"
                    role="group"
                    aria-labelledby={`remove-account-${account.id}`}
                    aria-describedby={`remove-account-description-${account.id}`}
                  >
                    <strong id={`remove-account-${account.id}`}>Remove {account.name}?</strong>
                    <p id={`remove-account-description-${account.id}`}>This removes its downloaded mail, drafts, pending changes, and saved sign-in from this Mac. Mail on the server is not changed.</p>
                    <div class="settings-actions">
                      <button bind:this={removalCancelButton} type="button" disabled={Boolean(removingAccountId)} on:click={() => { void cancelAccountRemoval(); }}>Cancel</button>
                      <button
                        class="danger-button"
                        type="button"
                        data-testid="confirm-remove-account"
                        disabled={gmailOAuthBusy || imapBusy || settingAccountColors.length > 0 || Boolean(removingAccountId)}
                        on:click={() => removeAccount(account.id, account.name)}
                      >{removingAccountId === account.id ? 'Removing…' : 'Remove account'}</button>
                    </div>
                  </div>
                {/if}
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
          <div class="provider-wizard" data-testid="account-wizard" data-step={accountWizardStep}>
            {#if accountWizardStep === 'email'}
              <form on:submit|preventDefault={startAccountWizard}>
                <h4>Type your email</h4>
                <label class="provider-wizard-email">
                  <span>Email address</span>
                  <input bind:this={accountEmailInput} bind:value={accountEmail} type="email" autocomplete="email" spellcheck="false" placeholder="you@example.com" data-testid="account-email" />
                </label>
                <div class="settings-actions">
                  <button class="primary-button" type="submit" data-testid="account-email-continue" disabled={!wizardEmailReady || Boolean(removingAccountId)}>Continue</button>
                </div>
              </form>
            {:else if accountWizardStep === 'mailbox-type'}
              <div class="provider-wizard-heading">
                <button type="button" on:click={() => { void moveAccountWizard('email'); }}>Back</button>
                <div><span>Mailbox type</span><strong>{accountEmail}</strong></div>
              </div>
              <h4 bind:this={wizardStepHeading} tabindex="-1">Where is this mailbox hosted?</h4>
              <div class="provider-choice-list">
                <button type="button" data-testid="choose-google" disabled={Boolean(removingAccountId)} on:click={chooseGoogleMailbox}>
                  <strong>Google Workspace</strong>
                  <span>Continue with Google in your browser.</span>
                </button>
                <button type="button" data-testid="choose-imap" disabled={Boolean(removingAccountId)} on:click={chooseImapMailbox}>
                  <strong>Another mail provider</strong>
                  <span>Connect using IMAP for incoming mail and SMTP for sending.</span>
                </button>
              </div>
            {:else if accountWizardStep === 'gmail'}
              <div class="provider-wizard-heading">
                <button type="button" disabled={gmailOAuthBusy} on:click={() => { void moveAccountWizard('email'); }}>Back</button>
                <div><span>Google sign-in</span><strong>{accountEmail}</strong></div>
              </div>
              <div class="provider-gmail-step">
                <h4 bind:this={wizardStepHeading} tabindex="-1">{gmailOAuthBusy ? 'Opening Google sign-in…' : 'Continue with Google'}</h4>
                <p>Mux never sees your Google password. The Google account you approve becomes the connected address.</p>
                <div class="settings-actions">
                  {#if gmailOAuthBusy}
                    <button type="button" on:click={cancelGmailOAuth}>Cancel</button>
                  {:else}
                    <button class="primary-button" type="button" disabled={Boolean(removingAccountId)} on:click={connectGmail}>Try Google sign-in again</button>
                  {/if}
                </div>
              </div>
            {:else}
              <form class="provider-form provider-wizard-form" data-testid="imap-form" on:submit|preventDefault={advanceImapWizard}>
                <div class="provider-wizard-heading">
                  <button type="button" disabled={imapBusy} on:click={() => { void backFromImapWizard(); }}>Back</button>
                  <div>
                    <span>{accountWizardStep === 'imap-incoming' ? 'Step 1 of 3' : accountWizardStep === 'imap-outgoing' ? 'Step 2 of 3' : 'Step 3 of 3'}</span>
                    <strong>{accountEmail}</strong>
                  </div>
                </div>

                {#if accountWizardStep === 'imap-incoming'}
                  <h3 bind:this={wizardStepHeading} tabindex="-1">Incoming mail</h3>
                  <p>Enter the IMAP details from your mail provider.</p>
                  <div class="provider-fields">
                    <label>
                      <span>Account name</span>
                      <input bind:value={imapForm.displayName} type="text" autocomplete="off" placeholder="Optional" data-testid="imap-name" disabled={imapBusy} />
                    </label>
                    <label class="provider-server">
                      <span>IMAP server</span>
                      <input bind:value={imapForm.host} type="text" autocomplete="off" spellcheck="false" placeholder="imap.example.com" data-testid="imap-host" disabled={imapBusy} />
                    </label>
                    <label class="provider-port">
                      <span>Port</span>
                      <input bind:value={imapForm.port} type="number" min="1" max="65535" data-testid="imap-port" disabled={imapBusy} />
                    </label>
                    <label>
                      <span>IMAP username</span>
                      <input bind:value={imapForm.username} type="text" autocomplete="off" spellcheck="false" data-testid="imap-username" disabled={imapBusy} />
                    </label>
                  </div>
                  <div class="settings-actions">
                    <button class="primary-button" type="submit" data-testid="imap-incoming-continue" disabled={!imapIncomingReady || Boolean(removingAccountId)}>Continue</button>
                  </div>
                {:else if accountWizardStep === 'imap-outgoing'}
                  <h3 bind:this={wizardStepHeading} tabindex="-1">Outgoing mail</h3>
                  <p>Enter the SMTP details used to send mail.</p>
                  <div class="provider-fields">
                    <label class="provider-server">
                      <span>SMTP server</span>
                      <input bind:value={imapForm.smtpHost} type="text" autocomplete="off" spellcheck="false" placeholder="smtp.example.com" data-testid="smtp-host" disabled={imapBusy} />
                    </label>
                    <label class="provider-port">
                      <span>Port</span>
                      <input bind:value={imapForm.smtpPort} type="number" min="1" max="65535" data-testid="smtp-port" disabled={imapBusy} />
                    </label>
                    <label>
                      <span>Security</span>
                      <select bind:value={imapForm.smtpTlsMode} data-testid="smtp-tls-mode" disabled={imapBusy}>
                        <option value="starttls">STARTTLS (usually port 587)</option>
                        <option value="implicit">Implicit TLS (usually port 465)</option>
                      </select>
                    </label>
                    <label>
                      <span>SMTP username</span>
                      <input bind:value={imapForm.smtpUsername} type="text" autocomplete="off" spellcheck="false" data-testid="smtp-username" disabled={imapBusy} />
                    </label>
                  </div>
                  <div class="settings-actions">
                    <button class="primary-button" type="submit" data-testid="imap-outgoing-continue" disabled={!imapOutgoingReady || Boolean(removingAccountId)}>Review</button>
                  </div>
                {:else}
                  <h3 bind:this={wizardStepHeading} tabindex="-1">Ready to connect</h3>
                  <p>Passwords are requested next in a native macOS dialog and never enter this interface.</p>
                  <dl class="provider-review">
                    <div><dt>Incoming</dt><dd>{imapForm.host}:{imapForm.port} · {imapForm.username}</dd></div>
                    <div><dt>Outgoing</dt><dd>{imapForm.smtpHost}:{imapForm.smtpPort} · {imapForm.smtpUsername}</dd></div>
                    <div><dt>Security</dt><dd>IMAP implicit TLS · SMTP {imapForm.smtpTlsMode === 'implicit' ? 'implicit TLS' : 'STARTTLS'}</dd></div>
                  </dl>
                  <div class="settings-actions">
                    <button class="primary-button" type="submit" data-testid="imap-submit" disabled={imapBusy || Boolean(removingAccountId)}>
                      {imapBusy ? 'Checking the servers…' : 'Connect account'}
                    </button>
                  </div>
                {/if}
              </form>
            {/if}
          </div>
        </section>
      {:else if settingsSection === 'appearance'}
        <header class="settings-heading">
          <h2>Appearance</h2>
          <p>Mux follows this choice on every launch.</p>
        </header>
        <!-- Answers every choice below it, so it comes first and stays put. The
             account colour is borrowed rather than passed as a preference: the
             stripe means "this account", and inventing one would misdescribe it. -->
        <ThreadRowSample {appearance} accountColor={mailbox.accounts[0]?.color ?? null} />
        <section class="settings-card">
          <h3>Theme</h3>
          <div class="settings-choice-row" role="group" aria-label="Theme" data-testid="theme-choice">
          {#each themeChoices as choice}
            <button
              class:is-active={themePreference === choice.value}
              type="button"
              data-action={`theme-${choice.value}`}
              data-theme-option={choice.value}
              title={choice.value === 'system'
                ? `Follow this Mac's appearance — ${theme} right now`
                : `Use the ${choice.label.toLocaleLowerCase()} appearance`}
              on:click={() => setTheme(choice.value)}
            >
              {#if choice.value !== 'system'}
                <span aria-hidden="true"><Icon name={choice.value === 'light' ? 'sun' : 'moon'} size={16} /></span>
              {/if}{choice.label}
            </button>
          {/each}
          </div>
        </section>

        <section class="settings-card">
          <h3>Accent</h3>
          <div class="settings-choice-row settings-accents" role="group" aria-label="Accent colour" data-testid="accent-choice">
          {#each accentChoices as choice}
            <button
              class:is-active={appearance.accent === choice.value}
              type="button"
              data-accent-option={choice.value}
              on:click={() => applyAppearance({ ...appearance, accent: choice.value })}
            >
              {#if accentSwatch(choice.value, theme)}
                <span class="accent-swatch" style:background={accentSwatch(choice.value, theme)} aria-hidden="true"></span>
              {/if}{choice.label}
            </button>
          {/each}
          </div>
        </section>

        <section class="settings-card">
          <h3>Text and typeface</h3>
          <p class="settings-hint">Sizes everything Mux draws, from headings down to the smallest label. A message's own HTML keeps the sizes its sender chose.</p>
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
          <p class="settings-hint">How tightly rows, cards, and the sidebar pack. Text keeps the size chosen above.</p>
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

        <section class="settings-card">
          <h3>Message list</h3>
          <p class="settings-hint">The third line of every row in the list.</p>
          <div class="settings-toggle-row" data-testid="list-display">
          <label><input type="checkbox" bind:checked={appearance.listSnippet} on:change={() => applyAppearance(appearance)} /><span>Show preview of body text in mail list</span></label>
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
