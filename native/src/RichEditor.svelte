<script lang="ts">
  import { createEventDispatcher, onMount } from 'svelte';
  import { normalizeRichHtml, parseRichText, richTextToPlain, safeHref } from './richText';
  import type { RichNode } from './richText';

  export let value = '';
  export let disabled = false;
  export let autofocus = false;
  export let showToolbar = true;
  export let compact = false;

  const dispatch = createEventDispatcher<{
    change: { html: string; text: string };
    escape: void;
    requestexpand: void;
    submit: void;
  }>();

  let editor: HTMLDivElement;
  let linkInput: HTMLInputElement;
  let linkOpen = false;
  let linkValue = '';
  let linkError = '';
  let editingLink = false;
  let savedRange: Range | null = null;
  let lastEmitted = normalizeRichHtml(value);
  let toolbarIndex = 0;

  onMount(() => {
    renderValue(value);
    const focusTimer = autofocus ? window.setTimeout(() => editor?.focus(), 0) : undefined;
    return () => window.clearTimeout(focusTimer);
  });

  function appendNode(parent: Node, node: RichNode) {
    if (node.type === 'text') {
      parent.appendChild(document.createTextNode(node.text));
      return;
    }
    const element = document.createElement(node.tag);
    if (node.tag === 'a' && node.href) element.setAttribute('href', node.href);
    for (const child of node.children) appendNode(element, child);
    parent.appendChild(element);
  }

  function renderValue(html: string) {
    const fragment = document.createDocumentFragment();
    for (const node of parseRichText(html)) appendNode(fragment, node);
    editor.replaceChildren(fragment);
  }

  function emitChange(normalize = false) {
    const normalized = normalizeRichHtml(editor.innerHTML);
    if (normalize && normalized !== editor.innerHTML) renderValue(normalized);
    value = normalized;
    if (normalized === lastEmitted) return;
    lastEmitted = normalized;
    dispatch('change', { html: normalized, text: richTextToPlain(normalized) });
  }

  function format(command: string) {
    if (disabled) return;
    if (!showToolbar) dispatch('requestexpand');
    editor.focus();
    document.execCommand(command, false);
    emitChange();
  }

  function rememberSelection() {
    const selection = window.getSelection();
    savedRange = selection?.rangeCount ? selection.getRangeAt(0).cloneRange() : null;
  }

  function openLink() {
    if (disabled) return;
    if (!showToolbar) dispatch('requestexpand');
    rememberSelection();
    const selection = window.getSelection();
    const selectionElement = selection?.anchorNode instanceof HTMLElement
      ? selection.anchorNode
      : selection?.anchorNode?.parentElement;
    const existingLink = selectionElement?.closest('a');
    editingLink = Boolean(existingLink && editor.contains(existingLink));
    if (editingLink && existingLink) {
      const range = document.createRange();
      range.selectNodeContents(existingLink);
      savedRange = range;
    }
    if (!savedRange || savedRange.collapsed) {
      linkError = 'Select the text you want to link first.';
    } else {
      linkError = '';
    }
    linkValue = editingLink && existingLink ? existingLink.getAttribute('href') ?? '' : '';
    linkOpen = true;
    window.setTimeout(() => linkInput.focus(), 0);
  }

  function applyLink() {
    const href = safeHref(linkValue);
    if (!href) {
      linkError = 'Use an http, https, or mailto link.';
      return;
    }
    if (!savedRange || savedRange.collapsed) {
      linkError = 'Select the text you want to link first.';
      return;
    }
    const selection = window.getSelection();
    selection?.removeAllRanges();
    selection?.addRange(savedRange);
    document.execCommand('createLink', false, href);
    linkOpen = false;
    editor.focus();
    emitChange(true);
  }

  function removeLink() {
    if (!savedRange) return;
    const selection = window.getSelection();
    selection?.removeAllRanges();
    selection?.addRange(savedRange);
    document.execCommand('unlink', false);
    linkOpen = false;
    editingLink = false;
    editor.focus();
    emitChange(true);
  }

  function editorKeydown(event: KeyboardEvent) {
    if ((event.metaKey || event.ctrlKey) && !event.altKey) {
      if (event.key === 'Enter') {
        event.preventDefault();
        dispatch('submit');
        return;
      }
      const command = { b: 'bold', i: 'italic', u: 'underline' }[event.key.toLocaleLowerCase()];
      if (command) {
        event.preventDefault();
        format(command);
        return;
      }
      if (event.key.toLocaleLowerCase() === 'k') {
        event.preventDefault();
        openLink();
        return;
      }
    }
    if (event.key === 'Escape') {
      event.preventDefault();
      dispatch('escape');
    }
  }

  export function focus() {
    editor?.focus();
    editor?.scrollIntoView({ block: 'nearest' });
  }

  export function blur() {
    editor?.blur();
  }

  export function appendHtml(html: string) {
    if (disabled) return;
    renderValue(normalizeRichHtml(`${editor.innerHTML}${html}`));
    emitChange(true);
    editor.focus();
    const range = document.createRange();
    range.selectNodeContents(editor);
    range.collapse(false);
    const selection = window.getSelection();
    selection?.removeAllRanges();
    selection?.addRange(range);
  }

  function linkKeydown(event: KeyboardEvent) {
    if (event.key === 'Enter') {
      event.preventDefault();
      applyLink();
    } else if (event.key === 'Escape') {
      event.preventDefault();
      event.stopPropagation();
      linkOpen = false;
      editor.focus();
    }
  }

  function toolbarKeydown(event: KeyboardEvent, index: number) {
    const keys = ['ArrowLeft', 'ArrowRight', 'Home', 'End'];
    if (!keys.includes(event.key)) return;
    const buttons = [...(event.currentTarget as HTMLElement).parentElement!.querySelectorAll<HTMLButtonElement>('button')];
    if (!buttons.length) return;
    event.preventDefault();
    if (event.key === 'Home') toolbarIndex = 0;
    else if (event.key === 'End') toolbarIndex = buttons.length - 1;
    else {
      const step = event.key === 'ArrowRight' ? 1 : -1;
      toolbarIndex = (index + step + buttons.length) % buttons.length;
    }
    buttons[toolbarIndex]?.focus();
  }

  function keyboardActivate(event: MouseEvent, action: () => void) {
    if (event.detail === 0) action();
  }
</script>

<div class="editor-shell" class:is-disabled={disabled} class:is-compact={compact} class:has-toolbar={showToolbar}>
  {#if showToolbar}
    <div class="format-toolbar" role="toolbar" aria-label="Message formatting">
      <button type="button" tabindex={toolbarIndex === 0 ? 0 : -1} aria-label="Bold (Command B)" title="Bold (⌘B)" on:focus={() => { toolbarIndex = 0; }} on:keydown={(event) => toolbarKeydown(event, 0)} on:mousedown|preventDefault={() => format('bold')} on:click={(event) => keyboardActivate(event, () => format('bold'))}><strong>B</strong></button>
      <button type="button" tabindex={toolbarIndex === 1 ? 0 : -1} aria-label="Italic (Command I)" title="Italic (⌘I)" on:focus={() => { toolbarIndex = 1; }} on:keydown={(event) => toolbarKeydown(event, 1)} on:mousedown|preventDefault={() => format('italic')} on:click={(event) => keyboardActivate(event, () => format('italic'))}><em>I</em></button>
      <button type="button" tabindex={toolbarIndex === 2 ? 0 : -1} aria-label="Underline (Command U)" title="Underline (⌘U)" on:focus={() => { toolbarIndex = 2; }} on:keydown={(event) => toolbarKeydown(event, 2)} on:mousedown|preventDefault={() => format('underline')} on:click={(event) => keyboardActivate(event, () => format('underline'))}><u>U</u></button>
      <span></span>
      <button type="button" tabindex={toolbarIndex === 3 ? 0 : -1} aria-label="Bulleted list" title="Bulleted list" on:focus={() => { toolbarIndex = 3; }} on:keydown={(event) => toolbarKeydown(event, 3)} on:mousedown|preventDefault={() => format('insertUnorderedList')} on:click={(event) => keyboardActivate(event, () => format('insertUnorderedList'))}>• List</button>
      <button type="button" tabindex={toolbarIndex === 4 ? 0 : -1} aria-label="Numbered list" title="Numbered list" on:focus={() => { toolbarIndex = 4; }} on:keydown={(event) => toolbarKeydown(event, 4)} on:mousedown|preventDefault={() => format('insertOrderedList')} on:click={(event) => keyboardActivate(event, () => format('insertOrderedList'))}>1. List</button>
      <span></span>
      <button type="button" tabindex={toolbarIndex === 5 ? 0 : -1} aria-label="Insert link (Command K)" title="Insert link (⌘K)" on:focus={() => { toolbarIndex = 5; }} on:keydown={(event) => toolbarKeydown(event, 5)} on:mousedown|preventDefault={openLink} on:click={(event) => keyboardActivate(event, openLink)}>Link</button>
    </div>
  {/if}
  <div
    class="rich-editor"
    class:is-empty={!richTextToPlain(value)}
    bind:this={editor}
    contenteditable="true"
    aria-disabled={disabled}
    role="textbox"
    aria-label="Message body"
    aria-multiline="true"
    data-placeholder="Write a message…"
    tabindex="0"
    on:beforeinput={(event) => { if (disabled) event.preventDefault(); }}
    on:input={() => emitChange()}
    on:blur={() => emitChange(true)}
    on:keydown={editorKeydown}
  ></div>
  {#if linkOpen}
    <div class="link-popover" role="dialog" aria-label="Insert link">
      <label for="composer-link">Link destination</label>
      <div>
        <input id="composer-link" bind:this={linkInput} bind:value={linkValue} placeholder="https://example.com" on:keydown={linkKeydown} />
        <button type="button" on:click={applyLink}>Apply</button>
        {#if editingLink}<button type="button" class="unlink-button" on:click={removeLink}>Remove</button>{/if}
        <button type="button" aria-label="Cancel link" on:click={() => { linkOpen = false; editor.focus(); }}>Cancel</button>
      </div>
      {#if linkError}<small role="alert">{linkError}</small>{/if}
    </div>
  {/if}
</div>
