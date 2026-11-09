// A small history router (the links: components/Link.tsx): three screens, real links (open in a new tab, copy the address), and the scroll position
// of every history entry restored on back and forward.

import { useLayoutEffect, useSyncExternalStore } from 'react'

export type Route =
  | { name: 'search'; q: string }
  | { name: 'track'; id: number }
  | { name: 'artist'; artist: string }

export const href = {
  search: (q: string) => (q ? `/?${new URLSearchParams({ q }).toString()}` : '/'),
  track: (id: number) => `/lyrics/${String(id)}`,
  artist: (name: string) => `/artist/${encodeURIComponent(name)}`,
}

function parse(location: Location): Route {
  const path = location.pathname
  const track = /^\/lyrics\/(\d+)\/?$/.exec(path)
  if (track?.[1] !== undefined) return { name: 'track', id: Number(track[1]) }
  const artist = /^\/artist\/([^/]+)\/?$/.exec(path)
  if (artist?.[1] !== undefined) {
    try {
      return { name: 'artist', artist: decodeURIComponent(artist[1]) }
    } catch {
      // A malformed escape: the search.
    }
  }
  return { name: 'search', q: new URLSearchParams(location.search).get('q') ?? '' }
}

interface EntryState {
  key: string
  scroll?: number
}

const newKey = () => Math.random().toString(36).slice(2)

function entryState(): EntryState {
  const state = window.history.state as Partial<EntryState> | null
  if (typeof state?.key === 'string') return state as EntryState
  const fresh = { key: newKey() }
  window.history.replaceState(fresh, '')
  return fresh
}

// The browser would restore the scroll position before the screen is rendered.
window.history.scrollRestoration = 'manual'

let current = { route: parse(window.location), state: entryState() }
const listeners = new Set<() => void>()

function update() {
  current = { route: parse(window.location), state: entryState() }
  for (const listener of listeners) listener()
}

window.addEventListener('popstate', update)

function subscribe(listener: () => void) {
  listeners.add(listener)
  return () => listeners.delete(listener)
}

/** Goes to `to`; `replace` replaces the current entry (typing a search) instead of adding one. */
export function navigate(to: string, { replace = false } = {}) {
  const here = window.location.pathname + window.location.search
  if (to === here) return
  // Where the page was scrolled to, for coming back to it.
  window.history.replaceState({ ...entryState(), scroll: window.scrollY }, '')
  if (replace) window.history.replaceState({ key: newKey() }, '', to)
  else window.history.pushState({ key: newKey() }, '', to)
  update()
}

export function useLocation() {
  return useSyncExternalStore(subscribe, () => current)
}

/**
 * Scrolls to where the entry was left (back, forward), or to the top (a new one), once the screen is rendered
 * with what it had: cached data renders in the same pass.
 */
export function useScrollRestoration() {
  const { state } = useLocation()
  useLayoutEffect(() => {
    window.scrollTo(0, state.scroll ?? 0)
  }, [state])
}
