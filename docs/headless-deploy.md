# Headless deployment (Camoufox sidecar)

This runs the same monitoring core the desktop app uses (`crates/apw-core`),
without Tauri, a window, or a local speaker, on a plain Linux server or VPS.

## Why this exists, and why not a Cloudflare Worker

Apple's store pages run a bot-check script (`shop/shld/v2_1/verify.js`)
before the pickup endpoint (`/shop/retail/pickup-message`) answers; a request without a
session that passed the check gets HTTP 541. That check only passes inside a
real browser JS engine with a DOM. A Cloudflare Worker is a bare V8 isolate
with no DOM, so it cannot run the check at all — there is no way to make
this work as a plain Worker. Cloudflare's Browser Rendering product (a real
managed Chrome instance) could in principle clear the check, but it is a
different, metered product with per-session cost and coarse (≥1 minute) cron
granularity, not a plain Worker rewrite.

This deployment instead runs a real (patched, anti-fingerprinting) browser —
[Camoufox](https://camoufox.com/) — as a small always-on sidecar process,
and drives it the same way the desktop app drives its embedded Chromium: one
warmed-up page per region, reused across polls, serialized and throttled so
requests don't look like automation.

## Layout

```
crates/apw-headless/   Rust binary: watcher + Bark notifications + a thin
                        HTTP client that talks to the sidecar + web UI server
crates/apw-headless/web/   Web build of the desktop UI (Tauri API shim)
camoufox-sidecar/       Python process: owns the one real browser session
deploy/docker-compose.yml   Runs both together
deploy/k8s/             The same pod on Kubernetes
.github/workflows/images.yml   Publishes both images to GHCR from main
```

`crates/apw-headless` reuses `apw-core` (the watcher engine, config storage,
catalog, Bark notifier) completely unmodified — the only new piece on the
Rust side is `CamoufoxFetcher`, which implements the same `Fetcher` trait
`AppleChromiumFetcher` (the desktop app's in-process Chromium driver) does,
except it hands the browser work to the sidecar over a local HTTP call
instead of driving CDP in-process.

The sidecar is intentionally generic: it has no idea what a "store" or "part
number" is. Every request tells it, in plain terms, "make sure this URL has
been visited and has finished loading, then `fetch()` that URL from inside
the page." All Apple-specific knowledge (URLs, query parameters, which page
to warm up on) stays in `apw-core` / `apw-headless`, so there is exactly one
place to update if Apple changes its API.

## Configuration

Both the desktop app and this headless binary read the same
`settings.v2.json` shape, at the OS-conventional config directory
(`~/.config/apple-store-inventory-monitor/settings.v2.json` on Linux) unless
overridden by `APW_SETTINGS_PATH`. The easiest way to fill it in is the
[web UI](#web-ui). You can also copy a file from the desktop app, or write
one by hand.

Headless owns the file the way the desktop app does: the web UI saves to it
(atomically), and on start it tightens the directory to 0700 and the file to
0600, because the file holds the Bark device key. So the **directory must be
writable and owned by the container user (uid 10001)**. A read-only mount
(a Kubernetes Secret, a `:ro` bind mount) makes startup fail. Startup also
fails, with exit status 1, if the file exists but is unreadable, over 1 MiB,
or not valid JSON. That is deliberate, so a later save from the UI can't
replace your real target list with an empty one. A missing file is a normal
first start when the web UI is on. With the web UI off, a missing file is
also fatal, since nothing could add targets.

```json
{
  "locale": "zh_CN",
  "targets": [
    {
      "locale": "zh_CN",
      "storeNumber": "R390",
      "storeTitle": "上海-南京东路",
      "partNumber": "MG6X4CH/A",
      "productName": "iPhone 17 512GB 黑色"
    }
  ],
  "intervalSeconds": 30,
  "barkUrl": "https://api.day.app/<your-device-key>",
  "productBarkUrls": {},
  "soundEnabled": false,
  "openOnHit": "bag"
}
```

Field notes:

- `targets[]`: one entry per (region, store, model) combination to watch.
  `storeNumber` and `partNumber` are Apple's own identifiers — the desktop
  app's dropdowns are the easiest way to find them; the values also appear
  directly in `crates/apw-core/data/stores.json` and
  `crates/apw-core/data/products_<locale>.json`.
- `intervalSeconds`: minimum 5, defaults to 30 if lower or omitted. This is
  the *base* interval; the watcher adds jitter and backs off (up to 8x) on
  repeated full-cycle failures, same as the desktop app.
- `barkUrl` / `productBarkUrls`: same [Bark](https://bark.day.app/) push
  target the desktop app uses. `productBarkUrls` overrides the default
  per part number, e.g. `{"MG6X4CH/A": "https://api.day.app/other-key"}`.
- `soundEnabled`: read but has no effect here — there is no local speaker.
- `openOnHit`: `"none" | "bag" | "education_bag" | "product"`. The desktop
  app uses this to auto-open a browser tab; headless has no browser to open,
  so instead the chosen URL (bag page, education-store bag page, or the
  product page for `"product"`) is embedded as the tappable link in the
  Bark push.

## Web UI

Set `APW_WEB_PASSWORD` and the monitor also serves the desktop app's own
interface over HTTP, at `APW_WEB_ADDR` (default `127.0.0.1:8080`). Log in as
user `admin` with that password. From the UI you can pick region, store and
model, start or pause the watcher, retry now, change the interval, set the
default or per-model Bark address, send a test push, refresh the store and
model lists from Apple, and watch each poll cycle live.

It is the same React code as the desktop app (`src/`), unmodified. The web
build swaps Tauri's `invoke`/`listen` for HTTP calls and a server-sent event
stream (`crates/apw-headless/web/tauri-shim.ts`), and the server implements
the same command names as the desktop backend (`crates/apw-headless/src/web.rs`).
Some controls have no headless counterpart. Using them logs a "not available
in headless mode" message:

- the alert sound toggle (no speaker; it is saved but does nothing)
- Clash route settings, node list and node test
- the in-app updater (deploy a newer image instead)

Security model:

- HTTP Basic auth on every request, static files included. It is plain
  HTTP, so keep the listener on loopback and reach it through
  `kubectl port-forward` or an SSH tunnel. If you put it behind a reverse
  proxy, terminate TLS there.
- The password still matters on loopback, because any web page you visit can
  send requests to `127.0.0.1`. Commands only accept
  `Content-Type: application/json`, so a cross-site form can't trigger them,
  and a cross-site `fetch` would need a CORS preflight the server never
  answers. Pages are served with `X-Frame-Options: DENY`.
- Use a long random password, e.g. `openssl rand -base64 24`. There is no
  rate limiting.

Build the UI locally (from the repo root) if you run the binary outside
Docker. Then point `APW_WEB_DIR` at `crates/apw-headless/web/dist`:

```bash
pnpm install
pnpm exec vite build --config crates/apw-headless/web/vite.config.ts
```

## Running it

Prepare the data directory and the web UI password, then start:

```bash
mkdir -p deploy/data
# optional: start from your desktop app's settings
cp ~/.config/apple-store-inventory-monitor/settings.v2.json deploy/data/
# the monitor container runs as uid 10001 and must own the directory
sudo chown -R 10001:10001 deploy/data
echo "APW_WEB_PASSWORD=$(openssl rand -base64 24)" > deploy/.env
cd deploy
docker compose up --build -d
```

This starts two containers:

- `sidecar`: the Camoufox browser process, port 8787, **not published to the
  host** — only `monitor` can reach it. Do not expose this port publicly;
  it has no authentication of its own.
- `monitor`: the Rust binary, keeping `./data/settings.v2.json`
  (bind-mounted from the host), pushing Bark alerts on hits, and serving the
  web UI on host port `127.0.0.1:8080`. From your own machine:
  `ssh -L 8080:127.0.0.1:8080 your-vps`, then open http://127.0.0.1:8080.

Logs (`docker compose logs -f monitor`) print one line per poll cycle and
one line per stock hit; Bark push failures go to stderr without stopping the
watcher, exactly like the desktop app's behavior of never letting a
notification failure affect the stock verdict.

## When Apple blocks your IP (HTTP 541)

If every target shows `请求被 Apple 拦截：HTTP 541`, Apple has flagged the
exit IP itself, not the browser session. To confirm, open
`https://www.apple.com/sg/shop/retail/pickup-message?pl=true&mts.0=regular&store=R669&parts.0=MG6J4X/A`
in a normal desktop browser on the same network. A real browser gets 541
too. Many browser sessions in a short time can trigger it; one test run hit
it after about 10 pod restarts in 30 minutes. A new browser fingerprint
doesn't help. Either pause the watcher in the web UI and wait (tens of
minutes to hours), or route the browser through a proxy with a different
exit IP:

- **docker compose:** set `SIDECAR_PROXY=http://[user:pass@]host:port` in
  `deploy/.env` (`https://` and `socks5://` also work).
- **Kubernetes:** create the optional Secret the sidecar already references,
  allow the proxy in `01-networkpolicy.yaml` (there is a commented example
  rule; private addresses and ports other than 443 are denied otherwise),
  then restart:

  ```bash
  kubectl -n apw-monitor create secret generic apw-proxy \
    --from-literal=url='http://[user:pass@]host:port'
  kubectl apply -f deploy/k8s/01-networkpolicy.yaml   # after editing the rule
  kubectl -n apw-monitor rollout restart deploy/apw-monitor
  ```

The sidecar log's first line says which route is in use
(`apple traffic via http://host:port` or `via direct connection`). Only the
browser uses the proxy. Bark pushes and the web UI's catalog refresh still
go out directly. A Clash instance on your own machine is reachable from a
local kind cluster only if Clash allows LAN connections; use the kind
network gateway as the host, e.g. `http://172.18.0.1:7890`. Camoufox logs a
`LeakWarning` recommending `geoip=True` when a proxy is set. That option
needs the `camoufox[geoip]` extra, which the image doesn't install (see
`requirements.txt`), so the browser's timezone and locale don't follow the
proxy's location.

## Known differences from the desktop app

- No sound alert, no tray icon, no in-app update check — none of those
  exist on a server.
- No "open browser tab on hit" — the destination URL rides along in the
  Bark push instead.
- Catalog refresh needs the web UI. The store and model lists start from
  the embedded snapshot the desktop app ships with; the UI's refresh button
  updates them from Apple and caches the result in `catalog-cache/` next to
  the settings file.
- No automatic route rotation. The desktop app (v1.0.18+) can rotate Clash
  nodes when Apple blocks an exit IP; the `network` settings that drive it
  are read but ignored here. Headless has one static exit proxy instead
  (see [When Apple blocks your IP](#when-apple-blocks-your-ip-http-541)).
- Delivery quotes (mainland China only, as in the desktop app) work the
  same way: set a province/city/district in the web UI and the "预计送货"
  column fills in. They are fetched through the same browser session after
  a successful pickup query, cached for an hour, and never change the
  pickup result.
- No nearby-store batching. The desktop app merges same-city stores into
  one location query; headless always issues one request per store.

## Running it on Kubernetes

`deploy/k8s/` has an alternative to the compose file: a `Namespace`
(`apw-monitor`, admission-enforced Pod Security Standards "restricted"),
a default-deny `NetworkPolicy` with DNS and HTTPS egress allowed, and a
single `Deployment` whose one pod runs both containers. **Requires
Kubernetes 1.29+.**

- The Camoufox sidecar is a *native sidecar container* (an init container
  with `restartPolicy: Always`). Kubernetes starts it first and only starts
  the monitor once the sidecar's `startupProbe` passes; on shutdown it stops
  the monitor first and the sidecar last, so the monitor's final in-flight
  poll cycle still has a browser to talk to.
- The sidecar binds `127.0.0.1` only and the monitor calls
  `http://127.0.0.1:8787`. Nothing outside the pod can reach the browser
  session, whether or not your CNI enforces `NetworkPolicy`. There is no
  `Service`. The web UI is loopback-only too (`APW_WEB_ADDR=127.0.0.1:8080`);
  `kubectl port-forward` still reaches it because it connects inside the
  pod's network namespace. Its probes are `exec` + `curl` against loopback, because
  kubelet `httpGet` probes connect to the pod IP and would never reach a
  loopback-only server.
- `replicas: 1` with `strategy: Recreate`: one browser session and one
  in-memory watcher state; a second replica, even briefly during a rolling
  update, would be a second concurrent Apple session.
- Trade-off: updating either image restarts the whole pod, so the browser
  session warms up again (seconds to tens of seconds).

```bash
# 1. Point the two REGISTRY/apw-*:REPLACE_WITH_TAG placeholders in
#    deploy/k8s/10-apw.yaml at real images (never :latest). CI publishes
#    both for every commit on main (see "Prebuilt images" below):
#      ghcr.io/<owner>/apw-camoufox-sidecar:sha-<short sha>
#      ghcr.io/<owner>/apw-monitor:sha-<short sha>
#    Or build and push them to your own registry:
docker build -t <registry>/apw-camoufox-sidecar:<tag> camoufox-sidecar/
docker build -t <registry>/apw-monitor:<tag> -f crates/apw-headless/Dockerfile .
docker push <registry>/apw-camoufox-sidecar:<tag>
docker push <registry>/apw-monitor:<tag>

# 2. Create the namespace and the web UI password Secret before the
#    Deployment (the pod reads it and won't start without it).
kubectl apply -f deploy/k8s/00-namespace.yaml
kubectl create secret generic apw-web \
  --namespace apw-monitor \
  --from-literal=password="$(openssl rand -base64 24)"

# 3. Check against your cluster before applying, then apply.
kubectl apply --dry-run=server -f deploy/k8s/
kubectl apply -f deploy/k8s/

# 4. Open the web UI (user "admin", the password from step 2), add targets.
kubectl -n apw-monitor get secret apw-web -o jsonpath='{.data.password}' | base64 -d; echo
kubectl -n apw-monitor port-forward deploy/apw-monitor 8080:8080
# then http://127.0.0.1:8080

# Optional: start from an existing settings file instead of an empty one.
kubectl -n apw-monitor exec deploy/apw-monitor -c monitor -- mkdir -p /data/config
kubectl -n apw-monitor cp ./settings.v2.json \
  "$(kubectl -n apw-monitor get pod -l app=apw-monitor -o name | cut -d/ -f2)":/data/config/settings.v2.json -c monitor
kubectl -n apw-monitor rollout restart deploy/apw-monitor
```

Settings and the refreshed catalog cache live on the `apw-data`
PersistentVolumeClaim (`/data/config/`), so UI changes survive restarts and
image updates. It needs a default StorageClass, or set `storageClassName`.

### Prebuilt images

`.github/workflows/images.yml` builds both images on every push to `main`
and pushes them to GitHub Container Registry, tagged `sha-<7-char commit
sha>` (no moving tags):

- `ghcr.io/<owner>/apw-monitor`
- `ghcr.io/<owner>/apw-camoufox-sidecar`

`<owner>` is the account that owns the repository the workflow runs in; on a
fork that is the fork's owner. Setup that GitHub doesn't do for you:

- On a fork, Actions stay disabled until you enable them in the fork's
  Actions tab.
- A newly published package can start out private. Then either make it
  public (package page → Package settings → Change visibility; this can't be
  undone) or give the cluster a pull secret: create a classic token with
  `read:packages`, run
  `kubectl -n apw-monitor create secret docker-registry ghcr --docker-server=ghcr.io --docker-username=<user> --docker-password=<token>`,
  and add `imagePullSecrets: [{name: ghcr}]` to the pod spec.
- Each `sha-<short sha>` tag is a multi-arch image (`linux/amd64` and
  `linux/arm64`), so the same manifest works on either kind of node. The
  single-arch builds it is made from stay in the registry as
  `sha-<short sha>-amd64` / `sha-<short sha>-arm64`.
- The arm64 builds run on GitHub's `ubuntu-24.04-arm` runners, which are
  free in public repositories.

Notes specific to the Kubernetes path:

- The `NetworkPolicy` resources only take effect if your cluster's CNI
  enforces them (Calico, Cilium, etc. do; some cloud default CNIs don't
  unless a companion controller is installed). They restrict egress only;
  the browser session's isolation comes from the loopback bind, not from
  them.
- Check `kubectl version` first. Below 1.28 the API server rejects
  `restartPolicy` on an init container (`--dry-run=server` catches that).
  On 1.28, where the feature is alpha and off by default, the field is
  silently dropped: the sidecar becomes a plain init container that never
  exits, and the pod hangs in `Init` forever. Dry-run does not catch that
  case.
- The sidecar container keeps `readOnlyRootFilesystem: false` — Firefox
  writes cache/profile data to several paths under `$HOME` that weren't
  practical to enumerate without running the real browser (not possible in
  the environment this was built in). See the comment in
  `deploy/k8s/10-apw.yaml` for the upgrade path.
- If the sidecar container fails to start with a sandbox-related error from
  Firefox (rather than the app-level errors this doc otherwise covers),
  the restricted Pod Security profile's seccomp/capability set is likely
  conflicting with Firefox's own internal content-process sandbox. The
  container's own isolation is already a sandbox boundary, so a reasonable
  fallback is disabling Firefox's redundant internal one by adding
  `MOZ_DISABLE_CONTENT_SANDBOX=1` to the sidecar container's `env`. It was
  not needed on kind (Kubernetes 1.37, containerd, RuntimeDefault seccomp);
  treat it as an escape hatch for this specific failure, not a default.

## Operational notes

- One Camoufox page per region locale is kept warm and reused; requests to
  the same region are serialized and throttled (same 2s minimum spacing the
  desktop app's Chromium fetcher uses). Monitoring many stores across many
  regions will take longer per cycle than the desktop default, by design —
  that serialization is what keeps Apple's edge from seeing a burst.
- `ponytail:` marks in `camoufox-sidecar/server.py` flag the deliberate
  corners cut for v1 (single global browser process, no in-process crash
  recovery beyond dropping the dead page) — see that file for the upgrade
  path if you need it.
