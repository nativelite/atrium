// The mod's own health: what it does when it fails. Pure (no `$`), so every
// rule is tested with nothing mocked.
//
// The rule of the house: a failure of the mod is never a failure of the pane.
// A hook that throws, a ctl call that hangs or answers nonsense, an engine
// that lacks a method: each is counted here, told to atrium on the next
// report, and Claude Code goes on exactly as if the mod were not loaded. Only
// a refusal atrium actually gave (a guard verdict from the files it named, a
// subagent policy it set) may stop a tool call; "could not ask" never does.

/**
 * How long one `atrium ctl` exchange may take, in milliseconds, by what waits
 * on it. Each is the child's `timeoutMs`; a watchdog of the mod's own fires
 * `WATCHDOG_MS` later, should the engine's timeout not.
 *
 * - `hello`, `whoami`: awaited inside `session.start`, which the engine awaits
 *   before the first prompt. With the watchdog behind each, the two can hold
 *   a start for 4 + 1 + 2 + 1 = 8 s at the very worst, against the hook's own
 *   10 s budget. A healthy reply takes tens of milliseconds: one process
 *   start and one line on a local socket.
 * - `report`, `poll`, `view`: background work nobody waits on; 5 s is past any
 *   healthy reply and short enough that a dead broker is noticed within a few
 *   calls.
 * - `tool`: an `atrium_*` tool the model called itself. The call is the tool,
 *   so it may take the 10 s it always had; its failure is the tool's result.
 */
export const DEADLINE = {
  hello: 4_000,
  whoami: 2_000,
  report: 5_000,
  poll: 5_000,
  view: 5_000,
  tool: 10_000,
} as const

/** How long after a call's own timeout the mod's watchdog gives up on it. */
export const WATCHDOG_MS = 1_000

/** How long `session.end` waits for the last report before letting go. */
export const END_WAIT_MS = 3_000

/**
 * How long the file guard waits on the engine for the session's directory or
 * a file's real path before it gives up and lets the edit through, counted.
 * An edit is a tool call a person is waiting on.
 */
export const GUARD_MS = 2_000

/**
 * How long the mod waits before each next hello, in seconds, when atrium
 * could not be asked: at the start, and again whenever it stops answering.
 * The last value repeats. Inside the thirty seconds atrium gives a pane to
 * say hello that is four tries when each fails at once (at 0, 3, 9 and 21 s)
 * and three when each hangs for its full five seconds (at 0, 8 and 19 s).
 * It keeps trying after that, more and more slowly: an atrium that was only
 * stalled finds its mods again without anyone doing anything.
 *
 * It is also the worst the mod can cost in processes while atrium is gone:
 * one `atrium ctl hello` per wait, so one every two minutes once the list
 * has run out, and nothing else (its loops are stopped while it waits).
 */
export const HELLO_BACKOFF_S: readonly number[] = [3, 6, 12, 24, 30, 60, 120]

/** The wait before hello number `attempt` (0 is the first retry), in milliseconds. */
export function helloBackoffMs(attempt: number): number {
  const i = Math.max(0, Math.min(Math.floor(attempt), HELLO_BACKOFF_S.length - 1))
  return (HELLO_BACKOFF_S[i] ?? 120) * 1000
}

/**
 * After this many errors thrown by its hooks within `FAIL_WINDOW_MS` the mod
 * switches itself off for the session; after this many ctl exchanges in a
 * row that got no reply it could read, it pauses (its loops stop, and it says
 * hello again on `HELLO_BACKOFF_S`). Either way every hook passes straight
 * through. Five: one failure is an accident, five is a broken mod or
 * a broker that is gone, and a mod that keeps failing costs every tool call a
 * caught exception for nothing.
 */
export const INERT_AFTER = 5

/** How many refused inbox polls in a row end the polling (atrium said no). */
export const INBOX_REFUSALS = 10

/**
 * The window the errors of the mod's own hooks are counted over, in
 * milliseconds: `INERT_AFTER` of them within ten minutes switch the mod off.
 * Five accidents spread over a long session are not a broken mod. atrium
 * counts the same classes over the same window (`FAIL_WINDOW_MS` in
 * `src/modstate.rs`).
 */
export const FAIL_WINDOW_MS = 600_000

/**
 * Is a refusal atrium's last word to this mod? Only when the reply says so
 * itself (`"final": true`: the mod was set aside, or its hello is not one
 * this session takes). Every other `ok: false` (a listener too busy to take
 * the request, a request too large, a verb it does not know, an ask that is
 * not this pane's) is an answer to that one request, and the mod goes on.
 */
export function isFinal(reply: { ok: boolean; final?: unknown }): boolean {
  return reply.ok !== true && reply.final === true
}

/**
 * What went wrong, in five words atrium knows:
 *
 * - `api`: the plugin interface is not what the mod was written for (a
 *   missing method, a call on something that is not a function). The one
 *   class that means "Claude Code changed".
 * - `timeout`: a ctl call outlasted its deadline.
 * - `ctl`: a ctl call could not be made at all (no binary, no endpoint).
 * - `shape`: a reply came back and was not what the verb answers.
 * - `hook`: anything else a hook threw.
 */
export type FailClass = 'api' | 'timeout' | 'ctl' | 'shape' | 'hook'

/** The classes, for a table and for the parser on the other side. */
export const FAIL_CLASSES: readonly FailClass[] = ['api', 'timeout', 'ctl', 'shape', 'hook']

/** The most of an error's message the mod sends. */
export const MESSAGE_CAP = 120

/** The most distinct hooks counted; the rest fall under `other`. */
export const HOOKS_CAP = 16

/**
 * Which class a thrown value is. A `TypeError` or a `ReferenceError` inside a
 * hook's own code (a null dereference on an event's data, a typo) is a bug in
 * the mod: `hook`, counted toward switching it off. `api` is kept for what
 * the engine itself says of a `$` call nothing answers or a plugin withheld;
 * a missing method found at the call is told apart by where it was thrown,
 * not by its text (`engineLacks`).
 */
export function classify(err: unknown): FailClass {
  const message = err instanceof Error ? err.message : String(err)
  if (/no implementation for|removed by plugin/i.test(message)) return 'api'
  if (/timed? ?out|deadline/i.test(message)) return 'timeout'
  return 'hook'
}

/**
 * Did a call on the engine (`$.noun.method(...)`) fail because the engine
 * does not have it? Asked only where the mod is making such a call: there, a
 * `TypeError` that the thing is not a function, or was read off `undefined`,
 * is the method or its noun missing; anywhere else the same text is a bug.
 */
export function engineLacks(err: unknown): boolean {
  if (classify(err) === 'api') return true
  if (!(err instanceof Error) || err.name !== 'TypeError') return false
  return /is not a function|Cannot read propert(y|ies) of undefined|undefined is not an object/i.test(err.message)
}

/**
 * An error as one short line that is safe to send: its name and its message
 * with everything that could be a user's data taken out. Quoted text (what a
 * JSON parse error quotes of its input, what a path error names), anything
 * with a path separator, and any long unbroken run (a token, a hash, a key)
 * each become `…`; what is left is printable ASCII, cut to `MESSAGE_CAP`. A
 * tool's input, a file's contents and the environment never ride in it.
 */
export function scrub(err: unknown): string {
  const name = err instanceof Error ? err.name.replace(/[^A-Za-z0-9_]/g, '').slice(0, 40) : 'Error'
  const raw = err instanceof Error ? err.message : typeof err === 'string' ? err : ''
  const text = raw
    .replace(/"[^"]*"|'[^']*'|`[^`]*`/g, '…')
    .replace(/\S*[\\/]\S*/g, '…')
    .replace(/[A-Za-z0-9+=_-]{24,}/g, '…')
    .replace(/[^\x20-\x7e…]/g, ' ')
    .replace(/\s+/g, ' ')
    .trim()
  const line = text === '' ? name : `${name}: ${text}`
  return Array.from(line).slice(0, MESSAGE_CAP).join('')
}

/**
 * A hook's name as a counter key: letters, digits, `.`, `_`, `-`; 40 at most.
 * Two families: a hook's own name (`tool.call`, `guard`, `feature.inbox`) for
 * what the mod's code threw, and `ctl.<site>` (`ctl.hello`, `ctl.report`) for
 * an exchange with atrium that failed. atrium sets a mod aside on the first
 * family alone: a slow or absent broker is not a broken mod.
 */
export function hookKey(hook: string): string {
  const key = hook.replace(/[^A-Za-z0-9._-]/g, '').slice(0, 40)
  return key === '' ? 'other' : key
}

/**
 * A feature the mod can switch off by itself when the engine lacks what it
 * calls. The engine's validator lets a hooks module spell `$` only as a call
 * (`$.noun.method(...)`), never read a method as a value, so a missing method
 * cannot be asked about beforehand: it is found at the call, as an `api`
 * failure, and that first failure switches the one feature off.
 *
 * - `pane`: the `/atrium` pane and the band (`$.command.register`, `$.ui.*`,
 *   `$.clock.every`).
 * - `inbox`: native delivery (`$.prompt.submit`).
 * - `tools`: the typed control plane (`$.tool.register`).
 * - `guard`: the file guard (`$.session.cwd`).
 * - `paths`: real paths (`$.fs.stat`); without it paths compare by their text.
 * - `captions`: model-made captions (`$.model.complete`, `$.clock.now`).
 * - `ask`: questions answered from a fork (`$.model.fork`).
 */
export type Feature = 'pane' | 'inbox' | 'tools' | 'guard' | 'paths' | 'captions' | 'ask'

/** The features, for a table. */
export const FEATURES: readonly Feature[] = ['pane', 'inbox', 'tools', 'guard', 'paths', 'captions', 'ask']

/** The caps to offer in `hello`: a session nobody types into has no prompt queue, so no inbox and no ask. */
export function offeredCaps(all: readonly string[], interactive: boolean): string[] {
  return all.filter(c => interactive || (c !== 'inbox' && c !== 'ask'))
}

/**
 * The mod's failures this session: a count per hook, the last one's class,
 * place and scrubbed message, and what they add up to.
 *
 * Two sums, of different classes, and they never mix:
 *
 * - **thrown**: errors of the mod's own hooks (`hook`, and `api` outside a
 *   lost feature). `INERT_AFTER` within `FAIL_WINDOW_MS` and the mod is
 *   broken: off for the session. This is the number atrium is told
 *   (`fail_thrown`) and counts over the same window.
 * - **unasked**: exchanges with atrium in a row (`ctl.<site>` keys) that got
 *   no reply the mod could read (`timeout`, `ctl`, `shape`). `INERT_AFTER`
 *   and the mod pauses. A timeout of one of the mod's own waits on the
 *   engine (the file guard on a slow disk) is counted and shown and is in
 *   neither sum: it says nothing of the mod's code or of atrium.
 */
export class Failures {
  private readonly counts = new Map<string, number>()
  private lastClass: FailClass | undefined
  private lastMessage = ''
  /** The key the last failure was counted under. */
  private lastAt = ''
  /** When each error of its own hooks was thrown, oldest first. */
  private thrownAt: number[] = []
  /** Every error of its own hooks, all session: what `fail_thrown` carries. */
  private thrownTotal = 0
  private unasked = 0
  /** Grows with every failure: what a report carried is known by it. */
  private generation = 0
  /** The generation atrium has been told up to. */
  private told = 0

  /** Count one failure of `hook`, at `now` (milliseconds). */
  note(hook: string, cls: FailClass, err: unknown, now: number = Date.now()): void {
    let key = hookKey(hook)
    if (!this.counts.has(key) && this.counts.size >= HOOKS_CAP) key = 'other'
    this.counts.set(key, (this.counts.get(key) ?? 0) + 1)
    this.lastClass = cls
    this.lastAt = key
    this.lastMessage = typeof err === 'string' ? scrub(new Error(err)).replace(/^Error: /, '') : scrub(err)
    // A feature lost to a missing engine method costs that feature, and is
    // not one of the errors that switch the whole mod off.
    if ((cls === 'hook' || cls === 'api') && !key.startsWith('feature.')) {
      this.thrownAt.push(now)
      this.thrownTotal += 1
    }
    if ((cls === 'timeout' || cls === 'ctl' || cls === 'shape') && key.startsWith('ctl.')) this.unasked += 1
    this.generation += 1
  }

  /** A ctl call was answered: the run of unanswered ones is over. */
  answered(): void {
    this.unasked = 0
  }

  /** Every failure so far. */
  total(): number {
    let n = 0
    for (const c of this.counts.values()) n += c
    return n
  }

  /** The count for one hook. */
  count(hook: string): number {
    return this.counts.get(hookKey(hook)) ?? 0
  }

  /** The class of the last failure. */
  last(): { cls: FailClass; message: string } | undefined {
    return this.lastClass === undefined ? undefined : { cls: this.lastClass, message: this.lastMessage }
  }

  /** Did its own hooks fail too often, lately: off for the session? */
  broken(now: number = Date.now()): boolean {
    this.thrownAt = this.thrownAt.filter(at => now - at <= FAIL_WINDOW_MS)
    return this.thrownAt.length >= INERT_AFTER
  }

  /** Did atrium stop answering: pause, and say hello again later? */
  unreachable(): boolean {
    return this.unasked >= INERT_AFTER
  }

  /** Is there something atrium has not been told? */
  unsent(): boolean {
    return this.generation > this.told
  }

  /**
   * The report fields that carry the counts (`fail`, `fail_class`, `fail_at`,
   * `fail_thrown`, `fail_msg`), and the generation they are a picture of:
   * hand it back to `sent` when atrium has taken them.
   */
  snapshot(): { fields: Record<string, string>; generation: number } {
    const last = this.last()
    if (last === undefined) return { fields: {}, generation: this.generation }
    const fail = Array.from(this.counts.entries())
      .map(([hook, n]) => `${hook}:${n}`)
      .join(',')
    return {
      fields: { fail, fail_class: last.cls, fail_at: this.lastAt, fail_thrown: String(this.thrownTotal), fail_msg: last.message },
      generation: this.generation,
    }
  }

  /** The fields alone. */
  fields(): Record<string, string> {
    return this.snapshot().fields
  }

  /**
   * atrium took (or, answering, declined) the picture of `generation`. A
   * failure counted since, while that report was in flight, is still to be
   * told: only what the report carried is cleared.
   */
  sent(generation: number): void {
    this.told = Math.max(this.told, generation)
  }
}
