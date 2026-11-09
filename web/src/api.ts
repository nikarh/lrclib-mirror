// The lookups of lrclib-mirror's API. The paged search and the artist listing (`/api/mirror/*`) are the mirror's
// own; against a server without them (an older mirror, or LRCLIB itself) the search falls back to `/api/v2/search`
// and then `/api/search`, which answer one page of 20.

/** A track without its lyrics (`/api/v2/search`). */
export interface TrackSummary {
  id: number
  trackName: string | null
  artistName: string | null
  albumName: string | null
  duration: number | null
  instrumental: boolean
  hasWordSync: boolean
  hasPlainLyrics: boolean
  hasSyncedLyrics: boolean
  hasLyricsfile: boolean
}

/** A track with its lyrics (`/api/get/{id}`, `/api/search`). */
export interface Track {
  id: number
  trackName: string | null
  artistName: string | null
  albumName: string | null
  duration: number | null
  instrumental: boolean
  hasWordSync: boolean
  plainLyrics: string | null
  syncedLyrics: string | null
  lyricsfile: string | null
}

export interface Page {
  tracks: TrackSummary[]
  /** Whether there may be more after this page. */
  more: boolean
}

export interface ArtistPage extends Page {
  total: number
  /** The server can't list an artist's tracks: these are what a search finds (one page). */
  approximate: boolean
}

export class ApiError extends Error {
  readonly status: number
  /** LRCLIB's error name (`TrackNotFound`, `ValidationError`...), when the body has one. */
  readonly kind: string | null

  constructor(message: string, status: number, kind: string | null) {
    super(message)
    this.name = 'ApiError'
    this.status = status
    this.kind = kind
  }
}

export const PAGE_SIZE = 40

async function get<T>(path: string, params: Record<string, string | number | undefined> = {}) {
  const search = new URLSearchParams()
  for (const [key, value] of Object.entries(params)) {
    if (value !== undefined && value !== '') search.set(key, String(value))
  }
  const query = search.toString()
  const response = await fetch(query ? `${path}?${query}` : path, {
    headers: { Accept: 'application/json' },
  })
  if (!response.ok) {
    let message = `${String(response.status)} ${response.statusText}`
    let kind: string | null = null
    try {
      const body = (await response.json()) as { message?: unknown; name?: unknown }
      if (typeof body.message === 'string') message = body.message
      if (typeof body.name === 'string') kind = body.name
    } catch {
      // Not JSON: an unknown route, or a proxy's error page.
    }
    throw new ApiError(message, response.status, kind)
  }
  return (await response.json()) as T
}

/** A route the server doesn't have (as opposed to a lookup that found nothing). */
const isMissingRoute = (e: unknown) => e instanceof ApiError && e.status === 404 && e.kind === null

export const getTrack = (id: number) => get<Track>(`/api/get/${String(id)}`)

const summarize = (t: Track): TrackSummary => ({
  id: t.id,
  trackName: t.trackName,
  artistName: t.artistName,
  albumName: t.albumName,
  duration: t.duration,
  instrumental: t.instrumental,
  hasWordSync: t.hasWordSync,
  hasPlainLyrics: t.plainLyrics !== null,
  hasSyncedLyrics: t.syncedLyrics !== null,
  hasLyricsfile: t.lyricsfile !== null,
})

type SearchApi = 'mirror' | 'v2' | 'v1'
// What the server has, learnt from the first search.
let searchApi: SearchApi = 'mirror'

/** One page of what a search for `q` finds, best first. */
export async function search(q: string, offset: number): Promise<Page> {
  for (;;) {
    try {
      switch (searchApi) {
        case 'mirror': {
          const tracks = await get<TrackSummary[]>('/api/mirror/search', {
            q,
            offset,
            limit: PAGE_SIZE,
          })
          return { tracks, more: tracks.length === PAGE_SIZE }
        }
        case 'v2':
          if (offset > 0) return { tracks: [], more: false }
          return { tracks: await get<TrackSummary[]>('/api/v2/search', { q }), more: false }
        case 'v1': {
          if (offset > 0) return { tracks: [], more: false }
          const tracks = await get<Track[]>('/api/search', { q })
          return { tracks: tracks.map(summarize), more: false }
        }
      }
    } catch (e) {
      if (!isMissingRoute(e) || searchApi === 'v1') throw e
      searchApi = searchApi === 'mirror' ? 'v2' : 'v1'
    }
  }
}

let artistApi = true

/** One page of an artist's tracks, by name. */
export async function artistTracks(artist: string, offset: number): Promise<ArtistPage> {
  if (artistApi) {
    try {
      const page = await get<{ total: number; tracks: TrackSummary[] }>('/api/mirror/artist', {
        artist_name: artist,
        offset,
        limit: PAGE_SIZE,
      })
      return {
        ...page,
        more: offset + page.tracks.length < page.total,
        approximate: false,
      }
    } catch (e) {
      if (!isMissingRoute(e)) throw e
      artistApi = false
    }
  }
  // Without the listing: what a search for the name finds, by this artist.
  if (offset > 0) return { tracks: [], total: 0, more: false, approximate: true }
  const wanted = artist.trim().toLocaleLowerCase()
  const { tracks } = await search(artist, 0)
  const own = tracks.filter((t) => t.artistName?.trim().toLocaleLowerCase() === wanted)
  return { tracks: own, total: own.length, more: false, approximate: true }
}
