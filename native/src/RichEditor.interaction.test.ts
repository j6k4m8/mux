import { fireEvent, render, screen } from '@testing-library/svelte';
import userEvent from '@testing-library/user-event';
import { describe, expect, test, vi } from 'vitest';
import RichEditor from './RichEditor.svelte';

function selectEditorText(editor: HTMLElement) {
  const textNode = editor.querySelector('p')?.firstChild;
  if (!textNode) throw new Error('Rich editor fixture did not render its paragraph');
  const range = document.createRange();
  range.selectNodeContents(textNode);
  const selection = window.getSelection();
  selection?.removeAllRanges();
  selection?.addRange(range);
}

describe('production rich-text editor interactions', () => {
  test('rejects unsafe link schemes and applies a normalized safe link', async () => {
    const user = userEvent.setup();
    render(RichEditor, { value: '<p>Visit example</p>' });
    const editor = screen.getByRole('textbox', { name: 'Message body' });
    selectEditorText(editor);

    await fireEvent.mouseDown(screen.getByRole('button', { name: 'Insert link (Command K)' }));
    const linkInput = await screen.findByRole('textbox', { name: 'Link destination' });
    await user.type(linkInput, 'javascript:alert(1)');
    await user.click(screen.getByRole('button', { name: 'Apply' }));

    expect(screen.getByRole('alert').textContent).toBe('Use an http, https, or mailto link.');
    expect(document.execCommand).not.toHaveBeenCalledWith('createLink', false, expect.any(String));

    await user.clear(linkInput);
    await user.type(linkInput, 'https://example.com/path');
    await user.click(screen.getByRole('button', { name: 'Apply' }));

    expect(vi.mocked(document.execCommand)).toHaveBeenCalledWith('createLink', false, 'https://example.com/path');
    expect(document.activeElement).toBe(editor);
  });

  test('exposes one toolbar tab stop and keyboard-operable list formatting', async () => {
    const user = userEvent.setup();
    render(RichEditor, { value: '<p>Keyboard formatting</p>' });
    const editor = screen.getByRole('textbox', { name: 'Message body' });
    const bold = screen.getByRole('button', { name: 'Bold (Command B)' });
    const bulleted = screen.getByRole('button', { name: 'Bulleted list' });

    bold.focus();
    await user.keyboard('{ArrowRight}{ArrowRight}{ArrowRight}');
    expect(document.activeElement).toBe(bulleted);

    await user.tab();
    expect(document.activeElement).toBe(editor);

    bulleted.focus();
    await user.keyboard('{Enter}');
    expect(vi.mocked(document.execCommand)).toHaveBeenCalledWith('insertUnorderedList', false);
    expect(document.activeElement).toBe(editor);
  });
});
