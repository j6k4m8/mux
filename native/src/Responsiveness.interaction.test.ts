import { mockIPC } from '@tauri-apps/api/mocks';
import { fireEvent, render, screen, waitFor } from '@testing-library/svelte';
import { describe, expect, test } from 'vitest';
import App from './App.svelte';
import ThreadConversation from './ThreadConversation.svelte';
import type {
  AccountSummary,
  MailboxBootstrap,
  MessagePage,
  MessageSummary,
  ThreadPage,
  ThreadSummary
} from './types';

const account: AccountSummary = {
  id: 'stress-account',
  name: 'Stress fixture',
  email: 'jordan@example.test',
  color: '#5168f4',
  signature: '',
  unread: 0,
  total: 50_000
};

function message(id: number, threadId = 1): MessageSummary {
  return {
    id,
    threadId,
    senderName: id % 2 ? 'External Sender' : 'Jordan',
    senderEmail: id % 2 ? 'sender@example.test' : account.email,
    recipients: account.email,
    ccRecipients: '',
    bccRecipients: '',
    sentAt: id,
    bodyText: `Bounded message ${id}`,
    bodyHtml: `<p>Bounded message ${id}</p>`,
    blockedRemoteResources: 0,
    remoteImages: [],
    isFromMe: id % 2 === 0
  };
}

function thread(id: number): ThreadSummary {
  return {
    id,
    accountId: account.id,
    subject: `Thread ${id}`,
    participants: 'External Sender',
    snippet: `Mailbox row ${id}`,
    latestAt: 50_001 - id,
    messageCount: 1,
    inInbox: true,
    unread: false,
    starred: false,
    hasFromMe: false,
    category: 'Work'
  };
}

describe('native rendering responsiveness', () => {
  test('renders every loaded conversation message without a range control', () => {
    const messages = Array.from({ length: 36 }, (_, index) => message(index + 1));
    const { container } = render(ThreadConversation, {
      messages,
      attachments: [],
      threadUnread: true
    });

    expect(container.querySelectorAll('.message-card')).toHaveLength(36);
    expect(screen.queryByRole('slider', { name: 'Thread timeline' })).toBeNull();
    expect(screen.queryByRole('button', { name: /Show .* earlier/u })).toBeNull();
    const firstCollapsed = container.querySelector<HTMLElement>('.message-card.is-collapsed');
    expect(firstCollapsed?.textContent).toContain('External Sender');
    expect(firstCollapsed?.textContent).toContain('Bounded message 1');
    expect(firstCollapsed?.textContent).not.toContain('sender@example.test');
    expect(firstCollapsed?.querySelector('.collapsed-heading strong')?.textContent).toBe('External Sender');
    expect(firstCollapsed?.querySelector('.collapsed-heading time')).toBeTruthy();
    expect(firstCollapsed?.querySelector('.collapsed-preview')?.textContent).toBe('Bounded message 1');
  });

  test('renders at most 120 rows from 50,000 loaded threads without changing reader selection', async () => {
    const threads = Array.from({ length: 50_000 }, (_, index) => thread(index + 1));
    const mailbox: MailboxBootstrap = {
      schemaVersion: 1,
      accounts: [account],
      viewCounts: [
        { accountId: null, inbox: 50_000, archive: 0, starred: 0, sent: 0, all: 50_000, snoozed: 0, trash: 0 }
      ],
      drafts: []
    };
    const listInputs: Array<{ cursor: string | null; limit: number }> = [];
    mockIPC((command, payload) => {
      if (command === 'vault_status') return { state: 'absent' };
      if (command === 'mailbox_bootstrap') return mailbox;
      if (command === 'list_threads') {
        const input = (payload as { input: { cursor: string | null; limit: number } }).input;
        listInputs.push({ cursor: input.cursor, limit: input.limit });
        if (listInputs.length === 1) {
          return { threads, hasMore: false, nextCursor: null } satisfies ThreadPage;
        }
        const start = input.cursor === null ? 0 : Number(input.cursor);
        const end = start + 50;
        return {
          threads: threads.slice(start, end),
          hasMore: end < threads.length,
          nextCursor: String(end)
        } satisfies ThreadPage;
      }
      if (command === 'get_thread_messages') {
        const threadId = Number((payload as { input: { threadId: number } }).input.threadId);
        return {
          messages: [message(threadId, threadId)],
          attachments: [],
          invitation: null,
          hasMore: false,
          nextCursor: null
        } satisfies MessagePage;
      }
      throw new Error(`Unexpected IPC command: ${command}`);
    }, { shouldMockEvents: true });

    render(App);
    await waitFor(() => expect(screen.getAllByTestId('thread-row')).toHaveLength(120));
    await waitFor(() => expect(screen.getByTestId('reader-subject').textContent).toBe('Thread 1'));

    await fireEvent.click(screen.getByRole('button', { name: 'Show next loaded threads' }));
    expect(screen.getAllByTestId('thread-row')).toHaveLength(120);
    expect(screen.getAllByTestId('thread-row')[0].textContent).toContain('Thread 121');
    expect(screen.getByTestId('reader-subject').textContent).toBe('Thread 1');

    await fireEvent.click(screen.getByRole('button', { name: 'Show previous loaded threads' }));
    expect(screen.getAllByTestId('thread-row')).toHaveLength(120);
    expect(screen.getAllByTestId('thread-row')[0].textContent).toContain('Thread 1');
    expect(screen.getAllByTestId('thread-row')[0].getAttribute('aria-pressed')).toBe('true');

    await fireEvent.focus(window);
    await waitFor(() => expect(listInputs).toHaveLength(3));
    await waitFor(() => expect(screen.getAllByTestId('thread-row')).toHaveLength(100));
    expect(listInputs.slice(1)).toEqual([
      { cursor: null, limit: 50 },
      { cursor: '50', limit: 50 }
    ]);
    expect(screen.getByTestId('reader-subject').textContent).toBe('Thread 1');
  });

  test('updates compact navigation from media-query changes without a resize event', async () => {
    const mailbox: MailboxBootstrap = {
      schemaVersion: 1,
      accounts: [account],
      viewCounts: [
        { accountId: null, inbox: 1, archive: 0, starred: 0, sent: 0, all: 1, snoozed: 0, trash: 0 }
      ],
      drafts: []
    };
    mockIPC((command) => {
      if (command === 'vault_status') return { state: 'absent' };
      if (command === 'mailbox_bootstrap') return mailbox;
      if (command === 'list_threads') {
        return { threads: [thread(1)], hasMore: false, nextCursor: null } satisfies ThreadPage;
      }
      if (command === 'get_thread_messages') {
        return {
          messages: [message(1)],
          attachments: [],
          invitation: null,
          hasMore: false,
          nextCursor: null
        } satisfies MessagePage;
      }
      if (command === 'list_operations') return [];
      throw new Error(`Unexpected IPC command: ${command}`);
    }, { shouldMockEvents: true });

    type MutableMediaQuery = MediaQueryList & { setMatches(next: boolean): void };
    const originalMatchMedia = window.matchMedia;
    const mediaQueries = new Map<string, MutableMediaQuery>();
    Object.defineProperty(window, 'matchMedia', {
      configurable: true,
      writable: true,
      value: (query: string): MediaQueryList => {
        let mediaQuery = mediaQueries.get(query);
        if (mediaQuery) return mediaQuery;
        const listeners = new Set<(event: MediaQueryListEvent) => void>();
        mediaQuery = {
          matches: false,
          media: query,
          onchange: null,
          addEventListener: (_type: string, listener: EventListenerOrEventListenerObject) => {
            if (typeof listener === 'function') listeners.add(listener as (event: MediaQueryListEvent) => void);
          },
          removeEventListener: (_type: string, listener: EventListenerOrEventListenerObject) => {
            if (typeof listener === 'function') listeners.delete(listener as (event: MediaQueryListEvent) => void);
          },
          addListener: (listener) => { if (listener) listeners.add(listener); },
          removeListener: (listener) => { if (listener) listeners.delete(listener); },
          dispatchEvent: () => true,
          setMatches(next: boolean) {
            Object.defineProperty(this, 'matches', { configurable: true, value: next });
            const event = { matches: next, media: query } as MediaQueryListEvent;
            for (const listener of listeners) listener(event);
            this.onchange?.(event);
          }
        };
        mediaQueries.set(query, mediaQuery);
        return mediaQuery;
      }
    });

    try {
      render(App);
      await waitFor(() => expect(screen.getAllByTestId('thread-row')).toHaveLength(1));
      const menu = screen.getByTestId('mobile-menu-button');
      const navigation = screen.getByTestId('mailbox-navigation');
      expect(menu.getAttribute('aria-expanded')).toBe('false');

      mediaQueries.get('(max-width: 980px)')?.setMatches(true);
      await fireEvent.click(menu);

      expect(menu.getAttribute('aria-expanded')).toBe('true');
      expect(navigation.getAttribute('aria-hidden')).toBe('false');
      expect(navigation.hasAttribute('inert')).toBe(false);
    } finally {
      Object.defineProperty(window, 'matchMedia', {
        configurable: true,
        writable: true,
        value: originalMatchMedia
      });
    }
  });
});
