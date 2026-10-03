import { expect, test } from 'claude-code/testing'
import { reportedAnswer, slug, spawnPointer, spawnedPane, subagentPrompt } from '../hooks/agents'
import { TOOLS, argvFor } from '../hooks/control'
import { parseWho, roleSection } from '../hooks/role'

test('a description becomes a short role slug', async () => {
  expect(slug('Explore the parser!')).toBe('explore-the-parser')
  expect(slug('  Review   M1: the attention module, again and again ')).toBe('review-m1-the-attention')
  expect(slug('!!!')).toBe('subagent')
  expect(slug('a'.repeat(50))).toBe('a'.repeat(24))
})

test('the task is sent as one framed line', async () => {
  const text = subagentPrompt(3, 'find the bug', 'Read src/filter.rs\n\nand say\twhere the OSC parser drops a byte.')
  expect(text.includes('\n')).toBe(false)
  expect(text.includes('\t')).toBe(false)
  expect(text).toContain('spawned by pane 3 for: find the bug.')
  expect(text).toContain('Task: Read src/filter.rs and say where the OSC parser drops a byte.')
  expect(text).toContain('your parent reads it')
})

test('the Agent tool is pointed at atrium_subagent under panes, refused under deny, untouched under native', async () => {
  expect(spawnPointer('panes')).toContain('atrium_subagent')
  expect(spawnPointer('deny')).toContain('atrium_spawn')
  expect(spawnPointer('native')).toBeUndefined()
  expect(spawnPointer('')).toBeUndefined()
})

test('spawn and answer replies are read, and anything else is not', async () => {
  expect(spawnedPane({ ok: true, pane: 4, role: 'x', session: 's' })).toBe(4)
  expect(spawnedPane({ ok: false, err: 'depth' })).toBeUndefined()
  expect(spawnedPane({ ok: true })).toBeUndefined()
  expect(reportedAnswer({ ok: true, pane: 4, status: 'waiting-prompt', seq: 2, answer: 'done' })).toEqual({ answer: 'done', seq: 2 })
  expect(reportedAnswer({ ok: true, pane: 4, status: 'working', seq: null, answer: null })).toBeUndefined()
  expect(reportedAnswer({ ok: false, err: 'timeout after 300s' })).toBeUndefined()
})

test('the subagent tool is registered and the role section says what a subagent is here', async () => {
  const t = TOOLS.find(t => t.name === 'subagent')
  expect(t).toBeDefined()
  expect((t!.inputSchema.required as string[])).toEqual(['description', 'prompt'])
  expect(argvFor('subagent', {})).toBeInstanceOf(Error)
  const panes = parseWho({ ok: true, pane: 0, depth: 0, mode: 'automode', can_spawn: true, deny: [], files: [], subagents: 'panes', subagents_keep: true })!
  expect(panes.subagents).toBe('panes')
  expect(panes.subagentsKeep).toBe(true)
  expect(roleSection(panes)).toContain('atrium_subagent runs one')
  const deny = parseWho({ ok: true, pane: 0, depth: 0, mode: 'automode', can_spawn: true, deny: [], files: [], subagents: 'deny' })!
  expect(roleSection(deny)).toContain('Subagents are off')
  const native = parseWho({ ok: true, pane: 0, depth: 0, mode: 'automode', can_spawn: true, deny: [], files: [], subagents: 'native' })!
  expect(roleSection(native)).not.toContain('atrium_subagent runs one')
  const old = parseWho({ ok: true, pane: 0, depth: 0, mode: 'automode', can_spawn: true, deny: [], files: [] })!
  expect(old.subagents).toBe('panes')
})
