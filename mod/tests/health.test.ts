import { expect, test } from 'claude-code/testing'
import { DEADLINE, Failures, HELLO_BACKOFF_S, HOOKS_CAP, INERT_AFTER, MESSAGE_CAP, WATCHDOG_MS, classify, engineLacks, helloBackoffMs, hookKey, offeredCaps, scrub } from '../hooks/health'
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

test('failures are counted per hook, carried as four fields, and marked sent', async () => {
  const f = new Failures()
  expect(f.fields()).toEqual({})
  expect(f.unsent()).toBe(false)
  f.note('tool.call', 'hook', new Error('boom'))
  f.note('tool.call', 'hook', new Error('boom'))
  f.note('whoami', 'shape', 'a whoami reply that names no pane')
  expect(f.count('tool.call')).toBe(2)
  expect(f.total()).toBe(3)
  expect(f.unsent()).toBe(true)
  expect(f.fields()).toEqual({ fail: 'tool.call:2,whoami:1', fail_class: 'shape', fail_at: 'whoami', fail_msg: 'a whoami reply that names no pane' })
  f.sent()
  expect(f.unsent()).toBe(false)
  f.note('x', 'hook', new Error('again'))
  expect(f.unsent()).toBe(true)
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

test('five errors of its own hooks break the mod; five exchanges nobody answered pause it', async () => {
  const thrown = new Failures()
  for (let i = 0; i < INERT_AFTER - 1; i += 1) thrown.note('tool.call', 'hook', new Error('x'))
  expect(thrown.broken()).toBe(false)
  thrown.note('guard', 'hook', new Error('x'))
  expect(thrown.broken()).toBe(true)
  expect(thrown.unreachable()).toBe(false)

  // A feature lost to a missing engine method costs that feature, however
  // many are lost: it is not one of the five.
  const features = new Failures()
  for (const name of ['pane', 'inbox', 'tools', 'guard', 'paths', 'captions', 'ask']) {
    features.note(`feature.${name}`, 'api', new Error('no implementation for x'))
  }
  expect(features.broken()).toBe(false)
  expect(features.fields().fail_at).toBe('feature.ask')

  const unasked = new Failures()
  for (let i = 0; i < INERT_AFTER - 1; i += 1) unasked.note('ctl.report', 'timeout', 'no reply in time')
  // An answer in between: the broker is there after all.
  unasked.answered()
  for (let i = 0; i < INERT_AFTER - 1; i += 1) unasked.note('ctl.report', 'ctl', 'could not be asked')
  expect(unasked.unreachable()).toBe(false)
  unasked.note('ctl.report', 'ctl', 'could not be asked')
  expect(unasked.unreachable()).toBe(true)
  // Unreachable is a pause, never broken.
  expect(unasked.broken()).toBe(false)

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
