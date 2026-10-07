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

import { atom, read, update } from 'claude-code'
import type { EngineInterface, Register, RenderElement } from 'claude-code'
import { fit, parseView } from './view'
import { WAIT_SLICES, WAIT_SLICE_S, reportedAnswer, slug, spawnPointer, spawnedPane, subagentPrompt } from './agents'
import { ASK_REPLY_CAP, askPrompt, askReplyText, captionDue, captionRequest, clipCaption, toolCaption } from './caption'
import { TOOLS, TOOL_PREFIX, argvFor, ctlTimeoutFor, isAtriumTool } from './control'
import { INBOX_POLL_MS, acceptedCaps, inboxItems, parseReply } from './ctl'
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
async function ctl($: EngineInterface, args: readonly string[], timeoutMs = 10_000): Promise<Reply> {
  const bin = (await $.env.get('ATRIUM_BIN')) || 'atrium'
  try {
    const ran = await $.process.run([bin, 'ctl', ...args], { timeoutMs })
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

/** The `/atrium` pane: its id, and the state its drawing reads. */
const PANE = 'atrium'
const view = atom({ plugin: 'atrium', key: 'view' } as const, null)
/** How often the pane refreshes while open, and the band while it is not. */
const VIEW_OPEN_MS = 3000
const VIEW_IDLE_MS = 30000

/**
 * What this session's mod holds. The module's variables start over on a hot
 * reload, and `session.start` fires again then, so a reload is a fresh hello.
 */
type State = {
  status: Status | undefined
  accepted: ReadonlySet<string>
  reporter: Reporter | undefined
  who: Who | undefined
  /** The inbox poll, while the session is interactive and the cap was accepted. */
  inbox: { cancel: () => void } | undefined
  inboxBusy: boolean
  viewTimer: { cancel: () => void } | undefined
  viewBusy: boolean
  /** The last refresh, to pace the idle (pane closed) cadence on the open timer. */
  viewAt: number
  /** When the last model-made caption was requested. */
  captionAt: number
}
const state: State = {
  status: undefined,
  accepted: new Set(),
  reporter: undefined,
  who: undefined,
  inbox: undefined,
  inboxBusy: false,
  viewTimer: undefined,
  viewBusy: false,
  viewAt: 0,
  captionAt: 0,
}

/** Report `fields` when the broker accepted the status cap. */
function report(fields: Record<string, string | string[]>): void {
  if (state.reporter === undefined || !state.accepted.has('status')) return
  state.reporter.push(fields)
}

/** Report the real path of a file this pane just edited (collision radar). */
async function touch($: EngineInterface, path: string | undefined): Promise<void> {
  if (path === undefined) return
  const real = await placed($, isAbsolute(path) ? path : join(await $.session.cwd(), path))
  if (real !== undefined) report({ touched: [real] })
}

/**
 * The guard, then the edit, then the touch report: one body for the three
 * editing tools. A refused edit touches nothing.
 */
async function editCall(
  $: EngineInterface,
  e: Record<string, unknown>,
  next: (e: Record<string, unknown>) => Promise<unknown>,
): Promise<unknown> {
  const path = pathOf(e)
  const reason = await guard($, path)
  if (reason !== undefined) return { deny: reason }
  const done = await next(e)
  if (done !== null && typeof done === 'object' && !('deny' in done)) void touch($, path)
  return done
}

/**
 * A caption from the model's own words: one small, cheap completion over the
 * last few hundred characters of a step's text, at most once in
 * `CAPTION_MIN_MS`, only when the fleet allows it (`captions`).
 */
async function captionFromText($: EngineInterface, text: string): Promise<void> {
  if (state.who?.captions === false || state.reporter === undefined || !state.accepted.has('status')) return
  const now = await $.clock.now()
  if (!captionDue(state.captionAt, now, Array.from(text).length)) return
  state.captionAt = now
  const { system, prompt } = captionRequest(text)
  try {
    const r = await $.model.complete({ model: 'haiku', system, prompt, maxTokens: 24, effort: 'low', timeoutMs: 10_000 })
    if (r.isAnswered && r.text.trim() !== '') report({ doing: clipCaption(r.text) })
  } catch (err) {
    $.ui.log(`atrium: caption not made: ${err instanceof Error ? err.message : String(err)}`, { to: 'debug' })
  }
}

/**
 * Answer a queued question from a fork of this session's own context: no
 * tools, the turn untouched, the transcript never leaving the pane. The reply
 * goes straight to the broker, outside the coalescing reporter, so two
 * questions in flight never merge.
 */
async function answerAsk($: EngineInterface, id: number, question: string): Promise<void> {
  let text: string
  try {
    text = askReplyText(await $.model.fork({ prompt: askPrompt(question) }))
  } catch (err) {
    text = `(no reply: ${err instanceof Error ? err.message : String(err)})`
  }
  const reply = await ctl($, ['report', `ask=${id}`, `reply=${Array.from(text).slice(0, ASK_REPLY_CAP).join('')}`])
  if (!reply.ok) $.ui.log(`atrium: ask #${id} reply refused: ${reply.err ?? 'no reply'}`, { to: 'debug' })
}

/** Fetch the board and the feed from the broker into the pane's state. */
async function refreshView($: EngineInterface): Promise<void> {
  if (state.viewBusy) return
  state.viewBusy = true
  try {
    const [board, feed] = await Promise.all([ctl($, ['board', 'list']), ctl($, ['bus', 'feed', '--since', '0'])])
    const now = await $.clock.now()
    state.viewAt = now
    await update($, view, () => parseView(board, feed, now))
  } finally {
    state.viewBusy = false
  }
}

/** Refresh on the timer: every tick while the pane is open, rarely while it is not. */
async function refreshViewOnTick($: EngineInterface): Promise<void> {
  const open = (await $.ui.panes()).some(p => p.id === PANE && p.isShown)
  const now = await $.clock.now()
  if (open || now - state.viewAt >= VIEW_IDLE_MS) await refreshView($)
}

/** Resolve a decision from the pane, then redraw. */
async function resolveDecision($: EngineInterface, seq: number): Promise<void> {
  const reply = await ctl($, ['bus', 'resolve', String(seq)])
  if (!reply.ok) $.ui.toast(`atrium: cannot resolve #${seq}: ${reply.err ?? 'no reply'}`)
  await refreshView($)
}

/**
 * Native delivery: take the pane's queued sends and wakes from the broker and
 * submit each through the engine's own prompt queue, which starts a turn only
 * when the session is idle and never folds one into a running turn. atrium
 * types nothing into a pane whose mod does this. One poll at a time.
 */
async function pollInbox($: EngineInterface): Promise<void> {
  if (state.inboxBusy) return
  state.inboxBusy = true
  try {
    const reply = await ctl($, ['inbox'])
    for (const item of inboxItems(reply)) {
      if (item.kind === 'ask') {
        void answerAsk($, item.id, item.text)
        continue
      }
      // Resolves when the turn starts, which may be a while: do not wait.
      void $.prompt.submit({ text: item.text, asUser: true }).catch(err => {
        $.ui.log(`atrium: delivery not submitted: ${err instanceof Error ? err.message : String(err)}`, { to: 'debug' })
      })
    }
  } finally {
    state.inboxBusy = false
  }
}

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

/**
 * A subagent as a pane: spawn a claude beside this one, send it the task,
 * wait for the answer its mod reports, close the pane unless kept, and hand
 * the answer back as the tool's result. Every step is one `atrium ctl` run.
 */
async function runSubagent(
  $: EngineInterface,
  input: Record<string, unknown>,
  toolUseId: string | undefined,
): Promise<{ result: string } | { deny: string }> {
  const description = typeof input.description === 'string' ? input.description : ''
  const prompt = typeof input.prompt === 'string' ? input.prompt : ''
  if (description.trim() === '' || prompt.trim() === '') return { deny: 'atrium_subagent: description and prompt are required' }
  const who = state.who
  if (who === undefined) return { deny: 'atrium_subagent: this pane is not registered with atrium yet' }
  const role = slug(description)
  const task = subagentPrompt(who.pane, description, prompt)
  // An interactive claude that is sent the task: the spawn policy lets a
  // teammate choose no claude flag beyond the model and effort ones, so a
  // `claude -p <task>` child is not an option here.
  const spawned = await ctl($, ['spawn', '--here', '--role', role, '--', 'claude'])
  const pane = spawnedPane(spawned)
  if (pane === undefined) return { deny: `atrium_subagent: cannot spawn a pane: ${spawned.err ?? 'no reply'}` }
  if (toolUseId !== undefined) $.ui.notice(toolUseId, `running as atrium pane ${pane} (${role})`)
  const sent = await ctl($, ['send', String(pane), task])
  if (!sent.ok) return { deny: `atrium_subagent: pane ${pane} opened but the task was not delivered: ${sent.err ?? 'no reply'}` }
  let answer: { answer: string; seq: number } | undefined
  for (let i = 0; i < WAIT_SLICES && answer === undefined; i += 1) {
    const reply = await ctl($, ['wait', String(pane), '--for', 'answer', '--timeout', String(WAIT_SLICE_S)], (WAIT_SLICE_S + 30) * 1000)
    answer = reportedAnswer(reply)
    if (answer === undefined && reply.ok !== true && !String(reply.err ?? '').startsWith('timeout')) {
      return { deny: `atrium_subagent: pane ${pane} (${role}) gave no answer: ${reply.err ?? 'no reply'}` }
    }
  }
  if (answer === undefined) {
    return { deny: `atrium_subagent: pane ${pane} (${role}) is still working after ${(WAIT_SLICES * WAIT_SLICE_S) / 60} minutes; read it with atrium_status or atrium_answer later, or kill it` }
  }
  const keep = input.keep === true || who.subagentsKeep
  if (!keep) await ctl($, ['kill', String(pane)])
  if (toolUseId !== undefined) $.ui.notice(toolUseId, `atrium pane ${pane} (${role}) answered${keep ? '' : ' and was closed'}`)
  return { result: answer.answer }
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
    if (e.isInteractive) {
      await $.command.register({
        name: 'atrium',
        description: 'Open the atrium pane: the board, the open decisions and the bus feed of this session.',
      })
      state.viewTimer?.cancel()
      state.viewTimer = $.clock.every(VIEW_OPEN_MS, () => {
        void refreshViewOnTick($)
      })
    }
    if (state.accepted.has('inbox') && e.isInteractive) {
      state.inbox?.cancel()
      state.inbox = $.clock.every(INBOX_POLL_MS, () => {
        void pollInbox($)
      })
    }
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

  on('command.run', { command: 'atrium' }, async $ => {
    await $.ui.open({ id: PANE, title: 'atrium', focus: true, closeOnEscape: true })
    await refreshView($)
    return { text: 'atrium pane opened.' }
  })

  // The pane: the open decisions first (each with a resolve button), then the
  // board, then the last feed lines. Drawn from state; the timer refreshes it.
  on('ui.render', { component: 'Pane', requestId: PANE }, async ($, e) => {
    const { Box, Text, Button } = $.ui.resolve(e)
    const v = await read($, view)
    const width = Math.max(20, e.props.bodyColumns - 2)
    const who = state.who
    const head = `atrium · pane ${who?.pane ?? '?'}${who?.role === undefined ? '' : ` · ${who.role}`}${who?.item === undefined ? '' : ` · ${who.item}`}`
    const rows: unknown[] = [
      h(Box, { flexDirection: 'row' }, [
        h(Text, { bold: true }, head),
        h(Text, { dimColor: true }, '  '),
        h(Button, { key: 'refresh', label: 'refresh', hotkey: 'r', onPress: () => void refreshView($) }),
        h(Text, { dimColor: true }, ' '),
        h(Button, { key: 'close', label: 'close', hotkey: 'c', role: 'dismiss', onPress: () => void $.ui.close({ id: PANE }) }),
      ]),
    ]
    if (v === null) {
      rows.push(h(Text, { dimColor: true }, 'fetching…'))
      return h(Box, { flexDirection: 'column' }, rows) as RenderElement
    }
    if (v.error !== undefined) rows.push(h(Text, { color: 'red' }, fit(v.error, width)))
    rows.push(h(Text, { dimColor: true }, `─ decisions (${v.decisions.length}) ─`))
    v.decisions.slice(0, 9).forEach((d, i) => {
      rows.push(
        h(Box, { flexDirection: 'row' }, [
          h(Text, { color: 'yellow', bold: true }, `#${d.seq} `),
          h(Text, {}, fit(`${d.from} on ${d.topic}${d.to === '' ? '' : ` → ${d.to}`}: ${d.msg}`, width - 20)),
          h(Text, {}, ' '),
          h(Button, { key: `resolve-${d.seq}`, label: 'resolve', hotkey: String(i + 1), onPress: () => void resolveDecision($, d.seq) }),
        ]),
      )
    })
    rows.push(h(Text, { dimColor: true }, `─ board (${v.board.length}) ─`))
    for (const b of v.board.slice(0, 30)) {
      rows.push(
        h(Box, { flexDirection: 'row' }, [
          h(Text, { bold: true }, fit(b.key, 16).padEnd(17)),
          h(Text, {}, fit(b.fields, width - 17)),
        ]),
      )
    }
    rows.push(h(Text, { dimColor: true }, `─ feed (last ${v.feed.length}) ─`))
    for (const f of v.feed) {
      rows.push(h(Text, { dimColor: f.kind !== 'decision_needed' }, fit(`#${f.seq} ${f.kind === 'decision_needed' ? '!' : '·'} ${f.from} on ${f.topic}: ${f.line}`, width)))
    }
    return h(Box, { flexDirection: 'column' }, rows) as RenderElement
  })

  // The band above the prompt: how many decisions are open, and the pane to
  // open. Quiet when there are none, or a survey is up.
  on('ui.render', { component: 'AbovePrompt' }, async ($, e, next) => {
    const v = await read($, view)
    if (v === null || v.decisions.length === 0 || e.props.hasSurvey) return next(e)
    const { Box, Text, Button } = $.ui.resolve(e)
    const n = v.decisions.length
    return h(Box, { flexDirection: 'row' }, [
      h(Text, { color: 'yellow' }, `◆ ${n} decision${n === 1 ? '' : 's'} open on the atrium bus `),
      h(Button, { key: 'open-atrium', label: '/atrium', onPress: () => void $.ui.open({ id: PANE, title: 'atrium', focus: true, closeOnEscape: true }) }),
    ]) as RenderElement
  })

  // The model's own subagents: under `panes` (the default) the Agent tool is
  // refused with a pointer to atrium_subagent and its agent types are hidden,
  // so a subagent is always a pane the human can see; `deny` refuses outright;
  // `native` leaves the engine alone.
  on('agent.spawn', ($, e, next) => {
    const mode = state.who?.subagents ?? 'native'
    const pointer = spawnPointer(mode)
    return pointer === undefined ? next(e) : { deny: pointer }
  })
  on('agent.offer', ($, e, next) => {
    const mode = state.who?.subagents ?? 'native'
    return mode === 'native' ? next(e) : { isOffered: false }
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
  // owner named, before the tool runs; an edit that ran reports its real
  // path, which is the collision radar's evidence.
  on('tool.call', { tool: 'Edit' }, ($, e, next) =>
    editCall($, e as Record<string, unknown>, x => next(x as typeof e)) as ReturnType<typeof next>,
  )
  on('tool.call', { tool: 'Write' }, ($, e, next) =>
    editCall($, e as Record<string, unknown>, x => next(x as typeof e)) as ReturnType<typeof next>,
  )
  on('tool.call', { tool: 'NotebookEdit' }, ($, e, next) =>
    editCall($, e as Record<string, unknown>, x => next(x as typeof e)) as ReturnType<typeof next>,
  )

  // The caption from the model's own words, off each step's streamed text.
  // The chunks pass through untouched; the step's result stands.
  on('turn.step', async function* ($, e, next) {
    if (e.agentId !== undefined) return yield* next(e)
    let text = ''
    for await (const c of next(e)) {
      if (c.kind === 'text') text += c.text
      yield c
    }
    void captionFromText($, text)
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
    const { tool: _tool, tool_use_id: toolUseId, consent: _consent, ...input } = e as Record<string, unknown>
    // The free caption: what this call says the pane is doing.
    const caption = toolCaption(String(e.tool), input)
    if (caption !== undefined) report({ doing: caption })
    if (!isAtriumTool(String(e.tool))) return next(e)
    // The typed control plane: each atrium_* call is one `atrium ctl` run
    // with the pane's own token, and the reply is what the model reads.
    const name = String(e.tool).slice(TOOL_PREFIX.length)
    if (name === 'subagent') return runSubagent($, input, typeof toolUseId === 'string' ? toolUseId : undefined)
    const argv = argvFor(name, input)
    if (argv instanceof Error) return { deny: `atrium_${name}: ${argv.message}` }
    const reply = await ctl($, argv, ctlTimeoutFor(argv))
    return { result: JSON.stringify(reply) }
  })

  on('turn.complete', async ($, e, next) => {
    const done = await next(e)
    const agent = e.agentId !== undefined
    say(transition(state.status, { kind: 'turn.complete', reason: e.reason, agent }))
    // The turn is over: the caption no longer says what the pane is doing.
    if (!agent) report({ doing: '' })
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
    state.inbox?.cancel()
    state.inbox = undefined
    state.viewTimer?.cancel()
    state.viewTimer = undefined
    // The process may be leaving: let the last report land first.
    await state.reporter?.idle()
    return next(e)
  })
}
