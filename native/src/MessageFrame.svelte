<script lang="ts">
  import { onDestroy, onMount, tick } from 'svelte';
  import {
    FALLBACK_THEME,
    MESSAGE_FRAME_SANDBOX,
    measuredFrameHeight,
    messageFrameDocument
  } from './messageFrame';
  import type { MessageFrameImage, MessageFrameTheme } from './messageFrame';

  export let bodyHtml = '';
  export let remoteImages: Record<number, MessageFrameImage> = {};
  export let label = 'Message body';

  /// A document whose height depends on the frame's height can be measured
  /// slightly taller every pass. The frame's own stylesheet refuses that
  /// dependency; this is the backstop for a message that gets around it.
  const MAX_HEIGHT_ADJUSTMENTS = 12;

  let frame: HTMLIFrameElement | undefined;
  let height = 0;
  let adjustments = 0;
  let observer: ResizeObserver | undefined;
  let pendingMeasure = 0;
  /// Read from the document root rather than the element, so the frame's
  /// document is built once instead of being rebuilt when the element binds.
  /// Colors and the typeface only: the root also carries the text-size scale
  /// and its type steps, and none of them is read here on purpose — a message
  /// renders at the sizes its sender wrote, whatever size the interface is.
  function currentTheme(): MessageFrameTheme {
    if (typeof getComputedStyle !== 'function') return FALLBACK_THEME;
    const root = getComputedStyle(document.documentElement);
    const variable = (name: string, value: string) => root.getPropertyValue(name).trim() || value;
    return {
      text: variable('--text-soft', FALLBACK_THEME.text),
      muted: variable('--text-faint', FALLBACK_THEME.muted),
      background: 'transparent',
      link: variable('--accent', FALLBACK_THEME.link),
      border: variable('--border', FALLBACK_THEME.border),
      fontFamily: variable('--app-font', FALLBACK_THEME.fontFamily)
    };
  }

  let theme = currentTheme();
  $: source = messageFrameDocument(bodyHtml, remoteImages, theme);

  function measure() {
    const measured = measuredFrameHeight(frame?.contentDocument);
    if (measured === null) return;
    // Settling resets the budget; only a document that will not settle spends it.
    if (Math.abs(measured - height) <= 1) {
      adjustments = 0;
      return;
    }
    if (adjustments >= MAX_HEIGHT_ADJUSTMENTS) return;
    adjustments += 1;
    height = measured;
  }

  /// Every expanded message holds a frame, and a drag-resize fires continuously,
  /// so measuring is deferred to one frame rather than run per event.
  function scheduleMeasure() {
    if (pendingMeasure || typeof requestAnimationFrame !== 'function') {
      if (typeof requestAnimationFrame !== 'function') measure();
      return;
    }
    pendingMeasure = requestAnimationFrame(() => {
      pendingMeasure = 0;
      // A new width is a genuinely new layout, not a failure to settle.
      adjustments = 0;
      measure();
    });
  }

  async function frameLoaded() {
    await tick();
    adjustments = 0;
    measure();
    // The frame's own load event waits for its subresources, so the document
    // has settled by now. What still changes the needed height afterwards is
    // the frame getting narrower or wider, which is a change to this element
    // and so is observed from this document rather than from inside the frame.
    if (observer || typeof ResizeObserver !== 'function' || !frame) return;
    observer = new ResizeObserver(() => measure());
    observer.observe(frame);
  }

  onMount(() => {
    // The frame inherits no styling, so the theme is copied in as literal
    // values. Those values go stale the moment the reader switches theme or
    // picks a different font, both of which land on the document root.
    if (typeof MutationObserver !== 'function') return;
    const watcher = new MutationObserver(() => { theme = currentTheme(); });
    watcher.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ['data-theme', 'style', 'class']
    });
    return () => watcher.disconnect();
  });

  onDestroy(() => {
    observer?.disconnect();
    if (pendingMeasure && typeof cancelAnimationFrame === 'function') {
      cancelAnimationFrame(pendingMeasure);
    }
  });
</script>

<svelte:window on:resize={scheduleMeasure} />

<iframe
  bind:this={frame}
  class="message-frame"
  title={label}
  sandbox={MESSAGE_FRAME_SANDBOX}
  srcdoc={source}
  style:height={height ? `${height}px` : undefined}
  on:load={frameLoaded}
></iframe>
