import { describe, expect, test } from 'vitest';
import { newContentParagraphs, parseMessageRichText } from './richText';

const flatten = (value: string) =>
  newContentParagraphs(value).paragraphs.map((lines) => lines.join('\n'));

describe('new content extraction', () => {
  test('keeps a whole multiline message', () => {
    const body = 'First line\nsecond line\n\nA second paragraph.';
    const result = newContentParagraphs(body);
    expect(result.paragraphs).toEqual([['First line', 'second line'], ['A second paragraph.']]);
    expect(result.trimmed).toBe(false);
  });

  test('drops the signature after a dash-dash line', () => {
    const result = newContentParagraphs('Real content here.\n\n-- \nJordan\nSome Title\nphone');
    expect(flatten('Real content here.\n\n-- \nJordan\nSome Title\nphone')).toEqual(['Real content here.']);
    expect(result.trimmed).toBe(true);
  });

  test('drops a quoted reply chain and its header', () => {
    const body = [
      'Sounds good, thanks.',
      '',
      'On Tue, Aug 25, 2026 at 9:14 AM Alice Example <alice@example.test> wrote:',
      '> here is the original',
      '> second quoted line',
      '>',
      '>> and a nested quote'
    ].join('\n');
    expect(flatten(body)).toEqual(['Sounds good, thanks.']);
  });

  test('handles a reply header wrapped onto two lines', () => {
    const body = 'My answer.\n\nOn Tue, Aug 25, 2026 at 9:14 AM\nAlice Example wrote:\n> quoted';
    expect(flatten(body)).toEqual(['My answer.']);
  });

  test('drops outlook style original-message blocks', () => {
    const body = 'Please see below.\n\n-----Original Message-----\nFrom: Alice\nSent: Tuesday\nTo: Jordan';
    expect(flatten(body)).toEqual(['Please see below.']);
  });

  test('drops client postmatter', () => {
    expect(flatten('Short reply.\n\nSent from my iPhone')).toEqual(['Short reply.']);
    expect(flatten('Short reply.\n\nGet Outlook for iOS')).toEqual(['Short reply.']);
  });

  test('drops interleaved quoted lines but keeps the surrounding reply', () => {
    const body = 'Above the quote.\n> quoted middle\nBelow the quote.';
    expect(flatten(body)).toEqual(['Above the quote.\nBelow the quote.']);
  });

  test('a message that is only a quote still shows something', () => {
    const body = '> nothing but a quote\n> second line';
    const result = newContentParagraphs(body);
    expect(result.paragraphs.length).toBeGreaterThan(0);
    // Nothing was really hidden, so it must not claim otherwise.
    expect(result.trimmed).toBe(false);
  });

  test('a run of blank lines collapses to one paragraph break', () => {
    expect(flatten('a\n\n\n\n\nb')).toEqual(['a', 'b']);
  });

  test('a plain hyphen line is not mistaken for a signature', () => {
    // Only "--" on its own is the RFC signature marker.
    expect(flatten('before\n-\nafter')).toEqual(['before\n-\nafter']);
  });
});

describe('render tree structure', () => {
  const tags = (html: string) =>
    parseMessageRichText(html)
      .flatMap(function walk(node): string[] {
        if (node.type !== 'element') return [];
        return [node.tag, ...node.children.flatMap(walk)];
      });

  test('keeps tables, headings, rules, and code', () => {
    const html = '<h2>Head</h2><table><tr><th>A</th><td>B</td></tr></table><hr><pre><code>x</code></pre>';
    expect(tags(html)).toEqual(
      expect.arrayContaining(['h2', 'table', 'tr', 'th', 'td', 'hr', 'pre', 'code'])
    );
  });

  test('still drops active and resource elements', () => {
    const html = '<script>alert(1)</script><img src="https://x.test/a.png"><form><input></form><p>kept</p>';
    const rendered = tags(html);
    for (const forbidden of ['script', 'img', 'form', 'input']) {
      expect(rendered).not.toContain(forbidden);
    }
    expect(rendered).toContain('p');
  });

  test('void elements carry no children', () => {
    const nodes = parseMessageRichText('<hr><br>');
    for (const node of nodes) {
      if (node.type === 'element') expect(node.children).toHaveLength(0);
    }
  });

  test('legacy strikethrough normalises onto one tag', () => {
    expect(tags('<s>a</s><strike>b</strike><del>c</del>')).toEqual(['del', 'del', 'del']);
  });
});
