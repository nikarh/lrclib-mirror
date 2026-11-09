import { Alert, Button, Text, Title } from '@mantine/core'
import { IconAlertTriangle, IconInfoCircle, IconMicrophone2 } from '@tabler/icons-react'
import { useInfiniteQuery } from '@tanstack/react-query'
import { useCallback } from 'react'

import { artistTracks } from '../api'
import { TrackList, TrackListSkeleton } from '../components/TrackList'

/** The tracks of an artist, by name. */
export function ArtistScreen({ artist }: { artist: string }) {
  const results = useInfiniteQuery({
    queryKey: ['artist', artist],
    queryFn: ({ pageParam }) => artistTracks(artist, pageParam),
    initialPageParam: 0,
    getNextPageParam: (last, pages) =>
      last.more ? pages.reduce((n, p) => n + p.tracks.length, 0) : undefined,
  })
  const { fetchNextPage } = results
  const loadMore = useCallback(() => {
    void fetchNextPage()
  }, [fetchNextPage])

  const first = results.data?.pages[0]
  const tracks = results.data?.pages.flatMap((p) => p.tracks) ?? []

  return (
    <section aria-labelledby="artist-name">
      <header className="screen-header">
        <Text className="eyebrow">Artist</Text>
        <Title order={1} id="artist-name" className="screen-title">
          {artist}
        </Title>
        {first !== undefined && (
          <Text c="dimmed" size="sm">
            {first.total === 1 ? '1 track' : `${first.total.toLocaleString()} tracks`}
          </Text>
        )}
      </header>

      {first?.approximate === true && (
        <Alert variant="light" color="gray" icon={<IconInfoCircle size={18} />} mb="md">
          This server can’t list an artist’s tracks: these are what a search for the name finds.
        </Alert>
      )}

      {results.isPending ? (
        <TrackListSkeleton />
      ) : results.isError && !results.isFetchNextPageError ? (
        <Alert
          color="red"
          variant="light"
          icon={<IconAlertTriangle size={18} />}
          title="Couldn’t load the artist’s tracks"
        >
          <Text size="sm" mb="sm">
            {results.error.message}
          </Text>
          <Button
            size="xs"
            variant="light"
            color="red"
            onClick={() => {
              void results.refetch()
            }}
          >
            Try again
          </Button>
        </Alert>
      ) : tracks.length === 0 ? (
        <section className="empty-state">
          <IconMicrophone2 size={40} stroke={1.3} className="empty-icon" />
          <Text fw={600}>No tracks by “{artist}”</Text>
        </section>
      ) : (
        <TrackList
          tracks={tracks}
          showArtist={false}
          hasMore={results.hasNextPage}
          loadingMore={results.isFetchingNextPage}
          loadMore={loadMore}
          error={results.isFetchNextPageError ? results.error : null}
        />
      )}
    </section>
  )
}
