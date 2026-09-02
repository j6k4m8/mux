/// The mailbox's folders and smart views, named once. The sidebar renders them,
/// the jump dialog offers them, and neither can drift from the other.

import type { MailboxView, SmartView, ViewCountSummary } from './types';

export type ViewIcon = 'inbox' | 'star' | 'clock' | 'sent' | 'drafts' | 'archive' | 'trash' | 'allMail';

export type ViewDescriptor = {
  id: MailboxView;
  icon: ViewIcon;
  label: string;
  title: string;
  count: (counts: ViewCountSummary, draftCount: number) => number;
};

export const mailboxViews: ViewDescriptor[] = [
  { id: 'inbox', icon: 'inbox', label: 'Inbox', title: 'Inbox', count: (counts) => counts.inbox },
  { id: 'starred', icon: 'star', label: 'Starred', title: 'Starred conversations', count: (counts) => counts.starred },
  { id: 'snoozed', icon: 'clock', label: 'Snoozed', title: 'Snoozed conversations', count: (counts) => counts.snoozed },
  { id: 'sent', icon: 'sent', label: 'Sent', title: 'Sent mail', count: (counts) => counts.sent },
  { id: 'drafts', icon: 'drafts', label: 'Drafts', title: 'Local drafts', count: (_counts, drafts) => drafts },
  { id: 'archive', icon: 'archive', label: 'Archive', title: 'Archived conversations', count: (counts) => counts.archive },
  { id: 'trash', icon: 'trash', label: 'Trash', title: 'Trash', count: (counts) => counts.trash },
  { id: 'all', icon: 'allMail', label: 'All mail', title: 'All mail', count: (counts) => counts.all }
];

export type SmartViewDescriptor = {
  id: Exclude<SmartView, ''>;
  label: string;
  dot: string;
  query: string;
  title: string;
};

export const smartViews: SmartViewDescriptor[] = [
  { id: 'unread', label: 'Unread', dot: 'blue', query: 'is:unread', title: 'Unread conversations' },
  { id: 'attachments', label: 'Attachments', dot: 'violet', query: 'has:attachment', title: 'Conversations with attachments' },
  { id: 'invitations', label: 'Invitations', dot: 'green', query: 'has:invite', title: 'Conversations with invitations' },
  { id: 'finance', label: 'Finance', dot: 'amber', query: 'category:Finance', title: 'Finance conversations' }
];
