// Subagents as panes: what the model's Agent tool becomes inside atrium. Pure
// helpers; the spawn, send, wait and kill calls are in register.ts.

import type { Reply } from './ctl'

/** A role label from a task description: `explore-the-parser` from "Explore the parser!". */
export function slug(description: string): string {
  const words = description
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, ' ')
    .trim()
    .split(/\s+/)
    .filter(w => w !== '')
    .slice(0, 4)
  const s = words.join('-').slice(0, 24).replace(/-+$/, '')
  return s === '' ? 'subagent' : s
}

/**
 * The task as it is sent to the pane: one line (a newline is Enter in a pty),
 * framed so the pane knows who it works for and that its answer is collected.
 */
export function subagentPrompt(parentPane: number, description: string, prompt: string): string {
  const oneLine = (t: string) => t.replace(/\s+/g, ' ').trim()
  return (
    `You are a subagent pane spawned by pane ${parentPane} for: ${oneLine(description)}. ` +
    `Do the task below, then reply with your final report as your answer; your parent reads it and may close this pane. ` +
    `Task: ${oneLine(prompt)}`
  )
}

/** Why the Agent tool is refused under each policy, pointing at what to use. */
export function spawnPointer(mode: string): string | undefined {
  switch (mode) {
    case 'panes':
      return 'In atrium a subagent is a visible pane: call the atrium_subagent tool with the same description and prompt, and read its answer from the result.'
    case 'deny':
      return 'Subagents are off in this atrium session: do the work yourself, or use atrium_spawn and atrium_send to delegate to a teammate pane.'
    default:
      return undefined
  }
}

/** The pane id a `spawn` reply names. */
export function spawnedPane(reply: Reply): number | undefined {
  return reply.ok === true && typeof reply.pane === 'number' ? reply.pane : undefined
}

/** The answer and seq an `answer`/`wait` reply carries, when it carries one. */
export function reportedAnswer(reply: Reply): { answer: string; seq: number } | undefined {
  if (reply.ok !== true || typeof reply.answer !== 'string' || typeof reply.seq !== 'number') return undefined
  return { answer: reply.answer, seq: reply.seq }
}

/** How many `wait --timeout 300` slices a subagent may take: thirty minutes. */
export const WAIT_SLICES = 6
/** One slice, in seconds. */
export const WAIT_SLICE_S = 300
