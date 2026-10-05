# Cloudflare media CDN

The project pages stream the startup video from a small Cloudflare Worker
(`forge-bot-cdn`) that reads the asset from Workers KV and serves it with HTTP
byte-range support. This directory is the deployable source for that Worker.

- Worker URL: `https://forge-bot-cdn.forge-bot-media.workers.dev`
- Video: `https://forge-bot-cdn.forge-bot-media.workers.dev/media/startup-v2.mp4`
- Poster: `https://forge-bot-cdn.forge-bot-media.workers.dev/media/startup-poster.webp`
- KV namespace: `forge-bot-media` (`418b77163d444d4db0fb1a25b45521d8`)
- KV keys: `media/startup-v2.mp4`, `media/startup-poster.webp`

The MP4 is generated locally and intentionally not committed to Git. The Worker
accepts any `media/*.mp4` or `media/*.webp` key and sets the content type from
the file extension, so a versioned replacement only needs a KV upload and a page
URL update; no Worker edit or redeploy is required.

## Working directory and CLI version

Run every command below from this directory. The commands pin Wrangler
`4.147.0`, which is tested with the syntax shown; an unpinned `npx wrangler`
can resolve to a version with different KV subcommands and flags.

```sh
cd contrib/cloudflare-media
```

The Worker also sets the KV values' content types, so no `--content-type` flag
is passed (and recent Wrangler versions reject it).

## Credentials

The Cloudflare account credentials are not stored in this repository. Provide a
scoped API token, or the account's Global API Key plus email, through the
environment before running Wrangler. Never commit the key.

```sh
export CLOUDFLARE_API_KEY=...      # Global API Key (kept out of Git)
export CLOUDFLARE_EMAIL=...
export CLOUDFLARE_ACCOUNT_ID=1d114c565b6f8f61e6626c21bb94f404
```

## Publishing an asset

`wrangler kv key put` reads the value from `--path` and uploads it to the
production namespace only with `--remote`; without that flag Wrangler writes to
the local emulated KV store instead of uploading the binary.

```sh
# From contrib/cloudflare-media.
npx wrangler@4.147.0 kv key put media/startup-v2.mp4 \
  --path ../../site/media/startup.mp4 \
  --namespace-id 418b77163d444d4db0fb1a25b45521d8 --remote
npx wrangler@4.147.0 kv key put media/startup-poster.webp \
  --path ../../site/media/startup-poster.webp \
  --namespace-id 418b77163d444d4db0fb1a25b45521d8 --remote

# Deploy the Worker.
npx wrangler@4.147.0 deploy
```

To publish a replacement video, upload the same file under a new key (for
example `media/startup-v3.mp4`; the current primary is `media/startup-v2.mp4`)
and update the page URLs. The extension-based Worker serves it immediately and
the deploy step is only needed when the Worker code itself changes. Delete old
keys once no page references them.

## Verifying the upload

KV writes are eventually consistent: a freshly uploaded asset can return `404`
for up to about a minute. Wait, then download the asset from the Worker and
compare it with the local file:

```sh
sha256sum ../../site/media/startup.mp4
curl -fsSL https://forge-bot-cdn.forge-bot-media.workers.dev/media/startup-v2.mp4 \
  -o /tmp/startup.mp4
sha256sum /tmp/startup.mp4

curl -sI  https://forge-bot-cdn.forge-bot-media.workers.dev/media/startup-v2.mp4
curl -s -o /dev/null -w 'range=%{http_code}\n' -r 0-1023 \
  https://forge-bot-cdn.forge-bot-media.workers.dev/media/startup-v2.mp4   # 206
```

The two `sha256sum` outputs must match. A `Range` response must be `206`, an
out-of-range request must be `416`, and `HEAD` must report the same length as
the local file.

## Creating the namespace from scratch

If the KV namespace is ever recreated, create it and update the `id` in
`wrangler.toml`:

```sh
cd contrib/cloudflare-media
npx wrangler@4.147.0 kv namespace create forge-bot-media
```

The Worker name is part of the public URL, so keep it `forge-bot-cdn` unless the
URL embedded in `site/index.html` and `site/zh.html` is updated at the same time.
