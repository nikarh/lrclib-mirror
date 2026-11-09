import { Alert, Button, Text, Title } from '@mantine/core'
import { IconAlertTriangle, IconSearch } from '@tabler/icons-react'
import { useInfiniteQuery } from '@tanstack/react-query'
import { useCallback } from 'react'

import { search } from '../api'
import { TrackList, TrackListSkeleton } from '../components/TrackList'

export function SearchScreen({ q }: { q: string }) {
  const query = q.trim()
  const results = useInfiniteQuery({
    queryKey: ['search', query],
    queryFn: ({ pageParam }) => search(query, pageParam),
    initialPageParam: 0,
    getNextPageParam: (last, pages) =>
      last.more ? pages.reduce((n, p) => n + p.tracks.length, 0) : undefined,
    enabled: query !== '',
  })
  const { fetchNextPage } = results
  const loadMore = useCallback(() => {
    void fetchNextPage()
  }, [fetchNextPage])

  if (query === '') {
    return (
      <section className="empty-state empty-home">
        <Title order={1} className="home-title">
          Find lyrics
        </Title>
        <Text c="dimmed" maw={420} ta="center">
          Search by song, artist or album, in any order. Press <kbd>/</kbd> to search from anywhere.
        </Text>
      </section>
    )
  }

  if (results.isPending) return <TrackListSkeleton />

  if (results.isError && !results.isFetchNextPageError) {
    return (
      <Alert
        color="red"
        variant="light"
        icon={<IconAlertTriangle size={18} />}
        title="Search failed"
        mt="md"
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
    )
  }

  const tracks = results.data.pages.flatMap((p) => p.tracks)
  if (tracks.length === 0) {
    return (
      <section className="empty-state">
        <IconSearch size={40} stroke={1.3} className="empty-icon" />
        <Text fw={600}>Nothing found for “{query}”</Text>
        <Text c="dimmed" size="sm">
          Try fewer words, or another spelling.
        </Text>
      </section>
    )
  }

  return (
    <section aria-label={`Results for ${query}`}>
      <TrackList
        tracks={tracks}
        hasMore={results.hasNextPage}
        loadingMore={results.isFetchingNextPage}
        loadMore={loadMore}
        error={results.isFetchNextPageError ? results.error : null}
      />
    </section>
  )
}
