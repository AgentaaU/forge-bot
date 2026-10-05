// Static media host for the forge-bot project pages.
//
// Assets live in Workers KV under the `media/` prefix. This Worker serves them
// with explicit HTTP byte-range support so desktop and mobile browsers (notably
// iOS Safari) can seek and stream the MP4 without downloading it in full.
//
// Deploy from this directory with the pinned CLI version:
//     npx wrangler@4.147.0 deploy
// after uploading the assets to the bound KV namespace.
//
// Media keys are versioned (for example `media/startup.mp4` or
// `media/startup-v2.mp4`). The Worker accepts any `media/` key with a known
// extension and derives the content type, so publishing a new version only
// needs a KV upload and a page URL update -- no Worker edit or redeploy.

const CONTENT_TYPES = {
  mp4: 'video/mp4',
  webp: 'image/webp',
};

const MEDIA_KEY = /^media\/[A-Za-z0-9][A-Za-z0-9._-]*\.(mp4|webp)$/;

function contentTypeFor(key) {
  const match = MEDIA_KEY.exec(key);
  return match ? CONTENT_TYPES[match[1]] : undefined;
}

function parseRange(header, size) {
  const match = /^bytes=(\d*)-(\d*)$/.exec(header.trim());
  if (!match) return null;
  const [, rawStart, rawEnd] = match;
  if (rawStart === '' && rawEnd === '') return null;
  if (rawStart === '') {
    const suffix = Number.parseInt(rawEnd, 10);
    if (!Number.isFinite(suffix) || suffix <= 0) return null;
    return { start: Math.max(0, size - suffix), end: size - 1 };
  }
  const start = Number.parseInt(rawStart, 10);
  const end = rawEnd === '' ? size - 1 : Math.min(Number.parseInt(rawEnd, 10), size - 1);
  if (!Number.isFinite(start) || start >= size || start > end) return null;
  return { start, end };
}

function baseHeaders(contentType, size) {
  return {
    'Content-Type': contentType,
    'Accept-Ranges': 'bytes',
    'Cache-Control': 'public, max-age=31536000, immutable',
    'Access-Control-Allow-Origin': '*',
    'Content-Length': String(size),
  };
}

export default {
  async fetch(request, env) {
    const { pathname } = new URL(request.url);
    const key = pathname.replace(/^\/+/, '');
    const contentType = contentTypeFor(key);
    if (!contentType) {
      return new Response('Not found\n', { status: 404, headers: { 'Content-Type': 'text/plain' } });
    }
    if (request.method !== 'GET' && request.method !== 'HEAD') {
      return new Response('Method not allowed\n', {
        status: 405,
        headers: { Allow: 'GET, HEAD', 'Content-Type': 'text/plain' },
      });
    }

    const value = await env.MEDIA.get(key, { type: 'arrayBuffer' });
    if (value === null) {
      return new Response('Not found\n', { status: 404, headers: { 'Content-Type': 'text/plain' } });
    }
    const size = value.byteLength;
    const headers = baseHeaders(contentType, size);
    const requested = request.headers.get('Range');

    if (requested) {
      const range = parseRange(requested, size);
      if (!range) {
        return new Response(null, {
          status: 416,
          headers: { ...headers, 'Content-Range': `bytes */${size}`, 'Content-Length': '0' },
        });
      }
      const { start, end } = range;
      const length = end - start + 1;
      return new Response(request.method === 'HEAD' ? null : value.slice(start, end + 1), {
        status: 206,
        headers: {
          ...headers,
          'Content-Range': `bytes ${start}-${end}/${size}`,
          'Content-Length': String(length),
        },
      });
    }

    return new Response(request.method === 'HEAD' ? null : value, { status: 200, headers });
  },
};
