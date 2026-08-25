<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { createEventDispatcher, onDestroy } from 'svelte';
  import RecipientField from './RecipientField.svelte';
  import RichEditor from './RichEditor.svelte';
  import { normalizeRichHtml, plainTextToRichHtml, richTextToPlain } from './richText';
  import type { AccountSummary, DraftSummary, MessageSummary, OperationSummary, SaveDraftInput, ThreadSummary } from './types';

  export let accounts: AccountSummary[] = [];
  export let draft: DraftSummary | null = null;
  export let replyThread: ThreadSummary | null = null;
  export let replyRecipient = '';
  export let replyAll = false;
  export let forwardThread: ThreadSummary | null = null;
  export let quoteSource: MessageSummary | null = null;

  const dispatch = createEventDispatcher<{
    close: void;
    saved: DraftSummary;
    deleted: string;
    queued: OperationSummary;
  }>();

  let currentDraft = draft;
  let editor: RichEditor;
  let composerDialog: HTMLElement;
  let accountId = draft?.accountId ?? replyThread?.accountId ?? forwardThread?.accountId ?? accounts[0]?.id ?? '';
  let recipients = draft?.recipients ?? (replyThread ? replyRecipient : '');
  let ccRecipients = draft?.ccRecipients ?? '';
  let bccRecipients = draft?.bccRecipients ?? '';
  let subject = draft?.subject ?? (replyThread ? replySubject(replyThread.subject) : forwardThread ? forwardSubject(forwardThread.subject) : '');
  let bodyHtml = draft?.bodyHtml || plainTextToRichHtml(draft?.body ?? '') || (forwardThread && quoteSource ? `<p><br></p>${quotedMessageHtml(quoteSource)}` : '');
  let body = draft?.body ?? richTextToPlain(bodyHtml);
  let showCcBcc = Boolean(ccRecipients || bccRecipients);
  let status = draft?.locked ? 'Waiting for send confirmation' : 'Ready';
  let error = '';
  let dirty = !draft && Boolean(replyThread || forwardThread) && hasContent();
  let sending = false;
  let saveTimer: number | undefined;
  let saveInFlight: Promise<DraftSummary> | null = null;

  function replySubject(value: string): string {
    return /^re:/iu.test(value.trim()) ? value : `Re: ${value}`;
  }

  function forwardSubject(value: string): string {
    return /^fwd:/iu.test(value.trim()) ? value : `Fwd: ${value.replace(/^re:\s*/iu, '')}`;
  }

  function quotedMessageHtml(message: MessageSummary): string {
    const header = `On ${new Intl.DateTimeFormat(undefined, { dateStyle: 'medium', timeStyle: 'short' }).format(message.sentAt)}, ${message.senderName} <${message.senderEmail}> wrote:`;
    const safeBody = message.bodyHtml ? normalizeRichHtml(message.bodyHtml) : plainTextToRichHtml(message.bodyText);
    return `<blockquote>${plainTextToRichHtml(header)}${safeBody}</blockquote>`;
  }

  function hasContent(): boolean {
    return Boolean(recipients.trim() || ccRecipients.trim() || bccRecipients.trim() || subject.trim() || body.trim());
  }

  function markDirty() {
    if (currentDraft?.locked) return;
    dirty = true;
    status = 'Unsaved changes';
    error = '';
    window.clearTimeout(saveTimer);
    saveTimer = window.setTimeout(() => void saveNow(), 500);
  }

  function editorChanged(event: CustomEvent<{ html: string; text: string }>) {
    bodyHtml = event.detail.html;
    body = event.detail.text;
    markDirty();
  }

  async function saveNow(force = false): Promise<DraftSummary | null> {
    if (currentDraft?.locked) return currentDraft;
    if (force && hasContent()) dirty = true;
    window.clearTimeout(saveTimer);
    if (saveInFlight) {
      await saveInFlight;
      return dirty ? saveNow() : currentDraft;
    }
    if (!dirty || !hasContent()) return currentDraft;
    dirty = false;
    status = 'Saving…';
    const input: SaveDraftInput = {
      id: currentDraft?.id ?? null,
      accountId,
      recipients,
      ccRecipients,
      bccRecipients,
      subject,
      body,
      bodyHtml,
      replyToThreadId: currentDraft?.replyToThreadId ?? replyThread?.id ?? null,
      expectedRevision: currentDraft?.revision ?? null
    };
    saveInFlight = invoke<DraftSummary>('save_draft', { input });
    try {
      const saved = await saveInFlight;
      currentDraft = saved;
      status = 'Saved';
      error = '';
      dispatch('saved', saved);
    } catch (cause) {
      dirty = true;
      error = cause instanceof Error ? cause.message : String(cause);
      status = 'Could not save';
    } finally {
      saveInFlight = null;
    }
    return dirty && !error ? saveNow() : currentDraft;
  }

  async function closeComposer() {
    if (sending) return;
    await saveNow();
    if (!error) dispatch('close');
  }

  async function send() {
    if (sending || currentDraft?.locked) return;
    sending = true;
    error = '';
    const saved = await saveNow(true);
    if (!saved || error) {
      sending = false;
      if (!error) error = 'Add a recipient and message before sending.';
      return;
    }
    try {
      const operation = await invoke<OperationSummary>('queue_send', {
        draftId: saved.id,
        undoWindowMs: 5_000
      });
      dispatch('queued', operation);
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause);
      sending = false;
    }
  }

  async function removeDraft() {
    if (!currentDraft || currentDraft.locked) return;
    if (!window.confirm('Delete this draft?')) return;
    try {
      await invoke('delete_draft', { draftId: currentDraft.id });
      dispatch('deleted', currentDraft.id);
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause);
    }
  }

  function insertQuote() {
    if (!quoteSource || currentDraft?.locked) return;
    editor?.appendHtml(quotedMessageHtml(quoteSource));
  }

  function insertSignature() {
    if (currentDraft?.locked) return;
    const signature = accounts.find((account) => account.id === accountId)?.signature.trim();
    if (!signature) return;
    editor?.appendHtml(`<p>—</p>${plainTextToRichHtml(signature)}`);
  }

  function windowKeydown(event: KeyboardEvent) {
    if (event.defaultPrevented) return;
    if ((event.metaKey || event.ctrlKey) && event.key === 'Enter') {
      event.preventDefault();
      void send();
    } else if (event.key === 'Escape') {
      event.preventDefault();
      void closeComposer();
    }
  }

  function backdropClick(event: MouseEvent) {
    if (event.target === event.currentTarget) void closeComposer();
  }

  function composerDialogKeydown(event: KeyboardEvent) {
    if (event.key !== 'Tab') return;
    const focusable = [...composerDialog.querySelectorAll<HTMLElement>(
      'button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), a[href], [contenteditable="true"], [tabindex]:not([tabindex="-1"])'
    )].filter((element) => !element.hasAttribute('hidden'));
    if (!focusable.length) {
      event.preventDefault();
      composerDialog.focus();
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

  onDestroy(() => window.clearTimeout(saveTimer));
</script>

<svelte:window on:keydown={windowKeydown} />

<div class="composer-backdrop" role="presentation" on:mousedown={backdropClick}>
  <div bind:this={composerDialog} class="composer" role="dialog" aria-modal="true" aria-labelledby="composer-title" tabindex="-1" on:mousedown|stopPropagation on:keydown={composerDialogKeydown}>
    <header>
      <div>
        <small>{replyThread ? (replyAll ? 'Reply all' : 'Reply') : forwardThread ? 'Forward' : currentDraft ? 'Draft' : 'New message'}</small>
        <h1 id="composer-title">{replyThread?.subject ?? forwardThread?.subject ?? currentDraft?.subject ?? 'Compose'}</h1>
      </div>
      <button class="icon-button" type="button" aria-label="Save and close" title="Save and close (Esc)" on:click={closeComposer}>×</button>
    </header>

    <div class="composer-fields">
      <label>
        <span>From</span>
        <select bind:value={accountId} disabled={Boolean(currentDraft) || Boolean(replyThread) || Boolean(forwardThread)} on:change={markDirty}>
          {#each accounts as account}
            <option value={account.id}>{account.name} — {account.email}</option>
          {/each}
        </select>
      </label>
      <RecipientField label="To" value={recipients} disabled={currentDraft?.locked} on:change={(event) => { recipients = event.detail.value; markDirty(); }} />
      {#if showCcBcc}
        <RecipientField label="Cc" value={ccRecipients} disabled={currentDraft?.locked} on:change={(event) => { ccRecipients = event.detail.value; markDirty(); }} />
        <RecipientField label="Bcc" value={bccRecipients} disabled={currentDraft?.locked} on:change={(event) => { bccRecipients = event.detail.value; markDirty(); }} />
      {:else}
        <button class="reveal-recipients" type="button" on:click={() => { showCcBcc = true; }}>Add Cc or Bcc</button>
      {/if}
      <label>
        <span>Subject</span>
        <input bind:value={subject} disabled={currentDraft?.locked} aria-label="Subject" placeholder="Subject" on:input={markDirty} />
      </label>
    </div>

    <RichEditor bind:this={editor} value={bodyHtml} disabled={currentDraft?.locked} autofocus on:change={editorChanged} on:escape={closeComposer} />

    <footer>
      <div class="save-status" class:has-error={Boolean(error)}>
        <span>{error || status}</span>
        {#if !error && !currentDraft?.locked}<small>Autosaves as you type</small>{/if}
      </div>
      <div class="composer-actions">
        {#if quoteSource && !forwardThread}<button type="button" on:click={insertQuote}>Quote original</button>{/if}
        {#if accounts.find((account) => account.id === accountId)?.signature}<button type="button" on:click={insertSignature}>Signature</button>{/if}
        {#if currentDraft && !currentDraft.locked}
          <button class="danger-button" type="button" on:click={removeDraft}>Delete</button>
        {/if}
        <button type="button" on:click={closeComposer}>Save & close</button>
        <button class="send-button" type="button" disabled={sending || currentDraft?.locked} on:click={send}>
          {sending ? 'Queueing…' : 'Send'} <kbd>⌘↵</kbd>
        </button>
      </div>
    </footer>
  </div>
</div>
