import { expect, test } from 'claude-code/testing'
import { TOOLS, TOOL_PREFIX, argvFor, ctlTimeoutFor, isAtriumTool } from '../hooks/control'
import { guardVerdict, isAbsolute, join, pathOf, splitParent } from '../hooks/guard'
import { parseWho, roleSection } from '../hooks/role'

test('every tool has a name, a description and an object schema, and is recognised by its full name', async () => {
  for (const t of TOOLS) {
    expect(t.name.length).toBeGreaterThan(0)
    expect(t.description.length).toBeGreaterThan(20)
    expect(t.inputSchema.type).toBe('object')
    expect(isAtriumTool(TOOL_PREFIX + t.name)).toBe(true)
  }
  expect(isAtriumTool('mcp__atrium__nope')).toBe(false)
  expect(isAtriumTool('Bash')).toBe(false)
})

test('ask, asked and who map to ctl argv, and an ask run waits for its reply', async () => {
  expect(argvFor('ask', { target: 'dev_1', question: 'what are you doing?' })).toEqual(['ask', 'dev_1', 'what are you doing?'])
  expect(argvFor('ask', { target: '3', question: 'q', timeout_s: 30 })).toEqual(['ask', '3', '--timeout', '30', 'q'])
  expect(argvFor('ask', { target: '3' })).toBeInstanceOf(Error)
  expect(argvFor('asked', { target: '3', id: 7 })).toEqual(['asked', '3', '7'])
  expect(argvFor('asked', { target: '3' })).toBeInstanceOf(Error)
  expect(argvFor('who', { path: 'src/x.rs' })).toEqual(['who', 'src/x.rs'])
  expect(argvFor('who', {})).toBeInstanceOf(Error)
  expect(ctlTimeoutFor(['list'])).toBe(10_000)
  expect(ctlTimeoutFor(['ask', '3', 'q'])).toBe(150_000)
  expect(ctlTimeoutFor(['ask', '3', '--timeout', '30', 'q'])).toBe(60_000)
  expect(ctlTimeoutFor(['ask', '3', '--timeout', 'x', 'q'])).toBe(150_000)
})

test('spawn, send, status, list and kill map to ctl argv', async () => {
  expect(argvFor('spawn', { role: 'dev_1' })).toEqual(['spawn', '--role', 'dev_1', '--', 'claude'])
  expect(argvFor('spawn', { role: 'r', here: true, mode: 'accept', worktree: 'w', identity: 'work', cmd: ['codex'] })).toEqual([
    'spawn', '--role', 'r', '--here', '--mode', 'accept', '--worktree', 'w', '--identity', 'work', '--', 'codex',
  ])
  expect(argvFor('spawn', {})).toBeInstanceOf(Error)
  expect(argvFor('spawn', { role: 'r', cmd: [1] })).toBeInstanceOf(Error)
  expect(argvFor('send', { target: 'lead', text: 'review M1' })).toEqual(['send', 'lead', 'review M1'])
  expect(argvFor('send', { target: 'lead' })).toBeInstanceOf(Error)
  expect(argvFor('status', {})).toEqual(['status'])
  expect(argvFor('status', { target: '3' })).toEqual(['status', '3'])
  expect(argvFor('list', {})).toEqual(['list'])
  expect(argvFor('kill', { target: 'dev_1' })).toEqual(['kill', 'dev_1'])
  expect(argvFor('nope', {})).toBeInstanceOf(Error)
})

test('board and bus tools map fields to k=v tokens and refuse bad shapes', async () => {
  expect(argvFor('board_set', { key: 'M1', fields: { status: 'DONE', commit: 'abc', open: '' } })).toEqual([
    'board', 'set', 'M1', 'status=DONE', 'commit=abc', 'open=',
  ])
  expect(argvFor('board_set', { key: 'M1', fields: {} })).toBeInstanceOf(Error)
  expect(argvFor('board_set', { key: 'M1', fields: { 'bad key': 'x' } })).toBeInstanceOf(Error)
  expect(argvFor('board_set', { key: 'M1', fields: { n: 1 } })).toBeInstanceOf(Error)
  expect(argvFor('board_get', { key: 'M1' })).toEqual(['board', 'get', 'M1'])
  expect(argvFor('board_list', {})).toEqual(['board', 'list'])
  expect(argvFor('board_claim', { key: 'k', ttl_s: 60 })).toEqual(['board', 'claim', 'k', '--ttl', '60'])
  expect(argvFor('board_claim', { key: 'k', ttl_s: -1 })).toBeInstanceOf(Error)
  expect(argvFor('board_release', { key: 'k' })).toEqual(['board', 'release', 'k'])
  expect(argvFor('bus_pub', { topic: 'work', fields: { item: 'M1', status: 'done' }, decision: true, to: ['lead', '2'] })).toEqual([
    'bus', 'pub', 'work', '--decision', '--to', 'lead,2', 'item=M1', 'status=done',
  ])
  expect(argvFor('bus_pub', { topic: 'work', fields: { msg: 'hi' }, to: [''] })).toBeInstanceOf(Error)
  expect(argvFor('bus_feed', {})).toEqual(['bus', 'feed'])
  expect(argvFor('bus_feed', { since: 7 })).toEqual(['bus', 'feed', '--since', '7'])
  expect(argvFor('bus_resolve', { seq: 7 })).toEqual(['bus', 'resolve', '7'])
  expect(argvFor('bus_topics', {})).toEqual(['bus', 'topics'])
})

test('whoami is read into a Who, and the role section says what the pane is', async () => {
  const who = parseWho({
    ok: true, pane: 3, role: 'attention', parent: 0, depth: 1, mode: 'accept', worktree: 'attention',
    cwd: '/w/attention', can_spawn: false, deny: ['Bash(cargo install*)'], mod: { version: '0.1.0' },
    item: 'M1', files: ['src/attention.rs', 'src/lib.rs'],
  })
  expect(who).toBeDefined()
  expect(who?.files).toEqual(['src/attention.rs', 'src/lib.rs'])
  const text = roleSection(who!)
  expect(text).toContain('pane 3, role "attention", depth 1, spawned by pane 0, at trust mode accept')
  expect(text).toContain('Your item is M1')
  expect(text).toContain('src/attention.rs, src/lib.rs')
  expect(text).toContain('worktree "attention"')
  expect(text).toContain('may not spawn')
  expect(text).toContain('[atrium bus #')
  expect(text).toContain('atrium_ask')
  expect(text).toContain('atrium_who')
  expect(who?.captions).toBe(true)
  expect(parseWho({ ok: true, pane: 1, depth: 0, mode: 'plan', can_spawn: false, deny: [], files: [], captions: false })?.captions).toBe(false)
  const lead = parseWho({ ok: true, pane: 0, depth: 0, mode: 'automode', can_spawn: true, deny: [], files: [] })!
  const leadText = roleSection(lead)
  expect(leadText).toContain('pane 0, depth 0, spawned by the human')
  expect(leadText).toContain('prefer atrium_spawn over the Agent tool')
  expect(leadText).not.toContain('Your item')
  expect(parseWho({ ok: false, err: 'x' })).toBeUndefined()
  expect(parseWho({ ok: true })).toBeUndefined()
})

test('the guard refuses an edit outside the owned files and names the owner', async () => {
  const owned = ['/w/attention/src/attention.rs', '/w/attention/src/lib.rs']
  expect(guardVerdict('/w/attention/src/lib.rs', owned, 'M1', 'attention')).toBeUndefined()
  expect(guardVerdict('C:\\w\\attention\\src\\lib.rs', ['C:/w/attention/src/lib.rs'], 'M1', 'attention')).toBeUndefined()
  const no = guardVerdict('/w/attention/src/filter.rs', owned, 'M1', 'attention')
  expect(no).toContain('M1 ("attention") owns attention.rs, lib.rs, not filter.rs')
  expect(guardVerdict(undefined, owned, 'M1', 'attention')).toContain('cannot be placed')
  expect(guardVerdict('/anything', [], 'M1', 'attention')).toBeUndefined()
  expect(guardVerdict('/x', ['/y'], undefined, undefined)).toContain('this pane owns y, not x')
})

test('paths split, join and place the same on both platforms', async () => {
  expect(splitParent('src/attention.rs')).toEqual({ folder: 'src', name: 'attention.rs' })
  expect(splitParent('attention.rs')).toEqual({ folder: '.', name: 'attention.rs' })
  expect(splitParent('/attention.rs')).toEqual({ folder: '/', name: 'attention.rs' })
  expect(splitParent('C:\\w\\a.rs')).toEqual({ folder: 'C:\\w', name: 'a.rs' })
  expect(isAbsolute('/x')).toBe(true)
  expect(isAbsolute('C:\\x')).toBe(true)
  expect(isAbsolute('src/x')).toBe(false)
  expect(join('/w/', 'src/x.rs')).toBe('/w/src/x.rs')
  expect(join('/w', '/abs.rs')).toBe('/abs.rs')
  expect(join('C:\\w', 'src\\x.rs')).toBe('C:\\w\\src\\x.rs')
  expect(pathOf({ file_path: 'a.rs' })).toBe('a.rs')
  expect(pathOf({ notebook_path: 'n.ipynb' })).toBe('n.ipynb')
  expect(pathOf({ command: 'ls' })).toBeUndefined()
})
