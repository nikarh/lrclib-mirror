import { Alert, Badge, Button, Group, Loader, Skeleton } from '@mantine/core'
import { useIntersection } from '@mantine/hooks'
import { IconAlertTriangle, IconChevronRight } from '@tabler/icons-react'
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

/** A track: the whole row opens its lyrics (and, as a link, in a new tab too). */
function TrackRow({ track, showArtist }: { track: TrackSummary; showArtist: boolean }) {
  const secondary = [showArtist ? track.artistName : null, track.albumName].filter(
    (s): s is string => !!s,
  )
  return (
    <li>
      <Link to={href.track(track.id)} className="track-row">
        <span className="track-main">
          <span className="track-title">{track.trackName ?? 'Untitled'}</span>
          <span className="track-meta">{secondary.join(' · ')}</span>
        </span>
        <span className="track-side">
          <LyricsBadge track={track} />
          <span className="track-duration">{duration(track.duration)}</span>
          <IconChevronRight size={16} className="track-chevron" aria-hidden="true" />
        </span>
      </Link>
    </li>
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
