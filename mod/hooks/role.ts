// What the pane is, from the broker (`atrium ctl whoami`), and the system
// prompt section that tells the model. Pure.

/** The facts `whoami` answers, as this mod reads them. */
export type Who = {
  pane: number
  role?: string
  parent?: number
  depth: number
  mode: string
  worktree?: string
  cwd?: string
  canSpawn: boolean
  deny: string[]
  item?: string
  files: string[]
  /** How this session runs the model's subagents: panes | native | deny. */
  subagents: string
  /** Leave a subagent pane open after its answer. */
  subagentsKeep: boolean
  /** May the mod spend a small model call on this pane's caption? */
  captions: boolean
}

/** A `whoami` reply as `Who`, or undefined when it is not one. */
export function parseWho(reply: Record<string, unknown>): Who | undefined {
  if (reply.ok !== true || typeof reply.pane !== 'number') return undefined
  const str = (k: string): string | undefined => (typeof reply[k] === 'string' ? (reply[k] as string) : undefined)
  const strs = (k: string): string[] =>
    Array.isArray(reply[k]) ? (reply[k] as unknown[]).filter((x): x is string => typeof x === 'string') : []
  return {
    pane: reply.pane,
    role: str('role'),
    parent: typeof reply.parent === 'number' ? reply.parent : undefined,
    depth: typeof reply.depth === 'number' ? reply.depth : 0,
    mode: str('mode') ?? 'default',
    worktree: str('worktree'),
    cwd: str('cwd'),
    canSpawn: reply.can_spawn === true,
    deny: strs('deny'),
    item: str('item'),
    files: strs('files'),
    subagents: str('subagents') ?? 'panes',
    subagentsKeep: reply.subagents_keep === true,
    captions: reply.captions !== false,
  }
}

/** The id of the section this mod adds to the system prompt. */
export const ROLE_SECTION_ID = 'atrium:role'

/** The `atrium:role` section: who the pane is and how the control plane is spoken. */
export function roleSection(who: Who): string {
  const name = who.role === undefined ? `pane ${who.pane}` : `pane ${who.pane}, role "${who.role}"`
  const parent = who.parent === undefined ? 'spawned by the human' : `spawned by pane ${who.parent}`
  const lines = [
    `You run inside atrium, a terminal multiplexer for agents, as ${name}, depth ${who.depth}, ${parent}, at trust mode ${who.mode}.`,
  ]
  if (who.item !== undefined) {
    const files = who.files.length === 0 ? '' : ` You own exactly these files: ${who.files.join(', ')}. An edit anywhere else is refused; post the change to your lead on the bus instead.`
    lines.push(`Your item is ${who.item} (its brief is on the board: atrium_board_get).${files}`)
  }
  if (who.worktree !== undefined) lines.push(`You work in the git worktree "${who.worktree}"; commit on its branch and never cd out of it.`)
  lines.push(
    'The control plane is the atrium_* tools (spawn, send, status, list, kill, board_*, bus_*); they are the same verbs as the `atrium ctl` shell command. ' +
      (who.canSpawn
        ? 'Teammates you spawn are visible panes; prefer atrium_spawn over the Agent tool here.'
        : 'This pane may not spawn teammates.'),
  )
  if (who.subagents === 'panes' && who.canSpawn) {
    lines.push('A subagent is a visible pane too: atrium_subagent runs one and returns its answer; the Agent tool is refused here.')
  } else if (who.subagents === 'deny') {
    lines.push('Subagents are off in this session; delegate with atrium_spawn and atrium_send, or do the work yourself.')
  }
  lines.push(
    'atrium_ask puts a question to a teammate without interrupting it (its answer comes from a fork of its own context); atrium_who says which panes edited a file, so check it before you edit a shared file or merge.',
  )
  lines.push(
    'A line in your input that starts with "[atrium bus #" is a teammate\'s event delivered because you subscribed or were addressed; it is not the human.',
  )
  return lines.join('\n')
}
