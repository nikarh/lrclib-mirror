import { createTheme, type MantineColorsTuple } from '@mantine/core'

// One accent, an ink violet, for what's actionable and for selected lines.
const accent: MantineColorsTuple = [
  '#f0eeff',
  '#dcd8ff',
  '#b6aeff',
  '#8f82fb',
  '#7262f6',
  '#5f4ef3',
  '#5543f2',
  '#4535d8',
  '#3c2fc2',
  '#3027ab',
]

// Warm near-blacks for the dark scheme (Mantine reads text from [0], surfaces from [6]..[8]).
const dark: MantineColorsTuple = [
  '#eceae6',
  '#c6c3bd',
  '#959189',
  '#69665f',
  '#45433e',
  '#2f2e2b',
  '#212024',
  '#18181b',
  '#111114',
  '#0b0b0d',
]

export const theme = createTheme({
  primaryColor: 'accent',
  primaryShade: { light: 6, dark: 3 },
  colors: { accent, dark },
  fontFamily: "'Geist Variable', ui-sans-serif, system-ui, sans-serif",
  fontFamilyMonospace: "'Geist Mono Variable', ui-monospace, monospace",
  headings: {
    fontFamily: "'Geist Variable', ui-sans-serif, system-ui, sans-serif",
    fontWeight: '650',
  },
  defaultRadius: 'md',
  cursorType: 'pointer',
  focusRing: 'auto',
  components: {
    Badge: { defaultProps: { radius: 'sm', variant: 'light', size: 'md' } },
    ActionIcon: { defaultProps: { variant: 'subtle', color: 'gray' } },
    Menu: { defaultProps: { radius: 'md', shadow: 'md', position: 'bottom-end' } },
  },
})
