import assert from 'node:assert/strict';
import test from 'node:test';

import { appendRecipientToken, recipientTokens } from '../src/recipientTokens.mjs';

test('recipient chips preserve quoted display-name commas', () => {
  assert.deepEqual(
    recipientTokens('"Cooper, Jane" <jane@acme.example>, priya@acme.example'),
    ['"Cooper, Jane" <jane@acme.example>', 'priya@acme.example']
  );
});

test('recipient chips deduplicate addresses case-insensitively', () => {
  assert.equal(
    appendRecipientToken('Jane <jane@acme.example>', 'JANE@acme.example').value,
    'Jane <jane@acme.example>'
  );
});

test('recipient chips reject incomplete addresses and header injection', () => {
  assert.match(appendRecipientToken('', 'not-an-address').error, /complete email/u);
  assert.match(appendRecipientToken('', 'jane@example.com\r\nBcc: x@example.com').error, /complete email/u);
});
