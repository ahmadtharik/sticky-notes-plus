import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';

// vanilla Svelte + Vite config (no SvelteKit)
const config = {
  preprocess: vitePreprocess(),
};

export default config;