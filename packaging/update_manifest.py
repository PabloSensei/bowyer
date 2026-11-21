"""Writes latest.json, the update feed of a release, from its packages and their signatures.

    python packaging/update_manifest.py <folder of packages> <version> <base url> [notes file]

Each package that can update an installation goes in with the text of its `.sig` file, which
cargo-packager writes when CARGO_PACKAGER_SIGN_PRIVATE_KEY is set: the NSIS installer for
Windows, the AppImage for Linux, `Bowyer.app.tar.gz` for macOS. `<base url>` is where the
release's files can be downloaded, without the trailing slash. Synthetic notes are fine.
"""

import json
import pathlib
import sys

# Which package updates which platform, as the update crate names them (`<os>-<arch>`).
KINDS = [
    ("-setup.exe", "windows-x86_64", "nsis"),
    (".AppImage", "linux-x86_64", "appimage"),
    (".app.tar.gz", "macos-aarch64", "app"),
]


def main() -> int:
    if len(sys.argv) < 4:
        print(__doc__, file=sys.stderr)
        return 2
    folder = pathlib.Path(sys.argv[1])
    version = sys.argv[2].removeprefix("v")
    base = sys.argv[3].rstrip("/")
    notes = pathlib.Path(sys.argv[4]).read_text(encoding="utf-8") if len(sys.argv) > 4 else ""
    platforms = {}
    for package in sorted(folder.iterdir()):
        for suffix, platform, kind in KINDS:
            if not package.name.endswith(suffix):
                continue
            signature = package.with_name(package.name + ".sig")
            if not signature.is_file():
                print(f"no signature for {package.name}: left out of the feed", file=sys.stderr)
                continue
            platforms[platform] = {
                "url": f"{base}/{package.name}",
                "signature": signature.read_text(encoding="utf-8").strip(),
                "format": kind,
            }
    feed = {"version": version, "notes": notes.strip(), "page": base.replace("/download/", "/tag/"), "platforms": platforms}
    json.dump(feed, sys.stdout, indent=2)
    print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
