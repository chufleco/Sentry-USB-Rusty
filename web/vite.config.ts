import path from "path"
import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
  esbuild: {
    // Strip development noise from the production bundle without
    // touching console.warn / console.error — those carry real user-
    // facing diagnostics (e.g. 3D preview fallbacks in CommunityWraps).
    pure: ['console.log', 'console.debug'],
  },
  build: {
    rollupOptions: {
      output: {
        // Named vendor chunks so an OTA update that only changes app
        // code doesn't bust the cache for libraries that haven't
        // moved. Each library lives in its own content-hashed file.
        manualChunks: {
          'vendor-react': ['react', 'react-dom', 'react-router-dom'],
          'vendor-charts': ['recharts'],
          'vendor-maps': ['leaflet'],
          'vendor-term': ['@xterm/xterm', '@xterm/addon-fit'],
          'vendor-icons': ['lucide-react'],
        },
      },
    },
  },
  server: {
    proxy: {
      '/api': 'http://localhost:8788',
      '/TeslaCam': 'http://localhost:8788',
    },
  },
})
