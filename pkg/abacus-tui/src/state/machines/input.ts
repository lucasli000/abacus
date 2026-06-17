// state/machines/input.ts — Input state machine

import { createMachine } from 'xstate'

type InputEvent =
  | { type: 'CHAR' }
  | { type: 'BACKSPACE' }
  | { type: 'ENTER' }
  | { type: 'SHIFT_ENTER' }
  | { type: 'TAB' }
  | { type: 'ESC' }
  | { type: 'TEXT_DELTA' }
  | { type: 'THINKING' }
  | { type: 'TOOL_START' }
  | { type: 'TOOL_END' }
  | { type: 'COMPLETE' }
  | { type: 'EMPTY' }
  | { type: 'UP' }
  | { type: 'DOWN' }
  | { type: 'RIGHT_AT_END' }

export const inputMachine = createMachine({
  id: 'input',
  initial: 'ready',
  types: {} as {
    events: InputEvent
  },
  states: {
    ready: {
      on: {
        CHAR: 'typing',
        UP: 'ready',
        TAB: 'completing',
      },
    },
    typing: {
      on: {
        CHAR: 'typing',
        BACKSPACE: 'typing',
        ENTER: 'thinking',
        SHIFT_ENTER: 'typing',
        TAB: 'completing',
        RIGHT_AT_END: 'typing',
        UP: 'typing',
        DOWN: 'typing',
        EMPTY: 'ready',
      },
    },
    completing: {
      on: {
        TAB: 'completing',
        ENTER: 'typing',
        ESC: 'typing',
        CHAR: 'completing',
        BACKSPACE: 'completing',
      },
    },
    thinking: {
      on: {
        TEXT_DELTA: 'outputting',
        THINKING: 'thinking',
        TOOL_START: 'executing',
        ESC: 'ready',
        ENTER: 'thinking',
      },
    },
    executing: {
      on: {
        TOOL_END: 'executing',
        TEXT_DELTA: 'outputting',
        COMPLETE: 'ready',
        ESC: 'ready',
        ENTER: 'executing',
      },
    },
    outputting: {
      on: {
        TEXT_DELTA: 'outputting',
        TOOL_START: 'executing',
        COMPLETE: 'ready',
        ESC: 'ready',
      },
    },
    paused: {
      on: {
        CHAR: 'ready',
      },
    },
    editor: {
      on: {
        ESC: 'ready',
        ENTER: 'editor',
      },
    },
  },
})
