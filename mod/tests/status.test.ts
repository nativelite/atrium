import { expect, test } from 'claude-code/testing'
import { ANSWER_CAP, Reporter, clipAnswer, toArgs, transition } from '../hooks/status'
import type { Status } from '../hooks/status'
import { acceptedCaps, parseReply } from '../hooks/ctl'

test('a start is at-the-prompt when a person is there, working otherwise', async () => {
  expect(transition(undefined, { kind: 'start', interactive: true })).toEqual({ status: 'waiting-prompt' })
  expect(transition(undefined, { kind: 'start', interactive: false })).toEqual({ status: 'working' })
})

test('an ask is waiting-approval with the tool as the reason; the next tool call is working', async () => {
  let s: Status | undefined = 'working'
  expect(transition(s, { kind: 'tool.check', decision: 'allow', tool: 'Read' })).toBeUndefined()
  const ask = transition(s, { kind: 'tool.check', decision: 'ask', tool: 'Bash' })
  expect(ask).toEqual({ status: 'waiting-approval', reason: 'Bash' })
  s = ask?.status
  expect(transition(s, { kind: 'tool.call', tool: 'Bash' })).toEqual({ status: 'working' })
  expect(transition(s, { kind: 'tool.check', decision: 'deny', tool: 'Bash' })).toBeUndefined()
})

test('a turn that ends on an error or a refusal is errored with its reason', async () => {
  expect(transition('working', { kind: 'turn.complete', reason: 'error', agent: false })).toEqual({
    status: 'errored',
    reason: 'error',
  })
  expect(transition('working', { kind: 'turn.complete', reason: 'refusal', agent: false })).toEqual({
    status: 'errored',
    reason: 'refusal',
  })
  expect(transition('working', { kind: 'turn.complete', reason: 'answer', agent: false })).toEqual({
    status: 'waiting-prompt',
  })
  expect(transition('working', { kind: 'turn.complete', reason: 'aborted', agent: false })).toEqual({
    status: 'waiting-prompt',
  })
})

test("a subagent's turn says nothing about the pane", async () => {
  expect(transition('working', { kind: 'turn.complete', reason: 'answer', agent: true })).toBeUndefined()
  expect(transition('working', { kind: 'turn.complete', reason: 'error', agent: true })).toBeUndefined()
})

test('the same status twice is reported once', async () => {
  expect(transition('working', { kind: 'turn.start' })).toBeUndefined()
  expect(transition('working', { kind: 'tool.call', tool: 'Edit' })).toBeUndefined()
  expect(transition('waiting-approval', { kind: 'tool.check', decision: 'ask', tool: 'Edit' })).toBeUndefined()
  expect(transition('ended', { kind: 'session.end', reason: 'other' })).toBeUndefined()
  expect(transition('working', { kind: 'session.end', reason: 'clear' })).toEqual({ status: 'ended', reason: 'clear' })
})

test('answers are clipped and fields become k=v tokens', async () => {
  expect(clipAnswer('x'.repeat(ANSWER_CAP + 5)).length).toBe(ANSWER_CAP)
  expect(clipAnswer('short')).toBe('short')
  expect(toArgs({ status: 'working', reason: 'Bash' })).toEqual(['status=working', 'reason=Bash'])
  expect(toArgs({ answer: 'a=b c' })).toEqual(['answer=a=b c'])
})

test('the reporter sends one at a time and merges what arrives meanwhile', async () => {
  const sent: Record<string, string>[] = []
  let release: () => void = () => undefined
  const r = new Reporter(fields => {
    sent.push(fields)
    return new Promise<void>(resolve => {
      release = resolve
    })
  })
  r.push({ status: 'working' })
  r.push({ status: 'waiting-approval', reason: 'Bash' })
  r.push({ context: '40' })
  r.push({ status: 'working' })
  expect(sent.length).toBe(1)
  expect(sent[0]).toEqual({ status: 'working' })
  release()
  await Promise.resolve()
  await Promise.resolve()
  expect(sent.length).toBe(2)
  expect(sent[1]).toEqual({ status: 'working', reason: 'Bash', context: '40' })
  release()
  await r.idle()
  expect(sent.length).toBe(2)
})

test('a failed send is dropped and the next one still goes', async () => {
  const sent: string[] = []
  let fail = true
  const r = new Reporter(async fields => {
    sent.push(fields.status ?? '?')
    if (fail) throw new Error('channel gone')
  })
  r.push({ status: 'working' })
  await r.idle()
  fail = false
  r.push({ status: 'idle' })
  await r.idle()
  expect(sent).toEqual(['working', 'idle'])
})

test('a reply is read off the last stdout line, and anything else is a refusal', async () => {
  const ok = parseReply({ exitCode: 0, stdout: 'noise\n{"ok":true,"pane":3,"accepted":["status",7]}\n', stderr: '' })
  expect(ok.ok).toBe(true)
  expect([...acceptedCaps(ok)]).toEqual(['status'])
  const usage = parseReply({ exitCode: 2, stdout: '', stderr: 'atrium ctl: not in a ctl session\n' })
  expect(usage.ok).toBe(false)
  expect(usage.err).toBe('atrium ctl: not in a ctl session')
  const silent = parseReply({ exitCode: 1, stdout: '', stderr: '' })
  expect(silent.err).toBe('exit 1 with no reply')
  const odd = parseReply({ exitCode: 0, stdout: '[1,2]', stderr: '' })
  expect(odd.ok).toBe(false)
  expect([...acceptedCaps({ ok: true })]).toEqual([])
})
