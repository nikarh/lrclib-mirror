// Lyrics as lines: LRC parsed into timed lines, plain lyrics split, and the text copied out of a selection.

export interface Line {
  /** Milliseconds; `null` for an untimed line. */
  time: number | null
  /** The timestamp as written (`01:02.50`), for copying it back as it was. */
  stamp: string | null
  text: string
}

const TIMESTAMP = /^\[(\d{1,3}):(\d{1,2}(?:[.:]\d{1,3})?)\]/
// `[ar: Artist]`, `[length: 3:45]`: ID tags, not lyrics.
const ID_TAG = /^\[[a-z#]+:[^\]]*\]$/i

function parseTime(minutes: string, seconds: string) {
  const [whole = '0', fraction = ''] = seconds.split(/[.:]/)
  const ms = fraction ? Number(fraction.padEnd(3, '0').slice(0, 3)) : 0
  return (Number(minutes) * 60 + Number(whole)) * 1000 + ms
}

/** The lines of LRC lyrics: a line with several timestamps is there at each, in order of time. */
export function parseLrc(lrc: string): Line[] {
  const lines: Line[] = []
  let repeated = false
  for (const raw of lrc.replace(/\r\n?/g, '\n').split('\n')) {
    let rest = raw.trim()
    const times: { time: number; stamp: string }[] = []
    for (let m = TIMESTAMP.exec(rest); m !== null; m = TIMESTAMP.exec(rest)) {
      const [whole, minutes = '0', seconds = '0'] = m
      times.push({ time: parseTime(minutes, seconds), stamp: `${minutes}:${seconds}` })
      rest = rest.slice(whole.length)
    }
    if (times.length === 0) {
      if (rest === '' || ID_TAG.test(rest)) continue
      lines.push({ time: null, stamp: null, text: rest })
      continue
    }
    repeated ||= times.length > 1
    for (const { time, stamp } of times) lines.push({ time, stamp, text: rest.trim() })
  }
  if (repeated) {
    // Stable: untimed lines keep their place relative to the line before them.
    let last = 0
    const keyed = lines.map((line) => {
      last = line.time ?? last
      return { line, at: last }
    })
    keyed.sort((a, b) => a.at - b.at)
    return keyed.map((k) => k.line)
  }
  return lines
}

export function plainLines(plain: string): Line[] {
  return plain
    .replace(/\r\n?/g, '\n')
    .split('\n')
    .map((text) => ({ time: null, stamp: null, text }))
}

const pad = (n: number, width = 2) => String(n).padStart(width, '0')

/** `m:ss` of a duration in seconds. */
export function duration(seconds: number | null) {
  if (seconds === null || !Number.isFinite(seconds)) return ''
  const s = Math.round(seconds)
  const hours = Math.floor(s / 3600)
  const minutes = Math.floor((s % 3600) / 60)
  return hours > 0
    ? `${String(hours)}:${pad(minutes)}:${pad(s % 60)}`
    : `${String(minutes)}:${pad(s % 60)}`
}

/** The lines as text, or as LRC (their timestamps as written) when `lrc` is set. */
export function linesText(lines: readonly Line[], lrc: boolean) {
  return lines
    .map((l) => (lrc && l.stamp !== null ? `[${l.stamp}] ${l.text}`.trimEnd() : l.text))
    .join('\n')
}
