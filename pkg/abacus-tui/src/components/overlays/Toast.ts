// components/overlays/Toast.ts — Toast notification renderer

import type { Theme } from '../../theme/types'

export interface Toast {
  id: string
  message: string
  type: 'info' | 'success' | 'warning' | 'error'
  createdAt: number
  duration: number
}

const TYPE_ICONS: Record<string, string> = {
  info: 'ℹ',
  success: '✓',
  warning: '⚠',
  error: '✗',
}

/// Render a single toast notification
export function renderToast(toast: Toast, theme: Theme, width: number): string[] {
  const maxW = Math.min(width - 4, 50)
  const icon = TYPE_ICONS[toast.type] || 'ℹ'
  const message = toast.message.length > maxW - 4
    ? toast.message.slice(0, maxW - 7) + '...'
    : toast.message

  const border = '─'.repeat(Math.min(maxW - 2, message.length + 4))

  return [
    `  ╭${border}╮`,
    `  │ ${icon} ${message}`.padEnd(maxW + 2) + '│',
    `  ╰${border}╯`,
  ]
}

/// Render stacked toasts (bottom-right)
export function renderToasts(
  toasts: Toast[],
  theme: Theme,
  terminalWidth: number,
  terminalHeight: number,
): string[] {
  if (toasts.length === 0) return []

  const lines: string[] = []
  const maxVisible = 3
  const visible = toasts.slice(-maxVisible)

  for (const toast of visible) {
    lines.push(...renderToast(toast, theme, terminalWidth))
  }

  return lines
}
