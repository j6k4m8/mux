<script lang="ts">
  export let name:
    | 'settings'
    | 'search'
    | 'compose'
    | 'inbox'
    | 'star'
    | 'sent'
    | 'drafts'
    | 'archive'
    | 'trash'
    | 'allMail'
    | 'clock'
    | 'calendar'
    | 'mail'
    | 'reply'
    | 'replyAll'
    | 'forward'
    | 'unread'
    | 'newest'
    | 'menu'
    | 'command'
    | 'moon'
    | 'sun'
    | 'chevron'
    | 'sync'
    | 'stats';
  export let size = 18;
  export let filled = false;
</script>

<svg
  viewBox="0 0 24 24"
  width={size}
  height={size}
  fill={filled ? 'currentColor' : 'none'}
  stroke="currentColor"
  stroke-width="1.75"
  stroke-linecap="round"
  stroke-linejoin="round"
  aria-hidden="true"
  focusable="false"
>
  {#if name === 'settings'}
    <circle cx="12" cy="12" r="3.2" />
    <path d="M19.4 15a1.6 1.6 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.6 1.6 0 0 0-1.8-.3 1.6 1.6 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.6 1.6 0 0 0-1-1.5 1.6 1.6 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.6 1.6 0 0 0 .3-1.8 1.6 1.6 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.6 1.6 0 0 0 1.5-1 1.6 1.6 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.6 1.6 0 0 0 1.8.3H9a1.6 1.6 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.6 1.6 0 0 0 1 1.5 1.6 1.6 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.6 1.6 0 0 0-.3 1.8V9a1.6 1.6 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.6 1.6 0 0 0-1.5 1Z" />
  {:else if name === 'search'}
    <path d="m21 21-4.35-4.35" />
    <circle cx="11.5" cy="11.5" r="7.5" />
  {:else if name === 'compose'}
    <path d="M12 20h9M16.5 3.5a2.12 2.12 0 0 1 3 3L8 18l-4 1 1-4L16.5 3.5Z" />
  {:else if name === 'inbox'}
    <path d="M4 4h16v13H4zM4 13h4l2 3h4l2-3h4" />
  {:else if name === 'star'}
    <path d="m12 3 2.7 5.47 6.03.88-4.36 4.25 1.03 6-5.4-2.84L6.6 19.6l1.03-6-4.36-4.25 6.03-.88L12 3Z" />
  {:else if name === 'sent'}
    <path d="m22 2-7 20-4-9-9-4 20-7ZM22 2 11 13" />
  {:else if name === 'drafts'}
    <path d="M6 2h8l4 4v16H6zM14 2v5h5" />
  {:else if name === 'archive'}
    <path d="M4 7h16v14H4zM3 3h18v4H3zM9 12h6" />
  {:else if name === 'trash'}
    <path d="M4 7h16M9 3h6l1 4H8l1-4Zm-3 4 1 14h10l1-14M10 11v6m4-6v6" />
  {:else if name === 'allMail'}
    <rect x="3" y="5" width="18" height="14" rx="2" />
    <path d="m4 7 8 6 8-6" />
  {:else if name === 'clock'}
    <circle cx="12" cy="12" r="9" />
    <path d="M12 7v5l3 2" />
  {:else if name === 'calendar'}
    <rect x="3" y="5" width="18" height="16" rx="2" />
    <path d="M8 3v4m8-4v4M3 10h18" />
  {:else if name === 'mail'}
    <rect x="3" y="5" width="18" height="14" rx="2" />
    <path d="m4 7 8 6 8-6" />
  {:else if name === 'reply'}
    <path d="m9 17-6-5 6-5v3h4c5 0 8 2 8 7-2-3-4-4-8-4H9v4Z" />
  {:else if name === 'replyAll'}
    <path d="m10 17-6-5 6-5v3h4c4.5 0 7 2 7 7-1.8-2.7-4-4-7-4h-4v4Z" />
    <path d="m6.5 17-4-3.3 2.1-1.8" />
  {:else if name === 'forward'}
    <path d="m15 17 6-5-6-5v3h-4c-5 0-8 2-8 7 2-3 4-4 8-4h4v4Z" />
  {:else if name === 'unread'}
    <path d="M12 4v14m-5-5 5 5 5-5" />
  {:else if name === 'newest'}
    <path d="M12 3v13m-5-5 5 5 5-5M5 21h14" />
  {:else if name === 'menu'}
    <path d="M4 7h16M4 12h16M4 17h16" />
  {:else if name === 'command'}
    <path d="M9 7V5a3 3 0 1 0-3 3h12a3 3 0 1 0-3-3v14a3 3 0 1 0 3-3H6a3 3 0 1 0 3 3V7Z" />
  {:else if name === 'moon'}
    <path d="M20 15.5A8.5 8.5 0 0 1 8.5 4 8.5 8.5 0 1 0 20 15.5Z" />
  {:else if name === 'sync'}
    <path d="M21 12a9 9 0 0 1-9 9 9 9 0 0 1-8.06-5" />
    <path d="M3 12a9 9 0 0 1 9-9 9 9 0 0 1 8.06 5" />
    <path d="M21 4v4h-4M3 20v-4h4" />
  {:else if name === 'stats'}
    <path d="M4 20V4" />
    <path d="M4 20h16" />
    <path d="M8 20v-6m4 6V8m4 12v-9" />
  {:else if name === 'sun'}
    <circle cx="12" cy="12" r="4" />
    <path d="M12 2v2m0 16v2M4.93 4.93l1.42 1.42m11.3 11.3 1.42 1.42M2 12h2m16 0h2M4.93 19.07l1.42-1.42m11.3-11.3 1.42-1.42" />
  {:else}
    <path d="m9 18 6-6-6-6" />
  {/if}
</svg>

<style>
  svg {
    display: block;
    flex: 0 0 auto;
  }
</style>
