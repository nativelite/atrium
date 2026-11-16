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
  await clock.advance(4_500)
  expect(started).toBe(false)
  // 4 s of deadline and 1 s of watchdog: the start is let go.
  await clock.advance(600)
  await starting
  expect(started).toBe(true)
  // The mod could not ask atrium, and the tool call is not its business.
  const out = await $.tool.call({ tool: 'Bash', command: 'ls' })
  expect(out).toEqual({ result: { ran: true } })
  expect(ran).toEqual(['Bash'])
  // The first retry, three seconds on: atrium answers, and learns what happened.
  await clock.advance(3_100)
  await clock.settle()
  expect(calls.filter(c => c[0] === 'hello').length).toBe(2)
  const said = reported(calls)
  expect(said.fail).toBe('ctl.hello:1')
  expect(said.fail_class).toBe('timeout')
  expect(said.fail_at).toBe('ctl.hello')
})

test('an atrium that is stalled through the whole start is found again: the hello keeps retrying', async ($, on) => {
  const clock = mock.clock(on)
  // Nothing answers for the first forty seconds (the roster is still being
  // spawned, the loop is not turning); then atrium is there.
  let up = false
  const { calls, ran } = atrium(on, (verb, a, n) => (up ? healthy(verb, a, n) : never()))
  on('tool.register', (_, e) => ({ value: { tool: `mcp__atrium__${e.name}` } }))
  const starting = $.session.start(START)
  await clock.advance(5_100)
  await starting
  const hellos = (): number => calls.filter(c => c[0] === 'hello').length
  // Inside atrium's thirty-second window, with every try hanging for its
  // full five seconds: the first, and two more (at 8 s and 19 s).
  await clock.advance(24_900)
  expect(hellos()).toBe(3)
  // Tool calls run all the while, and the mod asks atrium nothing else.
  expect(await $.tool.call({ tool: 'Bash', command: 'ls' })).toEqual({ result: { ran: true } })
  expect(calls.every(c => c[0] === 'hello')).toBe(true)
  await clock.advance(10_000)
  up = true
  // The next try is taken: the mod is live, late and whole.
  await clock.advance(60_000)
  await clock.settle()
  expect(calls.some(c => c[0] === 'whoami')).toBe(true)
  expect(await $.tool.call({ tool: 'Bash', command: 'ls' })).toEqual({ result: { ran: true } })
  await clock.settle()
  expect(calls.some(c => c[0] === 'report' && c.some(t => t.startsWith('status=')))).toBe(true)
  expect(ran).toEqual(['Bash', 'Bash'])
  // And it was never a storm: nine hellos at the very most in 100 seconds.
  expect(hellos()).toBeLessThan(10)
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

test('a broker that stops answering pauses the mod, which comes back when it answers again', async ($, on) => {
  const clock = mock.clock(on)
  // Healthy through the start; then nothing answers; later it does again.
  let up = true
  const { calls, ran } = atrium(on, (verb, a, n) => (up ? healthy(verb, a, n) : never()))
  on('tool.register', (_, e) => ({ value: { tool: `mcp__atrium__${e.name}` } }))
  await $.session.start(START)
  await clock.settle()
  const count = (verb: string): number => calls.filter(c => c[0] === verb).length
  const before = count('report')
  up = false
  for (let i = 0; i < 8; i += 1) {
    expect(await $.tool.call({ tool: 'Bash', command: `echo ${i}` })).toEqual({ result: { ran: true } })
    await clock.advance(6_100)
  }
  // Five reports went unanswered, one at a time; then the mod stopped asking.
  expect(ran.length).toBe(8)
  expect(count('report') - before).toBe(5)
  const paused = calls.length
  // Paused: tool calls cost atrium nothing, and the only thing the mod does
  // is say hello on its backoff (3, 6, 12, 24, 30, 60 s...).
  for (let i = 0; i < 20; i += 1) {
    expect(await $.tool.call({ tool: 'Bash', command: 'ls' })).toEqual({ result: { ran: true } })
    await clock.advance(1_000)
  }
  const since = calls.slice(paused)
  expect(since.every(c => c[0] === 'hello')).toBe(true)
  expect(since.length).toBeLessThan(5)
  // Atrium answers again: the next hello is taken and the mod reports again.
  up = true
  await clock.advance(130_000)
  await clock.settle()
  const resumed = count('report')
  expect(await $.tool.call({ tool: 'Bash', command: 'ls' })).toEqual({ result: { ran: true } })
  await clock.settle()
  expect(count('report')).toBeGreaterThan(resumed)
  // What it missed is told: the five that went unanswered.
  expect(reported(calls).fail).toContain('ctl.report:5')
})

test('a bug in a hook body is the hook, not the engine: five of them switch the mod off for good', async ($, on) => {
  const clock = mock.clock(on)
  const { calls, ran } = atrium(on, healthy)
  on('tool.register', (_, e) => ({ value: { tool: `mcp__atrium__${e.name}` } }))
  await $.session.start(START)
  await clock.settle()
  // An event whose data is not what the hook reads: `e.changed` is missing,
  // so `e.changed.includes` is a TypeError on undefined, in the mod's own code.
  for (let i = 0; i < 5; i += 1) {
    await $.session.measure({ context: { percent: 10 } } as never).catch(() => undefined)
  }
  await clock.settle()
  const said = reported(calls)
  expect(said.fail).toBe('session.measure:5')
  expect(said.fail_class).toBe('hook')
  expect(said.fail_at).toBe('session.measure')
  // Off for the session: no hello is said again, whatever the clock does.
  const after = calls.length
  expect(await $.tool.call({ tool: 'Bash', command: 'ls' })).toEqual({ result: { ran: true } })
  await clock.advance(600_000)
  expect(calls.length).toBe(after)
  expect(ran).toEqual(['Bash'])
})

test('a guard that hangs on the engine lets the edit through after two seconds', async ($, on) => {
  const clock = mock.clock(on)
  const who = { ...WHO, files: ['src/a.ts'], item: 'M1', cwd: '/work' }
  const { calls, ran } = atrium(on, verb => (verb === 'hello' ? HELLO : verb === 'whoami' ? who : OK))
  on('tool.register', (_, e) => ({ value: { tool: `mcp__atrium__${e.name}` } }))
  on('session.cwd', () => never())
  await $.session.start(START)
  let out: unknown
  const editing = $.tool.call({ tool: 'Edit', file_path: '/work/src/b.ts', old_string: 'a', new_string: 'b' }).then(r => {
    out = r
  })
  await clock.advance(1_900)
  expect(out).toBeUndefined()
  await clock.advance(200)
  await editing
  // The guard could not judge in time: the edit is not its to refuse.
  expect(out).toEqual({ result: { ran: true } })
  expect(ran).toEqual(['Edit'])
  await clock.settle()
  const said = reported(calls)
  expect(said.fail).toContain('guard:1')
  expect(said.fail_class).toBe('timeout')
})

test('atrium sets the mod aside: it stops asking, and tool calls run', async ($, on) => {
  const clock = mock.clock(on)
  const { calls, ran } = atrium(on, verb =>
    verb === 'hello' ? HELLO : verb === 'whoami' ? WHO : { ok: false, err: 'mod set aside: no hello within its window; the pane runs without it', final: true },
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

/** An interactive start: the engine calls the pane and the inbox need, answered. */
function interactive(on: On): { submitted: string[] } {
  const submitted: string[] = []
  on('command.register', (_, e) => ({ value: { command: e.name } }))
  on('ui.panes', () => ({ value: [] }))
  on('prompt.submit', (_, e) => {
    submitted.push(e.text)
    return { text: e.text }
  })
  on('tool.register', (_, e) => ({ value: { tool: `mcp__atrium__${e.name}` } }))
  return { submitted }
}

const HELLO_ALL = { ok: true, pane: 1, accepted: ['status', 'answer', 'context', 'tools', 'guard', 'inbox', 'ask'] }

test('a delivery that lands as the mod is set aside is still submitted, not dropped', async ($, on) => {
  const clock = mock.clock(on)
  // The held `inbox --wait` answers only when the test lets it.
  let land: (reply: unknown) => void = () => undefined
  const held = new Promise<unknown>(resolve => {
    land = resolve
  })
  const { calls } = atrium(on, (verb, args) => {
    if (verb === 'hello') return HELLO_ALL
    if (verb === 'whoami') return WHO
    if (verb === 'inbox' && args.includes('--wait')) return held
    if (verb === 'report') return { ok: false, err: 'mod set aside: it reported repeated failures; atrium treats pane 1 as one without a mod', final: true }
    return OK
  })
  const { submitted } = interactive(on)
  await $.session.start({ ...START, isInteractive: true })
  await clock.settle()
  // The first report came back "set aside": the mod is off, its loop stopped.
  expect(calls.some(c => c[0] === 'report')).toBe(true)
  const stopped = calls.length
  // And then the reply atrium had already written arrives, with a delivery
  // it took out of its queue for this pane.
  land({ ok: true, items: [{ kind: 'send', text: '[atrium: from pane 0] run the tests' }] })
  await clock.settle()
  expect(submitted).toEqual(['[atrium: from pane 0] run the tests'])
  // Submitted, and no new call made on the way: the mod stays off.
  await clock.advance(120_000)
  expect(calls.length).toBe(stopped)
})

test('a subagent whose pane has no mod is closed at once, with the Agent tool named instead', async ($, on) => {
  const clock = mock.clock(on)
  const who = { ...WHO, subagents: 'panes' }
  const { calls, ran } = atrium(on, (verb, args) => {
    if (verb === 'hello') return HELLO
    if (verb === 'whoami') return who
    if (verb === 'spawn') return { ok: true, pane: 7, role: 'look-at-the-parser' }
    // The child was launched without the mod: its answer reply has no `mod`.
    if (verb === 'answer') return { ok: true, pane: 7, status: null, seq: null, answer: null }
    if (verb === 'wait') return never()
    return { ok: true, pane: Number(args[0] ?? 1) }
  })
  on('tool.register', (_, e) => ({ value: { tool: `mcp__atrium__${e.name}` } }))
  on('ui.notice', () => ({ value: undefined }))
  await $.session.start(START)
  const out = await $.tool.call({ tool: 'mcp__atrium__subagent', description: 'look at the parser', prompt: 'read src/parse.rs' } as never)
  await clock.settle()
  // No thirty-minute wait, no task sent, and the pane it opened is closed.
  expect(JSON.stringify(out)).toContain('has no atrium mod')
  expect(JSON.stringify(out)).toContain('Subagents run with the Agent tool in this pane from now on')
  expect(calls.some(c => c[0] === 'kill' && c[1] === '7')).toBe(true)
  expect(calls.some(c => c[0] === 'send')).toBe(false)
  expect(calls.some(c => c[0] === 'wait')).toBe(false)
  expect(ran).toEqual([])
})

test('a subagent whose mod is set aside while it works is closed within one slice', async ($, on) => {
  const clock = mock.clock(on)
  const who = { ...WHO, subagents: 'panes' }
  let answers = 0
  const { calls } = atrium(on, verb => {
    if (verb === 'hello') return HELLO
    if (verb === 'whoami') return who
    if (verb === 'spawn') return { ok: true, pane: 7, role: 'x' }
    if (verb === 'answer') {
      answers += 1
      // Expected to say hello at first; set aside by the time the wait ends.
      return { ok: true, pane: 7, status: null, seq: null, answer: null, mod: { state: answers === 1 ? 'pending' : 'set-aside' } }
    }
    if (verb === 'wait') return { ok: false, err: 'timeout after 30s' }
    return OK
  })
  on('tool.register', (_, e) => ({ value: { tool: `mcp__atrium__${e.name}` } }))
  on('ui.notice', () => ({ value: undefined }))
  await $.session.start(START)
  const out = await $.tool.call({ tool: 'mcp__atrium__subagent', description: 'x', prompt: 'y' } as never)
  await clock.settle()
  expect(JSON.stringify(out)).toContain('had its atrium mod set aside')
  expect(calls.filter(c => c[0] === 'wait').length).toBe(1)
  expect(calls.some(c => c[0] === 'kill' && c[1] === '7')).toBe(true)
})

test('the Agent tool is taken over only while a pane could answer for it', async ($, on) => {
  const clock = mock.clock(on)
  // The policy is `panes`, the subagent tool is registered, and atrium says
  // whether the session's mod is backed out.
  let tripped = false
  atrium(on, verb => (verb === 'hello' ? HELLO : verb === 'whoami' ? { ...WHO, subagents: 'panes', mod_tripped: tripped } : OK))
  on('tool.register', (_, e) => ({ value: { tool: `mcp__atrium__${e.name}` } }))
  let native = 0
  on('agent.spawn', () => {
    native += 1
    return { deny: 'the engine ran it (the test stands for the engine)' }
  })
  await $.session.start(START)
  const spawn = (): Promise<unknown> => $.agent.spawn({ tool_use_id: 't1', prompt: 'p', description: 'd' } as never)
  // Healthy: refused by the mod, with the pointer; the engine never sees it.
  expect(JSON.stringify(await spawn())).toContain('atrium_subagent')
  expect(native).toBe(0)
  // The breaker trips; the mod learns it at the next turn's whoami.
  tripped = true
  await $.turn.start({ prompt: 'x' } as never).catch(() => undefined)
  await clock.settle()
  // Now a pane would have no mod to answer with: the engine's own tool runs.
  expect(JSON.stringify(await spawn())).toContain('the engine ran it')
  expect(native).toBe(1)
})

test('a busy listener at hello is not a refusal: the hello is said again', async ($, on) => {
  const clock = mock.clock(on)
  // What the unix listener answers while atrium's loop is not taking
  // connections (a fleet launch): a reply, ok false, and not final.
  let busy = true
  const { calls, ran } = atrium(on, (verb, a, n) => (busy ? { ok: false, err: 'server busy; ask again' } : healthy(verb, a, n)))
  on('tool.register', (_, e) => ({ value: { tool: `mcp__atrium__${e.name}` } }))
  await $.session.start(START)
  expect(calls.map(c => c[0])).toEqual(['hello'])
  expect(await $.tool.call({ tool: 'Bash', command: 'ls' })).toEqual({ result: { ran: true } })
  await clock.advance(3_100)
  expect(calls.filter(c => c[0] === 'hello').length).toBe(2)
  busy = false
  await clock.advance(6_100)
  await clock.settle()
  // Taken on the third try: the mod is live and reports.
  expect(calls.filter(c => c[0] === 'hello').length).toBe(3)
  expect(calls.some(c => c[0] === 'whoami')).toBe(true)
  expect(await $.tool.call({ tool: 'Bash', command: 'ls' })).toEqual({ result: { ran: true } })
  await clock.settle()
  expect(calls.some(c => c[0] === 'report' && c.some(t => t.startsWith('status=')))).toBe(true)
  expect(ran).toEqual(['Bash', 'Bash'])
})

test('a hello atrium refuses for good, and one it grants nothing, end the mod; nothing else does', async ($, on) => {
  const clock = mock.clock(on)
  const { calls } = atrium(on, verb =>
    verb === 'hello' ? { ok: false, err: 'pane 1 was not launched with the mod by this atrium, so its hello is not taken', final: true } : OK,
  )
  await $.session.start(START)
  await clock.advance(600_000)
  // Said once, never again.
  expect(calls.map(c => c[0])).toEqual(['hello'])
})

test('a busy listener mid-session ends nothing: the mod goes on reporting', async ($, on) => {
  const clock = mock.clock(on)
  let busy = false
  const { calls, ran } = atrium(on, (verb, a, n) => (busy ? { ok: false, err: 'server busy; ask again' } : healthy(verb, a, n)))
  on('tool.register', (_, e) => ({ value: { tool: `mcp__atrium__${e.name}` } }))
  await $.session.start(START)
  await clock.settle()
  busy = true
  for (let i = 0; i < 12; i += 1) {
    expect(await $.tool.call({ tool: 'Bash', command: `echo ${i}` })).toEqual({ result: { ran: true } })
    await $.turn.start({ prompt: 'x' } as never).catch(() => undefined)
    await clock.advance(1_000)
  }
  busy = false
  const before = calls.filter(c => c[0] === 'report').length
  expect(await $.tool.call({ tool: 'Bash', command: 'ls' })).toEqual({ result: { ran: true } })
  await $.turn.start({ prompt: 'x' } as never).catch(() => undefined)
  await clock.settle()
  // Still live: no hello was needed, and the next report went out.
  expect(calls.filter(c => c[0] === 'hello').length).toBe(1)
  expect(calls.filter(c => c[0] === 'report').length).toBeGreaterThan(before)
  expect(ran.length).toBe(13)
})

test('a failure counted while a report is in flight is carried by the next one', async ($, on) => {
  const clock = mock.clock(on)
  // The first report is held; the test lets it go.
  let release: () => void = () => undefined
  const held = new Promise<unknown>(resolve => {
    release = () => resolve(OK)
  })
  const { calls } = atrium(on, (verb, _a, n) => (verb === 'hello' ? HELLO : verb === 'whoami' ? WHO : verb === 'report' && n === 1 ? held : OK))
  on('tool.register', (_, e) => ({ value: { tool: `mcp__atrium__${e.name}` } }))
  await $.session.start(START)
  await clock.settle()
  expect(calls.filter(c => c[0] === 'report').length).toBe(1)
  // While it is in flight, a hook of the mod throws.
  await $.session.measure({ context: { percent: 10 } } as never).catch(() => undefined)
  await clock.settle()
  expect(reported(calls)).toEqual({})
  release()
  await clock.settle()
  // The report that was in flight carried nothing of it; the next one does.
  const said = reported(calls)
  expect(said.fail).toBe('session.measure:1')
  expect(said.fail_thrown).toBe('1')
})

test('the fifth failure is told even when it lands during a report: the farewell goes out', async ($, on) => {
  const clock = mock.clock(on)
  let release: () => void = () => undefined
  let hold = false
  const { calls } = atrium(on, verb => {
    if (verb === 'hello') return HELLO
    if (verb === 'whoami') return WHO
    if (verb === 'report' && hold) {
      hold = false
      return new Promise<unknown>(resolve => {
        release = () => resolve(OK)
      })
    }
    return OK
  })
  on('tool.register', (_, e) => ({ value: { tool: `mcp__atrium__${e.name}` } }))
  await $.session.start(START)
  await clock.settle()
  const broken = (): Promise<unknown> => $.session.measure({ context: { percent: 10 } } as never).catch(() => undefined)
  // Four failures, each told as it happens.
  for (let i = 0; i < 4; i += 1) {
    if (i === 3) hold = true
    await broken()
    await clock.settle()
  }
  // The report of the fourth is in flight. The fifth lands now, and the mod
  // switches itself off.
  await broken()
  await clock.settle()
  expect(reported(calls).fail).toBe('session.measure:4')
  release()
  await clock.settle()
  // atrium is told of all five, by the one report the mod sends on its way out.
  const said = reported(calls)
  expect(said.fail).toBe('session.measure:5')
  expect(said.fail_thrown).toBe('5')
  // And then it is off: nothing more is asked.
  const after = calls.length
  await $.tool.call({ tool: 'Bash', command: 'ls' })
  await clock.advance(600_000)
  expect(calls.length).toBe(after)
})

test('at the end of the session a paused mod stops its hello, and its tools answer plainly meanwhile', async ($, on) => {
  const clock = mock.clock(on)
  let up = true
  const { calls } = atrium(on, (verb, a, n) => (up ? healthy(verb, a, n) : never()))
  on('tool.register', (_, e) => ({ value: { tool: `mcp__atrium__${e.name}` } }))
  on('session.end', () => ({ sessionId: 's1' }) as never)
  await $.session.start(START)
  await clock.settle()
  up = false
  for (let i = 0; i < 6; i += 1) {
    await $.tool.call({ tool: 'Bash', command: `echo ${i}` })
    await clock.advance(6_100)
  }
  // Paused. One of its own tools, called now, says so instead of hanging on
  // a broker that is not there or falling through to nothing.
  const reports = calls.filter(c => c[0] === 'report').length
  const out = await $.tool.call({ tool: 'mcp__atrium__list' } as never)
  expect(JSON.stringify(out)).toContain("atrium's mod is paused")
  expect(calls.filter(c => c[0] === 'list').length).toBe(0)
  expect(calls.filter(c => c[0] === 'report').length).toBe(reports)
  // The session ends: the scheduled hello must not fire into it.
  const hellos = calls.filter(c => c[0] === 'hello').length
  await $.session.end({ reason: 'exit' } as never)
  await clock.advance(900_000)
  expect(calls.filter(c => c[0] === 'hello').length).toBe(hellos)
})

test('after a child with no mod, the Agent tool is the engine\'s again: the message and the tool agree', async ($, on) => {
  const clock = mock.clock(on)
  // The policy is `panes` and atrium does not (yet) say the mod is backed out.
  const who = { ...WHO, subagents: 'panes', mod_tripped: false }
  atrium(on, verb => {
    if (verb === 'hello') return HELLO
    if (verb === 'whoami') return who
    if (verb === 'spawn') return { ok: true, pane: 7, role: 'x' }
    if (verb === 'answer') return { ok: true, pane: 7, status: null, seq: null, answer: null }
    return OK
  })
  on('tool.register', (_, e) => ({ value: { tool: `mcp__atrium__${e.name}` } }))
  on('ui.notice', () => ({ value: undefined }))
  let native = 0
  on('agent.spawn', () => {
    native += 1
    return { deny: 'the engine ran it (the test stands for the engine)' }
  })
  await $.session.start(START)
  const spawn = (): Promise<unknown> => $.agent.spawn({ tool_use_id: 't1', prompt: 'p', description: 'd' } as never)
  // Before: the mod takes the Agent tool over.
  expect(JSON.stringify(await spawn())).toContain('atrium_subagent')
  const out = await $.tool.call({ tool: 'mcp__atrium__subagent', description: 'x', prompt: 'y' } as never)
  expect(JSON.stringify(out)).toContain('Subagents run with the Agent tool in this pane from now on')
  await clock.settle()
  // After: what the message said to call is not refused.
  expect(JSON.stringify(await spawn())).toContain('the engine ran it')
  expect(native).toBe(1)
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
