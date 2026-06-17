// components/modes/PlanArea.ts — Plan mode

import type { Theme } from '../../theme/types'

export interface PlanState {
  strategy: string
  tasks: PlanTask[]
  status: 'draft' | 'awaiting_approval' | 'executing' | 'completed' | 'failed'
}

export interface PlanTask {
  id: string
  name: string
  status: 'pending' | 'running' | 'completed' | 'failed'
  dependencies?: string[]
}

/// Render Plan mode
export function renderPlanArea(
  plan: PlanState,
  theme: Theme,
  width: number,
  height: number,
): string[] {
  const lines: string[] = []
  const maxW = Math.max(width - 4, 40)
  const border = '─'.repeat(Math.min(maxW - 2, 60))

  // Strategy section
  lines.push(`  ╭─ Strategy ${'─'.repeat(Math.max(0, border.length - 10))}╮`)

  const strategyLines = wrapText(plan.strategy, maxW - 4)
  for (const line of strategyLines) {
    lines.push(`  │ ${line}`.padEnd(maxW + 2) + '│')
  }
  lines.push(`  ╰${border}╯`)
  lines.push('')

  // Tasks section
  lines.push(`  ╭─ Tasks ${'─'.repeat(Math.max(0, border.length - 8))}╮`)

  // Progress bar
  const completed = plan.tasks.filter(t => t.status === 'completed').length
  const total = plan.tasks.length
  const barW = Math.max(0, maxW - 12)
  const filled = total > 0 ? Math.round((completed / total) * barW) : 0
  const bar = '■'.repeat(filled) + '░'.repeat(barW - filled)
  lines.push(`  │ ${bar} ${completed}/${total}`.padEnd(maxW + 2) + '│')
  lines.push(`  │`.padEnd(maxW + 2) + '│')

  // Task list
  for (const task of plan.tasks) {
    const icon = task.status === 'completed' ? '✓' : task.status === 'running' ? '⏳' : task.status === 'failed' ? '✗' : '☐'
    const deps = task.dependencies?.length ? ` (deps: ${task.dependencies.join(', ')})` : ''
    const name = task.name.slice(0, maxW - 8 - deps.length)
    lines.push(`  │ ${icon} ${name}${deps}`.padEnd(maxW + 2) + '│')
  }

  lines.push(`  ╰${border}╯`)
  lines.push('')

  // Status
  const statusLabel = plan.status.charAt(0).toUpperCase() + plan.status.slice(1)
  lines.push(`  Status: ${statusLabel}`)

  if (plan.status === 'awaiting_approval') {
    lines.push('')
    lines.push('  [A] Auto execute  [S] Step-by-step  [T] Team  [N] Cancel')
  }

  return lines
}

function wrapText(text: string, maxW: number): string[] {
  if (text.length <= maxW) return [text]
  const lines: string[] = []
  let remaining = text
  while (remaining.length > maxW) {
    let breakAt = maxW
    const spaceIdx = remaining.lastIndexOf(' ', maxW)
    if (spaceIdx > maxW * 0.5) breakAt = spaceIdx
    lines.push(remaining.slice(0, breakAt))
    remaining = remaining.slice(breakAt).trimStart()
  }
  if (remaining) lines.push(remaining)
  return lines
}
