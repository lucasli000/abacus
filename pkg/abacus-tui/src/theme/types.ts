// theme/types.ts — Theme interface

export interface Theme {
  name: string
  type: 'dark' | 'light'

  // Background layers
  crust: string
  base: string
  mantle: string
  surface0: string
  surface1: string
  surface2: string

  // Text layers
  text: string
  subtext1: string
  subtext0: string
  overlay2: string
  overlay1: string
  overlay0: string

  // Accent colors
  red: string
  peach: string
  yellow: string
  green: string
  teal: string
  sky: string
  blue: string
  lavender: string
  mauve: string
  pink: string

  // Mode colors
  mode_clarify: string
  mode_team: string
  mode_meeting: string
  mode_plan: string
}
