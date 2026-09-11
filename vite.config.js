import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

// https://vitejs.dev/config/
export default defineConfig({
  plugins: [svelte()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      // never let vite watch the Rust build tree (Windows file locks → EBUSY)
      ignored: ['**/src-tauri/**'],
    },
  },
});