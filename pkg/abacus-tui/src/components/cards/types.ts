// components/cards/types.ts — Card type definitions

export type CardKind = 'user' | 'llm' | 'abacus' | 'thinking' | 'expert'
export type ToolTier = 'S' | 'A' | 'B' | 'C' | 'D'
export type ToolStatus = 'Running' | 'Success' | 'Failed'

export interface Card {
  id: string
  kind: CardKind
  content: string
  timestamp: number
  mode: string
  collapsed: boolean
  thinking?: string
  toolCalls?: ToolCallInfo[]
  expertName?: string
  expertColor?: string
}

export interface ToolCallInfo {
  name: string
  args: string
  output?: string
  status: ToolStatus
  durationMs: number
  tier?: ToolTier
  filePath?: string
}

export interface TraceEvent {
  id: string
  kind: 'tool_call' | 'thinking' | 'generic'
  name: string
  args: string
  output?: string
  status: ToolStatus
  durationMs: number
  timestamp: number
  tier?: ToolTier
}

/// CardStream manages the lifecycle of cards during streaming
export class CardStream {
  private _cards: Card[] = []
  private _activeCard: Card | null = null
  private _nextId = 0

  get cards(): Card[] { return this._cards }
  get activeCard(): Card | null { return this._activeCard }
  get allCards(): Card[] {
    return this._activeCard ? [...this._cards, this._activeCard] : this._cards
  }

  /// Add a user card (immediately static)
  addUserCard(content: string, mode: string): Card {
    const card: Card = {
      id: `card-${this._nextId++}`,
      kind: 'user',
      content,
      timestamp: Date.now(),
      mode,
      collapsed: false,
    }
    this._cards.push(card)
    return card
  }

  /// Ensure active card is of given kind, finish previous if different
  ensureActive(kind: CardKind, mode: string): Card {
    if (this._activeCard && this._activeCard.kind !== kind) {
      this.finishActive()
    }
    if (!this._activeCard) {
      this._activeCard = {
        id: `card-${this._nextId++}`,
        kind,
        content: '',
        timestamp: Date.now(),
        mode,
        collapsed: kind === 'thinking' || kind === 'abacus',
      }
    }
    return this._activeCard
  }

  /// Append text to active card (delta may contain newlines)
  appendText(delta: string): void {
    if (this._activeCard) {
      this._activeCard.content += delta
    }
  }

  /// Append thinking to active card (delta may contain newlines)
  appendThinking(delta: string): void {
    if (this._activeCard) {
      this._activeCard.thinking = (this._activeCard.thinking || '') + delta
    }
  }

  /// Add a tool call to active card
  addToolCall(info: ToolCallInfo): void {
    if (this._activeCard) {
      if (!this._activeCard.toolCalls) this._activeCard.toolCalls = []
      this._activeCard.toolCalls.push(info)
    }
  }

  /// Update the last tool call status
  updateLastToolCall(patch: Partial<ToolCallInfo>): void {
    if (this._activeCard?.toolCalls?.length) {
      const last = this._activeCard.toolCalls[this._activeCard.toolCalls.length - 1]
      Object.assign(last, patch)
    }
  }

  /// Finish active card → becomes static
  finishActive(): void {
    if (this._activeCard) {
      this._cards.push(this._activeCard)
      this._activeCard = null
    }
  }

  /// Abort active card (error/cancel)
  abortActive(): void {
    this._activeCard = null
  }

  /// Toggle card collapse
  toggleCollapse(cardId: string): void {
    const card = this._cards.find(c => c.id === cardId)
    if (card) card.collapsed = !card.collapsed
  }

  /// Clear all cards
  clear(): void {
    this._cards = []
    this._activeCard = null
  }
}
