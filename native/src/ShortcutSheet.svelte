<script lang="ts">
  import { onMount } from 'svelte';
  import { trapModalKeydown } from './modalFocus';
  import { shortcutChips, shortcutGroups, shortcutsInGroup, visibleBindings } from './shortcuts';

  export let onclose: () => void;

  let dialog: HTMLDivElement | undefined;
  let closeButton: HTMLButtonElement | undefined;

  onMount(() => {
    closeButton?.focus();
  });
</script>

<div class="modal-backdrop" role="presentation" on:click|self={() => onclose()}>
  <div
    bind:this={dialog}
    class="modal-card shortcut-sheet"
    role="dialog"
    aria-modal="true"
    aria-labelledby="shortcut-sheet-title"
    data-testid="shortcut-sheet"
    tabindex="-1"
    on:keydown={(event) => trapModalKeydown(event, dialog, onclose)}
  >
    <header>
      <div>
        <small>Keyboard</small>
        <h1 id="shortcut-sheet-title">Keyboard shortcuts</h1>
      </div>
      <button bind:this={closeButton} class="icon-button" type="button" aria-label="Close keyboard shortcuts" on:click={() => onclose()}>×</button>
    </header>

    <div class="shortcut-body">
      {#each shortcutGroups as group}
        <section class="shortcut-group">
          <p class="section-label">{group}</p>
          <div class="shortcut-grid">
            {#each shortcutsInGroup(group) as definition (definition.action + definition.title)}
              <div class="shortcut-row">
                <span class="shortcut-name">
                  <strong>{definition.title}</strong>
                  <small>{definition.description}</small>
                </span>
                <span class="shortcut-keys">
                  {#each visibleBindings(definition) as binding, position}
                    <span class="shortcut-chord">
                      {#each shortcutChips(binding) as chip, chipPosition}
                        {#if chipPosition > 0}<i aria-hidden="true">+</i>{/if}<kbd>{chip}</kbd>
                      {/each}
                    </span>
                    {#if position < visibleBindings(definition).length - 1}<i class="shortcut-or" aria-hidden="true">or</i>{/if}
                  {/each}
                </span>
              </div>
            {/each}
          </div>
        </section>
      {/each}
    </div>

    <footer class="shortcut-footer">
      <span>Escape backs out of whatever is open — a dialog first, then the reader, then the search.</span>
    </footer>
  </div>
</div>
