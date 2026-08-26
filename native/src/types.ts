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
};

export type ThreadPageInput = {
  accountId: string | null;
  view: 'all' | 'inbox' | 'archive' | 'starred' | 'snoozed' | 'sent' | 'trash';
  cursor: string | null;
  limit: number;
  hiddenAccountIds: string[];
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
