import { mockIPC } from '@tauri-apps/api/mocks';
import { fireEvent, render, screen, waitFor } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { describe, expect, test } from 'vitest';
import RichText from './RichText.svelte';
import { parseMessageRichText, safeMessageHref } from './richText';

describe('hostile message links', () => {
  test('keeps only normalized absolute HTTP(S) destinations in controlled render nodes', () => {
    expect(safeMessageHref(' HTTPS://Example.COM:443/a/../plan?q=1 ')).toBe('https://example.com/plan?q=1');
    expect(safeMessageHref('https://example.com/%E2%9C%93')).toBe('https://example.com/%E2%9C%93');
    for (const denied of [
      'javascript:alert(1)',
      'java\nscript:alert(1)',
      'mailto:person@example.com',
      'file:///etc/passwd',
      '//example.com/path',
      'https://user:secret@example.com/',
      'https://example.com/%0aheader',
      'https://example.com/%C2%85control',
      'https://example.com/%zz',
      'https://example.com/a b',
      'example.com/path'
    ]) {
      expect(safeMessageHref(denied), denied).toBeNull();
    }

    const nodes = parseMessageRichText(`
      <p onclick="steal()">
        <a href="javascript:alert(1)">bad</a>
        <a href="https://EXAMPLE.com:443/a/../ok">safe</a>
        <img src="https://tracker.test/pixel" onerror="steal()">
      </p>
    `);
    const serialized = JSON.stringify(nodes);
    expect(serialized).toContain('https://example.com/ok');
    expect(serialized).not.toMatch(/javascript|onclick|onerror|tracker|img/iu);
  });

  test('cancels WebView navigation and invokes only the typed message-link command', async () => {
    const calls: Array<{ command: string; payload: unknown }> = [];
    mockIPC((command, payload) => {
      calls.push({ command, payload });
      if (command === 'open_message_link') return { destination: 'https://example.com/plan' };
      throw new Error(`unexpected command ${command}`);
    });
    render(RichText, {
      nodes: parseMessageRichText('<p>Read the <a href="HTTPS://Example.COM:443/a/../plan">plan</a>.</p>')
    });

    const link = screen.getByRole('link', { name: 'plan' });
    expect(link.getAttribute('href')).toBe('https://example.com/plan');
    expect(link.hasAttribute('target')).toBe(false);
    expect(await fireEvent.click(link)).toBe(false);
    await waitFor(() => expect(calls).toEqual([
      { command: 'open_message_link', payload: { destination: 'https://example.com/plan' } }
    ]));
  });

  test('keyboard activation uses the same explicit command and reports native denial', async () => {
    const user = userEvent.setup();
    mockIPC((command) => {
      if (command === 'open_message_link') throw new Error('denied');
      throw new Error(`unexpected command ${command}`);
    });
    render(RichText, { nodes: parseMessageRichText('<a href="https://example.com/">Example</a>') });

    await user.tab();
    expect(document.activeElement).toBe(screen.getByRole('link', { name: 'Example' }));
    await user.keyboard('{Enter}');
    expect((await screen.findByRole('alert')).textContent).toBe('Mux could not open that link.');
  });
});
