// The atrium mod's state contract: what its pane and band draw from.

/** An open decision on the bus, as the pane lists it. */
export type AtriumDecision = { seq: number; topic: string; from: string; to: string; msg: string }
/** One board entry: its key and its fields as `k=v` text. */
export type AtriumBoardRow = { key: string; by: string; fields: string }
/** One recent bus event, as one line. */
export type AtriumFeedRow = { seq: number; topic: string; from: string; kind: string; line: string }
/** What `/atrium` shows: the broker's board, the open decisions, the feed. */
export type AtriumView = {
  fetchedAt: number
  error?: string
  decisions: AtriumDecision[]
  board: AtriumBoardRow[]
  feed: AtriumFeedRow[]
}

declare module 'claude-code' {
  interface PluginState {
    atrium: { view: AtriumView | null }
  }
}
