import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { defineConfig, loadEnv } from 'vite';

export default defineConfig(({ mode }) => {
  const env = loadEnv(mode, process.cwd(), '');
  return {
    plugins: [
      sveltekit({
        preprocess: vitePreprocess(),
        adapter: adapter({ fallback: 'index.html' })
      })
    ],
    server: {
      watch: {
        usePolling: true
      },
      proxy: {
        '/api': {
          target: env.VITE_PROXY_TARGET || 'http://localhost:8090',
          changeOrigin: true,
          ws: false
        },
        '/health': {
          target: env.VITE_PROXY_TARGET || 'http://localhost:8090',
          changeOrigin: true
        },
        '/v1': {
          target: env.VITE_PROXY_TARGET || 'http://localhost:8090',
          changeOrigin: true
        }
      }
    }
  };
});
