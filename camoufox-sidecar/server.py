"""Camoufox sidecar for the Apple Store Inventory Monitor headless port.

Apple's store pages run a bot-check script before the pickup endpoint will
answer, and that check only passes inside a real browser. This
process is the only thing in the whole deployment that ever touches a
browser: it keeps one warmed-up Camoufox page per region alive and, on
request, runs a `fetch()` call inside that page's own session.

It is intentionally generic. It has no idea what a "store" or a "part
number" is -- the Rust side (crates/apw-headless) owns all of that and sends
this process a plain instruction: "make sure this URL has been visited and
has finished loading, then fetch that URL from inside the page". Keeping
Apple's URL shapes and query parameters out of this file means there is
exactly one place (apw-core) that needs to change when Apple's API does.

Protocol: a tiny hand-rolled HTTP/1.1 server (stdlib only, no aiohttp/fastapi)
since there is exactly one real endpoint and a full framework would be
weight with nothing to justify it:

  POST /pickup
    {"locale": "zh_CN",
     "warm": {"url": "https://www.apple.com/sg/"},
     "fetch": {"url": "https://www.apple.com/sg/shop/retail/pickup-message?...", "headers": {...}},
     "keep_session_on_block": false}
    -> {"status": 200, "body": "..."}  or  {"error": "..."}

  (Named for its first use; it fetches any same-origin URL, delivery
  quotes included.)

  GET /healthz -> {"ok": true}
"""

import asyncio
import json
import os
import signal
import time
from urllib.parse import unquote, urlsplit

from camoufox import DefaultAddons
from camoufox.async_api import AsyncCamoufox

WARM_TIMEOUT_SECONDS = 50.0
MIN_REQUEST_INTERVAL_SECONDS = 2.0
MAX_RESPONSE_BYTES = 4 * 1024 * 1024

FETCH_JS = """
async (spec) => {
    try {
        const response = await fetch(spec.url, {
            credentials: 'same-origin',
            headers: spec.headers,
        });
        const body = await response.text();
        return {status: response.status, body};
    } catch (err) {
        return {status: 0, body: String(err)};
    }
}
"""


def proxy_from_env() -> dict | None:
    """SIDECAR_PROXY=scheme://[user:pass@]host:port -> Playwright proxy settings.

    Apple blocks the pickup endpoint per exit IP (HTTP 541 even for a real
    desktop browser once an IP is flagged), so the browser's exit IP has to
    be switchable without rebuilding anything.
    """
    raw = os.environ.get("SIDECAR_PROXY", "").strip()
    if not raw:
        return None
    url = urlsplit(raw)
    if url.scheme not in ("http", "https", "socks5") or not url.hostname or url.port is None:
        # Never echo `raw`: it may carry the proxy password.
        raise ValueError("SIDECAR_PROXY must look like http://[user:pass@]host:port (or https://, socks5://)")
    host_port = url.netloc.rpartition("@")[2]  # keeps IPv6 brackets intact
    proxy = {"server": f"{url.scheme}://{host_port}"}
    if url.username:
        proxy["username"] = unquote(url.username)
        proxy["password"] = unquote(url.password or "")
    return proxy


class RegionSession:
    """One warmed-up page per region locale, reused across requests.

    The lock does double duty: it serializes requests to the same region
    (Apple treats a burst as automation) and it protects `page` from being
    read mid-replacement while a previous request is still tearing it down.
    """

    def __init__(self):
        self.lock = asyncio.Lock()
        self.page = None
        self.warm_url = None
        self.last_request = 0.0


class Sidecar:
    def __init__(self):
        self._camoufox = None
        self._browser = None
        self._sessions: dict[str, RegionSession] = {}
        # One Apple page load at a time across all regions. Loads are the
        # memory peak (a buy page pulls dozens of images); three regions
        # warming at once got the container OOM-killed at 1.5 GiB. The
        # desktop app goes further and keeps a single page for all regions.
        self._warm_lock = asyncio.Lock()

    async def start(self):
        # No uBlock Origin: Camoufox otherwise downloads it from
        # addons.mozilla.org on every launch (a runtime dependency that can
        # fail -- it returned HTTP 451 from some networks), and an ad blocker
        # could block Apple's own scripts. Upstream's desktop Chromium session
        # likewise runs with --disable-extensions.
        proxy = proxy_from_env()
        print(f"apple traffic via {proxy['server'] if proxy else 'direct connection'}", flush=True)
        # ponytail: one static proxy for the process lifetime; rotating on 541
        # (upstream's Clash node switching) needs a browser relaunch per switch.
        self._camoufox = AsyncCamoufox(
            headless=True, exclude_addons=[DefaultAddons.UBO], proxy=proxy
        )
        self._browser = await self._camoufox.__aenter__()

    async def stop(self):
        if self._camoufox is not None:
            await self._camoufox.__aexit__(None, None, None)

    def _session_for(self, locale: str) -> RegionSession:
        return self._sessions.setdefault(locale, RegionSession())

    async def _ensure_warm(self, session: RegionSession, warm: dict):
        url = warm["url"]

        if session.page is not None and session.warm_url == url:
            return

        if session.page is not None:
            await session.page.close()
            session.page = None

        async with self._warm_lock:
            page = await self._browser.new_page()
            try:
                # "load" fires when document.readyState becomes "complete" --
                # the same readiness signal upstream's desktop app waits for.
                # No cookie-name check: the names that used to be checked
                # here only reliably appear on the mainland China site.
                await page.goto(url, wait_until="load", timeout=WARM_TIMEOUT_SECONDS * 1000)
            except Exception:
                await page.close()
                raise
        session.page = page
        session.warm_url = url

    async def pickup(self, payload: dict) -> dict:
        locale = payload["locale"]
        warm = payload["warm"]
        fetch_spec = payload["fetch"]

        session = self._session_for(locale)
        async with session.lock:
            await self._ensure_warm(session, warm)

            elapsed = time.monotonic() - session.last_request
            if elapsed < MIN_REQUEST_INTERVAL_SECONDS:
                await asyncio.sleep(MIN_REQUEST_INTERVAL_SECONDS - elapsed)
            session.last_request = time.monotonic()

            try:
                result = await session.page.evaluate(
                    FETCH_JS,
                    {"url": fetch_spec["url"], "headers": fetch_spec.get("headers", {})},
                )
            except Exception as exc:  # noqa: BLE001 - page/browser may have died mid-flight
                # ponytail: a single global browser process with no live
                # in-process recovery. A fully dead browser (not just a
                # rejected page) needs the container to restart; wire that
                # up via the container runtime's restart policy, not here.
                session.page = None
                return {"error": f"page evaluation failed: {exc}"}

            body = result.get("body", "")
            if len(body.encode("utf-8", "replace")) > MAX_RESPONSE_BYTES:
                return {"error": "Apple response exceeded the 4 MiB safety cap"}

            status = result.get("status", 0)
            if status in (403, 541) and not payload.get("keep_session_on_block", False):
                # Session got rejected; rebuild from scratch next time
                # instead of retrying against a session Apple already
                # blacklisted. Callers opt out for endpoints that can refuse
                # on their own while the session is fine (delivery quotes).
                await session.page.close()
                session.page = None

            return {"status": status, "body": body}


sidecar = Sidecar()


async def _read_headers(reader: asyncio.StreamReader) -> tuple[str, str, dict[str, str]]:
    request_line = await reader.readline()
    if not request_line:
        raise ConnectionError("empty request")
    method, path, _ = request_line.decode("latin-1").strip().split(" ", 2)

    headers: dict[str, str] = {}
    while True:
        line = await reader.readline()
        if line in (b"\r\n", b"", b"\n"):
            break
        key, _, value = line.decode("latin-1").partition(":")
        headers[key.strip().lower()] = value.strip()
    return method, path, headers


async def _write_json(writer: asyncio.StreamWriter, status: str, payload: dict):
    body = json.dumps(payload).encode("utf-8")
    writer.write(f"HTTP/1.1 {status}\r\n".encode("latin-1"))
    writer.write(
        f"content-type: application/json\r\ncontent-length: {len(body)}\r\nconnection: close\r\n\r\n".encode(
            "latin-1"
        )
    )
    writer.write(body)
    await writer.drain()


async def handle_client(reader: asyncio.StreamReader, writer: asyncio.StreamWriter):
    try:
        method, path, headers = await _read_headers(reader)
        content_length = int(headers.get("content-length", "0"))
        body = await reader.readexactly(content_length) if content_length else b""

        if method == "GET" and path == "/healthz":
            await _write_json(writer, "200 OK", {"ok": True})
            return

        if method == "POST" and path == "/pickup":
            try:
                payload = json.loads(body)
                result = await sidecar.pickup(payload)
            except Exception as exc:  # noqa: BLE001 - reported back to the caller, not raised
                result = {"error": str(exc)}
            await _write_json(writer, "200 OK", result)
            return

        await _write_json(writer, "404 Not Found", {"error": "not found"})
    except Exception as exc:  # noqa: BLE001 - keep the server alive for the next connection
        try:
            await _write_json(writer, "500 Internal Server Error", {"error": str(exc)})
        except Exception:
            pass
    finally:
        writer.close()


async def main():
    host = os.environ.get("SIDECAR_HOST", "127.0.0.1")
    port = int(os.environ.get("SIDECAR_PORT", "8787"))

    await sidecar.start()
    server = await asyncio.start_server(handle_client, host, port)

    loop = asyncio.get_running_loop()
    stop = loop.create_future()
    for sig in (signal.SIGTERM, signal.SIGINT):
        loop.add_signal_handler(sig, stop.set_result, None)

    addr = server.sockets[0].getsockname()
    print(f"camoufox sidecar listening on {addr}", flush=True)

    async with server:
        await stop

    await sidecar.stop()


if __name__ == "__main__":
    asyncio.run(main())
