// state/machines/mode.ts — Mode state machine (Clarify/Team/Meeting/Plan)

import { createMachine } from 'xstate'

type ModeEvent =
  | { type: 'SWITCH_TEAM' }
  | { type: 'SWITCH_MEETING' }
  | { type: 'SWITCH_CLARIFY' }
  | { type: 'SWITCH_PLAN' }
  | { type: 'DONE' }

export const modeMachine = createMachine({
  id: 'mode',
  initial: 'clarify',
  types: {} as {
    events: ModeEvent
  },
  states: {
    clarify: {
      on: {
        SWITCH_TEAM: 'team',
        SWITCH_MEETING: 'meeting',
        SWITCH_PLAN: 'plan',
      },
    },
    team: {
      on: {
        SWITCH_CLARIFY: 'clarify',
        DONE: 'clarify',
      },
    },
    meeting: {
      on: {
        SWITCH_CLARIFY: 'clarify',
        DONE: 'clarify',
      },
    },
    plan: {
      on: {
        SWITCH_TEAM: 'team',
        DONE: 'team',
      },
    },
  },
})
