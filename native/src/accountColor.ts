/// An account's colour is the one thing in the interface that says whose mail a
/// row is. This is where it is chosen: the six colours new mailboxes are dealt,
/// offered by name, and the check every colour passes before it is sent.

/// The same six, in the same order, as `ACCOUNT_COLORS` in
/// `native/src-tauri/src/imap_access.rs`; a test there holds the two together.
export const accountColorChoices: Array<{ label: string; value: string }> = [
  { label: 'Indigo', value: '#5168f4' },
  { label: 'Teal', value: '#12a58c' },
  { label: 'Amber', value: '#b3730a' },
  { label: 'Rose', value: '#c93b63' },
  { label: 'Violet', value: '#7c4ddb' },
  { label: 'Ocean', value: '#0a6fa8' }
];

/// Exactly `#rrggbb`, lowercased, or null. The store refuses anything else and
/// the colour ends up in an inline style, so the interface checks first rather
/// than send something only to be told no.
export function normalizeAccountColor(value: unknown): string | null {
  if (typeof value !== 'string' || !/^#[0-9a-fA-F]{6}$/u.test(value)) return null;
  return value.toLowerCase();
}

/// What to call a colour in a sentence: its name when Settings offers it, and
/// the value itself when it does not.
export function describeAccountColor(color: string): string {
  const named = accountColorChoices.find((choice) => choice.value === normalizeAccountColor(color));
  return named ? named.label.toLocaleLowerCase() : color;
}
