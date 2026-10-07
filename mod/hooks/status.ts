// The pure half of the atrium mod: what the engine's events mean for the
// pane's status, and how reports to the broker are coalesced. No `$` here,
// so every rule is tested by `claude plugin test` with nothing mocked.

/** This mod's version, sent in `hello`. */
export const MOD_VERSION = '0.2.0'

/**
 * The capabilities this version implements, offered in `hello`. The broker
 * keeps the ones it knows (`modstate::CAPS`) and answers with `accepted`.
 */
export const CAPS = ['status', 'answer', 'context', 'tools', 'guard', 'inbox', 'ask'] as const

/** The most of an answer sent to the broker; it caps again on its side. */
export const ANSWER_CAP = 16 * 1024

/** A status as `atrium ctl report status=<label>` spells it. */
export type Status =
  | 'working'
  | 'waiting-approval'
  | 'waiting-prompt'
  | 'idle'
  | 'errored'
  | 'ended'

/** The engine events that move the status, reduced to what matters. */
export type Event =
  | { kind: 'start'; interactive: boolean }
  | { kind: 'turn.start' }
  | { kind: 'tool.check'; decision: 'allow' | 'ask' | 'deny'; tool: string }
  | { kind: 'tool.call'; tool: string }
  | {
      kind: 'turn.complete'
      reason: 'answer' | 'aborted' | 'refusal' | 'error'
      /** True for a subagent's turn, which says nothing about this pane. */
      agent: boolean
    }
  | { kind: 'session.end'; reason: string }

/** What to report: a status and, where one helps, why. */
export type Verdict = { status: Status; reason?: string }

/**
 * The status after `e`, or `undefined` when nothing changed and nothing need
 * be sent: the broker holds the last report, so only edges are reported.
 */
export function transition(prev: Status | undefined, e: Event): Verdict | undefined {
  const next = candidate(prev, e)
  if (next === undefined) return undefined
  if (next.status === prev) return undefined
  return next
}

function candidate(prev: Status | undefined, e: Event): Verdict | undefined {
  switch (e.kind) {
    case 'start':
      return { status: e.interactive ? 'waiting-prompt' : 'working' }
    case 'turn.start':
      return { status: 'working' }
    case 'tool.check':
      // An ask is the permission dialog: the pane is waiting on a human.
      // Allow and deny settle without one, and the next tool.call or
      // turn.complete says what follows.
      return e.decision === 'ask' ? { status: 'waiting-approval', reason: e.tool } : undefined
    case 'tool.call':
      // A tool runs only inside a turn, and only once any ask was answered.
      return { status: 'working' }
    case 'turn.complete':
      if (e.agent) return undefined
      if (e.reason === 'error' || e.reason === 'refusal') {
        return { status: 'errored', reason: e.reason }
      }
      // An answer, or an interruption: either way the prompt is free.
      return { status: 'waiting-prompt' }
    case 'session.end':
      return { status: 'ended', reason: e.reason }
  }
  // `prev` is only here so a future rule can read it.
  void prev
  return undefined
}

/** The first `ANSWER_CAP` characters of an answer. */
export function clipAnswer(text: string): string {
  return Array.from(text).slice(0, ANSWER_CAP).join('')
}

/**
 * The fields of one `report`, as `k=v` tokens on the command line. A list
 * field (`touched`) is one token per element, so a path survives whole.
 */
export type Fields = Record<string, string | string[]>

/** `{ status: 'working', touched: ['/a', '/b'] }` as `['status=working', 'touched=/a', 'touched=/b']`. */
export function toArgs(fields: Fields): string[] {
  const out: string[] = []
  for (const [k, v] of Object.entries(fields)) {
    if (Array.isArray(v)) {
      for (const x of v) out.push(`${k}=${x}`)
    } else {
      out.push(`${k}=${v}`)
    }
  }
  return out
}

/** `later` over `earlier`: a later value wins, and two lists join without repeats. */
export function mergeFields(earlier: Fields | undefined, later: Fields): Fields {
  const out: Fields = { ...(earlier ?? {}) }
  for (const [k, v] of Object.entries(later)) {
    const have = out[k]
    out[k] = Array.isArray(v) && Array.isArray(have) ? [...have, ...v.filter(x => !have.includes(x))] : v
  }
  return out
}

/**
 * Reports to the broker, one in flight at a time. Fields pushed while one is
 * in flight merge into the next (a later value wins), so a burst of tool
 * calls costs one request and the newest status is what lands. A send that
 * fails is dropped: the broker's stale window turns a dead channel into
 * "back to the inference", and nothing here retries forever.
 */
export class Reporter {
  private pending: Fields | undefined
  private inflight: Promise<void> | undefined
  private readonly send: (fields: Fields) => Promise<void>

  constructor(send: (fields: Fields) => Promise<void>) {
    this.send = send
  }

  /** Queue `fields`; starts a send at once when none is in flight. */
  push(fields: Fields): void {
    this.pending = mergeFields(this.pending, fields)
    if (this.inflight === undefined) this.drain()
  }

  /** Resolves once nothing is pending and nothing is in flight. */
  async idle(): Promise<void> {
    while (this.inflight !== undefined) await this.inflight
  }

  private drain(): void {
    const fields = this.pending
    this.pending = undefined
    if (fields === undefined) return
    this.inflight = this.send(fields)
      .catch(() => undefined)
      .then(() => {
        this.inflight = undefined
        this.drain()
      })
  }
}
