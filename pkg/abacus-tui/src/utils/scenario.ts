// utils/scenario.ts — Scenario detection (Trading/Coding/Editing)

export type Scenario = 'trading' | 'coding' | 'editing'

export interface ScenarioConfig {
  scenario: Scenario
  topBarData: TopBarData
  shortcuts: ScenarioShortcut[]
  confirmStrategy: 'auto' | 'manual' | 'none'
  density: 'compact' | 'comfortable'
}

export interface TopBarData {
  primary: string
  secondary: string
  metrics: Array<{ label: string; value: string; color?: string }>
}

export interface ScenarioShortcut {
  key: string
  label: string
  action: string
}

/// Detect scenario from project context
export function detectScenario(projectPath?: string): Scenario {
  if (!projectPath) return 'coding'

  // Trading: has trading config, market data, financial files
  if (projectPath.includes('trading') || projectPath.includes('finance')) {
    return 'trading'
  }

  // Editing: has doc files, no code files
  if (projectPath.includes('docs') || projectPath.includes('wiki')) {
    return 'editing'
  }

  // Default: Coding
  return 'coding'
}

/// Get scenario-specific config
export function getScenarioConfig(scenario: Scenario): ScenarioConfig {
  switch (scenario) {
    case 'trading':
      return {
        scenario: 'trading',
        topBarData: {
          primary: 'Trading',
          secondary: 'Market Analysis',
          metrics: [
            { label: 'BTC', value: '$67,234', color: 'green' },
            { label: 'P&L', value: '+$12.5K', color: 'green' },
            { label: 'VaR', value: '$2.3K' },
          ],
        },
        shortcuts: [
          { key: 'F1', label: 'Analyze', action: 'analyze' },
          { key: 'F2', label: 'Buy', action: 'buy' },
          { key: 'F3', label: 'Sell', action: 'sell' },
          { key: 'F5', label: 'Refresh', action: 'refresh' },
        ],
        confirmStrategy: 'manual', // Never auto-confirm in trading
        density: 'compact',
      }

    case 'coding':
      return {
        scenario: 'coding',
        topBarData: {
          primary: 'Coding',
          secondary: 'Software Development',
          metrics: [
            { label: 'Branch', value: 'main' },
            { label: 'Tests', value: '42 ✓', color: 'green' },
            { label: 'Modified', value: '3 files' },
          ],
        },
        shortcuts: [
          { key: 'F5', label: 'Test', action: 'test' },
          { key: 'F6', label: 'Build', action: 'build' },
          { key: 'F7', label: 'Lint', action: 'lint' },
          { key: 'F9', label: 'Diff', action: 'diff' },
        ],
        confirmStrategy: 'auto', // Auto-confirm low-risk in coding
        density: 'comfortable',
      }

    case 'editing':
      return {
        scenario: 'editing',
        topBarData: {
          primary: 'Editing',
          secondary: 'Content Creation',
          metrics: [
            { label: 'Words', value: '2,340' },
            { label: 'Grade', value: 'A', color: 'green' },
            { label: 'Suggestions', value: '3' },
          ],
        },
        shortcuts: [
          { key: 'Ctrl+Shift+R', label: 'Polish', action: 'polish' },
          { key: 'Ctrl+Shift+C', label: 'Continue', action: 'continue' },
          { key: 'Ctrl+Shift+S', label: 'Summarize', action: 'summarize' },
        ],
        confirmStrategy: 'none', // No confirmations in editing
        density: 'comfortable',
      }
  }
}

/// Format scenario-specific TopBar content
export function formatScenarioTopBar(
  scenario: Scenario,
  mode: string,
  modelName: string,
  tokens: number,
  cost: number,
): string {
  const modeLabel = mode.charAt(0).toUpperCase() + mode.slice(1)

  switch (scenario) {
    case 'trading':
      return `  ● ${modeLabel}  ·  ${modelName}  ·  BTC $67,234 ▲+2.3%  ·  P&L +$12.5K  ·  $${cost.toFixed(2)}`

    case 'coding':
      return `  ● ${modeLabel}  ·  ${modelName}  ·  main ✓  ·  ${tokens} tok  ·  $${cost.toFixed(2)}`

    case 'editing':
      return `  ● ${modeLabel}  ·  ${modelName}  ·  2,340 words  ·  Grade A  ·  $${cost.toFixed(2)}`

    default:
      return `  ● ${modeLabel}  ·  ${modelName}  ·  ${tokens} tok  ·  $${cost.toFixed(2)}`
  }
}

/// Format scenario-specific status line
export function formatScenarioStatusLine(scenario: Scenario, mode: string, modelName: string): string {
  switch (scenario) {
    case 'trading':
      return `  ${mode} · ${modelName} · F1:analyze · F2:buy · F3:sell · F5:refresh · Ctrl+D:dashboard · Ctrl+C:exit`

    case 'coding':
      return `  ${mode} · ${modelName} · Enter:send · Ctrl+K:cmd · Ctrl+D:dashboard · F5:test · Ctrl+C:exit`

    case 'editing':
      return `  ${mode} · ${modelName} · Enter:send · Ctrl+Shift+R:polish · Ctrl+D:dashboard · Ctrl+C:exit`

    default:
      return `  ${mode} · ${modelName} · Enter:send · Ctrl+K:cmd · Ctrl+D:dashboard · Ctrl+C:exit`
  }
}
