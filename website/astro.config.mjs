// @ts-check
import { defineConfig } from 'astro/config';

// Custom domain serves from root; GitHub Pages without a custom domain
// serves under /trillian/. Override with ASTRO_BASE if needed.
const base = process.env.ASTRO_BASE ?? '/';

export default defineConfig({
  base,
  trailingSlash: 'ignore',
});
