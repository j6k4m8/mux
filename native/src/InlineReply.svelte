<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { createEventDispatcher, onDestroy, tick } from 'svelte';
  import { shouldExpandInlineReply } from './replyRecipients';
  import RichEditor from './RichEditor.svelte';
  import { plainTextToRichHtml } from './richText';
  import type { AccountSummary, DraftSummary, OperationSummary, SaveDraftInput, ThreadSummary } from './types';

  export let accounts: AccountSummary[] = [];
  export let thread: ThreadSummary;
  export let draft: DraftSummary | null = null;
  export let replyRecipient = '';
  export let replyAllRecipients = '';

  type ReplyMode = 'reply' | 'replyAll';

  const dispatch = createEventDispatcher<{
    saved: DraftSummary;
    queued: OperationSummary;
    popout: { draft: DraftSummary | null; mode: ReplyMode };
  }>();

  let editor: RichEditor;
  let currentDraft = draft;
  let mode: ReplyMode = draft && replyAllRecipients && draft.recipients === replyAllRecipients ? 'replyAll' : 'reply';
  let recipients = draft?.recipients ?? replyRecipient;
  let ccRecipients = draft?.ccRecipients ?? '';
  let bccRecipients = draft?.bccRecipients ?? '';
  let body = draft?.body ?? '';
  let bodyHtml = draft?.bodyHtml || plainTextToRichHtml(draft?.body ?? '');
  let expanded = shouldExpandInlineReply(false, body, bodyHtml);
  let dirty = false;
  let sending = false;
  let status = currentDraft ? 'Saved' : 'Ready';
  let error = '';
  let saveTimer: number | undefined;
  let saveInFlight: Promise<DraftSummary> | null = null;

  function replySubject(value: string): string {
    return /^re:/iu.test(value.trim()) ? value : `Re: ${value}`;
  }

  function hasMessage(): boolean {
    return Boolean(body.trim());
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
    expanded = shouldExpandInlineReply(expanded, body, bodyHtml);
    markDirty();
  }

  function setMode(nextMode: ReplyMode) {
    mode = nextMode;
    recipients = nextMode === 'replyAll' ? replyAllRecipients : replyRecipient;
    if (hasMessage() || currentDraft) markDirty();
    void tick().then(() => editor?.focus());
  }

  async function saveNow(force = false): Promise<DraftSummary | null> {
    if (currentDraft?.locked) return currentDraft;
    if (force && (hasMessage() || currentDraft)) dirty = true;
    window.clearTimeout(saveTimer);
    if (saveInFlight) {
      await saveInFlight;
      return dirty ? saveNow() : currentDraft;
    }
    if (!dirty || (!hasMessage() && !currentDraft)) return currentDraft;
    dirty = false;
    status = 'Saving…';
    const input: SaveDraftInput = {
      id: currentDraft?.id ?? null,
      accountId: currentDraft?.accountId ?? thread.accountId ?? accounts[0]?.id ?? '',
      recipients,
      ccRecipients,
      bccRecipients,
      subject: currentDraft?.subject ?? replySubject(thread.subject),
      body,
      bodyHtml,
      replyToThreadId: thread.id,
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
      status = 'Could not save';
      error = cause instanceof Error ? cause.message : String(cause);
    } finally {
      saveInFlight = null;
    }
    return dirty && !error ? saveNow() : currentDraft;
  }

  async function popOut() {
    const saved = await saveNow(true);
    if (!error) dispatch('popout', { draft: saved, mode });
  }

  async function send() {
    if (sending || currentDraft?.locked) return;
    if (!hasMessage() || !recipients.trim()) {
      error = 'Add a recipient and message before sending.';
      return;
    }
    sending = true;
    error = '';
    const saved = await saveNow(true);
    if (!saved || error) {
      sending = false;
      return;
    }
    try {
      const operation = await invoke<OperationSummary>('queue_send', {
        draftId: saved.id,
        undoWindowMs: 5_000
      });
      status = 'Reply queued';
      dispatch('queued', operation);
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause);
      sending = false;
    }
  }

  async function finishEditing() {
    await saveNow(true);
    editor?.blur();
  }

  export async function focus(nextMode: ReplyMode = 'reply') {
    setMode(nextMode);
    await tick();
    editor?.focus();
  }

  onDestroy(() => {
    window.clearTimeout(saveTimer);
    if (dirty) void saveNow(true);
  });
</script>

<section class="inline-reply" class:is-expanded={expanded} aria-label="Quick reply">
  <header>
    <div class="reply-mode" aria-label="Reply recipients">
      <button type="button" class:is-active={mode === 'reply'} on:click={() => setMode('reply')}>Reply</button>
      <button type="button" class:is-active={mode === 'replyAll'} on:click={() => setMode('replyAll')}>Reply all</button>
    </div>
    <span class="reply-to" title={recipients}>To {recipients || 'recipient'}</span>
    <button class="popout-button" type="button" aria-label="Pop reply out into composer" title="Open in composer" on:click={popOut}>↗ <span>Pop out</span></button>
  </header>

  <RichEditor
    bind:this={editor}
    value={bodyHtml}
    disabled={sending || currentDraft?.locked}
    showToolbar={expanded}
    compact={!expanded}
    on:change={editorChanged}
    on:requestexpand={() => { expanded = true; }}
    on:submit={send}
    on:escape={finishEditing}
  />

  <footer>
    <div class="quick-status" class:has-error={Boolean(error)}>
      {#if error}{error}{:else if status !== 'Ready'}{status}{/if}
    </div>
    <div class="quick-actions">
      {#if !expanded}
        <button type="button" class="format-button" on:click={() => { expanded = true; void tick().then(() => editor?.focus()); }}>Formatting</button>
      {/if}
      <button class="send-button" type="button" disabled={sending || currentDraft?.locked || !hasMessage()} on:click={send}>
        {sending ? 'Queueing…' : 'Send'} <kbd>⌘↵</kbd>
      </button>
    </div>
  </footer>
</section>
