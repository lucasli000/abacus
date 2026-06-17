// components/cards/renderer.ts — Card rendering (polished)

import type { Card, ToolCallInfo, ToolTier } from './types'
import type { Theme } from '../../theme/types'

const TIER_BADGES: Record<ToolTier, string> = {
  S: '[S]',
  A: '[A]',
  B: '[B]',
  C: '[C]',
  D: '[D]',
}

const STATUS_ICONS: Record<string, string> = {
  Running: '⏳',
  Success: '✓',
  Failed: '✗',
}

export interface RenderedCard {
  lines: string[]
  totalHeight: number
  cardId: string
}

/// Render a single card to displayable lines
export function renderCard(card: Card, theme: Theme, width: number): RenderedCard {
  const maxW = Math.max(width - 4, 40) // 4 = border + padding
  const lines: string[] = []

  switch (card.kind) {
    case 'user':
      lines.push(...renderUserCard(card, theme, maxW))
      break
    case 'llm':
      lines.push(...renderLlmCard(card, theme, maxW))
      break
    case 'abacus':
      lines.push(...renderAbacusCard(card, theme, maxW))
      break
    case 'thinking':
      lines.push(...renderThinkingCard(card, theme, maxW))
      break
    case 'expert':
      lines.push(...renderExpertCard(card, theme, maxW))
      break
  }

  return {
    lines,
    totalHeight: lines.length,
    cardId: card.id,
  }
}

function renderUserCard(card: Card, theme: Theme, maxW: number): string[] {
  const time = formatTime(card.timestamp)
  const lines: string[] = []

  // Header: right-aligned time
  const header = 'You'
  const padding = Math.max(0, maxW - header.length - time.length - 2)
  lines.push(`  ${header}${' '.repeat(padding)}${time}`)

  // Content with proper indentation
  const contentLines = wrapText(card.content, maxW - 2)
  for (const line of contentLines) {
    lines.push(`  ${line}`)
  }

  return lines
}

function renderLlmCard(card: Card, theme: Theme, maxW: number): string[] {
  const time = formatTime(card.timestamp)
  const lines: string[] = []

  // Header
  const header = 'Claude'
  const padding = Math.max(0, maxW - header.length - time.length - 2)
  lines.push(`  ${header}${' '.repeat(padding)}${time}`)

  // Thinking (collapsible)
  if (card.thinking) {
    const thinkingLines = card.thinking.split('\n').filter(l => l.trim())
    const count = thinkingLines.length

    if (card.collapsed) {
      lines.push(`  ▸ thinking · ${count} line${count !== 1 ? 's' : ''}`)
    } else {
      lines.push(`  ▾ thinking · ${count} line${count !== 1 ? 's' : ''}`)
      for (const line of thinkingLines.slice(0, 10)) {
        lines.push(`    ${line.slice(0, maxW - 4)}`)
      }
      if (count > 10) {
        lines.push(`    ... +${count - 10} more lines`)
      }
    }
    lines.push('')
  }

  // Content
  if (card.content) {
    const contentLines = wrapText(card.content, maxW - 2)
    for (const line of contentLines) {
      lines.push(`  ${line}`)
    }
  }

  // Tool calls summary
  if (card.toolCalls?.length) {
    lines.push('')
    for (const tc of card.toolCalls) {
      lines.push(formatToolCall(tc, maxW))
    }
  }

  return lines
}

function renderAbacusCard(card: Card, theme: Theme, maxW: number): string[] {
  const lines: string[] = []

  if (!card.toolCalls?.length) return lines

  if (card.collapsed) {
    // Summary line
    const names = card.toolCalls.map(tc => tc.name).join(', ')
    const allSuccess = card.toolCalls.every(tc => tc.status === 'Success')
    const anyFailed = card.toolCalls.some(tc => tc.status === 'Failed')
    const status = anyFailed ? '✗' : allSuccess ? '✓' : '⏳'
    const summary = names.length > maxW - 10 ? names.slice(0, maxW - 13) + '...' : names
    lines.push(`  ▸ ⚙ ${summary} · ${status}`)
  } else {
    // Expanded
    for (const tc of card.toolCalls) {
      lines.push(formatToolCall(tc, maxW))

      // Show args preview if available
      if (tc.args && tc.args.length > 0) {
        try {
          const parsed = JSON.parse(tc.args)
          const preview = JSON.stringify(parsed).slice(0, maxW - 8)
          lines.push(`    args: ${preview}`)
        } catch {
          // Not valid JSON, skip
        }
      }

      // Show output preview
      if (tc.output && !card.collapsed) {
        const outputLines = tc.output.split('\n').slice(0, 3)
        for (const line of outputLines) {
          lines.push(`    ${line.slice(0, maxW - 4)}`)
        }
        if (tc.output.split('\n').length > 3) {
          lines.push(`    ... +${tc.output.split('\n').length - 3} more`)
        }
      }
    }
  }

  return lines
}

function renderThinkingCard(card: Card, theme: Theme, maxW: number): string[] {
  const lines: string[] = []
  const thinkingLines = card.thinking?.split('\n').filter(l => l.trim()) || []
  const count = thinkingLines.length

  if (card.collapsed) {
    lines.push(`  ▸ thinking · ${count} line${count !== 1 ? 's' : ''}`)
  } else {
    lines.push(`  ▾ thinking · ${count} line${count !== 1 ? 's' : ''}`)
    for (const line of thinkingLines.slice(0, 15)) {
      lines.push(`    ${line.slice(0, maxW - 4)}`)
    }
    if (count > 15) {
      lines.push(`    ... +${count - 15} more lines`)
    }
  }

  return lines
}

function renderExpertCard(card: Card, theme: Theme, maxW: number): string[] {
  const time = formatTime(card.timestamp)
  const name = card.expertName || 'Expert'
  const lines: string[] = []

  // Header
  const padding = Math.max(0, maxW - name.length - time.length - 2)
  lines.push(`  ${name}${' '.repeat(padding)}${time}`)

  // Thinking
  if (card.thinking) {
    const thinkingLines = card.thinking.split('\n').filter(l => l.trim()).length
    lines.push(`  ▸ thinking · ${thinkingLines} lines`)
  }

  // Content
  if (card.content) {
    const contentLines = wrapText(card.content, maxW - 2)
    for (const line of contentLines) {
      lines.push(`  ${line}`)
    }
  }

  return lines
}

function formatToolCall(tc: ToolCallInfo, maxW: number): string {
  const icon = STATUS_ICONS[tc.status] || '?'
  const tier = tc.tier ? ` ${TIER_BADGES[tc.tier]}` : ''
  const duration = tc.durationMs > 0 ? ` · ${formatDuration(tc.durationMs)}` : ''
  const file = tc.filePath ? ` · ${tc.filePath}` : ''

  const name = tc.name
  const suffix = `${tier}${file} · ${icon}${duration}`
  const available = maxW - 4 - name.length - suffix.length

  if (available >= 0) {
    return `  ⚙ ${name}${suffix}`
  } else {
    // Truncate file path
    const truncatedFile = file.length > 10 ? file.slice(0, 7) + '...' : file
    return `  ⚙ ${name}${tier}${truncatedFile} · ${icon}${duration}`
  }
}

function formatTime(ts: number): string {
  const d = new Date(ts)
  return `${d.getHours().toString().padStart(2, '0')}:${d.getMinutes().toString().padStart(2, '0')}`
}

function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`
  if (ms < 60000) return `${(ms / 1000).toFixed(1)}s`
  return `${Math.floor(ms / 60000)}m ${Math.floor((ms % 60000) / 1000)}s`
}

function wrapText(text: string, maxW: number): string[] {
  if (text.length <= maxW) return [text]

  const lines: string[] = []
  const paragraphs = text.split('\n')

  for (const para of paragraphs) {
    if (para.length <= maxW) {
      lines.push(para)
      continue
    }

    let remaining = para
    while (remaining.length > maxW) {
      let breakAt = maxW
      // Find a good break point (space, hyphen, or forced)
      const spaceIdx = remaining.lastIndexOf(' ', maxW)
      const hyphenIdx = remaining.lastIndexOf('-', maxW)
      if (spaceIdx > maxW * 0.4) breakAt = spaceIdx + 1
      else if (hyphenIdx > maxW * 0.6) breakAt = hyphenIdx + 1

      lines.push(remaining.slice(0, breakAt).trimEnd())
      remaining = remaining.slice(breakAt).trimStart()
    }
    if (remaining) lines.push(remaining)
  }

  return lines
}
