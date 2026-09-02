import { mockIPC } from '@tauri-apps/api/mocks';
import { fireEvent, render, screen, waitFor } from '@testing-library/svelte';
import { describe, expect, test } from 'vitest';
import ThreadConversation from './ThreadConversation.svelte';
import { resolveRemoteImages } from './messageFrame';
import type { MessageSummary, RemoteImageSummary } from './types';

const pngData = 'data:image/png;base64,AA==';

/// Images live inside the reader frame, so the frame's own document is what a
/// message actually gets to show.
function framedImages(container: HTMLElement): string[] {
  const frame = container.querySelector('iframe.message-frame');
  const source = frame?.getAttribute('srcdoc') ?? '';
  return source.match(/<img class="mux-remote"[^>]*>/gu) ?? [];
}

function remoteImage(
  id: number,
  domain: string,
  allowedByPolicy = false,
  altText = `Image ${id}`
): RemoteImageSummary {
  return { id, domain, altText, allowedByPolicy };
}

function message(remoteImages: RemoteImageSummary[]): MessageSummary {
  return {
    id: 11,
    threadId: 1,
    senderName: 'Alice Example',
    senderEmail: 'alice@example.com',
    recipients: 'Jordan <jordan@example.test>',
    ccRecipients: '',
    bccRecipients: '',
    sentAt: Date.UTC(2026, 7, 22, 14),
    bodyText: 'A message with remote images.',
    bodyHtml: '<p>Before <mux-remote-image data-id="7"></mux-remote-image> middle <mux-remote-image data-id="8"></mux-remote-image> after</p>',
    blockedRemoteResources: remoteImages.length,
    remoteImages,
    isFromMe: false
  };
}

describe('remote image privacy controls', () => {
  test('resolves only inert integer markers and never carries a remote URL into the frame', () => {
    const markers = [
      '<mux-remote-image data-id="7"></mux-remote-image>',
      '<mux-remote-image data-id="8"></mux-remote-image>'
    ].join(' ');

    // Nothing is approved yet, so both become placeholders naming what is held.
    const blocked = resolveRemoteImages(markers, {
      7: { dataUrl: null, altText: 'Company logo' },
      8: { dataUrl: null, altText: '' }
    });
    expect(blocked).toContain('aria-label="Company logo"');
    expect(blocked).not.toContain('<img');

    // A response bearing a remote URL rather than approved bytes is refused.
    const hostile = resolveRemoteImages(markers, {
      7: { dataUrl: 'https://tracker.example/pixel', altText: 'Tracker' },
      8: { dataUrl: 'data:text/html;base64,AA==', altText: 'Not an image' }
    });
    expect(hostile).not.toContain('<img');
    expect(hostile).not.toContain('tracker.example');

    // A marker naming a resource this message never declared leaves nothing.
    expect(resolveRemoteImages(markers, {})).toBe(' ');

    const approved = resolveRemoteImages(markers, {
      7: { dataUrl: pngData, altText: 'Logo "quoted"' },
      8: { dataUrl: pngData, altText: '' }
    });
    expect(approved.match(/<img /gu)).toHaveLength(2);
    expect(approved).toContain('alt="Logo &quot;quoted&quot;"');
  });

  test('a background refresh of the open thread does not rebuild the reader', async () => {
    const fixture = message([]);
    const { container, rerender } = render(ThreadConversation, {
      messages: [fixture],
      attachments: []
    });
    const before = container.querySelector('iframe.message-frame');
    expect(before).toBeTruthy();
    const rendered = before!.getAttribute('srcdoc');

    // A sync commit hands the reader a fresh array of equal rows several times a
    // minute. Rebuilding the element on each one reloads the document inside it,
    // which the reader sees as the message flashing.
    await rerender({ messages: [{ ...fixture }], attachments: [] });

    expect(container.querySelector('iframe.message-frame')).toBe(before);
    expect(before!.getAttribute('srcdoc')).toBe(rendered);
  });

  test('loads images for this view without changing persistent policy', async () => {
    const calls: Array<{ command: string; payload: unknown }> = [];
    mockIPC((command, payload) => {
      calls.push({ command, payload });
      if (command === 'load_remote_image') {
        const { input } = payload as { input: { messageId: number; resourceId: number } };
        return { ...input, dataUrl: pngData };
      }
      throw new Error(`unexpected command ${command}`);
    });
    const { container } = render(ThreadConversation, {
      messages: [message([remoteImage(7, 'images.example'), remoteImage(8, 'cdn.example')])],
      attachments: []
    });

    expect(screen.getByText('2 remote items blocked')).toBeTruthy();
    await fireEvent.click(screen.getByRole('button', { name: 'Load images' }));
    await waitFor(() => expect(framedImages(container)).toHaveLength(2));
    expect(calls).toEqual([
      { command: 'load_remote_image', payload: { input: { messageId: 11, resourceId: 7 } } },
      { command: 'load_remote_image', payload: { input: { messageId: 11, resourceId: 8 } } }
    ]);
    expect(calls.some(({ command }) => command.startsWith('allow_remote_content_'))).toBe(false);
  });

  test('auto-loads only policy-approved images and rejects non-data responses', async () => {
    const calls: Array<{ command: string; payload: unknown }> = [];
    mockIPC((command, payload) => {
      calls.push({ command, payload });
      if (command !== 'load_remote_image') throw new Error(`unexpected command ${command}`);
      const { input } = payload as { input: { messageId: number; resourceId: number } };
      return { ...input, dataUrl: input.resourceId === 7 ? pngData : 'https://tracker.example/pixel' };
    });
    const fixture = message([
      remoteImage(7, 'images.example', true),
      remoteImage(8, 'tracker.example', false)
    ]);
    const { container } = render(ThreadConversation, { messages: [fixture], attachments: [] });

    await waitFor(() => expect(framedImages(container)).toHaveLength(1));
    expect(calls).toEqual([
      { command: 'load_remote_image', payload: { input: { messageId: 11, resourceId: 7 } } }
    ]);

    await fireEvent.click(screen.getByRole('button', { name: 'Load images' }));
    expect((await screen.findByRole('alert')).textContent).toBe('Some images could not be loaded.');
    expect(framedImages(container)).toHaveLength(1);
  });

  test('persists sender and exact-domain choices only through their typed commands', async () => {
    const calls: Array<{ command: string; payload: unknown }> = [];
    mockIPC((command, payload) => {
      calls.push({ command, payload });
      if (command === 'load_remote_image') {
        const { input } = payload as { input: { messageId: number; resourceId: number } };
        return { ...input, dataUrl: pngData };
      }
      if (command === 'allow_remote_content_sender' || command === 'allow_remote_content_domain') return undefined;
      throw new Error(`unexpected command ${command}`);
    });
    const fixture = message([
      remoteImage(7, 'images.example'),
      remoteImage(8, 'cdn.example')
    ]);
    const view = render(ThreadConversation, { messages: [fixture], attachments: [] });

    await fireEvent.click(screen.getByRole('button', { name: 'See blocked domains' }));
    expect(screen.getByRole('dialog', { name: 'Remote content domains' })).toBeTruthy();
    const domainRow = screen.getByText('cdn.example').closest('.remote-domain-row');
    expect(domainRow).toBeTruthy();
    await fireEvent.click(domainRow!.querySelector('button')!);
    await waitFor(() => expect(calls).toContainEqual({
      command: 'allow_remote_content_domain',
      payload: { input: { messageId: 11, domain: 'cdn.example' } }
    }));
    await waitFor(() => expect(domainRow!.querySelector('button')?.textContent).toBe('Allowed'));

    view.unmount();
    calls.length = 0;
    render(ThreadConversation, { messages: [fixture], attachments: [] });
    await fireEvent.click(screen.getByRole('button', { name: 'Always load from sender' }));
    await waitFor(() => expect(calls[0]).toEqual({
      command: 'allow_remote_content_sender',
      payload: { input: { messageId: 11 } }
    }));
    await waitFor(() => expect(calls.filter(({ command }) => command === 'load_remote_image')).toHaveLength(2));
  });
});
