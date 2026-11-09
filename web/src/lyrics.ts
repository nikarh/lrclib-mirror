// Lyrics as lines: LRC parsed into timed lines, plain lyrics split, and the text copied out of a selection.

export interface Line {
  /** Milliseconds; `null` for an untimed line. */
  time: number | null
  /** The timestamp as written (`01:02.50`), for copying it back as it was. */
  stamp: string | null
  text: string
  /** The line starts a verse or chorus: a gap above it. */
  para?: boolean
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

const comparable = (text: string) =>
  text
    .normalize('NFKC')
    .toLocaleLowerCase()
    .replace(/[^\p{L}\p{N}]+/gu, '')

/**
 * Where the verses of synced lyrics start: where the plain lyrics have a blank line, when they are the same lines
 * (they usually are); otherwise after an empty timed line or a long pause.
 */
export function withParagraphs(synced: Line[], plain: string | null): Line[] {
  const fromPlain = plain === null ? null : paragraphsFromPlain(synced, plain)
  const starts = fromPlain ?? paragraphsFromTiming(synced)
  return synced.map((line, i) => (starts.has(i) ? { ...line, para: true } : line))
}

function paragraphsFromPlain(synced: Line[], plain: string): Set<number> | null {
  const rows = plain.replace(/\r\n?/g, '\n').split('\n').map(comparable)
  const starts = new Set<number>()
  let j = 0
  let matched = 0
  let words = 0
  for (const [i, line] of synced.entries()) {
    const text = comparable(line.text)
    if (text === '') continue
    words++
    // The line among the next plain ones (looking past a few that differ).
    let found = -1
    for (let k = j, seen = 0; k < rows.length && seen < 4; k++) {
      if (rows[k] === text) {
        found = k
        break
      }
      if (rows[k] !== '') seen++
    }
    if (found === -1) continue
    // A blank line between it and the line matched before: a new verse.
    if (matched > 0 && rows.slice(j, found).includes('')) starts.add(i)
    matched++
    j = found + 1
  }
  // Other lines than the synced ones: their blank lines say nothing about these.
  return words > 0 && matched / words >= 0.8 ? starts : null
}

function paragraphsFromTiming(lines: Line[]): Set<number> {
  const starts = new Set<number>()
  const gaps: number[] = []
  for (let i = 1; i < lines.length; i++) {
    const a = lines[i - 1]?.time
    const b = lines[i]?.time
    if (a != null && b != null && b > a) gaps.push(b - a)
  }
  const sorted = [...gaps].sort((x, y) => x - y)
  const median = sorted[Math.floor(sorted.length / 2)] ?? 0
  // A pause clearly longer than the usual line.
  const pause = Math.max(median * 2.2, median + 3500)
  let afterBreak = false
  for (const [i, line] of lines.entries()) {
    const prev = lines[i - 1]
    if (line.text.trim() === '') {
      afterBreak = true
      continue
    }
    const long =
      prev?.time != null && line.time !== null && median > 0 && line.time - prev.time > pause
    if (i > 0 && (afterBreak || long)) starts.add(i)
    afterBreak = false
  }
  return starts
}

/**
 * The lines as text (a blank line between verses), or as LRC (their timestamps as written) when `lrc` is set.
 */
export function linesText(lines: readonly Line[], lrc: boolean) {
  if (lrc) {
    return lines
      .map((l) => (l.stamp !== null ? `[${l.stamp}] ${l.text}`.trimEnd() : l.text))
      .join('\n')
  }
  const out: string[] = []
  for (const l of lines) {
    if (l.para === true && out.length > 0 && out.at(-1) !== '') out.push('')
    // An empty timed line (an instrumental part) is a verse break too.
    if (l.stamp !== null && l.text === '') {
      if (out.length > 0 && out.at(-1) !== '') out.push('')
      continue
    }
    out.push(l.text)
  }
  while (out.at(-1) === '') out.pop()
  return out.join('\n')
}
