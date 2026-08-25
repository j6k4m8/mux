/**
 * Collect a stable, deduplicated page window without committing partial results.
 * Returning null tells the caller that navigation made this request stale.
 *
 * @template {{ id: string | number }} T
 * @param {{
 *   minimumRows: number,
 *   initialRows?: T[],
 *   initialCursor?: string | null,
 *   initialHasMore?: boolean,
 *   fetchPage: (cursor: string | null) => Promise<{ rows: T[], nextCursor: string | null, hasMore: boolean }>,
 *   isCurrent: () => boolean,
 *   compareRows?: ((left: T, right: T) => number) | null
 * }} options
 * @returns {Promise<{ rows: T[], nextCursor: string | null, hasMore: boolean } | null>}
 */
export async function collectPagedWindow({
  minimumRows,
  initialRows = [],
  initialCursor = null,
  initialHasMore = true,
  fetchPage,
  isCurrent,
  compareRows = null
}) {
  const rows = new Map(initialRows.map((row) => [row.id, row]));
  let nextCursor = initialCursor;
  let hasMore = initialHasMore;
  let mustFetch = initialRows.length === 0;
  if (!Number.isInteger(minimumRows) || minimumRows < 1 || minimumRows > 10_000) {
    throw new Error('Mailbox refresh supports a loaded window of 1 to 10,000 rows');
  }
  const target = minimumRows;
  const seenCursors = new Set();

  while (mustFetch || (hasMore && rows.size < target)) {
    if (!isCurrent()) return null;
    if (nextCursor !== null) {
      if (seenCursors.has(nextCursor)) throw new Error('Mailbox paging cursor did not advance');
      seenCursors.add(nextCursor);
    }
    const page = await fetchPage(nextCursor);
    if (!isCurrent()) return null;
    for (const row of page.rows) rows.set(row.id, row);
    nextCursor = page.nextCursor;
    hasMore = page.hasMore;
    mustFetch = false;
    if (hasMore && !nextCursor) throw new Error('Mailbox page is missing its continuation cursor');
  }

  const collected = [...rows.values()];
  if (compareRows) collected.sort(compareRows);
  return { rows: collected, nextCursor, hasMore };
}

export const MAILBOX_RENDER_WINDOW_SIZE = 120;
export const MAILBOX_REFRESH_ROW_TARGET = 100;

/**
 * Refreshes restore at most two native pages. A selected row outside that
 * range is recovered separately by its scoped lookup, so accumulated paging
 * history never turns a focus/event refresh into an unbounded refetch.
 *
 * @param {number} loadedRows
 * @param {number} minimumRows
 * @param {number} maximumRows
 */
export function boundedRefreshRowTarget(
  loadedRows,
  minimumRows = 50,
  maximumRows = MAILBOX_REFRESH_ROW_TARGET
) {
  if (!Number.isInteger(loadedRows) || loadedRows < 0) throw new Error('Loaded row count must be a non-negative integer');
  if (!Number.isInteger(minimumRows) || minimumRows < 1) throw new Error('Minimum refresh rows must be a positive integer');
  if (!Number.isInteger(maximumRows) || maximumRows < minimumRows) throw new Error('Maximum refresh rows must include the minimum');
  return Math.min(maximumRows, Math.max(minimumRows, loadedRows));
}

/** @param {number} start @param {number} rowCount @param {number} size */
export function normalizeMailboxWindowStart(start, rowCount, size = MAILBOX_RENDER_WINDOW_SIZE) {
  if (!Number.isInteger(rowCount) || rowCount < 0) throw new Error('Mailbox row count must be a non-negative integer');
  if (!Number.isInteger(size) || size < 1 || size > MAILBOX_RENDER_WINDOW_SIZE) {
    throw new Error(`Mailbox render windows must contain 1 to ${MAILBOX_RENDER_WINDOW_SIZE} rows`);
  }
  const safeStart = Number.isInteger(start) ? start : 0;
  return Math.min(Math.max(0, rowCount - size), Math.max(0, safeStart));
}

/**
 * Return a cheap render-only slice while retaining every loaded row in the
 * paging model. Moving the render window never mutates or discards data.
 *
 * @template T
 * @param {T[]} rows
 * @param {number} start
 * @param {number} size
 */
export function mailboxRenderWindow(rows, start, size = MAILBOX_RENDER_WINDOW_SIZE) {
  const normalizedStart = normalizeMailboxWindowStart(start, rows.length, size);
  const end = Math.min(rows.length, normalizedStart + size);
  return { start: normalizedStart, end, rows: rows.slice(normalizedStart, end) };
}

/** @param {number} currentStart @param {number} rowCount @param {-1 | 1} direction @param {number} size */
export function adjacentMailboxWindowStart(
  currentStart,
  rowCount,
  direction,
  size = MAILBOX_RENDER_WINDOW_SIZE
) {
  if (direction !== -1 && direction !== 1) throw new Error('Mailbox window direction must be -1 or 1');
  const start = normalizeMailboxWindowStart(currentStart, rowCount, size);
  return normalizeMailboxWindowStart(start + direction * size, rowCount, size);
}

/**
 * Keep keyboard-selected rows inside the render window without scanning or
 * copying the full mailbox.
 *
 * @param {number} index
 * @param {number} rowCount
 * @param {number} currentStart
 * @param {number} size
 */
export function mailboxWindowStartForIndex(
  index,
  rowCount,
  currentStart,
  size = MAILBOX_RENDER_WINDOW_SIZE
) {
  if (!Number.isInteger(index) || index < 0 || index >= rowCount) return normalizeMailboxWindowStart(currentStart, rowCount, size);
  const start = normalizeMailboxWindowStart(currentStart, rowCount, size);
  if (index < start) return normalizeMailboxWindowStart(index, rowCount, size);
  if (index >= start + size) return normalizeMailboxWindowStart(index - size + 1, rowCount, size);
  return start;
}

/**
 * Recover one selected row that shifted just beyond a refreshed window. The
 * caller supplies a bounded, scope-aware local lookup; no page scan is needed.
 *
 * @template {{ id: string | number }} T
 * @param {{
 *   page: { rows: T[], nextCursor: string | null, hasMore: boolean },
 *   requiredId: string | number | null,
 *   fetchRequired: (id: string | number) => Promise<T | null>,
 *   isCurrent: () => boolean,
 *   compareRows?: ((left: T, right: T) => number) | null
 * }} options
 * @returns {Promise<{ rows: T[], nextCursor: string | null, hasMore: boolean } | null>}
 */
export async function recoverRequiredRow({
  page,
  requiredId,
  fetchRequired,
  isCurrent,
  compareRows = null
}) {
  if (requiredId === null || page.rows.some((row) => row.id === requiredId)) return page;
  if (!isCurrent()) return null;
  const row = await fetchRequired(requiredId);
  if (!isCurrent()) return null;
  if (!row) return page;
  const rows = [...page.rows, row];
  if (compareRows) rows.sort(compareRows);
  return { ...page, rows };
}

/** @param {{ id: number, latestAt: number }} left @param {{ id: number, latestAt: number }} right */
export function threadPageOrder(left, right) {
  return right.latestAt - left.latestAt || right.id - left.id;
}
