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
