import { expect, test } from 'claude-code/testing'
import { DEADLINE, Failures, HOOKS_CAP, INERT_AFTER, MESSAGE_CAP, classify, hookKey, offeredCaps, scrub } from '../hooks/health'
import { parseReply } from '../hooks/ctl'
import { lexical } from '../hooks/guard'

test('an error is classed by what it says of the engine, the clock or the hook', async () => {
  expect(classify(new TypeError('$.prompt.submit is not a function'))).toBe('api')
  expect(classify(new TypeError("Cannot read properties of undefined (reading 'submit')"))).toBe('api')
  expect(classify(new Error('no implementation for tool.register'))).toBe('api')
  expect(classify(new Error('removed by plugin'))).toBe('api')
  expect(classify(new ReferenceError('h is not defined'))).toBe('api')
  expect(classify(new Error('the command timed out after 5000 ms'))).toBe('timeout')
  expect(classify(new Error('boom'))).toBe('hook')
  expect(classify('a string')).toBe('hook')
  // A TypeError about a value is a bug in a hook, not a changed engine.
  expect(classify(new TypeError('Invalid URL'))).toBe('hook')
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

test('failures are counted per hook, carried as three fields, and marked sent', async () => {
  const f = new Failures()
  expect(f.fields()).toEqual({})
  expect(f.unsent()).toBe(false)
  f.note('tool.call', 'hook', new Error('boom'))
  f.note('tool.call', 'hook', new Error('boom'))
  f.note('whoami', 'shape', 'a whoami reply that names no pane')
  expect(f.count('tool.call')).toBe(2)
  expect(f.total()).toBe(3)
  expect(f.unsent()).toBe(true)
  expect(f.fields()).toEqual({ fail: 'tool.call:2,whoami:1', fail_class: 'shape', fail_msg: 'a whoami reply that names no pane' })
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

test('the mod is spent after five thrown errors, or five ctl calls in a row nobody answered', async () => {
  const thrown = new Failures()
  for (let i = 0; i < INERT_AFTER - 1; i += 1) thrown.note('tool.call', 'hook', new Error('x'))
  expect(thrown.spent()).toBe(false)
  thrown.note('feature.tools', 'api', new Error('x'))
  expect(thrown.spent()).toBe(true)

  const unasked = new Failures()
  for (let i = 0; i < INERT_AFTER - 1; i += 1) unasked.note('report', 'timeout', 'no reply in time')
  // An answer in between: the broker is there after all.
  unasked.answered()
  for (let i = 0; i < INERT_AFTER - 1; i += 1) unasked.note('report', 'ctl', 'could not be asked')
  expect(unasked.spent()).toBe(false)
  unasked.note('report', 'ctl', 'could not be asked')
  expect(unasked.spent()).toBe(true)

  // A reply of the wrong shape is counted and never switches the mod off alone.
  const shape = new Failures()
  for (let i = 0; i < 50; i += 1) shape.note('whoami', 'shape', 'x')
  expect(shape.spent()).toBe(false)
})

test('a session nobody types into is offered no inbox and no ask', async () => {
  const all = ['status', 'answer', 'context', 'tools', 'guard', 'inbox', 'ask']
  expect(offeredCaps(all, true)).toEqual(all)
  expect(offeredCaps(all, false)).toEqual(['status', 'answer', 'context', 'tools', 'guard'])
})

test('the deadlines inside session.start fit under the hook budget', async () => {
  expect(DEADLINE.hello + DEADLINE.whoami).toBeLessThan(10_000)
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
