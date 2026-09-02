/// Every keyboard shortcut, in one table. The dispatcher and the shortcut sheet
/// both read it, so a key cannot be documented without working, and cannot work
/// without being documented.

export type MailboxShortcut =
  | 'next-thread' | 'previous-thread' | 'open-thread' | 'archive' | 'toggle-star'
  | 'snooze' | 'toggle-unread' | 'move' | 'reply' | 'reply-all' | 'forward'
  | 'compose' | 'go-to' | 'go-to-account' | 'shortcuts';

/// Chords reach past a focused field: searching and the palette have to be
/// available while someone is typing in one.
export type ChordShortcut = 'search' | 'palette' | 'settings' | 'shortcuts';

export type ShortcutAction = MailboxShortcut | ChordShortcut;

/// `hidden` bindings work but are left out of the sheet: a second spelling of
/// the same chord is clutter, not documentation.
export type ShortcutBinding = { key: string; shift?: boolean; command?: boolean; hidden?: boolean };

export type ShortcutGroup = 'Navigation' | 'Conversation' | 'Writing' | 'Application';

export type ShortcutDefinition = {
  action: ShortcutAction;
  title: string;
  description: string;
  group: ShortcutGroup;
  /// Every accepted chord. The first is the one the sheet and the palette show.
  bindings: ShortcutBinding[];
};

export const shortcutGroups: ShortcutGroup[] = ['Navigation', 'Conversation', 'Writing', 'Application'];

export const shortcutCatalog: ShortcutDefinition[] = [
  {
    action: 'next-thread',
    title: 'Next conversation',
    description: 'Move down the list.',
    group: 'Navigation',
    bindings: [{ key: 'j' }]
  },
  {
    action: 'previous-thread',
    title: 'Previous conversation',
    description: 'Move up the list.',
    group: 'Navigation',
    bindings: [{ key: 'k' }]
  },
  {
    action: 'open-thread',
    title: 'Open conversation',
    description: 'Move focus into the reader.',
    group: 'Navigation',
    bindings: [{ key: 'Enter' }]
  },
  {
    action: 'go-to',
    title: 'Go to folder',
    description: 'Jump to any folder in any account.',
    group: 'Navigation',
    bindings: [{ key: 'g' }]
  },
  {
    action: 'go-to-account',
    title: 'Go to folder (this account)',
    description: 'Jump within the account you are reading.',
    group: 'Navigation',
    bindings: [{ key: 'g', shift: true }]
  },
  {
    action: 'search',
    title: 'Search mail',
    description: 'Focus the search field.',
    group: 'Navigation',
    bindings: [{ key: 'f', command: true }]
  },
  {
    action: 'palette',
    title: 'Command palette',
    description: 'Run any command by name.',
    group: 'Navigation',
    bindings: [{ key: 'k', command: true }, { key: 'p', command: true, hidden: true }]
  },
  {
    action: 'archive',
    title: 'Archive',
    description: 'Archive the conversation, or move it back to the inbox.',
    group: 'Conversation',
    bindings: [{ key: 'e' }]
  },
  {
    action: 'snooze',
    title: 'Snooze',
    description: 'Hide it until a time you pick.',
    group: 'Conversation',
    bindings: [{ key: 'b' }]
  },
  {
    action: 'toggle-star',
    title: 'Star',
    description: 'Add or remove the star.',
    group: 'Conversation',
    bindings: [{ key: 's' }]
  },
  {
    action: 'toggle-unread',
    title: 'Unread',
    description: 'Mark read or unread.',
    group: 'Conversation',
    bindings: [{ key: 'u' }]
  },
  {
    action: 'move',
    title: 'Move to folder',
    description: 'Move it within its own account.',
    group: 'Conversation',
    bindings: [{ key: 'm' }]
  },
  {
    action: 'compose',
    title: 'Compose',
    description: 'Start a new message.',
    group: 'Writing',
    bindings: [{ key: 'c' }]
  },
  {
    action: 'reply',
    title: 'Reply',
    description: 'Reply to the last message.',
    group: 'Writing',
    bindings: [{ key: 'r' }]
  },
  {
    action: 'reply-all',
    title: 'Reply all',
    description: 'Reply to everyone on the thread.',
    group: 'Writing',
    bindings: [{ key: 'a' }]
  },
  {
    action: 'forward',
    title: 'Forward',
    description: 'Forward the conversation.',
    group: 'Writing',
    bindings: [{ key: 'f' }]
  },
  {
    action: 'settings',
    title: 'Settings',
    description: 'Accounts, appearance, and mail data.',
    group: 'Application',
    bindings: [{ key: ',', command: true }]
  },
  {
    action: 'shortcuts',
    title: 'Keyboard shortcuts',
    description: 'Show this list.',
    group: 'Application',
    bindings: [{ key: '/', shift: true }, { key: '?', shift: true, hidden: true }, { key: '/', command: true }]
  }
];

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

/// Structural rather than a full KeyboardEvent: these six fields are all this
/// reads, and saying so lets it be exercised without synthesising DOM events.
export type ShortcutEvent = {
  key: string;
  metaKey?: boolean;
  ctrlKey?: boolean;
  altKey?: boolean;
  shiftKey?: boolean;
  target?: unknown;
};

function matches(binding: ShortcutBinding, event: ShortcutEvent, command: boolean): boolean {
  return Boolean(binding.command) === command
    && Boolean(binding.shift) === Boolean(event.shiftKey)
    && binding.key.toLocaleLowerCase() === event.key.toLocaleLowerCase();
}

function actionFor(event: ShortcutEvent, command: boolean): ShortcutAction | null {
  for (const definition of shortcutCatalog) {
    if (definition.bindings.some((binding) => matches(binding, event, command))) return definition.action;
  }
  return null;
}

const chordActions = new Set<ShortcutAction>(['search', 'palette', 'settings', 'shortcuts']);
const mailboxActions = new Set<ShortcutAction>([
  'next-thread', 'previous-thread', 'open-thread', 'archive', 'toggle-star', 'snooze',
  'toggle-unread', 'move', 'reply', 'reply-all', 'forward', 'compose', 'go-to',
  'go-to-account', 'shortcuts'
]);

/// Bare keys, and only when nothing else is listening for them.
export function mailboxShortcutFor(event: ShortcutEvent): MailboxShortcut | null {
  if (event.key === 'Tab' || event.metaKey || event.ctrlKey || event.altKey) return null;
  if (isInteractiveShortcutTarget(event.target)) return null;
  const action = actionFor(event, false);
  return action !== null && mailboxActions.has(action) ? (action as MailboxShortcut) : null;
}

/// Command chords, which stay live inside text fields.
export function chordShortcutFor(event: ShortcutEvent): ChordShortcut | null {
  if (event.altKey || !(event.metaKey || event.ctrlKey)) return null;
  const action = actionFor(event, true);
  return action !== null && chordActions.has(action) ? (action as ChordShortcut) : null;
}

const keyLabels: Record<string, string> = {
  enter: '↩',
  escape: 'esc',
  arrowup: '↑',
  arrowdown: '↓'
};

/// One chip per key, the way the sheet stacks them: ['⌘', 'K'].
export function shortcutChips(binding: ShortcutBinding): string[] {
  const chips: string[] = [];
  if (binding.command) chips.push('⌘');
  if (binding.shift) chips.push('⇧');
  const lowered = binding.key.toLocaleLowerCase();
  chips.push(keyLabels[lowered] ?? (binding.key.length === 1 ? binding.key.toLocaleUpperCase() : binding.key));
  return chips;
}

/// The compact form, for rows too narrow to stack chips.
export function shortcutLabel(action: ShortcutAction): string {
  const binding = shortcutCatalog.find((definition) => definition.action === action)?.bindings[0];
  return binding ? shortcutChips(binding).join('') : '';
}

export function shortcutsInGroup(group: ShortcutGroup): ShortcutDefinition[] {
  return shortcutCatalog.filter((definition) => definition.group === group);
}

export function visibleBindings(definition: ShortcutDefinition): ShortcutBinding[] {
  return definition.bindings.filter((binding) => !binding.hidden);
}
