// components/panel/SectionRegistry.ts — Side panel section system

import type { Theme } from '../../theme/types'
import type { AppState } from '../../state/store'

/// A section in the side panel
export interface PanelSection {
  id: string
  title: string
  order: number
  minHeight: number
  visible: (state: AppState) => boolean
  render: (state: AppState, theme: Theme, width: number) => string[]
}

/// Section registry
export class SectionRegistry {
  private sections: Map<string, PanelSection> = new Map()

  register(section: PanelSection): void {
    this.sections.set(section.id, section)
  }

  unregister(id: string): void {
    this.sections.delete(id)
  }

  getVisible(state: AppState): PanelSection[] {
    return Array.from(this.sections.values())
      .filter(s => s.visible(state))
      .sort((a, b) => a.order - b.order)
  }

  renderAll(state: AppState, theme: Theme, width: number): string[] {
    const visible = this.getVisible(state)
    const lines: string[] = []

    for (const section of visible) {
      lines.push(...section.render(state, theme, width))
      lines.push('')
    }

    return lines
  }
}

// ─── Built-in sections ───

/// LLM status section
export const llmSection: PanelSection = {
  id: 'llm',
  title: 'LLM Status',
  order: 10,
  minHeight: 4,
  visible: () => true,
  render: (state, theme, width) => {
    const lines: string[] = []
    const maxW = Math.max(width - 4, 20)

    lines.push(`  ┌─ LLM ${'─'.repeat(Math.max(0, maxW - 5))}┐`)
    lines.push(`  │  Model: ${state.modelName}`.padEnd(maxW + 2) + '│')
    lines.push(`  │  Tokens: ${state.sessionTokens}`.padEnd(maxW + 2) + '│')

    const pct = state.config.maxTokens > 0
      ? ((state.sessionTokens / state.config.maxTokens) * 100).toFixed(0)
      : '0'
    lines.push(`  │  Usage: ${pct}%`.padEnd(maxW + 2) + '│')

    lines.push(`  └${'─'.repeat(maxW)}┘`)
    return lines
  },
}

/// Tools section
export const toolsSection: PanelSection = {
  id: 'tools',
  title: 'Tools',
  order: 20,
  minHeight: 3,
  visible: () => true,
  render: (state, theme, width) => {
    const lines: string[] = []
    const maxW = Math.max(width - 4, 20)

    lines.push(`  ┌─ Tools ${'─'.repeat(Math.max(0, maxW - 7))}┐`)

    if (state.streamingTools.length > 0) {
      for (const tool of state.streamingTools) {
        const icon = tool.status === 'Running' ? '⏳' : tool.status === 'Success' ? '✓' : '✗'
        const dur = tool.durationMs > 0 ? ` · ${tool.durationMs}ms` : ''
        // 工具来源标识
        const source = tool.name.startsWith('agent_') ? ' 🤖' :
                       tool.name.startsWith('mcp_') ? ' 🔌' :
                       tool.name.startsWith('skill_') ? ' ⚡' : ''
        lines.push(`  │  ${icon} ${tool.name}${source}${dur}`.padEnd(maxW + 2) + '│')
      }
    } else {
      lines.push(`  │  No active tools`.padEnd(maxW + 2) + '│')
    }

    lines.push(`  └${'─'.repeat(maxW)}┘`)
    return lines
  },
}

/// Timeline section
export const timelineSection: PanelSection = {
  id: 'timeline',
  title: 'Timeline',
  order: 30,
  minHeight: 3,
  visible: () => true,
  render: (state, _theme, width) => {
    const lines: string[] = []
    const maxW = Math.max(width - 4, 20)
    lines.push(`  ┌─ Timeline ${'─'.repeat(Math.max(0, maxW - 10))}┐`)

    const events: string[] = []

    for (const tool of state.streamingTools.slice(-3)) {
      const icon = tool.status === 'Running' ? '⏳' : tool.status === 'Success' ? '✓' : '✗'
      events.push(`${icon} tool ${tool.name}`)
    }

    for (const item of state.inputHistory.slice(-3)) {
      const preview = item.length > maxW - 12 ? item.slice(0, maxW - 15) + '...' : item
      events.push(`👤 ${preview}`)
    }

    for (const toast of state.toasts.slice(-2)) {
      events.push(`ℹ ${toast.message}`)
    }

    if (events.length === 0) {
      lines.push(`  │  No activity yet`.padEnd(maxW + 2) + '│')
    } else {
      for (const event of events.slice(-5)) {
        lines.push(`  │  ${event.slice(0, maxW - 4)}`.padEnd(maxW + 2) + '│')
      }
    }

    lines.push(`  └${'─'.repeat(maxW)}┘`)
    return lines
  },
}

/// Safety section
export const safetySection: PanelSection = {
  id: 'safety',
  title: 'Safety',
  order: 40,
  minHeight: 3,
  visible: () => true,
  render: (state, theme, width) => {
    const lines: string[] = []
    const maxW = Math.max(width - 4, 20)

    lines.push(`  ┌─ Safety ${'─'.repeat(Math.max(0, maxW - 8))}┐`)
    lines.push(`  │  MCIP: ${(state as any).mcipCount || 0}`.padEnd(maxW + 2) + '│')
    lines.push(`  │  Inertia: ${(state as any).inertiaCount || 0}`.padEnd(maxW + 2) + '│')
    lines.push(`  └${'─'.repeat(maxW)}┘`)
    return lines
  },
}

/// Agent section — 外部 Agent 状态
export const agentSection: PanelSection = {
  id: 'agent',
  title: 'Agents',
  order: 50,
  minHeight: 3,
  visible: () => true,
  render: (state, theme, width) => {
    const lines: string[] = []
    const maxW = Math.max(width - 4, 20)

    lines.push(`  ┌─ Agents ${'─'.repeat(Math.max(0, maxW - 8))}┐`)

    // Agent 列表（从 store 读取，如果有的话）
    const agents = (state as any).agents as Array<{
      id: string; connected: boolean; tools: number; health: string
    }> | undefined

    if (agents && agents.length > 0) {
      for (const agent of agents.slice(0, 5)) {
        const status = agent.connected ? '●' : '○'
        const health = agent.health === 'healthy' ? '✓' : agent.health === 'unreachable' ? '✗' : '?'
        const name = agent.id.slice(0, maxW - 14)
        lines.push(`  │  ${status} ${name} · ${agent.tools}t · ${health}`.padEnd(maxW + 2) + '│')
      }
    } else {
      lines.push(`  │  No agents installed`.padEnd(maxW + 2) + '│')
    }

    lines.push(`  └${'─'.repeat(maxW)}┘`)
    return lines
  },
}

/// Create default section registry
export function createDefaultSections(): SectionRegistry {
  const registry = new SectionRegistry()
  registry.register(llmSection)
  registry.register(toolsSection)
  registry.register(timelineSection)
  registry.register(safetySection)
  registry.register(agentSection)
  return registry
}
