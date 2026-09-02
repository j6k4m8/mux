import { emailAddresses } from './replyRecipients';

/// Splits a recipient line on separators that are not inside a quoted display
/// name or an angle-bracketed address.
export function recipientTokens(value: string): string[] {
  const tokens: string[] = [];
  let current = '';
  let quote: string | null = null;
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

export function serializeRecipientTokens(tokens: string[]): string {
  return tokens.map((token) => token.trim()).filter(Boolean).join(', ');
}

export function appendRecipientToken(
  existing: string,
  candidate: string
): { value: string; error: string } {
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
