declare module 'eslint-plugin-jsx-a11y' {
  type Severity = 'off' | 'warn' | 'error' | 0 | 1 | 2
  type RuleEntry = Severity | [Severity, ...unknown[]]

  const plugin: {
    flatConfigs: Record<'recommended' | 'strict', { rules: Partial<Record<string, RuleEntry>> }>
  }
  export default plugin
}
