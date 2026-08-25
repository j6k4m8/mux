import { clearMocks } from '@tauri-apps/api/mocks';
import { cleanup } from '@testing-library/svelte';
import { afterEach, beforeEach, vi } from 'vitest';

const storageValues = new Map<string, string>();
const testStorage: Storage = {
  get length() { return storageValues.size; },
  clear: () => storageValues.clear(),
  getItem: (key) => storageValues.get(key) ?? null,
  key: (index) => [...storageValues.keys()][index] ?? null,
  removeItem: (key) => { storageValues.delete(key); },
  setItem: (key, value) => { storageValues.set(key, String(value)); }
};

Object.defineProperty(window, 'localStorage', {
  configurable: true,
  value: testStorage
});

function mediaQueryMatches(query: string): boolean {
  const maxWidth = /\(max-width:\s*(\d+)px\)/u.exec(query);
  if (maxWidth) return window.innerWidth <= Number(maxWidth[1]);
  const minWidth = /\(min-width:\s*(\d+)px\)/u.exec(query);
  if (minWidth) return window.innerWidth >= Number(minWidth[1]);
  return false;
}

Object.defineProperty(window, 'matchMedia', {
  configurable: true,
  writable: true,
  value: (query: string): MediaQueryList => ({
    matches: mediaQueryMatches(query),
    media: query,
    onchange: null,
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
    addListener: vi.fn(),
    removeListener: vi.fn(),
    dispatchEvent: vi.fn(() => true)
  })
});

Object.defineProperty(HTMLElement.prototype, 'scrollIntoView', {
  configurable: true,
  writable: true,
  value: vi.fn()
});

Object.defineProperty(document, 'execCommand', {
  configurable: true,
  writable: true,
  value: vi.fn(() => true)
});

beforeEach(() => {
  Object.defineProperty(window, 'innerWidth', { configurable: true, writable: true, value: 1440 });
  window.localStorage.clear();
  delete document.documentElement.dataset.theme;
  vi.mocked(document.execCommand).mockClear();
  vi.mocked(HTMLElement.prototype.scrollIntoView).mockClear();
});

afterEach(async () => {
  cleanup();
  await Promise.resolve();
  clearMocks();
  window.localStorage.clear();
  delete document.documentElement.dataset.theme;
});
