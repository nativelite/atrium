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
//
// It fails open (health.ts). Every hook below catches what its own code
// throws, counts it, and lets the engine go on as if the mod were not loaded;
// every `atrium ctl` exchange has a deadline; a method the engine lacks
// switches off the one feature that needs it. A tool call is refused only on
// a verdict atrium gave (the files an item owns, the subagent policy), never
// because atrium could not be asked.

import { atom, read, update } from 'claude-code'
import type { EngineInterface, Register, RenderElement } from 'claude-code'
import { fit, parseView } from './view'
import { WAIT_SLICES, WAIT_SLICE_S, reportedAnswer, slug, spawnPointer, spawnedPane, subagentPrompt } from './agents'
import { ASK_REPLY_CAP, askPrompt, askReplyText, captionDue, captionRequest, clipCaption, toolCaption } from './caption'
import { TOOLS, TOOL_PREFIX, argvFor, ctlTimeoutFor, isAtriumTool } from './control'
import { INBOX_POLL_MS, INBOX_WAIT_MS, acceptedCaps, inboxItems, parseReply, waitStep } from './ctl'
import type { Reply } from './ctl'
import { guardVerdict, isAbsolute, join, lexical, pathOf, splitParent } from './guard'
import {
  DEADLINE,
  END_WAIT_MS,
  Failures,
  HELLO_RETRY_MS,
  INBOX_REFUSALS,
  SET_ASIDE,
  WATCHDOG_MS,
  classify,
  offeredCaps,
  scrub,
} from './health'
import type { FailClass, Feature } from './health'
import { ROLE_SECTION_ID, parseWho, roleSection } from './role'
import type { Who } from './role'
import { CAPS, MOD_VERSION, Reporter, clipAnswer, toArgs, transition } from './status'
import type { Fields, Status, Verdict } from './status'

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
  /**
   * False until `session.start` found atrium, and again once the mod switched
   * itself off (too many failures, or atrium set it aside): every hook then
   * passes straight through.
   */
  live: boolean
  status: Status | undefined
  accepted: ReadonlySet<string>
  reporter: Reporter | undefined
  who: Who | undefined
  /** The features switched off because the engine lacked what they call. */
  off: Set<Feature>
  /** The `atrium_*` tools the engine took, by short name. */
  tools: Set<string>
  /**
   * The inbox, while the session is interactive and the cap was accepted: the
   * held `inbox --wait` loop, or the timed poll it falls back to. `cancel`
   * ends either.
   */
  inbox: { cancel: () => void } | undefined
  inboxBusy: boolean
  /** Inbox polls in a row that atrium refused. */
  inboxRefused: number
  viewTimer: { cancel: () => void } | undefined
  viewBusy: boolean
  /** The last refresh, to pace the idle (pane closed) cadence on the open timer. */
  viewAt: number
  /** When the last model-made caption was requested. */
  captionAt: number
  /** The one retry of a hello that could not be asked. */
  helloRetry: { cancel: () => void } | undefined
  /** Set as the mod switches itself off with something left to tell: one last report may go. */
  farewell: boolean
}
const state: State = {
  live: false,
  status: undefined,
  accepted: new Set(),
  reporter: undefined,
  who: undefined,
  off: new Set(),
  tools: new Set(),
  inbox: undefined,
  inboxBusy: false,
  inboxRefused: 0,
  viewTimer: undefined,
  viewBusy: false,
  viewAt: 0,
  captionAt: 0,
  helloRetry: undefined,
  farewell: false,
}

/** This session's failures (health.ts): counted, reported, and what switches the mod off. */
const failures = new Failures()

/** A line for the debug log; never throws, whatever the engine's `ui` is. */
function debug($: EngineInterface, text: string): void {
  try {
    $.ui.log(text, { to: 'debug' })
  } catch {
    // No log to write to: nothing to do about it.
  }
}

/** Stop a timer; never throws. */
function stop(timer: { cancel: () => void } | undefined): void {
  try {
    timer?.cancel()
  } catch {
    // A timer that cannot be cancelled runs out by itself.
  }
}

/**
 * Switch the mod off for the rest of the session: the loops stop, and every
 * hook passes straight through. What was counted is sent first, once.
 */
function goInert($: EngineInterface, why: string, tell: boolean): void {
  if (!state.live) return
  state.live = false
  stop(state.inbox)
  state.inbox = undefined
  stop(state.viewTimer)
  state.viewTimer = undefined
  stop(state.helloRetry)
  state.helloRetry = undefined
  debug($, `atrium: the mod switched itself off for this session: ${why}`)
  state.accepted = new Set()
  if (tell && state.reporter !== undefined) {
    state.farewell = true
    state.reporter.push({})
  }
}

/**
 * Count one failure of `hook`, tell atrium on the next report, and switch the
 * mod off when it has failed too often. Never throws: the counting is the one
 * thing a broken mod must still be able to do.
 */
function fail($: EngineInterface, hook: string, err: unknown, cls?: FailClass, tell = true): void {
  try {
    failures.note(hook, cls ?? classify(err), err)
    debug($, `atrium: ${hook} failed: ${typeof err === 'string' ? err : scrub(err)}`)
    if (failures.spent()) goInert($, 'it failed too often', tell)
    else if (tell) state.reporter?.push({})
  } catch {
    // Nothing more can be done from here.
  }
}

/**
 * A feature's own call failed. When the failure says the engine lacks the
 * method (`api`), the feature is switched off for the session and said so,
 * once; anything else is one more counted failure.
 */
function lost($: EngineInterface, feature: Feature, err: unknown): void {
  if (classify(err) !== 'api') {
    fail($, feature, err)
    return
  }
  if (state.off.has(feature)) return
  state.off.add(feature)
  debug($, `atrium: ${feature} is off for this session: the engine lacks what it calls`)
  if (feature === 'inbox') {
    stop(state.inbox)
    state.inbox = undefined
  }
  if (feature === 'pane') {
    stop(state.viewTimer)
    state.viewTimer = undefined
  }
  fail($, `feature.${feature}`, err, 'api')
}

/** Run `body`, which reports or draws and decides nothing; a throw is counted and dropped. */
function quiet($: EngineInterface, hook: string, body: () => void): void {
  try {
    body()
  } catch (err) {
    fail($, hook, err)
  }
}

/** `work`, with what it throws or rejects counted under `hook` and dropped. */
async function settle($: EngineInterface, hook: string, work: () => Promise<unknown>): Promise<void> {
  try {
    await work()
  } catch (err) {
    fail($, hook, err)
  }
}

/** What `within` resolves when the wait ran out. */
const LATE = Symbol('late')

/**
 * `work`, or `LATE` once `ms` have passed: the mod's own watchdog over a call
 * the engine should have ended by itself. Without `$.clock.after` there is no
 * watchdog and the engine's own timeout is the only one.
 */
function within<T>($: EngineInterface, work: Promise<T>, ms: number): Promise<T | typeof LATE> {
  return new Promise<T | typeof LATE>((resolve, reject) => {
    let settled = false
    let timer: { cancel: () => void } | undefined
    try {
      timer = $.clock.after(ms, () => {
        if (settled) return
        settled = true
        resolve(LATE)
      })
    } catch {
      timer = undefined
    }
    work.then(
      v => {
        if (settled) return
        settled = true
        stop(timer)
        resolve(v)
      },
      e => {
        if (settled) return
        settled = true
        stop(timer)
        reject(e)
      },
    )
  })
}

/** Is this engine hosted in an atrium pane with the control plane bound? */
async function inAtrium($: EngineInterface): Promise<boolean> {
  const address = await $.env.get('ATRIUM_CTL')
  const token = await $.env.get('ATRIUM_TOKEN')
  return address !== undefined && address !== '' && token !== undefined && token !== ''
}

/**
 * Run one `atrium ctl` verb on the host, with the pane's own token from the
 * environment atrium injected at spawn, and read its one-line reply. Never
 * throws and never outlasts `timeoutMs` by more than the watchdog: a call
 * that could not be made, or got no reply in time, is a reply with a `fault`,
 * counted under `site`. A refusal atrium gave has none.
 */
async function ctl($: EngineInterface, site: string, args: readonly string[], timeoutMs: number = DEADLINE.tool): Promise<Reply> {
  let reply: Reply
  try {
    const bin = (await $.env.get('ATRIUM_BIN')) || 'atrium'
    const ran = await within($, $.process.run([bin, 'ctl', ...args], { timeoutMs }), timeoutMs + WATCHDOG_MS)
    reply = ran === LATE ? { ok: false, err: `no reply within ${timeoutMs} ms`, fault: 'timeout' } : parseReply(ran)
  } catch (e) {
    const cls = classify(e)
    reply = {
      ok: false,
      err: e instanceof Error ? e.message : String(e),
      fault: cls === 'timeout' ? 'timeout' : 'ctl',
    }
    if (cls === 'api') {
      fail($, site, e, 'api', site !== 'report')
      return reply
    }
  }
  if (reply.fault === undefined) {
    failures.answered()
    // atrium set this pane's mod aside (its health window, its own reports):
    // the pane runs as one without a mod, and the mod stops asking.
    if (reply.ok !== true && String(reply.err ?? '').startsWith(SET_ASIDE)) goInert($, 'atrium set it aside', false)
  } else {
    // A report that could not be made is not retried by another report.
    fail($, site, reply.fault === 'timeout' ? 'no reply in time' : reply.fault === 'shape' ? 'not a reply' : 'could not be asked', reply.fault, site !== 'report')
  }
  return reply
}

/** The real path a file would have: its own when it exists, else its folder's plus its name. */
async function placed($: EngineInterface, path: string): Promise<string | undefined> {
  if (state.off.has('paths')) return undefined
  try {
    const own = await $.fs.stat(path, { resolve: true }).catch(() => undefined)
    if (own?.realPath !== undefined) return own.realPath
    const { folder, name } = splitParent(path)
    const dir = await $.fs.stat(folder, { resolve: true }).catch(() => undefined)
    return dir?.realPath === undefined ? undefined : join(dir.realPath, name)
  } catch (err) {
    // No `stat` to ask: the caller compares the path by its text.
    lost($, 'paths', err)
    return undefined
  }
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
 * editing tools. A refused edit touches nothing. The guard refuses on what
 * atrium said the item owns; a guard that could not judge (it threw) lets the
 * edit through and is counted.
 */
async function editCall(
  $: EngineInterface,
  e: Record<string, unknown>,
  next: (e: Record<string, unknown>) => Promise<unknown>,
): Promise<unknown> {
  if (!state.live) return next(e)
  let path: string | undefined
  let reason: string | undefined
  try {
    path = pathOf(e)
    reason = await guard($, path)
  } catch (err) {
    lost($, 'guard', err)
    reason = undefined
  }
  if (reason !== undefined) return { deny: reason }
  const done = await next(e)
  try {
    if (done !== null && typeof done === 'object' && !('deny' in done)) void settle($, 'touch', () => touch($, path))
  } catch (err) {
    fail($, 'touch', err)
  }
  return done
}

/**
 * A caption from the model's own words: one small, cheap completion over the
 * last few hundred characters of a step's text, at most once in
 * `CAPTION_MIN_MS`, only when the fleet asked for it (`captions: true`).
 */
async function captionFromText($: EngineInterface, text: string): Promise<void> {
  if (state.who?.captions !== true || state.reporter === undefined || !state.accepted.has('status')) return
  if (state.off.has('captions')) return
  try {
    const now = await $.clock.now()
    if (!captionDue(state.captionAt, now, Array.from(text).length)) return
    state.captionAt = now
    const { system, prompt } = captionRequest(text)
    const r = await $.model.complete({ model: 'haiku', system, prompt, maxTokens: 24, effort: 'low', timeoutMs: 10_000 })
    if (r.isAnswered && r.text.trim() !== '') report({ doing: clipCaption(r.text) })
  } catch (err) {
    lost($, 'captions', err)
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
    lost($, 'ask', err)
    text = `(no reply: ${scrub(err)})`
  }
  const reply = await ctl($, 'ask', ['report', `ask=${id}`, `reply=${Array.from(text).slice(0, ASK_REPLY_CAP).join('')}`], DEADLINE.report)
  if (!reply.ok) debug($, `atrium: ask #${id} reply refused: ${reply.err ?? 'no reply'}`)
}

/** Fetch the board and the feed from the broker into the pane's state. */
async function refreshView($: EngineInterface): Promise<void> {
  if (state.viewBusy || !state.live) return
  state.viewBusy = true
  try {
    const [board, feed] = await Promise.all([
      ctl($, 'view', ['board', 'list'], DEADLINE.view),
      ctl($, 'view', ['bus', 'feed', '--since', '0'], DEADLINE.view),
    ])
    const now = await $.clock.now()
    state.viewAt = now
    await update($, view, () => parseView(board, feed, now))
  } finally {
    state.viewBusy = false
  }
}

/** Refresh on the timer: every tick while the pane is open, rarely while it is not. */
async function refreshViewOnTick($: EngineInterface): Promise<void> {
  if (!state.live) return
  const open = (await $.ui.panes()).some(p => p.id === PANE && p.isShown)
  const now = await $.clock.now()
  if (open || now - state.viewAt >= VIEW_IDLE_MS) await refreshView($)
}

/** Resolve a decision from the pane, then redraw. */
async function resolveDecision($: EngineInterface, seq: number): Promise<void> {
  const reply = await ctl($, 'view', ['bus', 'resolve', String(seq)], DEADLINE.view)
  if (!reply.ok) $.ui.toast(`atrium: cannot resolve #${seq}: ${reply.err ?? 'no reply'}`)
  await refreshView($)
}

/**
 * Native delivery: take the pane's queued sends and wakes from the broker and
 * submit each through the engine's own prompt queue, which starts a turn only
 * when the session is idle and never folds one into a running turn. atrium
 * types nothing into a pane whose mod does this. One poll at a time. A run of
 * polls atrium refuses ends the polling: atrium types into the pane instead.
 */
async function pollInbox($: EngineInterface): Promise<void> {
  if (state.inboxBusy || !state.live) return
  state.inboxBusy = true
  try {
    const reply = await ctl($, 'inbox', ['inbox'], DEADLINE.poll)
    if (reply.ok !== true && reply.fault === undefined) {
      state.inboxRefused += 1
      if (state.inboxRefused >= INBOX_REFUSALS) {
        debug($, `atrium: inbox refused ${state.inboxRefused} times (${reply.err ?? 'no reason'}); polling ends`)
        stop(state.inbox)
        state.inbox = undefined
      }
      return
    }
    if (reply.ok === true) state.inboxRefused = 0
    deliver($, reply)
  } finally {
    state.inboxBusy = false
  }
}

/** Submit each item of an `inbox` reply: prompts through the engine's queue, asks to a fork. */
function deliver($: EngineInterface, reply: Reply): void {
  if (reply.ok === true && !Array.isArray(reply.items)) fail($, 'inbox', 'an inbox reply with no items', 'shape')
  for (const item of inboxItems(reply)) {
    if (item.kind === 'ask') {
      void settle($, 'ask', () => answerAsk($, item.id, item.text))
      continue
    }
    // Resolves when the turn starts, which may be a while: do not wait.
    try {
      void $.prompt.submit({ text: item.text, asUser: true }).catch(err => {
        lost($, 'inbox', err)
      })
    } catch (err) {
      lost($, 'inbox', err)
    }
  }
}

/**
 * Native delivery without a poll: one `atrium ctl inbox --wait` at a time,
 * which the broker holds until a delivery lands (or its hold runs out, then it
 * answers with no items), re-issued the moment it returns. An atrium that does
 * not hold the request answers at once; `waitStep` sees that and this falls
 * back to the timed poll, so an older broker or client is polled, not spun on.
 * Returns the handle that ends the loop.
 */
function waitInbox($: EngineInterface): { cancel: () => void } {
  let stopped = false
  let poll: { cancel: () => void } | undefined
  const loop = async (): Promise<void> => {
    let strikes = 0
    while (!stopped && state.live && !state.off.has('inbox')) {
      const started = Date.now()
      const reply = await ctl($, 'inbox', ['inbox', '--wait'], INBOX_WAIT_MS)
      if (stopped || !state.live) return
      deliver($, reply)
      const next = waitStep(strikes, reply, Date.now() - started)
      strikes = next.strikes
      if (next.step === 'poll') {
        debug($, `atrium: inbox --wait not held (${reply.ok ? 'answered at once' : reply.err ?? 'no reply'}); polling every ${INBOX_POLL_MS} ms`)
        try {
          poll = $.clock.every(INBOX_POLL_MS, () => {
            void settle($, 'inbox', () => pollInbox($))
          })
        } catch (err) {
          lost($, 'inbox', err)
        }
        return
      }
    }
  }
  void settle($, 'inbox', loop)
  return {
    cancel: () => {
      stopped = true
      stop(poll)
    },
  }
}

/** Re-read what the broker says this pane is (its item and files can change). */
async function refreshWho($: EngineInterface, timeoutMs: number = DEADLINE.report): Promise<void> {
  const reply = await ctl($, 'whoami', ['whoami'], timeoutMs)
  const parsed = parseWho(reply)
  if (parsed !== undefined) state.who = parsed
  else if (reply.ok === true) fail($, 'whoami', 'a whoami reply that names no pane', 'shape')
}

/**
 * Refuse an edit of `path` outside the files the item owns; undefined lets it
 * through. A path the filesystem cannot place (its folder is not there yet,
 * or `stat` failed) is compared by its text, so atrium's list still decides.
 */
async function guard($: EngineInterface, path: string | undefined): Promise<string | undefined> {
  const who = state.who
  if (!state.accepted.has('guard') || who === undefined || who.files.length === 0 || who.mode === 'plan') return undefined
  if (path === undefined) return undefined
  const cwd = await $.session.cwd()
  const base = who.cwd ?? cwd
  const place = async (p: string): Promise<string> => (await placed($, p)) ?? lexical(p)
  const owned = await Promise.all(who.files.map(f => place(join(base, f))))
  const real = await place(isAbsolute(path) ? path : join(cwd, path))
  const reason = guardVerdict(real, owned, who.item, who.role)
  if (reason !== undefined) quiet($, 'guard', () => $.ui.toast(reason))
  return reason
}

/** A notice on a tool call's row; never throws. */
function notice($: EngineInterface, toolUseId: string | undefined, text: string): void {
  if (toolUseId === undefined) return
  try {
    $.ui.notice(toolUseId, text)
  } catch {
    // A row with no notice is still a row.
  }
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
  const spawned = await ctl($, 'subagent', ['spawn', '--here', '--role', role, '--', 'claude'])
  const pane = spawnedPane(spawned)
  if (pane === undefined) return { deny: `atrium_subagent: cannot spawn a pane: ${spawned.err ?? 'no reply'}` }
  notice($, toolUseId, `running as atrium pane ${pane} (${role})`)
  const sent = await ctl($, 'subagent', ['send', String(pane), task])
  if (!sent.ok) {
    // The pane this call opened has no task and nothing will give it one:
    // close it, so a refused send does not leave an idle agent behind.
    const closed = await ctl($, 'subagent', ['kill', String(pane)])
    return { deny: `atrium_subagent: the task was not delivered to pane ${pane} (${sent.err ?? 'no reply'}); ${closed.ok ? 'the pane was closed' : `the pane could not be closed (${closed.err ?? 'no reply'}): kill it with atrium_kill`}` }
  }
  let answer: { answer: string; seq: number } | undefined
  for (let i = 0; i < WAIT_SLICES && answer === undefined; i += 1) {
    const reply = await ctl($, 'subagent', ['wait', String(pane), '--for', 'answer', '--timeout', String(WAIT_SLICE_S)], (WAIT_SLICE_S + 30) * 1000)
    answer = reportedAnswer(reply)
    if (answer === undefined && reply.ok !== true && !String(reply.err ?? '').startsWith('timeout')) {
      return { deny: `atrium_subagent: pane ${pane} (${role}) gave no answer: ${reply.err ?? 'no reply'}` }
    }
  }
  if (answer === undefined) {
    return { deny: `atrium_subagent: pane ${pane} (${role}) is still working after ${(WAIT_SLICES * WAIT_SLICE_S) / 60} minutes; read it with atrium_status or atrium_answer later, or kill it` }
  }
  const keep = input.keep === true || who.subagentsKeep
  if (!keep) await ctl($, 'subagent', ['kill', String(pane)])
  notice($, toolUseId, `atrium pane ${pane} (${role}) answered${keep ? '' : ' and was closed'}`)
  return { result: answer.answer }
}

/** Report a status edge when the broker accepted the status cap. */
function say(v: Verdict | undefined): void {
  if (v === undefined || state.reporter === undefined || !state.accepted.has('status')) return
  state.status = v.status
  state.reporter.push(v.reason === undefined ? { status: v.status } : { status: v.status, reason: v.reason })
}

/**
 * One report to the broker, with the failures it has not been told riding
 * along. A report that carries nothing is not sent.
 */
async function sendReport($: EngineInterface, fields: Fields): Promise<void> {
  // Off: nothing more is said, but for the one report that says why.
  if (!state.live) {
    if (!state.farewell) return
    state.farewell = false
  }
  const carried = failures.unsent()
  const all: Fields = carried ? { ...fields, ...failures.fields() } : fields
  if (Object.keys(all).length === 0) return
  const reply = await ctl($, 'report', ['report', ...toArgs(all)], DEADLINE.report)
  if (reply.ok) {
    if (carried) failures.sent()
  } else {
    debug($, `atrium: report refused: ${reply.err ?? 'no reply'}`)
  }
}

/**
 * What a `hello` atrium took switches on: the reporter, who this pane is, the
 * pane and its timer, the inbox, the tools, the first status. Each piece
 * stands alone: one that fails is counted and the rest go on.
 */
async function activate($: EngineInterface, hello: Reply, interactive: boolean): Promise<void> {
  if (!Array.isArray(hello.accepted)) fail($, 'hello', 'a hello reply with no accepted list', 'shape')
  state.accepted = acceptedCaps(hello)
  if (state.accepted.size === 0) {
    // atrium granted nothing (it set this pane aside, or knows none of the
    // caps): there is nothing for the mod to do here.
    goInert($, 'atrium accepted no capability', false)
    return
  }
  state.reporter = new Reporter(fields => sendReport($, fields))
  await settle($, 'whoami', () => refreshWho($, DEADLINE.whoami))
  if (interactive) {
    try {
      await $.command.register({
        name: 'atrium',
        description: 'Open the atrium pane: the board, the open decisions and the bus feed of this session.',
      })
      stop(state.viewTimer)
      state.viewTimer = $.clock.every(VIEW_OPEN_MS, () => {
        void refreshViewOnTick($).catch(err => lost($, 'pane', err))
      })
    } catch (err) {
      lost($, 'pane', err)
    }
  }
  if (state.accepted.has('inbox') && interactive) {
    quiet($, 'inbox', () => {
      stop(state.inbox)
      state.inboxRefused = 0
      state.inbox = waitInbox($)
    })
  }
  if (state.accepted.has('tools')) {
    for (const t of TOOLS) {
      if (state.off.has('tools') || !state.live) break
      try {
        await $.tool.register(t)
        state.tools.add(t.name)
      } catch (err) {
        lost($, 'tools', err)
      }
    }
  }
  say(transition(state.status, { kind: 'start', interactive }))
  // Whatever was counted on the way here (a feature switched off) is told now.
  if (failures.unsent()) state.reporter.push({})
}

/**
 * Find atrium and say hello. Outside atrium, or on an engine without the two
 * methods the mod speaks through, the mod stays off and says nothing.
 */
async function start($: EngineInterface, interactive: boolean): Promise<void> {
  state.live = false
  state.farewell = false
  state.accepted = new Set()
  state.tools = new Set()
  state.off = new Set()
  // Without `$.env.get` this throws, `session.start` counts it, and the mod
  // stays off: it has no way to learn where atrium is.
  if (!(await inAtrium($))) return
  state.live = true
  let engine: string | undefined
  try {
    engine = (await $.session.version()).version
  } catch {
    engine = undefined
  }
  // A session nobody types into has no prompt queue to submit to.
  const offered = offeredCaps(CAPS, interactive)
  const args = ['hello', `mod=${MOD_VERSION}`, ...(typeof engine === 'string' && engine !== '' ? [`engine=${engine}`] : []), `caps=${offered.join(',')}`]
  const hello = await ctl($, 'hello', args, DEADLINE.hello)
  if (hello.ok) {
    await activate($, hello, interactive)
    return
  }
  debug($, `atrium: hello refused: ${hello.err ?? 'no reply'}`)
  if (hello.fault === undefined) {
    // atrium answered and said no: that is its decision, and it stands.
    goInert($, 'atrium refused the hello', false)
    return
  }
  // atrium could not be asked. One more try, outside the start nobody should
  // wait on; after that the pane simply runs as one without a mod.
  stop(state.helloRetry)
  state.helloRetry = $.clock.after(HELLO_RETRY_MS, () => {
    void settle($, 'hello', async () => {
      if (!state.live) return
      const again = await ctl($, 'hello', args, DEADLINE.hello)
      if (again.ok) await activate($, again, interactive)
      else goInert($, 'atrium could not be asked', false)
    })
  })
}

export const register: Register = on => {
  on('session.start', async ($, e, next) => {
    const started = await next(e)
    await settle($, 'session.start', () => start($, e.isInteractive))
    return started
  })

  on('command.run', { command: 'atrium' }, async $ => {
    try {
      await $.ui.open({ id: PANE, title: 'atrium', focus: true, closeOnEscape: true })
      await refreshView($)
      return { text: 'atrium pane opened.' }
    } catch (err) {
      fail($, 'command.run', err)
      return { text: 'atrium: the pane could not be opened; `atrium ctl board list` and `atrium ctl bus feed` show the same.' }
    }
  })

  // The pane: the open decisions first (each with a resolve button), then the
  // board, then the last feed lines. Drawn from state; the timer refreshes it.
  // A drawing that fails is the engine's own node.
  on('ui.render', { component: 'Pane', requestId: PANE }, async ($, e, next) => {
    try {
      const { Box, Text, Button } = $.ui.resolve(e)
      const v = await read($, view)
      const width = Math.max(20, e.props.bodyColumns - 2)
      const who = state.who
      const head = `atrium · pane ${who?.pane ?? '?'}${who?.role === undefined ? '' : ` · ${who.role}`}${who?.item === undefined ? '' : ` · ${who.item}`}`
      const rows: unknown[] = [
        h(Box, { flexDirection: 'row' }, [
          h(Text, { bold: true }, head),
          h(Text, { dimColor: true }, '  '),
          h(Button, { key: 'refresh', label: 'refresh', hotkey: 'r', onPress: () => void settle($, 'view', () => refreshView($)) }),
          h(Text, { dimColor: true }, ' '),
          h(Button, { key: 'close', label: 'close', hotkey: 'c', role: 'dismiss', onPress: () => void settle($, 'view', () => $.ui.close({ id: PANE })) }),
        ]),
      ]
      if (v === null) {
        rows.push(h(Text, { dimColor: true }, state.live ? 'fetching…' : 'the atrium mod is off in this session'))
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
            h(Button, { key: `resolve-${d.seq}`, label: 'resolve', hotkey: String(i + 1), onPress: () => void settle($, 'view', () => resolveDecision($, d.seq)) }),
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
    } catch (err) {
      fail($, 'ui.render', err)
      return next(e)
    }
  })

  // The band above the prompt: how many decisions are open, and the pane to
  // open. Quiet when there are none, or a survey is up, or the mod is off.
  on('ui.render', { component: 'AbovePrompt' }, async ($, e, next) => {
    if (!state.live) return next(e)
    try {
      const v = await read($, view)
      if (v === null || v.decisions.length === 0 || e.props.hasSurvey) return next(e)
      const { Box, Text, Button } = $.ui.resolve(e)
      const n = v.decisions.length
      return h(Box, { flexDirection: 'row' }, [
        h(Text, { color: 'yellow' }, `◆ ${n} decision${n === 1 ? '' : 's'} open on the atrium bus `),
        h(Button, { key: 'open-atrium', label: '/atrium', onPress: () => void settle($, 'view', () => $.ui.open({ id: PANE, title: 'atrium', focus: true, closeOnEscape: true })) }),
      ]) as RenderElement
    } catch (err) {
      fail($, 'ui.render', err)
      return next(e)
    }
  })

  // The model's own subagents: under `panes` (the default) the Agent tool is
  // refused with a pointer to atrium_subagent and its agent types are hidden,
  // so a subagent is always a pane the human can see; `deny` refuses outright;
  // `native` leaves the engine alone. Both refusals are atrium's own policy,
  // as `whoami` gave it. With the mod off, or under `panes` without the tool
  // the pointer names (the engine did not take it), the engine is left alone:
  // a refusal that points nowhere would leave the pane with no subagents at all.
  const subagents = (): string => {
    if (!state.live) return 'native'
    const mode = state.who?.subagents ?? 'native'
    return mode === 'panes' && !state.tools.has('subagent') ? 'native' : mode
  }
  on('agent.spawn', ($, e, next) => {
    let pointer: string | undefined
    try {
      pointer = spawnPointer(subagents())
    } catch (err) {
      fail($, 'agent.spawn', err)
      pointer = undefined
    }
    return pointer === undefined ? next(e) : { deny: pointer }
  })
  on('agent.offer', ($, e, next) => {
    let native = true
    try {
      native = subagents() === 'native'
    } catch (err) {
      fail($, 'agent.offer', err)
      native = true
    }
    return native ? next(e) : { isOffered: false }
  })

  // The system prompt: who this pane is, from the broker, as one session
  // section. A `bare` prompt is left alone.
  on('prompt.compose', async ($, e, next) => {
    const composed = await next(e)
    if (!state.live) return composed
    try {
      if (state.who === undefined || e.traits.includes('bare')) return composed
      const section = { id: ROLE_SECTION_ID, text: roleSection(state.who), scope: 'session' as const }
      return { sections: [...composed.sections.filter(s => s.id !== ROLE_SECTION_ID), section] }
    } catch (err) {
      fail($, 'prompt.compose', err)
      return composed
    }
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
    if (e.agentId !== undefined || !state.live) return yield* next(e)
    let text = ''
    let reading = true
    for await (const c of next(e)) {
      if (reading) {
        try {
          if (c.kind === 'text') text += c.text
        } catch (err) {
          reading = false
          fail($, 'turn.step', err)
        }
      }
      yield c
    }
    if (reading) void settle($, 'caption', () => captionFromText($, text))
  })

  on('turn.start', ($, e, next) => {
    if (state.live) {
      quiet($, 'turn.start', () => {
        say(transition(state.status, { kind: 'turn.start' }))
        // The item and its files can change between turns (a lead re-briefs).
        if (state.who !== undefined) void settle($, 'whoami', () => refreshWho($))
      })
    }
    return next(e)
  })

  on('tool.check', async ($, e, next) => {
    const verdict = await next(e)
    if (!state.live) return verdict
    try {
      // An atrium_* tool is the control plane this pane was spawned into: an
      // ask for it is answered here, never a dialog. A deny from a rule stands.
      if (verdict.decision === 'ask' && state.accepted.has('tools') && isAtriumTool(String(e.tool))) {
        return { decision: 'allow', reason: 'atrium: a control-plane tool of this session' }
      }
      say(transition(state.status, { kind: 'tool.check', decision: verdict.decision, tool: e.tool }))
    } catch (err) {
      fail($, 'tool.check', err)
    }
    return verdict
  })

  on('tool.call', async ($, e, next) => {
    if (!state.live) return next(e)
    let input: Record<string, unknown> = {}
    let toolUseId: unknown
    quiet($, 'tool.call', () => {
      say(transition(state.status, { kind: 'tool.call', tool: String(e.tool) }))
      const { tool: _tool, tool_use_id: id, consent: _consent, ...rest } = e as Record<string, unknown>
      input = rest
      toolUseId = id
      // The free caption: what this call says the pane is doing.
      const caption = toolCaption(String(e.tool), input)
      if (caption !== undefined) report({ doing: caption })
    })
    let own = false
    try {
      own = isAtriumTool(String(e.tool)) && state.tools.has(String(e.tool).slice(TOOL_PREFIX.length))
    } catch (err) {
      fail($, 'tool.call', err)
    }
    // Every tool that is not this mod's own goes on, whatever happened above.
    if (!own) return next(e)
    // The typed control plane: each atrium_* call is one `atrium ctl` run
    // with the pane's own token, and the reply is what the model reads. The
    // tool exists only here, so its failure is its result.
    try {
      const name = String(e.tool).slice(TOOL_PREFIX.length)
      if (name === 'subagent') return await runSubagent($, input, typeof toolUseId === 'string' ? toolUseId : undefined)
      const argv = argvFor(name, input)
      if (argv instanceof Error) return { deny: `atrium_${name}: ${argv.message}` }
      const reply = await ctl($, 'tool', argv, ctlTimeoutFor(argv))
      return { result: JSON.stringify(reply) }
    } catch (err) {
      fail($, 'tool', err)
      return { result: JSON.stringify({ ok: false, err: 'the atrium mod failed running this tool; use `atrium ctl` from the shell instead' }) }
    }
  })

  on('turn.complete', async ($, e, next) => {
    const done = await next(e)
    if (!state.live) return done
    quiet($, 'turn.complete', () => {
      const agent = e.agentId !== undefined
      say(transition(state.status, { kind: 'turn.complete', reason: e.reason, agent }))
      // The turn is over: the caption no longer says what the pane is doing.
      if (!agent) report({ doing: '' })
      if (!agent && e.reason === 'answer' && state.accepted.has('answer') && state.reporter !== undefined) {
        const text = done.text.trim()
        if (text !== '') state.reporter.push({ answer: clipAnswer(text) })
      }
    })
    return done
  })

  on('session.measure', ($, e, next) => {
    if (state.live) {
      quiet($, 'session.measure', () => {
        const reporter = state.reporter
        if (reporter === undefined || !state.accepted.has('context')) return
        const fields: Record<string, string> = {}
        if (e.changed.includes('context') && e.context.percent !== undefined) {
          fields.context = String(Math.max(0, Math.min(100, Math.round(e.context.percent))))
        }
        if (e.changed.includes('cost') && e.cost !== undefined) {
          fields.cost = e.cost.usd.toFixed(4)
        }
        if (Object.keys(fields).length > 0) reporter.push(fields)
      })
    }
    return next(e)
  })

  on('session.end', async ($, e, next) => {
    if (state.live) {
      await settle($, 'session.end', async () => {
        say(transition(state.status, { kind: 'session.end', reason: e.reason }))
        stop(state.inbox)
        state.inbox = undefined
        stop(state.viewTimer)
        state.viewTimer = undefined
        stop(state.helloRetry)
        state.helloRetry = undefined
        // The process may be leaving: let the last report land first, for a
        // moment and no longer.
        const reporter = state.reporter
        if (reporter !== undefined) await within($, reporter.idle(), END_WAIT_MS)
      })
    }
    return next(e)
  })
}
