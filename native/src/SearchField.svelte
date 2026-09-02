<script lang="ts">
  import { tick } from 'svelte';
  import Icon from './Icon.svelte';
  import { activeWordAt, searchSegments, searchSuggestions } from './searchQuery';
  import type { SavedSearch, SearchSuggestion, SuggestionAccount } from './searchQuery';

  export let value = '';
  export let accounts: SuggestionAccount[] = [];
  export let saved: SavedSearch[] = [];
  export let placeholder = 'Search mail';
  export let label = 'Search mail';
  export let hint = '';
  export let oninput: () => void = () => {};
  export let onsave: (query: string) => void = () => {};
  export let onforget: (query: string) => void = () => {};

  let input: HTMLInputElement | undefined;
  let overlay: HTMLDivElement | undefined;
  let open = false;
  let caret = 0;
  /// -1 is "nothing highlighted", which is where the list always starts: Enter
  /// means run the search, and it can only mean anything else once someone has
  /// deliberately walked into the list.
  let index = -1;

  /// Colouring reads the raw string, so it stays right mid-word and mid-quote.
  $: segments = searchSegments(value);
  $: suggestions = open ? searchSuggestions({ text: value, caret, accounts, saved }) : [];
  $: if (index >= suggestions.length) index = -1;
  $: activeOption = index >= 0 ? `search-suggestion-${index}` : undefined;
  /// Tab completes what is being typed, so it never lands on the save row.
  $: firstCompletion = suggestions.findIndex((suggestion) => suggestion.kind !== 'save');

  export function focus() {
    input?.focus();
    input?.select();
    open = true;
  }

  export function blur() {
    open = false;
    input?.blur();
  }

  /// The coloured text sits on top of the field, so it has to follow the input
  /// when a long query scrolls sideways.
  function syncScroll() {
    if (overlay && input) overlay.scrollLeft = input.scrollLeft;
  }

  function readCaret() {
    caret = input?.selectionStart ?? value.length;
  }

  function changed() {
    readCaret();
    open = true;
    index = -1;
    oninput();
    void tick().then(syncScroll);
  }

  async function apply(suggestion: SearchSuggestion) {
    if (suggestion.kind === 'save') {
      onsave(value.trim());
      open = false;
      return;
    }
    value = suggestion.text;
    caret = suggestion.caret;
    index = -1;
    // The parent reads the bound value, so it is told only once the binding has
    // certainly carried the completion out of this component.
    await tick();
    input?.focus();
    input?.setSelectionRange(suggestion.caret, suggestion.caret);
    syncScroll();
    oninput();
  }

  /// Clicking the magnifier or the padding should land in the field, which is
  /// what the label element used to do for free.
  function focusFromChrome(event: MouseEvent) {
    const target = event.target as HTMLElement | null;
    if (!target || target === input || target.closest('.search-suggestions')) return;
    event.preventDefault();
    input?.focus();
  }

  function forget(suggestion: SearchSuggestion) {
    onforget(suggestion.text);
    index = -1;
    input?.focus();
  }

  function onKeydown(event: KeyboardEvent) {
    // The dropdown closes, and the same Escape carries on to the window
    // handler that clears the search: one press, one meaning.
    if (event.key === 'Escape') {
      open = false;
      return;
    }
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      if (!open) {
        open = true;
        index = -1;
        return;
      }
      if (!suggestions.length) return;
      const step = event.key === 'ArrowDown' ? 1 : -1;
      // Wrapping through -1 gives a way back out of the list to plain Enter.
      index = ((index + step + 1 + suggestions.length + 1) % (suggestions.length + 1)) - 1;
      return;
    }
    // Results are live as they are typed, so Enter runs the search by doing
    // nothing to it. It acts on a row only once one has been walked to, which
    // is the only way saving can happen from the keyboard.
    if (event.key === 'Enter') {
      const chosen = index >= 0 ? suggestions[index] : undefined;
      if (!chosen) {
        open = false;
        return;
      }
      event.preventDefault();
      void apply(chosen);
      return;
    }
    // Tab completes what is being typed. With nothing typed there is nothing to
    // complete, and Tab belongs to the browser.
    if (event.key === 'Tab') {
      const target = index >= 0 && suggestions[index]?.kind !== 'save' ? index : firstCompletion;
      if (!open || target < 0 || !activeWordAt(value, caret)) return;
      event.preventDefault();
      void apply(suggestions[target]);
      return;
    }
    void tick().then(readCaret);
  }

  function markFor(kind: SearchSuggestion['kind']): string {
    if (kind === 'saved') return '★';
    if (kind === 'save') return '+';
    return '#';
  }
</script>

<!-- svelte-ignore a11y-no-static-element-interactions -->
<div class="filter search-field" on:mousedown={focusFromChrome}>
  <Icon name="search" size={17} />
  <div class="search-input">
    <div class="search-highlight" bind:this={overlay} aria-hidden="true">
      {#each segments as segment}<span class={`token ${segment.kind}`}>{segment.text}</span>{/each}
    </div>
    <input
      bind:this={input}
      bind:value
      type="text"
      role="combobox"
      aria-label={label}
      aria-expanded={open && suggestions.length > 0}
      aria-controls="search-suggestions"
      aria-activedescendant={open ? activeOption : undefined}
      aria-autocomplete="list"
      autocomplete="off"
      spellcheck="false"
      {placeholder}
      data-testid="mailbox-search"
      on:input={changed}
      on:keydown={onKeydown}
      on:keyup={readCaret}
      on:click={() => { open = true; readCaret(); }}
      on:scroll={syncScroll}
      on:focus={() => { open = true; readCaret(); }}
      on:blur={() => { open = false; }}
    />
  </div>
  {#if hint}<kbd>{hint}</kbd>{/if}

  {#if open && suggestions.length}
    <div id="search-suggestions" class="search-suggestions" role="listbox" aria-label="Search suggestions" data-testid="search-suggestions">
      {#each suggestions as suggestion, position (suggestion.id)}
        <div
          id={`search-suggestion-${position}`}
          class="search-suggestion"
          class:is-selected={position === index}
          role="option"
          tabindex="-1"
          aria-selected={position === index}
          data-suggestion={suggestion.id}
          on:mouseenter={() => { index = position; }}
          on:mousedown|preventDefault={() => void apply(suggestion)}
        >
          <span class={`suggestion-mark ${suggestion.kind}`} aria-hidden="true">{markFor(suggestion.kind)}</span>
          <span class="suggestion-text">
            <strong>{suggestion.label}</strong>
            {#if suggestion.detail}<small>{suggestion.detail}</small>{/if}
          </span>
          {#if suggestion.kind === 'saved'}
            <button
              class="suggestion-forget"
              type="button"
              tabindex="-1"
              aria-label={`Forget ${suggestion.label}`}
              title="Forget this search"
              on:mousedown|preventDefault|stopPropagation={() => forget(suggestion)}
            >✕</button>
          {/if}
        </div>
      {/each}
    </div>
  {/if}
</div>
