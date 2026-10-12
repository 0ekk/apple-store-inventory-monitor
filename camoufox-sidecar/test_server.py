"""Self-check for SIDECAR_PROXY parsing. Needs the sidecar's requirements
installed, e.g. inside its image:

    docker run --rm -v "$PWD/camoufox-sidecar/test_server.py:/app/test_server.py:ro" \
        --entrypoint python <sidecar-image> test_server.py
"""

import os

from server import proxy_from_env


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
for bad in ("proxy.example:8080", "ftp://u:hunter2@proxy.example:21", "http://u:hunter2@proxy.example"):
    try:
        parsed(bad)
    except ValueError as err:
        assert "hunter2" not in str(err), "error message leaks the proxy password"
    else:
        raise AssertionError(f"accepted {bad!r}")
print("ok")
