// The atrium mod: inside a pane's Claude Code, tell the atrium broker what the
// engine knows for certain. atrium's chrome otherwise infers a pane's status
// from the transcript file (a 7 s dwell before "waiting on you", a 60 s decay
// to idle, nothing at all for an API error); this reports the edges the
// moment they happen: `turn.start`, a `tool.check` that resolves to an ask,
// the next `tool.call`, `turn.complete` with its reason, `session.end`.
//
// It speaks `atrium ctl hello|report` with the pane's own token, so it can
// only ever speak for the pane it runs in. Outside atrium (no ATRIUM_CTL) it
// does nothing at all.

import type { EngineInterface, Register } from 'claude-code'
import { acceptedCaps, parseReply } from './ctl'
import type { Reply } from './ctl'
import { CAPS, MOD_VERSION, Reporter, clipAnswer, toArgs, transition } from './status'
import type { Status, Verdict } from './status'

/** Is this engine hosted in an atrium pane with the control plane bound? */
async function inAtrium($: EngineInterface): Promise<boolean> {
  const address = await $.env.get('ATRIUM_CTL')
  const token = await $.env.get('ATRIUM_TOKEN')
  return address !== undefined && address !== '' && token !== undefined && token !== ''
}

/**
 * Run one `atrium ctl` verb on the host, with the pane's own token from the
 * environment atrium injected at spawn, and read its one-line reply. Never
 * throws: a failure to start is a refused reply.
 */
async function ctl($: EngineInterface, args: readonly string[]): Promise<Reply> {
  const bin = (await $.env.get('ATRIUM_BIN')) || 'atrium'
  try {
    const ran = await $.process.run([bin, 'ctl', ...args], { timeoutMs: 10_000 })
    return parseReply(ran)
  } catch (e) {
    return { ok: false, err: e instanceof Error ? e.message : String(e) }
  }
}

export const register: Register = on => {
  // The module's own variables start over on a hot reload, and `session.start`
  // fires again then, so a reload is a fresh hello.
  let status: Status | undefined
  let accepted: ReadonlySet<string> = new Set()
  let reporter: Reporter | undefined

  const say = (v: Verdict | undefined): void => {
    if (v === undefined || reporter === undefined || !accepted.has('status')) return
    status = v.status
    reporter.push(v.reason === undefined ? { status: v.status } : { status: v.status, reason: v.reason })
  }

  on('session.start', async ($, e, next) => {
    const started = await next(e)
    if (!(await inAtrium($))) return started
    let engine: string | undefined
    try {
      engine = (await $.session.version()).version
    } catch {
      engine = undefined
    }
    const hello = await ctl($, [
      'hello',
      `mod=${MOD_VERSION}`,
      ...(engine === undefined ? [] : [`engine=${engine}`]),
      `caps=${CAPS.join(',')}`,
    ])
    if (!hello.ok) {
      $.ui.log(`atrium: hello refused: ${hello.err ?? 'no reply'}`, { to: 'debug' })
      return started
    }
    accepted = acceptedCaps(hello)
    reporter = new Reporter(async fields => {
      const reply = await ctl($, ['report', ...toArgs(fields)])
      if (!reply.ok) $.ui.log(`atrium: report refused: ${reply.err ?? 'no reply'}`, { to: 'debug' })
    })
    say(transition(status, { kind: 'start', interactive: e.isInteractive }))
    return started
  })

  on('turn.start', ($, e, next) => {
    say(transition(status, { kind: 'turn.start' }))
    return next(e)
  })

  on('tool.check', async ($, e, next) => {
    const verdict = await next(e)
    say(transition(status, { kind: 'tool.check', decision: verdict.decision, tool: e.tool }))
    return verdict
  })

  on('tool.call', ($, e, next) => {
    say(transition(status, { kind: 'tool.call', tool: String(e.tool) }))
    return next(e)
  })

  on('turn.complete', async ($, e, next) => {
    const done = await next(e)
    const agent = e.agentId !== undefined
    say(transition(status, { kind: 'turn.complete', reason: e.reason, agent }))
    if (!agent && e.reason === 'answer' && accepted.has('answer') && reporter !== undefined) {
      const text = done.text.trim()
      if (text !== '') reporter.push({ answer: clipAnswer(text) })
    }
    return done
  })

  on('session.measure', ($, e, next) => {
    if (reporter !== undefined && accepted.has('context')) {
      const fields: Record<string, string> = {}
      if (e.changed.includes('context') && e.context.percent !== undefined) {
        fields.context = String(Math.max(0, Math.min(100, Math.round(e.context.percent))))
      }
      if (e.changed.includes('cost') && e.cost !== undefined) {
        fields.cost = e.cost.usd.toFixed(4)
      }
      if (Object.keys(fields).length > 0) reporter.push(fields)
    }
    return next(e)
  })

  on('session.end', async ($, e, next) => {
    say(transition(status, { kind: 'session.end', reason: e.reason }))
    // The process may be leaving: let the last report land first.
    await reporter?.idle()
    return next(e)
  })
}
