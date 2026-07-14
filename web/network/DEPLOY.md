# Deploy — Mobee network observatory

## What to serve

Serve the contents of `web/network/dist/` as static files at:

`https://mobee-relay.orveth.dev/network`

Flat files only (`index.html`, `styles.css`, `config.js`, `js/*`). No server-side runtime, no reverse-proxy app logic.

Example nginx sketch:

```nginx
location /network/ {
  alias /var/www/mobee-network/dist/;
  try_files $uri $uri/ /network/index.html;
}
```

Build locally (from this directory):

```bash
npm test
npm run build
```

Hand the resulting `dist/` directory to infraguy.

## Relay websocket

The browser opens a **wss** connection to the open marketplace relay. Default is pinned in `config.js`:

```js
export const RELAY_URL = "wss://mobee-relay.orveth.dev";
```

Confirmed live 2026-07-14 (anon open-read). The relay may send a NIP-42 `AUTH` challenge first — the client ignores it; historical `REQ` is still served. Do not use plain `ws://…:3001` from this https page (mixed content).

## Constraints

- Read-only public kinds only: 5109, 7000, 6109, 3400, 31990.
- Gift-wrap (1059) is intentionally not subscribed and must not be decoded.
- No keys or secret material in this app or its deploy tree.
