// components/modes/MeetingArea.ts — Meeting mode (three-column layout)

import type { Theme } from '../../theme/types'
import type { Card } from '../cards/types'

export interface MeetingState {
  topic: string
  experts: MeetingExpert[]
  agenda: MeetingAgenda
  conclusion?: string
  actionItems: string[]
  cost: number
  durationMs: number
}

export interface MeetingExpert {
  id: string
  name: string
  icon: string
  color: string
  status: 'idle' | 'speaking' | 'waiting'
  lastOpinion?: string
}

export interface MeetingAgenda {
  topics: string[]
  currentTopic: number
  decisions: string[]
}

/// Render Meeting mode three-column layout
export function renderMeetingArea(
  meeting: MeetingState,
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

  // Experts column (left)
  const expertLines = renderExperts(meeting.experts, theme, leftW)

  // Conversation column (center)
  const convLines = renderConversation(cards, theme, centerW)

  // Agenda column (right)
  const agendaLines = renderAgenda(meeting, theme, rightW)

  // Merge columns
  const maxLines = Math.max(expertLines.length, convLines.length, agendaLines.length, height)
  for (let i = 0; i < Math.min(maxLines, height); i++) {
    const left = (expertLines[i] || '').padEnd(leftW)
    const center = (convLines[i] || '').padEnd(centerW)
    const right = agendaLines[i] || ''
    lines.push(`${left}  ${center}  ${right}`)
  }

  return lines
}

function renderExperts(experts: MeetingExpert[], theme: Theme, width: number): string[] {
  const lines: string[] = []
  const border = '─'.repeat(Math.max(0, width - 2))

  lines.push(`┌─ Experts ${'─'.repeat(Math.max(0, border.length - 9))}┐`)

  for (const expert of experts) {
    const statusIcon = expert.status === 'speaking' ? '🗣' : expert.status === 'waiting' ? '⏸' : '○'
    lines.push(`│ ${expert.icon} ${expert.name}`.padEnd(width + 1) + '│')
    lines.push(`│   ${statusIcon} ${expert.status}`.padEnd(width + 1) + '│')

    if (expert.lastOpinion) {
      const preview = expert.lastOpinion.slice(0, width - 6)
      lines.push(`│   "${preview}"`.padEnd(width + 1) + '│')
    }
  }

  lines.push(`└${border}┘`)
  return lines
}

function renderConversation(cards: Card[], theme: Theme, width: number): string[] {
  const lines: string[] = []
  const border = '─'.repeat(Math.max(0, width - 2))

  lines.push(`┌─ Conversation ${'─'.repeat(Math.max(0, border.length - 14))}┐`)

  const recentCards = cards.slice(-8)
  for (const card of recentCards) {
    if (card.kind === 'expert') {
      const name = card.expertName || 'Expert'
      lines.push(`│ ${name}`.padEnd(width + 1) + '│')
      const content = card.content.slice(0, width - 4)
      lines.push(`│   ${content}`.padEnd(width + 1) + '│')
    } else if (card.kind === 'user') {
      lines.push(`│ You`.padEnd(width + 1) + '│')
      const content = card.content.slice(0, width - 4)
      lines.push(`│   ${content}`.padEnd(width + 1) + '│')
    }
    lines.push(`│`.padEnd(width + 1) + '│')
  }

  if (recentCards.length === 0) {
    lines.push(`│  No conversation yet`.padEnd(width + 1) + '│')
  }

  lines.push(`└${border}┘`)
  return lines
}

function renderAgenda(meeting: MeetingState, theme: Theme, width: number): string[] {
  const lines: string[] = []
  const border = '─'.repeat(Math.max(0, width - 2))

  lines.push(`┌─ Agenda ${'─'.repeat(Math.max(0, border.length - 8))}┐`)

  // Topic
  lines.push(`│ Topic: ${meeting.topic.slice(0, width - 10)}`.padEnd(width + 1) + '│')
  lines.push(`│`.padEnd(width + 1) + '│')

  // Topics list
  for (let i = 0; i < meeting.agenda.topics.length; i++) {
    const icon = i === meeting.agenda.currentTopic ? '▸' : i < meeting.agenda.currentTopic ? '✓' : '○'
    const topic = meeting.agenda.topics[i].slice(0, width - 6)
    lines.push(`│ ${icon} ${topic}`.padEnd(width + 1) + '│')
  }

  lines.push(`│`.padEnd(width + 1) + '│')

  // Decisions
  if (meeting.agenda.decisions.length > 0) {
    lines.push(`│ Decisions:`.padEnd(width + 1) + '│')
    for (const decision of meeting.agenda.decisions.slice(0, 3)) {
      const d = decision.slice(0, width - 4)
      lines.push(`│   • ${d}`.padEnd(width + 1) + '│')
    }
    lines.push(`│`.padEnd(width + 1) + '│')
  }

  // Conclusion
  if (meeting.conclusion) {
    lines.push(`│ Conclusion:`.padEnd(width + 1) + '│')
    const c = meeting.conclusion.slice(0, width - 4)
    lines.push(`│   ${c}`.padEnd(width + 1) + '│')
    lines.push(`│`.padEnd(width + 1) + '│')
  }

  // Action items
  if (meeting.actionItems.length > 0) {
    lines.push(`│ Action items:`.padEnd(width + 1) + '│')
    for (const item of meeting.actionItems.slice(0, 5)) {
      const i = item.slice(0, width - 6)
      lines.push(`│   ☐ ${i}`.padEnd(width + 1) + '│')
    }
    lines.push(`│`.padEnd(width + 1) + '│')
  }

  // Cost
  lines.push(`│ Cost: $${meeting.cost.toFixed(2)}`.padEnd(width + 1) + '│')
  lines.push(`│ Duration: ${formatDuration(meeting.durationMs)}`.padEnd(width + 1) + '│')

  lines.push(`└${border}┘`)
  return lines
}

function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`
  if (ms < 60000) return `${(ms / 1000).toFixed(1)}s`
  return `${Math.floor(ms / 60000)}m ${Math.floor((ms % 60000) / 1000)}s`
}
