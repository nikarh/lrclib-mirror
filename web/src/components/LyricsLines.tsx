import { Button, CloseButton, Group, Text } from '@mantine/core'
import { IconCopy } from '@tabler/icons-react'
import {
  type KeyboardEvent,
  type MouseEvent,
  useCallback,
  useEffect,
  useId,
  useMemo,
  useRef,
  useState,
} from 'react'

import { copyAndTell } from '../clipboard'
import { type Line, linesText } from '../lyrics'

const plural = (n: number, one: string) => (n === 1 ? `1 ${one}` : `${String(n)} ${one}s`)

/** Whether text is selected the usual way (by dragging) inside `el`. */
function textSelectedIn(el: HTMLElement | null) {
  const selection = window.getSelection()
  return (
    selection !== null &&
    !selection.isCollapsed &&
    el !== null &&
    el.contains(selection.anchorNode) &&
    selection.toString().trim() !== ''
  )
}

/**
 * Lyrics, a line per row. Lines are picked by clicking them (shift-click: a range) or from the keyboard, and copied
 * together as text or LRC. Dragging still selects text as usual, without the timestamps.
 */
export function LyricsLines({ lines, timed }: { lines: Line[]; timed: boolean }) {
  const [selected, setSelected] = useState<ReadonlySet<number>>(new Set())
  const [anchor, setAnchor] = useState<number | null>(null)
  const [active, setActive] = useState<number | null>(null)
  const list = useRef<HTMLDivElement>(null)
  const id = useId()

  const picked = useMemo(() => lines.filter((_, i) => selected.has(i)), [lines, selected])

  const copy = useCallback(
    (lrc: boolean) => {
      copyAndTell(linesText(picked, lrc), plural(picked.length, 'line'))
    },
    [picked],
  )

  const clear = useCallback(() => {
    setSelected(new Set())
    setAnchor(null)
  }, [])

  const range = (from: number, to: number) => {
    const next = new Set(selected)
    for (let i = Math.min(from, to); i <= Math.max(from, to); i++) next.add(i)
    return next
  }

  const toggle = (i: number) => {
    const next = new Set(selected)
    if (next.has(i)) next.delete(i)
    else next.add(i)
    setSelected(next)
    setAnchor(i)
  }

  const lineAt = (target: EventTarget) => {
    const row = (target as HTMLElement).closest<HTMLElement>('[data-line]')
    return row === null ? null : Number(row.dataset.line)
  }

  const onMouseDown = (e: MouseEvent) => {
    // Shift-click picks a range of lines, instead of extending a text selection.
    if (e.shiftKey) e.preventDefault()
  }

  const onClick = (e: MouseEvent) => {
    // The end of dragging over text: that's a text selection, not a pick.
    if (textSelectedIn(list.current)) return
    const i = lineAt(e.target)
    if (i === null) return
    setActive(i)
    if (e.shiftKey && anchor !== null) {
      setSelected(range(anchor, i))
    } else {
      toggle(i)
    }
  }

  const onKeyDown = (e: KeyboardEvent) => {
    const mod = e.ctrlKey || e.metaKey
    const last = lines.length - 1
    const move = (to: number) => {
      const next = Math.max(0, Math.min(last, to))
      e.preventDefault()
      setActive(next)
      if (e.shiftKey) {
        setSelected(range(anchor ?? active ?? next, next))
        if (anchor === null) setAnchor(active ?? next)
      }
      document.getElementById(`${id}-${String(next)}`)?.scrollIntoView({ block: 'nearest' })
    }
    switch (e.key) {
      case 'ArrowDown':
        move(active === null ? 0 : active + 1)
        break
      case 'ArrowUp':
        move(active === null ? 0 : active - 1)
        break
      case 'Home':
        move(0)
        break
      case 'End':
        move(last)
        break
      case ' ':
      case 'Enter':
        if (active !== null) {
          e.preventDefault()
          toggle(active)
        }
        break
      case 'Escape':
        if (selected.size > 0) {
          e.preventDefault()
          clear()
        }
        break
      case 'a':
        if (mod) {
          e.preventDefault()
          setSelected(new Set(lines.keys()))
        }
        break
      case 'c':
        if (mod && selected.size > 0 && !textSelectedIn(list.current)) {
          e.preventDefault()
          copy(false)
        }
        break
    }
  }

  // Escape clears the picked lines from anywhere on the page.
  useEffect(() => {
    if (selected.size === 0) return
    const onEscape = (e: globalThis.KeyboardEvent) => {
      if (e.key === 'Escape') clear()
    }
    window.addEventListener('keydown', onEscape)
    return () => {
      window.removeEventListener('keydown', onEscape)
    }
  }, [selected.size, clear])

  return (
    <div className="lyrics">
      <div
        ref={list}
        role="listbox"
        aria-multiselectable="true"
        aria-label="Lyrics lines: click to pick, shift-click for a range"
        aria-activedescendant={active === null ? undefined : `${id}-${String(active)}`}
        tabIndex={0}
        className={timed ? 'lines lines-timed' : 'lines'}
        onMouseDown={onMouseDown}
        onClick={onClick}
        onKeyDown={onKeyDown}
      >
        {lines.map((line, i) => {
          const blank = line.text.trim() === ''
          const classes = ['line']
          if (selected.has(i)) classes.push('line-selected')
          if (active === i) classes.push('line-active')
          if (blank) classes.push(line.time === null ? 'line-gap' : 'line-break')
          return (
            <div
              key={i}
              id={`${id}-${String(i)}`}
              role="option"
              aria-selected={selected.has(i)}
              data-line={i}
              className={classes.join(' ')}
            >
              {timed && <span className="line-time">{line.stamp ?? ''}</span>}
              <span className="line-text">{blank && line.time !== null ? '♪' : line.text}</span>
            </div>
          )
        })}
      </div>

      {selected.size > 0 && (
        <div className="selection-bar" role="toolbar" aria-label="Picked lines">
          <Text size="sm" fw={600} className="selection-count">
            {plural(selected.size, 'line')}
          </Text>
          <Group gap={6} wrap="nowrap">
            <Button
              size="compact-sm"
              leftSection={<IconCopy size={14} />}
              onClick={() => {
                copy(false)
              }}
            >
              Copy
            </Button>
            {timed && (
              <Button
                size="compact-sm"
                variant="light"
                onClick={() => {
                  copy(true)
                }}
              >
                Copy as LRC
              </Button>
            )}
            {selected.size < lines.length && (
              <Button
                size="compact-sm"
                variant="subtle"
                color="gray"
                onClick={() => {
                  setSelected(new Set(lines.keys()))
                }}
              >
                All
              </Button>
            )}
            <CloseButton size="sm" aria-label="Unpick the lines" onClick={clear} />
          </Group>
        </div>
      )}
    </div>
  )
}
