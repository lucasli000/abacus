// theme/oklch.ts — OKLCH color space utilities

/// Convert OKLCH to sRGB hex string
/// L: 0-1 (lightness), C: 0-0.37 (chroma), H: 0-360 (hue)
export function oklch(l: number, c: number, h: number): string {
  const [r, g, b] = oklchToSrgb(l, c, h)
  return `#${toHex(r)}${toHex(g)}${toHex(b)}`
}

function toHex(n: number): string {
  const clamped = Math.max(0, Math.min(255, Math.round(n)))
  return clamped.toString(16).padStart(2, '0')
}

function oklchToSrgb(l: number, c: number, h: number): [number, number, number] {
  // OKLCH → OKLab
  const hRad = (h * Math.PI) / 180
  const a = c * Math.cos(hRad)
  const b = c * Math.sin(hRad)

  // OKLab → linear sRGB
  const [r, g, b2] = oklabToLinearSrgb(l, a, b)

  // Linear sRGB → sRGB
  return [
    linearToSrgb(r) * 255,
    linearToSrgb(g) * 255,
    linearToSrgb(b2) * 255,
  ]
}

function oklabToLinearSrgb(L: number, a: number, b: number): [number, number, number] {
  const l_ = L + 0.3963377774 * a + 0.2158037573 * b
  const m_ = L - 0.1055613458 * a - 0.0638541728 * b
  const s_ = L - 0.0894841775 * a - 1.2914855480 * b

  const l = l_ * l_ * l_
  const m = m_ * m_ * m_
  const s = s_ * s_ * s_

  return [
    +4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
    -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
    -0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s,
  ]
}

function linearToSrgb(c: number): number {
  return c <= 0.0031308
    ? 12.92 * c
    : 1.055 * Math.pow(c, 1 / 2.4) - 0.055
}
