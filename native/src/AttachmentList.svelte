<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { onDestroy } from 'svelte';
  import { decodeAttachmentBase64 } from './attachmentContent.mjs';
  import type { AttachmentContent, AttachmentSummary } from './types';

  export let attachments: AttachmentSummary[] = [];

  type Preview = { id: string; filename: string; mediaType: string; url: string; text: string };
  let preview: Preview | null = null;
  let loadingId = '';
  let error = '';

  function releasePreview() {
    if (preview?.url) URL.revokeObjectURL(preview.url);
    preview = null;
  }

  async function load(attachment: AttachmentSummary): Promise<AttachmentContent | null> {
    loadingId = attachment.id;
    error = '';
    try {
      return await invoke<AttachmentContent>('read_attachment', { attachmentId: attachment.id });
    } catch (cause) {
      error = cause instanceof Error ? cause.message : String(cause);
      return null;
    } finally {
      loadingId = '';
    }
  }

  function bytesFor(content: AttachmentContent): Uint8Array | null {
    try {
      return decodeAttachmentBase64(content.dataBase64, content.byteLength);
    } catch (cause) {
      error = cause instanceof Error ? cause.message : 'Attachment content is invalid';
      return null;
    }
  }

  function blobBuffer(bytes: Uint8Array): ArrayBuffer {
    const buffer = new ArrayBuffer(bytes.byteLength);
    new Uint8Array(buffer).set(bytes);
    return buffer;
  }

  async function showPreview(attachment: AttachmentSummary) {
    if (preview?.id === attachment.id) {
      releasePreview();
      return;
    }
    const content = await load(attachment);
    if (!content) return;
    releasePreview();
    const bytes = bytesFor(content);
    if (!bytes) return;
    const text = content.mediaType.startsWith('text/') ? new TextDecoder().decode(bytes) : '';
    const url = content.mediaType.startsWith('image/')
      ? URL.createObjectURL(new Blob([blobBuffer(bytes)], { type: content.mediaType }))
      : '';
    preview = { id: attachment.id, filename: content.filename, mediaType: content.mediaType, url, text };
  }

  async function download(attachment: AttachmentSummary) {
    const content = await load(attachment);
    if (!content) return;
    const bytes = bytesFor(content);
    if (!bytes) return;
    const url = URL.createObjectURL(new Blob([blobBuffer(bytes)], { type: content.mediaType }));
    const anchor = document.createElement('a');
    anchor.href = url;
    anchor.download = content.filename;
    anchor.click();
    window.setTimeout(() => URL.revokeObjectURL(url), 0);
  }

  function readableSize(bytes: number): string {
    if (bytes < 1024) return `${bytes} B`;
    return `${(bytes / 1024).toFixed(bytes < 10_240 ? 1 : 0)} KB`;
  }

  onDestroy(releasePreview);
</script>

{#if attachments.length}
  <section class="attachments" aria-label="Attachments">
    {#each attachments as attachment}
      <div class="attachment-row">
        <span class="attachment-icon" aria-hidden="true">{attachment.mediaType.startsWith('image/') ? '▧' : '▤'}</span>
        <span class="attachment-name"><strong>{attachment.filename}</strong><small>{attachment.disposition === 'inline' ? 'Inline image' : attachment.mediaType} · {readableSize(attachment.byteLength)}</small></span>
        {#if attachment.mediaType.startsWith('image/') || attachment.mediaType.startsWith('text/')}
          <button type="button" disabled={loadingId === attachment.id} on:click={() => showPreview(attachment)}>{preview?.id === attachment.id ? 'Close' : loadingId === attachment.id ? 'Loading…' : 'Preview'}</button>
        {/if}
        <button type="button" disabled={loadingId === attachment.id} on:click={() => download(attachment)}>Download</button>
      </div>
      {#if preview?.id === attachment.id}
        <div class="attachment-preview">
          {#if preview.url}<img src={preview.url} alt={`Preview of ${preview.filename}`} />{/if}
          {#if preview.text}<pre>{preview.text}</pre>{/if}
        </div>
      {/if}
    {/each}
    {#if error}<small class="attachment-error" role="alert">{error}</small>{/if}
  </section>
{/if}
