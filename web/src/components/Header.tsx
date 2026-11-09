import { ActionIcon, CloseButton, Kbd, TextInput, useMantineColorScheme } from '@mantine/core'
import { useDebouncedCallback, useWindowEvent } from '@mantine/hooks'
import { IconMoon, IconSearch, IconSun } from '@tabler/icons-react'
import { useRef, useState } from 'react'

import { type Route, href, navigate } from '../router'
import { Link } from './Link'

/** The search box, on every screen: typing searches as you go. */
export function Header({ route }: { route: Route }) {
  const routeQ = route.name === 'search' ? route.q : null
  const [value, setValue] = useState(routeQ ?? '')
  const [shownQ, setShownQ] = useState(routeQ)
  const input = useRef<HTMLInputElement>(null)
  const { toggleColorScheme } = useMantineColorScheme()

  // Back and forward between searches show the search of the entry.
  if (routeQ !== shownQ) {
    setShownQ(routeQ)
    // (Not while typing: the address has the search trimmed.)
    if (routeQ !== null && routeQ !== value.trim()) setValue(routeQ)
  }

  const go = (q: string, replace: boolean) => {
    navigate(href.search(q.trim()), { replace })
  }
  // While typing on the search screen the entry is replaced; from another screen a new one is added.
  const typed = useDebouncedCallback((q: string) => {
    go(q, routeQ !== null)
  }, 250)

  useWindowEvent('keydown', (e) => {
    const target = e.target as HTMLElement | null
    const typing =
      target instanceof HTMLInputElement ||
      target instanceof HTMLTextAreaElement ||
      target?.isContentEditable === true
    if (e.key === '/' && !typing && !e.ctrlKey && !e.metaKey && !e.altKey) {
      e.preventDefault()
      input.current?.focus()
      input.current?.select()
    }
  })

  return (
    <header className="header">
      <div className="header-inner">
        <Link to="/" className="brand" aria-label="LRCLIB mirror, home">
          <span className="brand-mark" aria-hidden="true" />
          <span className="brand-name">lrclib</span>
        </Link>
        <form
          className="header-search"
          role="search"
          onSubmit={(e) => {
            e.preventDefault()
            typed.cancel()
            go(value, routeQ !== null)
            input.current?.blur()
          }}
        >
          <TextInput
            ref={input}
            value={value}
            onChange={(e) => {
              setValue(e.currentTarget.value)
              typed(e.currentTarget.value)
            }}
            placeholder="Song, artist or album"
            aria-label="Search lyrics"
            size="md"
            radius="xl"
            leftSection={<IconSearch size={18} stroke={1.8} />}
            rightSection={
              value ? (
                <CloseButton
                  aria-label="Clear the search"
                  onClick={() => {
                    setValue('')
                    typed.cancel()
                    go('', routeQ !== null)
                    input.current?.focus()
                  }}
                />
              ) : (
                <Kbd size="xs" className="search-kbd">
                  /
                </Kbd>
              )
            }
            classNames={{ input: 'search-input' }}
            autoComplete="off"
            spellCheck={false}
          />
        </form>
        <ActionIcon
          size="lg"
          radius="xl"
          onClick={toggleColorScheme}
          aria-label="Switch between light and dark"
        >
          <IconSun size={18} className="icon-light" />
          <IconMoon size={18} className="icon-dark" />
        </ActionIcon>
      </div>
    </header>
  )
}
