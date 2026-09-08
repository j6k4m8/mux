/// What the sidebar is showing: how wide it is, whether the smart views are out,
/// and which accounts' folders are open. All of it is the reader's own
/// arrangement of the rail rather than mail data, so it lives on this Mac next
/// to the appearance choices — and is read back defensively, because storage is
/// untrusted input.

import type { SmartView } from './types';

export const SIDEBAR_KEY = 'mux-sidebar';

const MAX_REMEMBERED_ACCOUNTS = 64;
const MAX_ACCOUNT_ID_LENGTH = 200;

export type SidebarLayout = {
  collapsed: boolean;
  /// Accounts whose folder section is open. Folders start folded away: an
  /// account can have a great many, and the fixed views are what the rail is
  /// mostly for.
  openFolderAccounts: string[];
  /// Whether the smart views are shown. They start open: there are only four,
  /// and they are folded away only by a reader who asked for that.
  smartViewsOpen: boolean;
};

export const DEFAULT_SIDEBAR_LAYOUT: SidebarLayout = {
  collapsed: false,
  openFolderAccounts: [],
  smartViewsOpen: true
};

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
    // Anything but a stored `false` reads as open, so a layout written before
    // the smart views could fold keeps showing them.
    return { collapsed: value.collapsed === true, openFolderAccounts, smartViewsOpen: value.smartViewsOpen !== false };
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

export function toggleSmartViews(layout: SidebarLayout): SidebarLayout {
  return { ...layout, smartViewsOpen: !layout.smartViewsOpen };
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

/// Likewise the smart views stay out while one of them is being read: folding
/// them must not hide the row that says what the list is.
export function smartViewsSectionIsOpen(open: boolean, selectedSmartView: SmartView): boolean {
  return open || selectedSmartView !== '';
}
