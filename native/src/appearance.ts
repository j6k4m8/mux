/// Everything that decides how Mux looks, in one place: the shape of the
/// choice, the options offered for it, how it is read back from storage, and
/// how it reaches the document. Kept out of the components so the settings
/// screen and the mailbox agree on it without one importing the other.

import { isAnimationSpeed, motionScale } from './motion';
import type { AnimationSpeed } from './motion';
import type { Theme } from './theme';

export type { AnimationSpeed };

/// Where the accent color comes from. 'account' follows the conversation being
/// read, so the color says whose mail this is; the rest are fixed choices.
export type AccentChoice = 'account' | 'indigo' | 'teal' | 'violet' | 'amber' | 'rose';

export type Density = 'roomy' | 'default' | 'sardine';
export type SwipeAction = 'archive' | 'delete' | 'snooze' | 'star' | 'unread' | 'none';

export type Appearance = {
  scale: number;
  font: string;
  density: Density;
  toolbarIcons: boolean;
  toolbarText: boolean;
  toolbarShortcuts: boolean;
  toolbarCollapseNarrow: boolean;
  swipeLeft: SwipeAction;
  swipeRight: SwipeAction;
  /// The body line under the participants and the subject in the mail list.
  /// Nothing to do with railPreview, which is the reader's hover preview: this
  /// one is always on screen, and it is a third of every row's height.
  listSnippet: boolean;
  railPreview: boolean;
  animation: AnimationSpeed;
  accent: AccentChoice;
};

export const APPEARANCE_KEY = 'mux-appearance';
export const DEFAULT_FONT =
  'Inter, ui-sans-serif, -apple-system, BlinkMacSystemFont, "SF Pro Text", "Segoe UI", sans-serif';

export const DEFAULT_APPEARANCE: Appearance = {
  scale: 1,
  font: DEFAULT_FONT,
  density: 'default',
  toolbarIcons: true,
  toolbarText: true,
  toolbarShortcuts: false,
  toolbarCollapseNarrow: true,
  swipeLeft: 'archive',
  swipeRight: 'snooze',
  listSnippet: true,
  railPreview: true,
  animation: 'medium',
  accent: 'account'
};

export const fontChoices: Array<{ label: string; value: string }> = [
  { label: 'Inter', value: DEFAULT_FONT },
  { label: 'System', value: '-apple-system, BlinkMacSystemFont, "SF Pro Text", system-ui, sans-serif' },
  { label: 'Serif', value: 'ui-serif, "New York", Georgia, "Times New Roman", serif' },
  { label: 'Mono', value: 'ui-monospace, "SF Mono", Menlo, Consolas, monospace' }
];

export const textSizes: Array<{ label: string; value: number }> = [
  { label: 'Small', value: 0.9 },
  { label: 'Default', value: 1 },
  { label: 'Large', value: 1.15 },
  { label: 'Larger', value: 1.3 }
];

export const refreshChoices: Array<{ label: string; value: number }> = [
  { label: '30s', value: 30 },
  { label: '1 min', value: 60 },
  { label: '5 min', value: 300 },
  { label: '15 min', value: 900 },
  { label: '1 hour', value: 3600 }
];

export const swipeChoices: Array<{ label: string; value: SwipeAction }> = [
  { label: 'Archive', value: 'archive' },
  { label: 'Trash', value: 'delete' },
  { label: 'Snooze', value: 'snooze' },
  { label: 'Star', value: 'star' },
  { label: 'Unread', value: 'unread' },
  { label: 'Nothing', value: 'none' }
];

export const densityChoices: Array<{ label: string; value: Density }> = [
  { label: 'Roomy', value: 'roomy' },
  { label: 'Default', value: 'default' },
  { label: 'Sardinemode', value: 'sardine' }
];

/// Each fixed accent has a light and a dark form. One hex cannot be both:
/// what reads as a color on white is nearly black on a dark ground.
const ACCENT_COLORS: Record<Exclude<AccentChoice, 'account'>, { light: string; dark: string }> = {
  indigo: { light: '#4c63ee', dark: '#7d8cff' },
  teal: { light: '#0f8a76', dark: '#4fd1b5' },
  violet: { light: '#7c4ddb', dark: '#b39aff' },
  amber: { light: '#b3730a', dark: '#f0b34e' },
  rose: { light: '#c93b63', dark: '#ff8fab' }
};

export const accentChoices: Array<{ label: string; value: AccentChoice }> = [
  { label: 'Current account', value: 'account' },
  { label: 'Indigo', value: 'indigo' },
  { label: 'Teal', value: 'teal' },
  { label: 'Violet', value: 'violet' },
  { label: 'Amber', value: 'amber' },
  { label: 'Rose', value: 'rose' }
];

/// The dot beside a choice, in the theme it will actually be seen in. Following
/// the message has no fixed color to show.
export function accentSwatch(choice: AccentChoice, theme: Theme): string {
  return choice === 'account' ? '' : ACCENT_COLORS[choice][theme];
}

export function isAccentChoice(value: unknown): value is AccentChoice {
  return accentChoices.some((choice) => choice.value === value);
}

/// Null means "whatever the stylesheet already says", which is how following
/// the message behaves before anything is selected: the theme's own accent is
/// already right for the theme, and inventing a stand-in would make the color
/// jump on the first selection for no reason.
export function accentColorFor(
  choice: AccentChoice,
  theme: Theme,
  accountColor: string | null
): string | null {
  if (choice === 'account') return accountColor;
  return ACCENT_COLORS[choice][theme];
}

/// Only the base accent is set. The stylesheet derives the hover and tint
/// variants from it, so one color is all any of this has to supply.
export function applyAccentToRoot(
  choice: AccentChoice,
  theme: Theme,
  accountColor: string | null
): void {
  const color = accentColorFor(choice, theme, accountColor);
  if (color) document.documentElement.style.setProperty('--accent', color);
  else document.documentElement.style.removeProperty('--accent');
}

export function swipeLabel(action: SwipeAction): string {
  return swipeChoices.find((choice) => choice.value === action)?.label ?? 'Nothing';
}

/// Scale, font, and how fast things move are the choices the whole document
/// answers to; the rest are read by the components that care.
export function applyAppearanceToRoot(next: Appearance): void {
  const root = document.documentElement;
  root.style.setProperty('--ui-scale', String(next.scale));
  root.style.setProperty('--app-font', next.font);
  root.style.setProperty('--motion-scale', String(motionScale(next.animation)));
}

export function persistAppearance(next: Appearance): void {
  try {
    window.localStorage.setItem(APPEARANCE_KEY, JSON.stringify(next));
  } catch {
    // Persistence is optional; the choice still applies for this session.
  }
}

/// Storage is not trusted input: anything read back is bounded and typed before
/// it reaches the interface.
export function readSavedAppearance(): Appearance {
  let raw: string | null = null;
  try {
    raw = window.localStorage.getItem(APPEARANCE_KEY);
  } catch {
    return { ...DEFAULT_APPEARANCE };
  }
  if (!raw) return { ...DEFAULT_APPEARANCE };
  try {
    const parsed: unknown = JSON.parse(raw);
    if (typeof parsed !== 'object' || parsed === null) throw new Error('shape');
    const value = parsed as Partial<Appearance>;
    const scale = typeof value.scale === 'number' && Number.isFinite(value.scale)
      ? Math.min(2, Math.max(0.75, value.scale))
      : 1;
    const font = typeof value.font === 'string' && value.font.trim() && value.font.length <= 200
      ? value.font
      : DEFAULT_FONT;
    const density: Density = value.density === 'roomy' || value.density === 'sardine'
      ? value.density
      : 'default';
    const flag = (candidate: unknown, fallback: boolean) =>
      typeof candidate === 'boolean' ? candidate : fallback;
    const swipe = (candidate: unknown, fallback: SwipeAction): SwipeAction =>
      swipeChoices.some((choice) => choice.value === candidate)
        ? (candidate as SwipeAction)
        : fallback;
    return {
      scale,
      font,
      density,
      toolbarIcons: flag(value.toolbarIcons, true),
      toolbarText: flag(value.toolbarText, true),
      toolbarShortcuts: flag(value.toolbarShortcuts, false),
      toolbarCollapseNarrow: flag(value.toolbarCollapseNarrow, true),
      swipeLeft: swipe(value.swipeLeft, 'archive'),
      swipeRight: swipe(value.swipeRight, 'snooze'),
      listSnippet: flag(value.listSnippet, true),
      railPreview: flag(value.railPreview, true),
      animation: isAnimationSpeed(value.animation) ? value.animation : 'medium',
      accent: isAccentChoice(value.accent) ? value.accent : 'account'
    };
  } catch {
    return { ...DEFAULT_APPEARANCE };
  }
}
