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
 * After this many errors thrown by its hooks the mod switches itself off for
 * the session; after this many ctl exchanges in a row that got no reply it
 * could read, it pauses (its loops stop, and it says hello again on
 * `HELLO_BACKOFF_S`). Either way every hook passes straight through. Five: one failure is an accident, five is a broken mod or
 * a broker that is gone, and a mod that keeps failing costs every tool call a
 * caught exception for nothing.
 */
export const INERT_AFTER = 5

/** How many refused inbox polls in a row end the polling (atrium said no). */
export const INBOX_REFUSALS = 10

/** What atrium's refusal starts with once it has set this pane's mod aside. */
export const SET_ASIDE = 'mod set aside'

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
 * The mod's failures this session: a count per hook, the last one's class and
 * scrubbed message, and whether the mod should switch itself off.
 */
export class Failures {
  private readonly counts = new Map<string, number>()
  private lastClass: FailClass | undefined
  private lastMessage = ''
  /** The key the last failure was counted under. */
  private lastAt = ''
  /** Errors a hook threw (`hook`, `api`), all session. */
  private thrown = 0
  /**
   * ctl exchanges in a row that got no reply the mod could read: nobody
   * answered (`timeout`, `ctl`), or what came back was no reply (`shape`).
   */
  private unasked = 0
  private dirty = false

  /** Count one failure of `hook`. */
  note(hook: string, cls: FailClass, err: unknown): void {
    let key = hookKey(hook)
    if (!this.counts.has(key) && this.counts.size >= HOOKS_CAP) key = 'other'
    this.counts.set(key, (this.counts.get(key) ?? 0) + 1)
    this.lastClass = cls
    this.lastAt = key
    this.lastMessage = typeof err === 'string' ? scrub(new Error(err)).replace(/^Error: /, '') : scrub(err)
    // A feature lost to a missing engine method costs that feature, and is
    // not one of the errors that switch the whole mod off.
    if ((cls === 'hook' || cls === 'api') && !key.startsWith('feature.')) this.thrown += 1
    if (cls === 'timeout' || cls === 'ctl' || cls === 'shape') this.unasked += 1
    this.dirty = true
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

  /** Did its own hooks fail too often: off for the session? */
  broken(): boolean {
    return this.thrown >= INERT_AFTER
  }

  /** Did atrium stop answering: pause, and say hello again later? */
  unreachable(): boolean {
    return this.unasked >= INERT_AFTER
  }

  /** Is there something atrium has not been told? */
  unsent(): boolean {
    return this.dirty
  }

  /** The report fields that carry the counts: `fail`, `fail_class`, `fail_at`, `fail_msg`. */
  fields(): Record<string, string> {
    const last = this.last()
    if (last === undefined) return {}
    const fail = Array.from(this.counts.entries())
      .map(([hook, n]) => `${hook}:${n}`)
      .join(',')
    return { fail, fail_class: last.cls, fail_at: this.lastAt, fail_msg: last.message }
  }

  /** The fields were sent and taken. */
  sent(): void {
    this.dirty = false
  }
}
