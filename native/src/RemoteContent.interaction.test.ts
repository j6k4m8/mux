import { mockIPC } from '@tauri-apps/api/mocks';
import { fireEvent, render, screen, waitFor } from '@testing-library/svelte';
import { describe, expect, test } from 'vitest';
import RichText from './RichText.svelte';
import ThreadConversation from './ThreadConversation.svelte';
import { parseMessageRichText, parseRichText } from './richText';
import type { MessageSummary, RemoteImageSummary } from './types';

const pngData = 'data:image/png;base64,AA==';

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
  test('parses only inert integer markers and never carries a remote URL into render nodes', () => {
    const nodes = parseMessageRichText(`
      <mux-remote-image data-id="7" src="https://tracker.example/pixel"></mux-remote-image>
      <mux-remote-image data-id="65"></mux-remote-image>
      <mux-remote-image data-id="-1"></mux-remote-image>
      <mux-remote-image data-id="7.5"></mux-remote-image>
    `);

    expect(nodes.filter((node) => node.type !== 'text' || node.text.trim())).toEqual([
      { type: 'remote-image', resourceId: 7 }
    ]);
    expect(JSON.stringify(nodes)).not.toContain('tracker.example');
    expect(parseRichText('<mux-remote-image data-id="7"></mux-remote-image>')).toEqual([]);

    const { container } = render(RichText, {
      nodes,
      remoteImages: { 7: { dataUrl: null, altText: 'Company logo' } }
    });
    expect(container.querySelector('img')).toBeNull();
    expect(screen.getByRole('img', { name: 'Company logo' })).toBeTruthy();

    const hostile = render(RichText, {
      nodes,
      remoteImages: { 7: { dataUrl: 'https://tracker.example/pixel', altText: 'Tracker' } }
    });
    expect(hostile.container.querySelector('img')).toBeNull();
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
    await waitFor(() => expect(container.querySelectorAll('img.remote-message-image')).toHaveLength(2));
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

    await waitFor(() => expect(container.querySelectorAll('img.remote-message-image')).toHaveLength(1));
    expect(calls).toEqual([
      { command: 'load_remote_image', payload: { input: { messageId: 11, resourceId: 7 } } }
    ]);

    await fireEvent.click(screen.getByRole('button', { name: 'Load images' }));
    expect((await screen.findByRole('alert')).textContent).toBe('Some images could not be loaded.');
    expect(container.querySelectorAll('img.remote-message-image')).toHaveLength(1);
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
