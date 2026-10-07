import { expect, test } from 'claude-code/testing'
import {
  ASK_REPLY_CAP,
  CAPTION_CAP,
  CAPTION_MIN_CHARS,
  CAPTION_MIN_MS,
  askPrompt,
  askReplyText,
  captionDue,
  captionRequest,
  clipCaption,
  toolCaption,
} from '../hooks/caption'

test('a tool call captions itself from its name and the one fact that matters', async () => {
  expect(toolCaption('Bash', { command: 'cargo test --lib ctl\necho done' })).toBe('running cargo test --lib ctl')
  expect(toolCaption('Bash', {})).toBe('running a command')
  expect(toolCaption('Edit', { file_path: '/w/a/src/filter.rs' })).toBe('editing filter.rs')
  expect(toolCaption('Write', { file_path: 'C:\\w\\a\\src\\x.rs' })).toBe('editing x.rs')
  expect(toolCaption('NotebookEdit', { notebook_path: '/n/b.ipynb' })).toBe('editing b.ipynb')
  expect(toolCaption('Read', { file_path: '/w/a/README.md' })).toBe('reading README.md')
  expect(toolCaption('Grep', { pattern: 'fn parse' })).toBe('searching for fn parse')
  expect(toolCaption('Glob', { pattern: '**/*.rs' })).toBe('finding **/*.rs')
  expect(toolCaption('WebFetch', { url: 'https://docs.rs/x/latest/x/' })).toBe('fetching docs.rs')
  expect(toolCaption('WebSearch', { query: 'rust pty' })).toBe('searching the web for rust pty')
  expect(toolCaption('Agent', { description: 'review the diff' })).toBe('delegating: review the diff')
  expect(toolCaption('TodoWrite', {})).toBe('planning')
  expect(toolCaption('mcp__atrium__send', { target: 'dev_1', text: 'x' })).toBe('atrium send dev_1')
  expect(toolCaption('mcp__atrium__board_list', {})).toBe('atrium board_list')
  expect(toolCaption('mcp__other__thing', {})).toBe('using mcp__other__thing')
})

test('captions are one line and capped', async () => {
  expect(clipCaption('  running\n  the   tests ')).toBe('running the tests')
  const long = clipCaption('x'.repeat(CAPTION_CAP + 20))
  expect(Array.from(long).length).toBe(CAPTION_CAP)
  expect(long.endsWith('…')).toBe(true)
  expect(toolCaption('Bash', { command: 'a'.repeat(200) })!.length).toBe(CAPTION_CAP)
})

test('a model caption is due only with enough text and enough time', async () => {
  expect(captionDue(0, CAPTION_MIN_MS, CAPTION_MIN_CHARS)).toBe(true)
  expect(captionDue(0, CAPTION_MIN_MS - 1, CAPTION_MIN_CHARS)).toBe(false)
  expect(captionDue(0, CAPTION_MIN_MS, CAPTION_MIN_CHARS - 1)).toBe(false)
  const { system, prompt } = captionRequest('a'.repeat(1000) + 'tail')
  expect(prompt.endsWith('tail')).toBe(true)
  expect(Array.from(prompt).length).toBe(600)
  expect(system).toContain('six words')
})

test('an ask is framed for a fork and its result becomes reply text', async () => {
  const p = askPrompt('  what now?  ')
  expect(p.startsWith('[atrium ask]')).toBe(true)
  expect(p).toContain('what now?')
  expect(p).toContain('No tools')
  expect(askReplyText({ isAnswered: true, text: ' fine, thanks ' })).toBe('fine, thanks')
  expect(askReplyText({ isAnswered: true, text: '  ' })).toBe('(no reply: empty)')
  expect(askReplyText({ isAnswered: false, reason: 'nothing-to-fork' })).toContain('first turn')
  expect(askReplyText({ isAnswered: false, reason: 'aborted' })).toContain('interrupted')
  expect(askReplyText({ isAnswered: false, reason: 'api-error' })).toContain('API')
  expect(askReplyText({ isAnswered: false, reason: 'empty-reply' })).toBe('(no reply: empty-reply)')
  const long = askReplyText({ isAnswered: true, text: 'y'.repeat(ASK_REPLY_CAP + 5) })
  expect(Array.from(long).length).toBe(ASK_REPLY_CAP)
})
