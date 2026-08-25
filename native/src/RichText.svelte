<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { safeRemoteImageDataUrl } from './richText';
  import type { RichNode } from './richText';

  export let nodes: RichNode[] = [];
  export let remoteImages: Record<number, { dataUrl: string | null; altText: string }> = {};

  let linkError = '';

  async function openMessageLink(event: MouseEvent, destination: string) {
    event.preventDefault();
    linkError = '';
    try {
      await invoke('open_message_link', { destination });
    } catch {
      linkError = 'Mux could not open that link.';
    }
  }
</script>

{#each nodes as node}
  {#if node.type === 'text'}
    {node.text}
  {:else if node.type === 'remote-image'}
    {@const image = remoteImages[node.resourceId]}
    {@const safeDataUrl = safeRemoteImageDataUrl(image?.dataUrl ?? '')}
    {#if safeDataUrl}
      <img class="remote-message-image" src={safeDataUrl} alt={image.altText} loading="lazy" decoding="async" />
    {:else if image?.altText}
      <span class="remote-image-placeholder" role="img" aria-label={image.altText}>{image.altText}</span>
    {/if}
  {:else if node.tag === 'p'}
    <p><svelte:self nodes={node.children} {remoteImages} /></p>
  {:else if node.tag === 'div'}
    <div><svelte:self nodes={node.children} {remoteImages} /></div>
  {:else if node.tag === 'strong'}
    <strong><svelte:self nodes={node.children} {remoteImages} /></strong>
  {:else if node.tag === 'em'}
    <em><svelte:self nodes={node.children} {remoteImages} /></em>
  {:else if node.tag === 'u'}
    <u><svelte:self nodes={node.children} {remoteImages} /></u>
  {:else if node.tag === 'ul'}
    <ul><svelte:self nodes={node.children} {remoteImages} /></ul>
  {:else if node.tag === 'ol'}
    <ol><svelte:self nodes={node.children} {remoteImages} /></ol>
  {:else if node.tag === 'li'}
    <li><svelte:self nodes={node.children} {remoteImages} /></li>
  {:else if node.tag === 'blockquote'}
    <blockquote><svelte:self nodes={node.children} {remoteImages} /></blockquote>
  {:else if node.tag === 'a'}
    <a
      href={node.href}
      rel="noopener noreferrer"
      data-message-link
      on:click={(event) => openMessageLink(event, node.href ?? '')}
      on:auxclick|preventDefault={() => undefined}
    ><svelte:self nodes={node.children} {remoteImages} /></a>
  {:else if node.tag === 'br'}
    <br />
  {/if}
{/each}
{#if linkError}<small class="message-link-error" role="alert">{linkError}</small>{/if}
