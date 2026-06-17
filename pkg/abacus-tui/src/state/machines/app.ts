// state/machines/app.ts — Top-level application state machine

import { createMachine, assign } from 'xstate'
import type { Engine } from '../../bridge/engine'

interface AppContext {
  engine: Engine | null
  error: string | null
}

export const appMachine = createMachine({
  id: 'app',
  initial: 'idle',
  context: {
    engine: null,
    error: null,
  } as AppContext,
  states: {
    idle: {
      on: {
        INIT: {
          target: 'running',
          actions: assign({ engine: ({ event }) => event.engine }),
        },
      },
    },
    running: {
      on: {
        SWITCH_MODE: {
          actions: assign({ error: null }),
        },
        SHUTDOWN: 'shutdown',
      },
    },
    shutdown: {
      type: 'final',
    },
  },
})
