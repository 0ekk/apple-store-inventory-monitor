"""Self-check for proxy parsing and idle-page eviction. Needs the sidecar's
requirements installed, e.g. inside its image:

    docker run --rm -v "$PWD/camoufox-sidecar/test_server.py:/app/test_server.py:ro" \
        --entrypoint python <sidecar-image> test_server.py
"""

import asyncio
import os

import server
from server import RegionSession, Sidecar, parse_proxy, proxy_from_env


def parsed(value):
    os.environ["SIDECAR_PROXY"] = value
    return proxy_from_env()


assert parsed("") is None
assert parsed("http://172.18.0.1:7890") == {"server": "http://172.18.0.1:7890"}
assert parsed("socks5://[fd00::1]:1080") == {"server": "socks5://[fd00::1]:1080"}
assert parsed("http://u%40corp:p%3Aw@proxy.example:8080") == {
    "server": "http://proxy.example:8080",
    "username": "u@corp",
    "password": "p:w",
}
for bad in (
    "proxy.example:8080",
    "ftp://u:hunter2@proxy.example:21",
    "http://u:hunter2@proxy.example",
    "socks5://u:hunter2@proxy.example:port",
    "socks5://u:hunter2@proxy.example:1080",
):
    try:
        parse_proxy(bad)
    except ValueError as err:
        assert "hunter2" not in str(err), "error message leaks the proxy password"
    else:
        raise AssertionError(f"accepted {bad!r}")


class FakePage:
    def __init__(self, name):
        self.name = name
        self.closed = False

    async def close(self):
        self.closed = True


async def eviction():
    server.MAX_PAGES = 2
    sidecar = Sidecar()
    sessions = {}
    for age, name in enumerate(("oldest", "busy", "middle", "newest")):
        session = RegionSession()
        session.page = FakePage(name)
        session.last_request = float(age)
        sessions[name] = session
        sidecar._sessions[(name, "zh_CN")] = session
    await sessions["busy"].lock.acquire()  # mid-request: never evicted
    await sidecar._evict_idle()
    closed = sorted(name for name, s in sessions.items() if s.page is None)
    assert closed == ["middle", "oldest"], closed
    assert sessions["busy"].page is not None and sessions["newest"].page is not None


asyncio.run(eviction())
print("ok")
