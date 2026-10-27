import vue from '@vitejs/plugin-vue';
import { defineConfig } from 'vite';

// Tauri expects a fixed dev port and serves the build from ../dist.
export default defineConfig({
  plugins: [vue()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: { ignored: ['**/src-tauri/**'] },
  },
  envPrefix: ['VITE_', 'TAURI_ENV_'],
  build: {
    outDir: 'dist',
    target: 'es2022',
    sourcemap: true,
    // A desktop app loads its chunks from disk; the result grid chunk (AG Grid) alone is about 1.2 MB and loads on first use.
    chunkSizeWarningLimit: 1300,
  },
});
