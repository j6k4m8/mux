import assert from 'node:assert/strict';
import { readdirSync, readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { test } from 'vitest';

// Resolved from the package root, which is where the test runner starts.
const SRC = resolve(process.cwd(), 'src');
const styles = readFileSync(resolve(SRC, 'styles.css'), 'utf8');
// A stylesheet that failed to load would make every assertion below vacuous.
if (!styles.includes('.thread-row')) throw new Error('styles.css did not load');

/// The smallest step the interface may draw text at. The message frame is a
/// sender's document and is not held to this; everything Mux draws is.
const FLOOR_PT = 11;

function withoutComments(css: string): string {
  return css.replace(/\/\*[\s\S]*?\*\//gu, '');
}

/// Every <style> block of every component, named so a failure says where.
function componentStyles(): Array<{ name: string; css: string }> {
  return readdirSync(SRC)
    .filter((name) => name.endsWith('.svelte'))
    .flatMap((name) => {
      const source = readFileSync(resolve(SRC, name), 'utf8');
      return [...source.matchAll(/<style[^>]*>([\s\S]*?)<\/style>/gu)]
        .map((match) => ({ name, css: withoutComments(match[1]) }));
    });
}

function declarations(css: string, selector: string): string[] {
  // Every declaration block whose selector list contains an exact match for `selector`.
  const blocks: string[] = [];
  const pattern = /(?<selectors>[^{}]+)\{(?<body>[^{}]*)\}/gu;
  for (const match of withoutComments(css).matchAll(pattern)) {
    const selectors = match.groups!.selectors
      .split(',')
      .map((value) => value.trim().replace(/\s+/gu, ' '));
    if (selectors.includes(selector)) blocks.push(match.groups!.body.trim());
  }
  return blocks;
}

/// `--text-N: calc(<pt>pt * var(--ui-scale))`, as the root declares them.
function typeSteps(): Array<{ name: string; pt: number }> {
  const root = declarations(styles, ':root').join('\n');
  return [...root.matchAll(/(--text-\d+)\s*:\s*calc\(\s*([\d.]+)pt\s*\*\s*var\(--ui-scale\)\s*\)/gu)]
    .map((match) => ({ name: match[1], pt: Number(match[2]) }));
}

const sheets = [{ name: 'styles.css', css: withoutComments(styles) }, ...componentStyles()];

test('every text size in the interface is a step of the type scale', () => {
  // A raw size cannot follow the text-size setting and cannot be checked
  // against the floor, so none may appear: not as font-size, and not inside
  // the font shorthand either. Relative units are fine, because they follow
  // whatever step their parent chose.
  const raw = /\bfont(?:-size)?\s*:[^;{}]*?\d\s*(?:px|pt|rem)\b/gu;
  for (const sheet of sheets) {
    const offenders = [...sheet.css.matchAll(raw)].map((match) => match[0].trim());
    assert.deepEqual(offenders, [], `${sheet.name} sizes text in absolute units: ${offenders.join('; ')}`);
  }
  // And the steps that are used all exist.
  const defined = new Set(typeSteps().map((step) => step.name));
  for (const sheet of sheets) {
    for (const match of sheet.css.matchAll(/var\((--text-\d+)\)/gu)) {
      assert.ok(defined.has(match[1]), `${sheet.name} uses ${match[1]}, which the root does not define`);
    }
  }
});

test('no step of the type scale is under 11pt at the default size', () => {
  const steps = typeSteps();
  assert.ok(steps.length >= 4, `expected a scale, found ${steps.length} steps`);
  for (const step of steps) {
    assert.ok(step.pt >= FLOOR_PT, `${step.name} is ${step.pt}pt, under the ${FLOOR_PT}pt floor`);
  }
  // The floor is the first step, not somewhere above it, or the smallest text
  // would be larger than it needs to be.
  assert.equal(steps[0].name, '--text-0');
  assert.equal(steps[0].pt, FLOOR_PT);
  // Steps ascend, so a higher number always means larger text.
  for (let index = 1; index < steps.length; index += 1) {
    assert.ok(steps[index].pt > steps[index - 1].pt, `${steps[index].name} is not above ${steps[index - 1].name}`);
  }
});

test('the text-size setting reaches the interface through its tokens, not a zoom', () => {
  // A zoom on the shell scaled the message frame along with everything else,
  // and the frame is the one thing the setting must not touch.
  assert.doesNotMatch(withoutComments(styles), /\bzoom\s*:/u);
  const root = declarations(styles, ':root').join('\n');
  assert.match(root, /--ui-scale\s*:\s*1\s*;/u);
  // Unstyled text takes the body step rather than the engine's default, so
  // nothing escapes the scale by having no rule of its own.
  assert.match(root, /font-size\s*:\s*var\(--text-1\)/u);
  // Icons are drawn beside labels and grow with them.
  const icon = componentStyles().find((sheet) => sheet.name === 'Icon.svelte');
  assert.ok(icon, 'Icon.svelte has a style block');
  assert.match(icon.css, /width\s*:\s*calc\(var\(--icon-size\)\s*\*\s*var\(--ui-scale/u);
});

test('density is one multiplier over spacing, derived where it is set', () => {
  // Custom properties compute on the element that declares them: a gap token
  // written on the root alone would bake in the root's density and ignore the
  // shell's. So the tokens that carry the multiplier must be declared on the
  // shell too, and each density must set the multiplier there.
  const shellBlocks = declarations(styles, '.shell');
  const gapTokens = shellBlocks.join('\n');
  for (const token of ['--gap-xs', '--gap-sm', '--gap-md', '--gap-lg', '--gap-xl', '--row-pad-block', '--card-pad-block']) {
    const rule = new RegExp(`${token}\\s*:([^;]*);`, 'u').exec(gapTokens);
    assert.ok(rule, `${token} is declared on the shell`);
    assert.match(rule[1], /var\(--density\)/u, `${token} follows density`);
    assert.match(rule[1], /var\(--ui-scale\)/u, `${token} follows the text size`);
  }
  for (const density of ['roomy', 'sardine']) {
    const block = declarations(styles, `.shell[data-density="${density}"]`).join('\n');
    assert.match(block, /--density\s*:\s*[\d.]+/u, `${density} sets the multiplier`);
  }
  const roomy = Number(/--density\s*:\s*([\d.]+)/u.exec(declarations(styles, '.shell[data-density="roomy"]').join())![1]);
  const sardine = Number(/--density\s*:\s*([\d.]+)/u.exec(declarations(styles, '.shell[data-density="sardine"]').join())![1]);
  assert.ok(sardine < 1 && 1 < roomy, 'sardine packs, roomy spreads');
  // The list rows and the sidebar rows both answer to it.
  assert.match(declarations(styles, '.thread-row')[0], /padding-block\s*:\s*var\(--row-pad-block\)/u);
  assert.match(declarations(styles, '.nav-section button')[0], /var\(--gap-sm\)/u);
});
