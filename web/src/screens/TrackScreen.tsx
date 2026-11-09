import { Alert, Button, Group, Menu, SegmentedControl, Skeleton, Text, Title } from '@mantine/core'
import { useLocalStorage } from '@mantine/hooks'
import {
  IconAlertTriangle,
  IconChevronDown,
  IconCopy,
  IconDownload,
  IconMusicOff,
} from '@tabler/icons-react'
import { useQuery } from '@tanstack/react-query'
import { useEffect, useMemo } from 'react'

import { ApiError, type Track, getTrack } from '../api'
import { copyAndTell, download } from '../clipboard'
import { LyricsLines } from '../components/LyricsLines'
import { LyricsBadge } from '../components/TrackList'
import { duration, parseLrc, plainLines } from '../lyrics'
import { Link } from '../components/Link'
import { href } from '../router'

type View = 'synced' | 'plain' | 'lyricsfile'

const VIEW_LABELS: Record<View, string> = {
  synced: 'Synced',
  plain: 'Plain',
  lyricsfile: 'Lyricsfile',
}

/** A file name for the track's lyrics: `Artist - Title.ext`, without what file systems refuse. */
function fileName(track: Track, ext: string) {
  const base =
    [track.artistName, track.trackName].filter(Boolean).join(' - ') || `track-${String(track.id)}`
  // oxlint-disable-next-line no-control-regex -- control characters are what's removed
  return `${base.replace(/[\\/:*?"<>|\u0000-\u001f]+/g, '_').trim()}.${ext}`
}

function Actions({ track }: { track: Track }) {
  const texts = [
    {
      key: 'plain',
      label: 'Plain lyrics',
      ext: 'txt',
      text: track.plainLyrics,
      type: 'text/plain',
    },
    {
      key: 'lrc',
      label: 'Synced lyrics (LRC)',
      ext: 'lrc',
      text: track.syncedLyrics,
      type: 'text/plain',
    },
    {
      key: 'lf',
      label: 'Lyricsfile (YAML)',
      ext: 'yaml',
      text: track.lyricsfile,
      type: 'application/yaml',
    },
  ] as const
  const any = texts.some((t) => t.text !== null)
  return (
    <Group gap={6} wrap="nowrap">
      <Menu>
        <Menu.Target>
          <Button
            size="compact-md"
            variant="default"
            radius="xl"
            leftSection={<IconCopy size={15} />}
            rightSection={<IconChevronDown size={14} />}
            disabled={!any}
          >
            Copy
          </Button>
        </Menu.Target>
        <Menu.Dropdown>
          {texts.map((t) => (
            <Menu.Item
              key={t.key}
              disabled={t.text === null}
              onClick={() => {
                if (t.text !== null) copyAndTell(t.text, t.label.toLowerCase())
              }}
            >
              {t.label}
            </Menu.Item>
          ))}
        </Menu.Dropdown>
      </Menu>
      <Menu>
        <Menu.Target>
          <Button
            size="compact-md"
            variant="default"
            radius="xl"
            leftSection={<IconDownload size={15} />}
            rightSection={<IconChevronDown size={14} />}
            disabled={!any}
          >
            Save
          </Button>
        </Menu.Target>
        <Menu.Dropdown>
          {texts.map((t) => (
            <Menu.Item
              key={t.key}
              disabled={t.text === null}
              rightSection={
                <Text size="xs" c="dimmed" ff="monospace">
                  .{t.ext}
                </Text>
              }
              onClick={() => {
                if (t.text !== null) download(t.text, fileName(track, t.ext), t.type)
              }}
            >
              {t.label}
            </Menu.Item>
          ))}
        </Menu.Dropdown>
      </Menu>
    </Group>
  )
}

function Lyrics({ track }: { track: Track }) {
  const [preferred, setPreferred] = useLocalStorage<View>({
    key: 'lyrics-view',
    defaultValue: 'synced',
  })
  const views = (['synced', 'plain', 'lyricsfile'] as const).filter((v) =>
    v === 'synced'
      ? track.syncedLyrics !== null
      : v === 'plain'
        ? track.plainLyrics !== null
        : track.lyricsfile !== null,
  )
  const lyricsViews = views.filter((v) => v !== 'lyricsfile')
  const view: View | undefined = views.includes(preferred)
    ? preferred
    : (lyricsViews[0] ?? views[0])

  const lines = useMemo(
    () =>
      view === 'synced'
        ? parseLrc(track.syncedLyrics ?? '')
        : view === 'plain'
          ? plainLines(track.plainLyrics ?? '')
          : [],
    [view, track],
  )

  if (view === undefined || lyricsViews.length === 0) {
    return (
      <section className="empty-state">
        <IconMusicOff size={40} stroke={1.3} className="empty-icon" />
        <Text fw={600}>
          {track.instrumental ? 'This track is instrumental' : 'No lyrics for this track yet'}
        </Text>
      </section>
    )
  }

  return (
    <>
      <div className="lyrics-toolbar">
        {views.length > 1 ? (
          <SegmentedControl
            size="sm"
            radius="xl"
            value={view}
            onChange={(v) => {
              setPreferred(v)
            }}
            data={views.map((v) => ({ value: v, label: VIEW_LABELS[v] }))}
          />
        ) : (
          <Text size="sm" c="dimmed" fw={500}>
            {VIEW_LABELS[view]} lyrics
          </Text>
        )}
        <Text size="xs" c="dimmed" className="lyrics-hint" visibleFrom="sm">
          Click lines to pick them, shift-click for a range
        </Text>
      </div>
      {view === 'lyricsfile' ? (
        <pre className="lyricsfile">{track.lyricsfile}</pre>
      ) : (
        <LyricsLines key={view} lines={lines} timed={view === 'synced'} />
      )}
    </>
  )
}

export function TrackScreen({ id }: { id: number }) {
  const result = useQuery({ queryKey: ['track', id], queryFn: () => getTrack(id) })
  const track = result.data

  useEffect(() => {
    if (track)
      document.title = `${track.trackName ?? 'Untitled'} · ${track.artistName ?? ''} · LRCLIB`
  }, [track])

  if (result.isPending) {
    return (
      <div aria-busy="true" aria-label="Loading">
        <Skeleton height={34} width="60%" mt="md" />
        <Skeleton height={18} width="35%" mt={14} />
        <Skeleton height={14} width="20%" mt={10} />
        {Array.from({ length: 12 }, (_, i) => (
          <Skeleton key={i} height={16} width={`${String(35 + ((i * 29) % 50))}%`} mt={18} />
        ))}
      </div>
    )
  }

  if (result.isError) {
    const notFound = result.error instanceof ApiError && result.error.status === 404
    return (
      <Alert
        color={notFound ? 'gray' : 'red'}
        variant="light"
        icon={<IconAlertTriangle size={18} />}
        title={notFound ? 'No such track' : 'Couldn’t load the lyrics'}
        mt="md"
      >
        {notFound ? (
          <Text size="sm">There is no track {id} in this copy of LRCLIB.</Text>
        ) : (
          <>
            <Text size="sm" mb="sm">
              {result.error.message}
            </Text>
            <Button
              size="xs"
              variant="light"
              color="red"
              onClick={() => {
                void result.refetch()
              }}
            >
              Try again
            </Button>
          </>
        )}
      </Alert>
    )
  }

  const t = result.data
  return (
    <article>
      <header className="screen-header track-header">
        <Title order={1} className="screen-title">
          {t.trackName ?? 'Untitled'}
        </Title>
        <Text className="track-byline">
          {t.artistName ? (
            <Link to={href.artist(t.artistName)} className="artist-link">
              {t.artistName}
            </Link>
          ) : (
            'Unknown artist'
          )}
          {t.albumName && <span className="track-album"> · {t.albumName}</span>}
        </Text>
        <Group gap={8} mt={10} justify="space-between" wrap="wrap">
          <Group gap={8}>
            <LyricsBadge
              track={{
                hasWordSync: t.hasWordSync,
                hasSyncedLyrics: t.syncedLyrics !== null,
                hasPlainLyrics: t.plainLyrics !== null,
                instrumental: t.instrumental,
              }}
            />
            {t.duration !== null && (
              <Text size="sm" c="dimmed" ff="monospace">
                {duration(t.duration)}
              </Text>
            )}
          </Group>
          <Actions track={t} />
        </Group>
      </header>
      <Lyrics track={t} />
    </article>
  )
}
