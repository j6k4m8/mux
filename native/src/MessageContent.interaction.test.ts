import { render, screen } from '@testing-library/svelte';
import { describe, expect, test } from 'vitest';
import MessageFrame from './MessageFrame.svelte';
import { MESSAGE_FRAME_SANDBOX, measuredFrameHeight, messageFrameDocument } from './messageFrame';

const theme = {
  text: '#111', muted: '#666', background: 'transparent', link: '#00f',
  border: '#ccc', fontFamily: 'serif'
};

/// The stylesheet the frame writes ahead of the message, without the message.
function frameStylesheet(document: string): string {
  return document.slice(document.indexOf('<style>'), document.indexOf('</style>'));
}

describe('the message reader frame', () => {
  test('withholds every capability a message could act through', () => {
    render(MessageFrame, { bodyHtml: '<p>Hello</p>', label: 'Message from Ada' });
    const frame = screen.getByTitle('Message from Ada') as HTMLIFrameElement;

    // Scripting is impossible by construction rather than by sanitizing: the
    // frame is never granted it, so a missed handler cannot run.
    expect(frame.getAttribute('sandbox')).toBe(MESSAGE_FRAME_SANDBOX);
    const granted = (frame.getAttribute('sandbox') ?? '').split(/\s+/u);
    for (const capability of [
      'allow-scripts',
      'allow-popups',
      'allow-forms',
      'allow-modals',
      'allow-top-navigation',
      'allow-top-navigation-by-user-activation',
      'allow-downloads',
      'allow-pointer-lock',
      'allow-presentation'
    ]) {
      expect(granted.includes(capability), capability).toBe(false);
    }
    expect(frame.hasAttribute('src')).toBe(false);
  });

  test('renders the message as its own document rather than into the application', () => {
    const document = messageFrameDocument('<p>Read the <a href="https://example.com/plan">plan</a>.</p>', {}, theme);
    expect(document.startsWith('<!doctype html>')).toBe(true);
    expect(document).toContain('<a href="https://example.com/plan">plan</a>');
    // The frame supplies the only styling context, so nothing leaks either way.
    expect(document).toContain('font-family: serif');
  });

  test('the interface text size and density stop at the frame', () => {
    // The reader's setting scales the interface. A message is a document the
    // sender laid out, so it gets no scale, no density, and no size of Mux's
    // own: a body that names no size renders at the engine's default, and one
    // that names sizes keeps them. The frame's stylesheet may still say how
    // big a heading is relative to the body — that is a default any client
    // supplies — but never in an absolute unit.
    const styles = frameStylesheet(messageFrameDocument('<p>x</p>', {}, theme));
    expect(styles).not.toContain('--ui-scale');
    expect(styles).not.toContain('--text-');
    expect(styles).not.toMatch(/density/u);
    expect(styles).not.toMatch(/font-size\s*:\s*[\d.]+\s*(px|pt|rem)/u);
    const body = /body\s*\{[^}]*\}/u.exec(styles)?.[0] ?? '';
    expect(body).not.toContain('font-size');

    // A message that sets its own size is passed through untouched.
    const sized = messageFrameDocument('<p style="font-size: 9px">tiny by design</p>', {}, theme);
    expect(sized).toContain('<p style="font-size: 9px">tiny by design</p>');

    // The mounted frame reads colours and the typeface off the root and no
    // more, so the same message builds the same document at any text size.
    render(MessageFrame, { bodyHtml: '<p>Hello</p>', label: 'At one times' });
    const before = (screen.getByTitle('At one times') as HTMLIFrameElement).getAttribute('srcdoc');
    document.documentElement.style.setProperty('--ui-scale', '1.5');
    render(MessageFrame, { bodyHtml: '<p>Hello</p>', label: 'At one and a half' });
    const after = (screen.getByTitle('At one and a half') as HTMLIFrameElement).getAttribute('srcdoc');
    document.documentElement.style.removeProperty('--ui-scale');
    expect(after).toBe(before);
    expect(after).not.toContain('--ui-scale');
  });

  test('markup a message supplies is never treated as application markup', () => {
    // Whatever arrives is placed in the frame verbatim; it was reduced to inert
    // markup at ingest, and the frame cannot execute it in any case.
    const document = messageFrameDocument('<p onclick="steal()">x</p>', {}, theme);
    expect(document).toContain('<body><p onclick="steal()">x</p>');
    expect(document.indexOf('<body>')).toBeGreaterThan(document.indexOf('</style>'));
  });

  test('the document cannot take its height from the frame that measures it', () => {
    const document = messageFrameDocument('<p>x</p>', {}, theme);
    // A sender writing `body { height: 100% !important }` would otherwise make
    // the measured height grow on every pass. Winning that needs the last word,
    // so this rule sits after the message rather than before it.
    const containment = document.lastIndexOf('height:auto!important');
    expect(containment).toBeGreaterThan(document.indexOf('<p>x</p>'));
    expect(document.slice(containment)).toContain('</body></html>');
  });

  test('an appearance setting cannot write rules into the frame', () => {
    // The font is free text the reader typed into settings, so it reaches here
    // unvalidated and lands inside a stylesheet.
    const hostile = messageFrameDocument('<p>x</p>', {}, {
      ...theme,
      fontFamily: 'x; } body { background: url(https://tracker.test/p) } .a {',
      link: 'red; position: fixed'
    });
    expect(hostile).not.toContain('tracker.test');
    expect(hostile).not.toContain('position: fixed');
    // The rejected values fall back rather than leaving a broken declaration.
    expect(hostile).toContain('font-family: ui-sans-serif, -apple-system, sans-serif');

    // An ordinary font stack, including quoted names, still passes through.
    const normal = messageFrameDocument('<p>x</p>', { }, {
      ...theme,
      fontFamily: 'Inter, ui-sans-serif, -apple-system, sans-serif'
    });
    expect(normal).toContain('font-family: Inter, ui-sans-serif, -apple-system, sans-serif');
  });

  test('height comes from the framed content, not from the frame itself', () => {
    expect(measuredFrameHeight(null)).toBeNull();
    expect(measuredFrameHeight(undefined)).toBeNull();

    // The document element fills whatever height the frame was given, so a
    // frame that once measured tall would never shrink again if it were used.
    const stretched = {
      documentElement: { scrollHeight: 1_000 },
      body: { scrollHeight: 384 },
      defaultView: { getComputedStyle: () => ({ marginTop: '0px', marginBottom: '0px' }) }
    };
    expect(measuredFrameHeight(stretched as unknown as Document)).toBe(384);

    // A body margin is outside the body's own scroll height but inside the frame.
    const withMargin = {
      documentElement: { scrollHeight: 0 },
      body: { scrollHeight: 100 },
      defaultView: { getComputedStyle: () => ({ marginTop: '8px', marginBottom: '12.5px' }) }
    };
    expect(measuredFrameHeight(withMargin as unknown as Document)).toBe(121);

    const runaway = {
      documentElement: { scrollHeight: 0 },
      body: { scrollHeight: 5_000_000 },
      defaultView: { getComputedStyle: () => ({ marginTop: '0px', marginBottom: '0px' }) }
    };
    expect(measuredFrameHeight(runaway as unknown as Document)).toBe(200_000);
  });
});
