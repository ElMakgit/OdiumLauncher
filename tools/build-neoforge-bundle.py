#!/usr/bin/env python3
"""Builds the offline fallback bundle for a NeoForge installer.

`maven.neoforged.net` is unreachable from some networks (notably in Russia),
while `libraries.minecraft.net` keeps working. The launcher then cannot fetch
the installer or its libraries and refuses to run an unverified jar.

The bundle contains everything the installer would have downloaded from
NeoForge's Maven host: the installer jar itself plus the artifacts referenced by
`install_profile.json`. Mojang-hosted libraries are deliberately *not* bundled;
the installer keeps downloading those from Mojang, which also keeps Minecraft's
redistribution terms out of the picture.

Usage:
    python tools/build-neoforge-bundle.py --minecraft 1.21.1 --loader 21.1.248
    python tools/build-neoforge-bundle.py --minecraft 1.20.1 --loader 47.1.79

The launcher pins the resulting SHA-256 in `NEOFORGE_OFFLINE_BUNDLES`, so a new
bundle must be uploaded to the `neoforge-mirror` GitHub release and the hash
copied into the code.
"""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import sys
import urllib.request
import zipfile
from pathlib import Path

MAVEN = "https://maven.neoforged.net/releases"

# 1.20.1 still uses Forge coordinates inside NeoForge's repository.
LEGACY_MC = "1.20.1"


def installer_url(minecraft: str, loader: str) -> str:
    if minecraft == LEGACY_MC:
        version = loader if loader.startswith("1.20.1-") else f"1.20.1-{loader}"
        return f"{MAVEN}/net/neoforged/forge/{version}/forge-{version}-installer.jar"
    return f"{MAVEN}/net/neoforged/neoforge/{loader}/neoforge-{loader}-installer.jar"


def bare_loader(loader: str) -> str:
    return loader.removeprefix("1.20.1-")


def fetch(url: str) -> bytes:
    request = urllib.request.Request(url, headers={"User-Agent": "OdiumLauncher-bundle-builder"})
    with urllib.request.urlopen(request, timeout=120) as response:
        return response.read()


def sha1(data: bytes) -> str:
    return hashlib.sha1(data).hexdigest()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--minecraft", required=True)
    parser.add_argument("--loader", required=True)
    parser.add_argument("--output", type=Path, default=None)
    args = parser.parse_args()

    url = installer_url(args.minecraft, args.loader)
    output = args.output or Path(
        f"neoforge-{args.minecraft}-{bare_loader(args.loader)}-offline.zip"
    )

    print(f"installer: {url}")
    installer = fetch(url)
    expected = fetch(f"{url}.sha1").decode().split()[0].strip().lower()
    actual = sha1(installer)
    if actual != expected:
        print(f"installer sha1 mismatch: {actual} != {expected}", file=sys.stderr)
        return 1
    print(f"installer sha1 ok: {actual} ({len(installer)} bytes)")

    with zipfile.ZipFile(io.BytesIO(installer)) as jar:
        profile = json.loads(jar.read("install_profile.json"))

    files: list[dict[str, object]] = []
    libraries: dict[str, bytes] = {}
    for library in profile["libraries"]:
        artifact = library["downloads"]["artifact"]
        library_url = artifact["url"]
        if "maven.neoforged.net/releases/" not in library_url:
            continue
        relative = library_url.split("maven.neoforged.net/releases/", 1)[1]
        data = fetch(library_url)
        digest = sha1(data)
        if digest != artifact["sha1"].lower():
            print(f"library sha1 mismatch: {library_url}", file=sys.stderr)
            return 1
        if len(data) != int(artifact["size"]):
            print(f"library size mismatch: {library_url}", file=sys.stderr)
            return 1
        libraries[relative] = data
        files.append({"path": f"libraries/{relative}", "sha1": digest, "size": len(data)})

    print(f"libraries: {len(libraries)} files, {sum(map(len, libraries.values())) / 1048576:.1f} MiB")

    manifest = {
        "format": 1,
        "minecraft": args.minecraft,
        "loader": bare_loader(args.loader),
        "installer": {
            "path": "installer.jar",
            "sha1": actual,
            "size": len(installer),
        },
        "files": sorted(files, key=lambda item: str(item["path"])),
    }

    if output.exists():
        output.unlink()
    with zipfile.ZipFile(output, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        archive.writestr("manifest.json", json.dumps(manifest, indent=2, sort_keys=True))
        archive.writestr("installer.jar", installer)
        for relative, data in sorted(libraries.items()):
            archive.writestr(f"libraries/{relative}", data)

    data = output.read_bytes()
    print(f"wrote {output} ({len(data)} bytes)")
    print(f"sha256: {hashlib.sha256(data).hexdigest()}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
