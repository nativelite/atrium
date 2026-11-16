import { expect, test } from 'claude-code/testing'
import { DEADLINE, FAIL_WINDOW_MS, Failures, HELLO_BACKOFF_S, HOOKS_CAP, INERT_AFTER, MESSAGE_CAP, WATCHDOG_MS, classify, engineLacks, helloBackoffMs, hookKey, isFinal, offeredCaps, scrub } from '../hooks/health'
import { childMod } from '../hooks/agents'
import { parseReply } from '../hooks/ctl'
import { lexical } from '../hooks/guard'

test('an error is classed by what it says, and a bug in a hook is not a changed engine', async () => {
  // What the engine itself says of a call nothing answers, or a plugin withheld.
  expect(classify(new Error('no implementation for tool.register'))).toBe('api')
  expect(classify(new Error('removed by plugin'))).toBe('api')
  expect(classify(new Error('the command timed out after 5000 ms'))).toBe('timeout')
  expect(classify(new Error('boom'))).toBe('hook')
  expect(classify('a string')).toBe('hook')
  // A null dereference on an event's data, a typo, a bad value: bugs in the
  // mod, counted toward switching it off, never "the interface changed".
  expect(classify(new TypeError("Cannot read properties of undefined (reading 'percent')"))).toBe('hook')
  expect(classify(new TypeError('e.changed.includes is not a function'))).toBe('hook')
  expect(classify(new ReferenceError('h is not defined'))).toBe('hook')
  expect(classify(new TypeError('Invalid URL'))).toBe('hook')
})

test('a missing engine method is told at the call that found it', async () => {
  // Asked only where the mod calls `$.noun.method(...)`.
  expect(engineLacks(new TypeError('$.prompt.submit is not a function'))).toBe(true)
  expect(engineLacks(new TypeError("Cannot read properties of undefined (reading 'submit')"))).toBe(true)
  expect(engineLacks(new Error('no implementation for prompt.submit'))).toBe(true)
  expect(engineLacks(new Error('removed by plugin'))).toBe(true)
  // The call was made and failed for a reason of its own.
  expect(engineLacks(new Error('the prompt queue is full'))).toBe(false)
  expect(engineLacks(new TypeError('Invalid argument'))).toBe(false)
  expect(engineLacks(new RangeError('x is not a function'))).toBe(false)
  expect(engineLacks('is not a function')).toBe(false)
})

test('a scrubbed message carries no quoted text, no path, no long token, and is short', async () => {
  const s = scrub(new SyntaxError('Unexpected token \'h\', "hello secret wor"... is not valid JSON'))
  expect(s.startsWith('SyntaxError: ')).toBe(true)
  expect(s).not.toContain('secret')
  expect(scrub(new Error('cannot open C:\\Users\\me\\.env or /home/me/.ssh/id_rsa'))).toBe('Error: cannot open … or …')
  expect(scrub(new Error('token sk-ant-ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 refused'))).toBe('Error: token … refused')
  expect(scrub(new Error('line one\nline two\u0007'))).toBe('Error: line one line two')
  expect(Array.from(scrub(new Error('x '.repeat(500)))).length).toBe(MESSAGE_CAP)
  expect(scrub(new Error(''))).toBe('Error')
  expect(scrub(undefined)).toBe('Error')
  expect(scrub({ toString: () => 'secret' })).toBe('Error')
})

test('failures are counted per hook and carried as five fields', async () => {
  const f = new Failures()
  expect(f.fields()).toEqual({})
  expect(f.unsent()).toBe(false)
  f.note('tool.call', 'hook', new Error('boom'))
  f.note('tool.call', 'hook', new Error('boom'))
  f.note('ctl.whoami', 'shape', 'a whoami reply that names no pane')
  expect(f.count('tool.call')).toBe(2)
  expect(f.total()).toBe(3)
  expect(f.unsent()).toBe(true)
  expect(f.fields()).toEqual({
    fail: 'tool.call:2,ctl.whoami:1',
    fail_class: 'shape',
    fail_at: 'ctl.whoami',
    // What its own hooks threw: two. The reply of the wrong shape is not one.
    fail_thrown: '2',
    fail_msg: 'a whoami reply that names no pane',
  })
})

test('a report clears only the failures it carried', async () => {
  const f = new Failures()
  f.note('tool.call', 'hook', new Error('one'))
  // The report goes out with a picture of one failure...
  const picture = f.snapshot()
  expect(picture.fields.fail).toBe('tool.call:1')
  // ...and while it is in flight a second failure is counted.
  f.note('tool.call', 'hook', new Error('two'))
  f.sent(picture.generation)
  // atrium was told of one. The second is still to be told.
  expect(f.unsent()).toBe(true)
  const next = f.snapshot()
  expect(next.fields.fail).toBe('tool.call:2')
  f.sent(next.generation)
  expect(f.unsent()).toBe(false)
  // An older picture acknowledged late un-tells nothing.
  f.sent(picture.generation)
  expect(f.unsent()).toBe(false)
  // The same for a feature lost while a report was in flight.
  const g = new Failures()
  g.note('ctl.report', 'timeout', 'no reply in time')
  const first = g.snapshot()
  g.note('feature.inbox', 'api', new Error('no implementation for prompt.submit'))
  g.sent(first.generation)
  expect(g.unsent()).toBe(true)
  expect(g.snapshot().fields.fail_at).toBe('feature.inbox')
})

test('only a refusal that says it is final is atrium\'s last word', async () => {
  expect(isFinal({ ok: false, err: 'mod set aside: no hello within its window', final: true } as never)).toBe(true)
  // What a listener too busy to take the request answers on unix.
  expect(isFinal({ ok: false, err: 'server busy; ask again' } as never)).toBe(false)
  expect(isFinal({ ok: false, err: 'request too large' } as never)).toBe(false)
  expect(isFinal({ ok: false, err: 'mod set aside: looks final, says nothing' } as never)).toBe(false)
  expect(isFinal({ ok: false, final: 'yes' } as never)).toBe(false)
  expect(isFinal({ ok: true, final: true } as never)).toBe(false)
})

test('a hook name is a safe key, and past the cap the rest count as other', async () => {
  expect(hookKey('tool.call')).toBe('tool.call')
  expect(hookKey('a=b,c:d e')).toBe('abcde')
  expect(hookKey('')).toBe('other')
  const f = new Failures()
  for (let i = 0; i < HOOKS_CAP + 5; i += 1) f.note(`hook${i}`, 'shape', 'x')
  expect(f.fields().fail?.split(',').length).toBe(HOOKS_CAP + 1)
  expect(f.count('other')).toBe(5)
})

test('five errors of its own hooks within ten minutes break the mod; five spread over hours do not', async () => {
  const t0 = 1_000_000
  const thrown = new Failures()
  for (let i = 0; i < INERT_AFTER - 1; i += 1) thrown.note('tool.call', 'hook', new Error('x'), t0 + i * 1000)
  expect(thrown.broken(t0 + 5_000)).toBe(false)
  thrown.note('guard', 'hook', new Error('x'), t0 + 6_000)
  expect(thrown.broken(t0 + 6_000)).toBe(true)
  expect(thrown.unreachable()).toBe(false)

  // Five accidents, two hours apart: never five in the window.
  const spread = new Failures()
  for (let i = 0; i < 5; i += 1) {
    const at = t0 + i * 2 * 3_600_000
    spread.note('tool.call', 'hook', new Error('x'), at)
    expect(spread.broken(at)).toBe(false)
  }
  // atrium is still told all five, and counts them the same way.
  expect(spread.fields().fail_thrown).toBe('5')
  // Four more inside ten minutes of the last: five in the window.
  const last = t0 + 4 * 2 * 3_600_000
  for (let i = 1; i <= 3; i += 1) spread.note('tool.call', 'hook', new Error('x'), last + i * 60_000)
  expect(spread.broken(last + 3 * 60_000)).toBe(false)
  spread.note('tool.call', 'hook', new Error('x'), last + 4 * 60_000)
  expect(spread.broken(last + 4 * 60_000)).toBe(true)
  // And it lapses: ten minutes with nothing thrown, and it is not broken.
  expect(spread.broken(last + 4 * 60_000 + FAIL_WINDOW_MS + 1)).toBe(false)

  // A feature lost to a missing engine method costs that feature, however
  // many are lost: it is not one of the five.
  const features = new Failures()
  for (const name of ['pane', 'inbox', 'tools', 'guard', 'paths', 'captions', 'ask']) {
    features.note(`feature.${name}`, 'api', new Error('no implementation for x'), t0)
  }
  expect(features.broken(t0)).toBe(false)
  expect(features.fields().fail_at).toBe('feature.ask')
  expect(features.fields().fail_thrown).toBe('0')

  // A wait of the mod's own on the engine that timed out (the file guard on
  // a slow disk): counted, shown, and neither broken nor a reason to pause.
  const slow = new Failures()
  for (let i = 0; i < 50; i += 1) slow.note('guard', 'timeout', new Error('the file guard timed out'), t0 + i)
  expect(slow.broken(t0 + 50)).toBe(false)
  expect(slow.unreachable()).toBe(false)
  expect(slow.count('guard')).toBe(50)
  expect(slow.fields().fail_thrown).toBe('0')
})

test('five exchanges in a row that atrium did not answer pause the mod', async () => {
  const unasked = new Failures()
  for (let i = 0; i < INERT_AFTER - 1; i += 1) unasked.note('ctl.report', 'timeout', 'no reply in time')
  // An answer in between: the broker is there after all.
  unasked.answered()
  for (let i = 0; i < INERT_AFTER - 1; i += 1) unasked.note('ctl.report', 'ctl', 'could not be asked')
  expect(unasked.unreachable()).toBe(false)
  unasked.note('ctl.report', 'ctl', 'could not be asked')
  expect(unasked.unreachable()).toBe(true)
  // Unreachable is a pause, never broken, and is not told as thrown.
  expect(unasked.broken()).toBe(false)
  expect(unasked.fields().fail_thrown).toBe('0')

  // A broker that answers things that are no replies is as good as silent:
  // five in a row pause the mod, so it does not poll such a broker for ever.
  const shape = new Failures()
  for (let i = 0; i < INERT_AFTER; i += 1) shape.note('ctl.inbox', 'shape', 'not a reply')
  expect(shape.unreachable()).toBe(true)
  expect(shape.broken()).toBe(false)
})

test('the hello backs off, lands four tries in the thirty-second window, and settles at two minutes', async () => {
  expect(HELLO_BACKOFF_S).toEqual([3, 6, 12, 24, 30, 60, 120])
  expect(helloBackoffMs(0)).toBe(3_000)
  expect(helloBackoffMs(6)).toBe(120_000)
  expect(helloBackoffMs(600)).toBe(120_000)
  expect(helloBackoffMs(-1)).toBe(3_000)
  // The first hello at 0, then at 3, 9 and 21 s when each fails at once:
  // four inside the window atrium gives a pane.
  let at = 0
  const tries = [at]
  for (let i = 0; i < 3; i += 1) {
    at += helloBackoffMs(i)
    tries.push(at)
  }
  expect(tries).toEqual([0, 3_000, 9_000, 21_000])
  expect(tries.every(t => t < 30_000)).toBe(true)
})

test('the child of a subagent call is read for a mod that could answer', async () => {
  expect(childMod({ ok: true, pane: 3, status: null, seq: null, answer: null })).toBe('none')
  expect(childMod({ ok: true, pane: 3, mod: null })).toBe('none')
  expect(childMod({ ok: true, pane: 3, mod: { state: 'pending' } })).toBe('ok')
  expect(childMod({ ok: true, pane: 3, mod: { state: 'ok' } })).toBe('ok')
  expect(childMod({ ok: true, pane: 3, mod: { state: 'no-inbox' } })).toBe('ok')
  expect(childMod({ ok: true, pane: 3, mod: { state: 'set-aside', why: 'x' } })).toBe('off')
  // A reply that could not be read decides nothing: the wait goes on.
  expect(childMod({ ok: false, err: 'timeout' })).toBe('unknown')
  expect(childMod({ ok: true, mod: 'yes' })).toBe('unknown')
  expect(childMod({ ok: true, mod: {} })).toBe('unknown')
})

test('a session nobody types into is offered no inbox and no ask', async () => {
  const all = ['status', 'answer', 'context', 'tools', 'guard', 'inbox', 'ask']
  expect(offeredCaps(all, true)).toEqual(all)
  expect(offeredCaps(all, false)).toEqual(['status', 'answer', 'context', 'tools', 'guard'])
})

test('the two calls inside session.start hold it for eight seconds at the very worst', async () => {
  // Each call's own deadline, and the watchdog behind it.
  expect(DEADLINE.hello + WATCHDOG_MS + DEADLINE.whoami + WATCHDOG_MS).toBe(8_000)
})

test('a reply says whether atrium refused or could not be asked', async () => {
  // atrium said no: no fault.
  expect(parseReply({ exitCode: 1, stdout: '{"ok":false,"err":"no such pane"}\n', stderr: '' })).toEqual({ ok: false, err: 'no such pane' })
  // No reply line: the question was never answered.
  expect(parseReply({ exitCode: 1, stdout: '', stderr: 'atrium ctl: cannot connect' }).fault).toBe('ctl')
  expect(parseReply({ exitCode: 0, stdout: '', stderr: '' }).fault).toBe('ctl')
  // A line that is JSON and not a reply.
  expect(parseReply({ exitCode: 0, stdout: '[1,2]\n', stderr: '' }).fault).toBe('shape')
  expect(parseReply({ exitCode: 0, stdout: '{"ok":"yes"}\n', stderr: '' }).fault).toBe('shape')
  expect(parseReply(undefined as never).fault).toBe('shape')
  expect(parseReply({ exitCode: 0 } as never).fault).toBe('shape')
  // A reply cannot mark itself a fault.
  expect(parseReply({ exitCode: 0, stdout: '{"ok":true,"fault":"ctl"}\n', stderr: '' })).toEqual({ ok: true })
})

test('a path the filesystem cannot place is folded by its text', async () => {
  expect(lexical('/work/src/../src/./a.ts')).toBe('/work/src/a.ts')
  expect(lexical('C:\\work\\src\\..\\a.ts')).toBe('C:\\work\\a.ts')
  expect(lexical('/../../etc')).toBe('/etc')
  expect(lexical('a/../../b')).toBe('../b')
  expect(lexical('/work/src/a.ts')).toBe('/work/src/a.ts')
})
