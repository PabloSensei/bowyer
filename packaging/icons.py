"""Makes the icons of the packages from assets/icon/app-1024.png: PNG sizes for Linux
(hicolor theme) and Bowyer.icns for macOS. The Windows .ico is assets/icon/bowyer.ico,
embedded by crates/app/harness/build.rs. Run from the repository root: python packaging/icons.py
"""
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
SOURCE = ROOT / "assets" / "icon" / "app-1024.png"
LINUX_SIZES = [16, 24, 32, 48, 64, 128, 256, 512]


def main():
    image = Image.open(SOURCE).convert("RGBA")
    for size in LINUX_SIZES:
        out = ROOT / "packaging" / "linux" / "icons" / f"{size}x{size}" / "bowyer.png"
        out.parent.mkdir(parents=True, exist_ok=True)
        image.resize((size, size), Image.LANCZOS).save(out, optimize=True)
    icns = ROOT / "packaging" / "macos" / "Bowyer.icns"
    icns.parent.mkdir(parents=True, exist_ok=True)
    image.save(icns, format="ICNS")
    print("ok")


if __name__ == "__main__":
    main()
