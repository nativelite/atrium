// Captions and questions, the pure half: what a tool call says the pane is
// doing, when a model-made caption is worth a small call, and how a queued
// question is put to a fork of the pane's own context. Tested with nothing
// mocked; the calls themselves are in register.ts.

/** The most of a caption sent to the broker; it caps again on its side. */
export const CAPTION_CAP = 80

/** The fewest characters of the model's own text a model caption is made from. */
export const CAPTION_MIN_CHARS = 120

/** The least time between two model captions for one pane. */
export const CAPTION_MIN_MS = 20_000

/** The most of an ask's reply sent to the broker. */
export const ASK_REPLY_CAP = 4_000

const basename = (p: string): string => {
  const parts = p.replace(/[\\/]+$/, '').split(/[\\/]/)
  return parts[parts.length - 1] || p
}

const str = (v: unknown): string | undefined => (typeof v === 'string' && v.trim() !== '' ? v : undefined)

/** `s` on one line, inner whitespace collapsed, at most `CAPTION_CAP` characters. */
export function clipCaption(s: string): string {
  const flat = s.replace(/\s+/g, ' ').trim()
  const chars = Array.from(flat)
  return chars.length <= CAPTION_CAP ? flat : chars.slice(0, CAPTION_CAP - 1).join('') + '…'
}

/**
 * The program a command line runs and its subcommand — `cargo test`,
 * `git commit`, `python dev.py check` — and nothing after. A command line
 * carries flags, assignments, quoted text, tokens and paths; a caption, which
 * the whole fleet reads in `ctl status` and the overview, carries only what
 * the pane is doing. Leading `VAR=value` are skipped, a path is its last
 * component, and the words stop at the first flag, assignment, quote,
 * operator or URL; at most three.
 */
export function commandWords(line: string): string {
  const toks = line.trim().split(/\s+/)
  let i = 0
  while (i < toks.length && /^[A-Za-z_][A-Za-z0-9_]*=/.test(toks[i])) i++
  const out: string[] = []
  for (; i < toks.length && out.length < 3; i++) {
    const t = toks[i]
    if (t.startsWith('-') || /[=:"'`$|&;<>()]/.test(t)) break
    out.push(basename(t))
  }
  return out.join(' ')
}

/**
 * What a tool call says the pane is doing, in a few words, free of charge:
 * `running cargo test`, `editing filter.rs`, `reading main.rs`. Nothing from
 * a conversation and nothing a command line carries: the program and its
 * subcommand, a file's name, a pattern.
 */
export function toolCaption(tool: string, input: Record<string, unknown>): string | undefined {
  const file = str(input.file_path) ?? str(input.notebook_path) ?? str(input.path)
  switch (tool) {
    case 'Bash': {
      const cmd = str(input.command)
      if (cmd === undefined) return 'running a command'
      const first = cmd.split('\n').find(l => l.trim() !== '') ?? cmd
      const words = commandWords(first)
      return words === '' ? 'running a command' : clipCaption(`running ${words}`)
    }
    case 'Edit':
    case 'MultiEdit':
    case 'Write':
    case 'NotebookEdit':
      return file === undefined ? 'editing a file' : clipCaption(`editing ${basename(file)}`)
    case 'Read':
      return file === undefined ? 'reading a file' : clipCaption(`reading ${basename(file)}`)
    case 'Grep': {
      const pat = str(input.pattern)
      return pat === undefined ? 'searching the code' : clipCaption(`searching for ${pat}`)
    }
    case 'Glob': {
      const pat = str(input.pattern)
      return pat === undefined ? 'listing files' : clipCaption(`finding ${pat}`)
    }
    case 'WebFetch': {
      const url = str(input.url)
      if (url === undefined) return 'fetching a page'
      const host = url.replace(/^[a-z]+:\/\//i, '').split('/')[0]
      return clipCaption(`fetching ${host}`)
    }
    case 'WebSearch': {
      const q = str(input.query)
      return q === undefined ? 'searching the web' : clipCaption(`searching the web for ${q}`)
    }
    case 'Agent':
    case 'Task': {
      const d = str(input.description)
      return d === undefined ? 'delegating' : clipCaption(`delegating: ${d}`)
    }
    case 'TodoWrite':
    case 'TaskCreate':
    case 'TaskUpdate':
      return 'planning'
    case 'AskUserQuestion':
      return 'asking the human'
    default:
      if (tool.startsWith('mcp__atrium__')) {
        const verb = tool.slice('mcp__atrium__'.length)
        const target = str(input.target) ?? str(input.role)
        return clipCaption(target === undefined ? `atrium ${verb}` : `atrium ${verb} ${target}`)
      }
      return clipCaption(`using ${tool}`)
  }
}

/** Is a model caption due: enough new text, and long enough since the last one? */
export function captionDue(lastAt: number, now: number, chars: number): boolean {
  return chars >= CAPTION_MIN_CHARS && now - lastAt >= CAPTION_MIN_MS
}

/** The small model request for a caption over the model's own recent text. */
export function captionRequest(text: string): { system: string; prompt: string } {
  const tail = Array.from(text).slice(-600).join('')
  return {
    system:
      'You caption what a coding agent is doing right now, for a status line. Answer with at most six words, lowercase, present tense, no period, no quotes, from the agent\'s own words below. Say what it is doing, not what it says.',
    prompt: tail,
  }
}

/** The one user message a queued question is put to the pane's fork as. */
export function askPrompt(question: string): string {
  return (
    `[atrium ask] A teammate or the human asks, without interrupting your work: ${question.trim()}\n` +
    'Answer in at most three sentences from what you know of your current task: what you are doing, what you need, what blocks you. No tools, no preamble.'
  )
}

/** What `$.model.fork` resolved to, as the reply text the broker gets. */
export function askReplyText(r: { isAnswered: boolean; text?: string; reason?: string }): string {
  if (r.isAnswered && typeof r.text === 'string' && r.text.trim() !== '') {
    const chars = Array.from(r.text.trim())
    return chars.length <= ASK_REPLY_CAP ? r.text.trim() : chars.slice(0, ASK_REPLY_CAP - 1).join('') + '…'
  }
  switch (r.reason) {
    case 'nothing-to-fork':
      return '(no reply yet: this pane has not finished a first turn)'
    case 'aborted':
      return '(no reply: the turn was interrupted while answering)'
    case 'api-error':
      return '(no reply: the API refused the fork)'
    default:
      return `(no reply: ${r.reason ?? 'empty'})`
  }
}
