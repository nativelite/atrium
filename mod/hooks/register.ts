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
import { TOOLS, TOOL_PREFIX, argvFor, isAtriumTool } from './control'
import { acceptedCaps, parseReply } from './ctl'
import type { Reply } from './ctl'
import { guardVerdict, isAbsolute, join, pathOf, splitParent } from './guard'
import { ROLE_SECTION_ID, parseWho, roleSection } from './role'
import type { Who } from './role'
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

/** The real path a file would have: its own when it exists, else its folder's plus its name. */
async function placed($: EngineInterface, path: string): Promise<string | undefined> {
  const own = await $.fs.stat(path, { resolve: true }).catch(() => undefined)
  if (own?.realPath !== undefined) return own.realPath
  const { folder, name } = splitParent(path)
  const dir = await $.fs.stat(folder, { resolve: true }).catch(() => undefined)
  return dir?.realPath === undefined ? undefined : join(dir.realPath, name)
}

/**
 * What this session's mod holds. The module's variables start over on a hot
 * reload, and `session.start` fires again then, so a reload is a fresh hello.
 */
type State = {
  status: Status | undefined
  accepted: ReadonlySet<string>
  reporter: Reporter | undefined
  who: Who | undefined
}
const state: State = { status: undefined, accepted: new Set(), reporter: undefined, who: undefined }

/** Re-read what the broker says this pane is (its item and files can change). */
async function refreshWho($: EngineInterface): Promise<void> {
  const reply = await ctl($, ['whoami'])
  const parsed = parseWho(reply)
  if (parsed !== undefined) state.who = parsed
}

/** Refuse an edit of `path` outside the files the item owns; undefined lets it through. */
async function guard($: EngineInterface, path: string | undefined): Promise<string | undefined> {
  const who = state.who
  if (!state.accepted.has('guard') || who === undefined || who.files.length === 0 || who.mode === 'plan') return undefined
  if (path === undefined) return undefined
  const base = who.cwd ?? (await $.session.cwd())
  const owned = (await Promise.all(who.files.map(f => placed($, join(base, f))))).filter(
    (p): p is string => p !== undefined,
  )
  const real = await placed($, isAbsolute(path) ? path : join(await $.session.cwd(), path))
  const reason = guardVerdict(real, owned, who.item, who.role)
  if (reason !== undefined) $.ui.toast(reason)
  return reason
}

export const register: Register = on => {

  const say = (v: Verdict | undefined): void => {
    if (v === undefined || state.reporter === undefined || !state.accepted.has('status')) return
    state.status = v.status
    state.reporter.push(v.reason === undefined ? { status: v.status } : { status: v.status, reason: v.reason })
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
    state.accepted = acceptedCaps(hello)
    state.reporter = new Reporter(async fields => {
      const reply = await ctl($, ['report', ...toArgs(fields)])
      if (!reply.ok) $.ui.log(`atrium: report refused: ${reply.err ?? 'no reply'}`, { to: 'debug' })
    })
    await refreshWho($)
    if (state.accepted.has('tools')) {
      for (const t of TOOLS) {
        await $.tool.register(t).catch(err => {
          $.ui.log(`atrium: cannot register ${t.name}: ${err instanceof Error ? err.message : String(err)}`, { to: 'debug' })
        })
      }
    }
    say(transition(state.status, { kind: 'start', interactive: e.isInteractive }))
    return started
  })

  // The system prompt: who this pane is, from the broker, as one session
  // section. A `bare` prompt is left alone.
  on('prompt.compose', async ($, e, next) => {
    const composed = await next(e)
    if (state.who === undefined || e.traits.includes('bare')) return composed
    const section = { id: ROLE_SECTION_ID, text: roleSection(state.who), scope: 'session' as const }
    return { sections: [...composed.sections.filter(s => s.id !== ROLE_SECTION_ID), section] }
  })

  // The file guard: an edit outside the item's files is refused, with the
  // owner named, before the tool runs.
  on('tool.call', { tool: 'Edit' }, async ($, e, next) => {
    const reason = await guard($, pathOf(e as Record<string, unknown>))
    return reason === undefined ? next(e) : { deny: reason }
  })
  on('tool.call', { tool: 'Write' }, async ($, e, next) => {
    const reason = await guard($, pathOf(e as Record<string, unknown>))
    return reason === undefined ? next(e) : { deny: reason }
  })
  on('tool.call', { tool: 'NotebookEdit' }, async ($, e, next) => {
    const reason = await guard($, pathOf(e as Record<string, unknown>))
    return reason === undefined ? next(e) : { deny: reason }
  })

  on('turn.start', ($, e, next) => {
    say(transition(state.status, { kind: 'turn.start' }))
    // The item and its files can change between turns (a lead re-briefs).
    if (state.who !== undefined) void refreshWho($)
    return next(e)
  })

  on('tool.check', async ($, e, next) => {
    const verdict = await next(e)
    // An atrium_* tool is the control plane this pane was spawned into: an
    // ask for it is answered here, never a dialog. A deny from a rule stands.
    if (verdict.decision === 'ask' && state.accepted.has('tools') && isAtriumTool(String(e.tool))) {
      return { decision: 'allow', reason: 'atrium: a control-plane tool of this session' }
    }
    say(transition(state.status, { kind: 'tool.check', decision: verdict.decision, tool: e.tool }))
    return verdict
  })

  on('tool.call', async ($, e, next) => {
    say(transition(state.status, { kind: 'tool.call', tool: String(e.tool) }))
    if (!isAtriumTool(String(e.tool))) return next(e)
    // The typed control plane: each atrium_* call is one `atrium ctl` run
    // with the pane's own token, and the reply is what the model reads.
    const name = String(e.tool).slice(TOOL_PREFIX.length)
    const { tool: _tool, tool_use_id: _id, consent: _consent, ...input } = e as Record<string, unknown>
    const argv = argvFor(name, input)
    if (argv instanceof Error) return { deny: `atrium_${name}: ${argv.message}` }
    const reply = await ctl($, argv)
    return { result: JSON.stringify(reply) }
  })

  on('turn.complete', async ($, e, next) => {
    const done = await next(e)
    const agent = e.agentId !== undefined
    say(transition(state.status, { kind: 'turn.complete', reason: e.reason, agent }))
    if (!agent && e.reason === 'answer' && state.accepted.has('answer') && state.reporter !== undefined) {
      const text = done.text.trim()
      if (text !== '') state.reporter.push({ answer: clipAnswer(text) })
    }
    return done
  })

  on('session.measure', ($, e, next) => {
    const reporter = state.reporter
    if (reporter !== undefined && state.accepted.has('context')) {
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
    say(transition(state.status, { kind: 'session.end', reason: e.reason }))
    // The process may be leaving: let the last report land first.
    await state.reporter?.idle()
    return next(e)
  })
}
