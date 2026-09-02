/// How fast the interface moves, in one place.
///
/// Two consumers need the same answer: CSS, which animates modals, toasts and
/// the dropdown, and Svelte's transitions, which need a number of milliseconds
/// to animate the message list. Both read the scale from here, so the setting
/// means the same thing everywhere and there is one number to change.

export type AnimationSpeed = 'slow' | 'medium' | 'zoomie' | 'none';

export const animationChoices: Array<{ label: string; value: AnimationSpeed }> = [
  { label: 'Slow', value: 'slow' },
  { label: 'Medium', value: 'medium' },
  { label: 'Zoomie', value: 'zoomie' },
  { label: 'None', value: 'none' }
];

const SCALES: Record<AnimationSpeed, number> = {
  slow: 1.8,
  medium: 1,
  zoomie: 0.45,
  none: 0
};

/// The medium-speed durations. Every other speed is these times the scale.
export const MOTION_QUICK_MS = 120;
export const MOTION_BASE_MS = 200;
export const MOTION_SLOW_MS = 320;

export function isAnimationSpeed(value: unknown): value is AnimationSpeed {
  return animationChoices.some((choice) => choice.value === value);
}

/// Someone who has asked their Mac to reduce motion has already answered this
/// question, and their answer wins over the app's own setting.
export function prefersReducedMotion(): boolean {
  if (typeof window !== 'object' || typeof window.matchMedia !== 'function') return false;
  try {
    return window.matchMedia('(prefers-reduced-motion: reduce)').matches;
  } catch {
    return false;
  }
}

export function motionScale(speed: AnimationSpeed, reducedMotion = prefersReducedMotion()): number {
  return reducedMotion ? 0 : SCALES[speed] ?? SCALES.medium;
}

export function motionDuration(
  speed: AnimationSpeed,
  base: number = MOTION_BASE_MS,
  reducedMotion = prefersReducedMotion()
): number {
  return Math.round(base * motionScale(speed, reducedMotion));
}

export type MotionTiming = { quick: number; base: number; slow: number };

export function motionTiming(speed: AnimationSpeed, reducedMotion = prefersReducedMotion()): MotionTiming {
  return {
    quick: motionDuration(speed, MOTION_QUICK_MS, reducedMotion),
    base: motionDuration(speed, MOTION_BASE_MS, reducedMotion),
    slow: motionDuration(speed, MOTION_SLOW_MS, reducedMotion)
  };
}

type TransitionParameters = { duration: number };
type Transition = { duration: number; css: (t: number, u: number) => string };

/// A row leaving the list: it slides out of the way and its height closes up,
/// which is what lets everything below rise to take its place.
export function slideAway(node: HTMLElement, { duration }: TransitionParameters): Transition {
  const height = node.offsetHeight;
  return {
    duration,
    css: (t, u) => `
      opacity: ${t};
      transform: translateX(${u * 40}px);
      height: ${t * height}px;
      min-height: 0;
      overflow: hidden;
      pointer-events: none;
    `
  };
}

/// The toast: it rises in and drops back out. Centring lives in the `translate`
/// property rather than `transform` precisely so this can move it freely.
export function toastMotion(_node: HTMLElement, { duration }: TransitionParameters): Transition {
  return {
    duration,
    css: (t, u) => `
      opacity: ${t};
      transform: translateY(${u * 16}px);
    `
  };
}

/// A row arriving: the list opens up and the row is revealed in the gap rather
/// than appearing on top of it.
export function slideReveal(node: HTMLElement, { duration }: TransitionParameters): Transition {
  const height = node.offsetHeight;
  return {
    duration,
    css: (t, u) => `
      opacity: ${t};
      transform: translateY(${u * -8}px);
      height: ${t * height}px;
      min-height: 0;
      overflow: hidden;
    `
  };
}
