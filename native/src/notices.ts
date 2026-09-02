/// Toasts, and how long each kind of them lasts.
///
/// The timings used to live at every call site, which is how several notices
/// ended up with no expiry at all: a toast carrying no Undo button and no
/// deadline had nothing to dismiss it and simply stayed until something else
/// replaced it. There is one rule now — a toast expires unless it is offering
/// to undo something — and one place to change how long any of it lasts.

export type Notice = {
  text: string;
  /// Present when the toast is offering to take the work back.
  operationId?: string;
  /// When the toast goes away on its own. Absent means it waits for the reader,
  /// which is only right while an Undo button is still worth pressing.
  until?: number;
};

/// Long enough to read a confirmation, and to notice one replacing another.
export const CONFIRMATION_MS = 5_000;
/// Failures get longer: they are worth reading twice and are not expected.
export const FAILURE_MS = 10_000;
/// How long the mailbox offers to take back a change before it goes out.
export const UNDO_MS = 5_000;

export function describeFailure(cause: unknown): string {
  return cause instanceof Error ? cause.message : String(cause);
}

/// Something happened and there is nothing to take back.
export function confirmationNotice(text: string, now: number = Date.now()): Notice {
  return { text, until: now + CONFIRMATION_MS };
}

export function failureNotice(cause: unknown, now: number = Date.now()): Notice {
  return { text: describeFailure(cause), until: now + FAILURE_MS };
}

/// Work that can be taken back. `deadline` is the moment undo stops being
/// offered — the window before a change is pushed, or a queued send's own
/// not-before. Leave it out when undo stays good until something supersedes it,
/// which is how the local snooze and invitation journals behave; the toast then
/// waits, because hiding it would take the only Undo button with it.
export function undoableNotice(text: string, operationId: string, deadline?: number): Notice {
  return deadline === undefined ? { text, operationId } : { text, operationId, until: deadline };
}

export function undoDeadline(now: number = Date.now()): number {
  return now + UNDO_MS;
}

export function noticeExpired(notice: Notice | null, at: number): boolean {
  return Boolean(notice?.until && at >= notice.until);
}

export function noticeRemainingSeconds(notice: Notice | null, at: number): number {
  return notice?.until ? Math.max(0, Math.ceil((notice.until - at) / 1_000)) : 0;
}

/// The Undo button is offered while the work is still takeable back: either the
/// toast carries no deadline, or its deadline has not passed.
export function noticeOffersUndo(notice: Notice | null, at: number): boolean {
  if (!notice?.operationId) return false;
  return !notice.until || noticeRemainingSeconds(notice, at) > 0;
}
