import { render } from '@testing-library/svelte';
import { describe, expect, test } from 'vitest';
import ThreadConversation from './ThreadConversation.svelte';
import type { AttachmentSummary, MessageSummary } from './types';

/// Messages are numbered in the order they were sent, so a higher id is a
/// newer message and the ids read as positions in the conversation.
function message(id: number, isFromMe = false): MessageSummary {
  return {
    id,
    threadId: 1,
    senderName: isFromMe ? 'Jordan Matelsky' : 'Alice Example',
    senderEmail: isFromMe ? 'jordan@example.test' : 'alice@example.com',
    recipients: isFromMe ? 'Alice <alice@example.com>' : 'Jordan <jordan@example.test>',
    ccRecipients: '',
    bccRecipients: '',
    sentAt: Date.UTC(2026, 7, 22, 8 + id),
    bodyText: `Message ${id} in full.`,
    bodyHtml: '',
    blockedRemoteResources: 0,
    remoteImages: [],
    isFromMe
  };
}

function folded(container: HTMLElement, id: number): boolean {
  return container.querySelector(`#native-message-${id}`)!.classList.contains('is-collapsed');
}

function attachment(messageId: number, disposition: AttachmentSummary['disposition'] = 'attachment'): AttachmentSummary {
  return {
    id: `att-${messageId}-${disposition}-${Math.random()}`,
    messageId,
    filename: 'invoice.pdf',
    mediaType: 'application/pdf',
    byteLength: 1024,
    contentId: '',
    disposition
  };
}

describe('how a conversation opens', () => {
  test('every message is folded, whatever its age or read state', () => {
    const { container } = render(ThreadConversation, {
      messages: [message(1), message(2, true), message(3)],
      attachments: [],
      threadUnread: true
    });

    const cards = [...container.querySelectorAll('.message-card')];
    expect(cards).toHaveLength(3);
    expect(cards.every((card) => card.classList.contains('is-collapsed'))).toBe(true);
    expect(container.querySelector('[aria-expanded="true"]')).toBeNull();
    expect(container.querySelector('.message-body')).toBeNull();

    // The unread boundary marks where to start reading; it does not open
    // the message it sits in front of.
    const boundary = container.querySelector('#native-unread-boundary');
    expect(boundary?.textContent).toBe('Unread from here');
    expect(boundary?.nextElementSibling?.id).toBe('native-message-3');

    // Folded, the newest message still says who wrote it, when, and what.
    const newest = container.querySelector('#native-message-3')!;
    expect(newest.querySelector('.collapsed-heading strong')?.textContent).toBe('Alice Example');
    expect(newest.querySelector('.collapsed-heading time')).toBeTruthy();
    expect(newest.querySelector('.collapsed-preview')?.textContent?.trim()).toBe('Message 3 in full.');
  });

  test('a folded message with a real attachment says so; an inline image does not count', () => {
    const { container } = render(ThreadConversation, {
      messages: [message(1), message(2)],
      attachments: [attachment(1, 'inline'), attachment(2, 'attachment'), attachment(2, 'attachment')]
    });

    // Message 1 carries only an inline image the body already shows — no badge.
    expect(container.querySelector('#native-message-1 .collapsed-attachment-badge')).toBeNull();

    // Message 2 carries two real attachments, invisible while folded but for this.
    const badge = container.querySelector('#native-message-2 .collapsed-attachment-badge');
    expect(badge?.textContent?.trim()).toBe('2');
    expect(badge?.getAttribute('title')).toBe('2 attachments');
  });

  test('jumping to the newest message unfolds it, and only it', async () => {
    const { container, component } = render(ThreadConversation, {
      messages: [message(1), message(2)],
      attachments: []
    });

    await component.jumpToNewest();
    expect(folded(container, 2)).toBe(false);
    expect(folded(container, 1)).toBe(true);
    expect(HTMLElement.prototype.scrollIntoView).toHaveBeenCalled();
  });

  test('jumping to the unread message unfolds it, and only it', async () => {
    const { container, component } = render(ThreadConversation, {
      messages: [message(1), message(2), message(3, true)],
      attachments: [],
      threadUnread: true
    });

    // Unread starts at the last message someone else sent, not at my own reply.
    await component.jumpToUnread();
    expect(folded(container, 2)).toBe(false);
    expect(folded(container, 1)).toBe(true);
    expect(folded(container, 3)).toBe(true);
  });

  test('history paged in stays folded, while a message that arrives opens', async () => {
    const { container, rerender } = render(ThreadConversation, {
      messages: [message(2), message(3)],
      attachments: []
    });

    // Show older prepends history; asking for it is not the same as it arriving.
    await rerender({ messages: [message(1), message(2), message(3)], attachments: [] });
    expect(container.querySelectorAll('.message-card')).toHaveLength(3);
    expect(container.querySelectorAll('.message-card.is-collapsed')).toHaveLength(3);

    await rerender({ messages: [message(1), message(2), message(3), message(4)], attachments: [] });
    expect(container.querySelectorAll('.message-card')).toHaveLength(4);
    expect(folded(container, 4)).toBe(false);
    expect(container.querySelectorAll('.message-card.is-collapsed')).toHaveLength(3);
  });
});
