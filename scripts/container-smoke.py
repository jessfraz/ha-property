"""Exercise the shipped image and verify its native cold recovery path."""

from __future__ import annotations

import json
import subprocess
import sys
import time
import urllib.error
import urllib.request
import uuid
from pathlib import Path


def docker(*args: str) -> str:
    return subprocess.check_output(["docker", *args], text=True).strip()


def request(
    origin: str,
    path: str,
    payload: object | None = None,
    revision: int | None = None,
    *,
    token: str | None = None,
) -> dict[str, object]:
    headers = {"Authorization": f"Bearer {token}"} if token is not None else {}
    if payload is not None:
        headers["Content-Type"] = "application/json"
    if revision is not None:
        headers["If-Match"] = f'"{revision}"'
    req = urllib.request.Request(
        origin + path,
        data=json.dumps(payload).encode() if payload is not None else None,
        headers=headers,
    )
    with urllib.request.urlopen(req, timeout=5) as response:
        return json.load(response)


def ready(container: str) -> str:
    host = docker("port", container, "8080/tcp").splitlines()[0]
    origin = f"http://{host}"
    for _ in range(100):
        try:
            if request(origin, "/health")["status"] == "ok":
                return origin
        except OSError, urllib.error.URLError:
            time.sleep(0.1)
    raise RuntimeError("container did not become ready")


def main() -> None:
    image = sys.argv[1]
    if len(sys.argv) > 2:
        assert (
            docker("image", "inspect", image, "--format", "{{.Architecture}}")
            == sys.argv[2]
        )
    volumes = [
        docker("volume", "create", f"property-smoke-{uuid.uuid4().hex}")
        for _ in range(2)
    ]
    containers: list[str] = []
    try:
        docker(
            "run",
            "--rm",
            "--user",
            "0:0",
            "--entrypoint",
            "/bin/sh",
            "-v",
            f"{volumes[0]}:/data",
            image,
            "-c",
            "printf '%s' '{}' > /data/options.json",
        )

        def start(volume: str) -> tuple[str, str]:
            container = docker(
                "run",
                "-d",
                "-p",
                "127.0.0.1::8080",
                "-v",
                f"{volume}:/data",
                image,
            )
            containers.append(container)
            return container, ready(container)

        container, origin = start(volumes[0])
        # Only disposable app-mode credentials cross this process boundary.
        # They stay in memory and are never printed or attached to CI artifacts.
        credentials = json.loads(
            docker("exec", container, "cat", "/data/credentials.json")
        )
        token = credentials["owner"]
        assert token != credentials["integration"]
        assert (
            docker("exec", container, "stat", "-c", "%a", "/data/credentials.json")
            == "600"
        )
        fixture = json.loads(Path("tests/fixtures/import.example.json").read_text())
        preview = request(origin, "/v1/imports/preview", fixture, token=token)
        apply = {
            "preview": preview,
            "if_match": preview["revision"],
            "idempotency_key": "example-container-import",
        }
        assert (
            request(origin, "/v1/imports", apply, preview["revision"], token=token)[
                "added"
            ]
            > 0
        )
        before = request(origin, "/v1/summary", token=token)
        assert before["needs_attention"] > 0
        docker("stop", "--time", "120", container)
        docker(
            "run",
            "--rm",
            "--user",
            "0:0",
            "--entrypoint",
            "/bin/sh",
            "-v",
            f"{volumes[0]}:/from:ro",
            "-v",
            f"{volumes[1]}:/to",
            image,
            "-c",
            "cp -a /from/. /to/",
        )
        restored, destination = start(volumes[1])
        assert credentials == json.loads(
            docker("exec", restored, "cat", "/data/credentials.json")
        )
        assert before == request(destination, "/v1/summary", token=token)
        assert (
            request(
                destination, "/v1/imports", apply, preview["revision"], token=token
            )["replayed"]
            is True
        )
    finally:
        for container in containers:
            subprocess.run(
                ["docker", "rm", "-f", container],
                check=False,
                stdout=subprocess.DEVNULL,
            )
        for volume in volumes:
            subprocess.run(
                ["docker", "volume", "rm", volume],
                check=False,
                stdout=subprocess.DEVNULL,
            )
    print("Container import, recovery and replay passed.")


if __name__ == "__main__":
    main()
