/** One daemon inbox message as the mod shows it: never claimed or changed by the mod. */
export type GoblinsMessage = {
  id: string
  sender: string
  senderName: string
  inReplyTo: string | null
  body: string
  createdAt: number
  /** queued, claimed, completed or failed when last read. */
  state: string
  operation: { kind: string; subject: string; status: string; message: string | null } | null
  raid: { name: string; event: string } | null
}

/** `goblin inbox status`, reduced to what the views draw. */
export type GoblinsInbox = {
  queued: number
  claim: string | null
  head: string | null
  revision: number
  health: string
}

declare module 'claude-code' {
  interface PluginState {
    'goblins-daemon': {
      inbox: Shaped<GoblinsInbox | null>
      /** Messages seen in this conversation, by ID. */
      messages: Shaped<Record<string, GoblinsMessage>>
      /** The message each inbox wake row was about (null: none named), by row ID. */
      wakes: Shaped<Record<string, { id: string | null; at: number }>>
      /** The message the pane shows. */
      selected: string | null
    }
  }
}
