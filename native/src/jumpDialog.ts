/// What "go to" can go to. Every row is one destination the mailbox already
/// knows how to open, so the dialog stays a list and the app keeps the
/// navigation.

import { mailboxViews, smartViews } from './mailboxViews';
import type { AccountSummary, MailboxView, SmartView } from './types';

export type JumpTarget =
  | { kind: 'view'; view: MailboxView; accountId: string | null }
  | { kind: 'smart'; smart: Exclude<SmartView, ''>; query: string; accountId: string | null }
  | { kind: 'account'; accountId: string | null };

/// What a filtered list dialog needs to draw one row. The command palette and
/// the jump dialog are the same widget over different rows.
export type ListRow = {
  id: string;
  title: string;
  subtitle: string;
  dot?: string;
  chip?: string;
  /// Rows for the account being read sort above the rest on an equal match.
  priority?: boolean;
};

export type JumpRow = ListRow & { target: JumpTarget };

export type JumpRowOptions = {
  accounts: AccountSummary[];
  /// The account being read. Its rows come first.
  selectedAccount: string | null;
  /// Set to limit the list to one account, which is what ⇧G does.
  scopeAccountId?: string | null;
  scoped?: boolean;
};

const ALL_ACCOUNTS = 'All accounts';

export function mailboxJumpRows(options: JumpRowOptions): JumpRow[] {
  const { accounts, selectedAccount } = options;
  const scopes: Array<{ id: string | null; label: string; detail: string; dot?: string }> = [];
  if (options.scoped) {
    const scope = accounts.find((account) => account.id === options.scopeAccountId) ?? null;
    scopes.push(scope
      ? { id: scope.id, label: scope.name, detail: scope.email, dot: scope.color }
      : { id: null, label: ALL_ACCOUNTS, detail: 'Every account together' });
  } else {
    scopes.push({ id: null, label: ALL_ACCOUNTS, detail: 'Every account together' });
    for (const account of accounts) {
      scopes.push({ id: account.id, label: account.name, detail: account.email, dot: account.color });
    }
  }

  const rows: JumpRow[] = [];
  for (const scope of scopes) {
    const priority = scope.id === selectedAccount;
    for (const view of mailboxViews) {
      rows.push({
        id: `view:${scope.id ?? 'all'}:${view.id}`,
        title: view.label,
        subtitle: scope.id === null ? ALL_ACCOUNTS : `${scope.label} · ${scope.detail}`,
        dot: scope.dot,
        priority,
        target: { kind: 'view', view: view.id, accountId: scope.id }
      });
    }
    for (const smart of smartViews) {
      rows.push({
        id: `smart:${scope.id ?? 'all'}:${smart.id}`,
        title: smart.label,
        subtitle: scope.id === null ? ALL_ACCOUNTS : `${scope.label} · ${scope.detail}`,
        dot: scope.dot,
        chip: smart.query,
        priority,
        target: { kind: 'smart', smart: smart.id, query: smart.query, accountId: scope.id }
      });
    }
  }

  if (options.scoped) return rows;

  rows.push({
    id: 'account:all',
    title: ALL_ACCOUNTS,
    subtitle: 'Read every account together',
    priority: selectedAccount === null,
    target: { kind: 'account', accountId: null }
  });
  for (const account of accounts) {
    rows.push({
      id: `account:${account.id}`,
      title: account.name,
      subtitle: account.email,
      dot: account.color,
      priority: account.id === selectedAccount,
      target: { kind: 'account', accountId: account.id }
    });
  }
  return rows;
}
