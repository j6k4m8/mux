/// Two-finger horizontal swipe, as a Svelte action.
///
/// A pointer drag cannot leave the list without losing the gesture, so this
/// reads wheel deltas: macOS reports a trackpad swipe as horizontal wheel
/// movement. Everything continuous is kept out of the component's reactive
/// state and written straight to the DOM, because a wheel event arrives up to
/// 120 times a second and invalidating a component that often re-runs whatever
/// else it renders.

export type SwipeSide = 'left' | 'right';

export type SwipeGestureState = {
  swiping: boolean;
  side: SwipeSide;
  armed: boolean;
};

export type SwipeGestureOptions = {
  /// False when neither direction is bound to an action, so nothing should move.
  enabled: boolean;
  /// Called only when something discrete changes, never per frame.
  onstate: (state: SwipeGestureState) => void;
  oncommit: (side: SwipeSide) => void;
};

/// Travel before a gesture commits to an axis. Below this it could still be
/// either, so nothing is decided and nothing moves.
const AXIS_LOCK_PX = 14;
/// How much more horizontal than vertical a gesture must be to count as a
/// swipe. Scrolling a list is rarely this lopsided.
const AXIS_RATIO = 1.6;
/// Past this the gesture is not ambiguous, so it commits at once rather than
/// waiting to confirm the fingers lifted.
const COMMIT_PX = 158;
/// Past this a released gesture commits; short of it the row slides home.
const TRIGGER_PX = 110;
const MAX_OFFSET_PX = 170;
/// Quiet before a gesture with nothing left to protect settles.
const SETTLE_MS = 200;
/// The longest a partial swipe waits before sliding back. A fixed value cannot
/// work: long enough to survive a slow drag is long enough to hang after the
/// fingers lift, so the wait is measured from the drag's own rhythm.
const MAX_QUIET_MS = 450;
const QUIET_INTERVALS = 2.5;

type Gesture = {
  node: HTMLElement;
  options: SwipeGestureOptions;
  offset: number;
  axis: 'undecided' | 'horizontal' | 'vertical';
  travelX: number;
  travelY: number;
  gapMs: number;
  lastAt: number;
  side: SwipeSide;
  armed: boolean;
  frame: number;
  timer: number | undefined;
};

/// Only one row swipes at a time. A swiped row slides sideways out from under
/// the pointer, so the rest of one physical gesture arrives on its neighbours;
/// those events have to keep this gesture alive rather than start another.
let active: Gesture | null = null;

function paint(gesture: Gesture) {
  gesture.frame = 0;
  gesture.node.style.setProperty('--swipe-offset', `${gesture.offset}px`);
}

function schedulePaint(gesture: Gesture) {
  if (typeof requestAnimationFrame !== 'function') {
    paint(gesture);
    return;
  }
  // Several wheel events can land in one frame; the row only needs moving once.
  if (!gesture.frame) gesture.frame = requestAnimationFrame(() => paint(gesture));
}

function clear(gesture: Gesture) {
  window.clearTimeout(gesture.timer);
  if (gesture.frame && typeof cancelAnimationFrame === 'function') {
    cancelAnimationFrame(gesture.frame);
  }
  // Dropping the property lets the row animate home under its own transition.
  gesture.node.style.removeProperty('--swipe-offset');
  gesture.options.onstate({ swiping: false, side: gesture.side, armed: false });
  if (active === gesture) active = null;
}

function end(gesture: Gesture) {
  const { offset, axis, options } = gesture;
  clear(gesture);
  if (axis !== 'horizontal' || Math.abs(offset) < TRIGGER_PX) return;
  options.oncommit(offset < 0 ? 'left' : 'right');
}

function armTimer(gesture: Gesture) {
  const armed = gesture.axis === 'horizontal' && Math.abs(gesture.offset) >= TRIGGER_PX;
  // Until an interval has been measured, assume a slow drag. Guessing fast
  // would end the gesture before its second event arrived.
  const paced = gesture.gapMs ? gesture.gapMs * QUIET_INTERVALS : MAX_QUIET_MS;
  const quiet = armed || gesture.axis !== 'horizontal'
    ? SETTLE_MS
    : Math.min(MAX_QUIET_MS, Math.max(SETTLE_MS, paced));
  window.clearTimeout(gesture.timer);
  gesture.timer = window.setTimeout(() => end(gesture), quiet);
}

export function swipeGesture(node: HTMLElement, options: SwipeGestureOptions) {
  let current = options;

  function onWheel(event: WheelEvent) {
    if (!current.enabled) return;

    if (!active) {
      active = {
        node,
        options: current,
        offset: 0,
        axis: 'undecided',
        travelX: 0,
        travelY: 0,
        gapMs: 0,
        lastAt: 0,
        side: 'left',
        armed: false,
        frame: 0,
        timer: undefined
      };
    }
    const gesture = active;

    // A drag's own rhythm is the only thing separating "still moving slowly"
    // from "let go", so the interval between events is smoothed and reused.
    const at = typeof performance === 'object' ? performance.now() : Date.now();
    if (gesture.lastAt) {
      const gap = at - gesture.lastAt;
      gesture.gapMs = gesture.gapMs ? gesture.gapMs * 0.7 + gap * 0.3 : gap;
    }
    gesture.lastAt = at;
    armTimer(gesture);
    if (gesture.node !== node) return;

    gesture.travelX += Math.abs(event.deltaX);
    gesture.travelY += Math.abs(event.deltaY);

    // The axis is decided once per gesture from accumulated travel. Deciding it
    // per event reads momentum as a swipe: after a vertical flick macOS keeps
    // sending events whose deltaY has decayed to nearly nothing while a small
    // deltaX remains, and each one alone looks horizontal.
    if (gesture.axis === 'undecided') {
      if (Math.max(gesture.travelX, gesture.travelY) < AXIS_LOCK_PX) return;
      gesture.axis = gesture.travelX > gesture.travelY * AXIS_RATIO ? 'horizontal' : 'vertical';
    }
    // A vertical gesture keeps scrolling the list for the rest of its life.
    if (gesture.axis === 'vertical') return;

    event.preventDefault();
    // Swiping left reports a positive deltaX, so the row follows the fingers.
    gesture.offset = Math.max(-MAX_OFFSET_PX, Math.min(MAX_OFFSET_PX, gesture.offset - event.deltaX));
    schedulePaint(gesture);

    const side: SwipeSide = gesture.offset < 0 ? 'left' : 'right';
    const armed = Math.abs(gesture.offset) >= TRIGGER_PX;
    if (side !== gesture.side || armed !== gesture.armed || !gesture.armed) {
      gesture.side = side;
      gesture.armed = armed;
      current.onstate({ swiping: true, side, armed });
    }

    // Decisive: act now. Nothing is learned by waiting for a gesture this far in.
    if (Math.abs(gesture.offset) >= COMMIT_PX) {
      end(gesture);
      return;
    }
    armTimer(gesture);
  }

  node.addEventListener('wheel', onWheel, { passive: false });

  return {
    update(next: SwipeGestureOptions) {
      current = next;
      if (active?.node === node) active.options = next;
    },
    destroy() {
      node.removeEventListener('wheel', onWheel);
      if (active?.node === node) clear(active);
    }
  };
}

export const swipeGestureThresholds = { TRIGGER_PX, COMMIT_PX, MAX_OFFSET_PX };
