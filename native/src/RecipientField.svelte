<script lang="ts">
  import { createEventDispatcher } from 'svelte';
  import { appendRecipientToken, recipientTokens, serializeRecipientTokens } from './recipientTokens';

  export let label = 'To';
  export let value = '';
  export let disabled = false;
  export let placeholder = 'name@example.com';

  const dispatch = createEventDispatcher<{ change: { value: string } }>();
  let input = '';
  let error = '';
  $: tokens = recipientTokens(value);

  function emit(nextTokens: string[]) {
    value = serializeRecipientTokens(nextTokens);
    error = '';
    dispatch('change', { value });
  }

  function commit() {
    const result = appendRecipientToken(value, input);
    error = result.error;
    if (error) return false;
    value = result.value;
    input = '';
    dispatch('change', { value });
    return true;
  }

  function remove(index: number) {
    if (disabled) return;
    emit(tokens.filter((_, tokenIndex) => tokenIndex !== index));
  }

  function keydown(event: KeyboardEvent) {
    if (disabled) return;
    if (['Enter', ',', ';'].includes(event.key) || (event.key === 'Tab' && input.trim())) {
      const committed = commit();
      if (event.key !== 'Tab' || !committed) event.preventDefault();
      return;
    }
    if (event.key === 'Backspace' && !input && tokens.length) {
      event.preventDefault();
      remove(tokens.length - 1);
    }
  }
</script>

<label class="recipient-field" class:has-error={Boolean(error)}>
  <span>{label}</span>
  <div class="recipient-chipbox" aria-label={`${label} recipients`} aria-invalid={Boolean(error)}>
    {#each tokens as token, index}
      <span class="recipient-chip">{token}<button type="button" aria-label={`Remove ${token}`} disabled={disabled} on:click={() => remove(index)}>×</button></span>
    {/each}
    <input
      bind:value={input}
      disabled={disabled}
      aria-label={`Add ${label} recipient`}
      {placeholder}
      on:keydown={keydown}
      on:blur={() => { if (input.trim()) commit(); }}
      on:input={() => { error = ''; }}
    />
  </div>
  {#if error}<small role="alert">{error}</small>{/if}
</label>
