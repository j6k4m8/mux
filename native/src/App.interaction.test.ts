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
  total: 2
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
    bodyText: 'Can we finalize the architecture today?',
    bodyHtml: '<p>Can we finalize the architecture today?</p>',
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
  drafts: []
};

type IpcCall = { command: string; payload: unknown };
type DraftLoader = (draftId: string) => unknown;

function installMailboxIpc(draftLoader?: DraftLoader, vaultState = 'absent'): IpcCall[] {
  const calls: IpcCall[] = [];
  mockIPC((command, payload) => {
    calls.push({ command, payload });
    if (command === 'vault_status') return { state: vaultState };
    if (command === 'gmail_oauth_begin') {
      return { state: 'connected', accountId: 'gmail-fixture', email: 'reader@example.test' };
    }
    if (command === 'gmail_oauth_cancel') return { state: 'cancel_requested' };
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
    throw new Error(`Unexpected IPC command: ${command}`);
  }, { shouldMockEvents: true });
  return calls;
}

async function renderMailbox(draftLoader?: DraftLoader, vaultState = 'absent') {
  const calls = installMailboxIpc(draftLoader, vaultState);
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
    const { calls, user } = await renderMailbox(undefined, 'unlocked');
    await user.click(screen.getByRole('button', { name: /Account security/u }));
    const dialog = screen.getByTestId('vault-dialog');
    await user.click(within(dialog).getByRole('button', { name: 'Connect Gmail' }));

    await waitFor(() => expect(within(dialog).getByRole('status').textContent).toBe('Connected reader@example.test.'));
    const oauthCall = calls.find((call) => call.command === 'gmail_oauth_begin');
    expect(oauthCall).toBeTruthy();
    expect(JSON.stringify(oauthCall?.payload ?? {})).not.toMatch(/token|secret|credential|clientId/iu);
  });

  test('boots the real light mailbox shell and toggles a persisted dark theme', async () => {
    const { calls, user } = await renderMailbox();
    const shell = screen.getByTestId('mux-shell');

    expect(shell.dataset.theme).toBe('light');
    expect(screen.getAllByTestId('thread-row').map((row) => row.textContent)).toEqual(
      expect.arrayContaining([expect.stringContaining('Architecture sync'), expect.stringContaining('Launch checklist')])
    );
    expect(screen.getByText('Yes. The architecture decision is recorded.')).toBeTruthy();
    expect(calls.some((call) => call.command === 'mailbox_bootstrap')).toBe(true);
    expect(calls.some((call) => call.command === 'get_thread_messages')).toBe(true);

    await user.click(screen.getByRole('button', { name: 'Switch to dark appearance' }));

    expect(shell.dataset.theme).toBe('dark');
    expect(document.documentElement.dataset.theme).toBe('dark');
    expect(window.localStorage.getItem('mux-theme')).toBe('dark');
  });

  test('treats a real account ID named all separately from the unified account scope', async () => {
    const literalAllAccount: AccountSummary = {
      id: 'all',
      name: 'Literal All Account',
      email: 'literal-all@example.test',
      color: '#123456',
      signature: '',
      unread: 0,
      total: 0
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
      const literalAccountButton = screen.getByRole('button', { name: /Literal All Account/u });
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
    expect(document.activeElement).toBe(screen.getByRole('button', { name: 'Open command palette' }));

    await user.click(search);
    await user.type(search, 'subject:budget');
    expect(search.value).toBe('subject:budget');
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

  test('opens snooze with h and operates the command palette with Command K', async () => {
    const { user } = await renderMailbox();
    blurActiveElement();

    await fireEvent.keyDown(window, { key: 'h' });
    const snoozeDialog = screen.getByTestId('snooze-dialog');
    expect(document.activeElement).toBe(within(snoozeDialog).getByRole('button', { name: /Later today/u }));
    await user.tab();
    await user.tab();
    await user.tab();
    expect(document.activeElement).toBe(within(snoozeDialog).getByRole('button', { name: 'Close snooze options' }));
    await user.tab({ shift: true });
    expect(document.activeElement).toBe(within(snoozeDialog).getByRole('button', { name: /Next week/u }));
    await fireEvent.keyDown(window, { key: 'Escape' });
    expect(screen.queryByTestId('snooze-dialog')).toBeNull();

    await fireEvent.keyDown(window, { key: 'k', metaKey: true });
    const palette = await screen.findByTestId('command-palette');
    const commandSearch = within(palette).getByRole('textbox', { name: 'Command search' });
    await user.type(commandSearch, 'appearance');
    await fireEvent.keyDown(window, { key: 'Enter' });

    expect(screen.queryByTestId('command-palette')).toBeNull();
    expect(screen.getByTestId('mux-shell').dataset.theme).toBe('dark');
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
