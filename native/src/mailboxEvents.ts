/**
 * App-wide notification emitted after a transaction changes mailbox-visible
 * local state. Duplicate notifications are allowed; emitters must not send it
 * before the transaction commits.
 */
export const MAILBOX_CHANGED_EVENT = 'mux://mailbox-changed' as const;

export const MAILBOX_CHANGE_SOURCES = ['operation-worker', 'provider-ingest'] as const;

export type MailboxChangeSource = (typeof MAILBOX_CHANGE_SOURCES)[number];

export type MailboxChangedPayload = {
  source: MailboxChangeSource;
};

export function isMailboxChangedPayload(payload: unknown): payload is MailboxChangedPayload {
  if (!payload || typeof payload !== 'object') return false;
  const source = (payload as { source?: unknown }).source;
  return typeof source === 'string'
    && (MAILBOX_CHANGE_SOURCES as readonly string[]).includes(source);
}
