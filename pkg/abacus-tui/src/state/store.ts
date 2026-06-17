// state/store.ts — Zustand store (vanilla, no React)

import { createStore } from 'zustand/vanilla'
import type { Card } from '../components/cards/types'

// Re-export Card for convenience
export type { Card }

// ─── Types ───

export type Mode = 'clarify' | 'team' | 'meeting' | 'plan'
export type InputState = 'ready' | 'typing' | 'completing' | 'thinking' | 'executing' | 'outputting' | 'paused' | 'editor'
export type Focus = 'input' | 'panel' | 'command_hint'
export type Scenario = 'trading' | 'coding' | 'editing'

export interface Toast {
  id: string
  message: string
  type: 'info' | 'success' | 'warning' | 'error'
  createdAt: number
  duration: number
}

export interface AppConfig {
  maxTokens: number
  costPerToken: number
  theme: string
  language: string
}

export interface StreamingTool {
  name: string
  status: 'Running' | 'Success' | 'Failed'
  durationMs: number
}

export interface AgentInfo {
  id: string
  connected: boolean
  tools: number
  health: 'healthy' | 'unreachable' | 'unknown'
  trust: string
}

// ─── Store Interface ───

export interface AppStore {
  // Mode
  mode: Mode
  setMode: (mode: Mode) => void

  // Input
  inputState: InputState
  input: string
  cursorPos: number
  inputHistory: string[]
  historyIndex: number
  setInput: (input: string) => void
  setInputState: (state: InputState) => void
  pushHistory: (entry: string) => void
  navigateHistory: (direction: 'up' | 'down') => void

  // UI
  focus: Focus
  panelVisible: boolean
  dashboardVisible: boolean
  toasts: Toast[]
  themeName: string
  scenario: Scenario
  compact: boolean
  setFocus: (f: Focus) => void
  togglePanel: () => void
  toggleDashboard: () => void
  addToast: (t: Omit<Toast, 'id' | 'createdAt'>) => void
  removeToast: (id: string) => void

  // Session
  modelName: string
  sessionTokens: number
  config: AppConfig
  setModel: (name: string) => void

  // Streaming tools (for dashboard/panel display)
  streamingTools: StreamingTool[]
  updateStreamingTool: (name: string, patch: Partial<StreamingTool>) => void
  resetStreamingTools: () => void

  // Safety counters (for dashboard)
  mcipCount: number
  inertiaCount: number
  incrementMcip: () => void
  incrementInertia: () => void

  // Memory counters (for dashboard)
  behaviorCount: number
  knowledgeCount: number

  // Agent state (for panel)
  agents: AgentInfo[]
  setAgents: (agents: AgentInfo[]) => void
}

export type AppState = AppStore

// ─── Store ───

export const store = createStore<AppStore>()((set, get) => ({
  // Mode
  mode: 'clarify' as Mode,
  setMode: (mode) => set({ mode }),

  // Input
  inputState: 'ready' as InputState,
  input: '',
  cursorPos: 0,
  inputHistory: [],
  historyIndex: -1,
  setInput: (input) => set({ input, cursorPos: input.length }),
  setInputState: (inputState) => set({ inputState }),
  pushHistory: (entry) => set(s => ({
    inputHistory: [...s.inputHistory.slice(-99), entry],
    historyIndex: -1,
  })),
  navigateHistory: (direction) => {
    const s = get()
    if (s.inputHistory.length === 0) return
    if (direction === 'up') {
      const idx = s.historyIndex < 0
        ? s.inputHistory.length - 1
        : Math.max(0, s.historyIndex - 1)
      set({ historyIndex: idx, input: s.inputHistory[idx], cursorPos: s.inputHistory[idx].length })
    } else {
      if (s.historyIndex < 0) return
      const idx = s.historyIndex + 1
      if (idx >= s.inputHistory.length) {
        set({ historyIndex: -1, input: '', cursorPos: 0 })
      } else {
        set({ historyIndex: idx, input: s.inputHistory[idx], cursorPos: s.inputHistory[idx].length })
      }
    }
  },

  // UI
  focus: 'input' as Focus,
  panelVisible: false,
  dashboardVisible: false,
  toasts: [],
  themeName: 'abacus-dark',
  scenario: 'coding' as Scenario,
  compact: false,
  setFocus: (focus) => set({ focus }),
  togglePanel: () => set(s => ({ panelVisible: !s.panelVisible })),
  toggleDashboard: () => set(s => ({ dashboardVisible: !s.dashboardVisible })),
  addToast: (t) => set(s => ({
    toasts: [...s.toasts, { ...t, id: Math.random().toString(36).slice(2), createdAt: Date.now() }],
  })),
  removeToast: (id) => set(s => ({ toasts: s.toasts.filter(t => t.id !== id) })),

  // Session
  modelName: 'claude-sonnet-4',
  sessionTokens: 0,
  config: { maxTokens: 200000, costPerToken: 0.000002, theme: 'abacus-dark', language: 'zh' },
  setModel: (modelName) => set({ modelName }),

  // Streaming tools
  streamingTools: [],
  updateStreamingTool: (name, patch) => set(s => {
    const exists = s.streamingTools.find(t => t.name === name)
    if (exists) {
      return { streamingTools: s.streamingTools.map(t => t.name === name ? { ...t, ...patch } : t) }
    }
    return { streamingTools: [...s.streamingTools, { name, status: 'Running' as const, durationMs: 0, ...patch }] }
  }),
  resetStreamingTools: () => set({ streamingTools: [] }),

  // Safety counters
  mcipCount: 0,
  inertiaCount: 0,
  incrementMcip: () => set(s => ({ mcipCount: s.mcipCount + 1 })),
  incrementInertia: () => set(s => ({ inertiaCount: s.inertiaCount + 1 })),

  // Memory counters
  behaviorCount: 0,
  knowledgeCount: 0,

  // Agent state
  agents: [] as AgentInfo[],
  setAgents: (agents) => set({ agents }),
}))

// ─── Convenience accessor ───
export const getState = store.getState
export const subscribe = store.subscribe
