import assert from 'node:assert/strict';
import { test } from 'vitest';

import {
  animationChoices,
  isAnimationSpeed,
  MOTION_BASE_MS,
  motionDuration,
  motionScale,
  motionTiming,
  slideAway,
  slideReveal
} from './motion';
import { DEFAULT_APPEARANCE, readSavedAppearance, APPEARANCE_KEY } from './appearance';

const row = { offsetHeight: 84 } as HTMLElement;

test('every offered speed is a real speed, and slower is slower', () => {
  for (const choice of animationChoices) assert.ok(isAnimationSpeed(choice.value), choice.value);
  assert.ok(!isAnimationSpeed('brisk'));
  const durations = animationChoices.map((choice) => motionDuration(choice.value, MOTION_BASE_MS, false));
  assert.deepEqual(durations, [...durations].sort((left, right) => right - left));
  assert.equal(motionDuration('none', MOTION_BASE_MS, false), 0);
  assert.equal(motionDuration('medium', MOTION_BASE_MS, false), MOTION_BASE_MS);
});

test('a Mac asking for reduced motion overrules whatever is chosen here', () => {
  for (const choice of animationChoices) {
    assert.equal(motionScale(choice.value, true), 0, choice.value);
    assert.deepEqual(motionTiming(choice.value, true), { quick: 0, base: 0, slow: 0 });
  }
});

test('a row leaves by getting out of the way and closing the gap it left', () => {
  const leaving = slideAway(row, { duration: 200 });
  assert.equal(leaving.duration, 200);
  const settled = leaving.css(1, 0);
  const gone = leaving.css(0, 1);
  assert.match(settled, /height: 84px/u);
  assert.match(settled, /translateX\(0px\)/u);
  assert.match(gone, /height: 0px/u);
  assert.match(gone, /opacity: 0/u);
  // Nothing can be clicked while it is on its way out.
  assert.match(gone, /pointer-events: none/u);
});

test('a row arrives by opening a gap and appearing in it', () => {
  const arriving = slideReveal(row, { duration: 200 });
  assert.match(arriving.css(0, 1), /height: 0px/u);
  assert.match(arriving.css(1, 0), /height: 84px/u);
});

test('the stored speed is read back, and anything else falls to the default', () => {
  assert.equal(DEFAULT_APPEARANCE.animation, 'medium');
  window.localStorage.setItem(APPEARANCE_KEY, JSON.stringify({ animation: 'zoomie' }));
  assert.equal(readSavedAppearance().animation, 'zoomie');
  window.localStorage.setItem(APPEARANCE_KEY, JSON.stringify({ animation: 'instantaneous' }));
  assert.equal(readSavedAppearance().animation, 'medium');
});
