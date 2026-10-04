// The `/atrium` pane's model, read off `ctl board list` and `ctl bus feed`
// replies. Pure: the fetching is in register.ts.

import type { Reply } from './ctl'
import type { AtriumBoardRow, AtriumDecision, AtriumFeedRow, AtriumView } from '../types'

const str = (v: unknown): string => (typeof v === 'string' ? v : v === null || v === undefined ? '' : String(v))

/** The fields of an event or entry as `k=v` tokens, `to` and the headline left out. */
function kv(fields: unknown, skip: readonly string[]): string {
  if (fields === null || typeof fields !== 'object') return ''
  return Object.entries(fields as Record<string, unknown>)
    .filter(([k]) => !skip.includes(k))
    .map(([k, v]) => `${k}=${str(v)}`)
    .join(' ')
}

/** The board's entries, in the broker's order. */
export function boardRows(reply: Reply): AtriumBoardRow[] {
  const board = reply.board
  if (reply.ok !== true || !Array.isArray(board)) return []
  const rows: AtriumBoardRow[] = []
  for (const e of board) {
    if (e === null || typeof e !== 'object') continue
    const { key, by, fields } = e as Record<string, unknown>
    if (typeof key !== 'string') continue
    rows.push({ key, by: str(by), fields: kv(fields, []) })
  }
  return rows
}

/** The feed's events as lines, and the unresolved decisions among them. */
export function feedRows(reply: Reply): { feed: AtriumFeedRow[]; decisions: AtriumDecision[] } {
  const feed = reply.feed
  const rows: AtriumFeedRow[] = []
  const decisions: AtriumDecision[] = []
  if (reply.ok !== true || !Array.isArray(feed)) return { feed: rows, decisions }
  for (const e of feed) {
    if (e === null || typeof e !== 'object') continue
    const { seq, topic, kind, from, resolved, fields } = e as Record<string, unknown>
    if (typeof seq !== 'number') continue
    const f = fields !== null && typeof fields === 'object' ? (fields as Record<string, unknown>) : {}
    const headline = str(f.msg) || str(f.q)
    const line = headline !== '' ? headline : kv(f, ['to'])
    rows.push({ seq, topic: str(topic), from: str(from) || 'atrium', kind: str(kind), line })
    if (kind === 'decision_needed' && resolved !== true) {
      decisions.push({ seq, topic: str(topic), from: str(from) || 'atrium', to: str(f.to), msg: line })
    }
  }
  return { feed: rows, decisions }
}

/** The whole view: the last `keep` feed lines, every open decision, the board. */
export function parseView(board: Reply, feed: Reply, fetchedAt: number, keep = 8): AtriumView {
  const { feed: rows, decisions } = feedRows(feed)
  const error = [board, feed].map(r => (r.ok === true ? '' : str(r.err))).filter(e => e !== '')[0]
  return {
    fetchedAt,
    ...(error === undefined ? {} : { error }),
    decisions,
    board: boardRows(board),
    feed: rows.slice(-keep),
  }
}

/** A line cut to `width` columns with an ellipsis. */
export function fit(text: string, width: number): string {
  const chars = Array.from(text)
  return chars.length <= width ? text : `${chars.slice(0, Math.max(0, width - 1)).join('')}…`
}
