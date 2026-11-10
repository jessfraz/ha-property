"""Exercise app startup, Supervisor-shaped restore, and ownership boundaries."""

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
    if docker("inspect", container, "--format", "{{.State.Running}}") != "true":
        raise RuntimeError("container exited before readiness")
    try:
        host = docker("port", container, "8080/tcp").splitlines()[0]
    except subprocess.CalledProcessError:
        if docker("inspect", container, "--format", "{{.State.Running}}") != "true":
            raise RuntimeError("container exited before readiness") from None
        raise RuntimeError("container port mapping is unavailable") from None
    origin = f"http://{host}"
    for _ in range(100):
        if docker("inspect", container, "--format", "{{.State.Running}}") != "true":
            raise RuntimeError("container exited before readiness")
        try:
            if request(origin, "/health")["status"] == "ok":
                return origin
        except (OSError, urllib.error.URLError):
            time.sleep(0.1)
    raise RuntimeError("container did not become ready")


def container_logs(container: str) -> str:
    return subprocess.check_output(
        ["docker", "logs", container], text=True, stderr=subprocess.STDOUT
    )


def expect_refusal(container: str) -> None:
    docker("start", container)
    for _ in range(40):
        if docker("inspect", container, "--format", "{{.State.Running}}") == "false":
            if docker("inspect", container, "--format", "{{.State.ExitCode}}") != "0":
                return
            break
        time.sleep(0.1)
    raise RuntimeError("app accepted a private-data ownership boundary violation")


def main() -> None:
    image = sys.argv[1]
    if len(sys.argv) > 2:
        assert (
            docker("image", "inspect", image, "--format", "{{.Architecture}}")
            == sys.argv[2]
        )
    volumes = [
        docker("volume", "create", f"property-smoke-{uuid.uuid4().hex}")
        for _ in range(3)
    ]
    live, copied, outside = volumes
    containers: list[str] = []
    try:

        def volume_shell(volume: str, script: str) -> str:
            return docker(
                "run",
                "--rm",
                "--user",
                "0:0",
                "--entrypoint",
                "/bin/sh",
                "-v",
                f"{volume}:/data",
                "-v",
                f"{outside}:/outside",
                image,
                "-eu",
                "-c",
                script,
            )

        def manifest(volume: str) -> str:
            # Hashes and modes stay in memory, including private credentials.
            # Ownership is deliberately separate: Supervisor restores recreate
            # files as root while retaining their contents and restrictive modes.
            return volume_shell(
                volume,
                "find -P /data -xdev -type f -exec sha256sum -- {} + | sort; "
                "find -P /data -xdev ! -type l -exec stat -c '%a %n' -- {} + | sort",
            )

        def start(volume: str) -> tuple[str, str]:
            container = docker(
                "run",
                "-d",
                "-p",
                "127.0.0.1::8080",
                "-v",
                f"{volume}:/data",
                "-v",
                f"{outside}:/outside",
                image,
            )
            containers.append(container)
            origin = ready(container)
            uid = docker(
                "exec", container, "awk", "/^Uid:/ {print $2}", "/proc/1/status"
            )
            if uid != "10001":
                raise RuntimeError("server did not drop to uid10001")
            return container, origin

        volume_shell(live, "printf '%s' '{}' > /data/options.json")
        container, origin = start(live)
        # Only disposable app-mode credentials cross this process boundary.
        # They stay in memory and are never printed or attached to CI artifacts.
        credential_bytes = subprocess.check_output(
            ["docker", "exec", container, "cat", "/data/credentials.json"]
        )
        credentials = json.loads(credential_bytes)
        token = credentials["owner"]
        assert token != credentials["integration"]
        assert (
            docker(
                "exec", container, "stat", "-c", "%a:%u:%g", "/data/credentials.json"
            )
            == "600:10001:10001"
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
        volume_shell(
            live,
            "umask 077; printf 'outside-private-data\\n' > /outside/sentinel; "
            "mkdir /outside/private /data/owned-nested; "
            "printf 'preserved-private-state\\n' > /data/owned-nested/marker; "
            "chown -R 10001:10001 /data/owned-nested; "
            "ln -s /outside /data/external-directory; "
            "ln -s /outside/sentinel /data/external-file",
        )
        original = manifest(live)
        # Equivalent ownership/mode semantics to Supervisor's root-run
        # shutil.copytree(..., symlinks=True), not cp -a's old-owner preservation.
        docker(
            "run",
            "--rm",
            "--user",
            "0:0",
            "--entrypoint",
            "/bin/sh",
            "-v",
            f"{live}:/from:ro",
            "-v",
            f"{copied}:/to",
            image,
            "-eu",
            "-c",
            "cp -pR --no-preserve=ownership --no-dereference /from/. /to/",
        )
        volume_shell(
            copied,
            "test \"$(stat -c '%a:%u:%g' /data/credentials.json)\" = 600:0:0; "
            "test \"$(stat -c '%a:%u:%g' /data/property.sqlite3)\" = 600:0:0; "
            "test \"$(stat -c '%a:%u:%g' /data/property.sync.sqlite3)\" = 600:0:0; "
            "test \"$(stat -c '%a:%u:%g' /data/owned-nested/marker)\" = 600:0:0; "
            "test -L /data/external-directory; test -L /data/external-file",
        )
        unreadable = subprocess.run(
            [
                "docker",
                "run",
                "--rm",
                "--entrypoint",
                "gosu",
                "-v",
                f"{copied}:/data",
                image,
                "property",
                "test",
                "-r",
                "/data/credentials.json",
            ],
            capture_output=True,
            check=False,
        )
        if unreadable.returncode != 1:
            raise RuntimeError("restore did not recreate unreadable private files")
        assert original == manifest(copied)
        # Exercise the real ownership repair without opening SQLite, so exact
        # file bytes/modes can be compared before ordinary runtime writes.
        docker(
            "run",
            "--rm",
            "-v",
            f"{copied}:/data",
            "-v",
            f"{outside}:/outside",
            image,
            "--version",
        )
        assert original == manifest(copied)
        restored, destination = start(copied)
        if credential_bytes != subprocess.check_output(
            ["docker", "exec", restored, "cat", "/data/credentials.json"]
        ):
            raise RuntimeError("restored app credentials changed")
        for path in (
            "/data/credentials.json",
            "/data/property.sqlite3",
            "/data/property.sync.sqlite3",
            "/data/owned-nested/marker",
        ):
            assert (
                docker("exec", restored, "stat", "-c", "%a:%u:%g", path)
                == "600:10001:10001"
            )
        volume_shell(
            copied,
            "test \"$(stat -c '%a:%u:%g' /outside/sentinel)\" = 600:0:0; "
            "test \"$(stat -c '%a:%u:%g' /outside/private)\" = 700:0:0; "
            "test \"$(stat -c '%u:%g' /outside)\" = 0:0; "
            'test "$(cat /outside/sentinel)" = outside-private-data; '
            "test \"$(stat -c '%u:%g' /data/external-file)\" = 10001:10001; "
            "test \"$(stat -c '%u:%g' /data/external-directory)\" = 10001:10001",
        )
        assert before == request(destination, "/v1/summary", token=token)
        assert (
            request(
                destination, "/v1/imports", apply, preview["revision"], token=token
            )["replayed"]
            is True
        )
        docker("stop", "--time", "120", restored)

        # Named volumes share a filesystem: -xdev alone would still traverse a
        # nested bind mount. Reject it before any ownership change can occur.
        boundary = docker(
            "create",
            "-v",
            f"{copied}:/data",
            "-v",
            f"{outside}:/data/nested-mount",
            "--entrypoint",
            "/bin/sh",
            image,
            "-eu",
            "-c",
            "test \"$(stat -c '%d' /data)\" = \"$(stat -c '%d' /data/nested-mount)\"; "
            "exec /usr/local/bin/docker-entrypoint.sh",
        )
        containers.append(boundary)
        expect_refusal(boundary)
        assert "contains nested mounts" in container_logs(boundary)
        tmpfs = docker(
            "create",
            "-v",
            f"{copied}:/data",
            "--tmpfs",
            "/data/nested-tmpfs:rw,noexec,nosuid,size=65536",
            image,
        )
        containers.append(tmpfs)
        expect_refusal(tmpfs)
        assert "contains nested mounts" in container_logs(tmpfs)
        root_link = docker(
            "create",
            "-v",
            f"{outside}:/outside",
            "--entrypoint",
            "/bin/sh",
            image,
            "-eu",
            "-c",
            "rmdir /data; ln -s /outside /data; "
            "exec /usr/local/bin/docker-entrypoint.sh",
        )
        containers.append(root_link)
        expect_refusal(root_link)
        assert "real directory" in container_logs(root_link)
        volume_shell(
            copied,
            "test \"$(stat -c '%a:%u:%g' /outside/sentinel)\" = 600:0:0; "
            "test \"$(stat -c '%a:%u:%g' /outside/private)\" = 700:0:0; "
            "test \"$(stat -c '%u:%g' /outside)\" = 0:0; "
            'test "$(cat /outside/sentinel)" = outside-private-data',
        )
    finally:
        cleanup_failed = False
        for container in containers:
            result = subprocess.run(
                ["docker", "rm", "-f", container],
                check=False,
                stdout=subprocess.DEVNULL,
            )
            cleanup_failed |= result.returncode != 0
        for volume in volumes:
            result = subprocess.run(
                ["docker", "volume", "rm", volume],
                check=False,
                stdout=subprocess.DEVNULL,
            )
            cleanup_failed |= result.returncode != 0
        if cleanup_failed:
            raise RuntimeError("task container or volume cleanup failed")
    print(
        "Container smoke passed: uid10001, root-owned restrictive restore, "
        "unchanged bytes/modes/credentials, ledger/replay durability, "
        "and symlink/mount boundaries."
    )


if __name__ == "__main__":
    main()
