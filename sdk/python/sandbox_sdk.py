"""Minimal Python SDK for sandboxd (spec §20).

The default path is one call::

    from sandbox_sdk import Client
    client = Client("https://api.sandboxes.example.com", api_key="sk_live_...")
    sandbox = client.sandboxes.create()          # cheap default path
    print(sandbox.exec("echo ok").stdout)
    sandbox.delete()

Heavier sandboxes require explicit options (spec §3.4)::

    sandbox = client.sandboxes.create(
        image="browser",
        resources={"cpu": 2, "memory_mb": 4096, "disk_gb": 16},
        browser={"enabled": True, "vnc": True, "cdp": True},
        startup={
            "git": {"url": "https://github.com/acme/app.git", "ref": "main", "depth": 1},
            "commands": [{"name": "install", "run": "pnpm install --frozen-lockfile"}],
            "ports": [3000, 6080],
            "ready": {"http": "http://127.0.0.1:3000", "timeout_seconds": 30},
        },
    )
    print(sandbox.urls["vnc"])

Uses only the standard library (urllib), so it has zero dependencies.
"""

from __future__ import annotations

import json
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass
from typing import Any, Optional


class SandboxError(Exception):
    def __init__(self, status: int, code: str, message: str):
        super().__init__(f"[{status} {code}] {message}")
        self.status = status
        self.code = code
        self.message = message


@dataclass
class ExecResult:
    exit_code: int
    stdout: str
    stderr: str


class _Http:
    def __init__(self, base_url: str, api_key: str, timeout: float = 60.0):
        self.base = base_url.rstrip("/")
        self.key = api_key
        self.timeout = timeout

    def request(self, method: str, path: str, body: Optional[dict] = None) -> Any:
        url = f"{self.base}{path}"
        data = json.dumps(body).encode() if body is not None else None
        req = urllib.request.Request(url, data=data, method=method)
        req.add_header("Authorization", f"Bearer {self.key}")
        req.add_header("Content-Type", "application/json")
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
                raw = resp.read()
                return json.loads(raw) if raw else {}
        except urllib.error.HTTPError as e:
            raw = e.read()
            try:
                err = json.loads(raw)["error"]
                raise SandboxError(e.code, err.get("code", "error"), err.get("message", "")) from None
            except (ValueError, KeyError):
                raise SandboxError(e.code, "error", raw.decode(errors="replace")) from None


class Sandbox:
    def __init__(self, http: _Http, data: dict):
        self._http = http
        self._data = data

    @property
    def id(self) -> str:
        return self._data["id"]

    @property
    def state(self) -> str:
        return self._data["state"]

    @property
    def boot_path(self) -> str:
        return self._data["boot_path"]

    @property
    def timings(self) -> dict:
        return self._data.get("timings", {})

    @property
    def urls(self) -> dict:
        return self._data.get("urls", {})

    @property
    def price(self) -> dict:
        return self._data.get("price", {})

    def refresh(self) -> "Sandbox":
        self._data = self._http.request("GET", f"/v1/sandboxes/{self.id}")
        return self

    def exec(self, cmd: str, cwd: Optional[str] = None, env: Optional[dict] = None,
             background: bool = False) -> ExecResult:
        body = {"cmd": cmd, "background": background}
        if cwd:
            body["cwd"] = cwd
        if env:
            body["env"] = env
        r = self._http.request("POST", f"/v1/sandboxes/{self.id}/exec", body)
        return ExecResult(r["exit_code"], r["stdout"], r["stderr"])

    def write_file(self, path: str, content: str) -> None:
        self._http.request("PUT", f"/v1/sandboxes/{self.id}/files",
                           {"path": path, "content": content})

    def read_file(self, path: str) -> str:
        q = urllib.parse.urlencode({"path": path})
        r = self._http.request("GET", f"/v1/sandboxes/{self.id}/files?{q}")
        return r["content"]

    def expose_port(self, port: int) -> str:
        r = self._http.request("POST", f"/v1/sandboxes/{self.id}/ports/{port}/expose")
        return r["url"]

    def browser(self) -> dict:
        return self._http.request("GET", f"/v1/sandboxes/{self.id}/browser")

    def snapshot(self) -> dict:
        return self._http.request("POST", f"/v1/sandboxes/{self.id}/snapshot")

    def pause(self) -> "Sandbox":
        self._data = self._http.request("POST", f"/v1/sandboxes/{self.id}/pause")
        return self

    def resume(self) -> "Sandbox":
        self._data = self._http.request("POST", f"/v1/sandboxes/{self.id}/resume")
        return self

    def delete(self) -> None:
        self._http.request("DELETE", f"/v1/sandboxes/{self.id}")


class _Sandboxes:
    def __init__(self, http: _Http):
        self._http = http

    def create(self, **options) -> Sandbox:
        # `create()` with no args yields the cheapest, fastest default path.
        body = {k: v for k, v in options.items() if v is not None}
        data = self._http.request("POST", "/v1/sandboxes", body)
        return Sandbox(self._http, data)

    def get(self, sandbox_id: str) -> Sandbox:
        return Sandbox(self._http, self._http.request("GET", f"/v1/sandboxes/{sandbox_id}"))

    def list(self) -> list[Sandbox]:
        data = self._http.request("GET", "/v1/sandboxes")
        return [Sandbox(self._http, s) for s in data.get("sandboxes", [])]


class _Images:
    def __init__(self, http: _Http):
        self._http = http

    def create(self, name: str, source: dict, resources_hint: Optional[dict] = None) -> dict:
        body = {"name": name, "source": source}
        if resources_hint:
            body["resources_hint"] = resources_hint
        return self._http.request("POST", "/v1/images", body)

    def get(self, image_id: str) -> dict:
        return self._http.request("GET", f"/v1/images/{image_id}")

    def list(self) -> dict:
        return self._http.request("GET", "/v1/images")

    def delete(self, image_id: str) -> dict:
        return self._http.request("DELETE", f"/v1/images/{image_id}")


class _Nodes:
    def __init__(self, http: _Http):
        self._http = http

    def list(self) -> dict:
        return self._http.request("GET", "/v1/nodes")

    def join_token(self) -> dict:
        return self._http.request("POST", "/v1/nodes/join-token")

    def drain(self, node_id: str) -> dict:
        return self._http.request("POST", f"/v1/nodes/{node_id}/drain")


class _Secrets:
    """Org-scoped secrets. Values are encrypted at rest and never returned."""

    def __init__(self, http: _Http):
        self._http = http

    def set(self, name: str, value: str) -> dict:
        return self._http.request("PUT", f"/v1/secrets/{name}", {"value": value})

    def list(self) -> list[dict]:
        return self._http.request("GET", "/v1/secrets").get("secrets", [])

    def delete(self, name: str) -> dict:
        return self._http.request("DELETE", f"/v1/secrets/{name}")


class Client:
    def __init__(self, base_url: str, api_key: str, timeout: float = 60.0):
        self._http = _Http(base_url, api_key, timeout)
        self.sandboxes = _Sandboxes(self._http)
        self.images = _Images(self._http)
        self.nodes = _Nodes(self._http)
        self.secrets = _Secrets(self._http)

    def usage(self) -> dict:
        return self._http.request("GET", "/v1/usage")


if __name__ == "__main__":
    import os
    client = Client(os.environ.get("SANDBOXD_URL", "http://127.0.0.1:8080"),
                    os.environ["SANDBOXD_KEY"])
    sb = client.sandboxes.create()
    print("created", sb.id, "boot_path", sb.boot_path, "boot_ms", sb.timings.get("boot_ms"))
    print("echo:", sb.exec("echo ok").stdout.strip())
    sb.delete()
    print("deleted")
