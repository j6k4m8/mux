/// Saved searches live on this Mac, next to the appearance choices: they are a
/// convenience of this window, not mail data, and nothing about them belongs on
/// a server. Storage is untrusted input, so everything read back is bounded and
/// typed before it reaches the search box.

import type { SavedSearch } from './searchQuery';

export type { SavedSearch };

export const SAVED_SEARCHES_KEY = 'mux-saved-searches';

const MAX_SAVED_SEARCHES = 24;
const MAX_NAME_LENGTH = 80;
const MAX_QUERY_LENGTH = 512;

function sanitize(value: unknown, maximum: number): string {
  return typeof value === 'string' ? value.trim().slice(0, maximum) : '';
}

export function readSavedSearches(): SavedSearch[] {
  let raw: string | null = null;
  try {
    raw = window.localStorage.getItem(SAVED_SEARCHES_KEY);
  } catch {
    return [];
  }
  if (!raw) return [];
  try {
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    const searches: SavedSearch[] = [];
    for (const entry of parsed) {
      if (typeof entry !== 'object' || entry === null) continue;
      const value = entry as Partial<SavedSearch>;
      const query = sanitize(value.query, MAX_QUERY_LENGTH);
      if (!query) continue;
      const name = sanitize(value.name, MAX_NAME_LENGTH) || query;
      if (searches.some((existing) => existing.query === query)) continue;
      searches.push({ name, query });
      if (searches.length === MAX_SAVED_SEARCHES) break;
    }
    return searches;
  } catch {
    return [];
  }
}

export function persistSavedSearches(searches: SavedSearch[]): void {
  try {
    window.localStorage.setItem(SAVED_SEARCHES_KEY, JSON.stringify(searches));
  } catch {
    // Persistence is optional; the list still works for this session.
  }
}

/// The newest save comes first, and saving the same query twice renames it
/// rather than filling the list with duplicates.
export function addSavedSearch(searches: SavedSearch[], query: string, name?: string): SavedSearch[] {
  const trimmedQuery = sanitize(query, MAX_QUERY_LENGTH);
  if (!trimmedQuery) return searches;
  const trimmedName = sanitize(name, MAX_NAME_LENGTH) || trimmedQuery;
  return [
    { name: trimmedName, query: trimmedQuery },
    ...searches.filter((entry) => entry.query !== trimmedQuery)
  ].slice(0, MAX_SAVED_SEARCHES);
}

export function removeSavedSearch(searches: SavedSearch[], query: string): SavedSearch[] {
  return searches.filter((entry) => entry.query !== query);
}
