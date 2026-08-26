import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const styles = readFileSync(new URL('../src/styles.css', import.meta.url), 'utf8');

function declarations(selector) {
  // Every declaration block whose selector list contains an exact match for `selector`.
  const blocks = [];
  const pattern = /(?<selectors>[^{}]+)\{(?<body>[^{}]*)\}/gu;
  for (const match of styles.matchAll(pattern)) {
    const selectors = match.groups.selectors
      .split(',')
      .map((value) => value.trim().replace(/\s+/gu, ' '));
    if (selectors.includes(selector)) blocks.push(match.groups.body.trim());
  }
  return blocks;
}

test('the message pane can yield its whole height to the reply card', () => {
  const blocks = declarations('.reader-scroll');
  assert.ok(blocks.length > 0, 'expected .reader-scroll rules');
  // Vertical padding on a border-box scroller floors its used height, so it cannot shrink
  // out of the reply card's way and the card gets clipped by .reader { overflow: hidden }.
  for (const body of blocks) {
    assert.doesNotMatch(body, /padding(-top|-bottom|-block)?\s*:/u, `vertical padding in: ${body}`);
  }
  assert.match(blocks[0], /min-height:\s*0/u);
  assert.match(blocks[0], /flex:\s*1 1 auto/u);
  assert.match(blocks[0], /overflow-y:\s*auto/u);
});

test('the reply card keeps an intrinsic height that no viewport unit can change', () => {
  const blocks = [
    ...declarations('.quick-reply-host'),
    ...declarations('.inline-reply'),
    ...declarations('.inline-reply.is-expanded'),
    ...declarations('.inline-reply > .editor-shell'),
    ...declarations('.inline-reply.is-expanded > .editor-shell'),
    ...declarations('.inline-reply .editor-shell.is-compact .rich-editor'),
    ...declarations('.inline-reply.is-expanded .rich-editor')
  ];
  assert.ok(blocks.length > 0, 'expected reply card rules');
  for (const body of blocks) {
    assert.doesNotMatch(body, /\d(vh|svh|lvh|dvh)\b/u, `viewport-relative height in: ${body}`);
  }
  assert.match(declarations('.quick-reply-host')[0], /flex:\s*0 0 auto/u);
});

test('the reader column nests no scroller inside the reply card', () => {
  // The text input scrolls its own content; no layout box around it may, or the reply
  // card stops being one intrinsically sized block and starts clipping itself.
  const containers = [];
  const pattern = /(?<selectors>[^{}]+)\{(?<body>[^{}]*)\}/gu;
  for (const match of styles.matchAll(pattern)) {
    if (!/overflow(-y)?\s*:\s*(auto|scroll)/u.test(match.groups.body)) continue;
    for (const selector of match.groups.selectors.split(',')) {
      const trimmed = selector.trim().replace(/\s+/gu, ' ');
      if (trimmed.endsWith('.rich-editor')) continue;
      if (/^\.(reader|reader-scroll|quick-reply-host|inline-reply)\b/u.test(trimmed)) {
        containers.push(trimmed);
      }
    }
  }
  assert.deepEqual(containers, ['.reader-scroll']);
});
