import { defineConfig } from 'oxlint'

import js from '@eslint/js'
import tseslint from 'typescript-eslint'
import react from 'eslint-plugin-react'
import reactHooks from 'eslint-plugin-react-hooks'
import reactRefresh from 'eslint-plugin-react-refresh'
import jsxA11y from 'eslint-plugin-jsx-a11y'

type Severity = 'off' | 'warn' | 'error' | 0 | 1 | 2
type RuleEntry = Severity | [Severity, ...unknown[]]
type RuleMap = Partial<Record<string, RuleEntry>>

const flatten = (configs: readonly { rules?: RuleMap }[]): RuleMap => {
  const merged: RuleMap = {}
  for (const config of configs) Object.assign(merged, config.rules ?? {})
  return merged
}

// The same upstream presets the ESLint config used, so bumping a plugin updates this set
// with no edits here.
const upstream: RuleMap = {
  ...js.configs.recommended.rules,
  ...flatten(tseslint.configs.strictTypeChecked),
  ...flatten(tseslint.configs.stylisticTypeChecked),
  ...(react.configs.flat.recommended.rules as RuleMap),
  ...(react.configs.flat['jsx-runtime'].rules as RuleMap),
  ...reactHooks.configs.flat.recommended.rules,
  ...flatten([reactRefresh.configs.vite]),
  ...(jsxA11y.flatConfigs.strict.rules as RuleMap),

  '@typescript-eslint/no-unused-vars': ['error', { ignoreRestSiblings: true }],
}

// oxlint refuses to start on a rule name it doesn't implement, so preset rules with no
// oxlint counterpart are skipped explicitly. If a preset upgrade adds another one, oxlint
// fails and names it. Legacy octal literals are a syntax error under ES modules, enums are
// rejected by `erasableSyntaxOnly` in tsconfig, and oxlint's no-unused-vars understands JSX
// usage natively. The rest are only ever switched off by the presets.
const UNSUPPORTED = new Set([
  'no-octal',
  '@typescript-eslint/no-unsafe-enum-assignment',
  'react/jsx-uses-vars',
  'react/jsx-uses-react',
  'no-dupe-args',
  'no-new-symbol',
  'no-return-await',
  'dot-notation',
  'jsx-a11y/label-has-for',
])

// Rules oxlint implements natively under a different name.
const NATIVE_EQUIVALENT: Partial<Record<string, string>> = {
  'react-hooks/rules-of-hooks': 'react/rules-of-hooks',
  'react-hooks/exhaustive-deps': 'react/exhaustive-deps',
  'react-refresh/only-export-components': 'react/only-export-components',
}

// eslint-plugin-react rules with no native port run through the JS plugin. `react` and
// `react-hooks` are reserved plugin names in oxlint, so the JS plugins are aliased.
const JS_PLUGIN_RULES: Partial<Record<string, string>> = {
  'react/no-deprecated': 'react-js/no-deprecated',
  'react/prop-types': 'react-js/prop-types',
}

const rules: RuleMap = {}
// Presets switch a rule off when a better replacement is enabled alongside it, e.g.
// typescript-eslint disables every base rule it supersedes with a type-aware version.
// Those opt-outs are emitted explicitly so oxlint's default category doesn't re-enable them.
const disabledByPresets: string[] = []

for (const [name, entry] of Object.entries(upstream)) {
  if (entry == null || UNSUPPORTED.has(name)) continue
  const severity = Array.isArray(entry) ? entry[0] : entry

  if (severity === 'off' || severity === 0) {
    disabledByPresets.push(name)
    continue
  }

  const target =
    NATIVE_EQUIVALENT[name] ??
    JS_PLUGIN_RULES[name] ??
    (name.startsWith('react-hooks/') ? `react-hooks-js/${name.slice('react-hooks/'.length)}` : name)
  rules[target] = entry
}

// oxlint treats a typescript-eslint extension rule and the base rule it supersedes as one
// rule, so the base rule's opt-out would switch the typescript-eslint version off too.
for (const name of disabledByPresets) {
  if (`@typescript-eslint/${name}` in rules) continue
  rules[name] ??= 'off'
}

export default defineConfig({
  ignorePatterns: ['dist'],
  env: { browser: true, es2024: true },
  plugins: ['typescript', 'react', 'jsx-a11y'],
  // Only the preset rules above apply, as with ESLint; oxlint's default category would add
  // native duplicates of the React Compiler rules.
  categories: { correctness: 'off' },
  jsPlugins: [
    { name: 'react-js', specifier: 'eslint-plugin-react' },
    { name: 'react-hooks-js', specifier: 'eslint-plugin-react-hooks' },
  ],
  settings: {
    react: { version: '19.3' },
  },
  overrides: [
    {
      files: ['**/*.{ts,tsx}'],
      rules,
    },
  ],
})
