import assert from 'node:assert/strict';
import { test } from 'vitest';

import {
  MAILBOX_REFRESH_ROW_TARGET,
  MAILBOX_RENDER_WINDOW_SIZE,
  adjacentMailboxWindowStart,
  boundedRefreshRowTarget,
  collectPagedWindow,
  mailboxRenderWindow,
  mailboxWindowStartForIndex,
  recoverRequiredRow,
  threadPageOrder
} from './pagedWindow';
import { performance } from 'node:perf_hooks';

function rows(first: number, last: number) {
  return Array.from({ length: last - first + 1 }, (_, index) => ({ id: first + index }));
}

function threadRows(firstRank: number, lastRank: number) {
  return Array.from({ length: lastRank - firstRank + 1 }, (_, index) => {
    const rank = firstRank + index;
    return { id: rank, latestAt: 10_000 - rank };
  });
}

test('refresh restores the loaded page-two window and selected row', async () => {
  const requested: Array<string | null> = [];
  const page = await collectPagedWindow({
    minimumRows: 100,
    fetchPage: async (cursor) => {
      requested.push(cursor);
      return cursor === null
        ? { rows: rows(1, 50), nextCursor: 'page-2', hasMore: true }
        : { rows: rows(51, 100), nextCursor: 'page-3', hasMore: true };
    },
    isCurrent: () => true
  });

  assert.deepEqual(requested, [null, 'page-2']);
  assert.equal(page?.rows.length, 100);
  assert.equal(page?.rows.find((row) => row.id === 75)?.id, 75);
  assert.equal(page?.nextCursor, 'page-3');
});

test('a delayed page is discarded after navigation changes generation', async () => {
  let current = true;
  let release: () => void = () => undefined;
  const gate = new Promise<void>((resolve) => { release = resolve; });
  const pending = collectPagedWindow({
    minimumRows: 50,
    fetchPage: async () => {
      await gate;
      return { rows: rows(1, 50), nextCursor: null, hasMore: false };
    },
    isCurrent: () => current
  });

  await Promise.resolve();
  current = false;
  release();
  assert.equal(await pending, null);
});

test('a scoped lookup recovers a selected row shifted beyond the refresh boundary', async () => {
  const refreshed = await collectPagedWindow({
    minimumRows: 100,
    fetchPage: async (cursor) => cursor === null
      ? { rows: rows(0, 49), nextCursor: 'page-2', hasMore: true }
      : { rows: rows(50, 99), nextCursor: 'page-3', hasMore: true },
    isCurrent: () => true
  });
  assert.ok(refreshed);
  assert.equal(refreshed.rows.some((row) => row.id === 100), false);

  const recovered = await recoverRequiredRow({
    page: refreshed,
    requiredId: 100,
    fetchRequired: async (id) => ({ id }),
    isCurrent: () => true
  });
  assert.equal(recovered?.rows.at(-1)?.id, 100);
  assert.equal(recovered?.nextCursor, 'page-3');
});

test('a recovered deeper row remains ordered after the next page is appended', async () => {
  const firstWindow = {
    rows: threadRows(1, 100),
    nextCursor: 'rank-101',
    hasMore: true
  };
  const recovered = await recoverRequiredRow({
    page: firstWindow,
    requiredId: 110,
    fetchRequired: async () => threadRows(110, 110)[0],
    isCurrent: () => true,
    compareRows: threadPageOrder
  });
  assert.ok(recovered);

  const appended = await collectPagedWindow({
    minimumRows: 150,
    initialRows: recovered.rows,
    initialCursor: recovered.nextCursor,
    initialHasMore: true,
    fetchPage: async () => ({ rows: threadRows(101, 150), nextCursor: 'rank-151', hasMore: true }),
    isCurrent: () => true,
    compareRows: threadPageOrder
  });
  assert.deepEqual(appended?.rows.map((row) => row.id), rows(1, 150).map((row) => row.id));
});

test('append deduplicates overlap and rejects a stuck continuation cursor', async () => {
  const appended = await collectPagedWindow({
    minimumRows: 4,
    initialRows: rows(1, 2),
    initialCursor: 'next',
    initialHasMore: true,
    fetchPage: async () => ({ rows: rows(2, 4), nextCursor: null, hasMore: false }),
    isCurrent: () => true
  });
  assert.deepEqual(appended?.rows.map((row) => row.id), [1, 2, 3, 4]);

  await assert.rejects(
    collectPagedWindow({
      minimumRows: 3,
      fetchPage: async () => ({ rows: [{ id: 1 }], nextCursor: 'stuck', hasMore: true }),
      isCurrent: () => true
    }),
    /cursor did not advance/
  );
  await assert.rejects(
    collectPagedWindow({
      minimumRows: 10_001,
      fetchPage: async () => ({ rows: [], nextCursor: null, hasMore: false }),
      isCurrent: () => true
    }),
    /1 to 10,000 rows/
  );
});

test('a 50,000-row mailbox keeps a bounded render window while moving both ways', () => {
  const mailbox = Array.from({ length: 50_000 }, (_, index) => ({ id: index + 1 }));
  const startedAt = performance.now();
  let start = 0;
  for (let page = 0; page < 250; page += 1) {
    const window = mailboxRenderWindow(mailbox, start);
    assert.ok(window.rows.length <= MAILBOX_RENDER_WINDOW_SIZE);
    start = adjacentMailboxWindowStart(window.start, mailbox.length, 1);
  }
  const selectedIndex = 49_876;
  start = mailboxWindowStartForIndex(selectedIndex, mailbox.length, start);
  const selectedWindow = mailboxRenderWindow(mailbox, start);
  assert.equal(selectedWindow.rows.some((row) => row.id === selectedIndex + 1), true);
  start = adjacentMailboxWindowStart(selectedWindow.start, mailbox.length, -1);
  assert.ok(mailboxRenderWindow(mailbox, start).rows.length <= MAILBOX_RENDER_WINDOW_SIZE);
  const elapsedMs = performance.now() - startedAt;

  assert.equal(MAILBOX_RENDER_WINDOW_SIZE, 120);
  assert.ok(elapsedMs < 50, `50k mailbox window operations took ${elapsedMs.toFixed(2)}ms`);
});

test('refresh restoration is constant-bounded after a 50,000-row paging session', async () => {
  const startedAt = performance.now();
  const target = boundedRefreshRowTarget(50_000);
  let fetchedPages = 0;
  const refreshed = await collectPagedWindow({
    minimumRows: target,
    fetchPage: async (cursor) => {
      fetchedPages += 1;
      const start = cursor === null ? 1 : Number(cursor);
      const end = start + 49;
      return { rows: rows(start, end), nextCursor: String(end + 1), hasMore: true };
    },
    isCurrent: () => true
  });
  const elapsedMs = performance.now() - startedAt;

  assert.equal(target, MAILBOX_REFRESH_ROW_TARGET);
  assert.equal(target, 100);
  assert.equal(refreshed?.rows.length, 100);
  assert.equal(fetchedPages, 2);
  assert.ok(elapsedMs < 100, `bounded 50k-session refresh took ${elapsedMs.toFixed(2)}ms`);
});
