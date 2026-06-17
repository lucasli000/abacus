// utils/scroll.ts — Scroll management utilities

export interface ScrollState {
  offset: number
  totalLines: number
  visibleHeight: number
}

/// Create initial scroll state
export function createScrollState(visibleHeight: number): ScrollState {
  return {
    offset: 0,
    totalLines: 0,
    visibleHeight,
  }
}

/// Scroll up by N lines
export function scrollUp(state: ScrollState, lines: number = 3): ScrollState {
  return {
    ...state,
    offset: Math.max(0, state.offset - lines),
  }
}

/// Scroll down by N lines
export function scrollDown(state: ScrollState, lines: number = 3): ScrollState {
  return {
    ...state,
    offset: Math.min(
      Math.max(0, state.totalLines - state.visibleHeight),
      state.offset + lines
    ),
  }
}

/// Scroll to bottom (auto-follow)
export function scrollToBottom(state: ScrollState): ScrollState {
  return {
    ...state,
    offset: Math.max(0, state.totalLines - state.visibleHeight),
  }
}

/// Scroll to top
export function scrollToTop(state: ScrollState): ScrollState {
  return {
    ...state,
    offset: 0,
  }
}

/// Page up (half screen)
export function pageUp(state: ScrollState): ScrollState {
  return scrollUp(state, Math.floor(state.visibleHeight / 2))
}

/// Page down (half screen)
export function pageDown(state: ScrollState): ScrollState {
  return scrollDown(state, Math.floor(state.visibleHeight / 2))
}

/// Update total lines and auto-scroll if at bottom
export function updateTotalLines(
  state: ScrollState,
  newTotal: number,
  autoFollow: boolean = true,
): ScrollState {
  const wasAtBottom = state.offset >= state.totalLines - state.visibleHeight - 2
  const newState = { ...state, totalLines: newTotal }

  if (autoFollow && wasAtBottom) {
    return scrollToBottom(newState)
  }

  return newState
}

/// Get visible slice of lines
export function getVisibleLines(
  lines: string[],
  state: ScrollState,
): string[] {
  const start = Math.max(0, Math.min(state.offset, state.totalLines - state.visibleHeight))
  return lines.slice(start, start + state.visibleHeight)
}

/// Check if scroll is at bottom
export function isAtBottom(state: ScrollState): boolean {
  return state.offset >= state.totalLines - state.visibleHeight - 2
}
