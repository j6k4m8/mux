export type ReplyMode = 'reply' | 'replyAll';

type ReplySource = {
  senderEmail: string;
  recipients: string;
  ccRecipients?: string;
  isFromMe: boolean;
};

const addressPattern = /[A-Z0-9.!#$%&'*+/=?^_`{|}~-]+@[A-Z0-9.-]+\.[A-Z]{2,}/giu;

export function emailAddresses(value: string): string[] {
  return value.match(addressPattern) ?? [];
}

/// Your own addresses never come back as recipients, and a plain reply stops at
/// the first source that yields anyone.
export function replyRecipients(
  messages: ReplySource[],
  ownAddresses: string[],
  mode: ReplyMode = 'reply'
): string {
  const own = new Set(ownAddresses.map((address) => address.trim().toLocaleLowerCase()).filter(Boolean));
  const latestExternal = [...messages].reverse().find((message) => !message.isFromMe);
  const latest = messages.at(-1);
  const candidates = mode === 'replyAll'
    ? [latestExternal?.senderEmail, latest?.senderEmail, latest?.recipients, latest?.ccRecipients, latestExternal?.recipients, latestExternal?.ccRecipients]
    : [latestExternal?.senderEmail, latest?.recipients];
  const unique = new Map<string, string>();

  for (const candidate of candidates) {
    for (const address of emailAddresses(candidate ?? '')) {
      const key = address.toLocaleLowerCase();
      if (!own.has(key) && !unique.has(key)) unique.set(key, address);
    }
    if (mode === 'reply' && unique.size) break;
  }

  return [...unique.values()].join(', ');
}

export function shouldExpandInlineReply(expanded: boolean, text: string, html: string): boolean {
  return expanded || text.length > 100 || /<(?:a|strong|em|u|ul|ol|li)\b/iu.test(html);
}
