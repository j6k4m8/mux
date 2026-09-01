import assert from 'node:assert/strict';
import { test } from 'vitest';

import { emailAddresses, replyRecipients, shouldExpandInlineReply } from './replyRecipients';

const messages = [
  {
    senderEmail: 'jordan@mux.example',
    recipients: 'Jane Cooper <jane@acme.example>, Priya Raman <priya@acme.example>',
    isFromMe: true
  },
  {
    senderEmail: 'jane@acme.example',
    recipients: 'Jordan <jordan@mux.example>, Priya Raman <priya@acme.example>',
    isFromMe: false
  }
];

test('emailAddresses extracts addresses from display-name recipient headers', () => {
  assert.deepEqual(
    emailAddresses('Jane <JANE@acme.example>, priya@acme.example'),
    ['JANE@acme.example', 'priya@acme.example']
  );
});

test('Reply targets the latest external sender', () => {
  assert.equal(replyRecipients(messages, ['jordan@mux.example']), 'jane@acme.example');
});

test('Reply All excludes own addresses and deduplicates recipients', () => {
  assert.equal(
    replyRecipients(messages, ['jordan@mux.example'], 'replyAll'),
    'jane@acme.example, priya@acme.example'
  );
});

test('outgoing latest messages retain every external participant for Reply All', () => {
  const outgoingLast = [...messages, {
    senderEmail: 'jordan@mux.example',
    recipients: 'Jane <jane@acme.example>, Priya <priya@acme.example>, jane@acme.example',
    isFromMe: true
  }];
  assert.equal(
    replyRecipients(outgoingLast, ['jordan@mux.example'], 'replyAll'),
    'jane@acme.example, priya@acme.example'
  );
});

test('quick replies expand after 100 characters and stay expanded', () => {
  assert.equal(shouldExpandInlineReply(false, 'x'.repeat(100), ''), false);
  assert.equal(shouldExpandInlineReply(false, 'x'.repeat(101), ''), true);
  assert.equal(shouldExpandInlineReply(true, 'short again', ''), true);
  assert.equal(shouldExpandInlineReply(false, 'formatted', '<p><strong>formatted</strong></p>'), true);
});
