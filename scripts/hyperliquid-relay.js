// Atlas's Hyperliquid relay: a Cloudflare Worker that forwards the engine's Hyperliquid requests,
// so they leave from Cloudflare's addresses instead of Render's shared ones (Hyperliquid limits
// requests per address and answered Render Frankfurt with 429 Too Many Requests).
//
// Only POST /info and POST /exchange are forwarded, only to api.hyperliquid.xyz, and only with the
// secret Atlas sends in x-atlas-relay. Set RELAY_SECRET as a Worker secret (Settings → Variables
// and Secrets) and the same value as HYPERLIQUID_RELAY_SECRET on the engine; the Worker's URL is
// HYPERLIQUID_RELAY_URL.
export default {
  async fetch(request, env) {
    const { pathname } = new URL(request.url);
    if (request.method !== 'POST' || (pathname !== '/info' && pathname !== '/exchange')) {
      return new Response('Not found', { status: 404 });
    }
    const secret = env.RELAY_SECRET;
    if (!secret || secret.length < 16 || request.headers.get('x-atlas-relay') !== secret) {
      return new Response('Forbidden', { status: 403 });
    }
    const body = await request.text();
    if (body.length > 64 * 1024) {
      return new Response('Too large', { status: 413 });
    }
    const upstream = await fetch(`https://api.hyperliquid.xyz${pathname}`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body,
    });
    return new Response(upstream.body, {
      status: upstream.status,
      headers: { 'content-type': upstream.headers.get('content-type') ?? 'application/json' },
    });
  },
};
