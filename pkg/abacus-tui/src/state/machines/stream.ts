// state/machines/stream.ts — Stream state machine

import { createMachine } from 'xstate'

type StreamEvent =
  | { type: 'CHUNK_RECEIVED' }
  | { type: 'TEXT_DELTA' }
  | { type: 'THINKING' }
  | { type: 'TOOL_START' }
  | { type: 'TOOL_ARGS' }
  | { type: 'TOOL_OUTPUT' }
  | { type: 'TOOL_END' }
  | { type: 'CONFIRM_REQUIRED' }
  | { type: 'COMPLETE' }
  | { type: 'ERROR' }
  | { type: 'RESET' }

export const streamMachine = createMachine({
  id: 'stream',
  initial: 'idle',
  types: {} as {
    events: StreamEvent
  },
  states: {
    idle: {
      on: {
        CHUNK_RECEIVED: 'streaming',
      },
    },
    streaming: {
      on: {
        TEXT_DELTA: 'streaming',
        THINKING: 'streaming',
        TOOL_START: 'streaming',
        TOOL_ARGS: 'streaming',
        TOOL_OUTPUT: 'streaming',
        TOOL_END: 'streaming',
        CONFIRM_REQUIRED: 'streaming',
        COMPLETE: 'complete',
        ERROR: 'idle',
        RESET: 'idle',
      },
    },
    complete: {
      on: {
        RESET: 'idle',
        CHUNK_RECEIVED: 'streaming',
      },
    },
  },
})
