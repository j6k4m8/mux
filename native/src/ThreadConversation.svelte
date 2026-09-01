<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { tick } from 'svelte';
  import AttachmentList from './AttachmentList.svelte';
  import MessageFrame from './MessageFrame.svelte';
  import { newContentParagraphs, plainTextParagraphs, safeRemoteImageDataUrl } from './richText';
  import type { AttachmentSummary, MessageSummary, RemoteImageContent, RemoteImageSummary } from './types';

  export let messages: MessageSummary[] = [];
  export let attachments: AttachmentSummary[] = [];
  export let accountColor = '#7180ff';
  export let threadUnread = false;
  export let railPreview = true;

  /// The two newest messages open with the thread. The component now outlives
  /// each background refresh, so a message that arrives later opens on arrival,
  /// while one the reader collapsed stays collapsed.
  const seen = { ids: new Set<number>(), primed: false };
  let expandedIds = new Set<number>();

  $: {
    if (!seen.primed && messages.length) {
      seen.primed = true;
      seen.ids = new Set(messages.map((message) => message.id));
      expandedIds = new Set(messages.slice(-2).map((message) => message.id));
    } else if (seen.primed) {
      const arrived = messages.filter((message) => !seen.ids.has(message.id));
      if (arrived.length) {
        seen.ids = new Set(messages.map((message) => message.id));
        expandedIds = new Set([...expandedIds, ...arrived.map((message) => message.id)]);
      }
    }
  }
  let bottomAnchor: HTMLDivElement;
  let loadedImageData = new Map<string, string>();
  let loadingMessageIds = new Set<number>();
  let focusedId: number | null = null;
  let remoteImageErrors = new Map<number, string>();
  let allowedDomains = new Map<number, Set<string>>();
  let remoteImageViews: Record<number, Record<number, { dataUrl: string | null; altText: string }>> = {};
  let blockedCounts: Record<number, number> = {};
  let unloadedImageCounts: Record<number, number> = {};
  let domainDialogMessageId: number | null = null;
  const autoRequestedKeys = new Set<string>();
  const MAX_REMOTE_IMAGES_PER_ACTION = 64;

  $: unreadMessageId = threadUnread
    ? [...messages].reverse().find((message) => !message.isFromMe)?.id ?? messages.at(-1)?.id ?? null
    : null;
  $: remoteImageViews = Object.fromEntries(messages.map((message) => [
    message.id,
    Object.fromEntries(message.remoteImages.map((image) => [
      image.id,
      { dataUrl: loadedImageData.get(imageKey(message.id, image.id)) ?? null, altText: image.altText }
    ]))
  ]));
  $: unloadedImageCounts = Object.fromEntries(messages.map((message) => [
    message.id,
    message.remoteImages.filter((image) => !loadedImageData.has(imageKey(message.id, image.id))).length
  ]));
  $: blockedCounts = Object.fromEntries(messages.map((message) => [
    message.id,
    Math.max(0, message.blockedRemoteResources - message.remoteImages.length)
      + (unloadedImageCounts[message.id] ?? 0)
  ]));
  $: void autoLoadPolicyImages(messages);

  function imageKey(messageId: number, resourceId: number): string {
    return `${messageId}:${resourceId}`;
  }

  function uniqueDomains(message: MessageSummary): string[] {
    return [...new Set(message.remoteImages.map((image) => image.domain).filter(Boolean))].sort();
  }

  async function fetchRemoteImage(
    message: MessageSummary,
    image: RemoteImageSummary
  ): Promise<[string, string] | null> {
    try {
      const loaded = await invoke<RemoteImageContent>('load_remote_image', {
        input: { messageId: message.id, resourceId: image.id }
      });
      if (
        loaded.messageId !== message.id
        || loaded.resourceId !== image.id
        || !safeRemoteImageDataUrl(loaded.dataUrl)
      ) throw new Error('invalid remote image response');
      return [imageKey(message.id, image.id), loaded.dataUrl];
    } catch {
      return null;
    }
  }

  async function loadImages(message: MessageSummary, requested = message.remoteImages): Promise<void> {
    if (loadingMessageIds.has(message.id)) return;
    const images = requested
      .filter((image) => !loadedImageData.has(imageKey(message.id, image.id)))
      .slice(0, MAX_REMOTE_IMAGES_PER_ACTION);
    if (!images.length) return;
    loadingMessageIds = new Set(loadingMessageIds).add(message.id);
    remoteImageErrors = new Map(remoteImageErrors).set(message.id, '');
    let failed = 0;
    // Every approved image is applied in one assignment. Applying them one at a
    // time rebuilds the reader frame's document once per image, which reloads
    // the message the reader is looking at as many times as it has pictures.
    const fetched: Array<[string, string]> = [];
    for (const image of images) {
      const loaded = await fetchRemoteImage(message, image);
      if (loaded) fetched.push(loaded);
      else failed += 1;
    }
    if (fetched.length) {
      const next = new Map(loadedImageData);
      for (const [key, dataUrl] of fetched) next.set(key, dataUrl);
      loadedImageData = next;
    }
    const nextLoading = new Set(loadingMessageIds);
    nextLoading.delete(message.id);
    loadingMessageIds = nextLoading;
    if (failed || requested.length > MAX_REMOTE_IMAGES_PER_ACTION) {
      const detail = requested.length > MAX_REMOTE_IMAGES_PER_ACTION
        ? `Mux loaded the first ${MAX_REMOTE_IMAGES_PER_ACTION} images.`
        : 'Some images could not be loaded.';
      remoteImageErrors = new Map(remoteImageErrors).set(message.id, detail);
    }
  }

  async function autoLoadPolicyImages(currentMessages: MessageSummary[]): Promise<void> {
    for (const message of currentMessages) {
      const policyImages = message.remoteImages.filter((image) => {
        const key = imageKey(message.id, image.id);
        if (!image.allowedByPolicy || loadedImageData.has(key) || autoRequestedKeys.has(key)) return false;
        autoRequestedKeys.add(key);
        return true;
      });
      if (policyImages.length) await loadImages(message, policyImages);
    }
  }

  async function alwaysLoadFromSender(message: MessageSummary): Promise<void> {
    remoteImageErrors = new Map(remoteImageErrors).set(message.id, '');
    try {
      await invoke('allow_remote_content_sender', { input: { messageId: message.id } });
      await loadImages(message);
    } catch {
      remoteImageErrors = new Map(remoteImageErrors).set(message.id, 'Mux could not save that sender preference.');
    }
  }

  async function allowDomain(message: MessageSummary, domain: string): Promise<void> {
    remoteImageErrors = new Map(remoteImageErrors).set(message.id, '');
    try {
      await invoke('allow_remote_content_domain', { input: { messageId: message.id, domain } });
      const nextAllowed = new Map(allowedDomains);
      nextAllowed.set(message.id, new Set(nextAllowed.get(message.id) ?? []).add(domain));
      allowedDomains = nextAllowed;
      await loadImages(message, message.remoteImages.filter((image) => image.domain === domain));
    } catch {
      remoteImageErrors = new Map(remoteImageErrors).set(message.id, 'Mux could not save that domain preference.');
    }
  }

  function closeDomainDialog() {
    domainDialogMessageId = null;
  }

  function handleWindowKeydown(event: KeyboardEvent) {
    if (domainDialogMessageId !== null && event.key === 'Escape') {
      event.preventDefault();
      event.stopPropagation();
      closeDomainDialog();
    }
  }

  function initials(name: string): string {
    return name.split(/\s+/u).map((part) => part[0] ?? '').join('').slice(0, 2).toLocaleUpperCase();
  }

  function fullTime(timestamp: number): string {
    return new Intl.DateTimeFormat(undefined, {
      weekday: 'short', month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit'
    }).format(timestamp);
  }

  /// Sender, when, and the opening of the message, for the rail's hover card.
  function railTooltip(message: MessageSummary): string {
    const preview = message.bodyText.replace(/\s+/gu, ' ').trim().slice(0, 140);
    const head = `${message.senderName} · ${fullTime(message.sentAt)}`;
    return preview ? `${head}\n${preview}` : head;
  }

  function toggle(messageId: number) {
    const next = new Set(expandedIds);
    if (next.has(messageId)) next.delete(messageId);
    else next.add(messageId);
    expandedIds = next;
  }

  /// Message-level focus, driven by j/k once the reader is entered from the list.
  export function focusedMessageId(): number | null {
    return focusedId;
  }

  export async function focusFirstMessage() {
    const target = unreadMessageId ?? messages.at(-1)?.id ?? null;
    await revealMessage(target);
  }

  export async function moveMessageFocus(delta: number) {
    if (!messages.length) return;
    const current = messages.findIndex((message) => message.id === focusedId);
    const start = current === -1 ? (delta > 0 ? -1 : messages.length) : current;
    const next = Math.min(messages.length - 1, Math.max(0, start + delta));
    await revealMessage(messages[next]?.id ?? null);
  }

  export function clearMessageFocus() {
    focusedId = null;
  }

  async function revealMessage(messageId: number | null) {
    focusedId = messageId;
    if (messageId === null) return;
    expandedIds = new Set(expandedIds).add(messageId);
    await tick();
    document
      .getElementById(`native-message-${messageId}`)
      ?.scrollIntoView({ behavior: 'smooth', block: 'nearest' });
  }

  export async function jumpToNewest() {
    await tick();
    bottomAnchor?.scrollIntoView({ behavior: 'smooth', block: 'end' });
  }

  export async function jumpToUnread() {
    if (!unreadMessageId) return jumpToNewest();
    expandedIds = new Set(expandedIds).add(unreadMessageId);
    await tick();
    document.getElementById(`native-message-${unreadMessageId}`)?.scrollIntoView({ behavior: 'smooth', block: 'center' });
  }
</script>

<svelte:window on:keydown={handleWindowKeydown} />

{#if messages.length > 1}
  <nav class="message-rail" aria-label="Jump to a message" data-testid="message-rail">
    {#each messages as message, index (message.id)}
      <button
        class:is-focused={focusedId === message.id}
        class:is-unread={message.id === unreadMessageId}
        class:is-mine={message.isFromMe}
        type="button"
        data-message-id={message.id}
        aria-label={`Message ${index + 1} of ${messages.length} from ${message.senderName}`}
        title={railPreview ? railTooltip(message) : undefined}
        on:click={() => revealMessage(message.id)}
      ></button>
    {/each}
  </nav>
{/if}
<div class="message-stack">
  {#each messages as message (message.id)}
    {#if message.id === unreadMessageId}
      <div class="unread-boundary" id="native-unread-boundary"><span>Unread from here</span></div>
    {/if}
    <section
      class="message-card"
      class:is-collapsed={!expandedIds.has(message.id)}
      class:is-focused={focusedId === message.id}
      class:is-mine={message.isFromMe}
      id={`native-message-${message.id}`}
    >
      <button
        class="message-card-toggle"
        type="button"
        aria-expanded={expandedIds.has(message.id)}
        aria-controls={`native-message-body-${message.id}`}
        on:click={() => toggle(message.id)}
      >
        <span class="avatar compact" style:--avatar-color={message.isFromMe ? '#6e75ff' : accountColor}>{initials(message.senderName)}</span>
        {#if expandedIds.has(message.id)}
          <span class="message-meta"><strong>{message.senderName}</strong><small>{message.senderEmail} → {message.recipients}{message.ccRecipients ? ` · Cc ${message.ccRecipients}` : ''}</small></span>
          <time>{fullTime(message.sentAt)}</time>
        {:else}
          {@const collapsed = newContentParagraphs(message.bodyText)}
          <span class="collapsed-summary">
            <span class="collapsed-heading"><strong>{message.senderName}</strong><time>{fullTime(message.sentAt)}</time></span>
            <span class="collapsed-preview">
              {#each collapsed.paragraphs as lines}
                <span class="collapsed-paragraph">{#each lines as line, index}{#if index > 0}<br />{/if}{line}{/each}</span>
              {/each}
              {#if collapsed.trimmed}<span class="collapsed-trimmed">quoted text and signature hidden</span>{/if}
            </span>
          </span>
        {/if}
        <span class="collapse-glyph" aria-hidden="true">{expandedIds.has(message.id) ? '⌃' : '⌄'}</span>
      </button>
      {#if expandedIds.has(message.id)}
        <div class="message-body" id={`native-message-body-${message.id}`}>
          {#if blockedCounts[message.id] > 0}
            <div class="remote-resource-notice" role="status">
              <span class="remote-resource-icon" aria-hidden="true">▧</span>
              <span>{blockedCounts[message.id] === 1 ? 'Remote content blocked' : `${blockedCounts[message.id]} remote items blocked`}</span>
              {#if unloadedImageCounts[message.id] > 0}
                <button type="button" disabled={loadingMessageIds.has(message.id)} on:click={() => loadImages(message)}>
                  {loadingMessageIds.has(message.id) ? 'Loading…' : 'Load images'}
                </button>
                <button class="remote-resource-icon-button" type="button" title="Always load from sender" aria-label="Always load from sender" disabled={loadingMessageIds.has(message.id)} on:click={() => alwaysLoadFromSender(message)}>♙+</button>
              {/if}
              {#if unloadedImageCounts[message.id] > 0 && uniqueDomains(message).length}
                <button class="remote-resource-icon-button" type="button" title="See blocked domains" aria-label="See blocked domains" on:click={() => { domainDialogMessageId = message.id; }}>◎</button>
              {/if}
            </div>
          {/if}
          {#if remoteImageErrors.get(message.id)}
            <small class="remote-resource-error" role="alert">{remoteImageErrors.get(message.id)}</small>
          {/if}
          {#if message.bodyHtml}
            <MessageFrame bodyHtml={message.bodyHtml} remoteImages={remoteImageViews[message.id] ?? {}} label={`Message from ${message.senderName || message.senderEmail}`} />
          {:else}
            {#each plainTextParagraphs(message.bodyText) as lines}
              <p>{#each lines as line, index}{#if index > 0}<br />{/if}{line}{/each}</p>
            {/each}
          {/if}
          <AttachmentList attachments={attachments.filter((attachment) => attachment.messageId === message.id)} />
        </div>
      {/if}
    </section>
  {/each}
</div>
<div class="thread-bottom-anchor" bind:this={bottomAnchor}></div>

{#if domainDialogMessageId !== null}
  {@const dialogMessage = messages.find((message) => message.id === domainDialogMessageId)}
  {#if dialogMessage}
    <div class="remote-domain-backdrop" role="presentation" on:click|self={closeDomainDialog}>
      <div class="remote-domain-dialog" role="dialog" aria-modal="true" aria-labelledby="remote-domain-title" tabindex="-1">
        <header>
          <h2 id="remote-domain-title">Remote content domains</h2>
          <button type="button" aria-label="Close remote content domains" on:click={closeDomainDialog}>×</button>
        </header>
        <div class="remote-domain-list">
          {#each uniqueDomains(dialogMessage) as domain}
            <div class="remote-domain-row">
              <span>{domain}</span>
              <button
                type="button"
                disabled={allowedDomains.get(dialogMessage.id)?.has(domain) ?? false}
                on:click={() => allowDomain(dialogMessage, domain)}
              >{allowedDomains.get(dialogMessage.id)?.has(domain) ? 'Allowed' : 'Allow'}</button>
            </div>
          {/each}
        </div>
        <footer><button type="button" on:click={closeDomainDialog}>Close</button></footer>
      </div>
    </div>
  {/if}
{/if}
