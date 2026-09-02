<script lang="ts">
  import { onMount, tick } from 'svelte';
  import Icon from './Icon.svelte';
  import { rankByFuzzy } from './fuzzy';
  import { trapModalKeydown } from './modalFocus';
  import type { ListRow } from './jumpDialog';

  export let title: string;
  export let placeholder = 'Type to filter…';
  export let rows: ListRow[] = [];
  export let emptyLabel = 'Nothing matches';
  export let testid = 'jump-dialog';
  export let onselect: (id: string) => void;
  export let onclose: () => void;

  let dialog: HTMLDivElement | undefined;
  let list: HTMLDivElement | undefined;
  let input: HTMLInputElement | undefined;
  let query = '';
  let index = 0;

  $: matches = rankByFuzzy(
    query,
    rows,
    (row) => [`${row.title} ${row.subtitle} ${row.chip ?? ''}`, `${row.subtitle} ${row.title}`],
    (row) => Boolean(row.priority)
  );
  $: if (index >= matches.length) index = 0;
  $: countLabel = matches.length === 1 ? '1 result' : `${matches.length} results`;

  onMount(() => {
    input?.focus();
  });

  async function move(step: number) {
    if (!matches.length) return;
    index = (index + step + matches.length) % matches.length;
    await tick();
    list?.querySelector('[aria-selected="true"]')?.scrollIntoView({ block: 'nearest' });
  }

  function choose(id: string) {
    onselect(id);
  }

  function onKeydown(event: KeyboardEvent) {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault();
      void move(event.key === 'ArrowDown' ? 1 : -1);
      return;
    }
    if (event.key === 'Enter') {
      event.preventDefault();
      const row = matches[index];
      if (row) choose(row.id);
      return;
    }
    trapModalKeydown(event, dialog, onclose);
  }
</script>

<div class="modal-backdrop command-backdrop" role="presentation" on:click|self={() => onclose()}>
  <div
    bind:this={dialog}
    class="modal-card command-dialog jump-dialog"
    role="dialog"
    aria-modal="true"
    aria-label={title}
    data-testid={testid}
    tabindex="-1"
    on:keydown={onKeydown}
  >
    <header class="jump-header">
      <h1>{title}</h1>
      <small>{countLabel}</small>
    </header>
    <label class="command-filter">
      <Icon name="search" size={17} />
      <input
        bind:this={input}
        bind:value={query}
        type="text"
        aria-label={`${title} filter`}
        autocomplete="off"
        spellcheck="false"
        {placeholder}
        data-testid={`${testid}-filter`}
        on:input={() => { index = 0; }}
      />
      <kbd>esc</kbd>
    </label>
    <div bind:this={list} class="command-list" role="listbox" aria-label={title}>
      {#if matches.length}
        {#each matches as row, position (row.id)}
          <button
            class:is-selected={position === index}
            type="button"
            role="option"
            aria-selected={position === index}
            data-row={row.id}
            on:mouseenter={() => { index = position; }}
            on:click={() => choose(row.id)}
          >
            {#if row.dot}<span class="jump-dot" style:background={row.dot} aria-hidden="true"></span>{/if}
            <span><strong>{row.title}</strong><small>{row.subtitle}</small></span>
            {#if row.chip}<kbd>{row.chip}</kbd>{/if}
          </button>
        {/each}
      {:else}
        <div class="empty"><strong>{emptyLabel}</strong></div>
      {/if}
    </div>
  </div>
</div>
