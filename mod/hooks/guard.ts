// The file guard: an edit outside the files the pane's item owns is refused
// with a reason that names the owner. Pure over real paths; register.ts
// resolves them.

const SEP = /[\\/]/

/** The file an edit tool call is about: `file_path`, or a notebook's path. */
export function pathOf(e: Record<string, unknown>): string | undefined {
  const p = e.file_path ?? e.notebook_path
  return typeof p === 'string' && p !== '' ? p : undefined
}

/** `path` split into its folder (with no trailing separator) and its name. */
export function splitParent(path: string): { folder: string; name: string } {
  const trimmed = path.replace(/[\\/]+$/, '')
  const idx = Math.max(trimmed.lastIndexOf('/'), trimmed.lastIndexOf('\\'))
  if (idx < 0) return { folder: '.', name: trimmed }
  return { folder: trimmed.slice(0, idx) || trimmed.slice(0, 1), name: trimmed.slice(idx + 1) }
}

/** Is `path` absolute, on either platform? */
export function isAbsolute(path: string): boolean {
  return path.startsWith('/') || /^[A-Za-z]:[\\/]/.test(path) || path.startsWith('\\\\')
}

/** `base` joined with `rel` (absolute `rel` wins), with one separator. */
export function join(base: string, rel: string): string {
  if (isAbsolute(rel)) return rel
  const sep = base.includes('\\') && !base.includes('/') ? '\\' : '/'
  return `${base.replace(/[\\/]+$/, '')}${sep}${rel.replace(/^[\\/]+/, '')}`
}

/**
 * `path` with its `.` and `..` segments folded, by its text alone: what a
 * path is compared as when the filesystem could not be asked for its real
 * one (the folder is not there yet, or the engine's `stat` failed). A `..`
 * never climbs past the root.
 */
export function lexical(path: string): string {
  const sep = path.includes('\\') && !path.includes('/') ? '\\' : '/'
  const drive = /^[A-Za-z]:/.exec(path)?.[0] ?? ''
  const rest = path.slice(drive.length)
  const rooted = /^[\\/]/.test(rest)
  const out: string[] = []
  for (const part of rest.split(SEP)) {
    if (part === '' || part === '.') continue
    if (part === '..') {
      if (out.length > 0 && out[out.length - 1] !== '..') out.pop()
      else if (!rooted) out.push('..')
      continue
    }
    out.push(part)
  }
  return `${drive}${rooted ? sep : ''}${out.join(sep)}`
}

/** Two real paths are the same file when equal after normalising separators. */
function same(a: string, b: string): boolean {
  return a.split(SEP).join('/') === b.split(SEP).join('/')
}

/**
 * The reason to refuse an edit of `real` (its real path, or undefined when it
 * could not be placed), given the real paths of the files the item owns; or
 * undefined when it is owned. An empty `owned` guards nothing.
 */
export function guardVerdict(
  real: string | undefined,
  owned: readonly string[],
  item: string | undefined,
  role: string | undefined,
): string | undefined {
  if (owned.length === 0) return undefined
  if (real !== undefined && owned.some(o => same(o, real))) return undefined
  const who = role === undefined ? 'this pane' : `"${role}"`
  const what = item === undefined ? who : `${item} (${who})`
  const names = owned.map(o => splitParent(o).name).join(', ')
  return real === undefined
    ? `atrium: ${what} owns ${names}; that path cannot be placed, so it is not among them. Post the change to your lead on the bus instead of making it here.`
    : `atrium: ${what} owns ${names}, not ${splitParent(real).name}. Post the change to your lead on the bus instead of making it here.`
}
