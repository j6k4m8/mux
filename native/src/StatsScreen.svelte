<script lang="ts">
  import { invoke } from '@tauri-apps/api/core';
  import { onMount, tick } from 'svelte';
  import Icon from './Icon.svelte';
  import {
    CHART_LABEL_PX,
    HEATMAP_MONTH_BASELINE,
    LINE_CHART_HEIGHT,
    LINE_CHART_WIDTH,
    areaPath,
    axisGridlines,
    dayFullLabel,
    densifyDays,
    formatCount,
    heatmapLayout,
    linePath,
    niceAxisMax,
    plotBox,
    rankedAddresses,
    seriesPoints,
    statsPeriods,
    statsSummary,
    xAxisLabels
  } from './stats';
  import type { MailStats, StatsSeries } from './stats';
  import type { AccountSummary } from './types';

  export let close: () => void;
  export let accounts: AccountSummary[] = [];

  /// Ten is enough to see a shape and short enough to read without scrolling.
  const TOP_LIMIT = 10;
  const HEATMAP_LEVELS = [0, 1, 2, 3, 4];
  const seriesChoices: Array<{ id: StatsSeries; label: string }> = [
    { id: 'received', label: 'Received' },
    { id: 'sent', label: 'Sent' }
  ];

  let periodId = statsPeriods[0].id;
  let accountId: string | null = null;
  /// One heatmap with a toggle rather than two stacked grids. Two grids double
  /// the tallest thing on the screen and still cannot be compared square by
  /// square from a foot apart; toggling keeps every cell where it was, so the
  /// switch reads as the same picture changing rather than a second picture.
  let heatmapSeries: StatsSeries = 'received';
  let stats: MailStats | null = null;
  let loading = true;
  let loadError = '';
  /// Period buttons are faster than the store. Only the newest request may
  /// write, so an earlier answer cannot land on top of a later one.
  let requestToken = 0;
  let heatmapScroller: HTMLDivElement | undefined;

  /// A plain lookup rather than only a reactive one: `load` runs the moment a
  /// period button is clicked, before reactive statements have flushed, so it
  /// has to resolve the period it was just handed instead of the last one.
  function periodFor(id: string) {
    return statsPeriods.find((candidate) => candidate.id === id) ?? statsPeriods[0];
  }

  $: dense = stats ? densifyDays(stats.startDay, stats.endDay, stats.days) : [];
  $: box = plotBox();
  $: axisMax = niceAxisMax(dense.reduce((highest, day) => Math.max(highest, day.received, day.sent), 0));
  $: gridlines = axisGridlines(axisMax, box);
  $: receivedPoints = seriesPoints(dense, 'received', axisMax, box);
  $: sentPoints = seriesPoints(dense, 'sent', axisMax, box);
  $: dayLabels = xAxisLabels(receivedPoints);
  $: heatmap = heatmapLayout(dense, heatmapSeries);
  $: summary = stats ? statsSummary(stats) : null;
  $: senders = stats ? rankedAddresses(stats.topSenders) : [];
  $: recipients = stats ? rankedAddresses(stats.topRecipients) : [];
  $: hasMail = Boolean(summary && summary.received + summary.sent > 0);
  $: windowLabel = !stats
    ? ''
    : stats.startDay === stats.endDay
      ? dayFullLabel(stats.endDay)
      : `${dayFullLabel(stats.startDay)} to ${dayFullLabel(stats.endDay)}`;
  $: rankings = [
    { id: 'senders', title: 'Top senders', caption: 'Who writes to you', rows: senders },
    { id: 'recipients', title: 'Top recipients', caption: 'Who you write to', rows: recipients }
  ];
  $: seriesColor = heatmapSeries === 'sent' ? 'var(--success)' : 'var(--accent)';

  onMount(() => {
    void load();
  });

  async function load(): Promise<void> {
    const token = requestToken + 1;
    requestToken = token;
    loading = true;
    loadError = '';
    try {
      const loaded = await invoke<MailStats>('mailbox_stats', {
        input: {
          days: periodFor(periodId).days,
          accountId,
          /// The store buckets days in the reader's zone, and only the browser
          /// knows what that is. Negated because `getTimezoneOffset` counts the
          /// other way round.
          timezoneOffsetMinutes: -new Date().getTimezoneOffset(),
          topLimit: TOP_LIMIT
        }
      });
      if (token !== requestToken) return;
      stats = loaded;
      loading = false;
      await tick();
      /// The newest week is the one worth seeing first, and it is at the far
      /// right of a grid that can be years wide.
      if (heatmapScroller) heatmapScroller.scrollLeft = heatmapScroller.scrollWidth;
    } catch (error) {
      if (token !== requestToken) return;
      stats = null;
      loadError = error instanceof Error ? error.message : String(error);
      loading = false;
    }
  }

  function selectPeriod(next: string): void {
    if (next === periodId) return;
    periodId = next;
    void load();
  }

  function selectAccount(event: Event): void {
    const chosen = (event.currentTarget as HTMLSelectElement).value;
    accountId = chosen === '' ? null : chosen;
    void load();
  }

  function cellTitle(day: number, count: number): string {
    const noun = count === 1 ? 'message' : 'messages';
    return `${formatCount(count)} ${heatmapSeries} ${noun} — ${dayFullLabel(day)}`;
  }
</script>

<section class="stats-screen" data-testid="stats-screen">
  <header class="stats-header">
    <button class="stats-back" type="button" title="Back to mail (Esc)" data-testid="stats-close" on:click={() => close()}>
      <span aria-hidden="true"><Icon name="chevron" size={15} /></span>Back to mail
    </button>
    <h1>Statistics</h1>

    <div class="stats-controls">
      <div class="stats-periods" role="group" aria-label="Reporting period">
        {#each statsPeriods as choice (choice.id)}
          <button
            class="stats-period"
            class:is-active={choice.id === periodId}
            type="button"
            aria-pressed={choice.id === periodId}
            data-testid={`stats-period-${choice.id}`}
            on:click={() => selectPeriod(choice.id)}
          >{choice.label}</button>
        {/each}
      </div>

      {#if accounts.length > 1}
        <label class="stats-account">
          <span>Account</span>
          <select data-testid="stats-account" value={accountId ?? ''} on:change={selectAccount}>
            <option value="">All accounts</option>
            {#each accounts as account (account.id)}
              <option value={account.id}>{account.name}</option>
            {/each}
          </select>
        </label>
      {/if}
    </div>
  </header>

  {#if loadError}
    <p class="stats-notice has-error" role="alert" data-testid="stats-error">{loadError}</p>
  {:else if loading && !stats}
    <p class="stats-notice" role="status" data-testid="stats-loading">Counting your mail…</p>
  {:else if stats}
    <div class="stats-body" class:is-stale={loading}>
      <p class="stats-window" data-testid="stats-window">
        {windowLabel}{#if stats.truncated} · older mail than this window reaches is not counted{/if}
      </p>

      {#if summary}
        <ul class="stats-tiles">
          <li><span class="stats-tile-value">{formatCount(summary.received)}</span><span class="stats-tile-label">Received</span></li>
          <li><span class="stats-tile-value">{formatCount(summary.sent)}</span><span class="stats-tile-label">Sent</span></li>
          <li><span class="stats-tile-value">{formatCount(summary.dayCount)}</span><span class="stats-tile-label">Days</span></li>
          <li><span class="stats-tile-value">{summary.dailyAverage}</span><span class="stats-tile-label">Messages a day</span></li>
        </ul>
      {/if}

      {#if !hasMail}
        <p class="stats-notice" data-testid="stats-empty">
          No mail in this period yet. Once messages arrive, the charts fill in from the left.
        </p>
      {/if}

      <section class="stats-card">
        <header>
          <h2>Received and sent, by day</h2>
          <ul class="stats-legend">
            <li><span class="stats-swatch is-received" aria-hidden="true"></span>Received</li>
            <li><span class="stats-swatch is-sent" aria-hidden="true"></span>Sent</li>
          </ul>
        </header>
        <!-- One viewBox scaled uniformly by CSS: stretching it to the card
             would stretch the axis text with it. -->
        <svg
          class="stats-line-chart"
          viewBox={`0 0 ${LINE_CHART_WIDTH} ${LINE_CHART_HEIGHT}`}
          role="img"
          data-testid="stats-volume-chart"
          aria-label={`Received and sent messages a day, ${windowLabel}. ${formatCount(summary?.received ?? 0)} received and ${formatCount(summary?.sent ?? 0)} sent in total.`}
        >
          {#each gridlines as line (line.value)}
            <line class="stats-gridline" x1={box.left} x2={box.right} y1={line.y} y2={line.y} />
            <text class="stats-axis-text" font-size={CHART_LABEL_PX} x={box.left - 6} y={line.y + 5} text-anchor="end">{formatCount(line.value)}</text>
          {/each}
          {#if receivedPoints.length}
            <path class="stats-area is-received" d={areaPath(receivedPoints, box)} />
            <path class="stats-area is-sent" d={areaPath(sentPoints, box)} />
            <path class="stats-line is-received" d={linePath(receivedPoints)} />
            <path class="stats-line is-sent" d={linePath(sentPoints)} />
          {/if}
          {#each dayLabels as label (label.day)}
            <text class="stats-axis-text" font-size={CHART_LABEL_PX} x={label.x} y={LINE_CHART_HEIGHT - 8} text-anchor={label.anchor}>{label.text}</text>
          {/each}
        </svg>
      </section>

      <section class="stats-card">
        <header>
          <h2>Daily rhythm</h2>
          <div class="stats-series-toggle" role="group" aria-label="Heatmap series">
            {#each seriesChoices as choice (choice.id)}
              <button
                class="stats-period"
                class:is-active={choice.id === heatmapSeries}
                type="button"
                aria-pressed={choice.id === heatmapSeries}
                data-testid={`stats-heatmap-${choice.id}`}
                on:click={() => (heatmapSeries = choice.id)}
              >{choice.label}</button>
            {/each}
          </div>
        </header>
        <!-- Drawn at one unit per pixel, then scaled with the text-size setting
             as a whole, cells and labels together. -->
        <div class="stats-heatmap-scroller" bind:this={heatmapScroller}>
          <svg
            class="stats-heatmap"
            width={heatmap.width}
            height={heatmap.height}
            style:width={`calc(${heatmap.width}px * var(--ui-scale))`}
            style:height={`calc(${heatmap.height}px * var(--ui-scale))`}
            viewBox={`0 0 ${heatmap.width} ${heatmap.height}`}
            role="img"
            data-testid="stats-heatmap"
            data-series={heatmapSeries}
            style:--stats-series={seriesColor}
            aria-label={`${heatmapSeries === 'sent' ? 'Sent' : 'Received'} messages a day, one square per day, oldest on the left. Busiest day: ${formatCount(heatmap.maxCount)}.`}
          >
            {#each heatmap.monthLabels as label (label.x)}
              <text class="stats-axis-text" font-size={CHART_LABEL_PX} x={label.x} y={HEATMAP_MONTH_BASELINE}>{label.text}</text>
            {/each}
            {#each heatmap.weekdayLabels as label (label.text)}
              <text class="stats-axis-text" font-size={CHART_LABEL_PX} x={0} y={label.y}>{label.text}</text>
            {/each}
            {#each heatmap.cells as cell (cell.day)}
              <rect
                class="stats-cell"
                data-level={cell.level}
                data-day={cell.day}
                x={cell.x}
                y={cell.y}
                width={heatmap.cellSize}
                height={heatmap.cellSize}
                rx="2"
              ><title>{cellTitle(cell.day, cell.count)}</title></rect>
            {/each}
          </svg>
        </div>
        <footer class="stats-scale" style:--stats-series={seriesColor}>
          <span>Quieter</span>
          {#each HEATMAP_LEVELS as level (level)}
            <i class="stats-cell-key" data-level={level} aria-hidden="true"></i>
          {/each}
          <span>Busier</span>
        </footer>
      </section>

      <div class="stats-rankings">
        {#each rankings as list (list.id)}
          <section class="stats-card">
            <header>
              <h2>{list.title}</h2>
              <p>{list.caption}</p>
            </header>
            {#if list.rows.length}
              <ol class="stats-rank" data-testid={`stats-top-${list.id}`}>
                {#each list.rows as row, position (row.address)}
                  <li>
                    <span class="stats-rank-position">{position + 1}</span>
                    <span class="stats-rank-name" title={row.address}>{row.label}</span>
                    <span class="stats-rank-bar" aria-hidden="true"><i style:width={`${row.share * 100}%`}></i></span>
                    <span class="stats-rank-count">{formatCount(row.count)}</span>
                  </li>
                {/each}
              </ol>
            {:else}
              <p class="stats-notice" data-testid={`stats-top-${list.id}-empty`}>Nothing to rank in this period.</p>
            {/if}
          </section>
        {/each}
      </div>
    </div>
  {/if}
</section>

<style>
  /* Scoped to the component on purpose: this screen carries its own styles so
     it can land without touching the shared stylesheet. Every color is a
     token, so light and dark both come out right. */
  .stats-screen {
    display: flex;
    flex-direction: column;
    gap: var(--gap-lg);
    height: 100%;
    overflow-y: auto;
    padding: var(--gap-xl) 1.6em calc(var(--gap-xl) + var(--gap-sm));
    background: var(--surface);
    color: var(--text);
  }

  .stats-header {
    display: flex;
    flex-direction: column;
    gap: .75em;
  }

  /* Type sizes are the app's own steps, so a screen with its own stylesheet
     still reads as the same application and follows the same setting. */
  .stats-header h1 {
    margin: 0;
    font-size: var(--text-4);
    font-weight: 700;
    letter-spacing: -0.03em;
  }

  .stats-back {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    align-self: flex-start;
    padding: .25em .55em .25em .15em;
    border: 0;
    border-radius: 7px;
    background: none;
    color: var(--text-faint);
    font: inherit;
    font-size: var(--text-0);
    cursor: pointer;
  }

  .stats-back span {
    display: inline-flex;
    transform: rotate(180deg);
  }

  .stats-back:hover {
    background: var(--surface-hover);
    color: var(--text);
  }

  .stats-controls {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: space-between;
    gap: .75em;
  }

  .stats-periods,
  .stats-series-toggle {
    display: inline-flex;
    gap: 2px;
    padding: 2px;
    border: 1px solid var(--border);
    border-radius: 9px;
    background: var(--surface-muted);
  }

  .stats-period {
    padding: .3em .75em;
    border: 0;
    border-radius: 7px;
    background: none;
    color: var(--text-soft);
    font: inherit;
    font-size: var(--text-1);
    font-weight: 620;
    cursor: pointer;
  }

  .stats-period:hover {
    color: var(--text);
  }

  .stats-period.is-active {
    background: var(--surface-raised);
    box-shadow: var(--shadow-sm);
    color: var(--text);
    font-weight: 700;
  }

  .stats-account {
    display: inline-flex;
    align-items: center;
    gap: .4em;
    color: var(--text-faint);
    font-size: var(--text-1);
  }

  .stats-account select {
    padding: .3em .5em;
    border: 1px solid var(--border-strong);
    border-radius: 8px;
    background: var(--surface-raised);
    color: var(--text);
    font: inherit;
    font-size: var(--text-1);
  }

  .stats-body {
    display: flex;
    flex-direction: column;
    gap: var(--gap-lg);
    transition: opacity 120ms ease;
  }

  /* A period change keeps the old numbers on screen, dimmed, rather than
     flashing the whole screen back to a loading line. */
  .stats-body.is-stale {
    opacity: 0.55;
  }

  .stats-window {
    margin: 0;
    color: var(--text-faint);
    font-size: var(--text-0);
  }

  .stats-notice {
    margin: 0;
    padding: .75em .9em;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: var(--surface-muted);
    color: var(--text-soft);
    font-size: var(--text-1);
    line-height: 1.55;
  }

  .stats-notice.has-error {
    border-color: var(--danger);
    color: var(--danger);
  }

  .stats-tiles {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(9em, 1fr));
    gap: var(--gap-sm);
    margin: 0;
    padding: 0;
    list-style: none;
  }

  .stats-tiles li {
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: var(--gap-md) .9em;
    border: 1px solid var(--border);
    border-radius: 12px;
    background: var(--surface-raised);
  }

  .stats-tile-value {
    font-size: var(--text-4);
    font-weight: 700;
    font-variant-numeric: tabular-nums;
    letter-spacing: -0.02em;
  }

  .stats-tile-label {
    color: var(--text-faint);
    font-size: var(--text-0);
    font-weight: 750;
    letter-spacing: 0.12em;
    text-transform: uppercase;
  }

  .stats-card {
    display: flex;
    flex-direction: column;
    gap: var(--gap-md);
    padding: var(--gap-lg) 1.1em;
    border: 1px solid var(--border);
    border-radius: 14px;
    background: var(--surface-raised);
    box-shadow: var(--shadow-sm);
  }

  .stats-card > header {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    justify-content: space-between;
    gap: .5em;
  }

  .stats-card h2 {
    margin: 0;
    font-size: var(--text-2);
    font-weight: 700;
    letter-spacing: -0.01em;
  }

  .stats-card > header p {
    margin: 0;
    color: var(--text-faint);
    font-size: var(--text-0);
  }

  .stats-legend {
    display: flex;
    gap: .9em;
    margin: 0;
    padding: 0;
    color: var(--text-soft);
    font-size: var(--text-0);
    list-style: none;
  }

  .stats-legend li {
    display: inline-flex;
    align-items: center;
    gap: .35em;
  }

  .stats-swatch {
    width: .7em;
    height: .7em;
    border-radius: 3px;
  }

  .stats-swatch.is-received {
    background: var(--accent);
  }

  .stats-swatch.is-sent {
    background: var(--success);
  }

  .stats-line-chart {
    display: block;
    width: 100%;
    height: auto;
  }

  .stats-gridline {
    stroke: var(--border);
    stroke-width: 1;
    vector-effect: non-scaling-stroke;
  }

  /* Sized by the chart, not here: a label's size is chart geometry, drawn in the
     chart's own units beside the gutters that make room for it. */
  .stats-axis-text {
    fill: var(--text-faint);
  }

  .stats-line {
    fill: none;
    stroke-width: 1.75;
    stroke-linejoin: round;
    stroke-linecap: round;
    vector-effect: non-scaling-stroke;
  }

  .stats-line.is-received {
    stroke: var(--accent);
  }

  .stats-line.is-sent {
    stroke: var(--success);
  }

  .stats-area {
    stroke: none;
    opacity: 0.16;
  }

  .stats-area.is-received {
    fill: var(--accent);
  }

  .stats-area.is-sent {
    fill: var(--success);
  }

  .stats-heatmap-scroller {
    overflow-x: auto;
    padding-bottom: 4px;
  }

  .stats-heatmap {
    display: block;
  }

  /* Mixed against the surface rather than set as an opacity, so a cell keeps
     its edge on both themes. The hue is whatever the series is: --accent is
     user-configurable, and nothing here assumes what it currently is. */
  .stats-cell,
  .stats-cell-key {
    fill: var(--surface-muted);
    background: var(--surface-muted);
    stroke: var(--border);
  }

  .stats-cell[data-level='1'],
  .stats-cell-key[data-level='1'] {
    fill: color-mix(in srgb, var(--stats-series) 26%, var(--surface-muted));
    background: color-mix(in srgb, var(--stats-series) 26%, var(--surface-muted));
    stroke: none;
  }

  .stats-cell[data-level='2'],
  .stats-cell-key[data-level='2'] {
    fill: color-mix(in srgb, var(--stats-series) 48%, var(--surface-muted));
    background: color-mix(in srgb, var(--stats-series) 48%, var(--surface-muted));
    stroke: none;
  }

  .stats-cell[data-level='3'],
  .stats-cell-key[data-level='3'] {
    fill: color-mix(in srgb, var(--stats-series) 72%, var(--surface-muted));
    background: color-mix(in srgb, var(--stats-series) 72%, var(--surface-muted));
    stroke: none;
  }

  .stats-cell[data-level='4'],
  .stats-cell-key[data-level='4'] {
    fill: var(--stats-series);
    background: var(--stats-series);
    stroke: none;
  }

  .stats-scale {
    display: flex;
    align-items: center;
    gap: .25em;
    color: var(--text-faint);
    font-size: var(--text-0);
  }

  .stats-scale span:first-child {
    margin-right: 2px;
  }

  .stats-scale span:last-child {
    margin-left: 2px;
  }

  .stats-cell-key {
    width: .7em;
    height: .7em;
    border: 1px solid var(--border);
    border-radius: 2px;
  }

  .stats-cell-key[data-level='1'],
  .stats-cell-key[data-level='2'],
  .stats-cell-key[data-level='3'],
  .stats-cell-key[data-level='4'] {
    border-color: transparent;
  }

  .stats-rankings {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(calc(280px * var(--ui-scale)), 1fr));
    gap: var(--gap-lg);
  }

  .stats-rank {
    display: flex;
    flex-direction: column;
    gap: var(--gap-sm);
    margin: 0;
    padding: 0;
    list-style: none;
  }

  .stats-rank li {
    display: grid;
    grid-template-columns: 1.2em minmax(0, 1fr) 4.5em auto;
    align-items: center;
    gap: .5em;
    font-size: var(--text-1);
  }

  .stats-rank-position {
    color: var(--text-faint);
    font-variant-numeric: tabular-nums;
    text-align: right;
  }

  .stats-rank-name {
    overflow: hidden;
    color: var(--text);
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .stats-rank-bar {
    height: .4em;
    border-radius: 3px;
    background: var(--surface-muted);
  }

  .stats-rank-bar i {
    display: block;
    height: 100%;
    border-radius: 3px;
    background: var(--accent);
  }

  .stats-rank-count {
    color: var(--text-soft);
    font-variant-numeric: tabular-nums;
  }

  @media (max-width: 720px) {
    .stats-screen {
      padding: var(--gap-lg) .9em var(--gap-xl);
    }

    .stats-rank li {
      grid-template-columns: 1em minmax(0, 1fr) 2.5em auto;
    }
  }
</style>
