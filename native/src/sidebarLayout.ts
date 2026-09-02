/// What the sidebar is showing: how wide it is, and which accounts' folders are
/// open. Both are the reader's own arrangement of the rail rather than mail
/// data, so both live on this Mac next to the appearance choices — and both are
/// read back defensively, because storage is untrusted input.

export const SIDEBAR_KEY = 'mux-sidebar';

const MAX_REMEMBERED_ACCOUNTS = 64;
const MAX_ACCOUNT_ID_LENGTH = 200;

export type SidebarLayout = {
  collapsed: boolean;
  /// Accounts whose folder section is open. Folders start folded away: an
  /// account can have a great many, and the fixed views are what the rail is
  /// mostly for.
  openFolderAccounts: string[];
};

export const DEFAULT_SIDEBAR_LAYOUT: SidebarLayout = { collapsed: false, openFolderAccounts: [] };

export function readSidebarLayout(): SidebarLayout {
  let raw: string | null = null;
  try {
    raw = window.localStorage.getItem(SIDEBAR_KEY);
  } catch {
    return { ...DEFAULT_SIDEBAR_LAYOUT };
  }
  if (!raw) return { ...DEFAULT_SIDEBAR_LAYOUT };
  try {
    const parsed: unknown = JSON.parse(raw);
    if (typeof parsed !== 'object' || parsed === null) return { ...DEFAULT_SIDEBAR_LAYOUT };
    const value = parsed as Partial<SidebarLayout>;
    const accounts = Array.isArray(value.openFolderAccounts) ? value.openFolderAccounts : [];
    const openFolderAccounts: string[] = [];
    for (const entry of accounts) {
      if (typeof entry !== 'string' || !entry || entry.length > MAX_ACCOUNT_ID_LENGTH) continue;
      if (openFolderAccounts.includes(entry)) continue;
      openFolderAccounts.push(entry);
      if (openFolderAccounts.length === MAX_REMEMBERED_ACCOUNTS) break;
    }
    return { collapsed: value.collapsed === true, openFolderAccounts };
  } catch {
    return { ...DEFAULT_SIDEBAR_LAYOUT };
  }
}

export function persistSidebarLayout(layout: SidebarLayout): void {
  try {
    window.localStorage.setItem(SIDEBAR_KEY, JSON.stringify(layout));
  } catch {
    // Persistence is optional; the arrangement still holds for this session.
  }
}

export function toggleFolderAccount(layout: SidebarLayout, accountId: string): SidebarLayout {
  const open = layout.openFolderAccounts.includes(accountId);
  return {
    ...layout,
    openFolderAccounts: open
      ? layout.openFolderAccounts.filter((id) => id !== accountId)
      : [...layout.openFolderAccounts, accountId].slice(-MAX_REMEMBERED_ACCOUNTS)
  };
}

/// The section holding the folder being read is open whether or not it was
/// folded away: the rail should show where you are.
export function folderSectionIsOpen(
  openFolderAccounts: string[],
  accountId: string,
  openFolderAccountId: string | null
): boolean {
  return openFolderAccounts.includes(accountId) || accountId === openFolderAccountId;
}
