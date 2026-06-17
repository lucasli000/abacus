// bridge/types.ts — TypeScript types matching Rust NAPI bridge

export interface StreamEvent {
  eventType: string
  data: string
}

export type StreamChunk =
  | { kind: 'iteration_start'; iteration: number }
  | { kind: 'thinking'; text: string }
  | { kind: 'text_delta'; text: string }
  | { kind: 'tool_start'; name: string }
  | { kind: 'tool_args'; name: string; argsJson: string }
  | { kind: 'tool_output'; name: string; outputJson: string }
  | { kind: 'tool_end'; name: string; success: boolean; durationMs: number; failureKind?: string }
  | { kind: 'confirm_required'; request: McipConfirmRequest }
  | { kind: 'compress_start' }
  | { kind: 'compress_end'; messagesCompressed: number; tokensSaved: number }
  | { kind: 'complete'; stats: TurnStats }
  | { kind: 'error'; message: string }
  | { kind: 'retry_progress'; attempt: number; maxAttempts: number; reason: string }
  | { kind: 'team_progress'; phase: string; tasks: TeamTaskInfo[] }
  | { kind: 'tool_health'; entries: ToolHealthEntry[] }
  | { kind: 'auth_result'; tool: string; approved: boolean }
  | { kind: 'command_result'; command: string; success: boolean }
  | { kind: 'meeting_started'; topic: string }

export interface TurnStats {
  promptTokens: number
  completionTokens: number
  cachedTokens: number
  totalLatencyMs: number
  toolCalls: number
  iterations: number
}

export interface McipConfirmRequest {
  toolId: string
  reason: string
  kind: 'McipPolicy' | 'DestructiveOp'
  paramsPreview: string
  nonce: string
  suggestedAction?: boolean
}

export interface ToolRecord {
  name: string
  args: string
  status: 'Success' | 'Failed' | 'Running'
  durationMs: number
  time: string
}

export interface TeamTaskInfo {
  id: string
  name: string
  status: string
  assignee?: string
}

export interface ToolHealthEntry {
  toolId: string
  tier: 'S' | 'A' | 'B' | 'C' | 'D'
  blockedByEnv: boolean
}

export interface EngineResponse {
  text: string
  thinking?: string
  toolRecords: ToolRecord[]
  stats?: TurnStats
  inertiaWarning?: string
  pendingConfirmations: McipConfirmRequest[]
  needsClarify?: string
  tokensFreed?: number
}

// Native bridge interface (polling model)
export interface NativeBridge {
  new (): NativeBridge
  init(model: string, thinking: string): Promise<void>
  destroy(): Promise<void>
  isInitialized(): Promise<boolean>
  getModel(): Promise<string>
  getThinking(): Promise<string>
  sendMessageEcho(input: string): Promise<string>
  sendMessage(input: string): Promise<void>
  pollEvents(): Promise<string>  // Returns JSON array of StreamEvent
  cancelTurn(): void
  continueGeneration(): Promise<void>
  startTeam(goal: string): Promise<void>
  sendTeamMessage(input: string): Promise<void>
  startMeeting(topic: string): Promise<void>
  sendMeetingMessage(input: string): Promise<void>
  confirmTools(decisionsJson: string): Promise<void>
  saveSession(): Promise<string>
  loadSession(path: string): Promise<void>
  listSessions(): Promise<string>
  getConfig(): Promise<string>
  reloadConfig(): Promise<void>
  listModels(): Promise<string>
  executeSlashCommand(command: string): Promise<void>
}
