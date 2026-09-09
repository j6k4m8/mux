/// Reading the search box the same way the native side does. The Rust compiler
/// in search.rs owns the grammar; this mirrors it twice over — once to colour
/// what is typed, once to suggest what could come next — so the box never
/// offers a term the query engine would reject.

import type { MailboxView } from './types';

export type SearchTokenKind = 'field' | 'value' | 'operator' | 'paren' | 'text';

export type SearchToken = { kind: SearchTokenKind; start: number; end: number };

export type SearchSegment = { kind: SearchTokenKind | 'gap'; text: string };

export type SearchFieldName =
  | 'from' | 'to' | 'subject' | 'account' | 'in' | 'label' | 'category' | 'is'
  | 'has' | 'after' | 'before' | 'date' | 'filename' | 'domain';

type SearchField = { name: SearchFieldName; description: string; values: string[] };

const DATE_VALUES = ['today', 'yesterday', '1d', '1w', '1mo', '3mo', '6mo', '1y'];

/// Exactly the fields FIELD_NAMES lists, with exactly the values compile_field
/// accepts. Anything else here would be a suggestion that fails on Enter.
export const searchFields: SearchField[] = [
  { name: 'from', description: 'Sender name or address', values: [] },
  { name: 'to', description: 'Any recipient, including cc and bcc', values: [] },
  { name: 'subject', description: 'Words in the subject line', values: [] },
  { name: 'is', description: 'Conversation state', values: ['unread', 'read', 'starred', 'unstarred', 'snoozed', 'sent', 'me'] },
  { name: 'has', description: 'What the conversation carries', values: ['attachment', 'invite', 'calendar', 'link'] },
  { name: 'in', description: 'Where it lives', values: ['inbox', 'archive', 'sent', 'snoozed', 'trash', 'all'] },
  { name: 'after', description: 'Newer than a date', values: DATE_VALUES },
  { name: 'before', description: 'Older than a date', values: DATE_VALUES },
  { name: 'date', description: 'On one day', values: DATE_VALUES },
  { name: 'account', description: 'One of your accounts', values: [] },
  { name: 'label', description: 'Category label', values: [] },
  { name: 'category', description: 'Category label', values: [] },
  { name: 'filename', description: 'Attachment file name', values: [] },
  { name: 'domain', description: 'Anyone at a domain', values: [] }
];

const fieldsByName = new Map(searchFields.map((field) => [field.name, field]));

const OPERATORS = ['and', 'or', 'not'] as const;

function isTermBreak(character: string): boolean {
  return character === '(' || character === ')' || /\s/.test(character);
}

/// Mirrors tokenize() in search.rs: quotes are stripped while scanning, so a
/// quoted "and" really is the operator, and a term runs until whitespace or a
/// parenthesis outside quotes.
export function lexSearchQuery(input: string): SearchToken[] {
  const tokens: SearchToken[] = [];
  let cursor = 0;
  while (cursor < input.length) {
    const character = input[cursor];
    if (/\s/.test(character)) {
      cursor += 1;
      continue;
    }
    if (character === '(' || character === ')') {
      tokens.push({ kind: 'paren', start: cursor, end: cursor + 1 });
      cursor += 1;
      continue;
    }
    const start = cursor;
    let quote: string | null = null;
    let unquoted = '';
    let colonAt = -1;
    while (cursor < input.length) {
      const current = input[cursor];
      if (quote !== null) {
        if (current === quote) {
          quote = null;
          cursor += 1;
          continue;
        }
        if (current === '\\' && cursor + 1 < input.length) {
          unquoted += input[cursor + 1];
          cursor += 2;
          continue;
        }
        unquoted += current;
        cursor += 1;
        continue;
      }
      if (current === '"' || current === "'") {
        quote = current;
        cursor += 1;
        continue;
      }
      if (isTermBreak(current)) break;
      if (current === ':' && colonAt === -1) colonAt = cursor;
      unquoted += current;
      cursor += 1;
    }
    const end = cursor;
    tokens.push(...classifyTerm(input, start, end, unquoted, colonAt));
  }
  return tokens;
}

function classifyTerm(
  input: string,
  start: number,
  end: number,
  unquoted: string,
  colonAt: number
): SearchToken[] {
  if (OPERATORS.includes(unquoted.toLocaleLowerCase() as (typeof OPERATORS)[number])) {
    return [{ kind: 'operator', start, end }];
  }
  if (colonAt === -1) return [{ kind: 'text', start, end }];
  const name = input.slice(start, colonAt).toLocaleLowerCase();
  if (!fieldsByName.has(name as SearchFieldName)) return [{ kind: 'text', start, end }];
  const field: SearchToken = { kind: 'field', start, end: colonAt + 1 };
  return colonAt + 1 < end ? [field, { kind: 'value', start: colonAt + 1, end }] : [field];
}

/// The same tokens, but covering every character, which is what an overlay has
/// to render: one span per segment, in order, nothing dropped.
export function searchSegments(input: string): SearchSegment[] {
  const segments: SearchSegment[] = [];
  let cursor = 0;
  for (const token of lexSearchQuery(input)) {
    if (token.start > cursor) segments.push({ kind: 'gap', text: input.slice(cursor, token.start) });
    segments.push({ kind: token.kind, text: input.slice(token.start, token.end) });
    cursor = token.end;
  }
  if (cursor < input.length) segments.push({ kind: 'gap', text: input.slice(cursor) });
  return segments;
}

/// The field terms a query carries, unquoted and lowercased. Reading them back
/// out of the text is what lets the mailbox scope a search with a term the
/// reader can see — and delete.
export function searchTerms(input: string): Array<{ field: SearchFieldName; value: string }> {
  const tokens = lexSearchQuery(input);
  const terms: Array<{ field: SearchFieldName; value: string }> = [];
  for (let index = 0; index < tokens.length; index += 1) {
    const token = tokens[index];
    if (token.kind !== 'field') continue;
    const name = input.slice(token.start, token.end - 1).toLocaleLowerCase() as SearchFieldName;
    const next = tokens[index + 1];
    if (!next || next.kind !== 'value') continue;
    const raw = input.slice(next.start, next.end).trim();
    const unquoted = raw.length > 1 && (raw.startsWith('"') || raw.startsWith("'")) && raw.at(-1) === raw[0]
      ? raw.slice(1, -1)
      : raw;
    terms.push({ field: name, value: unquoted.toLocaleLowerCase() });
  }
  return terms;
}

/// How each mailbox view reads as a search term. Views that are flags rather
/// than folders still have one, because the reader has to be able to see the
/// scope they are searching inside and take it away.
const VIEW_SCOPES: Record<MailboxView, string> = {
  inbox: 'in:inbox',
  archive: 'in:archive',
  sent: 'in:sent',
  snoozed: 'in:snoozed',
  trash: 'in:trash',
  starred: 'is:starred',
  // Neither of these narrows anything: All mail is the whole account already,
  // and drafts are not searchable at all.
  all: '',
  drafts: ''
};

export function viewScopeTerm(view: MailboxView): string {
  return VIEW_SCOPES[view] ?? '';
}

/// Whether the box has actually narrowed anything. A query that is only the
/// scope the mailbox seeded describes the view the reader is already in, so it
/// is not a search yet.
export function searchIsNarrowed(query: string, view: MailboxView): boolean {
  const trimmed = query.trim();
  return Boolean(trimmed) && trimmed !== viewScopeTerm(view);
}

/// Which mailbox the native side should search, given a query that carries its
/// own scope. Everything except trash lives under "all", so the query's own
/// terms do the narrowing; trash is the one view "all" excludes outright, so a
/// query asking for it has to be run against trash instead.
export function searchViewFor(input: string): Extract<MailboxView, 'all' | 'trash'> {
  return searchTerms(input).some((term) => term.field === 'in' && term.value === 'trash')
    ? 'trash'
    : 'all';
}

/// The mailbox a search still covers, which is what a heading over the results
/// has to name. The seeded scope is what keeps a search inside the view it
/// started from, so that view holds only while the term is in the box; once it
/// has been deleted the results are as wide as the native side runs them.
/// Drafts are the exception: they are never searched, only filtered in place.
export function searchScopeView(query: string, view: MailboxView): MailboxView {
  if (view === 'drafts') return view;
  const scope = viewScopeTerm(view);
  if (scope && searchTerms(query).some((term) => `${term.field}:${term.value}` === scope)) return view;
  return searchViewFor(query);
}

export type SearchSuggestionKind = 'saved' | 'field' | 'value' | 'account' | 'operator' | 'save';

export type SearchSuggestion = {
  id: string;
  kind: SearchSuggestionKind;
  label: string;
  detail: string;
  /// The whole replacement value for the input, and where the caret lands.
  text: string;
  caret: number;
};

export type SuggestionAccount = { id: string; name: string; email: string };

export type SavedSearch = { name: string; query: string };

export type SuggestionOptions = {
  text: string;
  caret?: number;
  accounts?: SuggestionAccount[];
  saved?: SavedSearch[];
  limit?: number;
};

const DEFAULT_LIMIT = 7;
const STARTER_FIELDS: SearchFieldName[] = ['from', 'to', 'subject', 'is', 'has', 'after'];

/// The word the caret sits in, which is what a completion replaces. Parentheses
/// bound a word so "(from:jo" completes the sender and leaves the bracket be.
export function activeWordAt(text: string, caret: number): string {
  return activeWord(text, caret).value;
}

function activeWord(text: string, caret: number): { start: number; end: number; value: string } {
  const position = Math.max(0, Math.min(text.length, caret));
  let start = position;
  while (start > 0 && !isTermBreak(text[start - 1])) start -= 1;
  let end = position;
  while (end < text.length && !isTermBreak(text[end])) end += 1;
  return { start, end, value: text.slice(start, end) };
}

function replaceRange(text: string, start: number, end: number, replacement: string): { text: string; caret: number } {
  return { text: text.slice(0, start) + replacement + text.slice(end), caret: start + replacement.length };
}

export function searchSuggestions(options: SuggestionOptions): SearchSuggestion[] {
  const { text, accounts = [], saved = [] } = options;
  const limit = options.limit ?? DEFAULT_LIMIT;
  const caret = options.caret ?? text.length;
  const word = activeWord(text, caret);
  const lowered = word.value.toLocaleLowerCase();
  const suggestions: SearchSuggestion[] = [];

  const complete = (replacement: string, rest: Omit<SearchSuggestion, 'text' | 'caret'>) => {
    const applied = replaceRange(text, word.start, word.end, replacement);
    suggestions.push({ ...rest, ...applied });
  };

  const colonAt = lowered.indexOf(':');
  const field = colonAt === -1 ? null : fieldsByName.get(lowered.slice(0, colonAt) as SearchFieldName) ?? null;

  if (field) {
    const partial = lowered.slice(colonAt + 1);
    if (field.name === 'account') {
      for (const account of accounts) {
        if (partial && !`${account.name} ${account.email}`.toLocaleLowerCase().includes(partial)) continue;
        complete(`account:${account.email} `, {
          id: `account:${account.id}`,
          kind: 'account',
          label: `account:${account.email}`,
          detail: account.name
        });
      }
    }
    for (const value of field.values) {
      if (partial && !value.startsWith(partial)) continue;
      complete(`${field.name}:${value} `, {
        id: `value:${field.name}:${value}`,
        kind: 'value',
        label: `${field.name}:${value}`,
        detail: ''
      });
    }
  } else if (lowered) {
    for (const candidate of searchFields) {
      if (!candidate.name.startsWith(lowered)) continue;
      complete(`${candidate.name}:`, {
        id: `field:${candidate.name}`,
        kind: 'field',
        label: `${candidate.name}:`,
        detail: candidate.description
      });
    }
    for (const operator of OPERATORS) {
      if (!operator.startsWith(lowered)) continue;
      complete(`${operator} `, {
        id: `operator:${operator}`,
        kind: 'operator',
        label: operator.toLocaleUpperCase(),
        detail: operatorDescription(operator)
      });
    }
    for (const entry of saved) {
      if (!`${entry.name} ${entry.query}`.toLocaleLowerCase().includes(lowered)) continue;
      suggestions.push({
        id: `saved:${entry.name}`,
        kind: 'saved',
        label: entry.name,
        detail: entry.query,
        text: entry.query,
        caret: entry.query.length
      });
    }
  } else {
    for (const entry of saved) {
      suggestions.push({
        id: `saved:${entry.name}`,
        kind: 'saved',
        label: entry.name,
        detail: entry.query,
        text: entry.query,
        caret: entry.query.length
      });
    }
    for (const name of STARTER_FIELDS) {
      const candidate = fieldsByName.get(name);
      if (!candidate) continue;
      complete(`${candidate.name}:`, {
        id: `field:${candidate.name}`,
        kind: 'field',
        label: `${candidate.name}:`,
        detail: candidate.description
      });
    }
  }

  const trimmed = text.trim();
  const bounded = suggestions.slice(0, limit);
  if (trimmed && !saved.some((entry) => entry.query === trimmed)) {
    bounded.push({
      id: 'save',
      kind: 'save',
      label: `Save "${trimmed}"`,
      detail: 'Keep this search for later',
      text,
      caret
    });
  }
  return bounded;
}

function operatorDescription(operator: (typeof OPERATORS)[number]): string {
  if (operator === 'and') return 'Both sides must match';
  if (operator === 'or') return 'Either side may match';
  return 'Exclude what matches';
}
