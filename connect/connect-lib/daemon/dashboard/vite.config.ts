import { fileURLToPath, URL } from 'node:url';
import tailwindcss from '@tailwindcss/vite';
import react from '@vitejs/plugin-react';
import { defineConfig } from 'vite';
import { viteSingleFile } from 'vite-plugin-singlefile';

// The daemon serves the dashboard with `include_str!("../dashboard/dist/index.html")`,
// so the build must be exactly one self-contained file: JS, CSS and
// datum-ui's font files all inlined (viteSingleFile + an unbounded
// assetsInlineLimit). The shell carries no secrets, so inlining is safe.
export default defineConfig({
  plugins: [tailwindcss(), react(), viteSingleFile()],
  resolve: {
    alias: { '@': fileURLToPath(new URL('./src', import.meta.url)) },
  },
  build: {
    assetsInlineLimit: Number.MAX_SAFE_INTEGER,
    chunkSizeWarningLimit: 4096,
  },
  server: {
    // `bun run dev` against a daemon already running on the default port.
    proxy: { '/v1': 'http://127.0.0.1:47780' },
  },
});
