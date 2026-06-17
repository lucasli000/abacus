// components/overlays/MemoryPalace.ts — Memory Palace visualization

import type { Theme } from '../../theme/types'

export interface MemoryState {
  behavior: {
    count: number
    max: number
    highFreqPatterns: Array<{ pattern: string; confidence: number }>
  }
  knowledge: {
    count: number
    max: number
    pendingReviews: Array<{ topic: string; nextReview: string }>
    relations: number
  }
}

/// Render Memory Palace overlay
export function renderMemoryPalace(
  memory: MemoryState,
  theme: Theme,
  width: number,
): string[] {
  const lines: string[] = []
  const maxW = Math.max(width - 4, 50)
  const border = '─'.repeat(Math.min(maxW - 2, 60))

  lines.push('')
  lines.push(`  ╭─ Memory Palace ${'─'.repeat(Math.max(0, border.length - 16))}╮`)
  lines.push('')

  // Behavior Palace
  lines.push(`  │  🧠 Behavior Palace ${memory.behavior.count}/${memory.behavior.max}`.padEnd(maxW + 2) + '│')
  lines.push(`  │`.padEnd(maxW + 2) + '│')

  if (memory.behavior.highFreqPatterns.length > 0) {
    lines.push(`  │  High-frequency patterns:`.padEnd(maxW + 2) + '│')
    for (const p of memory.behavior.highFreqPatterns.slice(0, 5)) {
      const conf = (p.confidence * 100).toFixed(0)
      const pattern = p.pattern.slice(0, maxW - 14)
      lines.push(`  │    ${pattern} (${conf}%)`.padEnd(maxW + 2) + '│')
    }
  } else {
    lines.push(`  │  No patterns recorded yet`.padEnd(maxW + 2) + '│')
  }

  lines.push(`  │`.padEnd(maxW + 2) + '│')

  // Knowledge Palace
  lines.push(`  │  📚 Knowledge Palace ${memory.knowledge.count}/${memory.knowledge.max}`.padEnd(maxW + 2) + '│')
  lines.push(`  │  Relations: ${memory.knowledge.relations}`.padEnd(maxW + 2) + '│')
  lines.push(`  │`.padEnd(maxW + 2) + '│')

  if (memory.knowledge.pendingReviews.length > 0) {
    lines.push(`  │  Pending reviews (SM-2):`.padEnd(maxW + 2) + '│')
    for (const r of memory.knowledge.pendingReviews.slice(0, 5)) {
      const topic = r.topic.slice(0, maxW - 20)
      lines.push(`  │    ${topic} · ${r.nextReview}`.padEnd(maxW + 2) + '│')
    }
  } else {
    lines.push(`  │  No pending reviews`.padEnd(maxW + 2) + '│')
  }

  lines.push(`  │`.padEnd(maxW + 2) + '│')
  lines.push(`  ╰${border}╯`)

  return lines
}
