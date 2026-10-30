import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import { readFileSync } from 'node:fs';

// Fonts are build assets; their full redistribution notices must travel with dist.
const fontLicenses = {
  name: 'bundled-font-licenses',
  generateBundle() {
    const manifest = JSON.parse(readFileSync(new URL('./src/fonts/manifest.json', import.meta.url)));
    for (const font of manifest) {
      const family = font.package.split('/').pop();
      this.emitFile({ type: 'asset', fileName: `assets/${family}-LICENSE.txt`,
        source: readFileSync(new URL(`./src/fonts/${family}/LICENSE.txt`, import.meta.url)) });
    }
  },
};

export default defineConfig({ plugins: [react(), fontLicenses], build: { outDir: 'dist' } });
