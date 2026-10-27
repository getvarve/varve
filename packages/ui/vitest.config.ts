import vue from '@vitejs/plugin-vue';
import { defineConfig } from 'vitest/config';

export default defineConfig({
  plugins: [vue()],
  test: {
    environment: 'jsdom',
    globals: true,
    // Stylesheets stay empty in tests, except when a test reads one as text (`?raw`).
    css: { include: [/\.css\?raw$/] },
  },
});
