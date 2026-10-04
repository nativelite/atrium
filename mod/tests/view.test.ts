import { expect, test } from 'claude-code/testing'
import { boardRows, feedRows, fit, parseView } from '../hooks/view'

const board = {
  ok: true,
  board: [
    { key: 'M1', by: 'attention', fields: { status: 'DONE', commit: 'abc' } },
    { key: 'lead', by: 'lead', fields: { phase: '2' } },
    'junk',
    { fields: {} },
  ],
}
const feed = {
  ok: true,
  feed: [
    { seq: 1, topic: 'work', kind: 'fyi', from: 'builder', resolved: false, fields: { item: 'M1', status: 'done' } },
    { seq: 2, topic: 'work', kind: 'decision_needed', from: 'builder', resolved: false, fields: { to: 'lead', msg: 'bigger than M' } },
    { seq: 3, topic: 'atrium', kind: 'decision_needed', from: null, resolved: true, fields: { msg: 'old one' } },
    { seq: 4, topic: 'work', kind: 'decision_needed', from: 'reviewer', resolved: false, fields: { q: 'merge now?' } },
    { kind: 'fyi' },
  ],
  cursor: 4,
}

test('board rows carry key, author and fields as k=v', async () => {
  expect(boardRows(board)).toEqual([
    { key: 'M1', by: 'attention', fields: 'status=DONE commit=abc' },
    { key: 'lead', by: 'lead', fields: 'phase=2' },
  ])
  expect(boardRows({ ok: false, err: 'x' })).toEqual([])
})

test('feed rows are headlines, and the open decisions are picked out', async () => {
  const { feed: rows, decisions } = feedRows(feed)
  expect(rows.map(r => r.line)).toEqual(['item=M1 status=done', 'bigger than M', 'old one', 'merge now?'])
  expect(rows[2]?.from).toBe('atrium')
  expect(decisions.map(d => d.seq)).toEqual([2, 4])
  expect(decisions[0]).toEqual({ seq: 2, topic: 'work', from: 'builder', to: 'lead', msg: 'bigger than M' })
})

test('the view keeps the last lines, every open decision, and names an error', async () => {
  const v = parseView(board, feed, 1000, 2)
  expect(v.fetchedAt).toBe(1000)
  expect(v.error).toBeUndefined()
  expect(v.feed.map(r => r.seq)).toEqual([3, 4])
  expect(v.decisions.length).toBe(2)
  expect(v.board.length).toBe(2)
  const broken = parseView({ ok: false, err: 'not in a ctl session' }, feed, 1)
  expect(broken.error).toBe('not in a ctl session')
  expect(broken.board).toEqual([])
})

test('fit cuts to the width with an ellipsis', async () => {
  expect(fit('short', 10)).toBe('short')
  expect(fit('a longer line', 6)).toBe('a lon…')
})
