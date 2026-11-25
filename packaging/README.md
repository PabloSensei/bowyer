# Packaging Bowyer

Packages are built by [cargo-packager](https://github.com/crabnebula-dev/cargo-packager) 0.11.8 (MIT/Apache-2.0, installed as a tool: `cargo install cargo-packager --locked --version 0.11.8`). The description is [packager.toml](packager.toml); its paths are relative to this folder. For a `v*` tag, [release.yml](../.github/workflows/release.yml) does everything below.

| System | Format | Installs to |
|---|---|---|
| Windows | NSIS installer `bowyer_<version>_x64-setup.exe` | `%LOCALAPPDATA%\Bowyer` for the current user, no administrator rights; shortcuts in the Start menu and on the desktop |
| Linux | `.deb`, `.rpm`, Arch package, AppImage | `/usr/bin/bowyer`, hicolor icons, `bowyer.desktop`, licenses in `/usr/lib/bowyer/licenses` |
| macOS | `.app`, `.dmg` | `Bowyer.app` |

Uninstalling removes the program only: the library, themes and caches (`%APPDATA%\Bowyer`, `~/.local/share/Bowyer`, `~/Library/Application Support/Bowyer`) stay. The Windows installer remembers its folder in `HKCU\Software\Bowyer\Bowyer` and leaves that key after uninstalling (the cargo-packager template does so).

## Building by hand

```shell
cargo build --release -p harness
python packaging/prepare.py
cargo packager -c packaging/packager.toml --release -f nsis
```

On Linux and macOS, run `strip target/release/bowyer` before packaging (the release profile keeps line tables: about 216 MB unstripped on Linux, 41 MB stripped) and use `-f deb,appimage` or `-f app,dmg`. The packages land in `target/packages`. The `.rpm` (Fedora, openSUSE) and the Arch package carry the files of the `.deb`: `bash packaging/linux/repackage.sh target/packages/bowyer_<version>_amd64.deb <version>` builds them with `rpmbuild` and, in an Arch Linux container, `makepkg`; `bash packaging/linux/try-packages.sh target/packages` installs every Linux package in a clean container of its distribution and checks that the libraries the program needs are there. The release workflow runs both.

`prepare.py` gathers into `target/packaging/licenses` what every package has to carry: the GPL of Bowyer itself, the licenses of the Inter and JetBrains Mono fonts and of the IntelliJ icons, the notices of code taken from Arro and Zed, and `THIRD-PARTY.txt` with every Rust crate in the binary, its license and its license files (from `cargo metadata` for the current platform).

## Icons

The source is `assets/icon/app-1024.png`. `python packaging/icons.py` rebuilds the PNGs from 16 to 512 for Linux (`linux/icons`) and `macos/Bowyer.icns`. The Windows icon is `assets/icon/bowyer.ico` (16 to 256), embedded into the exe by `crates/app/harness/build.rs`.

The window and the shortcuts share the identifier `dev.bowyer.app`, which must not change after the first release: pinned Windows shortcuts and the macOS bundle ID depend on it. On Windows it is the AppUserModelID of the installer's shortcuts and of the process (`main.rs`), so the window groups under a pinned shortcut; on Linux the window's `app_id` is `bowyer`, the `StartupWMClass` of `bowyer.desktop`.

## Updates

Bowyer looks for a new version in the feed `latest.json` and installs it only when the package's signature matches the public key built into the program. The owner of the repository makes the key pair once:

```shell
cargo packager signer generate --path bowyer-update.key
```

The private key and its password are the repository secrets `UPDATE_PRIVATE_KEY` and `UPDATE_PRIVATE_KEY_PASSWORD`; the public key (`bowyer-update.key.pub`, one line of base64) is the variable `BOWYER_UPDATE_KEY`, and the feed address is the variable `BOWYER_UPDATE_FEED`, for example `https://github.com/<owner>/<repo>/releases/latest/download/latest.json`. With the key, cargo-packager writes a `.sig` file next to each package and `update_manifest.py` turns them into the feed. A build without the variables does not look for updates (no feed) or does not install them (no key).

The NSIS installation updates by running the new installer in passive mode, an AppImage replaces itself, and the macOS bundle is swapped for the one in `Bowyer.app.tar.gz`. A `.deb` installation only learns that a version is out.

## Signing

Without certificates the packages are unsigned.

- **Windows.** cargo-packager calls [windows/sign.ps1](windows/sign.ps1) for the exe, the installer and the uninstaller. The certificate comes from the environment: `BOWYER_SIGN_PFX` and `BOWYER_SIGN_PFX_PASSWORD`, or `BOWYER_SIGN_THUMBPRINT` for a certificate in the user's store; the timestamp server is `BOWYER_SIGN_TIMESTAMP` (DigiCert by default). Without them the script says the file stays unsigned. In CI: the secrets `WINDOWS_PFX_BASE64` and `WINDOWS_PFX_PASSWORD`.
- **macOS.** cargo-packager signs the `.app` and the `.dmg` with `codesign` when `[macos]` has a `signing-identity`: in CI a step adds it from the secret `APPLE_SIGNING_IDENTITY`, with the certificate in `APPLE_CERTIFICATE` (a base64 `.p12`) and `APPLE_CERTIFICATE_PASSWORD`. Notarization reads `APPLE_API_KEY`, `APPLE_API_ISSUER` and `APPLE_API_KEY_BASE64` (or `APPLE_ID`, `APPLE_PASSWORD` and `APPLE_TEAM_ID`).
- **Linux.** Every package in a release has a `.sha256` file next to it.
