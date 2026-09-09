/// Every number the Stats screen draws is worked out here, so the component is
/// markup and nothing else. The native side in store/stats.rs hands back day
/// *indexes* — days since the epoch in the reader's own zone — which keeps all
/// of this integer arithmetic: no Date parsing, no locale, no timezone left to
/// get wrong twice.

export type DayVolume = { day: number; received: number; sent: number };

export type AddressCount = { address: string; name: string; count: number };

export type MailStats = {
  startDay: number;
  endDay: number;
  days: DayVolume[];
  topSenders: AddressCount[];
  topRecipients: AddressCount[];
  receivedTotal: number;
  sentTotal: number;
  truncated: boolean;
};

export type StatsSeries = 'received' | 'sent';

export type StatsPeriod = { id: string; label: string; days: number | null };

const DAY_MS = 86_400_000;

/// Matches MAX_STAT_DAYS in store/stats.rs. A wider window than the native side
/// will ever report means the payload is not the one this screen asked for.
export const MAX_STATS_DAYS = 5 * 366;

/// Ordered coarsest-last so the row reads as a zoom-out. `days` is what the
/// native command takes; null asks it for everything it can reach.
export const statsPeriods: StatsPeriod[] = [
  { id: '30d', label: '30 days', days: 30 },
  { id: '90d', label: '90 days', days: 90 },
  { id: '1y', label: '1 year', days: 365 },
  { id: 'all', label: 'All time', days: null }
];

const MONTH_NAMES = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
const WEEKDAY_NAMES = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'];

function assertDay(day: number): void {
  if (!Number.isInteger(day)) throw new Error('A stats day must be a whole number of days since the epoch');
}

/// Day indexes are already shifted into the reader's zone, so the calendar is
/// read back out in UTC. Doing otherwise would apply the offset twice.
function dayParts(day: number): { year: number; month: number; date: number } {
  assertDay(day);
  const instant = new Date(day * DAY_MS);
  return { year: instant.getUTCFullYear(), month: instant.getUTCMonth(), date: instant.getUTCDate() };
}

export function dayIsoDate(day: number): string {
  const { year, month, date } = dayParts(day);
  return `${String(year).padStart(4, '0')}-${String(month + 1).padStart(2, '0')}-${String(date).padStart(2, '0')}`;
}

/// Named months and weekdays rather than `toLocaleDateString`, which would make
/// every label a function of the machine the app happens to be running on.
export function dayShortLabel(day: number): string {
  const { month, date } = dayParts(day);
  return `${MONTH_NAMES[month]} ${date}`;
}

export function dayMonthLabel(day: number): string {
  const { year, month } = dayParts(day);
  return `${MONTH_NAMES[month]} ${year}`;
}

export function dayFullLabel(day: number): string {
  const { year, month, date } = dayParts(day);
  return `${WEEKDAY_NAMES[weekdayOfDay(day)]}, ${MONTH_NAMES[month]} ${date}, ${year}`;
}

/// 0 is Sunday, because that is the row a contribution grid starts on. Epoch
/// day 0 was a Thursday, hence the four.
export function weekdayOfDay(day: number): number {
  assertDay(day);
  return (((day + 4) % 7) + 7) % 7;
}

/// Fills the window's silent days with zeros. The native side only sends days
/// that carried mail, so this is where a sparse answer becomes a chart: a gap
/// in a line or an empty square is information, and it has to be drawn.
export function densifyDays(startDay: number, endDay: number, days: DayVolume[]): DayVolume[] {
  assertDay(startDay);
  assertDay(endDay);
  if (endDay < startDay) return [];
  const span = endDay - startDay + 1;
  if (span > MAX_STATS_DAYS) throw new Error(`A stats window covers at most ${MAX_STATS_DAYS} days`);
  const counted = new Map(days.map((day) => [day.day, day]));
  return Array.from({ length: span }, (_unused, offset) => {
    const day = startDay + offset;
    const found = counted.get(day);
    return { day, received: found?.received ?? 0, sent: found?.sent ?? 0 };
  });
}

export function seriesValue(day: DayVolume, series: StatsSeries): number {
  return series === 'sent' ? day.sent : day.received;
}

/// Rounds the axis up to a round step so the gridlines land on numbers a person
/// would choose. Never returns zero: an empty mailbox still needs a scale to
/// divide by.
export function niceAxisMax(maxValue: number): number {
  if (!Number.isFinite(maxValue) || maxValue <= 0) return 1;
  const magnitude = 10 ** Math.floor(Math.log10(maxValue));
  // 3 and 4 are in the list because without them a mailbox peaking at thirty a
  // day gets a fifty-message axis and spends a third of the plot on nothing.
  for (const step of [1, 2, 3, 4, 5]) {
    if (step * magnitude >= maxValue) return step * magnitude;
  }
  return 10 * magnitude;
}

/// Whole-number gridlines only — these are message counts, and a tick reading
/// "1.25 messages" is a tick that means nothing.
export function axisTicks(axisMax: number): number[] {
  const top = Math.max(1, Math.round(axisMax));
  const divisions = [5, 4, 3, 2].find((candidate) => top % candidate === 0) ?? 1;
  return Array.from({ length: divisions + 1 }, (_unused, index) => (top / divisions) * index);
}

export type ChartPadding = { left: number; right: number; top: number; bottom: number };

export type ChartBox = { left: number; top: number; right: number; bottom: number; width: number; height: number };

/// Chart labels are drawn in the chart's own units, so their size is geometry
/// and lives here beside the gutters that make room for it rather than in the
/// stylesheet: 11pt, the interface's smallest step, in CSS pixels. The heatmap
/// is drawn one unit to the pixel and scaled with the text-size setting as a
/// whole; the line chart is scaled to its card, so its labels are this size at
/// 720px across and grow with the card from there.
export const CHART_LABEL_PX = (11 * 96) / 72;

/// A fixed viewBox scaled by CSS, so the line chart reflows with the window
/// while the geometry stays a pure function of the data. The left gutter fits
/// a five-character count at label size; the bottom fits a line of day labels.
export const LINE_CHART_WIDTH = 720;
export const LINE_CHART_HEIGHT = 220;
const LINE_CHART_PADDING: ChartPadding = { left: 58, right: 10, top: 14, bottom: 32 };

export function plotBox(
  width = LINE_CHART_WIDTH,
  height = LINE_CHART_HEIGHT,
  padding: ChartPadding = LINE_CHART_PADDING
): ChartBox {
  const inner = {
    left: padding.left,
    top: padding.top,
    right: width - padding.right,
    bottom: height - padding.bottom
  };
  const box = { ...inner, width: inner.right - inner.left, height: inner.bottom - inner.top };
  if (box.width <= 0 || box.height <= 0) throw new Error('A chart needs more room than its own padding');
  return box;
}

export type ChartPoint = { day: number; value: number; x: number; y: number };

export type Gridline = { value: number; y: number };

function round(value: number): number {
  return Math.round(value * 100) / 100;
}

/// The ticks, placed. Here rather than in the markup so the component never
/// does arithmetic inside an attribute.
export function axisGridlines(axisMax: number, box: ChartBox = plotBox()): Gridline[] {
  const scale = Math.max(1, axisMax);
  return axisTicks(axisMax).map((value) => ({
    value,
    y: round(box.bottom - (box.height * value) / scale)
  }));
}

/// One point per day. A single-day window sits in the middle rather than on the
/// left edge, which is both what it means and what stops the x scale dividing
/// by a zero-width span.
export function seriesPoints(
  days: DayVolume[],
  series: StatsSeries,
  axisMax: number,
  box: ChartBox = plotBox()
): ChartPoint[] {
  const scale = Math.max(1, axisMax);
  return days.map((day, index) => {
    const value = seriesValue(day, series);
    const ratio = days.length === 1 ? 0.5 : index / (days.length - 1);
    return {
      day: day.day,
      value,
      x: round(box.left + box.width * ratio),
      y: round(box.bottom - box.height * Math.min(1, value / scale))
    };
  });
}

export function linePath(points: ChartPoint[]): string {
  if (!points.length) return '';
  return points.map((point, index) => `${index === 0 ? 'M' : 'L'}${point.x} ${point.y}`).join(' ');
}

/// Closes the line down to the baseline so a series can be washed in under its
/// own stroke. Two flat washes read as volume where two bare lines read as a
/// tangle.
export function areaPath(points: ChartPoint[], box: ChartBox = plotBox()): string {
  if (!points.length) return '';
  const first = points[0];
  const last = points[points.length - 1];
  return `${linePath(points)} L${last.x} ${box.bottom} L${first.x} ${box.bottom} Z`;
}

export type AxisLabel = { day: number; x: number; text: string; anchor: 'start' | 'middle' | 'end' };

/// At most `maximum` labels, always including both ends, always in order. A
/// year of days cannot be labelled day by day, and an unlabelled axis is worse
/// than a sparse one. The end labels anchor inwards, because a centred label on
/// the last point hangs half of itself off the edge of the viewBox.
export function xAxisLabels(points: ChartPoint[], maximum = 5): AxisLabel[] {
  if (!points.length) return [];
  const span = points[points.length - 1].day - points[0].day;
  const chosen =
    points.length <= maximum
      ? points.map((_unused, index) => index)
      : (() => {
          const steps = Math.max(1, maximum - 1);
          const indexes = new Set(
            Array.from({ length: steps + 1 }, (_unused, step) =>
              Math.round((step * (points.length - 1)) / steps)
            )
          );
          return [...indexes].sort((left, right) => left - right);
        })();
  return chosen.map((index) => ({
    day: points[index].day,
    x: points[index].x,
    text: span > 120 ? dayMonthLabel(points[index].day) : dayShortLabel(points[index].day),
    anchor: index === 0 ? 'start' : index === points.length - 1 ? 'end' : 'middle'
  }));
}

export type HeatmapCell = { day: number; count: number; level: number; x: number; y: number };

export type HeatmapLayout = {
  width: number;
  height: number;
  cellSize: number;
  cells: HeatmapCell[];
  monthLabels: { x: number; text: string }[];
  weekdayLabels: { y: number; text: string }[];
  maxCount: number;
  columns: number;
};

const HEATMAP_CELL = 11;
const HEATMAP_GAP = 3;
/// The gutter fits a three-letter weekday at label size; the header fits a
/// month label sitting on the baseline below, clear of the first row of cells.
const HEATMAP_GUTTER = 38;
const HEATMAP_HEADER = 20;
export const HEATMAP_MONTH_BASELINE = 15;

/// Five steps, matching the legend. Zero is its own step so an empty day never
/// looks like a quiet one; the rest are quarters of the busiest day in view, so
/// the ramp rescales with the period instead of fixing a threshold that suits
/// one mailbox and no other.
export function heatmapLevel(count: number, maxCount: number): number {
  if (count <= 0) return 0;
  if (maxCount <= 0) return 1;
  const ratio = count / maxCount;
  if (ratio <= 0.25) return 1;
  if (ratio <= 0.5) return 2;
  if (ratio <= 0.75) return 3;
  return 4;
}

/// Weeks as columns of seven, oldest on the left. The first column starts on
/// the weekday its first day actually fell on, so the grid keeps real weeks
/// instead of drifting a row per period — that offset is the whole trick.
export function heatmapLayout(days: DayVolume[], series: StatsSeries): HeatmapLayout {
  const pitch = HEATMAP_CELL + HEATMAP_GAP;
  const weekdayLabels = [1, 3, 5].map((row) => ({
    y: HEATMAP_HEADER + row * pitch + HEATMAP_CELL - 2,
    text: WEEKDAY_NAMES[row]
  }));
  if (!days.length) {
    return {
      width: HEATMAP_GUTTER,
      height: HEATMAP_HEADER + 7 * pitch,
      cellSize: HEATMAP_CELL,
      cells: [],
      monthLabels: [],
      weekdayLabels,
      maxCount: 0,
      columns: 0
    };
  }
  const firstDay = days[0].day;
  const lead = weekdayOfDay(firstDay);
  const maxCount = days.reduce((highest, day) => Math.max(highest, seriesValue(day, series)), 0);
  const columnOf = (day: number) => Math.floor((day - firstDay + lead) / 7);
  const columns = columnOf(days[days.length - 1].day) + 1;
  const cells = days.map((day) => {
    const count = seriesValue(day, series);
    return {
      day: day.day,
      count,
      level: heatmapLevel(count, maxCount),
      x: HEATMAP_GUTTER + columnOf(day.day) * pitch,
      y: HEATMAP_HEADER + weekdayOfDay(day.day) * pitch
    };
  });
  return {
    width: HEATMAP_GUTTER + columns * pitch,
    height: HEATMAP_HEADER + 7 * pitch,
    cellSize: HEATMAP_CELL,
    cells,
    monthLabels: heatmapMonthLabels(firstDay - lead, columns, pitch),
    weekdayLabels,
    maxCount,
    columns
  };
}

/// Close enough to measure against without a DOM. The labels are three to
/// eight characters of the interface font, and the only question being asked is
/// whether the next one would land on top of this one.
export function approximateTextWidth(text: string, fontPx = CHART_LABEL_PX): number {
  return text.length * fontPx * 0.62;
}

/// A label over the first column of each month, dropped when the one before it
/// would still be occupying that space — measured, because a label carrying a
/// year is twice the width of a bare month and overlapped the next one when
/// this used a single fixed gap. The year rides along whenever it changes, so a
/// multi-year grid does not read as the same twelve months over and over.
function heatmapMonthLabels(
  firstColumnDay: number,
  columns: number,
  pitch: number
): { x: number; text: string }[] {
  const labels: { x: number; text: string }[] = [];
  let previousMonth = -1;
  let previousYear = -1;
  let previousRight = -Infinity;
  for (let column = 0; column < columns; column += 1) {
    const { year, month } = dayParts(firstColumnDay + column * 7);
    if (month === previousMonth && year === previousYear) continue;
    const x = HEATMAP_GUTTER + column * pitch;
    if (x < previousRight) continue;
    const text = year === previousYear ? MONTH_NAMES[month] : `${MONTH_NAMES[month]} ${year}`;
    previousMonth = month;
    previousYear = year;
    previousRight = x + approximateTextWidth(text) + 6;
    labels.push({ x, text });
  }
  return labels;
}

export type RankedAddress = { address: string; label: string; count: number; share: number };

/// `share` is width relative to the leader, not to the total: a ranked list is
/// read by comparing rows to the top row. Zero counts give zero width rather
/// than a division by zero.
export function rankedAddresses(entries: AddressCount[]): RankedAddress[] {
  const leader = entries.reduce((highest, entry) => Math.max(highest, entry.count), 0);
  return entries.map((entry) => ({
    address: entry.address,
    label: entry.name.trim() || entry.address,
    count: entry.count,
    share: leader > 0 ? Math.max(0, Math.min(1, entry.count / leader)) : 0
  }));
}

export type StatsSummary = { received: number; sent: number; dayCount: number; dailyAverage: number };

export function statsSummary(stats: MailStats): StatsSummary {
  const dayCount = Math.max(1, stats.endDay - stats.startDay + 1);
  const total = stats.receivedTotal + stats.sentTotal;
  return {
    received: stats.receivedTotal,
    sent: stats.sentTotal,
    dayCount,
    dailyAverage: Math.round((total / dayCount) * 10) / 10
  };
}

/// Grouped by hand, because `toLocaleString` would make the rendered number a
/// property of the host machine and the tests would have to guess it.
export function formatCount(value: number): string {
  if (!Number.isFinite(value)) return '0';
  const whole = Math.round(Math.abs(value));
  const grouped = String(whole).replace(/\B(?=(\d{3})+(?!\d))/gu, ',');
  return value < 0 ? `-${grouped}` : grouped;
}
