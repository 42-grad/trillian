// @ts-check
import { defineConfig } from 'astro/config';

// GitHub Pages serves the site under /trillian/, so we need a matching base path.
// For local dev or other deploy targets set ASTRO_BASE="/" in the environment.
const base = process.env.ASTRO_BASE ?? '/trillian/';

export default defineConfig({
  base,
  trailingSlash: 'ignore',
});
