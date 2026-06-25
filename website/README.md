# Trillian website

Landing page for [Trillian](https://github.com/cpthappy/trillian), built with [Astro](https://astro.build).

## Develop

```bash
cd website
npm install
npm run dev
```

The dev server runs on `http://localhost:4321` with no base path.

## Build

```bash
# For GitHub Pages (base path /trillian/)
npm run build

# For local / standalone deploy (base path /)
ASTRO_BASE="/" npm run build
```

Output goes to `dist/`. It's a fully static site — serve `dist/` with any static file server.

## Deploy

Deployment is automatic via GitHub Actions (`.github/workflows/deploy-website.yml`).

On push to `master` or `develop` (touching `website/**`), the workflow builds the site
and deploys to GitHub Pages at **<https://cpthappy.github.io/trillian/>**.

### One-time GitHub setup

1. Repo → **Settings** → **Pages** → **Source**: "GitHub Actions"
2. Push to `master` — the workflow runs automatically
3. The site appears at `https://cpthappy.github.io/trillian/`

### Custom domain (optional)

1. Repo → **Settings** → **Pages** → **Custom domain**: enter e.g. `trillian.42-grad.com`
2. Add a CNAME record: `trillian.42-grad.com → cpthappy.github.io`
3. Set `ASTRO_BASE="/"` in the workflow env (or use a domain config) so paths are root-relative
