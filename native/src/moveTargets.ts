/// Where a conversation can be moved, and nowhere else.
///
/// A move flips a flag on a thread that already belongs to one account, so
/// there is no such thing as a cross-account move here and none is ever built:
/// every destination is described by the thread's own account, and the account
/// is read from the thread rather than passed in beside it.
///
/// One row is one operation. Reaching some places takes two flag changes, and
/// two journal entries for one gesture would leave the undo toast able to take
/// back only half of it, so those are not offered.

export type MoveAction = 'restore' | 'archive' | 'delete' | 'untrash';

export type MoveDestination = {
  id: string;
  title: string;
  subtitle: string;
  /// The account's own colour, so the row says whose folder this is the way
  /// every other account-bound row in the app does.
  dot?: string;
  action: MoveAction;
  notice: string;
};

export type MovableThread = {
  accountId: string;
  inInbox: boolean;
};

export type MoveContext = {
  /// Trash is a flag rather than a folder, and a thread summary does not carry
  /// it; the mailbox knows it from the view the thread was listed in.
  trashed: boolean;
  /// How the thread's own account reads. Only ever the thread's own.
  accountLabel: string;
  accountColor?: string;
};

export function moveDestinations(thread: MovableThread, context: MoveContext): MoveDestination[] {
  const subtitle = context.accountLabel;
  const dot = context.accountColor;
  if (context.trashed) {
    // Lifting the trash flag returns it wherever it already belonged, so the
    // row says which of the two that is instead of guessing for the reader.
    const home = thread.inInbox
      ? { id: 'inbox', title: 'Inbox' }
      : { id: 'archive', title: 'Archive' };
    return [{ ...home, subtitle, dot, action: 'untrash', notice: `Moved to ${home.title}` }];
  }
  const destinations: MoveDestination[] = thread.inInbox
    ? [{ id: 'archive', title: 'Archive', subtitle, dot, action: 'archive', notice: 'Moved to Archive' }]
    : [{ id: 'inbox', title: 'Inbox', subtitle, dot, action: 'restore', notice: 'Moved to Inbox' }];
  destinations.push({ id: 'trash', title: 'Trash', subtitle, dot, action: 'delete', notice: 'Moved to Trash' });
  return destinations;
}
