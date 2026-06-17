// components/App.ts — Root component (polished)

import { createCliRenderer, BoxRenderable, TextRenderable } from '@opentui/core'
import type { CliRenderer } from '@opentui/core'
import { store, getState, subscribe } from '../state/store'
import type { Theme } from '../theme/types'
import { getThemeByName } from '../theme/palettes'
import { computeLayout } from '../layout/responsive'
import { Engine } from '../bridge/engine'
import type { StreamEvent, StreamChunk } from '../bridge/types'
import { CardStream } from './cards/types'
import { renderCard } from './cards/renderer'
import { renderDashboard } from './Dashboard'
import { renderCommandPalette } from './overlays/CommandPalette'
import { renderToast } from './overlays/Toast'
import { SectionRegistry, createDefaultSections } from './panel/SectionRegistry'
import { CommandRegistry, createDefaultCommands } from '../commands/registry'
import { detectScenario, getScenarioConfig, formatScenarioTopBar, formatScenarioStatusLine } from '../utils/scenario'
import type { Scenario, ScenarioConfig } from '../utils/scenario'
import { renderTeamArea, type TeamState } from './modes/TeamArea'
import { renderMeetingArea, type MeetingState } from './modes/MeetingArea'
import { renderPlanArea, type PlanState } from './modes/PlanArea'

type OverlayMode = 'none' | 'dashboard' | 'command_palette'

export class App {
  private renderer: CliRenderer
  private engine: Engine
  private theme!: Theme
  private cardStream: CardStream
  private sections: SectionRegistry
  private commands: CommandRegistry
  private scenario!: Scenario
  private scenarioConfig!: ScenarioConfig

  // Overlay
  private overlay: OverlayMode = 'none'
  private commandQuery = ''
  private commandSelectedIndex = 0

  // Renderables
  private topBar!: BoxRenderable
  private topBarText!: TextRenderable
  private chatArea!: BoxRenderable
  private chatContent!: TextRenderable
  private inputBar!: BoxRenderable
  private inputText!: TextRenderable
  private statusLine!: TextRenderable
  private toastArea!: TextRenderable

  // Scroll
  private scrollOffset = 0
  private totalLines = 0
  private visibleHeight = 0
  private autoFollow = true

  // Performance: debounce rendering
  private renderPending = false

  constructor(renderer: CliRenderer, engine: Engine) {
    this.renderer = renderer
    this.engine = engine
    this.cardStream = new CardStream()
    this.sections = createDefaultSections()
    this.commands = this.createCommands()

    // Initialize theme and scenario BEFORE building UI
    this.theme = getThemeByName(getState().themeName)
    this.scenario = detectScenario()
    this.scenarioConfig = getScenarioConfig(this.scenario)

    // Sync scenario to store
    store.setState({ scenario: this.scenario })

    this.buildUI()
    this.bindEngine()
    this.bindKeys()
    this.bindStore()
    this.startToastCleanup()
  }

  // ─── Commands ───

  private createCommands(): CommandRegistry {
    return createDefaultCommands({
      onClear: () => {
        this.cardStream.clear()
        this.scrollOffset = 0
        this.requestRender()
      },
      onNew: () => {
        this.cardStream.clear()
        const s = getState()
        s.resetStreamingTools()
        s.setInput('')
        s.setInputState('ready')
        this.scrollOffset = 0
        this.autoFollow = true
        this.requestRender()
        s.addToast({ message: 'New session', type: 'info', duration: 1500 })
      },
      onSave: () => {
        getState().addToast({ message: 'Session saved', type: 'success', duration: 2000 })
      },
      onModel: () => {
        getState().addToast({ message: 'Model picker — use /model <name>', type: 'info', duration: 3000 })
      },
      onTheme: () => {
        const themes = ['abacus-dark', 'nord', 'tokyo-night', 'catppuccin', 'dracula', 'gruvbox']
        const current = getState().themeName
        const idx = themes.indexOf(current)
        const next = themes[(idx + 1) % themes.length]
        this.theme = getThemeByName(next)
        store.setState({ themeName: next })
        this.rebuildUI()
        getState().addToast({ message: `Theme: ${next}`, type: 'info', duration: 1500 })
      },
      onMode: (mode) => {
        getState().setMode(mode as any)
        getState().addToast({ message: `Switched to ${mode}`, type: 'info', duration: 1500 })
      },
      onHelp: (all) => {
        const helpLines = this.commands.getHelp(all)
        // Show help as a temporary card
        this.cardStream.addUserCard('/help', getState().mode)
        const helpCard = this.cardStream.ensureActive('llm', getState().mode)
        this.cardStream.appendText(helpLines.join('\n'))
        this.cardStream.finishActive()
        this.requestRender()
      },
      onDashboard: () => {
        this.toggleOverlay('dashboard')
      },
      onMemory: () => {
        getState().addToast({ message: 'Memory: /memory to view', type: 'info', duration: 2000 })
      },
      onStatus: () => {
        const s = getState()
        getState().addToast({
          message: `${s.mode} · ${s.modelName} · ${s.sessionTokens} tok`,
          type: 'info',
          duration: 3000,
        })
      },
      onTokens: () => {
        const s = getState()
        getState().addToast({
          message: `${s.sessionTokens} tokens · $${(s.sessionTokens * s.config.costPerToken).toFixed(4)}`,
          type: 'info',
          duration: 3000,
        })
      },
      onVersion: () => {
        getState().addToast({ message: 'Abacus TUI v3.0.0', type: 'info', duration: 3000 })
      },
    })
  }

  // ─── Overlay ───

  private toggleOverlay(mode: OverlayMode): void {
    this.overlay = this.overlay === mode ? 'none' : mode
    this.commandQuery = ''
    this.commandSelectedIndex = 0
    this.renderOverlay()
  }

  // ─── UI Build ───

  private buildUI(): void {
    const W = this.renderer.terminalWidth || 120
    const H = this.renderer.terminalHeight || 40
    const layout = computeLayout(W, false, false)
    this.visibleHeight = H - 10

    // TopBar (3 rows: border + content + border)
    this.topBar = new BoxRenderable(this.renderer, {
      id: 'topbar',
      width: W,
      height: 3,
      borderStyle: 'rounded',
      borderColor: this.theme.overlay0,
      backgroundColor: this.theme.crust,
    })
    this.topBarText = new TextRenderable(this.renderer, {
      id: 'topbar-text',
      content: this.formatTopBar(),
      fg: this.theme.text,
    })
    this.topBar.add(this.topBarText)
    this.renderer.root.add(this.topBar)

    // Chat area (fills remaining space)
    const chatHeight = Math.max(this.visibleHeight, 5)
    this.chatArea = new BoxRenderable(this.renderer, {
      id: 'chat-area',
      width: layout.messageArea,
      height: chatHeight,
      borderStyle: 'rounded',
      borderColor: this.theme.overlay0,
      backgroundColor: this.theme.base,
    })
    this.chatContent = new TextRenderable(this.renderer, {
      id: 'chat-content',
      content: this.getWelcomeMessage(),
      fg: this.theme.subtext1,
    })
    this.chatArea.add(this.chatContent)
    this.renderer.root.add(this.chatArea)

    // Input bar (5 rows)
    this.inputBar = new BoxRenderable(this.renderer, {
      id: 'input-bar',
      width: W,
      height: 5,
      borderStyle: 'rounded',
      borderColor: this.theme.overlay0,
      backgroundColor: this.theme.crust,
    })
    this.inputText = new TextRenderable(this.renderer, {
      id: 'input-text',
      content: '  > Type a message...',
      fg: this.theme.overlay1,
    })
    this.inputBar.add(this.inputText)
    this.renderer.root.add(this.inputBar)

    // Status line (bottom)
    this.statusLine = new TextRenderable(this.renderer, {
      id: 'status-line',
      content: this.formatStatusLine(),
      fg: this.theme.overlay2,
      position: 'absolute',
      left: 0,
      top: H - 1,
    })
    this.renderer.root.add(this.statusLine)

    // Toast area (above input bar)
    this.toastArea = new TextRenderable(this.renderer, {
      id: 'toast-area',
      content: '',
      fg: this.theme.text,
      position: 'absolute',
      left: 0,
      top: H - 6,
    })
    this.renderer.root.add(this.toastArea)
  }

  private rebuildUI(): void {
    // Remove all children and rebuild
    for (const child of this.renderer.root.getChildren()) {
      this.renderer.root.remove(child.id)
    }
    this.buildUI()
    this.renderCards()
    this.renderer.requestRender()
  }

  private getWelcomeMessage(): string {
    return [
      '',
      '  Welcome to Abacus TUI v3',
      '',
      '  Type a message and press Enter to start.',
      '',
      '  Shortcuts:',
      '    Enter       Send message',
      '    Ctrl+K      Command palette',
      '    Ctrl+D      Dashboard',
      '    Ctrl+N      New session',
      '    PgUp/PgDn   Scroll',
      '    Esc         Cancel / clear',
      '    Ctrl+C      Exit',
      '',
      '  Commands: /help for full list',
      '',
    ].join('\n')
  }

  // ─── Formatting ───

  private formatTopBar(): string {
    const s = getState()
    return formatScenarioTopBar(
      this.scenario,
      s.mode,
      s.modelName,
      s.sessionTokens,
      s.sessionTokens * s.config.costPerToken,
    )
  }

  private formatStatusLine(): string {
    const s = getState()
    return formatScenarioStatusLine(this.scenario, s.mode, s.modelName)
  }

  // ─── Engine ───

  private bindEngine(): void {
    this.engine.onEvent((event: StreamEvent) => {
      if (event.eventType === 'chunk') {
        const chunk: StreamChunk = JSON.parse(event.data)
        this.handleChunk(chunk)
      }
    })
  }

  private handleChunk(chunk: StreamChunk): void {
    const s = getState()

    switch (chunk.kind) {
      case 'thinking': {
        this.cardStream.ensureActive('thinking', s.mode)
        this.cardStream.appendThinking(chunk.text)
        this.requestRender()
        break
      }
      case 'text_delta': {
        this.cardStream.ensureActive('llm', s.mode)
        this.cardStream.appendText(chunk.text)
        this.requestRender()
        break
      }
      case 'tool_start': {
        this.cardStream.finishActive()
        this.cardStream.ensureActive('abacus', s.mode)
        this.cardStream.addToolCall({
          name: chunk.name,
          args: '',
          status: 'Running',
          durationMs: 0,
        })
        s.updateStreamingTool(chunk.name, { status: 'Running' })
        this.requestRender()
        break
      }
      case 'tool_args': {
        this.cardStream.updateLastToolCall({ args: chunk.argsJson })
        break
      }
      case 'tool_output': {
        this.cardStream.updateLastToolCall({ output: chunk.outputJson })
        break
      }
      case 'tool_end': {
        this.cardStream.updateLastToolCall({
          status: chunk.success ? 'Success' : 'Failed',
          durationMs: chunk.durationMs,
        })
        s.updateStreamingTool(chunk.name, {
          status: chunk.success ? 'Success' : 'Failed',
          durationMs: chunk.durationMs,
        })
        this.requestRender()
        break
      }
      case 'confirm_required': {
        s.addToast({
          message: `🔒 ${chunk.request.toolId} — [A] Allow [S] Always [D] Deny`,
          type: 'warning',
          duration: 10000,
        })
        break
      }
      case 'complete': {
        this.cardStream.finishActive()
        s.setInputState('ready')
        s.resetStreamingTools()
        if (chunk.stats) {
          store.setState({ sessionTokens: s.sessionTokens + chunk.stats.completionTokens })
        }
        this.autoFollow = true
        this.requestRender()
        break
      }
      case 'error': {
        this.cardStream.abortActive()
        s.setInputState('ready')
        s.resetStreamingTools()
        s.addToast({ message: chunk.message, type: 'error', duration: 5000 })
        this.requestRender()
        break
      }
    }
  }

  // ─── Rendering ───

  private requestRender(): void {
    if (this.renderPending) return
    this.renderPending = true
    // Use microtask to batch multiple updates in same frame
    queueMicrotask(() => {
      this.renderPending = false
      this.doRender()
    })
  }

  private doRender(): void {
    if (this.overlay !== 'none') {
      this.renderOverlay()
    } else {
      this.renderCards()
      this.renderToasts()
    }
    this.renderer.requestRender()
  }

  private renderCards(): void {
    const W = this.renderer.terminalWidth || 120
    const H = this.renderer.terminalHeight || 40
    const layout = computeLayout(W, false, false)
    const s = getState()

    // Mode-specific rendering
    if (s.mode === 'team') {
      const teamState: TeamState = {
        goal: 'Optimize database queries',
        phase: 'executing',
        agents: [
          { id: '1', role: 'Leader', status: 'executing', currentTool: 'bash_exec', icon: '👤' },
          { id: '2', role: 'Member', status: 'executing', currentTool: 'fs_edit', icon: '👤' },
          { id: '3', role: 'Advisor', status: 'idle', icon: '👤' },
        ],
        tasks: [
          { id: '1', name: 'Analyze slow queries', status: 'completed', assignee: 'Leader', cost: 0.08, durationMs: 12000 },
          { id: '2', name: 'Identify N+1 issues', status: 'completed', assignee: 'Member', cost: 0.05, durationMs: 8000 },
          { id: '3', name: 'Add composite indexes', status: 'completed', assignee: 'Member', cost: 0.03, durationMs: 5000 },
          { id: '4', name: 'Refactor batch queries', status: 'running', assignee: 'Member' },
          { id: '5', name: 'Performance benchmark', status: 'pending' },
        ],
        cost: { leader: 0.12, member: 0.08, advisor: 0.03, total: 0.23 },
      }
      const teamLines = renderTeamArea(teamState, this.cardStream.allCards, this.theme, layout.messageArea, this.visibleHeight)
      this.chatContent.content = teamLines.join('\n')
    } else if (s.mode === 'meeting') {
      const meetingState: MeetingState = {
        topic: 'API versioning strategy',
        experts: [
          { id: '1', name: 'Architect', icon: '🧑‍💻', color: 'blue', status: 'speaking', lastOpinion: 'Use /api/v2/ prefix' },
          { id: '2', name: 'Security', icon: '🧑‍💻', color: 'red', status: 'waiting', lastOpinion: 'Consider backward compat' },
          { id: '3', name: 'Performance', icon: '🧑‍💻', color: 'green', status: 'waiting' },
        ],
        agenda: {
          topics: ['API versioning strategy', 'Backward compatibility', 'Migration plan'],
          currentTopic: 0,
          decisions: ['Use /api/v2/ prefix', 'Keep v1 compatibility layer'],
        },
        actionItems: ['Create v2 routes', 'Add v1→v2 redirect', 'Update docs'],
        cost: 0.15,
        durationMs: 135000,
      }
      const meetingLines = renderMeetingArea(meetingState, this.cardStream.allCards, this.theme, layout.messageArea, this.visibleHeight)
      this.chatContent.content = meetingLines.join('\n')
    } else if (s.mode === 'plan') {
      const planState: PlanState = {
        strategy: 'Optimize database query performance. Target: P99 latency from 200ms to 50ms.',
        tasks: [
          { id: '1', name: 'Analyze slow query logs', status: 'completed' },
          { id: '2', name: 'Identify N+1 problems', status: 'completed' },
          { id: '3', name: 'Add composite indexes', status: 'running' },
          { id: '4', name: 'Refactor batch queries', status: 'pending' },
          { id: '5', name: 'Performance benchmark', status: 'pending' },
        ],
        status: 'executing',
      }
      const planLines = renderPlanArea(planState, this.theme, layout.messageArea, this.visibleHeight)
      this.chatContent.content = planLines.join('\n')
    } else {
      // Clarify mode: render card stream
      const allCards = this.cardStream.allCards
      const lines: string[] = []

      for (let i = 0; i < allCards.length; i++) {
        const card = allCards[i]
        const rendered = renderCard(card, this.theme, layout.messageArea)
        lines.push(...rendered.lines)
        // Add separator between cards (except after last)
        if (i < allCards.length - 1) {
          lines.push('')
        }
      }

      this.totalLines = lines.length

      // Auto-scroll to bottom if following
      if (this.autoFollow) {
        this.scrollOffset = Math.max(0, this.totalLines - this.visibleHeight)
      }

      const start = Math.max(0, Math.min(this.scrollOffset, this.totalLines - this.visibleHeight))
      const visible = lines.slice(start, start + this.visibleHeight)

      this.chatContent.content = visible.join('\n') || this.getWelcomeMessage()
    }

    // Note: renderer.requestRender() is called by doRender() or directly by scroll handlers
  }

  private renderOverlay(): void {
    const W = this.renderer.terminalWidth || 120
    const H = this.renderer.terminalHeight || 40

    if (this.overlay === 'dashboard') {
      const dashLines = renderDashboard(this.theme, W)
      this.chatContent.content = dashLines.join('\n')
    } else if (this.overlay === 'command_palette') {
      const cmdLines = renderCommandPalette(this.commandQuery, this.commandSelectedIndex, this.theme, W, H - 10)
      this.chatContent.content = cmdLines.join('\n')
    }

    // Note: renderer.requestRender() is called by doRender()
  }

  private renderToasts(): void {
    const s = getState()
    const W = this.renderer.terminalWidth || 120
    const H = this.renderer.terminalHeight || 40

    if (s.toasts.length === 0) {
      this.toastArea.content = ''
      return
    }

    const toastLines: string[] = []
    const maxVisible = 3
    const visible = s.toasts.slice(-maxVisible)

    for (const toast of visible) {
      const icon = toast.type === 'success' ? '✓' : toast.type === 'error' ? '✗' : toast.type === 'warning' ? '⚠' : 'ℹ'
      const msg = toast.message.slice(0, W - 10)
      toastLines.push(`  ${icon} ${msg}`)
    }

    this.toastArea.content = toastLines.join('\n')
    // Position above input bar (fixed, not dynamic to avoid overlap issues)
    this.toastArea.left = 0
    this.toastArea.top = H - 6
  }

  // ─── Input ───

  private updateInputDisplay(): void {
    const s = getState()
    if (s.input) {
      this.inputText.content = `  > ${s.input}`
      this.inputBar.borderColor = this.theme.blue
    } else {
      this.inputText.content = '  > Type a message...'
      this.inputBar.borderColor = this.theme.overlay0
    }
    this.renderer.requestRender()
  }

  // ─── Keys ───

  private bindKeys(): void {
    this.renderer.keyInput.on('keypress', async (key) => {
      const s = getState()

      // Overlay mode
      if (this.overlay === 'command_palette') {
        this.handleCommandPaletteKeys(key)
        return
      }
      if (this.overlay === 'dashboard') {
        if (key.name === 'escape' || (key.ctrl && key.name === 'd')) {
          this.toggleOverlay('none')
        }
        return
      }

      // Ctrl+C: exit
      if (key.ctrl && key.name === 'c') {
        await this.engine.destroy()
        this.renderer.destroy()
        process.exit(0)
      }

      // Ctrl+D: dashboard
      if (key.ctrl && key.name === 'd') {
        this.toggleOverlay('dashboard')
        return
      }

      // Ctrl+K: command palette
      if (key.ctrl && key.name === 'k') {
        this.toggleOverlay('command_palette')
        return
      }

      // Ctrl+N: new session
      if (key.ctrl && key.name === 'n') {
        this.cardStream.clear()
        s.resetStreamingTools()
        s.setInput('')
        s.setInputState('ready')
        this.scrollOffset = 0
        this.autoFollow = true
        this.requestRender()
        s.addToast({ message: 'New session', type: 'info', duration: 1500 })
        return
      }

      // Esc: cancel or clear
      if (key.name === 'escape') {
        if (s.inputState === 'thinking' || s.inputState === 'executing' || s.inputState === 'outputting') {
          await this.engine.cancelTurn()
          this.cardStream.abortActive()
          s.resetStreamingTools()
          s.setInputState('ready')
          this.requestRender()
          s.addToast({ message: 'Cancelled', type: 'warning', duration: 1500 })
        } else if (s.input) {
          s.setInput('')
          this.updateInputDisplay()
        }
        return
      }

      // PageUp/Down: scroll (disable auto-follow)
      if (key.name === 'pageup') {
        this.autoFollow = false
        this.scrollOffset = Math.max(0, this.scrollOffset - Math.floor(this.visibleHeight / 2))
        this.requestRender()
        return
      }
      if (key.name === 'pagedown') {
        this.scrollOffset = Math.min(
          Math.max(0, this.totalLines - this.visibleHeight),
          this.scrollOffset + Math.floor(this.visibleHeight / 2)
        )
        if (this.scrollOffset >= this.totalLines - this.visibleHeight - 2) {
          this.autoFollow = true
        }
        this.requestRender()
        return
      }
      if (key.name === 'home') {
        this.autoFollow = false
        this.scrollOffset = 0
        this.requestRender()
        return
      }
      if (key.name === 'end') {
        this.autoFollow = true
        this.scrollOffset = Math.max(0, this.totalLines - this.visibleHeight)
        this.requestRender()
        return
      }

      // Space: toggle collapse (when input empty)
      if (key.name === 'space' && s.inputState === 'ready' && s.input === '') {
        const allCards = this.cardStream.cards
        if (allCards.length > 0) {
          this.cardStream.toggleCollapse(allCards[allCards.length - 1].id)
          this.requestRender()
        }
        return
      }

      // Enter: send or execute command
      if (key.name === 'return' && (s.inputState === 'ready' || s.inputState === 'typing')) {
        const input = s.input.trim()
        if (!input) return

        // Slash command
        if (input.startsWith('/')) {
          const { result, command } = await this.commands.dispatch(input)
          s.setInput('')
          this.updateInputDisplay()
          if (result === 'not_found') {
            s.addToast({ message: `Unknown: /${command}`, type: 'error', duration: 2000 })
          }
          return
        }

        // Regular message
        s.pushHistory(input)
        s.setInput('')
        s.setInputState('thinking')
        this.autoFollow = true

        this.cardStream.addUserCard(input, s.mode)
        this.requestRender()

        await this.engine.sendMessage(input)
        return
      }

      // Backspace
      if (key.name === 'backspace') {
        if (s.input.length > 0) {
          s.setInput(s.input.slice(0, -1))
          this.updateInputDisplay()
        }
        return
      }

      // Up/Down: history
      if (key.name === 'up' && (s.inputState === 'ready' || s.inputState === 'typing')) {
        s.navigateHistory('up')
        this.updateInputDisplay()
        return
      }
      if (key.name === 'down' && (s.inputState === 'ready' || s.inputState === 'typing')) {
        s.navigateHistory('down')
        this.updateInputDisplay()
        return
      }

      // Character input
      if (key.sequence && !key.ctrl && !key.meta) {
        s.setInput(s.input + key.sequence)
        if (s.inputState === 'ready') {
          s.setInputState('typing')
        }
        this.updateInputDisplay()
      }
    })
  }

  private handleCommandPaletteKeys(key: any): void {
    const filtered = this.commandQuery
      ? this.commands.complete(this.commandQuery)
      : this.commands.getAllNames().map(n => `/${n}`)

    if (key.name === 'escape') {
      this.toggleOverlay('none')
      return
    }
    if (key.name === 'up') {
      this.commandSelectedIndex = Math.max(0, this.commandSelectedIndex - 1)
      this.renderOverlay()
      return
    }
    if (key.name === 'down') {
      this.commandSelectedIndex = Math.min(filtered.length - 1, this.commandSelectedIndex + 1)
      this.renderOverlay()
      return
    }
    if (key.name === 'return') {
      const selected = filtered[this.commandSelectedIndex]
      if (selected) {
        this.toggleOverlay('none')
        getState().setInput(selected)
        this.updateInputDisplay()
      }
      return
    }
    if (key.name === 'backspace') {
      this.commandQuery = this.commandQuery.slice(0, -1)
      this.commandSelectedIndex = 0
      this.renderOverlay()
      return
    }
    if (key.sequence && !key.ctrl && !key.meta) {
      this.commandQuery += key.sequence
      this.commandSelectedIndex = 0
      this.renderOverlay()
    }
  }

  // ─── Store subscriptions ───

  private bindStore(): void {
    // Zustand vanilla subscribe: callback receives (state, prevState)
    // We manually compare to avoid unnecessary re-renders
    let prevMode = getState().mode
    let prevTokens = getState().sessionTokens
    let prevState = getState().inputState

    subscribe((state) => {
      // Mode changed
      if (state.mode !== prevMode) {
        prevMode = state.mode
        this.topBarText.content = this.formatTopBar()
        this.statusLine.content = this.formatStatusLine()
        this.requestRender()
      }

      // Tokens changed
      if (state.sessionTokens !== prevTokens) {
        prevTokens = state.sessionTokens
        this.topBarText.content = this.formatTopBar()
        this.renderer.requestRender()
      }

      // Input state changed
      if (state.inputState !== prevState) {
        prevState = state.inputState
        this.topBarText.content = this.formatTopBar()
        this.renderer.requestRender()
      }
    })
  }

  private startToastCleanup(): void {
    setInterval(() => {
      const s = getState()
      const now = Date.now()
      for (const toast of s.toasts) {
        if (now - toast.createdAt > toast.duration) {
          s.removeToast(toast.id)
          this.renderToasts()
        }
      }
    }, 500)
  }
}
