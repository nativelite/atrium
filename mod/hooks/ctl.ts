// The reply half of the transport: what `atrium ctl` printed, read into a
// reply. The call itself (`$.process.run`) lives in register.ts, because the
// engine's validator follows `$` only inside the file that received it.

/** A reply as `atrium ctl` prints it: `ok` and either the verb's fields or `err`. */
export type Reply = { ok: boolean; err?: string } & Record<string, unknown>

/** What `$.process.run` answers, the part a reply is read from. */
export type Ran = { exitCode: number; stdout: string; stderr: string }

/**
 * The reply on the last line of stdout, or `{ ok: false, err }` when there is
 * none: a channel that is gone, a binary that is missing, a usage error. Never
 * throws; the caller decides what a refusal means.
 */
export function parseReply(ran: Ran): Reply {
  const line = ran.stdout.trim().split('\n').pop() ?? ''
  let parsed: unknown
  try {
    parsed = JSON.parse(line)
  } catch {
    const err = (ran.stderr || ran.stdout).trim().slice(0, 200)
    return { ok: false, err: err || `exit ${ran.exitCode} with no reply` }
  }
  if (parsed !== null && typeof parsed === 'object' && 'ok' in parsed) {
    return parsed as Reply
  }
  return { ok: false, err: `not a ctl reply: ${line.slice(0, 120)}` }
}

/** One delivery `ctl inbox` handed over: a `send`'s text, or a bus wake's framed line. */
export type InboxItem = { kind: 'send' | 'wake'; text: string }

/** The items of an `inbox` reply, in order; none for anything else. */
export function inboxItems(reply: Reply): InboxItem[] {
  const items = reply.items
  if (reply.ok !== true || !Array.isArray(items)) return []
  const out: InboxItem[] = []
  for (const it of items) {
    if (it === null || typeof it !== 'object') continue
    const { kind, text } = it as Record<string, unknown>
    if ((kind === 'send' || kind === 'wake') && typeof text === 'string' && text.trim() !== '') out.push({ kind, text })
  }
  return out
}

/** How often the mod asks for its deliveries, in milliseconds. */
export const INBOX_POLL_MS = 1500

/** The `accepted` list of a `hello` reply, as strings only. */
export function acceptedCaps(reply: Reply): ReadonlySet<string> {
  const got = reply.accepted
  return new Set(Array.isArray(got) ? got.filter((c): c is string => typeof c === 'string') : [])
}
