// components/overlays/UndoTimeline.ts — Undo timeline visualization

import type { Theme } from '../../theme/types'

export interface UndoEntry {
  seq: number
  turn: number
  tool: string
  filePath: string
  opKind: 'Create' | 'Overwrite' | 'Move' | 'Mkdir'
  timestamp: number
}

/// Render Undo timeline overlay
export function renderUndoTimeline(
  entries: UndoEntry[],
  theme: Theme,
  width: number,
): string[] {
  const lines: string[] = []
  const maxW = Math.max(width - 4, 50)
  const border = '─'.repeat(Math.min(maxW - 2, 60))

  lines.push('')
  lines.push(`  ╭─ Undo Timeline ${'─'.repeat(Math.max(0, border.length - 16))}╮`)
  lines.push('')

  if (entries.length === 0) {
    lines.push(`  │  No file operations recorded`.padEnd(maxW + 2) + '│')
  } else {
    for (const entry of entries.slice(0, 15)) {
      const d = new Date(entry.timestamp)
      const time = `${d.getHours().toString().padStart(2, '0')}:${d.getMinutes().toString().padStart(2, '0')}`
      const icon = entry.opKind === 'Create' ? '📄' : entry.opKind === 'Overwrite' ? '✏️' : entry.opKind === 'Move' ? '📦' : '📁'
      const file = entry.filePath.slice(0, maxW - 20)
      lines.push(`  │  ${icon} Turn ${entry.turn} · ${entry.tool} · ${file} · ${time}`.padEnd(maxW + 2) + '│')
      lines.push(`  │     [undo]`.padEnd(maxW + 2) + '│')
    }
  }

  lines.push(`  │`.padEnd(maxW + 2) + '│')
  lines.push(`  │  Commands: /undo [seq|turn]  /redo  /history`.padEnd(maxW + 2) + '│')
  lines.push(`  ╰${border}╯`)

  return lines
}
