// The reply half of the transport: what `atrium ctl` printed, read into a
// reply. The call itself (`$.process.run`) lives in register.ts, because the
// engine's validator follows `$` only inside the file that received it.

/**
 * A reply as `atrium ctl` prints it: `ok` and either the verb's fields or
 * `err`. `fault` is the mod's own mark, never atrium's: the question could not
 * be asked (`ctl`: no binary, no endpoint, no reply line; `timeout`: no reply
 * in time) or the answer was not a reply (`shape`). A refusal atrium gave has
 * no `fault`, and only such a refusal is atrium saying no.
 */
export type Reply = { ok: boolean; err?: string; fault?: 'ctl' | 'timeout' | 'shape' } & Record<string, unknown>

/** What `$.process.run` answers, the part a reply is read from. */
export type Ran = { exitCode: number; stdout: string; stderr: string }

/**
 * The reply on the last line of stdout, or `{ ok: false, err, fault }` when
 * there is none: a channel that is gone, a binary that is missing, a usage
 * error (`ctl`), or a line that is not a reply (`shape`). Never throws; the
 * caller decides what a refusal means.
 */
export function parseReply(ran: Ran): Reply {
  if (ran === null || typeof ran !== 'object' || typeof ran.stdout !== 'string') {
    return { ok: false, err: 'the engine ran the command and answered no output', fault: 'shape' }
  }
  const line = ran.stdout.trim().split('\n').pop() ?? ''
  let parsed: unknown
  try {
    parsed = JSON.parse(line)
  } catch {
    const said = typeof ran.stderr === 'string' && ran.stderr !== '' ? ran.stderr : ran.stdout
    const err = said.trim().slice(0, 200)
    return { ok: false, err: err || `exit ${ran.exitCode} with no reply`, fault: 'ctl' }
  }
  if (parsed !== null && typeof parsed === 'object' && typeof (parsed as { ok?: unknown }).ok === 'boolean') {
    // `fault` is the mod's word: one a reply happens to carry is not believed.
    const { fault: _fault, ...reply } = parsed as Record<string, unknown>
    return reply as Reply
  }
  return { ok: false, err: `not a ctl reply: ${line.slice(0, 120)}`, fault: 'shape' }
}

/**
 * One item `ctl inbox` handed over: a `send`'s text or a bus wake's framed
 * line (each a prompt to submit), or an `ask` (a question to answer from a
 * fork, with the id its reply names).
 */
export type InboxItem = { kind: 'send' | 'wake'; text: string } | { kind: 'ask'; text: string; id: number }

/** The items of an `inbox` reply, in order; none for anything else. */
export function inboxItems(reply: Reply): InboxItem[] {
  const items = reply.items
  if (reply.ok !== true || !Array.isArray(items)) return []
  const out: InboxItem[] = []
  for (const it of items) {
    if (it === null || typeof it !== 'object') continue
    const { kind, text, id } = it as Record<string, unknown>
    if (typeof text !== 'string' || text.trim() === '') continue
    if (kind === 'send' || kind === 'wake') out.push({ kind, text })
    else if (kind === 'ask' && typeof id === 'number' && Number.isInteger(id) && id > 0) out.push({ kind, text, id })
  }
  return out
}

/**
 * How often the mod asks for its deliveries when it has to poll, in
 * milliseconds: the fallback for an atrium that does not hold `inbox --wait`.
 */
export const INBOX_POLL_MS = 1500

/**
 * How long one `atrium ctl inbox --wait` may run. The broker holds the request
 * for up to 60 s and then answers with no items (the client budgets 60 s plus
 * its usual margins); this is above both, so the engine's own timeout fires
 * only on a broker that stopped answering altogether.
 */
export const INBOX_WAIT_MS = 100_000

/**
 * A `--wait` that came back with nothing this quickly was not held: an older
 * atrium (client or broker) that does not know the flag answered a plain poll.
 * A held request that found nothing takes the whole hold.
 */
export const INBOX_FAST_MS = 500

/** How many such fast empty answers in a row mean `--wait` is not understood. */
export const INBOX_FAST_STRIKES = 3

/** What the wait loop does after one `inbox --wait` reply. */
export type WaitStep = 'again' | 'poll'

/**
 * Judge one `inbox --wait` reply that took `elapsedMs`, carrying `strikes`
 * fast empties so far: `again` re-issues the wait at once, `poll` gives up on
 * `--wait` and falls back to the timed poll. The verdict and the new strike
 * count.
 *
 * - Items, or a slow empty answer (the hold ran out): the flag works; strikes
 *   reset.
 * - A fast empty answer: a strike; three in a row is an older atrium that
 *   answered a poll, and polling it on purpose beats a hot loop.
 * - `ok:false` naming the flag as an unexpected argument: an older client;
 *   poll at once.
 * - Any other refusal (the channel is gone, the hold pool is full, the hold
 *   timed out at the transport): the timed poll is the safe cadence to retry
 *   at; it comes back to `--wait` on the next `session.start`.
 */
export function waitStep(strikes: number, reply: Reply, elapsedMs: number): { step: WaitStep; strikes: number } {
  if (reply.ok !== true) {
    return { step: 'poll', strikes }
  }
  if (inboxItems(reply).length > 0 || elapsedMs >= INBOX_FAST_MS) {
    return { step: 'again', strikes: 0 }
  }
  const next = strikes + 1
  return { step: next >= INBOX_FAST_STRIKES ? 'poll' : 'again', strikes: next }
}

/** The `accepted` list of a `hello` reply, as strings only. */
export function acceptedCaps(reply: Reply): ReadonlySet<string> {
  const got = reply.accepted
  return new Set(Array.isArray(got) ? got.filter((c): c is string => typeof c === 'string') : [])
}
