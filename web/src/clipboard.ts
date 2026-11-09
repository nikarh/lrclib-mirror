import { notifications } from '@mantine/notifications'

/** Copies `text`; also where the Clipboard API isn't there (a page served over plain HTTP). */
export async function copyText(text: string) {
  if (window.isSecureContext && 'clipboard' in navigator) {
    await navigator.clipboard.writeText(text)
    return
  }
  const area = document.createElement('textarea')
  area.value = text
  area.setAttribute('readonly', '')
  area.style.position = 'fixed'
  area.style.opacity = '0'
  document.body.append(area)
  area.select()
  try {
    // Deprecated, and the only way without the Clipboard API.
    // eslint-disable-next-line @typescript-eslint/no-deprecated
    if (!document.execCommand('copy')) throw new Error('The browser refused to copy')
  } finally {
    area.remove()
  }
}

/** Copies `text` and says so. */
export function copyAndTell(text: string, what: string) {
  copyText(text).then(
    () => notifications.show({ message: `Copied ${what}`, color: 'accent' }),
    (e: unknown) =>
      notifications.show({
        title: 'Couldn’t copy',
        message: e instanceof Error ? e.message : String(e),
        color: 'red',
      }),
  )
}

/** Saves `text` as a file named `name`. */
export function download(text: string, name: string, type = 'text/plain') {
  const url = URL.createObjectURL(new Blob([text], { type: `${type};charset=utf-8` }))
  const a = document.createElement('a')
  a.href = url
  a.download = name
  a.click()
  setTimeout(() => {
    URL.revokeObjectURL(url)
  }, 1000)
}
