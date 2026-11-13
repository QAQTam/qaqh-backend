export interface Message {
  role: 'user' | 'assistant'
  content: string
  reasoning?: string
  reasoningSegments?: string[]
  tool_calls?: ToolCallEntry[]
}

export interface ToolCallEntry {
  id?: string
  name: string
  args: string
  output?: string
}

export interface SessionMeta {
  seed: string
  model?: string
  updated_at?: number
  message_count?: number
  messages?: unknown[]
  last_summary?: string
}
