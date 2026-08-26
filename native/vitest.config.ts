import { svelte } from '@sveltejs/vite-plugin-svelte';
import { svelteTesting } from '@testing-library/svelte/vite';
import { defineConfig } from 'vitest/config';

export default defineConfig({
  plugins: [svelte(), svelteTesting()],
  test: {
    environment: 'jsdom',
    environmentOptions: {
      jsdom: { url: 'http://localhost/' }
    },
    include: ['src/**/*.interaction.test.ts', 'src/**/*.unit.test.ts'],
    setupFiles: ['./src/test/setup.ts']
  }
});
