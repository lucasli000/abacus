// commands/registry.ts — Slash command framework

import type { Theme } from '../theme/types'

export type CmdResult = 'consumed' | 'pending' | 'not_found'

export interface CommandHandler {
  names: string[]
  help: string
  tier: 0 | 1 | 2 | 3
  handler: (args: string[]) => CmdResult | Promise<CmdResult>
}

/// Command registry
export class CommandRegistry {
  private commands: CommandHandler[] = []

  register(cmd: CommandHandler): void {
    this.commands.push(cmd)
  }

  /// Dispatch a slash command
  async dispatch(input: string): Promise<{ result: CmdResult; command: string; args: string[] }> {
    if (!input.startsWith('/')) return { result: 'not_found', command: '', args: [] }

    const parts = input.slice(1).trim().split(/\s+/)
    const name = parts[0].toLowerCase()
    const args = parts.slice(1)

    const cmd = this.commands.find(c => c.names.includes(name))
    if (!cmd) return { result: 'not_found', command: name, args }

    const result = await cmd.handler(args)
    return { result, command: name, args }
  }

  /// Get all command names for completion
  getAllNames(): string[] {
    return this.commands.flatMap(c => c.names)
  }

  /// Get help text
  getHelp(all: boolean = false): string[] {
    const lines: string[] = []
    const tiers = all ? [0, 1, 2, 3] : [0, 1]

    for (const tier of tiers) {
      const cmds = this.commands.filter(c => c.tier === tier)
      if (cmds.length === 0) continue

      const tierLabel = ['', 'Basic', 'Core', 'Advanced', 'Diagnostic'][tier]
      lines.push(`  ${tierLabel}:`)

      for (const cmd of cmds) {
        const names = cmd.names.map(n => `/${n}`).join(', ')
        lines.push(`    ${names.padEnd(24)} ${cmd.help}`)
      }
      lines.push('')
    }

    return lines
  }

  /// Get commands matching a prefix (for completion)
  complete(prefix: string): string[] {
    const bare = prefix.startsWith('/') ? prefix.slice(1) : prefix
    return this.commands
      .flatMap(c => c.names)
      .filter(n => n.startsWith(bare))
      .map(n => `/${n}`)
  }
}

/// Create default command registry
export function createDefaultCommands(
  callbacks: {
    onClear: () => void
    onNew: () => void
    onSave: () => void
    onModel: () => void
    onTheme: () => void
    onMode: (mode: string) => void
    onHelp: (all: boolean) => void
    onDashboard: () => void
    onMemory: () => void
    onStatus: () => void
    onTokens: () => void
    onVersion: () => void
  }
): CommandRegistry {
  const registry = new CommandRegistry()

  // Tier 0: Basic
  registry.register({
    names: ['clear', 'cls'],
    help: 'Clear screen',
    tier: 0,
    handler: () => { callbacks.onClear(); return 'consumed' },
  })
  registry.register({
    names: ['new', 'reset'],
    help: 'New session',
    tier: 0,
    handler: () => { callbacks.onNew(); return 'consumed' },
  })
  registry.register({
    names: ['save'],
    help: 'Save session',
    tier: 0,
    handler: () => { callbacks.onSave(); return 'consumed' as CmdResult }
  })
  registry.register({
    names: ['quit', 'exit', 'q'],
    help: 'Exit',
    tier: 0,
    handler: () => { process.exit(0); return 'consumed' },
  })

  // Tier 1: Core
  registry.register({
    names: ['model', 'm'],
    help: 'Switch model',
    tier: 1,
    handler: () => { callbacks.onModel(); return 'consumed' },
  })
  registry.register({
    names: ['theme'],
    help: 'Switch theme',
    tier: 1,
    handler: () => { callbacks.onTheme(); return 'consumed' },
  })
  registry.register({
    names: ['plan'],
    help: 'Plan mode',
    tier: 1,
    handler: (args) => { callbacks.onMode('plan'); return 'consumed' },
  })
  registry.register({
    names: ['team'],
    help: 'Team mode',
    tier: 1,
    handler: (args) => { callbacks.onMode('team'); return 'consumed' },
  })
  registry.register({
    names: ['meeting'],
    help: 'Meeting mode',
    tier: 1,
    handler: () => { callbacks.onMode('meeting'); return 'consumed' },
  })
  registry.register({
    names: ['clarify', 'chat'],
    help: 'Clarify mode',
    tier: 1,
    handler: () => { callbacks.onMode('clarify'); return 'consumed' },
  })

  // Tier 2: Advanced
  registry.register({
    names: ['dashboard', 'dash'],
    help: 'Toggle dashboard',
    tier: 2,
    handler: () => { callbacks.onDashboard(); return 'consumed' },
  })
  registry.register({
    names: ['memory', 'mem'],
    help: 'Memory palace',
    tier: 2,
    handler: () => { callbacks.onMemory(); return 'consumed' },
  })
  registry.register({
    names: ['status'],
    help: 'Current status',
    tier: 2,
    handler: () => { callbacks.onStatus(); return 'consumed' },
  })
  registry.register({
    names: ['tokens', 'tok'],
    help: 'Token stats',
    tier: 2,
    handler: () => { callbacks.onTokens(); return 'consumed' },
  })

  // Tier 3: Diagnostic
  registry.register({
    names: ['version', 'v'],
    help: 'Version info',
    tier: 3,
    handler: () => { callbacks.onVersion(); return 'consumed' },
  })
  registry.register({
    names: ['help', 'h'],
    help: 'Show help',
    tier: 3,
    handler: (args) => { callbacks.onHelp(args.includes('all')); return 'consumed' },
  })

  return registry
}
