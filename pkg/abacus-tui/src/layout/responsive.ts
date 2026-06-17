// layout/responsive.ts — Responsive layout calculation

export interface LayoutConfig {
  messageArea: number
  panel: number        // 0 = hidden
  dashboard: number    // 0 = hidden
  inputBar: number
  codeBlockMax: number
  markdownMax: number
  threeColSupported: boolean
}

export type LayoutBreakpoint = 'narrow' | 'standard' | 'wide' | 'ultrawide'

export function classifyWidth(W: number): LayoutBreakpoint {
  if (W < 90) return 'narrow'
  if (W < 120) return 'standard'
  if (W < 160) return 'wide'
  return 'ultrawide'
}

export function computeLayout(
  W: number,
  hasPanel: boolean,
  hasDashboard: boolean,
): LayoutConfig {
  // Narrow: W < 90
  if (W < 90) {
    return {
      messageArea: W - 2,
      panel: 0,
      dashboard: hasDashboard ? W - 2 : 0,
      inputBar: W - 2,
      codeBlockMax: Math.min(W - 4, 100),
      markdownMax: Math.min(W - 6, 80),
      threeColSupported: false,
    }
  }

  // Standard: 90-120
  if (W < 120) {
    const panelW = hasPanel ? Math.floor(W * 0.25) : 0
    const msgW = W - panelW - 2
    return {
      messageArea: msgW,
      panel: panelW,
      dashboard: hasDashboard ? W - 2 : 0,
      inputBar: W - 2,
      codeBlockMax: Math.min(msgW - 4, 100),
      markdownMax: Math.min(msgW - 6, 80),
      threeColSupported: W >= 100,
    }
  }

  // Wide: 120-160
  if (W < 160) {
    const panelW = hasPanel ? Math.floor(W * 0.28) : 0
    const dashW = hasDashboard ? Math.floor(W * 0.40) : 0
    const msgW = W - panelW - dashW - 2
    return {
      messageArea: msgW,
      panel: panelW,
      dashboard: dashW,
      inputBar: W - 2,
      codeBlockMax: Math.min(msgW - 4, 100),
      markdownMax: Math.min(msgW - 6, 80),
      threeColSupported: true,
    }
  }

  // Ultrawide: 160+
  const panelW = hasPanel ? Math.floor(W * 0.25) : 0
  const dashW = Math.floor(W * 0.25)
  const msgW = W - panelW - dashW - 3
  return {
    messageArea: msgW,
    panel: panelW,
    dashboard: dashW,
    inputBar: W - 2,
    codeBlockMax: Math.min(msgW - 4, 100),
    markdownMax: Math.min(msgW - 6, 80),
    threeColSupported: true,
  }
}

/// Three-column layout for Team/Meeting modes
export function computeThreeColLayout(
  W: number,
): { left: number; center: number; right: number } {
  if (W < 90) {
    // Degraded: single column with tabs
    return { left: W - 2, center: 0, right: 0 }
  }
  if (W < 120) {
    return { left: 18, center: W - 18 - 24 - 2, right: 24 }
  }
  if (W < 160) {
    return { left: 24, center: W - 24 - 36 - 2, right: 36 }
  }
  return { left: 30, center: Math.floor(W * 0.45), right: W - 30 - Math.floor(W * 0.45) - 2 }
}
