// The typed control plane: the `atrium_*` tools the model can call instead of
// remembering `atrium ctl` argv. Pure: each tool's schema and the argv it
// maps to, tested with nothing mocked. The call itself is in register.ts.

/** What `$.tool.register` takes, as far as this file needs it. */
export type Spec = {
  name: string
  description: string
  inputSchema: Record<string, unknown>
}

const str = (description: string) => ({ type: 'string', description })
const fields = (description: string) => ({
  type: 'object',
  description,
  additionalProperties: { type: 'string' },
})

/** The tools, in the order they are registered. Names are `atrium_<verb>`. */
export const TOOLS: readonly Spec[] = [
  {
    name: 'subagent',
    description:
      'Run a subagent as a visible atrium pane beside this one: spawns a claude, gives it the task, waits for its answer and returns it. Use this where you would use the Agent tool; the human can watch, steer and kill the pane. Set keep to leave the pane open afterwards.',
    inputSchema: {
      type: 'object',
      properties: {
        description: str('A short (3-5 word) description of the task; becomes the pane\'s role.'),
        prompt: str('The task for the subagent, in full.'),
        keep: { type: 'boolean', description: 'Leave the pane open after its answer (default: close it).' },
      },
      required: ['description', 'prompt'],
    },
  },
  {
    name: 'spawn',
    description:
      'Open a visible teammate pane in atrium running claude (or another allowed command), tagged with a role. Returns {pane, role, session}. Prefer this over the Agent tool here: every teammate is a pane the human can watch, send to and kill.',
    inputSchema: {
      type: 'object',
      properties: {
        role: str('The role label for the pane, e.g. "dev_1" or "reviewer".'),
        cmd: { type: 'array', items: { type: 'string' }, description: 'The command to host; default ["claude"].' },
        here: { type: 'boolean', description: 'Split beside this pane instead of a new window.' },
        mode: str('Trust posture: plan | accept | automode | skip (capped at the session ceiling).'),
        worktree: str('Name of a git worktree to create and run in.'),
        identity: str('A credential identity name this pane holds, to delegate.'),
      },
      required: ['role'],
    },
  },
  {
    name: 'send',
    description: 'Deliver text to a teammate pane as a submitted prompt; queued until it is idle. Target is a pane id or role.',
    inputSchema: { type: 'object', properties: { target: str('Pane id or role.'), text: str('The prompt to submit.') }, required: ['target', 'text'] },
  },
  {
    name: 'status',
    description: 'The live status of one pane (working, waiting-approval, waiting-prompt, idle, errored, ended), or of your whole subtree when no target is given.',
    inputSchema: { type: 'object', properties: { target: str('Pane id or role; omit for the subtree.') } },
  },
  {
    name: 'list',
    description: 'The live org chart: every pane with its parent, role, depth and status.',
    inputSchema: { type: 'object', properties: {} },
  },
  {
    name: 'kill',
    description: 'Terminate a teammate pane and its whole subtree.',
    inputSchema: { type: 'object', properties: { target: str('Pane id or role.') }, required: ['target'] },
  },
  {
    name: 'board_set',
    description: "Merge fields into the shared board entry `key` (the team's source of truth); an empty value clears a field.",
    inputSchema: { type: 'object', properties: { key: str('The entry.'), fields: fields('field name to value.') }, required: ['key', 'fields'] },
  },
  {
    name: 'board_get',
    description: 'Read one board entry.',
    inputSchema: { type: 'object', properties: { key: str('The entry.') }, required: ['key'] },
  },
  {
    name: 'board_list',
    description: 'Every entry on the shared board, with its fields.',
    inputSchema: { type: 'object', properties: {} },
  },
  {
    name: 'board_claim',
    description: 'Atomically claim a board key as a lease (granted only if free or expired).',
    inputSchema: { type: 'object', properties: { key: str('The key.'), ttl_s: { type: 'integer', description: 'Lease length in seconds.' } }, required: ['key'] },
  },
  {
    name: 'board_release',
    description: 'Release a board key you claimed.',
    inputSchema: { type: 'object', properties: { key: str('The key.') }, required: ['key'] },
  },
  {
    name: 'bus_pub',
    description: 'Publish a structured event to a bus topic. `decision` marks one that needs a human or lead answer; `to` addresses panes by role or id, which wakes them.',
    inputSchema: {
      type: 'object',
      properties: {
        topic: str('The topic.'),
        fields: fields('The event, e.g. {item: "M1", status: "done", commit: "abc"}; `msg` is the headline.'),
        decision: { type: 'boolean', description: 'An escalation that needs an answer.' },
        to: { type: 'array', items: { type: 'string' }, description: 'Roles or pane ids to address.' },
      },
      required: ['topic', 'fields'],
    },
  },
  {
    name: 'bus_feed',
    description: 'Pull the events on the topics you subscribe to, after `since`.',
    inputSchema: { type: 'object', properties: { since: { type: 'integer', description: 'The last seq you read.' } } },
  },
  {
    name: 'bus_resolve',
    description: 'Mark a decision event answered.',
    inputSchema: { type: 'object', properties: { seq: { type: 'integer', description: 'The event.' } }, required: ['seq'] },
  },
  {
    name: 'bus_topics',
    description: 'The active topics and how many panes subscribe to each.',
    inputSchema: { type: 'object', properties: {} },
  },
  {
    name: 'ask',
    description:
      "Ask a teammate pane a question without interrupting it: its mod answers from a fork of the pane's own context (no tools, its turn untouched). Waits for the reply (default 120 s). Use it to learn what a teammate is doing, needs, or is blocked on.",
    inputSchema: {
      type: 'object',
      properties: {
        target: str('Pane id or role.'),
        question: str('The question, in one or two sentences.'),
        timeout_s: { type: 'integer', description: 'How long to wait for the reply (1..600; default 120).' },
      },
      required: ['target', 'question'],
    },
  },
  {
    name: 'asked',
    description: 'Read the reply to an earlier atrium_ask by its id (an ask that timed out may answer later).',
    inputSchema: { type: 'object', properties: { target: str('Pane id or role.'), id: { type: 'integer', description: 'The ask id.' } }, required: ['target', 'id'] },
  },
  {
    name: 'who',
    description: 'Which panes edited a file, with when and whether each is still live. A path relative to a worktree (src/x.rs) finds it in every worktree. Ask before you edit a shared file, and before a merge.',
    inputSchema: { type: 'object', properties: { path: str('A file path, real or worktree-relative.') }, required: ['path'] },
  },
]

/** The default wait of `ctl ask`, in seconds, as the broker's client spells it. */
export const ASK_DEFAULT_TIMEOUT_S = 120

/**
 * How long the `atrium ctl` run for `argv` may take: a plain verb answers at
 * once; `ask` waits for the reply, so its run gets the ask's own timeout and
 * a margin.
 */
export function ctlTimeoutFor(argv: readonly string[]): number {
  if (argv[0] !== 'ask') return 10_000
  const i = argv.indexOf('--timeout')
  const t = i >= 0 ? Number(argv[i + 1]) : ASK_DEFAULT_TIMEOUT_S
  return ((Number.isFinite(t) && t > 0 ? t : ASK_DEFAULT_TIMEOUT_S) + 30) * 1000
}

/** The prefix a plugin tool's full name carries: `mcp__<plugin>__`. */
export const TOOL_PREFIX = 'mcp__atrium__'

/** Is `tool` one of this mod's? */
export function isAtriumTool(tool: string): boolean {
  return tool.startsWith(TOOL_PREFIX) && TOOLS.some(t => TOOL_PREFIX + t.name === tool)
}

type Input = Record<string, unknown>

const s = (input: Input, key: string): string | undefined => {
  const v = input[key]
  return typeof v === 'string' && v.trim() !== '' ? v : undefined
}
const need = (input: Input, key: string): string => {
  const v = s(input, key)
  if (v === undefined) throw new Error(`${key} is required`)
  return v
}
const kv = (input: Input, key: string): string[] => {
  const v = input[key]
  if (v === null || typeof v !== 'object' || Array.isArray(v)) throw new Error(`${key} must be an object of strings`)
  const out: string[] = []
  for (const [k, val] of Object.entries(v as Record<string, unknown>)) {
    if (typeof val !== 'string') throw new Error(`${key}.${k} must be a string`)
    if (!/^[A-Za-z0-9_.-]+$/.test(k)) throw new Error(`${key}.${k}: a field name is letters, digits, _ . -`)
    out.push(`${k}=${val}`)
  }
  if (out.length === 0) throw new Error(`${key} needs at least one field`)
  return out
}
const whole = (input: Input, key: string): string | undefined => {
  const v = input[key]
  if (v === undefined) return undefined
  if (typeof v !== 'number' || !Number.isInteger(v) || v < 0) throw new Error(`${key} must be a whole number`)
  return String(v)
}

/**
 * The `atrium ctl` argv for a call of tool `name` (short, without the
 * prefix) with `input`, or an Error naming what is wrong with the input.
 */
export function argvFor(name: string, input: Input): string[] | Error {
  try {
    switch (name) {
      case 'spawn': {
        const args = ['spawn', '--role', need(input, 'role')]
        if (input.here === true) args.push('--here')
        const mode = s(input, 'mode')
        if (mode !== undefined) args.push('--mode', mode)
        const wt = s(input, 'worktree')
        if (wt !== undefined) args.push('--worktree', wt)
        const id = s(input, 'identity')
        if (id !== undefined) args.push('--identity', id)
        const cmd = input.cmd
        const argv = Array.isArray(cmd) && cmd.length > 0 ? cmd : ['claude']
        if (!argv.every(a => typeof a === 'string')) return new Error('cmd must be an array of strings')
        return [...args, '--', ...(argv as string[])]
      }
      case 'send':
        return ['send', need(input, 'target'), need(input, 'text')]
      case 'status': {
        const t = s(input, 'target')
        return t === undefined ? ['status'] : ['status', t]
      }
      case 'list':
        return ['list']
      case 'kill':
        return ['kill', need(input, 'target')]
      case 'board_set':
        return ['board', 'set', need(input, 'key'), ...kv(input, 'fields')]
      case 'board_get':
        return ['board', 'get', need(input, 'key')]
      case 'board_list':
        return ['board', 'list']
      case 'board_claim': {
        const args = ['board', 'claim', need(input, 'key')]
        const ttl = whole(input, 'ttl_s')
        if (ttl !== undefined) args.push('--ttl', ttl)
        return args
      }
      case 'board_release':
        return ['board', 'release', need(input, 'key')]
      case 'bus_pub': {
        const args = ['bus', 'pub', need(input, 'topic')]
        if (input.decision === true) args.push('--decision')
        const to = input.to
        if (Array.isArray(to) && to.length > 0) {
          if (!to.every(t => typeof t === 'string' && t.trim() !== '')) return new Error('to must be an array of roles or ids')
          args.push('--to', (to as string[]).join(','))
        }
        return [...args, ...kv(input, 'fields')]
      }
      case 'bus_feed': {
        const since = whole(input, 'since')
        return since === undefined ? ['bus', 'feed'] : ['bus', 'feed', '--since', since]
      }
      case 'bus_resolve': {
        const seq = whole(input, 'seq')
        if (seq === undefined) throw new Error('seq is required')
        return ['bus', 'resolve', seq]
      }
      case 'bus_topics':
        return ['bus', 'topics']
      case 'ask': {
        const args = ['ask', need(input, 'target')]
        const t = whole(input, 'timeout_s')
        if (t !== undefined) args.push('--timeout', t)
        return [...args, need(input, 'question')]
      }
      case 'asked': {
        const id = whole(input, 'id')
        if (id === undefined) throw new Error('id is required')
        return ['asked', need(input, 'target'), id]
      }
      case 'who':
        return ['who', need(input, 'path')]
      case 'subagent':
        return new Error('subagent is run by the mod, not mapped to one ctl verb')
      default:
        return new Error(`no such tool: ${name}`)
    }
  } catch (e) {
    return e instanceof Error ? e : new Error(String(e))
  }
}
