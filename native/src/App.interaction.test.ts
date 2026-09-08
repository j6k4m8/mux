import { mockIPC } from '@tauri-apps/api/mocks';
import { fireEvent, render, screen, waitFor, within } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { describe, expect, test } from 'vitest';
import App from './App.svelte';
import type {
  AccountSummary,
  MailboxBootstrap,
  MessagePage,
  MessageSummary,
  SearchPage,
  ThreadPage,
  ThreadSummary
} from './types';

const account: AccountSummary = {
  id: 'acc_work',
  name: 'Work',
  email: 'jordan@acme.example',
  color: '#5168f4',
  signature: 'Jordan',
  unread: 1,
  total: 2,
  refreshSeconds: 60
};

const threads: ThreadSummary[] = [
  {
    id: 1,
    accountId: account.id,
    subject: 'Architecture sync',
    participants: 'Alice Example, Bob Example',
    snippet: 'Here are the decisions from today.',
    latestAt: Date.UTC(2026, 7, 22, 16),
    messageCount: 2,
    inInbox: true,
    unread: false,
    starred: true,
    hasFromMe: true,
    category: 'Work'
  },
  {
    id: 2,
    accountId: account.id,
    subject: 'Launch checklist',
    participants: 'Dana Example',
    snippet: 'The final checklist is ready.',
    latestAt: Date.UTC(2026, 7, 22, 15),
    messageCount: 1,
    inInbox: true,
    unread: true,
    starred: false,
    hasFromMe: false,
    category: 'Work'
  }
];

const messages: MessageSummary[] = [
  {
    id: 11,
    threadId: 1,
    senderName: 'Alice Example',
    senderEmail: 'alice@example.com',
    recipients: 'Jordan <jordan@acme.example>',
    ccRecipients: 'Bob <bob@example.com>',
    bccRecipients: '',
    sentAt: Date.UTC(2026, 7, 22, 14),
    bodyText: 'Can we finalize the architecture today?\n\n\n\nThe agenda is attached.',
    bodyHtml: '',
    blockedRemoteResources: 0,
    remoteImages: [],
    isFromMe: false
  },
  {
    id: 12,
    threadId: 1,
    senderName: 'Jordan Matelsky',
    senderEmail: account.email,
    recipients: 'Alice <alice@example.com>, Bob <bob@example.com>',
    ccRecipients: 'Carol <carol@example.com>',
    bccRecipients: '',
    sentAt: Date.UTC(2026, 7, 22, 16),
    bodyText: 'Yes. The architecture decision is recorded.',
    bodyHtml: '<p>Yes. The architecture decision is recorded.</p>',
    blockedRemoteResources: 0,
    remoteImages: [],
    isFromMe: true
  }
];

const mailbox: MailboxBootstrap = {
  schemaVersion: 1,
  accounts: [account],
  viewCounts: [
    { accountId: null, inbox: 2, archive: 0, starred: 1, sent: 1, all: 2, snoozed: 0, trash: 0 },
    { accountId: account.id, inbox: 2, archive: 0, starred: 1, sent: 1, all: 2, snoozed: 0, trash: 0 }
  ],
  drafts: [],
  containers: []
};

type IpcCall = { command: string; payload: unknown };
type DraftLoader = (draftId: string) => unknown;

function installMailboxIpc(draftLoader?: DraftLoader): IpcCall[] {
  const calls: IpcCall[] = [];
  mockIPC((command, payload) => {
    calls.push({ command, payload });
    if (command === 'gmail_oauth_begin') {
      return { state: 'connected', accountId: 'gmail-fixture', email: 'reader@example.test' };
    }
    if (command === 'gmail_oauth_cancel') return { state: 'cancel_requested' };
    if (command === 'imap_account_add') {
      const input = (payload as { input?: { host?: string; email?: string } })?.input ?? {};
      if (input.host === 'wrong.example.test') throw new Error('The server refused that username and password');
      return { accountId: 'imap:fixture', email: input.email, folders: 12 };
    }
    if (command === 'mailbox_bootstrap') return mailbox;
    if (command === 'list_threads') {
      return { threads, hasMore: false, nextCursor: null } satisfies ThreadPage;
    }
    if (command === 'get_thread_messages') {
      const threadId = Number((payload as { input: { threadId: number } }).input.threadId);
      const pageMessages = threadId === 1 ? messages : [
        {
          ...messages[0],
          id: 21,
          threadId: 2,
          senderName: 'Dana Example',
          senderEmail: 'dana@example.com',
          ccRecipients: '',
          bodyText: 'The final checklist is ready.',
          bodyHtml: '<p>The final checklist is ready.</p>'
        }
      ];
      return {
        messages: pageMessages,
        attachments: [],
        invitation: threadId === 1 ? {
          threadId: 1,
          uid: 'architecture-sync@example.test',
          title: 'Mux architecture review',
          startAt: Date.UTC(2026, 7, 23, 17),
          endAt: Date.UTC(2026, 7, 23, 18),
          timezone: 'America/New_York',
          location: 'Conference Room A',
          organizer: 'Alice Example <alice@example.com>',
          attendees: 'Jordan <jordan@acme.example>',
          response: 'needsAction',
          conflictText: 'Overlaps a scheduled review.'
        } : null,
        hasMore: false,
        nextCursor: null
      } satisfies MessagePage;
    }
    if (command === 'search_threads') {
      return { rows: threads, hasMore: false, nextCursor: null } satisfies SearchPage;
    }
    if (command === 'get_thread_summary') {
      const threadId = Number((payload as { input: { threadId: number } }).input.threadId);
      return threads.find((thread) => thread.id === threadId) ?? null;
    }
    if (command === 'get_draft') {
      const draftId = String((payload as { draftId: string }).draftId);
      if (draftLoader) return draftLoader(draftId);
      const header = mailbox.drafts.find((draft) => draft.id === draftId);
      if (!header) throw new Error('Draft not found');
      return {
        ...header,
        body: 'Private saved draft body.',
        bodyHtml: '<p>Private saved draft body.</p>'
      };
    }
    if (command === 'apply_thread_action') {
      const action = String((payload as { action: string }).action);
      return { id: `op-${action}`, threadId: 1, field: action === 'read' || action === 'unread' ? 'unread' : 'in_inbox', kind: action, state: 'pending', notBefore: Date.now() + 350 };
    }
    if (command === 'snooze_thread') {
      return { id: 'op-snooze', threadId: 1, field: 'snoozed_until', kind: 'snooze', state: 'confirmed', notBefore: Date.now() };
    }
    if (command === 'rsvp_thread') {
      return { id: 'op-rsvp', threadId: 1, field: 'invitation_response', kind: 'rsvp', state: 'confirmed', notBefore: Date.now() };
    }
    if (command === 'list_operations') return [];
    if (command === 'save_draft') {
      const input = (payload as { input: Record<string, unknown> }).input;
      return {
        ...input,
        id: 'draft-saved',
        accountName: account.name,
        accountEmail: account.email,
        accountColor: account.color,
        updatedAt: Date.now(),
        revision: 1,
        locked: false
      };
    }
    if (command === 'resync_all_mail') return { accountsReset: 1 };
    if (command === 'set_account_refresh') return null;
    if (command === 'sync_account_now') return 1;
    throw new Error(`Unexpected IPC command: ${command}`);
  }, { shouldMockEvents: true });
  return calls;
}

async function renderMailbox(draftLoader?: DraftLoader) {
  const calls = installMailboxIpc(draftLoader);
  const user = userEvent.setup();
  render(App);
  await waitFor(() => expect(screen.getAllByTestId('thread-row')).toHaveLength(2));
  await waitFor(() => expect(screen.getByTestId('reader-subject').textContent).toBe('Architecture sync'));
  return { calls, user };
}

function blurActiveElement() {
  if (document.activeElement instanceof HTMLElement) document.activeElement.blur();
}

describe('production mailbox interactions', () => {
  test('starts typed Gmail onboarding without sending credentials through IPC', async () => {
    const { calls, user } = await renderMailbox();
    await user.click(screen.getByRole('button', { name: /Settings/u }));
    const dialog = screen.getByTestId('settings-screen');
    await user.click(within(dialog).getByRole('button', { name: 'Add Gmail account' }));

    await waitFor(() => expect(within(dialog).getByRole('status').textContent).toBe('Connected reader@example.test.'));
    const oauthCall = calls.find((call) => call.command === 'gmail_oauth_begin');
    expect(oauthCall).toBeTruthy();
    expect(JSON.stringify(oauthCall?.payload ?? {})).not.toMatch(/token|secret|credential|clientId/iu);
  });

  test('re-downloads all mail without asking the store to remove anything', async () => {
    const { calls, user } = await renderMailbox();
    await user.click(screen.getByRole('button', { name: /Settings/u }));
    const dialog = screen.getByTestId('settings-screen');
    await user.click(within(dialog).getByRole('button', { name: 'Mail' }));
    await user.click(within(dialog).getByTestId('resync-all'));

    await waitFor(() =>
      expect(within(dialog).getByRole('status').textContent).toBe(
        'Re-downloading every message from your provider. Nothing was removed.'
      )
    );
    const resync = calls.filter((call) => call.command === 'resync_all_mail');
    expect(resync).toHaveLength(1);
    expect(resync[0]?.payload ?? {}).toEqual({});
    // Re-syncing must never reach for a delete or purge command of any kind.
    expect(calls.some((call) => /purge|delete_all|wipe|clear_mail/iu.test(call.command))).toBe(false);
  });

  test('opens the settings screen from the shortcut and the palette, not a plain comma', async () => {
    const { user } = await renderMailbox();
    blurActiveElement();
    // A bare comma is a typing key, never a navigation key.
    await user.keyboard(',');
    expect(screen.queryByTestId('settings-screen')).toBeNull();

    await user.keyboard('{Meta>},{/Meta}');
    const settings = screen.getByTestId('settings-screen');
    // The settings screen replaces the mailbox rather than floating over it.
    expect(screen.queryByTestId('mailbox-workspace')).toBeNull();
    expect(settings.getAttribute('data-section')).toBe('accounts');

    await user.keyboard('{Escape}');
    await waitFor(() => expect(screen.queryByTestId('settings-screen')).toBeNull());
    expect(screen.getByTestId('mailbox-workspace')).toBeTruthy();

    blurActiveElement();
    await user.keyboard('{Meta>}k{/Meta}');
    const palette = screen.getByTestId('command-palette');
    await user.click(within(palette).getByRole('option', { name: /Open settings/u }));
    await waitFor(() => expect(screen.getByTestId('settings-screen')).toBeTruthy());
  });

  test('moves between settings sections and keeps each one to its own controls', async () => {
    const { user } = await renderMailbox();
    await user.keyboard('{Meta>},{/Meta}');
    const settings = screen.getByTestId('settings-screen');

    // Accounts owns provider sign-in; other sections must not duplicate it.
    expect(within(settings).getByRole('button', { name: 'Add Gmail account' })).toBeTruthy();
    expect(within(settings).queryByTestId('resync-all')).toBeNull();

    await user.click(within(settings).getByRole('button', { name: 'Appearance' }));
    expect(settings.getAttribute('data-section')).toBe('appearance');
    expect(within(settings).queryByRole('button', { name: 'Add Gmail account' })).toBeNull();

    await user.click(within(settings).getByRole('button', { name: 'Shortcuts' }));
    // The reference reads from the shortcut catalog, so it lists the keys that
    // actually work rather than a list kept by hand.
    expect(within(settings).getByText('Command palette')).toBeTruthy();
    expect(within(settings).getByText('Go to folder')).toBeTruthy();
    expect(within(settings).getByText('Move to folder')).toBeTruthy();

    await user.click(within(settings).getByRole('button', { name: 'Back to mail' }));
    await waitFor(() => expect(screen.getByTestId('mailbox-workspace')).toBeTruthy());
  });

  test('hiding an account filters the list without stopping its sync', async () => {
    const { calls, user } = await renderMailbox();
    const toggle = document.querySelector<HTMLButtonElement>(
      '[data-action="toggle-account-visibility"]'
    )!;
    const accountId = toggle.getAttribute('data-account-id')!;
    expect(toggle.getAttribute('aria-pressed')).toBe('true');

    await user.click(toggle);
    await waitFor(() => {
      const requests = calls.filter((call) => call.command === 'list_threads');
      const input = requests.at(-1)?.payload as { input: { hiddenAccountIds: string[] } };
      expect(input.input.hiddenAccountIds).toEqual([accountId]);
    });
    expect(toggle.getAttribute('aria-pressed')).toBe('false');
    // Hiding is a view filter: it must never reach for a sync or delete command.
    expect(calls.some((call) => /sync|delete|remove|disable/iu.test(call.command))).toBe(false);
    expect(window.localStorage.getItem('mux-hidden-accounts')).toBe(JSON.stringify([accountId]));

    await user.click(toggle);
    await waitFor(() => {
      const requests = calls.filter((call) => call.command === 'list_threads');
      const input = requests.at(-1)?.payload as { input: { hiddenAccountIds: string[] } };
      expect(input.input.hiddenAccountIds).toEqual([]);
    });
    expect(toggle.getAttribute('aria-pressed')).toBe('true');
  });

  test('Enter steps into the conversation so j and k move messages until Escape', async () => {
    const { user } = await renderMailbox();
    await waitFor(() => expect(screen.getByTestId('reader-subject')).toBeTruthy());
    blurActiveElement();

    // Before entering, j and k move the thread selection, not messages.
    expect(document.querySelector('.message-card.is-focused')).toBeNull();

    await user.keyboard('{Enter}');
    const focused = await waitFor(() => {
      const card = document.querySelector('.message-card.is-focused');
      expect(card).toBeTruthy();
      return card!;
    });
    const firstFocusedId = focused.id;

    await user.keyboard('k');
    await waitFor(() => {
      const card = document.querySelector('.message-card.is-focused')!;
      expect(card.id).not.toBe(firstFocusedId);
    });

    await user.keyboard('j');
    await waitFor(() => {
      expect(document.querySelector('.message-card.is-focused')!.id).toBe(firstFocusedId);
    });

    // Escape hands navigation back to the folder list.
    await user.keyboard('{Escape}');
    await waitFor(() => expect(document.querySelector('.message-card.is-focused')).toBeNull());
    const selectedBefore = screen.getByTestId('reader-subject').textContent;
    await user.keyboard('j');
    await waitFor(() =>
      expect(screen.getByTestId('reader-subject').textContent).not.toBe(selectedBefore)
    );
  });

  test('appearance choices apply to the shell and survive a reload', async () => {
    const { user } = await renderMailbox();
    await user.keyboard('{Meta>},{/Meta}');
    const settings = screen.getByTestId('settings-screen');
    await user.click(within(settings).getByRole('button', { name: 'Appearance' }));

    await user.click(within(screen.getByTestId('density')).getByRole('button', { name: 'Sardinemode' }));
    expect(screen.getByTestId('mux-shell').getAttribute('data-density')).toBe('sardine');

    await user.click(within(screen.getByTestId('text-size')).getByRole('button', { name: 'Large' }));
    expect(document.documentElement.style.getPropertyValue('--ui-scale')).toBe('1.15');

    await user.click(within(screen.getByTestId('typeface')).getByRole('button', { name: 'Mono' }));
    expect(document.documentElement.style.getPropertyValue('--app-font')).toContain('monospace');

    await user.click(within(screen.getByTestId('animation-speed')).getByRole('button', { name: 'Zoomie' }));
    expect(document.documentElement.style.getPropertyValue('--motion-scale')).toBe('0.45');
    // Picking a speed answers with something moving at it.
    await waitFor(() => expect(screen.getByTestId('undo-toast').textContent).toContain('Like this!'));

    await user.click(within(screen.getByTestId('animation-speed')).getByRole('button', { name: 'None' }));
    expect(document.documentElement.style.getPropertyValue('--motion-scale')).toBe('0');
    await waitFor(() => expect(screen.getByTestId('undo-toast').textContent).toContain('Like this!'));

    const saved = JSON.parse(window.localStorage.getItem('mux-appearance')!);
    expect(saved.density).toBe('sardine');
    expect(saved.scale).toBe(1.15);
    expect(saved.font).toContain('monospace');
    expect(saved.animation).toBe('none');
  });

  test('turning off the body preview takes the third line out of the list', async () => {
    const { user } = await renderMailbox();
    const firstRow = () => screen.getAllByTestId('thread-row')[0];
    expect(firstRow().querySelector('.snippet')?.textContent).toBe('Here are the decisions from today.');

    await user.keyboard('{Meta>},{/Meta}');
    const settings = screen.getByTestId('settings-screen');
    await user.click(within(settings).getByRole('button', { name: 'Appearance' }));
    await user.click(within(screen.getByTestId('list-display')).getByRole('checkbox', {
      name: 'Show preview of body text in mail list'
    }));
    await user.click(within(settings).getByRole('button', { name: 'Back to mail' }));

    await waitFor(() => {
      for (const row of screen.getAllByTestId('thread-row')) {
        expect(row.querySelector('.snippet')).toBeNull();
      }
    });
    // The line is gone from the markup rather than hidden, and the two lines
    // that say what the mail is stay.
    expect(firstRow().querySelector('.thread-line strong')?.textContent).toBe('Alice Example, Bob Example');
    expect(firstRow().querySelector('.subject')?.textContent).toContain('Architecture sync');
    expect(JSON.parse(window.localStorage.getItem('mux-appearance')!).listSnippet).toBe(false);
  });

  test('the appearance sample is a real list row, and answers the preview setting', async () => {
    const { user } = await renderMailbox();
    // The stripe the sample has to match, read off the mail list itself.
    const realStripe = screen.getAllByTestId('thread-row')[0]
      .querySelector<HTMLElement>('.thread-accent')!.style.background;

    await user.keyboard('{Meta>},{/Meta}');
    const settings = screen.getByTestId('settings-screen');
    await user.click(within(settings).getByRole('button', { name: 'Appearance' }));

    const sample = screen.getByTestId('appearance-sample');
    // Built from the list's own class names, so the list's own styling — text
    // size, typeface, density — reaches it without being restated here. Density
    // arrives by inheritance, which needs the sample inside the shell that
    // carries it.
    const shell = screen.getByTestId('mux-shell');
    expect(shell.getAttribute('data-density')).toBe('default');
    expect(shell.contains(sample)).toBe(true);
    const unread = sample.querySelector<HTMLElement>('.thread-row.is-unread')!;
    expect(unread.querySelector<HTMLElement>('.thread-accent')!.style.background).toBe(realStripe);
    expect(unread.querySelector('.avatar')).toBeTruthy();
    expect(unread.querySelector('.thread-copy .thread-line strong')).toBeTruthy();
    expect(unread.querySelector('.thread-copy .subject')).toBeTruthy();
    expect(sample.querySelector('.thread-row.is-selected')).toBeTruthy();
    expect(sample.querySelectorAll('.snippet')).toHaveLength(2);
    // Invented mail: a new install with no messages still has a sample to show.
    expect(sample.textContent).not.toContain('Architecture sync');

    await user.click(within(screen.getByTestId('list-display')).getByRole('checkbox', {
      name: 'Show preview of body text in mail list'
    }));
    await waitFor(() =>
      expect(screen.getByTestId('appearance-sample').querySelectorAll('.snippet')).toHaveLength(0)
    );
  });

  test('each account carries its own refresh cadence, defaulting to a minute', async () => {
    const { calls, user } = await renderMailbox();
    await user.keyboard('{Meta>},{/Meta}');
    const settings = screen.getByTestId('settings-screen');
    const group = within(settings).getAllByTestId('refresh-interval')[0];
    const accountId = group.getAttribute('data-account-id')!;

    // The seeded account defaults to one minute.
    expect(within(group).getByRole('button', { name: '1 min' }).classList.contains('is-active')).toBe(true);

    await user.click(within(group).getByRole('button', { name: '5 min' }));
    await waitFor(() => {
      const call = calls.filter((entry) => entry.command === 'set_account_refresh').at(-1);
      expect(call?.payload).toEqual({ input: { accountId, refreshSeconds: 300 } });
    });
    await waitFor(() =>
      expect(within(settings).getByRole('status').textContent).toBe('Checking for new mail every 5 min.')
    );
  });

  test('thread action buttons show icon, text, and shortcut independently', async () => {
    const { user } = await renderMailbox();
    const archive = document.querySelector<HTMLButtonElement>('.reader-toolbar [data-action="archive"]')!;
    // Default: icon and text, no shortcut chip. Every button carries a tooltip.
    expect(archive.querySelector('svg')).toBeTruthy();
    expect(archive.querySelector('span')?.textContent).toBe('Archive');
    expect(archive.querySelector('kbd')).toBeNull();
    expect(archive.getAttribute('title')).toBe('Archive (E)');

    await user.keyboard('{Meta>},{/Meta}');
    const settings = screen.getByTestId('settings-screen');
    await user.click(within(settings).getByRole('button', { name: 'Appearance' }));
    const display = within(screen.getByTestId('toolbar-display'));
    await user.click(display.getByRole('checkbox', { name: 'Key shortcut' }));
    await user.click(display.getByRole('checkbox', { name: 'Text' }));
    await user.click(within(settings).getByRole('button', { name: 'Back to mail' }));

    const updated = await waitFor(() =>
      document.querySelector<HTMLButtonElement>('.reader-toolbar [data-action="archive"]')!
    );
    expect(updated.querySelector('span')).toBeNull();
    expect(updated.querySelector('kbd')?.textContent).toBe('E');
    expect(updated.querySelector('svg')).toBeTruthy();

    const saved = JSON.parse(window.localStorage.getItem('mux-appearance')!);
    expect(saved.toolbarText).toBe(false);
    expect(saved.toolbarShortcuts).toBe(true);
  });

  test('a custom snooze time must be in the future', async () => {
    const { calls, user } = await renderMailbox();
    blurActiveElement();
    await fireEvent.keyDown(window, { key: 'b' });
    const dialog = screen.getByTestId('snooze-dialog');
    const input = within(dialog).getByTestId('custom-snooze-input') as HTMLInputElement;

    // A past time is refused rather than silently snoozing into the past.
    await fireEvent.input(input, { target: { value: '2020-01-01T09:00' } });
    await user.click(within(dialog).getByRole('button', { name: 'Snooze' }));
    await waitFor(() => expect(within(dialog).getByRole('alert').textContent).toBe('Pick a time in the future.'));
    expect(calls.some((call) => call.command === 'snooze_thread')).toBe(false);

    const future = new Date(Date.now() + 3 * 24 * 60 * 60 * 1000);
    const stamp = `${future.getFullYear()}-${String(future.getMonth() + 1).padStart(2, '0')}-${String(future.getDate()).padStart(2, '0')}T09:00`;
    await fireEvent.input(input, { target: { value: stamp } });
    await user.click(within(dialog).getByRole('button', { name: 'Snooze' }));
    await waitFor(() => {
      const call = calls.filter((entry) => entry.command === 'snooze_thread').at(-1);
      expect((call?.payload as { wakeAt: number }).wakeAt).toBe(new Date(stamp).getTime());
    });
  });

  test('the message rail offers one jump target per message', async () => {
    await renderMailbox();
    const rail = await waitFor(() => screen.getByTestId('message-rail'));
    // Thread 1 has two messages, and the removed Newest button must not return.
    expect(within(rail).getAllByRole('button')).toHaveLength(2);
    expect(document.querySelector('[data-action="jump-newest"]')).toBeNull();
  });

  test('a run of blank lines is one paragraph gap, not several', async () => {
    await renderMailbox();
    await waitFor(() => expect(screen.getByTestId('reader-subject')).toBeTruthy());
    const body = document.querySelector('.message-body')!;
    // This fixture arrives without HTML, so it renders through the plain-text path.
    const paragraphs = [...body.querySelectorAll('p')];
    expect(paragraphs.length).toBeGreaterThan(0);
    // No paragraph may be blank; blank runs collapse rather than stacking gaps.
    expect(paragraphs.every((node) => node.textContent!.trim().length > 0)).toBe(true);
  });

  test('a trackpad swipe fires once, and scroll momentum never fires one', async () => {
    const { calls } = await renderMailbox();
    const row = screen.getAllByTestId('thread-row')[0];
    const threadId = row.getAttribute('data-thread-id');
    const actions = () => calls.filter((call) => call.command === 'apply_thread_action');

    // A vertical flick, followed by the momentum macOS keeps sending afterwards:
    // deltaY decays to almost nothing while a small deltaX remains, so each
    // late event looks horizontal on its own. The gesture committed to vertical
    // and has to stay there, or scrolling the list archives mail.
    await fireEvent.wheel(row, { deltaX: 4, deltaY: 60 });
    for (let index = 0; index < 12; index += 1) {
      await fireEvent.wheel(row, { deltaX: 9, deltaY: 0.4 });
    }
    expect(actions()).toHaveLength(0);

    // A gesture has to go quiet before the next one can start.
    await new Promise((resolve) => setTimeout(resolve, 320));

    // Horizontal, but short of the trigger.
    await fireEvent.wheel(row, { deltaX: 40, deltaY: 0 });
    expect(actions()).toHaveLength(0);

    // A decisive swipe does not wait to confirm the fingers lifted.
    await fireEvent.wheel(row, { deltaX: 90, deltaY: 0 });
    await fireEvent.wheel(row, { deltaX: 90, deltaY: 0 });
    expect(actions()).toHaveLength(1);
    expect(actions()[0]?.payload).toEqual({ threadId: Number(threadId), action: 'archive' });
  });

  test('a swipe that stops somewhere undecided waits for the gesture to end', async () => {
    const { calls } = await renderMailbox();
    const row = screen.getAllByTestId('thread-row')[0];
    const threadId = row.getAttribute('data-thread-id');
    const actions = () => calls.filter((call) => call.command === 'apply_thread_action');

    // Past the trigger but short of decisive, so it is still cancellable.
    await fireEvent.wheel(row, { deltaX: 60, deltaY: 0 });
    await fireEvent.wheel(row, { deltaX: 60, deltaY: 0 });
    expect(actions()).toHaveLength(0);

    await waitFor(() => {
      expect(actions()).toHaveLength(1);
      expect(actions()[0]?.payload).toEqual({ threadId: Number(threadId), action: 'archive' });
    });
  });

  test('a slow drag keeps its progress across gaps between events', async () => {
    const { calls } = await renderMailbox();
    const row = screen.getAllByTestId('thread-row')[0];
    const threadId = row.getAttribute('data-thread-id');
    const actions = () => calls.filter((call) => call.command === 'apply_thread_action');

    // Dragging slowly means real gaps between wheel events — longer here than a
    // gesture that is already past the trigger is given. A partial swipe has to
    // hold what it has built up instead of sliding back between events.
    for (let index = 0; index < 4; index += 1) {
      await fireEvent.wheel(row, { deltaX: 30, deltaY: 0 });
      expect(actions()).toHaveLength(0);
      await new Promise((resolve) => setTimeout(resolve, 300));
    }

    // Four 30px steps reach the trigger, so it commits once the drag stops.
    await waitFor(() => {
      expect(actions()).toHaveLength(1);
      expect(actions()[0]?.payload).toEqual({ threadId: Number(threadId), action: 'archive' });
    });
  });

  test('a row sliding out from under the cursor does not restart the gesture', async () => {
    const { calls } = await renderMailbox();
    const [first, second] = screen.getAllByTestId('thread-row');
    const firstId = first.getAttribute('data-thread-id');
    const actions = () => calls.filter((call) => call.command === 'apply_thread_action');

    // Past the trigger on the first row, but short of committing outright.
    await fireEvent.wheel(first, { deltaX: 60, deltaY: 0 });
    await fireEvent.wheel(first, { deltaX: 60, deltaY: 0 });

    // The row has now translated away from the pointer, so the rest of the same
    // physical gesture lands on its neighbour. Spread over longer than the idle
    // window, so a gesture that ignored these would have ended mid-swipe.
    for (let index = 0; index < 4; index += 1) {
      await new Promise((resolve) => setTimeout(resolve, 120));
      await fireEvent.wheel(second, { deltaX: 20, deltaY: 0 });
    }
    expect(actions()).toHaveLength(0);

    // It commits once the gesture actually stops, and for the row it started on.
    await waitFor(() => {
      expect(actions()).toHaveLength(1);
      expect(actions()[0]?.payload).toEqual({ threadId: Number(firstId), action: 'archive' });
    });
  });

  test('a swipe taken back before the gesture ends does nothing', async () => {
    const { calls } = await renderMailbox();
    const row = screen.getAllByTestId('thread-row')[0];
    const actions = () => calls.filter((call) => call.command === 'apply_thread_action');

    // Past the trigger but short of decisive, then back to where it started.
    await fireEvent.wheel(row, { deltaX: 60, deltaY: 0 });
    await fireEvent.wheel(row, { deltaX: 60, deltaY: 0 });
    await fireEvent.wheel(row, { deltaX: -120, deltaY: 0 });
    await new Promise((resolve) => setTimeout(resolve, 320));
    expect(actions()).toHaveLength(0);
  });

  test('sync now asks the one account and says so', async () => {
    const { calls, user } = await renderMailbox();
    await user.keyboard('{Meta>},{/Meta}');
    const settings = screen.getByTestId('settings-screen');
    const button = document.querySelector<HTMLButtonElement>('[data-action="sync-now"]')!;
    const accountId = button.getAttribute('data-account-id')!;

    await user.click(button);
    await waitFor(() => {
      const call = calls.filter((entry) => entry.command === 'sync_account_now').at(-1);
      expect(call?.payload).toEqual({ input: { accountId } });
    });
    await waitFor(() =>
      expect(within(settings).getByRole('status').textContent).toMatch(/Checking .* for new mail\./u)
    );
    // Adding a mailbox is a separate idea from syncing an existing one.
    expect(within(settings).getByRole('button', { name: 'Add Gmail account' })).toBeTruthy();
    expect(within(settings).queryByRole('button', { name: 'Connect Gmail' })).toBeNull();
  });

  test('boots the real light mailbox shell and toggles a persisted dark theme', async () => {
    const { calls, user } = await renderMailbox();
    const shell = screen.getByTestId('mux-shell');

    expect(shell.dataset.theme).toBe('light');
    expect(screen.getAllByTestId('thread-row').map((row) => row.textContent)).toEqual(
      expect.arrayContaining([expect.stringContaining('Architecture sync'), expect.stringContaining('Launch checklist')])
    );
    // A message that does carry HTML is rendered by its own frame, so the
    // reader's copy of it lives in that document rather than in this one.
    const frame = document.querySelector('iframe.message-frame');
    expect(frame?.getAttribute('srcdoc')).toContain('Yes. The architecture decision is recorded.');
    expect(calls.some((call) => call.command === 'mailbox_bootstrap')).toBe(true);
    expect(calls.some((call) => call.command === 'get_thread_messages')).toBe(true);

    // The theme lives with the other appearance choices rather than in the
    // toolbar, and the stored value is the choice, not the resolved theme.
    await user.keyboard('{Meta>},{/Meta}');
    const settings = screen.getByTestId('settings-screen');
    await user.click(within(settings).getByRole('button', { name: 'Appearance' }));
    const themeChoice = within(settings).getByTestId('theme-choice');
    await user.click(within(themeChoice).getByRole('button', { name: /Dark/u }));

    expect(document.documentElement.dataset.theme).toBe('dark');
    expect(window.localStorage.getItem('mux-theme')).toBe('dark');

    await user.click(within(themeChoice).getByRole('button', { name: 'System' }));
    expect(window.localStorage.getItem('mux-theme')).toBe('system');
    expect(document.documentElement.dataset.theme).toBe('light');
  });

  test('treats a real account ID named all separately from the unified account scope', async () => {
    const literalAllAccount: AccountSummary = {
      id: 'all',
      name: 'Literal All Account',
      email: 'literal-all@example.test',
      color: '#123456',
      signature: '',
      unread: 0,
      total: 0,
      refreshSeconds: 60
    };
    mailbox.accounts.push(literalAllAccount);
    mailbox.viewCounts.push({
      accountId: literalAllAccount.id,
      inbox: 0,
      archive: 0,
      starred: 0,
      sent: 0,
      all: 0,
      snoozed: 0,
      trash: 0
    });
    try {
      const { calls, user } = await renderMailbox();
      // The row holds both a visibility toggle and the select button, so target the latter.
      const literalAccountButton = document.querySelector<HTMLButtonElement>(
        '[data-action="select-account"][data-account-id="all"]'
      )!;
      const unifiedButton = screen.getByRole('button', { name: /All accounts/u });

      await user.click(literalAccountButton);
      await waitFor(() => {
        const requests = calls.filter((call) => call.command === 'list_threads');
        expect((requests.at(-1)?.payload as { input: { accountId: string | null } }).input.accountId).toBe('all');
      });
      expect(literalAccountButton.classList.contains('is-active')).toBe(true);
      expect(unifiedButton.classList.contains('is-active')).toBe(false);

      await user.click(unifiedButton);
      await waitFor(() => {
        const requests = calls.filter((call) => call.command === 'list_threads');
        expect((requests.at(-1)?.payload as { input: { accountId: string | null } }).input.accountId).toBeNull();
      });
      expect(unifiedButton.classList.contains('is-active')).toBe(true);
    } finally {
      mailbox.accounts.pop();
      mailbox.viewCounts.pop();
    }
  });

  test('leaves Tab to the browser and clears search with Escape', async () => {
    const { user } = await renderMailbox();
    const search = screen.getByTestId('mailbox-search') as HTMLInputElement;

    await user.click(search);
    await user.tab();
    expect(document.activeElement).toBe(screen.getByRole('button', { name: 'Open sync status' }));

    await user.click(search);
    // Focusing an empty box seeds the mailbox being searched.
    expect(search.value).toBe('in:inbox ');
    await user.type(search, 'subject:budget');
    expect(search.value).toBe('in:inbox subject:budget');
    await user.keyboard('{Escape}');

    expect(search.value).toBe('');
    expect(document.activeElement).not.toBe(search);
  });

  test('routes r to reply and a to reply-all without stealing editor focus', async () => {
    const { user } = await renderMailbox();
    const inlineReply = screen.getByTestId('inline-reply');
    const reply = within(inlineReply).getByRole('button', { name: /^Reply$/u });
    const replyAll = within(inlineReply).getByRole('button', { name: /^Reply all$/u });
    const editor = within(inlineReply).getByRole('textbox', { name: 'Message body' });
    const replyTo = inlineReply.querySelector<HTMLElement>('.reply-to');

    await user.click(replyAll);
    expect(replyAll.classList.contains('is-active')).toBe(true);
    expect(replyTo?.title).toBe('alice@example.com, bob@example.com, carol@example.com');

    editor.focus();
    await fireEvent.keyDown(editor, { key: 'r' });
    await fireEvent.keyDown(editor, { key: 'a' });
    expect(replyAll.classList.contains('is-active')).toBe(true);
    expect(replyTo?.title).toBe('alice@example.com, bob@example.com, carol@example.com');

    blurActiveElement();
    await fireEvent.keyDown(window, { key: 'r' });
    await waitFor(() => expect(reply.classList.contains('is-active')).toBe(true));
    expect(replyTo?.title).toBe('alice@example.com');

    blurActiveElement();
    await fireEvent.keyDown(window, { key: 'a' });
    await waitFor(() => expect(replyAll.classList.contains('is-active')).toBe(true));
    expect(replyTo?.title).toBe('alice@example.com, bob@example.com, carol@example.com');
  });

  test('expands only after 100 characters and preserves a reply-all draft when popped out', async () => {
    const { calls, user } = await renderMailbox();
    const inlineReply = screen.getByTestId('inline-reply');
    const replyPanel = inlineReply.querySelector<HTMLElement>('.inline-reply');
    const editor = within(inlineReply).getByRole('textbox', { name: 'Message body' }) as HTMLDivElement;
    const firstHundred = 'x'.repeat(100);

    expect(inlineReply.textContent).not.toContain('expands after 100 characters');
    expect(inlineReply.textContent).not.toContain('rich text enabled');

    editor.innerHTML = `<p>${firstHundred}</p>`;
    await fireEvent.input(editor);
    expect(replyPanel?.classList.contains('is-expanded')).toBe(false);
    expect(within(inlineReply).queryByRole('toolbar', { name: 'Message formatting' })).toBeNull();

    const completeMessage = `${firstHundred}y`;
    editor.innerHTML = `<p>${completeMessage}</p>`;
    await fireEvent.input(editor);
    await waitFor(() => expect(replyPanel?.classList.contains('is-expanded')).toBe(true));
    expect(within(inlineReply).getByRole('toolbar', { name: 'Message formatting' })).toBeTruthy();

    await user.click(within(inlineReply).getByRole('button', { name: /^Reply all$/u }));
    await user.click(within(inlineReply).getByRole('button', { name: 'Pop reply out into composer' }));

    const composer = await screen.findByTestId('composer');
    expect(within(composer).getByText('Reply all')).toBeTruthy();
    expect(within(composer).getByLabelText('To recipients').textContent).toContain('alice@example.com');
    expect(within(composer).getByLabelText('To recipients').textContent).toContain('bob@example.com');
    expect(within(composer).getByLabelText('To recipients').textContent).toContain('carol@example.com');
    expect(within(composer).getByRole('textbox', { name: 'Message body' }).textContent).toBe(completeMessage);
    expect(calls.some((call) => call.command === 'save_draft')).toBe(true);

    await user.keyboard('{Escape}');
    await waitFor(() => expect(screen.queryByTestId('composer')).toBeNull());
  });

  test('uses Escape to close the inline link popover before blurring the editor', async () => {
    const { user } = await renderMailbox();
    const inlineReply = screen.getByTestId('inline-reply');
    const editor = within(inlineReply).getByRole('textbox', { name: 'Message body' }) as HTMLDivElement;

    await user.click(within(inlineReply).getByRole('button', { name: 'Formatting' }));
    await user.click(within(inlineReply).getByRole('button', { name: 'Insert link (Command K)' }));
    const linkDialog = within(inlineReply).getByRole('dialog', { name: 'Insert link' });
    await waitFor(() => expect(document.activeElement).toBe(within(linkDialog).getByLabelText('Link destination')));

    await user.keyboard('{Escape}');
    expect(within(inlineReply).queryByRole('dialog', { name: 'Insert link' })).toBeNull();
    expect(document.activeElement).toBe(editor);

    await user.keyboard('{Escape}');
    await waitFor(() => expect(document.activeElement).not.toBe(editor));
  });

  test('opens snooze with b and operates the command palette with Command K', async () => {
    const { user } = await renderMailbox();
    blurActiveElement();

    await fireEvent.keyDown(window, { key: 'b' });
    const snoozeDialog = screen.getByTestId('snooze-dialog');
    expect(document.activeElement).toBe(within(snoozeDialog).getByRole('button', { name: /Later today/u }));
    // Presets, then the custom time field and its submit, then back to close.
    await user.tab();
    await user.tab();
    expect(document.activeElement).toBe(within(snoozeDialog).getByRole('button', { name: /Next week/u }));
    await user.tab();
    expect(document.activeElement).toBe(within(snoozeDialog).getByTestId('custom-snooze-input'));
    await user.tab();
    await user.tab();
    expect(document.activeElement).toBe(within(snoozeDialog).getByRole('button', { name: 'Close snooze options' }));
    await user.tab({ shift: true });
    expect(document.activeElement).toBe(within(snoozeDialog).getByRole('button', { name: 'Snooze' }));
    await fireEvent.keyDown(window, { key: 'Escape' });
    expect(screen.queryByTestId('snooze-dialog')).toBeNull();

    await fireEvent.keyDown(window, { key: 'k', metaKey: true });
    const palette = await screen.findByTestId('command-palette');
    const commandSearch = within(palette).getByRole('textbox', { name: 'Commands filter' });
    await user.type(commandSearch, 'appearance');
    await user.keyboard('{Enter}');

    expect(screen.queryByTestId('command-palette')).toBeNull();
    expect(screen.getByTestId('mux-shell').dataset.theme).toBe('dark');
  });

  test('the toolbar opens sync status, and the rail opens stats', async () => {
    const { user } = await renderMailbox();

    await user.click(screen.getByTestId('sync-button'));
    expect(await screen.findByTestId('sync-screen')).toBeTruthy();
    expect(screen.queryByTestId('mailbox-workspace')).toBeNull();

    await fireEvent.keyDown(window, { key: 'Escape' });
    await waitFor(() => expect(screen.getByTestId('mailbox-workspace')).toBeTruthy());

    await user.click(within(screen.getByTestId('mailbox-navigation')).getByTestId('stats-button'));
    expect(await screen.findByTestId('stats-screen')).toBeTruthy();
    await fireEvent.keyDown(window, { key: 'Escape' });
    await waitFor(() => expect(screen.getByTestId('mailbox-workspace')).toBeTruthy());
  });

  test('adding an imap mailbox verifies it, then clears the password', async () => {
    const { calls, user } = await renderMailbox();
    await user.keyboard('{Meta>},{/Meta}');
    const settings = screen.getByTestId('settings-screen');
    const submit = within(settings).getByTestId('imap-submit') as HTMLButtonElement;
    // Nothing to submit until the mailbox is actually described.
    expect(submit.disabled).toBe(true);

    await user.type(within(settings).getByTestId('imap-host'), 'imap.example.test');
    await user.type(within(settings).getByTestId('imap-username'), 'reader@example.test');
    await user.type(within(settings).getByTestId('imap-password'), 'swordfish');
    await user.type(within(settings).getByTestId('imap-email'), 'reader@example.test');
    expect(submit.disabled).toBe(false);
    await user.click(submit);

    await waitFor(() => {
      const request = calls.filter((call) => call.command === 'imap_account_add').at(-1);
      expect(request?.payload).toMatchObject({
        input: {
          host: 'imap.example.test',
          port: 993,
          username: 'reader@example.test',
          email: 'reader@example.test',
          password: 'swordfish'
        }
      });
    });
    // What the server showed is reported, and the password does not linger.
    await waitFor(() => expect(within(settings).getByRole('status').textContent)
      .toContain('Connected reader@example.test — 12 folders.'));
    expect((within(settings).getByTestId('imap-password') as HTMLInputElement).value).toBe('');
    expect(calls.some((call) => call.command === 'mailbox_bootstrap')).toBe(true);
  });

  test('a mailbox the server refuses is reported and nothing is cleared', async () => {
    const { user } = await renderMailbox();
    await user.keyboard('{Meta>},{/Meta}');
    const settings = screen.getByTestId('settings-screen');

    await user.type(within(settings).getByTestId('imap-host'), 'wrong.example.test');
    await user.type(within(settings).getByTestId('imap-username'), 'reader@example.test');
    await user.type(within(settings).getByTestId('imap-password'), 'swordfish');
    await user.type(within(settings).getByTestId('imap-email'), 'reader@example.test');
    await user.click(within(settings).getByTestId('imap-submit'));

    await waitFor(() => expect(within(settings).getByRole('alert').textContent)
      .toContain('The server refused that username and password'));
    // The details stay put so the mistake can be corrected rather than retyped.
    expect((within(settings).getByTestId('imap-host') as HTMLInputElement).value).toBe('wrong.example.test');
    expect((within(settings).getByTestId('imap-password') as HTMLInputElement).value).toBe('swordfish');
  });

  test('the sender shows who else was on the message, and only the caret closes it', async () => {
    const { user } = await renderMailbox();
    const reader = screen.getByTestId('reader');
    const sender = within(reader).getAllByTitle('Show who this went to')[0];

    // Reading the addresses must not cost you the message.
    await user.click(sender);
    const addresses = reader.querySelector('.message-addresses');
    expect(addresses?.textContent).toContain('alice@example.com');
    expect(within(reader).getAllByRole('button', { name: /^Collapse message/u }).length).toBeGreaterThan(0);

    await user.click(sender);
    expect(reader.querySelector('.message-addresses')).toBeNull();

    // Collapsing is the caret's job alone.
    const caret = within(reader).getAllByRole('button', { name: /^Collapse message/u })[0];
    await user.click(caret);
    expect(within(reader).getAllByRole('button', { name: /^Expand message/u }).length).toBeGreaterThan(0);
  });

  test('the sidebar narrows to icons and remembers that it did', async () => {
    const { user } = await renderMailbox();
    const workspace = screen.getByTestId('mailbox-workspace');
    const toggle = screen.getByTestId('rail-toggle');
    expect(workspace.classList.contains('rail-collapsed')).toBe(false);

    await user.click(toggle);
    expect(workspace.classList.contains('rail-collapsed')).toBe(true);
    expect(screen.getByTestId('mailbox-navigation').classList.contains('is-rail')).toBe(true);
    expect(toggle.getAttribute('aria-expanded')).toBe('false');
    expect(JSON.parse(window.localStorage.getItem('mux-sidebar')!).collapsed).toBe(true);

    // The same chord that narrows it widens it again.
    blurActiveElement();
    await fireEvent.keyDown(window, { key: '\\', metaKey: true });
    expect(workspace.classList.contains('rail-collapsed')).toBe(false);
    expect(JSON.parse(window.localStorage.getItem('mux-sidebar')!).collapsed).toBe(false);
  });

  test('the accent follows the conversation being read, or a colour you pick', async () => {
    const { user } = await renderMailbox();
    const accent = () => document.documentElement.style.getPropertyValue('--accent');
    // Following the message is the default, so the account's own colour wins.
    expect(accent()).toBe(account.color);

    await user.keyboard('{Meta>},{/Meta}');
    const settings = screen.getByTestId('settings-screen');
    await user.click(within(settings).getByRole('button', { name: 'Appearance' }));
    await user.click(within(screen.getByTestId('accent-choice')).getByRole('button', { name: /Teal/u }));
    expect(accent()).toBe('#0f8a76');
    expect(JSON.parse(window.localStorage.getItem('mux-appearance')!).accent).toBe('teal');

    await user.click(within(screen.getByTestId('accent-choice')).getByRole('button', { name: /Current account/u }));
    expect(accent()).toBe(account.color);
  });

  test('an account folder is listed, opens on its own, and is a jump target', async () => {
    mailbox.containers.push({
      accountId: account.id,
      remoteId: 'Label_17',
      name: 'Zoomie Cycle',
      kind: 'label',
      role: 'custom',
      unread: 1,
      total: 3
    });
    try {
      const { calls, user } = await renderMailbox();
      const navigation = screen.getByTestId('mailbox-navigation');
      // Folders are folded away under the account they belong to: an account
      // can have a great many, and the fixed views are what the rail is for.
      expect(within(navigation).queryByRole('button', { name: /Zoomie Cycle/u })).toBeNull();
      const section = within(navigation).getByRole('button', { name: /Work's folders/u });
      expect(section.getAttribute('aria-expanded')).toBe('false');
      await user.click(section);

      const folder = within(navigation).getByRole('button', { name: /Zoomie Cycle/u });
      await user.click(folder);
      // A folder is read inside its own account, and lists only its own mail.
      await waitFor(() => {
        const request = calls.filter((call) => call.command === 'list_threads').at(-1);
        expect(request?.payload).toMatchObject({
          input: { accountId: account.id, view: 'all', containerId: 'Label_17' }
        });
      });
      expect(folder.classList.contains('is-active')).toBe(true);
      expect(within(screen.getByTestId('thread-list-header')).getByRole('heading').textContent).toBe('Zoomie Cycle');

      // Leaving for a fixed view takes the folder scope with it.
      await user.click(within(navigation).getByRole('button', { name: /Inbox/u }));
      await waitFor(() => {
        const request = calls.filter((call) => call.command === 'list_threads').at(-1);
        expect((request?.payload as { input: { containerId: string | null } }).input.containerId).toBeNull();
      });

      blurActiveElement();
      await fireEvent.keyDown(window, { key: 'g' });
      const dialog = await screen.findByTestId('go-to-dialog');
      await user.type(within(dialog).getByRole('textbox', { name: 'Go to filter' }), 'zoomie');
      expect(within(dialog).getAllByRole('option')[0].textContent).toContain('Zoomie Cycle');
      await fireEvent.keyDown(dialog, { key: 'Escape' });
    } finally {
      mailbox.containers.pop();
    }
  });

  test('both headers name a folder and its account, and give every account back afterwards', async () => {
    mailbox.containers.push({
      accountId: account.id,
      remoteId: 'Label_17',
      name: 'Zoomie Cycle',
      kind: 'label',
      role: 'custom',
      unread: 1,
      total: 3
    });
    try {
      const { user } = await renderMailbox();
      const topbar = screen.getByTestId('topbar-workspace');
      const heading = screen.getByTestId('thread-list-header');
      const navigation = screen.getByTestId('mailbox-navigation');
      expect(topbar.querySelector('span')?.textContent).toBe('All accounts');
      expect(within(heading).getByRole('heading').textContent).toBe('Inbox');

      await user.click(within(navigation).getByRole('button', { name: /Work's folders/u }));
      await user.click(within(navigation).getByRole('button', { name: /Zoomie Cycle/u }));
      // A folder exists in one account, so that account is the scope in both places.
      expect(topbar.querySelector('span')?.textContent).toBe('Work');
      expect(topbar.querySelector('small')?.textContent).toBe('Zoomie Cycle');
      expect(heading.querySelector('small')?.textContent).toBe('jordan@acme.example');
      expect(within(heading).getByRole('heading').textContent).toBe('Zoomie Cycle');

      // A search from inside a folder covers the account, not the folder.
      await user.click(screen.getByTestId('mailbox-search'));
      await user.keyboard('budget');
      expect(topbar.querySelector('span')?.textContent).toBe('Work');
      expect(topbar.querySelector('small')?.textContent).toBe('Search all mail');
      expect(within(heading).getByRole('heading').textContent).toBe('Search all mail');
      await user.keyboard('{Escape}');
      expect(topbar.querySelector('small')?.textContent).toBe('Zoomie Cycle');
      expect(within(heading).getByRole('heading').textContent).toBe('Zoomie Cycle');

      await user.click(screen.getByRole('button', { name: /All accounts/u }));
      expect(topbar.querySelector('span')?.textContent).toBe('All accounts');
      expect(topbar.querySelector('small')?.textContent).toBe('All mail');
      expect(heading.querySelector('small')?.textContent).toBe('All accounts');
      expect(within(heading).getByRole('heading').textContent).toBe('All mail');
    } finally {
      mailbox.containers.pop();
    }
  });

  test('both headers say a search is on, how wide it is, and step back when it is cleared', async () => {
    const { user } = await renderMailbox();
    const topbar = screen.getByTestId('topbar-workspace');
    const heading = within(screen.getByTestId('thread-list-header')).getByRole('heading');
    const search = screen.getByTestId('mailbox-search') as HTMLInputElement;

    // The seeded scope alone is not a search yet, so nothing changes.
    await user.click(search);
    expect(search.value).toBe('in:inbox ');
    expect(topbar.querySelector('small')?.textContent).toBe('Inbox');
    expect(heading.textContent).toBe('Inbox');

    await user.keyboard('budget');
    expect(topbar.querySelector('small')?.textContent).toBe('Search inbox');
    expect(heading.textContent).toBe('Search inbox');

    // Without the scope the same words cover the whole account, and the headers say so.
    await user.clear(search);
    await user.keyboard('budget');
    expect(topbar.querySelector('small')?.textContent).toBe('Search all mail');
    expect(heading.textContent).toBe('Search all mail');

    await user.keyboard('{Escape}');
    expect(search.value).toBe('');
    expect(topbar.querySelector('small')?.textContent).toBe('Inbox');
    expect(heading.textContent).toBe('Inbox');
  });

  test('a smart view keeps its own name in both headers', async () => {
    const { user } = await renderMailbox();
    const topbar = screen.getByTestId('topbar-workspace');
    const heading = screen.getByTestId('thread-list-header');

    await user.click(screen.getByTitle('Show only Work'));
    await user.click(screen.getByTitle('Unread conversations'));
    // It runs as a search, but it is the view the reader chose, not a search of one.
    await waitFor(() => expect(within(heading).getByRole('heading').textContent).toBe('Unread'));
    expect(topbar.querySelector('span')?.textContent).toBe('Work');
    expect(topbar.querySelector('small')?.textContent).toBe('Unread');
    expect(heading.querySelector('small')?.textContent).toBe('jordan@acme.example');

    await user.click(screen.getByRole('button', { name: /All accounts/u }));
    expect(topbar.querySelector('span')?.textContent).toBe('All accounts');
    expect(heading.querySelector('small')?.textContent).toBe('All accounts');
    expect(topbar.querySelector('small')?.textContent).toBe('All mail');
    expect(within(heading).getByRole('heading').textContent).toBe('All mail');
  });

  test('jumps to a folder with g without leaving the keyboard', async () => {
    const { calls, user } = await renderMailbox();
    blurActiveElement();

    await fireEvent.keyDown(window, { key: 'g' });
    const dialog = await screen.findByTestId('go-to-dialog');
    const filter = within(dialog).getByRole('textbox', { name: 'Go to filter' });
    expect(document.activeElement).toBe(filter);

    await user.type(filter, 'arch');
    const options = within(dialog).getAllByRole('option');
    expect(options[0].textContent).toContain('Archive');
    await user.keyboard('{Enter}');

    expect(screen.queryByTestId('go-to-dialog')).toBeNull();
    await waitFor(() => {
      const request = calls.filter((call) => call.command === 'list_threads').at(-1);
      expect((request?.payload as { input: { view: string } }).input.view).toBe('archive');
    });
  });

  test('shift+g offers only the account being read, and escape backs out', async () => {
    const { user } = await renderMailbox();
    await user.click(screen.getByTitle('Show only Work'));
    blurActiveElement();

    await fireEvent.keyDown(window, { key: 'G', shiftKey: true });
    const dialog = await screen.findByTestId('go-to-dialog');
    const subtitles = within(dialog).getAllByRole('option').map((option) => option.querySelector('small')?.textContent);
    expect(new Set(subtitles)).toEqual(new Set(['Work · jordan@acme.example']));

    await fireEvent.keyDown(dialog, { key: 'Escape' });
    expect(screen.queryByTestId('go-to-dialog')).toBeNull();
  });

  test('m moves a conversation, and only within its own account', async () => {
    const { calls, user } = await renderMailbox();
    blurActiveElement();

    await fireEvent.keyDown(window, { key: 'm' });
    const dialog = await screen.findByTestId('move-dialog');
    const options = within(dialog).getAllByRole('option');
    expect(options.map((option) => option.querySelector('strong')?.textContent)).toEqual(['Archive', 'Trash']);
    // Every destination names the conversation's own account and no other.
    expect(new Set(options.map((option) => option.querySelector('small')?.textContent)))
      .toEqual(new Set(['Work · jordan@acme.example']));

    await user.click(options[0]);
    expect(screen.queryByTestId('move-dialog')).toBeNull();
    await waitFor(() => {
      const request = calls.filter((call) => call.command === 'apply_thread_action').at(-1);
      expect(request?.payload).toMatchObject({ threadId: 1, action: 'archive' });
    });
  });

  test('the shortcut sheet documents the keys that actually work', async () => {
    await renderMailbox();
    blurActiveElement();

    await fireEvent.keyDown(window, { key: '?', shiftKey: true });
    const sheet = await screen.findByTestId('shortcut-sheet');
    const goTo = within(sheet).getByText('Go to folder').closest('.shortcut-row');
    expect([...(goTo?.querySelectorAll('kbd') ?? [])].map((key) => key.textContent)).toEqual(['G']);
    expect([...within(sheet).getByText('Command palette').closest('.shortcut-row')?.querySelectorAll('kbd') ?? []]
      .map((key) => key.textContent)).toEqual(['⌘', 'K']);

    await fireEvent.keyDown(sheet, { key: 'Escape' });
    expect(screen.queryByTestId('shortcut-sheet')).toBeNull();
  });

  test('the search box colours the query and completes a field with Tab', async () => {
    const { calls, user } = await renderMailbox();
    const search = screen.getByTestId('mailbox-search') as HTMLInputElement;

    await user.click(search);
    await user.keyboard('fro');
    const suggestions = await screen.findByTestId('search-suggestions');
    expect(within(suggestions).getAllByRole('option')[0].textContent).toContain('from:');

    await user.keyboard('{Tab}');
    expect(search.value).toBe('in:inbox from:');
    expect(document.activeElement).toBe(search);
    // The completion reaches the query the native side runs, not just the box.
    await user.keyboard('alice');
    await waitFor(() => {
      const request = calls.filter((call) => call.command === 'search_threads').at(-1);
      expect((request?.payload as { input: { query: string } }).input.query).toBe('in:inbox from:alice');
    });

    await user.keyboard(' is:unread');
    const highlight = search.closest('.search-field')?.querySelector('.search-highlight');
    expect([...(highlight?.querySelectorAll('.field') ?? [])].map((token) => token.textContent)).toEqual(['in:', 'from:', 'is:']);
    expect(highlight?.textContent).toBe(search.value);
  });

  test('the seeded scope is what narrows a search, so deleting it widens', async () => {
    const { calls, user } = await renderMailbox();
    const search = screen.getByTestId('mailbox-search') as HTMLInputElement;

    // The scope alone has narrowed nothing, so the mailbox is still the mailbox.
    await user.click(search);
    expect(search.value).toBe('in:inbox ');
    expect(calls.some((call) => call.command === 'search_threads')).toBe(false);

    await user.keyboard('budget');
    await waitFor(() => {
      const request = calls.filter((call) => call.command === 'search_threads').at(-1);
      expect(request?.payload).toMatchObject({ input: { query: 'in:inbox budget', view: 'all' } });
    });

    // Take the scope away and the same words cover the whole account.
    await user.clear(search);
    await user.keyboard('budget');
    await waitFor(() => {
      const request = calls.filter((call) => call.command === 'search_threads').at(-1);
      expect(request?.payload).toMatchObject({ input: { query: 'budget', view: 'all' } });
    });
  });

  test('enter runs the search, and never saves one on its own', async () => {
    const { user } = await renderMailbox();
    const search = screen.getByTestId('mailbox-search') as HTMLInputElement;

    await user.click(search);
    await user.clear(search);
    await user.keyboard('subject:budget');
    // The only thing on offer is the save row, and it is not preselected.
    const suggestions = await screen.findByTestId('search-suggestions');
    expect(within(suggestions).getAllByRole('option').map((option) => option.getAttribute('aria-selected')))
      .toEqual(['false']);

    await user.keyboard('{Enter}');
    expect(search.value).toBe('subject:budget');
    expect(screen.queryByTestId('search-suggestions')).toBeNull();

    // Walking onto the row and pressing enter is the deliberate act that saves.
    await user.click(search);
    await user.keyboard('{ArrowDown}{Enter}');
    await user.clear(search);
    await user.click(search);
    await user.clear(search);
    const reopened = await screen.findByTestId('search-suggestions');
    expect(within(reopened).getAllByRole('option')[0].textContent).toContain('subject:budget');
  });

  test('a saved search comes back from the dropdown', async () => {
    const { user } = await renderMailbox();
    const search = screen.getByTestId('mailbox-search') as HTMLInputElement;

    await user.click(search);
    await user.clear(search);
    await user.keyboard('is:unread');
    const suggestions = await screen.findByTestId('search-suggestions');
    const save = within(suggestions).getByText('Save "is:unread"');
    await user.click(save);

    await user.clear(search);
    await user.click(search);
    await user.clear(search);
    const reopened = await screen.findByTestId('search-suggestions');
    const saved = within(reopened).getAllByRole('option')[0];
    expect(saved.textContent).toContain('is:unread');
    await user.click(saved);
    expect(search.value).toBe('is:unread');
  });

  test('dispatches toolbar, snooze, and invitation actions through typed native commands', async () => {
    const { calls, user } = await renderMailbox();

    const reader = screen.getByTestId('reader');
    await user.click(within(reader).getByRole('button', { name: /Archive/u }));
    await waitFor(() => expect(calls.some((call) => call.command === 'apply_thread_action')).toBe(true));
    await user.click(within(reader).getByRole('button', { name: 'Trash' }));
    await waitFor(() => expect(calls.some((call) => call.command === 'apply_thread_action' && (call.payload as { action?: string }).action === 'delete')).toBe(true));

    await user.click(within(reader).getByRole('button', { name: /Snooze/u }));
    const snoozeDialog = screen.getByTestId('snooze-dialog');
    await user.click(within(snoozeDialog).getByRole('button', { name: /Later today/u }));
    await waitFor(() => expect(calls.some((call) => call.command === 'snooze_thread')).toBe(true));

    const invitation = await screen.findByTestId('invitation-card');
    await user.click(within(invitation).getByRole('button', { name: 'Accept' }));
    await waitFor(() => expect(calls.some((call) => call.command === 'rsvp_thread')).toBe(true));

    expect(calls.find((call) => call.command === 'apply_thread_action')?.payload).toMatchObject({ threadId: 1, action: 'archive' });
    expect(calls.some((call) => call.command === 'apply_thread_action' && (call.payload as { action?: string }).action === 'delete')).toBe(true);
    expect(calls.find((call) => call.command === 'snooze_thread')?.payload).toMatchObject({ threadId: 1 });
    expect(calls.find((call) => call.command === 'rsvp_thread')?.payload).toEqual({ threadId: 1, response: 'accepted' });
  });

  test('saves an untouched prefilled forward and keeps focus inside its composer', async () => {
    const { calls, user } = await renderMailbox();
    const reader = screen.getByTestId('reader');
    const forward = within(reader).getByRole('button', { name: 'Forward' });
    await user.click(forward);

    const composer = screen.getByTestId('composer');
    const send = within(composer).getByRole('button', { name: /Send/u });
    send.focus();
    await user.tab();
    expect(document.activeElement).toBe(within(composer).getByRole('button', { name: 'Save and close' }));

    await user.click(within(composer).getByRole('button', { name: 'Save & close' }));
    await waitFor(() => expect(calls.some((call) => call.command === 'save_draft')).toBe(true));
    const input = (calls.find((call) => call.command === 'save_draft')?.payload as { input: Record<string, unknown> }).input;
    expect(input.subject).toBe('Fwd: Architecture sync');
    expect(input.bodyHtml).toContain('<blockquote>');
    await waitFor(() => expect(document.activeElement).toBe(forward));
  });

  test('fetches saved draft content explicitly before opening the composer', async () => {
    mailbox.drafts.push({
      id: 'draft-header-only',
      accountId: account.id,
      accountName: account.name,
      accountEmail: account.email,
      accountColor: account.color,
      recipients: 'alice@example.com',
      ccRecipients: '',
      bccRecipients: '',
      subject: 'Header-only draft',
      replyToThreadId: null,
      updatedAt: Date.now(),
      revision: 3,
      locked: false
    });
    try {
      const { calls, user } = await renderMailbox();
      await user.click(screen.getByRole('button', { name: /Drafts/u }));
      const draftRow = await screen.findByTestId('thread-row');
      expect(draftRow.textContent).not.toContain('Private saved draft body.');

      await user.click(draftRow);
      const composer = await screen.findByTestId('composer');
      expect(calls.some((call) => call.command === 'get_draft'
        && (call.payload as { draftId?: string }).draftId === 'draft-header-only')).toBe(true);
      expect((within(composer).getByRole('textbox', { name: 'Subject' }) as HTMLInputElement).value).toBe('Header-only draft');
      expect(composer.textContent).toContain('Private saved draft body.');
    } finally {
      mailbox.drafts.length = 0;
    }
  });

  test('ignores a slower saved-draft response after a newer draft is chosen', async () => {
    const firstHeader = {
      id: 'draft-first',
      accountId: account.id,
      accountName: account.name,
      accountEmail: account.email,
      accountColor: account.color,
      recipients: 'first@example.com',
      ccRecipients: '',
      bccRecipients: '',
      subject: 'First draft',
      replyToThreadId: null,
      updatedAt: Date.now() - 1,
      revision: 1,
      locked: false
    };
    const secondHeader = {
      ...firstHeader,
      id: 'draft-second',
      recipients: 'second@example.com',
      subject: 'Second draft',
      updatedAt: Date.now()
    };
    let resolveFirst!: (value: unknown) => void;
    let resolveSecond!: (value: unknown) => void;
    const firstDetail = new Promise((resolve) => { resolveFirst = resolve; });
    const secondDetail = new Promise((resolve) => { resolveSecond = resolve; });
    mailbox.drafts.push(firstHeader, secondHeader);
    try {
      const { user } = await renderMailbox((draftId) => draftId === firstHeader.id ? firstDetail : secondDetail);
      await user.click(screen.getByRole('button', { name: /Drafts/u }));
      const firstRow = document.querySelector<HTMLButtonElement>(`[data-draft-id="${firstHeader.id}"]`)!;
      const secondRow = document.querySelector<HTMLButtonElement>(`[data-draft-id="${secondHeader.id}"]`)!;

      await user.click(firstRow);
      await user.click(secondRow);
      resolveSecond({ ...secondHeader, body: 'Second body', bodyHtml: '<p>Second body</p>' });

      const composer = await screen.findByTestId('composer');
      await waitFor(() => expect((within(composer).getByRole('textbox', { name: 'Subject' }) as HTMLInputElement).value).toBe('Second draft'));

      resolveFirst({ ...firstHeader, body: 'First body', bodyHtml: '<p>First body</p>' });
      await Promise.resolve();
      await Promise.resolve();
      expect((within(composer).getByRole('textbox', { name: 'Subject' }) as HTMLInputElement).value).toBe('Second draft');
      expect(composer.textContent).not.toContain('First body');
    } finally {
      mailbox.drafts.length = 0;
    }
  });

  test('marks an already-loaded unread conversation read after the user focuses it', async () => {
    threads[0].unread = true;
    try {
      const { calls, user } = await renderMailbox();
      await user.click(screen.getAllByTestId('thread-row')[0]);
      await waitFor(
        () => expect(calls.some((call) => call.command === 'apply_thread_action'
          && (call.payload as { action?: string }).action === 'read')).toBe(true),
        { timeout: 1_200 }
      );
    } finally {
      threads[0].unread = false;
    }
  });

  test('focuses, traps, closes, and restores the local activity dialog', async () => {
    const { user } = await renderMailbox();
    const activityButton = screen.getByRole('button', { name: /Up to date/u });
    await user.click(activityButton);

    const activity = await screen.findByTestId('activity-dialog');
    const close = within(activity).getByRole('button', { name: 'Close activity' });
    expect(document.activeElement).toBe(close);
    await user.tab();
    expect(document.activeElement).toBe(close);
    await user.keyboard('{Escape}');
    expect(screen.queryByTestId('activity-dialog')).toBeNull();
    await waitFor(() => expect(document.activeElement).toBe(activityButton));
  });
});
