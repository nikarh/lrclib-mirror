import { useEffect } from 'react'

import { Header } from './components/Header'
import { useLocation, useScrollRestoration } from './router'
import { ArtistScreen } from './screens/ArtistScreen'
import { SearchScreen } from './screens/SearchScreen'
import { TrackScreen } from './screens/TrackScreen'

export function App() {
  const { route } = useLocation()
  useScrollRestoration()

  useEffect(() => {
    if (route.name === 'search') document.title = route.q ? `${route.q} · LRCLIB` : 'LRCLIB'
    if (route.name === 'artist') document.title = `${route.artist} · LRCLIB`
  }, [route])

  return (
    <>
      <Header route={route} />
      <main className="page">
        {route.name === 'search' && <SearchScreen q={route.q} />}
        {route.name === 'track' && <TrackScreen key={route.id} id={route.id} />}
        {route.name === 'artist' && <ArtistScreen key={route.artist} artist={route.artist} />}
      </main>
    </>
  )
}
