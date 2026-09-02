/// Every dialog in Mux keeps Tab inside itself and closes on Escape. That is
/// one behaviour, so it is written once here rather than in each dialog.

const FOCUSABLE =
  'button:not(:disabled), input:not(:disabled), select:not(:disabled), textarea:not(:disabled), a[href], [tabindex]:not([tabindex="-1"])';

export function trapModalKeydown(event: KeyboardEvent, dialog: HTMLElement | undefined, close: () => void): void {
  if (event.key === 'Escape') {
    event.preventDefault();
    // The window handler would otherwise read this Escape as a second one and
    // clear whatever sits behind the dialog.
    event.stopPropagation();
    close();
    return;
  }
  if (event.key !== 'Tab' || !dialog) return;
  const focusable = [...dialog.querySelectorAll<HTMLElement>(FOCUSABLE)]
    .filter((element) => !element.hasAttribute('hidden'));
  if (!focusable.length) {
    event.preventDefault();
    dialog.focus();
    return;
  }
  const first = focusable[0];
  const last = focusable.at(-1) ?? first;
  if (event.shiftKey && document.activeElement === first) {
    event.preventDefault();
    last.focus();
  } else if (!event.shiftKey && document.activeElement === last) {
    event.preventDefault();
    first.focus();
  }
}
