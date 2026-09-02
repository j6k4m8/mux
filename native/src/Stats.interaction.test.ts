import { mockIPC } from '@tauri-apps/api/mocks';
import { render, screen, waitFor, within } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { expect, test, vi } from 'vitest';
import StatsScreen from './StatsScreen.svelte';
import type { MailStats } from './stats';
import type { AccountSummary } from './types';

type StatsInput = {
  days: number | null;
  accountId: string | null;
  timezoneOffsetMinutes: number;
  topLimit: number;
};

type IpcCall = { command: string; input: StatsInput };

const accounts: AccountSummary[] = [
  {
    id: 'acc_work',
    name: 'Work',
    email: 'jordan@acme.example',
    color: '#5168f4',
    signature: 'Jordan',
    unread: 1,
    total: 2,
    refreshSeconds: 60
  },
  {
    id: 'acc_home',
    name: 'Home',
    email: 'jordan@home.example',
    color: '#12b886',
    signature: '',
    unread: 0,
    total: 1,
    refreshSeconds: 60
  }
];

/// A Wednesday, so the heatmap's first column is a partial week.
const TODAY = Math.floor(Date.UTC(2026, 8, 2) / 86_400_000);

function statsFor(days: number | null): MailStats {
  const span = days ?? 120;
  const startDay = TODAY - (span - 1);
  return {
    startDay,
    endDay: TODAY,
    days: [
      { day: startDay, received: 4, sent: 1 },
      { day: TODAY - 1, received: 9, sent: 2 },
      { day: TODAY, received: 2, sent: 6 }
    ],
    topSenders: [
      { address: 'alice@example.test', name: 'Alice Example', count: 12 },
      { address: 'bulletin@example.test', name: '', count: 3 }
    ],
    topRecipients: [{ address: 'dana@example.test', name: 'Dana Example', count: 7 }],
    receivedTotal: 15,
    sentTotal: 9,
    truncated: days === null
  };
}

function installStatsIpc(answer: (input: StatsInput) => unknown = (input) => statsFor(input.days)): IpcCall[] {
  const calls: IpcCall[] = [];
  mockIPC((command, payload) => {
    const input = (payload as { input: StatsInput }).input;
    calls.push({ command, input });
    if (command === 'mailbox_stats') return answer(input);
    throw new Error(`unexpected command ${command}`);
  });
  return calls;
}

async function renderStats(options: {
  answer?: (input: StatsInput) => unknown;
  accountList?: AccountSummary[];
} = {}) {
  const calls = installStatsIpc(options.answer);
  const close = vi.fn();
  const user = userEvent.setup();
  const { unmount } = render(StatsScreen, { close, accounts: options.accountList ?? accounts });
  return { calls, close, user, unmount };
}

test('the screen asks the store for its own numbers, in the browser timezone', async () => {
  const { calls } = await renderStats();
  await waitFor(() => expect(screen.getByTestId('stats-window')).toBeTruthy());

  expect(calls).toHaveLength(1);
  expect(calls[0].command).toBe('mailbox_stats');
  expect(calls[0].input.days).toBe(30);
  expect(calls[0].input.accountId).toBe(null);
  expect(calls[0].input.topLimit).toBe(10);
  expect(calls[0].input.timezoneOffsetMinutes).toBe(-new Date().getTimezoneOffset());
});

test('the screen shows a loading line before the store answers', async () => {
  let release = (_value: MailStats) => {};
  await renderStats({ answer: () => new Promise<MailStats>((resolve) => { release = resolve; }) });

  expect(screen.getByTestId('stats-loading')).toBeTruthy();
  expect(screen.queryByTestId('stats-volume-chart')).toBe(null);
  release(statsFor(30));
  await waitFor(() => expect(screen.getByTestId('stats-volume-chart')).toBeTruthy());
  expect(screen.queryByTestId('stats-loading')).toBe(null);
});

test('one period button refetches every plot on the screen', async () => {
  const { calls, user } = await renderStats();
  await waitFor(() => expect(screen.getByTestId('stats-volume-chart')).toBeTruthy());
  const cellsFor30Days = screen.getByTestId('stats-heatmap').querySelectorAll('.stats-cell').length;
  expect(cellsFor30Days).toBe(30);
  expect(screen.getByTestId('stats-period-30d').getAttribute('aria-pressed')).toBe('true');

  await user.click(screen.getByTestId('stats-period-1y'));

  await waitFor(() => {
    expect(screen.getByTestId('stats-heatmap').querySelectorAll('.stats-cell')).toHaveLength(365);
  });
  expect(calls.map((call) => call.input.days)).toEqual([30, 365]);
  expect(screen.getByTestId('stats-period-1y').getAttribute('aria-pressed')).toBe('true');
  expect(screen.getByTestId('stats-period-30d').getAttribute('aria-pressed')).toBe('false');
  // The line chart is redrawn from the same window, so it has a point a day.
  const chart = screen.getByTestId('stats-volume-chart');
  expect(chart.querySelectorAll('path.stats-line')).toHaveLength(2);

  // Clicking the period already showing does not ask again.
  await user.click(screen.getByTestId('stats-period-1y'));
  expect(calls).toHaveLength(2);
});

test('all time asks for no day window at all and says the window is clipped', async () => {
  const { calls, user } = await renderStats();
  await waitFor(() => expect(screen.getByTestId('stats-volume-chart')).toBeTruthy());

  await user.click(screen.getByTestId('stats-period-all'));

  await waitFor(() => expect(calls).toHaveLength(2));
  expect(calls[1].input.days).toBe(null);
  await waitFor(() => {
    expect(screen.getByTestId('stats-window').textContent).toContain('is not counted');
  });
});

test('the heatmap toggle swaps the series without asking the store again', async () => {
  const { calls, user } = await renderStats();
  await waitFor(() => expect(screen.getByTestId('stats-heatmap')).toBeTruthy());

  const received = screen.getByTestId('stats-heatmap');
  expect(received.getAttribute('data-series')).toBe('received');
  // Nine received on the day before, against six sent: the busiest square
  // changes with the series.
  const busiestReceived = received.querySelector(`[data-day="${TODAY - 1}"]`);
  expect(busiestReceived?.getAttribute('data-level')).toBe('4');
  expect(received.querySelector(`[data-day="${TODAY}"]`)?.getAttribute('data-level')).toBe('1');

  await user.click(screen.getByTestId('stats-heatmap-sent'));

  const sent = screen.getByTestId('stats-heatmap');
  expect(sent.getAttribute('data-series')).toBe('sent');
  expect(sent.querySelector(`[data-day="${TODAY}"]`)?.getAttribute('data-level')).toBe('4');
  expect(sent.querySelector(`[data-day="${TODAY - 1}"]`)?.getAttribute('data-level')).toBe('2');
  expect(screen.getByTestId('stats-heatmap-sent').getAttribute('aria-pressed')).toBe('true');
  expect(calls).toHaveLength(1);
});

test('every day in the window gets a square, and a silent day is drawn empty', async () => {
  await renderStats();
  await waitFor(() => expect(screen.getByTestId('stats-heatmap')).toBeTruthy());

  const cells = screen.getByTestId('stats-heatmap').querySelectorAll('.stats-cell');
  expect(cells).toHaveLength(30);
  const quiet = screen.getByTestId('stats-heatmap').querySelector(`[data-day="${TODAY - 5}"]`);
  expect(quiet?.getAttribute('data-level')).toBe('0');
  expect(quiet?.querySelector('title')?.textContent).toContain('0 received messages');
  expect(
    screen.getByTestId('stats-heatmap').querySelector(`[data-day="${TODAY - 1}"] title`)?.textContent
  ).toContain('9 received messages');
});

test('the ranked lists name people and count their mail', async () => {
  await renderStats();
  await waitFor(() => expect(screen.getByTestId('stats-top-senders')).toBeTruthy());

  const senders = within(screen.getByTestId('stats-top-senders')).getAllByRole('listitem');
  expect(senders).toHaveLength(2);
  expect(senders[0].textContent).toContain('Alice Example');
  expect(senders[0].textContent).toContain('12');
  // No display name in the mail, so the address itself is the label.
  expect(senders[1].textContent).toContain('bulletin@example.test');

  const recipients = within(screen.getByTestId('stats-top-recipients')).getAllByRole('listitem');
  expect(recipients).toHaveLength(1);
  expect(recipients[0].textContent).toContain('Dana Example');
});

test('an account filter scopes every plot to one account', async () => {
  const { calls, user } = await renderStats();
  await waitFor(() => expect(screen.getByTestId('stats-volume-chart')).toBeTruthy());

  await user.selectOptions(screen.getByTestId('stats-account'), 'acc_home');

  await waitFor(() => expect(calls).toHaveLength(2));
  expect(calls[1].input.accountId).toBe('acc_home');
  expect(calls[1].input.days).toBe(30);
});

test('a single account is not offered as a choice', async () => {
  await renderStats({ accountList: [accounts[0]] });
  await waitFor(() => expect(screen.getByTestId('stats-volume-chart')).toBeTruthy());
  expect(screen.queryByTestId('stats-account')).toBe(null);
});

test('a brand new install draws an empty window rather than a broken chart', async () => {
  const empty: MailStats = {
    startDay: TODAY,
    endDay: TODAY,
    days: [],
    topSenders: [],
    topRecipients: [],
    receivedTotal: 0,
    sentTotal: 0,
    truncated: false
  };
  await renderStats({ answer: () => empty, accountList: [] });

  await waitFor(() => expect(screen.getByTestId('stats-empty')).toBeTruthy());
  const chart = screen.getByTestId('stats-volume-chart');
  // A baseline and a top gridline: an axis, not a division by zero.
  expect(chart.querySelectorAll('line.stats-gridline').length).toBeGreaterThanOrEqual(2);
  expect(screen.getByTestId('stats-heatmap').querySelectorAll('.stats-cell')).toHaveLength(1);
  expect(screen.getByTestId('stats-top-senders-empty')).toBeTruthy();
  expect(screen.getByTestId('stats-top-recipients-empty')).toBeTruthy();
});

test('a refused window is reported instead of drawn', async () => {
  await renderStats({
    answer: () => {
      throw new Error('days must be between 1 and 1830, or omitted for all time');
    }
  });

  await waitFor(() => expect(screen.getByTestId('stats-error')).toBeTruthy());
  expect(screen.getByTestId('stats-error').textContent).toContain('days must be between 1 and 1830');
  expect(screen.queryByTestId('stats-volume-chart')).toBe(null);
});

test('no chart attribute is ever NaN, on a full window or an empty one', async () => {
  const empty: MailStats = {
    startDay: TODAY,
    endDay: TODAY,
    days: [],
    topSenders: [],
    topRecipients: [],
    receivedTotal: 0,
    sentTotal: 0,
    truncated: false
  };
  for (const answer of [(input: StatsInput) => statsFor(input.days), () => empty]) {
    const { unmount } = await renderStats({ answer });
    await waitFor(() => expect(screen.getByTestId('stats-volume-chart')).toBeTruthy());
    for (const id of ['stats-volume-chart', 'stats-heatmap']) {
      for (const element of screen.getByTestId(id).querySelectorAll('*')) {
        for (const attribute of element.attributes) {
          expect(attribute.value, `${id} ${element.tagName}.${attribute.name}`).not.toMatch(
            /NaN|undefined|Infinity/u
          );
        }
      }
    }
    unmount();
  }
});

test('going back is the caller s decision, not the screen s', async () => {
  const { close, user } = await renderStats();
  await waitFor(() => expect(screen.getByTestId('stats-volume-chart')).toBeTruthy());

  await user.click(screen.getByTestId('stats-close'));

  expect(close).toHaveBeenCalledTimes(1);
});
