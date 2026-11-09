import { Alert, Badge, Button, Group, Loader, Skeleton } from '@mantine/core'
import { useIntersection } from '@mantine/hooks'
import { IconAlertTriangle, IconChevronRight, IconMicrophone2 } from '@tabler/icons-react'
import { useEffect } from 'react'

import type { TrackSummary } from '../api'
import { duration } from '../lyrics'
import { href } from '../router'
import { Link } from './Link'

/** What lyrics a track has, as one badge. */
export function LyricsBadge({
  track,
}: {
  track: Pick<TrackSummary, 'hasWordSync' | 'hasSyncedLyrics' | 'hasPlainLyrics' | 'instrumental'>
}) {
  if (track.hasWordSync) return <Badge className="badge-word">Word synced</Badge>
  if (track.hasSyncedLyrics) return <Badge className="badge-synced">Synced</Badge>
  if (track.hasPlainLyrics) return <Badge color="gray">Plain</Badge>
  if (track.instrumental)
    return (
      <Badge color="gray" variant="outline">
        Instrumental
      </Badge>
    )
  return (
    <Badge color="gray" variant="transparent">
      No lyrics
    </Badge>
  )
}

/**
 * A track: the whole row opens its lyrics (the title is a link stretched over the row, so it also opens in a new
 * tab), and the artist's name opens the artist's tracks.
 */
function TrackRow({ track, showArtist }: { track: TrackSummary; showArtist: boolean }) {
  const artist = showArtist ? track.artistName : null
  return (
    <li className="track-row">
      <span className="track-main">
        <Link to={href.track(track.id)} className="track-title track-link">
          {track.trackName ?? 'Untitled'}
        </Link>
        <span className="track-meta">
          {artist && (
            <Link
              to={href.artist(artist)}
              className="track-artist"
              title={`All tracks by ${artist}`}
            >
              {artist}
            </Link>
          )}
          {artist && track.albumName && ' · '}
          {track.albumName}
        </span>
      </span>
      <span className="track-side">
        <LyricsBadge track={track} />
        <span className="track-duration">{duration(track.duration)}</span>
        <IconChevronRight size={16} className="track-chevron" aria-hidden="true" />
      </span>
    </li>
  )
}

/** The artists among the results, most frequent first: a way into an artist's tracks from a search. */
export function ArtistChips({ tracks }: { tracks: TrackSummary[] }) {
  const counts = new Map<string, number>()
  for (const t of tracks) {
    if (t.artistName) counts.set(t.artistName, (counts.get(t.artistName) ?? 0) + 1)
  }
  const artists = [...counts].sort((a, b) => b[1] - a[1]).slice(0, 5)
  if (artists.length === 0) return null
  return (
    <nav className="artist-chips" aria-label="Artists in the results">
      <span className="artist-chips-label">Artists</span>
      {artists.map(([name]) => (
        <Link key={name} to={href.artist(name)} className="artist-chip">
          <IconMicrophone2 size={14} aria-hidden="true" />
          <span>{name}</span>
        </Link>
      ))}
    </nav>
  )
}

export function TrackListSkeleton({ rows = 8 }: { rows?: number }) {
  return (
    <ul className="track-list" aria-busy="true" aria-label="Loading">
      {Array.from({ length: rows }, (_, i) => (
        <li key={i} className="track-row track-row-skeleton">
          <span className="track-main">
            <Skeleton height={14} width={`${String(40 + ((i * 37) % 45))}%`} radius="sm" />
            <Skeleton height={11} width={`${String(25 + ((i * 23) % 35))}%`} radius="sm" mt={8} />
          </span>
        </li>
      ))}
    </ul>
  )
}

/** Tracks, loading the next page as the end of the list comes into view. */
export function TrackList({
  tracks,
  showArtist = true,
  hasMore,
  loadingMore,
  loadMore,
  error,
}: {
  tracks: TrackSummary[]
  showArtist?: boolean
  hasMore: boolean
  loadingMore: boolean
  loadMore: () => void
  error: Error | null
}) {
  const { ref, entry } = useIntersection({ rootMargin: '800px 0px' })
  const near = entry?.isIntersecting ?? false

  useEffect(() => {
    if (near && hasMore && !loadingMore && error === null) loadMore()
  }, [near, hasMore, loadingMore, error, loadMore])

  return (
    <>
      <ul className="track-list">
        {tracks.map((t) => (
          <TrackRow key={t.id} track={t} showArtist={showArtist} />
        ))}
      </ul>
      <div ref={ref} className="list-end">
        {error !== null ? (
          <Alert
            color="red"
            variant="light"
            icon={<IconAlertTriangle size={18} />}
            title="Couldn't load more"
          >
            <Group justify="space-between" gap="sm">
              <span>{error.message}</span>
              <Button size="xs" variant="light" color="red" onClick={loadMore}>
                Try again
              </Button>
            </Group>
          </Alert>
        ) : loadingMore ? (
          <Loader size="sm" type="dots" />
        ) : hasMore ? (
          <Button variant="subtle" color="gray" onClick={loadMore}>
            Load more
          </Button>
        ) : null}
      </div>
    </>
  )
}
