// test-opentui.ts — Verify OpenTUI renders in terminal

import { createCliRenderer, TextRenderable, BoxRenderable } from '@opentui/core'

async function testOpenTUI() {
  console.log('Creating CLI renderer...')

  const renderer = await createCliRenderer({
    exitOnCtrlC: true,
    targetFps: 30,
  })

  console.log('Renderer created!')

  // Create a simple text
  const text = new TextRenderable(renderer, {
    id: 'hello-text',
    content: 'Hello from Abacus TUI v3! Press Ctrl+C to exit.',
    fg: '#7080f0', // blue
    position: { x: 2, y: 2 },
  })

  renderer.root.add(text)

  // Create a box
  const box = new BoxRenderable(renderer, {
    id: 'test-box',
    borderStyle: 'rounded',
    title: ' Status ',
    padding: 1,
    width: 50,
    height: 8,
    position: { x: 2, y: 5 },
    fg: '#d4d8f0',
    borderFg: '#4a506e',
  })

  const statusText = new TextRenderable(renderer, {
    id: 'status-text',
    content: [
      '  ● Ready',
      '  Model: claude-sonnet-4',
      '  Tokens: 0 / 200K',
      '  Cost: $0.00',
    ].join('\n'),
    fg: '#a8aed0',
  })

  box.add(statusText)
  renderer.root.add(box)

  // Create a second text with animated content
  let tick = 0
  const animText = new TextRenderable(renderer, {
    id: 'anim-text',
    content: 'Frame: 0',
    fg: '#70d080', // green
    position: { x: 2, y: 14 },
  })
  renderer.root.add(animText)

  // Update animation
  const interval = setInterval(() => {
    tick++
    animText.content = `Frame: ${tick} | FPS: ${renderer.targetFps} | Time: ${new Date().toLocaleTimeString()}`
    renderer.requestRender()
  }, 100)

  // Handle Ctrl+C
  renderer.keyInput.on('keypress', (key) => {
    if (key.ctrl && key.name === 'c') {
      clearInterval(interval)
      renderer.destroy()
      process.exit(0)
    }
  })

  console.log('Rendering... Press Ctrl+C to exit.')
}

testOpenTUI().catch(err => {
  console.error('Failed:', err)
  process.exit(1)
})
