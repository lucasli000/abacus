// components/overlays/InlineConfirm.ts — MCIP inline confirmation

import type { Theme } from '../../theme/types'

export interface ConfirmRequest {
  toolId: string
  reason: string
  kind: 'McipPolicy' | 'DestructiveOp'
  paramsPreview: string
  suggestedAction?: boolean
  createdAt: number
}

const AUTO_ALLOW_MS = 3000
const AUTO_DENY_MS = 8000

/// Render an inline confirmation box for MCIP
export function renderInlineConfirm(
  confirm: ConfirmRequest,
  theme: Theme,
  width: number,
): string[] {
  const maxW = Math.max(width - 4, 40)
  const lines: string[] = []
  const border = '─'.repeat(Math.min(maxW - 2, 56))

  lines.push(`  ╭${border}╮`)
  lines.push(`  │ 🔒 ${confirm.toolId}`.padEnd(maxW + 2) + '│')

  if (confirm.reason) {
    const reasonLines = wrapText(confirm.reason, maxW - 4)
    for (const line of reasonLines) {
      lines.push(`  │   ${line}`.padEnd(maxW + 2) + '│')
    }
  }

  if (confirm.paramsPreview) {
    const preview = confirm.paramsPreview.length > maxW - 6
      ? confirm.paramsPreview.slice(0, maxW - 9) + '...'
      : confirm.paramsPreview
    lines.push(`  │   ${preview}`.padEnd(maxW + 2) + '│')
  }

  lines.push(`  │`.padEnd(maxW + 2) + '│')
  lines.push(`  │   [A] Allow Once    [S] Always    [D] Deny`.padEnd(maxW + 2) + '│')

  // Auto-action countdown
  const elapsed = Date.now() - confirm.createdAt
  if (confirm.suggestedAction === true) {
    const remaining = Math.max(0, Math.ceil((AUTO_ALLOW_MS - elapsed) / 1000))
    if (remaining > 0) {
      lines.push(`  │   ⏱ Auto-allow in ${remaining}s`.padEnd(maxW + 2) + '│')
    }
  } else if (confirm.suggestedAction === false) {
    const remaining = Math.max(0, Math.ceil((AUTO_DENY_MS - elapsed) / 1000))
    if (remaining > 0) {
      lines.push(`  │   ⏱ Auto-deny in ${remaining}s`.padEnd(maxW + 2) + '│')
    }
  }

  lines.push(`  ╰${border}╯`)

  return lines
}

/// Render an inertia detection alert
export function renderInertiaAlert(
  signalType: string,
  message: string,
  attempt: number,
  theme: Theme,
  width: number,
): string[] {
  const maxW = Math.max(width - 4, 40)
  const lines: string[] = []
  const border = '─'.repeat(Math.min(maxW - 2, 56))

  const icons: Record<string, string> = {
    ToolAvoidance: '⚡',
    PrematureGiveUp: '⚡',
    IncompleteTask: '⚡',
    UncertaintyAvoidance: '⚡',
    ShallowResponse: '⚡',
  }
  const icon = icons[signalType] || '⚡'

  lines.push(`  ╭${border}╮`)
  lines.push(`  │ ${icon} ${message}`.padEnd(maxW + 2) + '│')

  if (attempt > 0) {
    lines.push(`  │   Auto-retry (${attempt}/2)`.padEnd(maxW + 2) + '│')
  }

  lines.push(`  ╰${border}╯`)

  return lines
}

/// Render a tool tier badge
export function renderTierBadge(tier: string): string {
  const badges: Record<string, string> = {
    S: '[S]',
    A: '[A]',
    B: '[B]',
    C: '[C]',
    D: '[D]',
  }
  return badges[tier] || ''
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
