"""Exercise private-data startup, native restore states, and mount boundaries."""

from __future__ import annotations

import json
import subprocess
import sys
import time
import urllib.error
import urllib.request
import uuid
from pathlib import Path

NATIVE_RESTORE = """
import io
import importlib.metadata
from pathlib import Path
import shutil
import sys
import tarfile
import tempfile

from securetar import SecureTarArchive, SecureTarFile, atomic_contents_add

assert sys.version_info[:3] == (3, 14, 10)
assert importlib.metadata.version("securetar") == "2026.10.0"
password = "fictional-container-restore-only"
encrypted = io.BytesIO()
with SecureTarArchive(fileobj=encrypted, mode="w", password=password,
                      create_version=3) as archive:
    with archive.create_tar("./app.tar.gz") as private:
        atomic_contents_add(private, Path("/source"),
                            file_filter=lambda _: False, arcname="data")
encrypted.seek(0)
with tempfile.TemporaryDirectory() as extracted, \\
     tarfile.open(fileobj=encrypted, mode="r:") as outer:
    member = next(item for item in outer if item.name.endswith("app.tar.gz"))
    with SecureTarFile(fileobj=outer.extractfile(member), password=password,
                       gzip=True) as private:
        private.extractall(path=extracted, filter="tar")
    shutil.copytree(Path(extracted, "data"), "/restored", symlinks=True,
                    dirs_exist_ok=True)
for name in ("credentials.json", "property.sqlite3", "property.sync.sqlite3"):
    item = Path("/restored", name).stat()
    assert item.st_uid == 0 and item.st_gid == 0
    assert item.st_mode & 0o777 == 0o644
    assert item.st_mtime > 946684800
"""


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
    architecture = docker("image", "inspect", image, "--format", "{{.Architecture}}")
    if len(sys.argv) > 2:
        assert architecture == sys.argv[2]
    supervisor_images = {
        "amd64": (
            "ghcr.io/home-assistant/amd64-hassio-supervisor@"
            "sha256:f5f3728cf7a9a637485576c7588d209a1527fe9d6bb46f7326ef9aa629c55713"
        ),
        "arm64": (
            "ghcr.io/home-assistant/aarch64-hassio-supervisor@"
            "sha256:0f606324063cd5e7e591fb71efb5214ff56a05514898e33e461757950abc2914"
        ),
    }
    supervisor_image = supervisor_images[architecture]
    volumes = [
        docker("volume", "create", f"property-smoke-{uuid.uuid4().hex}")
        for _ in range(4)
    ]
    live, permissive, restrictive, outside = volumes
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

        def contents(volume: str) -> str:
            # Hashes stay in memory, including private credentials. Native
            # restore may lose private modes; only bytes and links are immutable.
            return volume_shell(
                volume,
                "find -P /data -xdev -type f -exec sha256sum -- {} + | sort; "
                "find -P /data -xdev -type l -printf '%p %l\\n' | sort",
            )

        def mtimes(volume: str) -> str:
            return volume_shell(
                volume,
                "find -P /data -xdev -exec stat -c '%Y %n' -- {} + | sort",
            )

        def private_modes(volume: str) -> None:
            # Check the credential first so a baseline failure identifies the
            # observed root-owned 0644 restore, rather than a directory mismatch.
            credential_mode = volume_shell(
                volume, "stat -c '%a:%u:%g' /data/credentials.json"
            )
            if credential_mode != "600:10001:10001":
                raise RuntimeError(
                    "restored credentials were not normalized to private mode 0600"
                )
            assert not volume_shell(
                volume,
                "find -P /data -xdev -type f ! -perm 0600 -print -quit; "
                "find -P /data -xdev -type d ! -perm 0700 -print -quit; "
                "find -P /data -xdev \\( ! -uid 10001 -o ! -gid 10001 \\) -print -quit",
            )

        def start(volume: str) -> tuple[str, str]:
            container = docker(
                "run",
                "-d",
                "--cap-drop",
                "FOWNER",
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

        volume_shell(live, "umask 077; printf '%s' '{}' > /data/options.json")
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
            "chmod 640 /outside/sentinel; chmod 750 /outside /outside/private; "
            "printf 'preserved-private-state\\n' > /data/owned-nested/marker; "
            "chown -R 10001:10001 /data/owned-nested; "
            "ln -s /outside /data/external-directory; "
            "ln -s /outside/sentinel /data/external-file; "
            "find -P /data -xdev -type f -exec touch -d @946684800 -- {} +",
        )
        original = contents(live)
        # Exercise the same SecureTar extraction/copy path as Supervisor. Dropped
        # FOWNER reproduces loss of chmod/mtime after app-owned extraction.
        # The restrictive root-created copy covers the separate UID0-header case.
        for copied, mode in ((permissive, "644"), (restrictive, "600")):
            if mode == "644":
                subprocess.run(
                    [
                        "docker",
                        "run",
                        "--rm",
                        "--network",
                        "none",
                        "--read-only",
                        "--cap-drop",
                        "FOWNER",
                        "--tmpfs",
                        "/tmp:rw,nosuid,nodev,noexec",
                        "-v",
                        f"{live}:/source:ro",
                        "-v",
                        f"{copied}:/restored",
                        "--entrypoint",
                        "python3",
                        "--interactive",
                        supervisor_image,
                    ],
                    input=NATIVE_RESTORE,
                    text=True,
                    check=True,
                )
            else:
                docker(
                    "run",
                    "--rm",
                    "--user",
                    "0:0",
                    "--cap-drop",
                    "FOWNER",
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
                f"test \"$(stat -c '%a:%u:%g' /data/credentials.json)\" = {mode}:0:0; "
                f"test \"$(stat -c '%a:%u:%g' /data/property.sqlite3)\" = {mode}:0:0; "
                "test \"$(stat -c '%a:%u:%g' /data/property.sync.sqlite3)\" "
                f"= {mode}:0:0; "
                "test \"$(stat -c '%a:%u:%g' /data/owned-nested/marker)\" "
                f"= {mode}:0:0; "
                "test -L /data/external-directory; test -L /data/external-file; "
                + (
                    "test \"$(stat -c '%Y' /data/credentials.json)\" -gt 946684800"
                    if mode == "644"
                    else "test \"$(stat -c '%Y' /data/credentials.json)\" = 946684800"
                ),
            )
            assert original == contents(copied)
            restored_mtimes = mtimes(copied)
            print(
                f"Restore precondition verified: root-owned {mode} files, "
                "FOWNER dropped."
            )
            # Exercise the production entrypoint before SQLite can write. Mode
            # normalization must preserve every byte, symlink and extracted mtime.
            docker(
                "run",
                "--rm",
                "--cap-drop",
                "FOWNER",
                "-v",
                f"{copied}:/data",
                "-v",
                f"{outside}:/outside",
                image,
                "--version",
            )
            assert original == contents(copied)
            assert restored_mtimes == mtimes(copied)
            print(
                "Restored credential mode before service startup: "
                + volume_shell(copied, "stat -c '%a:%u:%g' /data/credentials.json")
            )
            restored, destination = start(copied)
            if credential_bytes != subprocess.check_output(
                ["docker", "exec", restored, "cat", "/data/credentials.json"]
            ):
                raise RuntimeError("restored app credentials changed")
            private_modes(copied)
            assert before == request(destination, "/v1/summary", token=token)
            assert (
                request(
                    destination, "/v1/imports", apply, preview["revision"], token=token
                )["replayed"]
                is True
            )
            docker("stop", "--time", "120", restored)
            # Ordinary restart begins with app-owned private inodes. Permission
            # repair must work without FOWNER here as well as on a root restore.
            restarted, destination = start(copied)
            private_modes(copied)
            assert credential_bytes == subprocess.check_output(
                ["docker", "exec", restarted, "cat", "/data/credentials.json"]
            )
            assert before == request(destination, "/v1/summary", token=token)
            assert (
                request(
                    destination, "/v1/imports", apply, preview["revision"], token=token
                )["replayed"]
                is True
            )
            docker("stop", "--time", "120", restarted)
        volume_shell(
            copied,
            "test \"$(stat -c '%a:%u:%g' /outside/sentinel)\" = 640:0:0; "
            "test \"$(stat -c '%a:%u:%g' /outside/private)\" = 750:0:0; "
            "test \"$(stat -c '%a:%u:%g' /outside)\" = 750:0:0; "
            'test "$(cat /outside/sentinel)" = outside-private-data; '
            "test \"$(stat -c '%u:%g' /data/external-file)\" = 10001:10001; "
            "test \"$(stat -c '%u:%g' /data/external-directory)\" = 10001:10001",
        )
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
            "test \"$(stat -c '%a:%u:%g' /outside/sentinel)\" = 640:0:0; "
            "test \"$(stat -c '%a:%u:%g' /outside/private)\" = 750:0:0; "
            "test \"$(stat -c '%a:%u:%g' /outside)\" = 750:0:0; "
            'test "$(cat /outside/sentinel)" = outside-private-data',
        )

        # A hardlink can alias an outside inode without traversing a link or
        # another mount. Create the alias within one backing volume, then expose
        # only its private subdirectory as /data to the real entrypoint.
        volume_shell(copied, "ln /outside/sentinel /outside/private/outside-alias")
        outside_link = docker(
            "create",
            "--cap-drop",
            "FOWNER",
            "--mount",
            f"type=volume,source={outside},target=/data,volume-subpath=private",
            image,
            "--version",
        )
        containers.append(outside_link)
        expect_refusal(outside_link)
        assert "outside hard links" in container_logs(outside_link)
        volume_shell(
            copied,
            "test \"$(stat -c '%a:%u:%g:%h' /outside/sentinel)\" = 640:0:0:2; "
            "test \"$(stat -c '%a:%u:%g' /outside/private)\" = 750:0:0; "
            'test "$(cat /outside/sentinel)" = outside-private-data; '
            "rm /outside/private/outside-alias; "
            "mkfifo -m 640 /outside/private/unsupported-fifo",
        )
        unsupported = docker(
            "create",
            "--cap-drop",
            "FOWNER",
            "--mount",
            f"type=volume,source={outside},target=/data,volume-subpath=private",
            image,
            "--version",
        )
        containers.append(unsupported)
        expect_refusal(unsupported)
        assert "unsupported entries" in container_logs(unsupported)
        volume_shell(
            copied,
            "test \"$(stat -c '%a:%u:%g' /outside/sentinel)\" = 640:0:0; "
            "test \"$(stat -c '%a:%u:%g' /outside/private)\" = 750:0:0; "
            "test \"$(stat -c '%a:%u:%g' /outside/private/unsupported-fifo)\" "
            "= 640:0:0; "
            'test "$(cat /outside/sentinel)" = outside-private-data; '
            "rm /outside/private/unsupported-fifo; "
            "umask 077; printf 'internal-publication-state\\n' "
            "> /outside/private/atomic-one; "
            "ln /outside/private/atomic-one /outside/private/atomic-two",
        )
        # Atomic publication can retain both private names after a crash. Accept
        # and preserve this state when every alias is within the private tree.
        docker(
            "run",
            "--rm",
            "--cap-drop",
            "FOWNER",
            "--mount",
            f"type=volume,source={outside},target=/data,volume-subpath=private",
            image,
            "--version",
        )
        volume_shell(
            copied,
            "test \"$(stat -c '%a:%u:%g:%h' /outside/private/atomic-one)\" "
            "= 600:10001:10001:2; "
            "test \"$(stat -c '%a:%u:%g:%h' /outside/private/atomic-two)\" "
            "= 600:10001:10001:2; "
            "test \"$(stat -c '%a:%u:%g' /outside/private)\" = 700:10001:10001; "
            "test \"$(stat -c '%i' /outside/private/atomic-one)\" "
            "= \"$(stat -c '%i' /outside/private/atomic-two)\"; "
            'test "$(cat /outside/private/atomic-one)" = internal-publication-state; '
            "test \"$(stat -c '%a:%u:%g' /outside/sentinel)\" = 640:0:0; "
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
        "Container smoke passed: uid10001, root-owned 0644/0600 restores, "
        "files 0600/directories 0700, unchanged bytes/mtimes/credentials, "
        "ledger/replay durability, FOWNER-free restarts, "
        "and symlink/mount/hardlink boundaries."
    )


if __name__ == "__main__":
    main()
