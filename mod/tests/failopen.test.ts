import { expect, mock, test } from 'claude-code/testing'
import type { On } from 'claude-code'

// The mod under failure, against the engine itself: in every test the tool
// call goes on as if the mod were not loaded, and what failed is counted and
// told to atrium on the next report.

type Ctl = (verb: string, args: readonly string[], n: number) => unknown | Promise<unknown>

/** A promise that never settles: a ctl call that hangs. */
const never = (): Promise<never> => new Promise<never>(() => undefined)

/** The world beneath the mod: an atrium pane's environment, and `atrium ctl` answered by `ctl`. */
function atrium(on: On, ctl: Ctl): { calls: string[][]; ran: string[] } {
  const calls: string[][] = []
  const ran: string[] = []
  const env: Record<string, string> = { ATRIUM_CTL: 'pipe', ATRIUM_TOKEN: 'tok', ATRIUM_BIN: 'atrium' }
  on('env.get', (_, e) => ({ value: env[e.name] }))
  on('process.run', async (_, e) => {
    // argv is `<bin> ctl --json <verb> ...`.
    expect(e.argv.slice(1, 3)).toEqual(['ctl', '--json'])
    const args = e.argv.slice(3)
    calls.push([...args])
    const answer = await ctl(args[0] ?? '', args.slice(1), calls.filter(c => c[0] === args[0]).length)
    return { value: { exitCode: 0, stdout: typeof answer === 'string' ? answer : `${JSON.stringify(answer)}\n`, stderr: '', isStdoutTruncated: false, isStderrTruncated: false } }
  })
  on('session.start', (_, e) => ({ cwd: e.cwd }))
  on('session.version', () => ({ value: { version: '9.9.9' } }))
  on('ui.log', () => ({ value: undefined }))
  on('ui.toast', () => ({ value: undefined }))
  on('tool.call', (_, e) => {
    ran.push(String(e.tool))
    return { result: { ran: true } }
  })
  return { calls, ran }
}

const HELLO = { ok: true, pane: 1, accepted: ['status', 'answer', 'context', 'tools', 'guard'] }
const WHO = { ok: true, pane: 1, role: 'builder', mode: 'accept', files: [], subagents: 'native' }
const OK = { ok: true, pane: 1 }

/** A healthy atrium: every verb answered as the broker answers it. */
const healthy: Ctl = verb => (verb === 'hello' ? HELLO : verb === 'whoami' ? WHO : OK)

/** The failure fields of the last report that carried them. */
function reported(calls: string[][]): Record<string, string> {
  const out: Record<string, string> = {}
  for (const c of calls) {
    if (c[0] !== 'report') continue
    for (const token of c.slice(1)) {
      const at = token.indexOf('=')
      const key = token.slice(0, at)
      if (key.startsWith('fail')) out[key] = token.slice(at + 1)
    }
  }
  return out
}

const START = { cwd: '/work', surface: 'terminal', isInteractive: false } as const

test('a healthy mod says hello, reports, and leaves the tool call alone', async ($, on) => {
  const clock = mock.clock(on)
  const { calls, ran } = atrium(on, healthy)
  on('tool.register', (_, e) => ({ value: { tool: `mcp__atrium__${e.name}` } }))
  await $.session.start(START)
  const out = await $.tool.call({ tool: 'Bash', command: 'ls' })
  await clock.settle()
  expect(calls.slice(0, 2).map(c => c[0])).toEqual(['hello', 'whoami'])
  expect(calls[0]).toContain('caps=status,answer,context,tools,guard')
  expect(ran).toEqual(['Bash'])
  expect(out).toEqual({ result: { ran: true } })
  expect(reported(calls)).toEqual({})
})

test('a hello that hangs past its deadline: the session starts, the tool call runs, the failure is counted', async ($, on) => {
  const clock = mock.clock(on)
  // The first hello never answers; the retry and everything after do.
  const { calls, ran } = atrium(on, (verb, _args, n) => (verb === 'hello' && n === 1 ? never() : healthy(verb, _args, n)))
  on('tool.register', (_, e) => ({ value: { tool: `mcp__atrium__${e.name}` } }))
  let started = false
  const starting = $.session.start(START).then(() => {
    started = true
  })
  await clock.advance(5_500)
  expect(started).toBe(false)
  // 5 s of deadline and 1 s of watchdog: the start is let go.
  await clock.advance(600)
  await starting
  expect(started).toBe(true)
  // The mod could not ask atrium, and the tool call is not its business.
  const out = await $.tool.call({ tool: 'Bash', command: 'ls' })
  expect(out).toEqual({ result: { ran: true } })
  expect(ran).toEqual(['Bash'])
  // One retry, five seconds on: atrium answers, and learns what happened.
  await clock.advance(5_100)
  await clock.settle()
  expect(calls.filter(c => c[0] === 'hello').length).toBe(2)
  const said = reported(calls)
  expect(said.fail).toBe('ctl.hello:1')
  expect(said.fail_class).toBe('timeout')
})

test('a hook that throws: the edit goes through, and the count and a scrubbed message reach atrium', async ($, on) => {
  const clock = mock.clock(on)
  const who = { ...WHO, files: ['src/a.ts'], item: 'M1', cwd: '/work' }
  const { calls, ran } = atrium(on, verb => (verb === 'hello' ? HELLO : verb === 'whoami' ? who : OK))
  on('tool.register', (_, e) => ({ value: { tool: `mcp__atrium__${e.name}` } }))
  // The guard asks the engine where the session is, and the engine fails it.
  on('session.cwd', () => {
    throw new Error('cannot read "C:/Users/someone/secret.txt" with key sk-ABCDEFGHIJKLMNOPQRSTUVWXYZ012345')
  })
  await $.session.start(START)
  const out = await $.tool.call({ tool: 'Edit', file_path: '/work/src/b.ts', old_string: 'a', new_string: 'b' })
  await clock.settle()
  // The guard could not judge, so the edit is not its to refuse.
  expect(out).toEqual({ result: { ran: true } })
  expect(ran).toEqual(['Edit'])
  const said = reported(calls)
  expect(said.fail).toContain('guard:1')
  expect(said.fail_msg).not.toContain('secret')
  expect(said.fail_msg).not.toContain('sk-')
  expect(said.fail_msg).not.toContain('C:')
})

test('a refusal atrium gave stays a refusal: an edit outside the item is denied', async ($, on) => {
  const clock = mock.clock(on)
  const who = { ...WHO, files: ['src/a.ts'], item: 'M1', cwd: '/work' }
  const { ran } = atrium(on, verb => (verb === 'hello' ? HELLO : verb === 'whoami' ? who : OK))
  on('tool.register', (_, e) => ({ value: { tool: `mcp__atrium__${e.name}` } }))
  on('session.cwd', () => ({ value: '/work' }))
  await $.session.start(START)
  const denied = await $.tool.call({ tool: 'Edit', file_path: '/work/src/b.ts', old_string: 'a', new_string: 'b' })
  expect(JSON.stringify(denied)).toContain('owns a.ts')
  expect(ran).toEqual([])
  // The file the item owns is edited, with no filesystem to ask: by its text.
  const allowed = await $.tool.call({ tool: 'Edit', file_path: '/work/src/../src/a.ts', old_string: 'a', new_string: 'b' })
  expect(allowed).toEqual({ result: { ran: true } })
  expect(ran).toEqual(['Edit'])
  await clock.settle()
})

test('a reply of the wrong shape: the tool call runs and the failure is counted', async ($, on) => {
  const clock = mock.clock(on)
  // whoami answers ok with nothing a pane is; a report answers a line that is no reply.
  const { calls, ran } = atrium(on, (verb, _a, n) =>
    verb === 'hello' ? HELLO : verb === 'whoami' ? { ok: true } : verb === 'report' && n === 1 ? 'it is fine, really\n' : OK,
  )
  on('tool.register', (_, e) => ({ value: { tool: `mcp__atrium__${e.name}` } }))
  await $.session.start(START)
  const out = await $.tool.call({ tool: 'Bash', command: 'ls' })
  await clock.settle()
  expect(out).toEqual({ result: { ran: true } })
  expect(ran).toEqual(['Bash'])
  await $.turn.start({ prompt: 'x' } as never).catch(() => undefined)
  await clock.settle()
  const said = reported(calls)
  expect(said.fail).toContain('ctl.whoami:1')
  expect(said.fail).toContain('ctl.report:1')
})

test('a hello whose accepted list is not a list switches the mod off, and tool calls run', async ($, on) => {
  const clock = mock.clock(on)
  const { calls, ran } = atrium(on, verb => (verb === 'hello' ? { ok: true, pane: 1, accepted: 'status' } : OK))
  await $.session.start(START)
  expect(await $.tool.call({ tool: 'Bash', command: 'ls' })).toEqual({ result: { ran: true } })
  expect(await $.tool.call({ tool: 'Edit', file_path: '/x', old_string: 'a', new_string: 'b' })).toEqual({ result: { ran: true } })
  await clock.settle()
  expect(ran).toEqual(['Bash', 'Edit'])
  // Nothing after the hello: the mod is off.
  expect(calls.map(c => c[0])).toEqual(['hello'])
})

test('an engine without a method the mod calls: that feature is off, said once, and the rest goes on', async ($, on) => {
  const clock = mock.clock(on)
  const { calls, ran } = atrium(on, healthy)
  // Nothing beneath answers `$.tool.register`: to the mod, an engine without it.
  await $.session.start(START)
  const out = await $.tool.call({ tool: 'Bash', command: 'ls' })
  await clock.settle()
  expect(out).toEqual({ result: { ran: true } })
  expect(ran).toEqual(['Bash'])
  // Asked once, not once per tool: one failure, not eighteen.
  const said = reported(calls)
  expect(said.fail).toBe('feature.tools:1')
  expect(said.fail_class).toBe('api')
  // The status still reports: one feature is off, not the mod.
  expect(calls.some(c => c[0] === 'report' && c.some(t => t.startsWith('status=')))).toBe(true)
})

test('a broker that stops answering: after five unanswered calls the mod is off and asks nothing more', async ($, on) => {
  const clock = mock.clock(on)
  const { calls, ran } = atrium(on, verb => (verb === 'hello' ? HELLO : verb === 'whoami' ? WHO : never()))
  on('tool.register', (_, e) => ({ value: { tool: `mcp__atrium__${e.name}` } }))
  await $.session.start(START)
  for (let i = 0; i < 8; i += 1) {
    expect(await $.tool.call({ tool: 'Bash', command: `echo ${i}` })).toEqual({ result: { ran: true } })
    await clock.advance(6_100)
  }
  await clock.settle()
  expect(ran.length).toBe(8)
  const reports = calls.filter(c => c[0] === 'report').length
  expect(reports).toBe(5)
  expect(await $.tool.call({ tool: 'Bash', command: 'ls' })).toEqual({ result: { ran: true } })
  await clock.advance(20_000)
  expect(calls.filter(c => c[0] === 'report').length).toBe(5)
})

test('atrium sets the mod aside: it stops asking, and tool calls run', async ($, on) => {
  const clock = mock.clock(on)
  const { calls, ran } = atrium(on, verb =>
    verb === 'hello' ? HELLO : verb === 'whoami' ? WHO : { ok: false, err: 'mod set aside: no hello within its window; the pane runs without it' },
  )
  on('tool.register', (_, e) => ({ value: { tool: `mcp__atrium__${e.name}` } }))
  await $.session.start(START)
  await clock.settle()
  const before = calls.length
  expect(await $.tool.call({ tool: 'Bash', command: 'ls' })).toEqual({ result: { ran: true } })
  await clock.settle()
  expect(ran).toEqual(['Bash'])
  expect(calls.length).toBe(before)
})

test('outside atrium the mod does nothing at all', async ($, on) => {
  const calls: string[] = []
  on('env.get', () => ({ value: undefined }))
  on('process.run', (_, e) => {
    calls.push(e.argv.join(' '))
    return { value: { exitCode: 0, stdout: '', stderr: '', isStdoutTruncated: false, isStderrTruncated: false } }
  })
  on('session.start', (_, e) => ({ cwd: e.cwd }))
  on('tool.call', () => ({ result: { ran: true } }))
  await $.session.start(START)
  expect(await $.tool.call({ tool: 'Bash', command: 'ls' })).toEqual({ result: { ran: true } })
  expect(calls).toEqual([])
})
