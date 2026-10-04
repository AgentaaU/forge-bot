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

Both languages embed the 24-second silent introduction in `media/startup.mp4`.
The first video source is jsDelivr's CDN URL for the GitHub mirror's `main`;
the second is the same asset served locally by Pages. The poster is local,
playback requires an explicit user action, and `preload="none"` avoids fetching
the video before playback. English and Chinese transcripts are included below
it. No audio or external footage is used.

The CDN becomes available after this change is merged and synchronized to
`AgentaaU/forge-bot` on GitHub. No CDN account or upload secret is needed.
Check that the CDN URL returns `200` and `Content-Type: video/mp4` after
synchronization. Until then, the local video source and download link work.
jsDelivr caches branch URLs; when replacing the video, use a new asset filename
and update both pages so viewers receive the new version immediately.

To regenerate the committed video and poster, install Python Pillow and ffmpeg,
and use the JetBrains Mono Regular font at the path in the renderer:

```sh
python3 contrib/render-startup-video.py
ffprobe -v error -show_entries format=duration,size site/media/startup.mp4
```

The renderer creates six animated scenes at 1280×720, 12 fps, encoded as H.264
with `yuv420p` and MP4 fast-start metadata for browser playback. It is a separate
asset authoring step; the website and Pages deployment still need no build.
