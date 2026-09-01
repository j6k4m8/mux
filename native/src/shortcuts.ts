export type MailboxShortcut =
  | 'next-thread' | 'previous-thread' | 'archive' | 'toggle-star' | 'snooze'
  | 'toggle-unread' | 'reply' | 'reply-all' | 'forward' | 'compose' | 'escape';

const interactiveTags = new Set(['A', 'BUTTON', 'INPUT', 'SELECT', 'TEXTAREA']);
const interactiveRoles = new Set(['button', 'dialog', 'link', 'menu', 'menuitem', 'textbox']);

/// A single letter must not act as a shortcut while someone is typing it, or
/// while focus sits on something that already answers to keys.
export function isInteractiveShortcutTarget(target: unknown): boolean {
  if (!target || typeof target !== 'object') return false;
  const node = target as {
    tagName?: string;
    isContentEditable?: boolean;
    getAttribute?: (name: string) => string | null;
    closest?: (selector: string) => unknown;
  };
  if ((node.tagName && interactiveTags.has(node.tagName)) || node.isContentEditable) return true;
  const role = typeof node.getAttribute === 'function' ? node.getAttribute('role') : null;
  if (role && interactiveRoles.has(role)) return true;
  return typeof node.closest === 'function'
    && Boolean(node.closest('a, button, input, select, textarea, [contenteditable="true"], [role="dialog"]'));
}

/// Structural rather than a full KeyboardEvent: these five fields are all this
/// reads, and saying so lets it be exercised without synthesising DOM events.
export type ShortcutEvent = {
  key: string;
  metaKey?: boolean;
  ctrlKey?: boolean;
  altKey?: boolean;
  target?: unknown;
};

export function mailboxShortcutFor(event: ShortcutEvent): MailboxShortcut | null {
  if (event.key === 'Tab' || event.metaKey || event.ctrlKey || event.altKey) return null;
  if (isInteractiveShortcutTarget(event.target)) return null;
  switch (event.key.toLocaleLowerCase()) {
    case 'j': return 'next-thread';
    case 'k': return 'previous-thread';
    case 'e': return 'archive';
    case 's': return 'toggle-star';
    case 'h': return 'snooze';
    case 'u': return 'toggle-unread';
    case 'r': return 'reply';
    case 'a': return 'reply-all';
    case 'f': return 'forward';
    case 'c': return 'compose';
    case 'escape': return 'escape';
    default: return null;
  }
}
