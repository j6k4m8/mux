const interactiveTags = new Set(['A', 'BUTTON', 'INPUT', 'SELECT', 'TEXTAREA']);
const interactiveRoles = new Set(['button', 'dialog', 'link', 'menu', 'menuitem', 'textbox']);

/** @param {any} target */
export function isInteractiveShortcutTarget(target) {
  if (!target || typeof target !== 'object') return false;
  if (interactiveTags.has(target.tagName) || target.isContentEditable) return true;
  const role = typeof target.getAttribute === 'function' ? target.getAttribute('role') : null;
  if (role && interactiveRoles.has(role)) return true;
  return typeof target.closest === 'function'
    && Boolean(target.closest('a, button, input, select, textarea, [contenteditable="true"], [role="dialog"]'));
}

/** @param {any} event */
export function mailboxShortcutFor(event) {
  if (event.key === 'Tab' || event.metaKey || event.ctrlKey || event.altKey) return null;
  if (isInteractiveShortcutTarget(event.target)) return null;
  switch (event.key.toLocaleLowerCase()) {
    case '/': return 'focus-filter';
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
