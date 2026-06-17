// components/Dashboard.ts — Scene-aware Dashboard (Ctrl+D)

import type { Theme } from '../theme/types'
import { getState } from '../state/store'
import { formatCost, formatTokens, tokenUsagePercent } from '../utils/cost'

export type DashboardSection = 'cost' | 'runtime' | 'safety' | 'agent' | 'memory'

export interface DashboardData {
  // Cost
  sessionTokens: number
  maxTokens: number
  turnCost: number
  totalCost: number
  turnCount: number
  approachingBudget: boolean

  // Runtime
  streamingActive: boolean
  currentTool: string
  toolCallsThisTurn: number
  routerActive: boolean

  // Safety
  mcipCount: number
  inertiaCount: number
  toolTiers: Record<string, number>

  // Agent
  mode: string
  modelName: string

  // Memory
  behaviorCount: number
  knowledgeCount: number
}

/// Render the full dashboard
export function renderDashboard(theme: Theme, width: number): string[] {
  const s = getState()
  const data: DashboardData = {
    sessionTokens: s.sessionTokens,
    maxTokens: s.config.maxTokens,
    turnCost: s.sessionTokens * s.config.costPerToken,
    totalCost: s.sessionTokens * s.config.costPerToken,
    turnCount: 0,
    approachingBudget: tokenUsagePercent(s.sessionTokens, s.config.maxTokens) > 80,
    streamingActive: s.inputState === 'thinking' || s.inputState === 'executing' || s.inputState === 'outputting',
    currentTool: s.streamingTools[0]?.name || '',
    toolCallsThisTurn: s.streamingTools.length,
    routerActive: true,
    mcipCount: s.mcipCount || 0,
    inertiaCount: s.inertiaCount || 0,
    toolTiers: { S: 18, A: 4, B: 2, C: 1, D: 1 },
    mode: s.mode,
    modelName: s.modelName,
    behaviorCount: s.behaviorCount || 0,
    knowledgeCount: s.knowledgeCount || 0,
  }

  const lines: string[] = []
  const maxW = Math.max(width - 4, 50)

  lines.push('')
  lines.push(...renderCostSection(data, theme, maxW))
  lines.push('')
  lines.push(...renderRuntimeSection(data, theme, maxW))
  lines.push('')
  lines.push(...renderSafetySection(data, theme, maxW))
  lines.push('')
  lines.push(...renderAgentSection(data, theme, maxW))
  lines.push('')
  lines.push(...renderMemorySection(data, theme, maxW))
  lines.push('')

  return lines
}

function renderCostSection(data: DashboardData, theme: Theme, maxW: number): string[] {
  const lines: string[] = []
  const border = '─'.repeat(Math.min(maxW - 2, 50))

  lines.push(`  ┌─ Cost ${'─'.repeat(Math.max(0, border.length - 6))}┐`)
  lines.push(`  │`.padEnd(maxW + 2) + '│')

  // Token progress bar
  const pct = tokenUsagePercent(data.sessionTokens, data.maxTokens)
  const barWidth = 20
  const filled = Math.round((pct / 100) * barWidth)
  const bar = '█'.repeat(filled) + '░'.repeat(barWidth - filled)
  const tokenStr = formatTokens(data.sessionTokens)
  const maxStr = formatTokens(data.maxTokens)

  lines.push(`  │  Session: ${bar} ${tokenStr}/${maxStr}`.padEnd(maxW + 2) + '│')

  if (data.approachingBudget) {
    lines.push(`  │  ⚠ Approaching budget (${pct.toFixed(0)}%)`.padEnd(maxW + 2) + '│')
  }

  lines.push(`  │`.padEnd(maxW + 2) + '│')
  lines.push(`  │  Turn:    ${formatCost(data.turnCost)}`.padEnd(maxW + 2) + '│')
  lines.push(`  │  Total:   ${formatCost(data.totalCost)}`.padEnd(maxW + 2) + '│')
  lines.push(`  │  Turns:   ${data.turnCount}`.padEnd(maxW + 2) + '│')
  lines.push(`  └${'─'.repeat(Math.min(maxW - 2, border.length))}┘`)

  return lines
}

function renderRuntimeSection(data: DashboardData, theme: Theme, maxW: number): string[] {
  const lines: string[] = []
  const border = '─'.repeat(Math.min(maxW - 2, 50))

  lines.push(`  ┌─ Runtime ${'─'.repeat(Math.max(0, border.length - 9))}┐`)

  const status = data.streamingActive ? '⏳ Running' : '○ Idle'
  lines.push(`  │  Status: ${status}`.padEnd(maxW + 2) + '│')

  if (data.currentTool) {
    lines.push(`  │  Current: ${data.currentTool}`.padEnd(maxW + 2) + '│')
  }

  if (data.toolCallsThisTurn > 0) {
    lines.push(`  │  Tools:   ${data.toolCallsThisTurn} calls this turn`.padEnd(maxW + 2) + '│')
  }

  lines.push(`  │  Router:  ${data.routerActive ? 'Active' : 'Inactive'}`.padEnd(maxW + 2) + '│')
  lines.push(`  └${'─'.repeat(Math.min(maxW - 2, border.length))}┘`)

  return lines
}

function renderSafetySection(data: DashboardData, theme: Theme, maxW: number): string[] {
  const lines: string[] = []
  const border = '─'.repeat(Math.min(maxW - 2, 50))

  lines.push(`  ┌─ Safety ${'─'.repeat(Math.max(0, border.length - 8))}┐`)
  lines.push(`  │  MCIP:   ${data.mcipCount} confirmations`.padEnd(maxW + 2) + '│')
  lines.push(`  │  Inertia: ${data.inertiaCount} detections`.padEnd(maxW + 2) + '│')

  // Tool tiers
  const tiers = Object.entries(data.toolTiers)
    .filter(([_, count]) => count > 0)
    .map(([tier, count]) => `${tier}:${count}`)
    .join(' ')
  lines.push(`  │  Tiers:  ${tiers}`.padEnd(maxW + 2) + '│')

  lines.push(`  └${'─'.repeat(Math.min(maxW - 2, border.length))}┘`)

  return lines
}

function renderAgentSection(data: DashboardData, theme: Theme, maxW: number): string[] {
  const lines: string[] = []
  const border = '─'.repeat(Math.min(maxW - 2, 50))

  lines.push(`  ┌─ Agent ${'─'.repeat(Math.max(0, border.length - 7))}┐`)
  lines.push(`  │  Mode:  ${data.mode}`.padEnd(maxW + 2) + '│')
  lines.push(`  │  Model: ${data.modelName}`.padEnd(maxW + 2) + '│')

  lines.push(`  │`.padEnd(maxW + 2) + '│')
  lines.push(`  │  Mode flow:`.padEnd(maxW + 2) + '│')
  lines.push(`  │  Clarify → Team → Clarify`.padEnd(maxW + 2) + '│')
  lines.push(`  │       ↓`.padEnd(maxW + 2) + '│')
  lines.push(`  │    Meeting → Clarify`.padEnd(maxW + 2) + '│')
  lines.push(`  │  Plan → Team`.padEnd(maxW + 2) + '│')

  lines.push(`  └${'─'.repeat(Math.min(maxW - 2, border.length))}┘`)

  return lines
}

function renderMemorySection(data: DashboardData, theme: Theme, maxW: number): string[] {
  const lines: string[] = []
  const border = '─'.repeat(Math.min(maxW - 2, 50))

  lines.push(`  ┌─ Memory ${'─'.repeat(Math.max(0, border.length - 8))}┐`)
  lines.push(`  │  🧠 Behavior:  ${data.behaviorCount}/2000`.padEnd(maxW + 2) + '│')
  lines.push(`  │  📚 Knowledge: ${data.knowledgeCount}/5000`.padEnd(maxW + 2) + '│')
  lines.push(`  └${'─'.repeat(Math.min(maxW - 2, border.length))}┘`)

  return lines
}
