export type AccountSummary = {
  id: string;
  name: string;
  email: string;
  color: string;
  signature: string;
  unread: number;
  total: number;
  refreshSeconds: number;
};

export type ThreadSummary = {
  id: number;
  accountId: string;
  subject: string;
  participants: string;
  snippet: string;
  latestAt: number;
  messageCount: number;
  inInbox: boolean;
  unread: boolean;
  starred: boolean;
  hasFromMe: boolean;
  category: string;
};

export type MessageSummary = {
  id: number;
  threadId: number;
  senderName: string;
  senderEmail: string;
  recipients: string;
  ccRecipients: string;
  bccRecipients: string;
  sentAt: number;
  bodyText: string;
  bodyHtml: string;
  blockedRemoteResources: number;
  remoteImages: RemoteImageSummary[];
  isFromMe: boolean;
};

export type RemoteImageSummary = {
  id: number;
  domain: string;
  altText: string;
  allowedByPolicy: boolean;
};

export type RemoteImageContent = {
  messageId: number;
  resourceId: number;
  dataUrl: string;
};

export type AttachmentSummary = {
  id: string;
  messageId: number;
  filename: string;
  mediaType: string;
  byteLength: number;
  contentId: string;
  disposition: 'inline' | 'attachment';
};

export type AttachmentContent = {
  filename: string;
  mediaType: string;
  byteLength: number;
  dataBase64: string;
};

export type InvitationSummary = {
  threadId: number;
  uid: string;
  title: string;
  startAt: number;
  endAt: number;
  timezone: string;
  location: string;
  organizer: string;
  attendees: string;
  response: 'needsAction' | 'accepted' | 'tentative' | 'declined';
  conflictText: string | null;
};

export type DraftSummary = {
  id: string;
  accountId: string;
  accountName: string;
  accountEmail: string;
  accountColor: string;
  recipients: string;
  ccRecipients: string;
  bccRecipients: string;
  subject: string;
  body: string;
  bodyHtml: string;
  replyToThreadId: number | null;
  updatedAt: number;
  revision: number;
  locked: boolean;
};

export type DraftHeaderSummary = Omit<DraftSummary, 'body' | 'bodyHtml'>;

export type SaveDraftInput = {
  id: string | null;
  accountId: string;
  recipients: string;
  ccRecipients: string;
  bccRecipients: string;
  subject: string;
  body: string;
  bodyHtml: string;
  replyToThreadId: number | null;
  expectedRevision: number | null;
};

export type OperationSummary = {
  id: string;
  threadId: number | null;
  field: string;
  kind: string;
  state: string;
  notBefore: number;
};

export type OperationActivitySummary = {
  id: string;
  threadId: number | null;
  field: string;
  kind: string;
  state: string;
  createdAt: number;
  notBefore: number;
  confirmedAt: number | null;
  undoOf: string | null;
  attempts: number;
};

export type ProcessResult = {
  changed: boolean;
  confirmed: string[];
  failed: string[];
};

export type ViewCountSummary = {
  accountId: string | null;
  inbox: number;
  archive: number;
  starred: number;
  sent: number;
  all: number;
  snoozed: number;
  trash: number;
};

export type MailboxBootstrap = {
  schemaVersion: number;
  accounts: AccountSummary[];
  viewCounts: ViewCountSummary[];
  drafts: DraftHeaderSummary[];
  containers: ContainerSummary[];
};

/// One of an account's own folders or labels — everything the eight fixed
/// views do not already stand for. `remoteId` is opaque: the mailbox shows the
/// name, passes the id back, and never reads anything into it.
export type ContainerSummary = {
  accountId: string;
  remoteId: string;
  name: string;
  kind: 'folder' | 'label';
  role: string;
  unread: number;
  total: number;
};

export type ThreadPageInput = {
  accountId: string | null;
  view: 'all' | 'inbox' | 'archive' | 'starred' | 'snoozed' | 'sent' | 'trash';
  cursor: string | null;
  limit: number;
  hiddenAccountIds: string[];
  /// One of the account's own folders, named by the opaque remote id the
  /// mailbox was handed. Only meaningful with the account it belongs to.
  containerId?: string | null;
};

export type ThreadPage = {
  threads: ThreadSummary[];
  hasMore: boolean;
  nextCursor: string | null;
};

export type ThreadLookupInput = {
  threadId: number;
  accountId: string | null;
  view: 'all' | 'inbox' | 'archive' | 'starred' | 'snoozed' | 'sent' | 'trash';
  query: string;
  timezoneOffsetMinutes: number;
};

export type MessagePageInput = {
  threadId: number;
  cursor: string | null;
  limit: number;
};

export type MessagePage = {
  messages: MessageSummary[];
  attachments: AttachmentSummary[];
  invitation: InvitationSummary | null;
  hasMore: boolean;
  nextCursor: string | null;
};

export type SearchInput = {
  query: string;
  accountId: string | null;
  view: 'all' | 'inbox' | 'archive' | 'starred' | 'snoozed' | 'sent' | 'trash' | 'drafts';
  cursor: string | null;
  limit: number;
  timezoneOffsetMinutes: number;
  hiddenAccountIds: string[];
};

export type SearchPage = {
  rows: ThreadSummary[];
  hasMore: boolean;
  nextCursor: string | null;
};

/// Re-exported so the components that only need the resolved theme keep one
/// import; theme.ts owns what it means and how it is chosen.
export type { Theme } from './theme';
export type SettingsSection = 'accounts' | 'appearance' | 'mail' | 'shortcuts';

export type MailboxView = 'all' | 'inbox' | 'archive' | 'starred' | 'snoozed' | 'sent' | 'trash' | 'drafts';
export type SmartView = '' | 'unread' | 'attachments' | 'invitations' | 'finance';
