#!/usr/bin/env python3
"""Generate THIRD-PARTY-NOTICES.md from the Cargo dependency graph.

Run from the repository root:

    python tools/generate-third-party-notices.py

The script lists every third-party crate in the workspace dependency graph and
embeds the license texts shipped with the crate. Crates that do not package a
license file get their texts fetched from the upstream repository; those texts
are marked with their source. The result is deterministic and can be committed.
"""

from __future__ import annotations

import hashlib
import json
import re
import subprocess
import sys
import urllib.error
import urllib.request
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUTPUT = ROOT / "THIRD-PARTY-NOTICES.md"

LICENSE_NAME_RE = re.compile(r"(?i)^(license|licence|copying|notice|unlicense|ofl|ufl)")
TEXT_SUFFIXES = {"", ".txt", ".md", ".rst"}
MAX_TEXT_BYTES = 2_000_000
FETCH_NAMES = (
    "LICENSE",
    "LICENSE.md",
    "LICENSE.txt",
    "LICENSE-MIT",
    "LICENSE-APACHE",
    "LICENSE-APACHE-2.0",
    "LICENSE-LGPL-2.1",
    "LICENSE-LGPL",
    "COPYING",
    "COPYING.md",
    "COPYING.txt",
    "LICENCE",
    "LICENSE-OFL",
    "LICENSE-UFL",
    "OFL.txt",
    "UFL.txt",
)


def cargo_metadata() -> dict:
    raw = subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--all-features"],
        cwd=ROOT,
    )
    return json.loads(raw.decode("utf-8"))


def normalize_repo(url: str | None) -> str | None:
    if not url:
        return None
    url = url.strip().rstrip("/")
    url = re.sub(r"/tree/.*$", "", url)
    url = re.sub(r"\.git$", "", url)
    url = url.replace("http://", "https://")
    match = re.match(r"https://github\.com/([^/]+)/([^/]+)", url)
    if not match:
        return None
    return f"https://github.com/{match.group(1)}/{match.group(2)}"


def read_text(path: Path) -> str | None:
    try:
        if path.stat().st_size > MAX_TEXT_BYTES:
            return None
        text = path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None
    text = text.replace("\r\n", "\n").replace("\r", "\n").strip()
    return text or None


def own_license_files(package: dict) -> list[Path]:
    crate_dir = Path(package["manifest_path"]).parent
    found: list[Path] = []
    license_file = package.get("license_file")
    if license_file:
        path = Path(license_file)
        if not path.is_absolute():
            path = crate_dir / path
        if path.is_file():
            found.append(path)
    # `license_file` often points at a short pointer file; always scan the
    # crate as well so sibling LICENSE-MIT / LICENSE-APACHE files are included.
    for path in crate_dir.rglob("*"):
        if not path.is_file() or "target" in path.parts:
            continue
        if not LICENSE_NAME_RE.match(path.name):
            continue
        if path.suffix.lower() not in TEXT_SUFFIXES:
            continue
        found.append(path)
    unique: list[Path] = []
    seen: set[Path] = set()
    for path in sorted(found):
        if path not in seen:
            seen.add(path)
            unique.append(path)
    return unique


def is_license_pointer(text: str) -> bool:
    """True for files that only name an SPDX expression instead of its text."""
    compact = " ".join(text.split())
    if len(compact) > 200:
        return False
    return bool(
        re.fullmatch(
            r"[A-Za-z0-9.+\-()/ ]+( OR [A-Za-z0-9.+\-()/ ]+| AND [A-Za-z0-9.+\-()/ ]+| WITH [A-Za-z0-9.+\-()/ ]+)*",
            compact,
        )
    )


def has_full_text(files: list[Path]) -> bool:
    for path in files:
        text = read_text(path)
        if text and not is_license_pointer(text) and len(text) > 400:
            return True
    return False


def fetch_repo_licenses(repo: str) -> dict[str, str]:
    owner_repo = repo.removeprefix("https://github.com/")
    files: dict[str, str] = {}
    for name in FETCH_NAMES:
        url = f"https://raw.githubusercontent.com/{owner_repo}/HEAD/{name}"
        request = urllib.request.Request(
            url, headers={"User-Agent": "odium-launcher-notices"}
        )
        try:
            with urllib.request.urlopen(request, timeout=20) as response:
                data = response.read(MAX_TEXT_BYTES)
        except (urllib.error.URLError, OSError, ValueError):
            continue
        text = (
            data.decode("utf-8", errors="replace")
            .replace("\r\n", "\n")
            .replace("\r", "\n")
            .strip()
        )
        if text and not text.startswith("404:"):
            files[name] = text
    return files


def license_label(name: str, text: str) -> str:
    upper_name = name.upper()
    if "OFL" in upper_name:
        return "OFL-1.1"
    if "UFL" in upper_name:
        return "UFL-1.0"
    if "APACHE" in upper_name:
        return "Apache-2.0"
    if "ZLIB" in upper_name:
        return "Zlib"
    if "BSL" in upper_name:
        return "BSL-1.0"
    if "BSD" in upper_name:
        return "BSD"
    if "LGPL" in upper_name:
        return "LGPL"
    if re.search(r"\bMIT\b", upper_name):
        return "MIT"
    upper_text = text[:8000].upper()
    if "UNICODE, INC. LICENSE AGREEMENT" in upper_text or "UNICODE LICENSE V3" in upper_text:
        return "Unicode-3.0"
    if "MOZILLA PUBLIC LICENSE" in upper_text and "2.0" in upper_text:
        return "MPL-2.0"
    if "CC0 1.0 UNIVERSAL" in upper_text:
        return "CC0-1.0"
    if "THIS IS FREE AND UNENCUMBERED SOFTWARE RELEASED INTO THE PUBLIC DOMAIN" in upper_text:
        return "Unlicense"
    if "APACHE LICENSE" in upper_text:
        return "Apache-2.0"
    if "BOOST SOFTWARE LICENSE" in upper_text:
        return "BSL-1.0"
    if "SIL OPEN FONT LICENSE" in upper_text:
        return "OFL-1.1"
    if "ZLIB LICENSE" in upper_text or (
        "THIS SOFTWARE IS PROVIDED 'AS-IS'" in upper_text
        and "ALTERED SOURCE VERSIONS MUST BE PLAINLY MARKED" in upper_text
    ):
        return "Zlib"
    if "PERMISSION IS HEREBY GRANTED, FREE OF CHARGE" in upper_text:
        return "MIT"
    if (
        "PERMISSION TO USE, COPY, MODIFY, AND/OR DISTRIBUTE THIS SOFTWARE" in upper_text
        and "PROVIDED THAT THE ABOVE COPYRIGHT NOTICE" in upper_text
    ):
        return "ISC"
    if "PERMISSION TO USE, COPY, MODIFY, AND/OR DISTRIBUTE THIS SOFTWARE" in upper_text:
        return "0BSD"
    if "REDISTRIBUTION AND USE IN SOURCE AND BINARY FORMS" in upper_text:
        return "BSD"
    if "GNU LESSER GENERAL PUBLIC LICENSE" in upper_text:
        return "LGPL"
    return "License text"


def main() -> int:
    metadata = cargo_metadata()
    packages = [
        package
        for package in metadata["packages"]
        if package.get("source") and not package["source"].startswith("path+")
    ]
    packages.sort(key=lambda package: (package["name"].lower(), package["version"]))

    own_files = {package["id"]: own_license_files(package) for package in packages}
    repos = {
        package["id"]: normalize_repo(package.get("repository")) for package in packages
    }

    repo_cache: dict[str, dict[str, str]] = {}
    for package in packages:
        repo = repos[package["id"]]
        if (
            repo
            and not has_full_text(own_files[package["id"]])
            and repo not in repo_cache
        ):
            print(f"fetching licenses for {repo}", file=sys.stderr)
            repo_cache[repo] = fetch_repo_licenses(repo)

    texts: dict[str, dict] = {}
    package_text_ids: dict[str, list[str]] = defaultdict(list)

    def add_text(text: str, name: str, provenance: str, package: dict) -> str:
        digest = hashlib.sha256(text.encode("utf-8")).hexdigest()[:16]
        entry = texts.get(digest)
        if entry is None:
            entry = {
                "text": text,
                "label": license_label(name, text),
                "source_name": name,
                "provenance": provenance,
                "packages": [],
            }
            texts[digest] = entry
        package_name = f'{package["name"]} {package["version"]}'
        if package_name not in entry["packages"]:
            entry["packages"].append(package_name)
        return digest

    for package in packages:
        seen_texts: set[str] = set()
        for path in own_files[package["id"]]:
            text = read_text(path)
            if text and not is_license_pointer(text):
                seen_texts.add(add_text(text, path.name, "crate package", package))
        repo = repos[package["id"]]
        if repo and repo in repo_cache:
            for name, text in repo_cache[repo].items():
                seen_texts.add(
                    add_text(text, name, f"upstream repository {repo}", package)
                )
        package_text_ids[package["id"]] = sorted(seen_texts)

    lines: list[str] = []
    lines.append("# Third-party notices")
    lines.append("")
    lines.append(
        "Odium Launcher itself is licensed under the **0BSD** license (see `LICENSE`)."
    )
    lines.append("")
    lines.append(
        "The launcher's dependency graph contains the third-party crates listed "
        "below. Each crate keeps its own license; the license texts are reproduced "
        "in this file."
    )
    lines.append("")
    lines.append(
        f"Generated from `cargo metadata`: **{len(packages)} packages**, "
        f"**{len(texts)} distinct license texts**."
    )
    lines.append("")
    lines.append("## Packages")
    lines.append("")
    lines.append("| Package | Version | License | Repository |")
    lines.append("| --- | --- | --- | --- |")

    for package in packages:
        name = package["name"]
        version = package["version"]
        license_expr = (package.get("license") or "see repository").replace("|", "\\|")
        repo = repos[package["id"]]
        repo_cell = f"[{repo}]({repo})" if repo else ""
        lines.append(f"| {name} | {version} | {license_expr} | {repo_cell} |")

    lines.append("")
    lines.append("## License texts")
    lines.append("")

    ordered = sorted(
        texts.items(),
        key=lambda item: (item[1]["label"], item[1]["source_name"], item[0]),
    )
    for index, (_digest, entry) in enumerate(ordered, start=1):
        label = entry["label"]
        lines.append(f"### {label} — text {index}")
        lines.append("")
        lines.append(f"Source: `{entry['source_name']}` ({entry['provenance']}).")
        used = entry["packages"]
        shown = ", ".join(f"`{name}`" for name in used[:8])
        if len(used) > 8:
            shown += f" and {len(used) - 8} more"
        lines.append(f"Used by: {shown}.")
        lines.append("")
        lines.append("~~~~text")
        lines.append(entry["text"])
        lines.append("~~~~")
        lines.append("")

    not_covered = [
        package for package in packages if not package_text_ids[package["id"]]
    ]
    if not_covered:
        lines.append("## Packages without an embedded license text")
        lines.append("")
        lines.append(
            "The following crates did not ship a license file and their repository "
            "could not be reached:"
        )
        lines.append("")
        for package in not_covered:
            license_expr = package.get("license") or "unknown license"
            repo = repos[package["id"]]
            suffix = f" — {repo}" if repo else ""
            lines.append(
                f"- `{package['name']} {package['version']}` — "
                f"{license_expr}{suffix}"
            )
        lines.append("")

    OUTPUT.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(
        f"wrote {OUTPUT} ({OUTPUT.stat().st_size} bytes, "
        f"{len(packages)} packages, {len(texts)} license texts)",
        file=sys.stderr,
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
