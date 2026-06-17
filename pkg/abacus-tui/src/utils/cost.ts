// utils/cost.ts — Cost estimation utilities

const CNY_PER_TOKEN_DEFAULT = 0.000002
const USD_PER_TOKEN_DEFAULT = 0.0000003
const FX_RATE = 7.10 // CNY per USD

/// Estimate cost in CNY
export function estimateCostCny(tokens: number, costPerToken: number = CNY_PER_TOKEN_DEFAULT): number {
  return tokens * costPerToken
}

/// Estimate cost in USD
export function estimateCostUsd(tokens: number, costPerToken: number = USD_PER_TOKEN_DEFAULT): number {
  return tokens * costPerToken
}

/// Format cost for display
export function formatCost(cny: number): string {
  if (cny < 0.0001) return '$0.00'
  if (cny < 0.01) return `$${cny.toFixed(4)}`
  if (cny < 1) return `$${cny.toFixed(3)}`
  if (cny < 100) return `$${cny.toFixed(2)}`
  return `$${cny.toFixed(0)}`
}

/// Format token count
export function formatTokens(tokens: number): string {
  if (tokens < 1000) return `${tokens}`
  if (tokens < 1000000) return `${(tokens / 1000).toFixed(1)}K`
  return `${(tokens / 1000000).toFixed(2)}M`
}

/// Token budget usage percentage
export function tokenUsagePercent(current: number, max: number): number {
  if (max <= 0) return 0
  return Math.min(100, (current / max) * 100)
}

/// Is approaching budget limit?
export function isApproachingBudget(current: number, max: number, threshold: number = 0.8): boolean {
  return tokenUsagePercent(current, max) >= threshold * 100
}
