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
 *   before the first prompt, so together they bound what the mod can add to a
 *   start (8 s, against the hook's own 10 s budget). A healthy reply takes
 *   tens of milliseconds: one process start and one line on a local socket.
 * - `report`, `poll`, `view`: background work nobody waits on; 5 s is past any
 *   healthy reply and short enough that a dead broker is noticed within a few
 *   calls.
 * - `tool`: an `atrium_*` tool the model called itself. The call is the tool,
 *   so it may take the 10 s it always had; its failure is the tool's result.
 */
export const DEADLINE = {
  hello: 5_000,
  whoami: 3_000,
  report: 5_000,
  poll: 5_000,
  view: 5_000,
  tool: 10_000,
} as const

/** How long after a call's own timeout the mod's watchdog gives up on it. */
export const WATCHDOG_MS = 1_000

/** How long `session.end` waits for the last report before letting go. */
export const END_WAIT_MS = 3_000

/** How long after a hello that could not be asked the one retry is made. */
export const HELLO_RETRY_MS = 5_000

/**
 * After this many hook errors, or this many ctl calls in a row that could not
 * be asked, the mod switches itself off for the session: every hook passes
 * straight through. Five: one failure is an accident, five is a broken mod or
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

/** Which class a thrown value is. */
export function classify(err: unknown): FailClass {
  const name = err instanceof Error ? err.name : ''
  const message = err instanceof Error ? err.message : String(err)
  if (
    name === 'TypeError' &&
    /is not a function|is not iterable|is not a constructor|Cannot read propert|undefined is not|null is not|is not defined/i.test(message)
  ) {
    return 'api'
  }
  if (name === 'ReferenceError') return 'api'
  // What the engine says of a `$` call nothing answers, or one a plugin withheld.
  if (/no implementation for|removed by plugin|not available in this|unknown (event|noun|method)/i.test(message)) return 'api'
  if (/timed? ?out|deadline/i.test(message)) return 'timeout'
  return 'hook'
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

/** A hook's name as a counter key: letters, digits, `.`, `_`, `-`; 40 at most. */
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
  /** Errors a hook threw (`hook`, `api`), all session. */
  private thrown = 0
  /** ctl calls in a row that could not be asked (`timeout`, `ctl`). */
  private unasked = 0
  private dirty = false

  /** Count one failure of `hook`. */
  note(hook: string, cls: FailClass, err: unknown): void {
    let key = hookKey(hook)
    if (!this.counts.has(key) && this.counts.size >= HOOKS_CAP) key = 'other'
    this.counts.set(key, (this.counts.get(key) ?? 0) + 1)
    this.lastClass = cls
    this.lastMessage = typeof err === 'string' ? scrub(new Error(err)).replace(/^Error: /, '') : scrub(err)
    if (cls === 'hook' || cls === 'api') this.thrown += 1
    if (cls === 'timeout' || cls === 'ctl') this.unasked += 1
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

  /** Should the mod switch itself off? */
  spent(): boolean {
    return this.thrown >= INERT_AFTER || this.unasked >= INERT_AFTER
  }

  /** Is there something atrium has not been told? */
  unsent(): boolean {
    return this.dirty
  }

  /** The report fields that carry the counts: `fail`, `fail_class`, `fail_msg`. */
  fields(): Record<string, string> {
    const last = this.last()
    if (last === undefined) return {}
    const fail = Array.from(this.counts.entries())
      .map(([hook, n]) => `${hook}:${n}`)
      .join(',')
    return { fail, fail_class: last.cls, fail_msg: last.message }
  }

  /** The fields were sent and taken. */
  sent(): void {
    this.dirty = false
  }
}
