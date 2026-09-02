/// Light, dark, or whatever the Mac is doing.
///
/// Two different things are called "theme": what someone chose, and what is
/// actually on screen. Following the system means those differ and the second
/// can change without anyone touching Mux, so they are named apart here.

export type Theme = 'light' | 'dark';
export type ThemePreference = Theme | 'system';

export const THEME_KEY = 'mux-theme';

export const themeChoices: Array<{ label: string; value: ThemePreference }> = [
  { label: 'Light', value: 'light' },
  { label: 'Dark', value: 'dark' },
  { label: 'System', value: 'system' }
];

const DARK_QUERY = '(prefers-color-scheme: dark)';

export function isThemePreference(value: unknown): value is ThemePreference {
  return value === 'light' || value === 'dark' || value === 'system';
}

export function systemPrefersDark(): boolean {
  if (typeof window !== 'object' || typeof window.matchMedia !== 'function') return false;
  try {
    return window.matchMedia(DARK_QUERY).matches;
  } catch {
    return false;
  }
}

export function resolveTheme(preference: ThemePreference, prefersDark = systemPrefersDark()): Theme {
  if (preference === 'system') return prefersDark ? 'dark' : 'light';
  return preference;
}

/// Storage is untrusted input, and an older Mux only ever wrote 'light' or
/// 'dark' here, both of which still mean what they say.
export function readThemePreference(): ThemePreference {
  let raw: string | null = null;
  try {
    raw = window.localStorage.getItem(THEME_KEY);
  } catch {
    return 'system';
  }
  return isThemePreference(raw) ? raw : 'system';
}

export function persistThemePreference(preference: ThemePreference): void {
  try {
    window.localStorage.setItem(THEME_KEY, preference);
  } catch {
    // Persistence is optional; the choice still applies for this session.
  }
}

/// Calls back whenever the Mac's own setting changes. The caller decides
/// whether that matters, because it only does while "System" is chosen.
export function watchSystemTheme(onChange: (prefersDark: boolean) => void): () => void {
  if (typeof window !== 'object' || typeof window.matchMedia !== 'function') return () => {};
  let query: MediaQueryList;
  try {
    query = window.matchMedia(DARK_QUERY);
  } catch {
    return () => {};
  }
  const listener = (event: MediaQueryListEvent) => onChange(event.matches);
  if (typeof query.addEventListener === 'function') {
    query.addEventListener('change', listener);
    return () => query.removeEventListener('change', listener);
  }
  // Older WebKit only has the deprecated form.
  query.addListener?.(listener);
  return () => query.removeListener?.(listener);
}
