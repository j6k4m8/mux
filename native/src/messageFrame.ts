/// The reader renders a message inside a sandboxed frame. The frame is granted
/// `allow-same-origin` so this document can be measured from the outside, and
/// is denied `allow-scripts`, so nothing inside it can run — an omission the
/// engine enforces, rather than something a sanitizer has to keep catching.
export const MESSAGE_FRAME_SANDBOX = 'allow-same-origin';

import { safeRemoteImageDataUrl } from './richText';

export type MessageFrameTheme = {
  text: string;
  muted: string;
  background: string;
  link: string;
  border: string;
  fontFamily: string;
  fontSize: string;
};

export type MessageFrameImage = { dataUrl: string | null; altText: string };

const MARKER = /<mux-remote-image data-id="(\d+)"><\/mux-remote-image>/gu;
/// Appearance values are written into the frame's stylesheet, and one of them —
/// the font — is free text the reader typed into settings. A value that could
/// close a declaration or open a rule would be writing CSS, not choosing it.
const SAFE_CSS_VALUE = /^[\w\s,.#%()+/-]+$/u;
const MAX_CSS_VALUE_UNITS = 200;
const MAX_BODY_UNITS = 8 * 1024 * 1024;

function escapeText(value: string): string {
  return value.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;');
}

function escapeAttribute(value: string): string {
  return escapeText(value).replaceAll('"', '&quot;');
}

/// Swaps each inert marker for the approved image, or for a placeholder naming
/// what is being withheld. A marker with no matching record disappears rather
/// than leaving markup the frame would render as an unknown element.
export function resolveRemoteImages(
  html: string,
  images: Record<number, MessageFrameImage>
): string {
  return html.replace(MARKER, (_match, rawId: string) => {
    const image = images[Number(rawId)];
    if (!image) return '';
    const source = safeRemoteImageDataUrl(image.dataUrl ?? '');
    if (source) {
      return `<img class="mux-remote" src="${escapeAttribute(source)}" alt="${escapeAttribute(image.altText)}" loading="lazy" decoding="async">`;
    }
    return image.altText
      ? `<span class="mux-blocked" role="img" aria-label="${escapeAttribute(image.altText)}">${escapeText(image.altText)}</span>`
      : '<span class="mux-blocked" aria-hidden="true"></span>';
  });
}

/// The appearance a frame falls back to when the application stylesheet cannot
/// be read, and the source of every replacement for a value that fails the
/// guard above.
export const FALLBACK_THEME: MessageFrameTheme = {
  text: '#5f6675',
  muted: '#8a91a0',
  background: 'transparent',
  link: '#4c63ee',
  border: '#e4e7ee',
  fontFamily: 'ui-sans-serif, -apple-system, sans-serif',
  fontSize: '13px'
};

function cssValue(value: string, fallback: string): string {
  const trimmed = value.trim();
  return trimmed
    && trimmed.length <= MAX_CSS_VALUE_UNITS
    && SAFE_CSS_VALUE.test(trimmed)
    && !/url\s*\(/iu.test(trimmed)
    ? trimmed
    : fallback;
}

function safeTheme(theme: MessageFrameTheme): MessageFrameTheme {
  return {
    text: cssValue(theme.text, FALLBACK_THEME.text),
    muted: cssValue(theme.muted, FALLBACK_THEME.muted),
    background: cssValue(theme.background, FALLBACK_THEME.background),
    link: cssValue(theme.link, FALLBACK_THEME.link),
    border: cssValue(theme.border, FALLBACK_THEME.border),
    fontFamily: cssValue(theme.fontFamily, FALLBACK_THEME.fontFamily),
    fontSize: cssValue(theme.fontSize, FALLBACK_THEME.fontSize)
  };
}

function frameStyles(raw: MessageFrameTheme): string {
  const theme = safeTheme(raw);
  // Defaults only. A message's own stylesheet is written into the document
  // after this one, so a sender that states a rule wins it. Nothing here draws
  // table borders: mail uses tables for layout far more often than for data,
  // and bordering them puts a grid over ordinary messages.
  return `
/* The frame must not paint a canvas of its own: the reader's surface shows
   through wherever the message does not state a background. Declaring support
   for a dark scheme would hand the canvas to the platform and paint it. */
html { background: transparent; color-scheme: only light; }
body {
  margin: 0;
  color: ${theme.text};
  background: ${theme.background};
  font-family: ${theme.fontFamily};
  font-size: ${theme.fontSize};
  line-height: 1.72;
  overflow-x: auto;
  overflow-y: hidden;
  overflow-wrap: anywhere;
  -webkit-text-size-adjust: none;
}
p, div { margin: 0 0 .8em; }
p:last-child, div:last-child { margin-bottom: 0; }
ul, ol { margin: .5em 0; padding-left: 1.5em; }
h1, h2, h3, h4, h5, h6 { margin: 1.1em 0 .45em; line-height: 1.3; }
h1 { font-size: 1.42em; }
h2 { font-size: 1.26em; }
h3 { font-size: 1.13em; }
h4, h5, h6 { font-size: 1em; }
h1:first-child, h2:first-child, h3:first-child { margin-top: 0; }
hr { height: 0; margin: 1.2em 0; border: 0; border-top: 1px solid ${theme.border}; }
img, video { max-width: 100%; height: auto; }
caption { margin-bottom: .4em; color: ${theme.muted}; text-align: left; }
pre {
  margin: .8em 0;
  padding: 11px 13px;
  border: 1px solid ${theme.border};
  border-radius: 9px;
  font-family: ui-monospace, "SF Mono", Menlo, monospace;
  font-size: .92em;
  line-height: 1.5;
  overflow-x: auto;
  white-space: pre-wrap;
}
code { font-family: ui-monospace, "SF Mono", Menlo, monospace; font-size: .92em; }
pre code { padding: 0; background: none; }
dl { margin: .6em 0; }
dt { font-weight: 700; }
dd { margin: 0 0 .5em 1.2em; }
del { color: ${theme.muted}; }
sub, sup { font-size: .78em; line-height: 0; }
a { color: ${theme.link}; text-decoration-thickness: 1px; text-underline-offset: 2px; }
blockquote {
  margin: .9em 0;
  padding-left: 13px;
  border-left: 2px solid ${theme.border};
  color: ${theme.muted};
}
.mux-blocked {
  display: inline-block;
  padding: 0 .35em;
  border: 1px dashed ${theme.border};
  border-radius: 4px;
  color: ${theme.muted};
  font-size: .85em;
}
`.trim();
}

/// Builds the whole frame document. The message's own markup goes in verbatim:
/// it was reduced to inert markup at ingest, and the frame cannot execute it in
/// any case.
export function messageFrameDocument(
  bodyHtml: string,
  images: Record<number, MessageFrameImage>,
  theme: MessageFrameTheme
): string {
  const body = resolveRemoteImages(bodyHtml.slice(0, MAX_BODY_UNITS), images);
  // Last in the document, so it wins the cascade against a sender's own
  // `!important`. A message that gives the document a percentage height makes
  // its content height depend on the frame's height, and the frame's height is
  // measured from its content — which grows on every pass. Refusing to let the
  // document take its height from the frame breaks that circle at the source.
  const containment = '<style>html,body{height:auto!important;min-height:0!important;max-height:none!important}</style>';
  return `<!doctype html><html><head><meta charset="utf-8"><style>${frameStyles(theme)}</style></head><body>${body}${containment}</body></html>`;
}

/// The frame's own document decides the height; the element is told what that
/// is. Measurement is clamped so a runaway document cannot produce an element
/// taller than the reader can scroll through.
export const MAX_FRAME_HEIGHT = 200_000;

export function measuredFrameHeight(document: Document | null | undefined): number | null {
  const body = document?.body;
  // The document element stretches to fill the frame, so measuring it reports
  // whatever height the frame already has and the frame can then only ever
  // grow. The body reports what the content actually needs.
  if (!body) {
    const root = document?.documentElement;
    const fallback = root?.scrollHeight ?? 0;
    return fallback > 0 ? Math.min(Math.ceil(fallback), MAX_FRAME_HEIGHT) : null;
  }
  const style = document?.defaultView?.getComputedStyle(body);
  const edge = (value: string | undefined) => {
    const parsed = Number.parseFloat(value ?? '');
    return Number.isFinite(parsed) ? parsed : 0;
  };
  const height = body.scrollHeight + edge(style?.marginTop) + edge(style?.marginBottom);
  if (!Number.isFinite(height) || height <= 0) return null;
  return Math.min(Math.ceil(height), MAX_FRAME_HEIGHT);
}
