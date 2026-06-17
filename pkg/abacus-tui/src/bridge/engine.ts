// bridge/engine.ts — TypeScript wrapper with polling model

import { loadNativeBridge } from './native'
import type { StreamEvent, NativeBridge } from './types'

export class Engine {
  private bridge: InstanceType<NativeBridge>
  private listeners: Set<(event: StreamEvent) => void> = new Set()
  private pollTimer: ReturnType<typeof setInterval> | null = null
  private polling = false

  constructor() {
    const BridgeClass = loadNativeBridge()
    this.bridge = new BridgeClass()
  }

  async init(model: string, thinking: string): Promise<void> {
    await this.bridge.init(model, thinking)
    this.startPolling()
  }

  async destroy(): Promise<void> {
    this.stopPolling()
    await this.bridge.destroy()
    this.listeners.clear()
  }

  async isInitialized(): Promise<boolean> {
    return this.bridge.isInitialized()
  }

  async getModel(): Promise<string> {
    return this.bridge.getModel()
  }

  async getThinking(): Promise<string> {
    return this.bridge.getThinking()
  }

  async sendMessageEcho(input: string): Promise<string> {
    return this.bridge.sendMessageEcho(input)
  }

  async sendMessage(input: string): Promise<void> {
    await this.bridge.sendMessage(input)
  }

  async cancelTurn(): Promise<void> {
    this.bridge.cancelTurn()
  }

  async continueGeneration(): Promise<void> {
    await this.bridge.continueGeneration()
  }

  async startTeam(goal: string): Promise<void> {
    await this.bridge.startTeam(goal)
  }

  async sendTeamMessage(input: string): Promise<void> {
    await this.bridge.sendTeamMessage(input)
  }

  async startMeeting(topic: string): Promise<void> {
    await this.bridge.startMeeting(topic)
  }

  async sendMeetingMessage(input: string): Promise<void> {
    await this.bridge.sendMeetingMessage(input)
  }

  async confirmTools(decisions: Array<{ tool: string; grant: 'Once' | 'Always' | 'Deny' }>): Promise<void> {
    await this.bridge.confirmTools(JSON.stringify(decisions))
  }

  async saveSession(): Promise<string> {
    return this.bridge.saveSession()
  }

  async loadSession(path: string): Promise<void> {
    return this.bridge.loadSession(path)
  }

  async listSessions(): Promise<string> {
    return this.bridge.listSessions()
  }

  async getConfig(): Promise<string> {
    return this.bridge.getConfig()
  }

  async reloadConfig(): Promise<void> {
    return this.bridge.reloadConfig()
  }

  async listModels(): Promise<string> {
    return this.bridge.listModels()
  }

  async executeSlashCommand(command: string): Promise<void> {
    await this.bridge.executeSlashCommand(command)
  }

  onEvent(fn: (event: StreamEvent) => void): () => void {
    this.listeners.add(fn)
    return () => {
      this.listeners.delete(fn)
    }
  }

  /// Start polling for events (every 50ms ≈ 20fps — sufficient for terminal)
  private startPolling(): void {
    if (this.polling) return
    this.polling = true
    this.pollTimer = setInterval(async () => {
      await this.pollOnce()
    }, 50)
  }

  /// Stop polling
  private stopPolling(): void {
    if (this.pollTimer) {
      clearInterval(this.pollTimer)
      this.pollTimer = null
    }
    this.polling = false
  }

  /// Poll once and dispatch events
  private async pollOnce(): Promise<void> {
    try {
      const json = await this.bridge.pollEvents()
      if (!json || json === '[]') return

      const events: StreamEvent[] = JSON.parse(json)
      for (const event of events) {
        for (const fn of this.listeners) {
          fn(event)
        }
      }
    } catch {
      // Ignore polling errors
    }
  }
}
