# Project page

This directory contains the standalone project landing page, separate from the
Rust-embedded operational pages in `web/`. It uses plain HTML and CSS, system
fonts, and no JavaScript, package installation, or build step. Asset URLs are
relative so the same page works at `/` or a GitHub Pages project subpath.

Preview from the repository root:

```sh
python3 -m http.server 8000 --directory site
# Open http://localhost:8000
```

## Languages

`index.html` is the English page and `zh.html` is the Simplified Chinese page.
Each navigation bar links to the other language without requiring JavaScript.
Both pages share `style.css` and use relative language links so switching also
works under a GitHub Pages project subpath. Keep both pages in sync when
changing content, including titles, descriptions, and accessibility labels.
The documentation links currently point to the existing English documentation.

## Publishing on GitHub Pages

The GitHub mirror referenced by the project docs is `AgentaaU/forge-bot`.
A repository administrator must enable **Settings → Pages → Build and
deployment → Source → GitHub Actions** on that mirror and allow the
`github-pages` environment to deploy from `main`.

After these files reach the mirror's `main`, `.github/workflows/pages.yml`
uploads `site/` and deploys it with GitHub's Pages actions. It can also be run
manually from the Actions tab. The deployment reports the published URL;
without a custom domain, it is expected at
`https://agentaau.github.io/forge-bot/`. This address is not a claim that the
site has already been published.

The deployment job is skipped on Forgejo because GitHub Pages needs GitHub's
API and OIDC provider. No GitHub token is stored in this repository. Static
files can also be served by any ordinary web server.

When updating the page, keep its feature descriptions aligned with `README.md`
and check mobile layouts, keyboard navigation, and the documentation links.
GitHub workflow setup reference:
https://docs.github.com/en/pages/getting-started-with-github-pages/using-custom-workflows-with-github-pages

## Startup video

Both languages stream the 24-second silent introduction from the Cloudflare
Worker CDN. The storyboard follows one concrete run: a maintainer's bug report,
your `@agent` reply, the gateway, the agent's test-and-diff terminal session,
and the reply in the thread. The MP4 is generated locally, published to the CDN,
and intentionally not committed to Git, so `site/media/startup.mp4` is listed in
`.gitignore`. The local `<source>` stays as a fallback: the Pages workflow
materializes it from the page's primary CDN URL before publishing, and the
renderer recreates it for local previews. The poster is committed, playback
requires an explicit user action, and `preload="none"` avoids fetching the
video before playback. English and Chinese transcripts are included below it.
No audio or external footage is used.

### CDN hosting

The primary video is stored in Cloudflare Workers KV under the
`shylock/agent-earn-money` Cloudflare account and served by the
`forge-bot-cdn` Worker at:

```text
https://forge-bot-cdn.forge-bot-media.workers.dev/media/startup-v2.mp4
```

The Worker adds explicit HTTP byte-range support so desktop and mobile
browsers (including iOS Safari) can seek. The `*-media.workers.dev` host is
provided by Cloudflare; no custom domain or DNS zone is required. The Worker
source, pinned Wrangler commands, and upload/verification steps live in
`contrib/cloudflare-media/`.

### Publishing a replacement

The Worker sends `Cache-Control: public, max-age=31536000, immutable`, so a URL
must never change content. Publish a replacement under a new versioned key:

1. Render the new asset (below) and note its SHA-256.
2. Upload it under a new key, for example `media/startup-v3.mp4` (the current
   primary is `media/startup-v2.mp4`). The Worker derives the content type from
   the `media/*.mp4` file name, so no Worker edit or redeploy is needed. See
   `contrib/cloudflare-media/README.md` for the exact commands.
3. Update the primary `<source>` and download URLs on both `index.html` and
   `zh.html` to the new key. The Pages workflow reads the primary URL from
   `index.html`, so the fallback follows automatically.
4. Verify the new URL: the full GET matches the local file byte-for-byte,
   `Range` requests return `206` with the expected bytes, out-of-range returns
   `416`, and `HEAD` reports the correct length. Old keys can be deleted once no
   page references them.

To regenerate the local video and the committed poster, install Python Pillow
and ffmpeg, and use the JetBrains Mono Regular and Bold fonts at the paths in
the renderer:

```sh
python3 contrib/render-startup-video.py
ffprobe -v error -show_entries format=duration,size site/media/startup.mp4
```

The renderer writes the 1080p MP4 to `site/media/startup.mp4` (gitignored) and
the poster to `site/media/startup-poster.webp`, and creates six animated scenes
at 1920x1080, 30 fps, encoded as H.264 with yuv420p and MP4 fast-start metadata
for browser playback. Keep the page transcripts aligned with the scene text. The
pages remain plain static HTML/CSS; `.github/workflows/pages.yml` fails the
deploy instead of publishing a broken local fallback when the CDN video cannot
be materialized.
