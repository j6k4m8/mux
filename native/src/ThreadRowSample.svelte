<script lang="ts">
  /// What the mail list will look like, shown where the choices are made.
  ///
  /// Deliberately built from the mail list's own class names and nothing else:
  /// the typeface and the text size arrive from the document root, the density
  /// variables from the shell this renders inside, and the row rules from the
  /// stylesheet. Styling any of it here would be inventing a second mail list,
  /// and a sample that is drawn differently from the thing it stands for lies.
  import type { Appearance } from './appearance';

  export let appearance: Appearance;
  /// The stripe says which account a row belongs to, so the sample borrows a
  /// real account's colour when the mailbox has one. A fresh install has none,
  /// and the theme's own accent stands in better than a colour invented here.
  export let accountColor: string | null = null;

  /// Made up, and fixed. Reaching into the mailbox would leave the sample blank
  /// on a new install — which is exactly when someone is choosing how it looks —
  /// and would put real correspondence on the settings screen.
  const rows = [
    {
      initials: 'AR',
      participants: 'Alex Rivera',
      time: '9:41 AM',
      subject: 'Quarterly review notes',
      snippet: 'Sending these through early so nobody has to read them in the meeting.',
      unread: true
    },
    {
      initials: 'SO',
      participants: 'Sam Okafor, Dana Lin',
      time: 'Tue',
      subject: 'Re: Studio move',
      snippet: 'Thursday works for me. I can bring the boxes over in the morning.',
      unread: false
    }
  ];

  $: stripe = accountColor || 'var(--accent)';
</script>

<figure class="appearance-sample" data-testid="appearance-sample">
  <div class="appearance-sample-rows" aria-hidden="true">
    {#each rows as row (row.participants)}
      <div class="thread-row" class:is-unread={row.unread} class:is-selected={!row.unread}>
        <span class="thread-accent" style:background={stripe}></span>
        <span class="avatar" style:--avatar-color={stripe}>{row.initials}</span>
        <span class="thread-copy">
          <span class="thread-line"><strong>{row.participants}</strong><time>{row.time}</time></span>
          <span class="subject">{row.subject}</span>
          {#if appearance.listSnippet}<span class="snippet">{row.snippet}</span>{/if}
        </span>
        {#if row.unread}<span class="unread-dot"></span>{/if}
      </div>
    {/each}
  </div>
  <figcaption>Invented mail, drawn the way your list is drawn.</figcaption>
</figure>

<style>
  /* The frame only. Everything inside the frame is the mail list's own styling,
     and scoped here rather than in styles.css because this component owns the
     frame and nothing else needs it. */
  .appearance-sample {
    /* The settings body scrolls, and some of the choices the sample answers to
       sit a long way down it. Pinned, it is still on screen to be watched when
       one of them is clicked. */
    position: sticky;
    top: 0;
    z-index: 1;
    max-width: 520px;
    margin: 0 0 18px;
    /* Opaque, with a little air, so the cards scrolling underneath do not read
       as part of the sample. */
    padding: 10px 0 14px;
    background: var(--bg);
  }
  .appearance-sample-rows {
    overflow: hidden;
    border: 1px solid var(--border);
    border-radius: 12px;
    /* An illustration, not a control: nothing here answers a pointer. */
    pointer-events: none;
  }
  .appearance-sample figcaption {
    margin-top: 8px;
    color: var(--text-faint);
    font-size: 10px;
  }
</style>
