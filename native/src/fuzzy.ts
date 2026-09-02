/// Subsequence matching, so "inwk" finds "Inbox · work@example.com" without
/// anyone having to remember how a folder is spelled. Scored rather than
/// boolean, because a jump list is only useful if the obvious answer is first.

const CONSECUTIVE_BONUS = 8;
const BOUNDARY_BONUS = 6;
const PREFIX_BONUS = 10;
const GAP_PENALTY = 1;
const MAX_GAP_PENALTY = 12;

function isBoundary(text: string, index: number): boolean {
  if (index === 0) return true;
  const previous = text[index - 1];
  return previous === ' ' || previous === '·' || previous === '-' || previous === '@' || previous === '/';
}

/// Null when the query is not a subsequence of the text. Otherwise a score
/// where larger is a better match; only comparable against the same query.
export function fuzzyScore(query: string, text: string): number | null {
  const needle = query.trim().toLocaleLowerCase();
  if (!needle) return 0;
  const haystack = text.toLocaleLowerCase();
  let score = 0;
  let cursor = 0;
  let previousIndex = -1;
  for (const character of needle) {
    if (character === ' ') continue;
    const index = haystack.indexOf(character, cursor);
    if (index === -1) return null;
    if (index === previousIndex + 1) score += CONSECUTIVE_BONUS;
    if (isBoundary(text, index)) score += index === 0 ? PREFIX_BONUS : BOUNDARY_BONUS;
    if (previousIndex !== -1) score -= Math.min(MAX_GAP_PENALTY, (index - previousIndex - 1) * GAP_PENALTY);
    previousIndex = index;
    cursor = index + 1;
  }
  // A short haystack that contains the query is a closer answer than a long one.
  return score - Math.min(10, Math.floor(haystack.length / 12));
}

export function fuzzyMatches(query: string, text: string): boolean {
  return fuzzyScore(query, text) !== null;
}

/// The best score across several spellings of the same row. A folder row reads
/// "Archive · Work", but someone jumping to it is as likely to think "work
/// archive", and only one of those is a subsequence of the other.
function bestScore(query: string, text: string | string[]): number | null {
  const candidates = typeof text === 'string' ? [text] : text;
  let best: number | null = null;
  for (const candidate of candidates) {
    const score = fuzzyScore(query, candidate);
    if (score !== null && (best === null || score > best)) best = score;
  }
  return best;
}

/// Filter and order in one pass. Ties keep the caller's order, so a list that
/// arrives in a deliberate order still reads that way with an empty query.
export function rankByFuzzy<T>(
  query: string,
  items: T[],
  textOf: (item: T) => string | string[],
  priorityOf: (item: T) => boolean = () => false
): T[] {
  return items
    .map((item, index) => ({ item, index, score: bestScore(query, textOf(item)), priority: priorityOf(item) }))
    .filter((row): row is { item: T; index: number; score: number; priority: boolean } => row.score !== null)
    .sort((left, right) =>
      right.score - left.score
      || Number(right.priority) - Number(left.priority)
      || left.index - right.index)
    .map((row) => row.item);
}
