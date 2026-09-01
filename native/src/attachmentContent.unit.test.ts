import assert from 'node:assert/strict';
import { test } from 'vitest';

import { decodeAttachmentBase64, MAX_ATTACHMENT_BYTES } from './attachmentContent';

test('bounded attachment base64 decodes exact binary bytes', () => {
  const expected = Uint8Array.from([0, 1, 127, 128, 254, 255]);
  const encoded = Buffer.from(expected).toString('base64');
  assert.deepEqual(decodeAttachmentBase64(encoded, expected.length), expected);
});

test('attachment decoding rejects invalid length, encoding, and metadata before use', () => {
  assert.throws(
    () => decodeAttachmentBase64('', MAX_ATTACHMENT_BYTES + 1),
    /Attachment byte length is invalid/u
  );
  assert.throws(() => decodeAttachmentBase64('not base64!', 3), /Attachment encoding is invalid/u);
  assert.throws(
    () => decodeAttachmentBase64(Buffer.from('three').toString('base64'), 4),
    /Attachment byte metadata is inconsistent/u
  );
});
