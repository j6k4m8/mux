export type RichNode =
  | { type: 'text'; text: string }
  | { type: 'element'; tag: 'p' | 'div' | 'strong' | 'em' | 'u' | 'ul' | 'ol' | 'li' | 'blockquote' | 'br' | 'a'; href?: string; children: RichNode[] }
  | { type: 'remote-image'; resourceId: number };

const allowedTags = new Set(['p', 'div', 'strong', 'b', 'em', 'i', 'u', 'ul', 'ol', 'li', 'blockquote', 'br', 'a']);
const MAX_RENDER_HTML_UNITS = 8 * 1024 * 1024 + 1024;
const MAX_RENDER_TREE_DEPTH = 64;
const MAX_RENDER_TREE_NODES = 50_000;
const MAX_REMOTE_IMAGE_RESOURCE_ID = 64;
const MAX_REMOTE_IMAGE_DATA_URL_UNITS = 8 * 1024 * 1024 + 128;

/// Plain-text bodies carry their own structure in newlines. Any run of blank
/// lines is one paragraph break, so "a\n\n\n\nb" reads the same as "a\n\nb";
/// single newlines stay as line breaks inside the paragraph.
export function plainTextParagraphs(value: string): string[][] {
  return value
    .replace(/\r\n?/gu, '\n')
    .split(/\n[ \t]*\n+/u)
    .map((paragraph) => paragraph.split('\n').map((line) => line.trimEnd()))
    .filter((lines) => lines.some((line) => line.trim().length > 0));
}

export function safeRemoteImageDataUrl(value: string): string | null {
  return value.length <= MAX_REMOTE_IMAGE_DATA_URL_UNITS
    && /^data:image\/(?:gif|jpeg|png|webp);base64,[a-z0-9+/]+={0,2}$/iu.test(value)
    ? value
    : null;
}

export function safeHref(value: string): string | null {
  const trimmed = value.trim();
  if (!trimmed) return null;
  const candidate = /^(https?:|mailto:)/iu.test(trimmed) ? trimmed : `https://${trimmed}`;
  try {
    const url = new URL(candidate);
    return ['http:', 'https:', 'mailto:'].includes(url.protocol) ? url.href : null;
  } catch {
    return null;
  }
}

function hasPercentEncodedControl(value: string): boolean {
  try {
    return /[\u0000-\u001f\u007f-\u009f]/u.test(decodeURIComponent(value));
  } catch {
    return true;
  }
}

/**
 * Message bodies use a stricter link contract than the composer. Incoming
 * content may name only an absolute HTTP(S) destination; it never gets the
 * editor's convenient scheme inference and never turns mailto into ambient
 * navigation authority.
 */
export function safeMessageHref(value: string): string | null {
  const trimmed = value.trim();
  if (
    !trimmed
    || new TextEncoder().encode(trimmed).byteLength > 2_048
    || /[\s\u0000-\u001f\u007f-\u009f]/u.test(trimmed)
    || hasPercentEncodedControl(trimmed)
  ) return null;
  try {
    const url = new URL(trimmed);
    if (!['http:', 'https:'].includes(url.protocol) || !url.hostname || url.username || url.password) return null;
    return url.href;
  } catch {
    return null;
  }
}

type ParseBudget = { visited: number };

function convertNode(
  node: Node,
  linkPolicy: (value: string) => string | null,
  budget: ParseBudget,
  depth: number,
  allowRemoteImageMarkers = false
): RichNode[] {
  if (budget.visited >= MAX_RENDER_TREE_NODES || depth > MAX_RENDER_TREE_DEPTH) return [];
  budget.visited += 1;
  if (node.nodeType === Node.TEXT_NODE) {
    return [{ type: 'text', text: node.textContent ?? '' }];
  }
  if (!(node instanceof HTMLElement)) return [];
  const rawTag = node.tagName.toLocaleLowerCase();
  if (allowRemoteImageMarkers && rawTag === 'mux-remote-image') {
    const rawId = node.getAttribute('data-id') ?? '';
    const resourceId = /^(?:0|[1-9]\d*)$/u.test(rawId) ? Number(rawId) : Number.NaN;
    return Number.isSafeInteger(resourceId) && resourceId >= 1 && resourceId <= MAX_REMOTE_IMAGE_RESOURCE_ID
      ? [{ type: 'remote-image', resourceId }]
      : [];
  }
  const children = Array.from(node.childNodes).flatMap((child) => convertNode(child, linkPolicy, budget, depth + 1, allowRemoteImageMarkers));
  if (!allowedTags.has(rawTag)) return children;
  const tag = rawTag === 'b' ? 'strong' : rawTag === 'i' ? 'em' : rawTag;
  if (tag === 'a') {
    const href = linkPolicy(node.getAttribute('href') ?? '');
    return href ? [{ type: 'element', tag, href, children }] : children;
  }
  return [{ type: 'element', tag: tag as RichNode & string, children } as RichNode];
}

export function parseRichText(value: string): RichNode[] {
  if (!value.trim() || typeof DOMParser === 'undefined') return [];
  const document = new DOMParser().parseFromString(value, 'text/html');
  const budget: ParseBudget = { visited: 0 };
  return Array.from(document.body.childNodes).flatMap((node) => convertNode(node, safeHref, budget, 0));
}

export function parseMessageRichText(value: string): RichNode[] {
  if (!value.trim() || typeof DOMParser === 'undefined') return [];
  const document = new DOMParser().parseFromString(value.slice(0, MAX_RENDER_HTML_UNITS), 'text/html');
  const budget: ParseBudget = { visited: 0 };
  return Array.from(document.body.childNodes).flatMap((node) => convertNode(node, safeMessageHref, budget, 0, true));
}

function escapeText(value: string): string {
  return value.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;');
}

function escapeAttribute(value: string): string {
  return escapeText(value).replaceAll('"', '&quot;');
}

function serializeNode(node: RichNode): string {
  if (node.type === 'text') return escapeText(node.text);
  if (node.type === 'remote-image') return '';
  if (node.tag === 'br') return '<br>';
  const content = node.children.map(serializeNode).join('');
  if (node.tag === 'a' && node.href) {
    return `<a href="${escapeAttribute(node.href)}">${content}</a>`;
  }
  return `<${node.tag}>${content}</${node.tag}>`;
}

export function normalizeRichHtml(value: string): string {
  return parseRichText(value).map(serializeNode).join('');
}

export function plainTextToRichHtml(value: string): string {
  if (!value) return '';
  return value
    .split(/\n{2,}/gu)
    .map((paragraph) => `<p>${escapeText(paragraph).replaceAll('\n', '<br>')}</p>`)
    .join('');
}

export function richTextToPlain(value: string): string {
  if (typeof DOMParser === 'undefined') return value;
  const document = new DOMParser().parseFromString(normalizeRichHtml(value), 'text/html');
  for (const breakElement of document.querySelectorAll('br')) breakElement.replaceWith('\n');
  for (const block of document.querySelectorAll('p, div, li, blockquote')) block.append('\n');
  return (document.body.textContent ?? '')
    .replaceAll('\u00a0', ' ')
    .replace(/\n{3,}/gu, '\n\n')
    .trimEnd();
}
