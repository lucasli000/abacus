// bridge/native.ts — Load the native .node file

import { join, dirname } from 'path'
import { existsSync } from 'fs'
import type { NativeBridge } from './types'

const PLATFORM_SUFFIX: Record<string, string> = {
  'darwin-arm64': 'darwin-arm64',
  'darwin-x64': 'darwin-x64',
  'linux-x64': 'linux-x64',
  'linux-arm64': 'linux-arm64',
}

function getPlatformSuffix(): string {
  const key = `${process.platform}-${process.arch}`
  const suffix = PLATFORM_SUFFIX[key]
  if (!suffix) {
    throw new Error(`Unsupported platform: ${key}. Supported: ${Object.keys(PLATFORM_SUFFIX).join(', ')}`)
  }
  return suffix
}

function findNativePath(suffix: string): string {
  const fileName = `abacus-bridge.${suffix}.node`

  // 1. Next to the executable (compiled binary: bun build --compile)
  const execDir = dirname(process.execPath)
  const execPath = join(execDir, fileName)
  if (existsSync(execPath)) return execPath

  // 2. Next to the script (development: bun run src/index.ts)
  const scriptDir = dirname(import.meta.dir)
  const scriptPath = join(scriptDir, '..', fileName)
  if (existsSync(scriptPath)) return scriptPath

  // 3. In project root (fallback)
  const projectRoot = join(import.meta.dir, '..', '..')
  const projectPath = join(projectRoot, fileName)
  if (existsSync(projectPath)) return projectPath

  // 4. Current working directory
  const cwdPath = join(process.cwd(), fileName)
  if (existsSync(cwdPath)) return cwdPath

  throw new Error(
    `Cannot find ${fileName}.\n` +
    `Searched:\n` +
    `  - ${execPath}\n` +
    `  - ${scriptPath}\n` +
    `  - ${projectPath}\n` +
    `  - ${cwdPath}\n` +
    `Run 'cargo build -p abacus-bridge --release' and copy the .node file.`
  )
}

export function loadNativeBridge(): NativeBridge {
  const suffix = getPlatformSuffix()
  const nativePath = findNativePath(suffix)

  try {
    const native = require(nativePath)
    // Use bracket notation — Bun NAPI module property access workaround
    const BridgeClass = native['AbacusBridge'] as NativeBridge
    if (!BridgeClass) {
      throw new Error(`AbacusBridge not found in module. Keys: ${Object.keys(native)}`)
    }
    return BridgeClass
  } catch (err) {
    throw new Error(
      `Failed to load native bridge from ${nativePath}.\n` +
      `Run 'cargo build -p abacus-bridge --release' first.\n` +
      `Error: ${err}`
    )
  }
}
