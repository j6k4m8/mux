import { emailAddresses } from './replyRecipients.mjs';

/** @param {string} value */
export function recipientTokens(value) {
  const tokens = [];
  let current = '';
  let quote = null;
  let angleDepth = 0;
  for (const character of String(value ?? '')) {
    if (quote) {
      current += character;
      if (character === quote) quote = null;
      continue;
    }
    if (character === '"' || character === "'") {
      quote = character;
      current += character;
      continue;
    }
    if (character === '<') angleDepth += 1;
    if (character === '>') angleDepth = Math.max(0, angleDepth - 1);
    if ((character === ',' || character === ';') && angleDepth === 0) {
      if (current.trim()) tokens.push(current.trim());
      current = '';
      continue;
    }
    current += character;
  }
  if (current.trim()) tokens.push(current.trim());
  return tokens;
}

/** @param {string[]} tokens */
export function serializeRecipientTokens(tokens) {
  return tokens.map((token) => token.trim()).filter(Boolean).join(', ');
}

/** @param {string} existing @param {string} candidate */
export function appendRecipientToken(existing, candidate) {
  const clean = candidate.trim();
  if (!clean) return { value: serializeRecipientTokens(recipientTokens(existing)), error: '' };
  if (/\r|\n/u.test(clean) || emailAddresses(clean).length !== 1) {
    return { value: existing, error: 'Enter one complete email address.' };
  }
  const tokens = recipientTokens(existing);
  const [completeAddress] = emailAddresses(clean);
  if (!completeAddress) return { value: existing, error: 'Enter one complete email address.' };
  const address = completeAddress.toLocaleLowerCase();
  if (!tokens.some((token) => emailAddresses(token)[0]?.toLocaleLowerCase() === address)) tokens.push(clean);
  return { value: serializeRecipientTokens(tokens), error: '' };
}
