// build.ts — Build script for Abacus TUI
// Usage: bun run build.ts

import { $ } from 'bun'
import { join } from 'path'
import { existsSync, copyFileSync, mkdirSync } from 'fs'

const PROJECT_ROOT = import.meta.dir
const DIST_DIR = join(PROJECT_ROOT, 'dist')
const TARGET = 'bun-linux-x64' // or 'bun-darwin-arm64', 'bun-windows-x64'

async function build() {
  console.log('Building Abacus TUI v3...')
  console.log(`Project root: ${PROJECT_ROOT}`)

  // 1. Ensure dist directory exists
  if (!existsSync(DIST_DIR)) {
    mkdirSync(DIST_DIR, { recursive: true })
  }

  // 2. Check for native bridge
  const platform = process.platform
  const arch = process.arch
  const suffix = `${platform}-${arch}`
  const nativeSrc = join(PROJECT_ROOT, `abacus-bridge.${suffix}.node`)
  const nativeDest = join(DIST_DIR, `abacus-bridge.${suffix}.node`)

  if (existsSync(nativeSrc)) {
    console.log(`Copying native bridge: ${nativeSrc}`)
    copyFileSync(nativeSrc, nativeDest)
  } else {
    console.warn(`Warning: Native bridge not found at ${nativeSrc}`)
    console.warn('Run: cargo build -p abacus-bridge --release')
  }

  // 3. Compile with bun build --compile
  console.log('Compiling TypeScript...')
  try {
    await $`bun build --compile --minify --sourcemap src/index.ts --outfile dist/abacus`
    console.log('Build successful!')
    console.log(`Output: ${join(DIST_DIR, 'abacus')}`)
  } catch (err) {
    console.error('Build failed:', err)
    process.exit(1)
  }

  // 4. Copy native bridge next to binary
  if (existsSync(nativeSrc)) {
    const binaryDir = DIST_DIR
    const nativeInDist = join(binaryDir, `abacus-bridge.${suffix}.node`)
    if (!existsSync(nativeInDist)) {
      copyFileSync(nativeSrc, nativeInDist)
    }
  }

  console.log('')
  console.log('Done! To run:')
  console.log(`  cd ${DIST_DIR} && ./abacus`)
}

build().catch(err => {
  console.error('Build error:', err)
  process.exit(1)
})
