/// Everything that decides how Mux looks, in one place: the shape of the
/// choice, the options offered for it, how it is read back from storage, and
/// how it reaches the document. Kept out of the components so the settings
/// screen and the mailbox agree on it without one importing the other.

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
  railPreview: boolean;
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
  railPreview: true
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

export function swipeLabel(action: SwipeAction): string {
  return swipeChoices.find((choice) => choice.value === action)?.label ?? 'Nothing';
}

/// Scale and font are the two choices the whole document answers to; the rest
/// are read by the components that care.
export function applyAppearanceToRoot(next: Appearance): void {
  const root = document.documentElement;
  root.style.setProperty('--ui-scale', String(next.scale));
  root.style.setProperty('--app-font', next.font);
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
      railPreview: flag(value.railPreview, true)
    };
  } catch {
    return { ...DEFAULT_APPEARANCE };
  }
}
