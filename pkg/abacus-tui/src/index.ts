// index.ts — Abacus TUI v3 entry point

import { createCliRenderer } from '@opentui/core'
import { Engine } from './bridge/engine'
import { App } from './components/App'
import { getState } from './state/store'

async function main() {
  // 1. Initialize engine
  const engine = new Engine()
  await engine.init('claude-sonnet-4', 'adaptive')

  // 2. Create renderer
  const renderer = await createCliRenderer({
    exitOnCtrlC: false, // We handle Ctrl+C ourselves
    targetFps: 30,
    useMouse: true,
    autoFocus: true,
  })

  // 3. Build UI
  const app = new App(renderer, engine)

  // 4. Set initial state
  getState().setModel('claude-sonnet-4')

  console.log('Abacus TUI v3 ready.')
}

main().catch(err => {
  console.error('Fatal:', err)
  process.exit(1)
})
