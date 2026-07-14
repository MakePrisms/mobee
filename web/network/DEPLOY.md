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

The browser opens a **wss** connection to the open marketplace relay. Default is the config constant in `config.js`:

```js
export const RELAY_URL = "wss://mobee-relay.orveth.dev";
```

The page is served over https, so a plain `ws://…:3001` endpoint will be blocked as mixed content. Pin/update `RELAY_URL` to the TLS-terminated wss URL once confirmed.

## Constraints

- Read-only public kinds only: 5109, 7000, 6109, 3400, 31990.
- Gift-wrap (1059) is intentionally not subscribed and must not be decoded.
- No keys or secret material in this app or its deploy tree.
