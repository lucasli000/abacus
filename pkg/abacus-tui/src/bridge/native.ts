// bridge/native.ts — Load the native .node file

import { join } from 'path'
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

export function loadNativeBridge(): NativeBridge {
  const suffix = getPlatformSuffix()
  const projectRoot = join(import.meta.dir, '..', '..')
  const nativePath = join(projectRoot, `abacus-bridge.${suffix}.node`)

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
