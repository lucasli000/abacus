// test-bridge.ts — Verify NAPI bridge with polling model

import { Engine } from './src/bridge/engine'

async function testBridge() {
  console.log('Loading NAPI bridge...')
  const engine = new Engine()
  console.log('Bridge loaded')

  console.log('Initializing engine...')
  await engine.init('test-model', 'medium')
  console.log(`Model: ${await engine.getModel()}`)
  console.log(`Thinking: ${await engine.getThinking()}`)
  console.log(`Initialized: ${await engine.isInitialized()}`)

  console.log('\nTesting echo...')
  const echo = await engine.sendMessageEcho('Hello from Bun!')
  console.log(`Echo: ${echo}`)

  console.log('\nTesting streaming (polling model)...')
  const events: any[] = []
  engine.onEvent((event) => {
    const data = JSON.parse(event.data)
    events.push(data)
    console.log(`  Event: ${event.eventType} → ${data.kind}`)
  })

  await engine.sendMessage('Hello streaming!')

  // Wait for events to arrive
  await new Promise(resolve => setTimeout(resolve, 500))
  console.log(`Total events: ${events.length}`)

  console.log('\nTesting listModels...')
  const models = await engine.listModels()
  console.log(`Models: ${models}`)

  console.log('\nDestroying engine...')
  await engine.destroy()
  console.log('Destroyed')

  console.log('\nAll tests passed!')
}

testBridge().catch(err => {
  console.error('Test failed:', err)
  process.exit(1)
})
