export const MAX_ATTACHMENT_BYTES = 20 * 1024 * 1024;
const MAX_BASE64_LENGTH = 4 * Math.ceil(MAX_ATTACHMENT_BYTES / 3);

/// Revalidates the typed Rust attachment response before allocating its binary
/// copy. The native boundary is authoritative; this check keeps a compromised
/// or stale IPC mock from turning an attachment action into an unbounded array.
export function decodeAttachmentBase64(dataBase64: string, byteLength: number): Uint8Array {
  if (!Number.isInteger(byteLength) || byteLength < 0 || byteLength > MAX_ATTACHMENT_BYTES) {
    throw new Error('Attachment byte length is invalid');
  }
  if (typeof dataBase64 !== 'string' || dataBase64.length > MAX_BASE64_LENGTH) {
    throw new Error('Attachment encoding exceeds its limit');
  }
  let decoded: string;
  try {
    decoded = atob(dataBase64);
  } catch {
    throw new Error('Attachment encoding is invalid');
  }
  if (decoded.length !== byteLength) {
    throw new Error('Attachment byte metadata is inconsistent');
  }
  const bytes = new Uint8Array(byteLength);
  for (let index = 0; index < decoded.length; index += 1) {
    bytes[index] = decoded.charCodeAt(index);
  }
  return bytes;
}
