// components/modes/TeamArea.ts — Team mode (three-column layout)

import type { Theme } from '../../theme/types'
import type { Card } from '../cards/types'

export interface TeamState {
  goal: string
  phase: 'idle' | 'planning' | 'executing' | 'reviewing' | 'completed'
  agents: TeamAgent[]
  tasks: TeamTask[]
  cost: { leader: number; member: number; advisor: number; total: number }
}

export interface TeamAgent {
  id: string
  role: 'Leader' | 'Member' | 'Advisor'
  status: 'idle' | 'executing' | 'waiting'
  currentTool?: string
  icon: string
}

export interface TeamTask {
  id: string
  name: string
  status: 'pending' | 'running' | 'completed' | 'failed'
  assignee?: string
  cost?: number
  durationMs?: number
}

/// Render Team mode three-column layout
export function renderTeamArea(
  team: TeamState,
  cards: Card[],
  theme: Theme,
  totalWidth: number,
  height: number,
): string[] {
  const lines: string[] = []

  // Column widths (20/55/25)
  const leftW = Math.floor(totalWidth * 0.20)
  const rightW = Math.floor(totalWidth * 0.25)
  const centerW = totalWidth - leftW - rightW - 4

  // Roles column (left)
  const roleLines = renderRoles(team.agents, theme, leftW)

  // Kanban column (center)
  const kanbanLines = renderKanban(team, theme, centerW)

  // Interaction column (right)
  const interactLines = renderInteraction(team, cards, theme, rightW)

  // Merge columns side by side
  const maxLines = Math.max(roleLines.length, kanbanLines.length, interactLines.length, height)
  for (let i = 0; i < Math.min(maxLines, height); i++) {
    const left = (roleLines[i] || '').padEnd(leftW)
    const center = (kanbanLines[i] || '').padEnd(centerW)
    const right = interactLines[i] || ''
    lines.push(`${left}  ${center}  ${right}`)
  }

  return lines
}

function renderRoles(agents: TeamAgent[], theme: Theme, width: number): string[] {
  const lines: string[] = []
  const border = '─'.repeat(Math.max(0, width - 2))

  lines.push(`┌─ Roles ${'─'.repeat(Math.max(0, border.length - 7))}┐`)

  for (const agent of agents) {
    const statusIcon = agent.status === 'executing' ? '⏳' : agent.status === 'waiting' ? '⏸' : '○'
    const toolInfo = agent.currentTool ? ` · ${agent.currentTool}` : ''
    lines.push(`│ ${agent.icon} ${agent.role}`.padEnd(width + 1) + '│')
    lines.push(`│   ${statusIcon}${toolInfo}`.padEnd(width + 1) + '│')
  }

  lines.push(`└${border}┘`)
  return lines
}

function renderKanban(team: TeamState, theme: Theme, width: number): string[] {
  const lines: string[] = []
  const border = '─'.repeat(Math.max(0, width - 2))

  lines.push(`┌─ Tasks ${'─'.repeat(Math.max(0, border.length - 7))}┐`)

  // Phase indicator
  const phaseLabel = team.phase.charAt(0).toUpperCase() + team.phase.slice(1)
  lines.push(`│ Phase: ${phaseLabel}`.padEnd(width + 1) + '│')
  lines.push(`│`.padEnd(width + 1) + '│')

  // Progress bar
  const completed = team.tasks.filter(t => t.status === 'completed').length
  const total = team.tasks.length
  const barW = Math.max(0, width - 12)
  const filled = total > 0 ? Math.round((completed / total) * barW) : 0
  const bar = '■'.repeat(filled) + '░'.repeat(barW - filled)
  lines.push(`│ ${bar} ${completed}/${total}`.padEnd(width + 1) + '│')
  lines.push(`│`.padEnd(width + 1) + '│')

  // Tasks
  for (const task of team.tasks.slice(0, 10)) {
    const icon = task.status === 'completed' ? '✓' : task.status === 'running' ? '⏳' : task.status === 'failed' ? '✗' : '☐'
    const assignee = task.assignee ? ` · ${task.assignee}` : ''
    const cost = task.cost ? ` · $${task.cost.toFixed(2)}` : ''
    const dur = task.durationMs ? ` · ${formatDuration(task.durationMs)}` : ''
    const name = task.name.slice(0, width - 14)
    lines.push(`│ ${icon} ${name}${assignee}${cost}${dur}`.padEnd(width + 1) + '│')
  }

  lines.push(`└${border}┘`)
  return lines
}

function renderInteraction(team: TeamState, cards: Card[], theme: Theme, width: number): string[] {
  const lines: string[] = []
  const border = '─'.repeat(Math.max(0, width - 2))

  lines.push(`┌─ Activity ${'─'.repeat(Math.max(0, border.length - 10))}┐`)

  // Recent agent activity
  const recentCards = cards.slice(-5)
  for (const card of recentCards) {
    const icon = card.kind === 'user' ? '👤' : '🤖'
    const preview = card.content.slice(0, width - 6)
    lines.push(`│ ${icon} ${preview}`.padEnd(width + 1) + '│')
  }

  if (recentCards.length === 0) {
    lines.push(`│  No activity`.padEnd(width + 1) + '│')
  }

  lines.push(`│`.padEnd(width + 1) + '│')

  // Cost summary
  lines.push(`│ Cost:`.padEnd(width + 1) + '│')
  lines.push(`│   Leader:  $${team.cost.leader.toFixed(2)}`.padEnd(width + 1) + '│')
  lines.push(`│   Member:  $${team.cost.member.toFixed(2)}`.padEnd(width + 1) + '│')
  lines.push(`│   Advisor: $${team.cost.advisor.toFixed(2)}`.padEnd(width + 1) + '│')
  lines.push(`│   Total:   $${team.cost.total.toFixed(2)}`.padEnd(width + 1) + '│')

  lines.push(`└${border}┘`)
  return lines
}

function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`
  if (ms < 60000) return `${(ms / 1000).toFixed(1)}s`
  return `${Math.floor(ms / 60000)}m ${Math.floor((ms % 60000) / 1000)}s`
}
