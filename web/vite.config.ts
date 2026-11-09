import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// The web search against an lrclib-mirror running elsewhere, instead of the local one:
// LRCLIB_MIRROR_REMOTE=https://lrclib.example (`mise run dev:remote`).
const remote = process.env.LRCLIB_MIRROR_REMOTE

// https://vite.dev/config/
export default defineConfig({
  base: '/',
  plugins: [react()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    // Built into the binary: no source maps.
    sourcemap: false,
  },
  server: {
    proxy: {
      '/api': remote
        ? { target: remote, changeOrigin: true, secure: true }
        : { target: 'http://127.0.0.1:3300' },
    },
  },
})
