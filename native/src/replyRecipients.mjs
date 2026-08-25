const addressPattern = /[A-Z0-9.!#$%&'*+/=?^_`{|}~-]+@[A-Z0-9.-]+\.[A-Z]{2,}/giu;

/** @param {string} value */
export function emailAddresses(value) {
  return value.match(addressPattern) ?? [];
}

/**
 * @param {Array<{ senderEmail: string, recipients: string, ccRecipients?: string, isFromMe: boolean }>} messages
 * @param {string[]} ownAddresses
 * @param {'reply' | 'replyAll'} mode
 */
export function replyRecipients(messages, ownAddresses, mode = 'reply') {
  const own = new Set(ownAddresses.map((address) => address.trim().toLocaleLowerCase()).filter(Boolean));
  const latestExternal = [...messages].reverse().find((message) => !message.isFromMe);
  const latest = messages.at(-1);
  const candidates = mode === 'replyAll'
    ? [latestExternal?.senderEmail, latest?.senderEmail, latest?.recipients, latest?.ccRecipients, latestExternal?.recipients, latestExternal?.ccRecipients]
    : [latestExternal?.senderEmail, latest?.recipients];
  const unique = new Map();

  for (const candidate of candidates) {
    for (const address of emailAddresses(candidate ?? '')) {
      const key = address.toLocaleLowerCase();
      if (!own.has(key) && !unique.has(key)) unique.set(key, address);
    }
    if (mode === 'reply' && unique.size) break;
  }

  return [...unique.values()].join(', ');
}

/** @param {boolean} expanded @param {string} text @param {string} html */
export function shouldExpandInlineReply(expanded, text, html) {
  return expanded || text.length > 100 || /<(?:a|strong|em|u|ul|ol|li)\b/iu.test(html);
}
