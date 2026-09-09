import assert from 'node:assert/strict';
import { test } from 'vitest';

import {
  activeWordAt,
  lexSearchQuery,
  searchFields,
  searchIsNarrowed,
  searchScopeView,
  searchSegments,
  searchSuggestions,
  searchTerms,
  searchViewFor,
  viewScopeTerm
} from './searchQuery';

const accounts = [
  { id: 'acc_work', name: 'Work', email: 'jordan@acme.example' },
  { id: 'acc_home', name: 'Home', email: 'jordan@home.example' }
];

function kinds(query: string): string[] {
  return lexSearchQuery(query).map((token) => `${token.kind}:${query.slice(token.start, token.end)}`);
}

test('the lexer splits fields, operators, and grouping the way the native parser does', () => {
  assert.deepEqual(
    kinds('from:alice OR (subject:"quarter end" AND is:unread)'),
    [
      'field:from:', 'value:alice', 'operator:OR', 'paren:(',
      'field:subject:', 'value:"quarter end"', 'operator:AND',
      'field:is:', 'value:unread', 'paren:)'
    ]
  );
});

test('only the fields the query compiler knows are colored as fields', () => {
  assert.deepEqual(kinds('mystery:value'), ['text:mystery:value']);
  assert.deepEqual(kinds('domain:acme.example'), ['field:domain:', 'value:acme.example']);
  // Half-typed is still a field, because coloring has to help while typing.
  assert.deepEqual(kinds('from:'), ['field:from:']);
});

test('quoting follows the native tokenizer, including a quoted operator', () => {
  assert.deepEqual(kinds('"and"'), ['operator:"and"']);
  assert.deepEqual(kinds("subject:'two words' rest"), ['field:subject:', "value:'two words'", 'text:rest']);
  // An unterminated quote is a query in progress, not a crash.
  assert.deepEqual(kinds('subject:"open'), ['field:subject:', 'value:"open']);
});

test('segments cover every character exactly once, so the overlay cannot drift', () => {
  for (const query of ['', '  ', 'from:alice OR  (is:unread)', 'plain words here', 'subject:"a b"  ']) {
    assert.equal(searchSegments(query).map((segment) => segment.text).join(''), query, query);
  }
});

test('the active word is what a completion replaces', () => {
  assert.equal(activeWordAt('from:ali more', 5), 'from:ali');
  assert.equal(activeWordAt('(from:ali', 9), 'from:ali');
  assert.equal(activeWordAt('one two', 3), 'one');
  assert.equal(activeWordAt('one ', 4), '');
});

test('a partial field name completes to the field and leaves the caret on the value', () => {
  const suggestions = searchSuggestions({ text: 'fro', accounts });
  const field = suggestions.find((suggestion) => suggestion.kind === 'field');
  assert.equal(field?.label, 'from:');
  assert.equal(field?.text, 'from:');
  assert.equal(field?.caret, 5);
});

test('field values come from the native compiler, and nothing else is offered', () => {
  const values = searchSuggestions({ text: 'is:', accounts }).filter((suggestion) => suggestion.kind === 'value');
  assert.deepEqual(values.map((suggestion) => suggestion.label), [
    'is:unread', 'is:read', 'is:starred', 'is:unstarred', 'is:snoozed', 'is:sent', 'is:me'
  ]);
  assert.deepEqual(
    searchSuggestions({ text: 'has:a', accounts }).filter((suggestion) => suggestion.kind === 'value').map((suggestion) => suggestion.label),
    ['has:attachment']
  );
  // A free-text field has no list to offer, so nothing but the save row appears.
  assert.deepEqual(
    searchSuggestions({ text: 'subject:budget', accounts }).map((suggestion) => suggestion.kind),
    ['save']
  );
});

test('account values come from the accounts that exist', () => {
  const suggestions = searchSuggestions({ text: 'account:ho', accounts });
  assert.deepEqual(
    suggestions.filter((suggestion) => suggestion.kind === 'account').map((suggestion) => suggestion.text),
    ['account:jordan@home.example ']
  );
});

test('completing replaces only the word under the caret', () => {
  const suggestions = searchSuggestions({ text: 'is:unread fro', caret: 13, accounts });
  const field = suggestions.find((suggestion) => suggestion.label === 'from:');
  assert.equal(field?.text, 'is:unread from:');
  assert.equal(field?.caret, 15);
});

test('an empty box offers saved searches first, then somewhere to start', () => {
  const saved = [{ name: 'Money', query: 'category:Finance' }];
  const suggestions = searchSuggestions({ text: '', accounts, saved });
  assert.equal(suggestions[0].kind, 'saved');
  assert.equal(suggestions[0].text, 'category:Finance');
  assert.ok(suggestions.slice(1).every((suggestion) => suggestion.kind === 'field'));
  // Nothing typed is nothing to save.
  assert.ok(!suggestions.some((suggestion) => suggestion.kind === 'save'));
});

test('a query that is not saved yet offers to be saved, and once saved does not', () => {
  const query = 'is:unread from:alice';
  assert.equal(searchSuggestions({ text: query, accounts }).at(-1)?.kind, 'save');
  const saved = [{ name: 'Alice unread', query }];
  assert.ok(!searchSuggestions({ text: query, accounts, saved }).some((suggestion) => suggestion.kind === 'save'));
});

test('operators complete too, and the list stays short enough to read', () => {
  const suggestions = searchSuggestions({ text: 'is:unread o', caret: 11, accounts });
  assert.equal(suggestions.find((suggestion) => suggestion.kind === 'operator')?.text, 'is:unread or ');
  assert.ok(searchSuggestions({ text: '', accounts, limit: 3 }).length <= 4);
});

test('each mailbox reads as a term the query language actually accepts', () => {
  const scoped: Array<[string, string]> = [
    ['inbox', 'in:inbox'],
    ['archive', 'in:archive'],
    ['sent', 'in:sent'],
    ['snoozed', 'in:snoozed'],
    ['trash', 'in:trash'],
    ['starred', 'is:starred']
  ];
  for (const [view, term] of scoped) {
    assert.equal(viewScopeTerm(view as never), term, view);
    // Whatever the term is, it has to lex as a real field with a real value.
    const [field] = searchTerms(term);
    const known = searchFields.find((candidate) => candidate.name === field.field);
    assert.ok(known, term);
    assert.ok(known.values.includes(field.value), term);
  }
  // All mail is the whole account already, and drafts are not searchable.
  assert.equal(viewScopeTerm('all' as never), '');
  assert.equal(viewScopeTerm('drafts' as never), '');
});

test('field terms are read back out of the text, quotes and all', () => {
  assert.deepEqual(searchTerms('in:Inbox from:"Alice Example" hello'), [
    { field: 'in', value: 'inbox' },
    { field: 'from', value: 'alice example' }
  ]);
  // A field with no value yet is not a term.
  assert.deepEqual(searchTerms('in:'), []);
});

test('only a query asking for trash is run against trash', () => {
  assert.equal(searchViewFor('in:inbox budget'), 'all');
  assert.equal(searchViewFor('budget'), 'all');
  assert.equal(searchViewFor(''), 'all');
  assert.equal(searchViewFor('in:trash budget'), 'trash');
  assert.equal(searchViewFor('IN:TRASH'), 'trash');
});

test('a box holding only the seeded scope has not narrowed anything', () => {
  assert.equal(searchIsNarrowed('in:inbox', 'inbox' as never), false);
  assert.equal(searchIsNarrowed('  in:inbox  ', 'inbox' as never), false);
  assert.equal(searchIsNarrowed('in:inbox budget', 'inbox' as never), true);
  // The same term in a different mailbox is a real narrowing.
  assert.equal(searchIsNarrowed('in:inbox', 'archive' as never), true);
  assert.equal(searchIsNarrowed('', 'inbox' as never), false);
});

test('a search covers its view only while the seeded scope is still in the box', () => {
  assert.equal(searchScopeView('in:inbox budget', 'inbox' as never), 'inbox');
  assert.equal(searchScopeView('budget in:inbox', 'inbox' as never), 'inbox');
  assert.equal(searchScopeView('is:starred budget', 'starred' as never), 'starred');
  // Deleting the scope widens the search to the whole account, and the title with it.
  assert.equal(searchScopeView('budget', 'inbox' as never), 'all');
  assert.equal(searchScopeView('in:trash budget', 'inbox' as never), 'trash');
  // All mail has no scope to keep, and drafts are only ever filtered in place.
  assert.equal(searchScopeView('budget', 'all' as never), 'all');
  assert.equal(searchScopeView('budget', 'drafts' as never), 'drafts');
});
