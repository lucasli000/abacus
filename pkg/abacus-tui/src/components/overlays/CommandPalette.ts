// components/overlays/CommandPalette.ts — Command palette (Ctrl+K)

import type { Theme } from '../../theme/types'

export interface CommandEntry {
  label: string
  description: string
  shortcut?: string
  action: string
  category: 'session' | 'mode' | 'settings' | 'tools' | 'help'
}

/// Built-in commands
export const COMMANDS: CommandEntry[] = [
  { label: 'Switch Model', description: 'Change LLM model', shortcut: '/model', action: 'model', category: 'settings' },
  { label: 'Change Thinking', description: 'Set thinking depth', shortcut: '/thinking', action: 'thinking', category: 'settings' },
  { label: 'Scene Preset', description: 'Apply preset (quick/code/creative)', shortcut: '/preset', action: 'preset', category: 'settings' },
  { label: 'Change Theme', description: 'Switch UI theme', shortcut: '/theme', action: 'theme', category: 'settings' },
  { label: 'New Session', description: 'Clear and start fresh', shortcut: 'Ctrl+N', action: 'new', category: 'session' },
  { label: 'Save Session', description: 'Save current session', shortcut: 'Ctrl+S', action: 'save', category: 'session' },
  { label: 'Toggle Panel', description: 'Show/hide side panel', shortcut: 'Ctrl+I', action: 'panel', category: 'settings' },
  { label: 'Dashboard', description: 'Toggle dashboard view', shortcut: 'Ctrl+D', action: 'dashboard', category: 'settings' },
  { label: 'Memory Palace', description: 'View memory state', shortcut: '/memory', action: 'memory', category: 'tools' },
  { label: 'Undo File Change', description: 'Undo last file operation', shortcut: '/undo', action: 'undo', category: 'tools' },
  { label: 'Safety Status', description: 'View safety state', shortcut: '/safety', action: 'safety', category: 'tools' },
  { label: 'Switch to Clarify', description: 'Clarify mode', shortcut: 'Ctrl+1', action: 'clarify', category: 'mode' },
  { label: 'Switch to Team', description: 'Team collaboration', shortcut: '/team', action: 'team', category: 'mode' },
  { label: 'Switch to Meeting', description: 'Expert consultation', shortcut: '/meeting', action: 'meeting', category: 'mode' },
  { label: 'Help', description: 'Show all commands', shortcut: '/help', action: 'help', category: 'help' },
]

const CATEGORY_LABELS: Record<string, string> = {
  session: '📋 Session',
  mode: '🔄 Mode',
  settings: '⚙ Settings',
  tools: '🔧 Tools',
  help: '❓ Help',
}

/// Render the command palette
export function renderCommandPalette(
  query: string,
  selectedIndex: number,
  theme: Theme,
  width: number,
  height: number,
): string[] {
  const maxW = Math.min(width - 8, 60)
  const lines: string[] = []

  // Filter commands
  const filtered = query
    ? COMMANDS.filter(cmd =>
        cmd.label.toLowerCase().includes(query.toLowerCase()) ||
        cmd.description.toLowerCase().includes(query.toLowerCase())
      )
    : COMMANDS

  // Group by category
  const grouped = new Map<string, CommandEntry[]>()
  for (const cmd of filtered) {
    const cat = cmd.category
    if (!grouped.has(cat)) grouped.set(cat, [])
    grouped.get(cat)!.push(cmd)
  }

  // Build lines
  const border = '─'.repeat(maxW - 2)
  lines.push(`  ╭${border}╮`)

  // Search input
  const searchLine = query ? `  🔍 ${query}` : '  🔍 Type to search...'
  lines.push(`  │${searchLine.padEnd(maxW - 2)}│`)
  lines.push(`  ├${border}┤`)

  // Commands
  let visibleCount = 0
  const maxVisible = height - 6 // border + search + separator + footer

  for (const [category, commands] of grouped) {
    if (visibleCount >= maxVisible) break

    // Category header
    const catLabel = CATEGORY_LABELS[category] || category
    lines.push(`  │ ${catLabel}`.padEnd(maxW + 2) + '│')
    visibleCount++

    for (const cmd of commands) {
      if (visibleCount >= maxVisible) break

      const isSelected = visibleCount === selectedIndex + 1
      const prefix = isSelected ? '  ▸ ' : '    '
      const shortcut = cmd.shortcut ? `  ${cmd.shortcut}` : ''

      const label = `${cmd.label}`.slice(0, maxW - shortcut.length - 8)
      const line = `${prefix}${label}${shortcut.padStart(maxW - label.length - 6)}`

      lines.push(`  │${line.padEnd(maxW - 2)}│`)
      visibleCount++
    }
  }

  // Footer
  lines.push(`  ├${border}┤`)
  lines.push(`  │  ↑↓ Navigate  Enter Execute  Esc Close`.padEnd(maxW + 2) + '│')
  lines.push(`  ╰${border}╯`)

  return lines
}

/// Get filtered commands for a query
export function getFilteredCommands(query: string): CommandEntry[] {
  if (!query) return COMMANDS
  return COMMANDS.filter(cmd =>
    cmd.label.toLowerCase().includes(query.toLowerCase()) ||
    cmd.description.toLowerCase().includes(query.toLowerCase())
  )
}
