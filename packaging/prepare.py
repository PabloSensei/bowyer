"""Gathers the licenses every package of Bowyer carries into target/packaging/licenses:

- LICENSE.txt: the GPL-3.0-or-later text of Bowyer itself;
- the notices of what is built into the executable (fonts, the IntelliJ icons, code taken
  from Arro and Zed);
- THIRD-PARTY.txt: every Rust crate linked into `bowyer`, with its license and the license
  files the crate ships.

Run from the repository root: python packaging/prepare.py
"""
import json
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "target" / "packaging" / "licenses"

NOTICES = {
    "LICENSE.txt": "LICENSE",
    "Inter-LICENSE.txt": "crates/ui/ui-kit/assets/fonts/Inter-LICENSE.txt",
    "JetBrainsMono-OFL.txt": "crates/ui/ui-kit/assets/fonts/JetBrainsMono-OFL.txt",
    "IntelliJ-icons-NOTICE.txt": "crates/ui/ui-kit/assets/icon-sets/intellij/NOTICE",
    "IntelliJ-icons-LICENSE.txt": "crates/ui/ui-kit/assets/icon-sets/intellij/LICENSE",
    "ui-kit-NOTICE.md": "crates/ui/ui-kit/NOTICE.md",
    "theme-NOTICE.md": "crates/ui/theme/NOTICE.md",
    "gpui-linux-NOTICE.md": "vendor/gpui-pre-linux/NOTICE-ARRO.md",
    "gpui-macos-NOTICE.md": "vendor/gpui-pre-macos/NOTICE-ARRO.md",
    "gpui-windows-NOTICE.md": "vendor/gpui-pre-windows/NOTICE-ARRO.md",
}

LICENSE_NAMES = ("license", "licence", "copying", "notice", "unlicense")


def linked_crates():
    """The crates in the dependency graph of the `harness` package for this platform, without
    the workspace's own and the build-only ones."""
    metadata = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--filter-platform", host_triple()],
            cwd=ROOT,
            check=True,
            capture_output=True,
        ).stdout
    )
    packages = {p["id"]: p for p in metadata["packages"]}
    nodes = {n["id"]: n for n in metadata["resolve"]["nodes"]}
    workspace = set(metadata["workspace_members"])
    start = next(id for id in workspace if packages[id]["name"] == "harness")
    seen, stack = set(), [start]
    while stack:
        id = stack.pop()
        if id in seen:
            continue
        seen.add(id)
        for dep in nodes[id]["deps"]:
            kinds = {k["kind"] for k in dep["dep_kinds"]}
            if kinds & {None, "normal"}:
                stack.append(dep["pkg"])
    return sorted(
        (packages[id] for id in seen if id not in workspace),
        key=lambda p: (p["name"], p["version"]),
    )


def host_triple():
    out = subprocess.run(["rustc", "-vV"], check=True, capture_output=True, text=True).stdout
    return next(line.split(": ", 1)[1] for line in out.splitlines() if line.startswith("host: "))


def license_texts(package):
    folder = Path(package["manifest_path"]).parent
    texts = []
    for path in sorted(folder.iterdir()):
        if path.is_file() and path.name.lower().startswith(LICENSE_NAMES):
            try:
                texts.append((path.name, path.read_text(encoding="utf-8", errors="replace")))
            except OSError:
                pass
    return texts


def main():
    if OUT.exists():
        shutil.rmtree(OUT)
    OUT.mkdir(parents=True)
    for name, source in NOTICES.items():
        shutil.copyfile(ROOT / source, OUT / name)
    crates = linked_crates()
    lines = [
        "Bowyer is built from the Rust crates below. Each is listed with its license and the",
        "license files it ships.",
        "",
    ]
    for package in crates:
        lines.append("=" * 78)
        lines.append(f"{package['name']} {package['version']}")
        lines.append(f"License: {package.get('license') or 'see the files below'}")
        if package.get("repository"):
            lines.append(f"Source: {package['repository']}")
        for name, text in license_texts(package):
            lines.append("")
            lines.append(f"--- {name} ---")
            lines.append(text.rstrip())
        lines.append("")
    (OUT / "THIRD-PARTY.txt").write_text("\n".join(lines), encoding="utf-8", newline="\n")
    print(f"{len(crates)} crates, licenses in {OUT}")


if __name__ == "__main__":
    sys.exit(main())
