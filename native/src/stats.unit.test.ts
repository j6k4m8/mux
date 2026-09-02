import assert from 'node:assert/strict';
import { test } from 'vitest';

import {
  MAX_STATS_DAYS,
  approximateTextWidth,
  areaPath,
  axisGridlines,
  axisTicks,
  dayFullLabel,
  dayIsoDate,
  dayMonthLabel,
  dayShortLabel,
  densifyDays,
  formatCount,
  heatmapLayout,
  heatmapLevel,
  linePath,
  niceAxisMax,
  plotBox,
  rankedAddresses,
  seriesPoints,
  statsPeriods,
  statsSummary,
  weekdayOfDay,
  xAxisLabels
} from './stats';
import type { DayVolume, MailStats } from './stats';

/// 2026-09-02 as a day index, so the fixtures read as a real calendar.
const SEP_2_2026 = Math.floor(Date.UTC(2026, 8, 2) / 86_400_000);

function volumes(entries: Array<[number, number, number]>): DayVolume[] {
  return entries.map(([day, received, sent]) => ({ day, received, sent }));
}

test('day indexes read back as the calendar the native side bucketed them in', () => {
  assert.equal(dayIsoDate(SEP_2_2026), '2026-09-02');
  assert.equal(dayShortLabel(SEP_2_2026), 'Sep 2');
  assert.equal(dayMonthLabel(SEP_2_2026), 'Sep 2026');
  assert.equal(dayFullLabel(SEP_2_2026), 'Wed, Sep 2, 2026');
  assert.equal(dayIsoDate(0), '1970-01-01');
  assert.throws(() => dayIsoDate(1.5), /whole number of days/u);
});

test('the epoch was a Thursday, so weekday rows line up with real weeks', () => {
  assert.equal(weekdayOfDay(0), 4);
  assert.deepEqual([0, 1, 2, 3, 4, 5, 6].map((offset) => weekdayOfDay(SEP_2_2026 + offset)), [3, 4, 5, 6, 0, 1, 2]);
  assert.equal(weekdayOfDay(-1), 3);
});

test('densifying fills the silent days and keeps the reported ones', () => {
  const dense = densifyDays(SEP_2_2026 - 3, SEP_2_2026, volumes([[SEP_2_2026 - 3, 2, 1], [SEP_2_2026, 5, 0]]));
  assert.deepEqual(dense, volumes([
    [SEP_2_2026 - 3, 2, 1],
    [SEP_2_2026 - 2, 0, 0],
    [SEP_2_2026 - 1, 0, 0],
    [SEP_2_2026, 5, 0]
  ]));
});

test('an empty or impossible window densifies to nothing rather than breaking', () => {
  assert.deepEqual(densifyDays(SEP_2_2026, SEP_2_2026 - 1, []), []);
  assert.deepEqual(densifyDays(SEP_2_2026, SEP_2_2026, []), volumes([[SEP_2_2026, 0, 0]]));
  assert.throws(() => densifyDays(0, MAX_STATS_DAYS + 1, []), /at most/u);
});

test('the axis rounds up to a readable step and never scales by zero', () => {
  assert.equal(niceAxisMax(0), 1);
  assert.equal(niceAxisMax(-4), 1);
  assert.equal(niceAxisMax(1), 1);
  assert.equal(niceAxisMax(3), 3);
  assert.equal(niceAxisMax(25), 30);
  assert.equal(niceAxisMax(32), 40);
  assert.equal(niceAxisMax(7), 10);
  assert.equal(niceAxisMax(41), 50);
  assert.equal(niceAxisMax(120), 200);
});

test('gridlines are whole message counts', () => {
  assert.deepEqual(axisTicks(1), [0, 1]);
  assert.deepEqual(axisTicks(2), [0, 1, 2]);
  assert.deepEqual(axisTicks(5), [0, 1, 2, 3, 4, 5]);
  assert.deepEqual(axisTicks(10), [0, 2, 4, 6, 8, 10]);
  assert.deepEqual(axisTicks(0), [0, 1]);
  assert.deepEqual(axisTicks(3), [0, 1, 2, 3]);
  assert.deepEqual(axisTicks(30), [0, 6, 12, 18, 24, 30]);
  for (const max of [1, 2, 3, 4, 5, 10, 20, 30, 40, 50, 100, 200, 300, 400, 500]) {
    assert.ok(axisTicks(max).every(Number.isInteger), `ticks for ${max} are whole`);
    assert.equal(axisTicks(max).at(-1), max);
  }
});

test('gridlines sit on the plot, baseline at the bottom and top at the top', () => {
  const box = plotBox();
  const lines = axisGridlines(10, box);
  assert.deepEqual(lines.map((line) => line.value), [0, 2, 4, 6, 8, 10]);
  assert.equal(lines[0].y, box.bottom);
  assert.equal(lines.at(-1)?.y, box.top);
  // An empty mailbox still has a baseline and a top, one message apart.
  assert.deepEqual(axisGridlines(0, box).map((line) => line.y), [box.bottom, box.top]);
});

test('series points span the plot and clamp to the axis top', () => {
  const box = plotBox();
  const days = volumes([[SEP_2_2026 - 2, 0, 3], [SEP_2_2026 - 1, 5, 0], [SEP_2_2026, 10, 1]]);
  const received = seriesPoints(days, 'received', 10, box);
  assert.equal(received[0].x, box.left);
  assert.equal(received[2].x, box.right);
  assert.equal(received[0].y, box.bottom);
  assert.equal(received[2].y, box.top);
  assert.equal(received[1].y, box.bottom - box.height / 2);
  assert.deepEqual(seriesPoints(days, 'sent', 10, box).map((point) => point.value), [3, 0, 1]);
  // An axis smaller than the data would otherwise draw above the plot.
  assert.equal(seriesPoints(days, 'received', 1, box)[2].y, box.top);
});

test('a single day sits in the middle instead of dividing by an empty span', () => {
  const box = plotBox();
  const [only] = seriesPoints(volumes([[SEP_2_2026, 4, 0]]), 'received', 4, box);
  assert.equal(only.x, box.left + box.width / 2);
  assert.equal(only.y, box.top);
});

test('an empty series produces no path at all', () => {
  assert.equal(linePath([]), '');
  assert.equal(areaPath([]), '');
  assert.deepEqual(seriesPoints([], 'received', 1), []);
  assert.deepEqual(xAxisLabels([]), []);
});

test('paths move once and then line, and areas close on the baseline', () => {
  const box = plotBox();
  const points = seriesPoints(volumes([[SEP_2_2026 - 1, 0, 0], [SEP_2_2026, 2, 0]]), 'received', 2, box);
  assert.equal(linePath(points), `M${box.left} ${box.bottom} L${box.right} ${box.top}`);
  assert.equal(
    areaPath(points, box),
    `M${box.left} ${box.bottom} L${box.right} ${box.top} L${box.right} ${box.bottom} L${box.left} ${box.bottom} Z`
  );
});

test('x labels thin out to at most five and always keep both ends', () => {
  const dense = densifyDays(SEP_2_2026 - 364, SEP_2_2026, []);
  const labels = xAxisLabels(seriesPoints(dense, 'received', 1), 5);
  assert.equal(labels.length, 5);
  assert.equal(labels[0].day, SEP_2_2026 - 364);
  assert.equal(labels.at(-1)?.day, SEP_2_2026);
  assert.deepEqual(labels, [...labels].sort((left, right) => left.day - right.day));
  // Centred end labels would hang off the viewBox, so they anchor inwards.
  assert.deepEqual(labels.map((label) => label.anchor), ['start', 'middle', 'middle', 'middle', 'end']);
  // Over four months the day of the month stops being the useful part.
  assert.equal(labels[0].text, 'Sep 2025');
  const short = xAxisLabels(seriesPoints(densifyDays(SEP_2_2026 - 6, SEP_2_2026, []), 'received', 1), 5);
  assert.equal(short[0].text, 'Aug 27');
});

test('heatmap levels give zero its own step and quarter the busiest day', () => {
  assert.equal(heatmapLevel(0, 20), 0);
  assert.equal(heatmapLevel(1, 20), 1);
  assert.equal(heatmapLevel(5, 20), 1);
  assert.equal(heatmapLevel(6, 20), 2);
  assert.equal(heatmapLevel(11, 20), 3);
  assert.equal(heatmapLevel(16, 20), 4);
  assert.equal(heatmapLevel(20, 20), 4);
  // One message in the whole period is still that period's maximum.
  assert.equal(heatmapLevel(1, 1), 4);
  assert.equal(heatmapLevel(3, 0), 1);
});

test('the heatmap lays weeks out as columns of seven, oldest first', () => {
  // A Thursday start, so the first column carries four leading blanks.
  const start = SEP_2_2026 + 1;
  assert.equal(weekdayOfDay(start), 4);
  const dense = densifyDays(start, start + 13, volumes([[start, 3, 1], [start + 3, 1, 9]]));
  const layout = heatmapLayout(dense, 'received');
  const pitch = layout.cellSize + 3;

  assert.equal(layout.cells.length, 14);
  assert.equal(layout.columns, 3);
  assert.equal(layout.maxCount, 3);
  const [first] = layout.cells;
  assert.equal(first.level, 4);
  // A Thursday start sits on row four, and the first column holds only the
  // three days that are actually in the window.
  assert.equal(layout.cells.filter((cell) => cell.x === first.x).length, 3);
  assert.equal(layout.cells[2].x, first.x);
  assert.equal(layout.cells[2].y, first.y + 2 * pitch);
  // The Sunday after opens the next column, back at the top row.
  assert.equal(layout.cells[3].x, first.x + pitch);
  assert.equal(layout.cells[3].y, first.y - 4 * pitch);
  assert.equal(layout.cells.filter((cell) => cell.x === first.x + pitch).length, 7);
  assert.ok(layout.width > layout.cells.at(-1)!.x);
  assert.deepEqual(layout.weekdayLabels.map((label) => label.text), ['Mon', 'Wed', 'Fri']);
});

test('the heatmap reads the series it was asked for', () => {
  const dense = densifyDays(SEP_2_2026 - 1, SEP_2_2026, volumes([[SEP_2_2026 - 1, 0, 4], [SEP_2_2026, 9, 0]]));
  assert.deepEqual(heatmapLayout(dense, 'received').cells.map((cell) => cell.count), [0, 9]);
  assert.deepEqual(heatmapLayout(dense, 'sent').cells.map((cell) => cell.count), [4, 0]);
  assert.equal(heatmapLayout(dense, 'sent').maxCount, 4);
});

test('an empty heatmap still has a grid to draw', () => {
  const layout = heatmapLayout([], 'received');
  assert.deepEqual(layout.cells, []);
  assert.deepEqual(layout.monthLabels, []);
  assert.equal(layout.columns, 0);
  assert.equal(layout.maxCount, 0);
  assert.ok(layout.height > 0);
  assert.equal(layout.weekdayLabels.length, 3);
});

test('month labels mark each month once, carry the year, and never collide', () => {
  const year = heatmapLayout(densifyDays(SEP_2_2026 - 364, SEP_2_2026, []), 'received');
  const texts = year.monthLabels.map((label) => label.text);
  assert.ok(texts.length >= 12 && texts.length <= 14, `a year of labels, got ${texts.length}`);
  // A 365-day window covers the same month name twice, so only the year tells
  // the two apart.
  assert.equal(texts[0], 'Aug 2025');
  assert.ok(texts.includes('Jan 2026'));
  assert.deepEqual(texts, [...new Set(texts)]);
  const fiveYears = heatmapLayout(densifyDays(SEP_2_2026 - (MAX_STATS_DAYS - 1), SEP_2_2026, []), 'received');
  for (const layout of [year, fiveYears]) {
    for (let index = 1; index < layout.monthLabels.length; index += 1) {
      const [previous, label] = [layout.monthLabels[index - 1], layout.monthLabels[index]];
      // A label carrying a year is twice as wide as a bare month, so the gap
      // that matters is the width of the label before it.
      assert.ok(label.x >= previous.x + approximateTextWidth(previous.text), `${previous.text} then ${label.text}`);
      assert.notEqual(label.text, previous.text);
    }
  }
  // Five years of months repeat by name; the year appears once per year to say
  // which pass through the calendar this is.
  const dated = fiveYears.monthLabels.filter((label) => /\d{4}/u.test(label.text));
  assert.ok(dated.length >= 5 && dated.length <= 6, `one dated label a year, got ${dated.length}`);
});

test('ranked rows are sized against the leader, not the total', () => {
  const ranked = rankedAddresses([
    { address: 'loud@example.test', name: 'Loud Person', count: 20 },
    { address: 'quiet@example.test', name: '   ', count: 5 }
  ]);
  assert.deepEqual(ranked.map((row) => row.label), ['Loud Person', 'quiet@example.test']);
  assert.deepEqual(ranked.map((row) => row.share), [1, 0.25]);
});

test('ranking nothing, or nothing but zeros, never divides by zero', () => {
  assert.deepEqual(rankedAddresses([]), []);
  const zeroed = rankedAddresses([{ address: 'a@example.test', name: '', count: 0 }]);
  assert.deepEqual(zeroed.map((row) => row.share), [0]);
  assert.equal(zeroed[0].label, 'a@example.test');
});

test('the summary averages over the window, not over the days with mail', () => {
  const stats: MailStats = {
    startDay: SEP_2_2026 - 9,
    endDay: SEP_2_2026,
    days: volumes([[SEP_2_2026, 20, 5]]),
    topSenders: [],
    topRecipients: [],
    receivedTotal: 20,
    sentTotal: 5,
    truncated: false
  };
  assert.deepEqual(statsSummary(stats), { received: 20, sent: 5, dayCount: 10, dailyAverage: 2.5 });
  const empty = statsSummary({ ...stats, days: [], receivedTotal: 0, sentTotal: 0, endDay: stats.startDay });
  assert.deepEqual(empty, { received: 0, sent: 0, dayCount: 1, dailyAverage: 0 });
});

test('counts are grouped the same way on every machine', () => {
  assert.equal(formatCount(0), '0');
  assert.equal(formatCount(999), '999');
  assert.equal(formatCount(1000), '1,000');
  assert.equal(formatCount(1234567), '1,234,567');
  assert.equal(formatCount(Number.NaN), '0');
});

test('the period row offers a bounded window and one open-ended one', () => {
  assert.deepEqual(statsPeriods.map((period) => period.id), ['30d', '90d', '1y', 'all']);
  assert.equal(statsPeriods.filter((period) => period.days === null).length, 1);
  for (const period of statsPeriods) {
    if (period.days !== null) assert.ok(period.days >= 1 && period.days <= MAX_STATS_DAYS, period.id);
  }
});
