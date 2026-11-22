<p align="center">
  <img src="assets/icon/bowyer-256.png" width="112" alt="Bowyer logo">
</p>

<h1 align="center">Bowyer</h1>

<p align="center">
  <b>One native window for every AI coding agent you run.</b><br>
  Claude Code, Codex, OpenCode and DeepSeek sessions, on this computer and over SSH, in one fast GPU-rendered app.
</p>

<p align="center">
  <a href="https://pablosensei.github.io/bowyer/"><b>Website</b></a> ·
  <a href="https://github.com/PabloSensei/bowyer/releases">Releases</a> ·
  <a href="https://github.com/PabloSensei/arro">Arro, the sibling IDE</a>
</p>

<p align="center">
  <a href="https://github.com/PabloSensei/bowyer/releases/latest"><img alt="Latest release" src="https://img.shields.io/github/v/release/PabloSensei/bowyer?include_prereleases&label=release&color=EA580C"></a>
  <a href="https://github.com/PabloSensei/bowyer/releases"><img alt="Downloads" src="https://img.shields.io/github/downloads/PabloSensei/bowyer/total?color=EA580C"></a>
  <a href="LICENSE"><img alt="License: GPL-3.0-or-later" src="https://img.shields.io/badge/license-GPL--3.0--or--later-1F1F23"></a>
  <img alt="Platform: Windows 11, Linux x86-64, macOS" src="https://img.shields.io/badge/platform-Windows%2011%20%C2%B7%20Linux%20%C2%B7%20macOS-1F1F23">
  <a href="https://github.com/sponsors/PabloSensei"><img alt="Sponsor" src="https://img.shields.io/badge/sponsor-%E2%99%A5-EA580C"></a>
</p>

<p align="center">
  <img alt="Rust" src="https://img.shields.io/badge/Rust-000000?style=for-the-badge&logo=rust&logoColor=white">
  <img alt="GPUI" src="https://img.shields.io/badge/GPUI-GPU%20rendered-1F1F23?style=for-the-badge">
  <img alt="Claude Code" src="https://img.shields.io/badge/Claude%20Code-D97757?style=for-the-badge&logo=anthropic&logoColor=white">
  <img alt="Codex" src="https://img.shields.io/badge/Codex-1F1F23?style=for-the-badge">
  <img alt="OpenCode" src="https://img.shields.io/badge/OpenCode-1F1F23?style=for-the-badge">
  <img alt="MCP" src="https://img.shields.io/badge/MCP-server-EA580C?style=for-the-badge">
  <img alt="SQLite FTS5" src="https://img.shields.io/badge/SQLite-FTS5-003B57?style=for-the-badge&logo=sqlite&logoColor=white">
  <img alt="SSH" src="https://img.shields.io/badge/SSH-remote%20hosts-1F1F23?style=for-the-badge&logo=gnubash&logoColor=white">
</p>

<p align="center">
  <a href="https://github.com/PabloSensei/bowyer/releases/latest"><b>Download</b></a> ·
  <a href="#features">Features</a> ·
  <a href="#install">Install</a> ·
  <a href="#build-from-source">Build</a> ·
  <a href="#status">Status</a> ·
  <a href="https://github.com/PabloSensei/bowyer/issues/new">Report a bug</a>
</p>

<p align="center">
  <img src=".github/readme/hero.png" alt="Bowyer in the Dark theme: the session tree groups three agent sessions under the Claude Code session that started them, the conversation shows folded tool calls and a code block, and the side panel shows the context window, the session cost and the changed files" width="100%">
</p>

> [!WARNING]
> **Early preview.** Bowyer is young. Expect rough edges and breaking changes between versions. Bug reports are very welcome.

## Why Bowyer

- **Every agent, one place.** Claude Code, Codex, OpenCode and DeepSeek each keep their history in their own folder, format and terminal. Bowyer reads all of them and shows every session you ever ran, on this machine and on your servers, in one searchable tree.
- **Native and fast.** Rust from top to bottom and a GPU-rendered UI on GPUI. No Electron, no web view. Hundreds of sessions load in a moment and search as you type.
- **Your files stay yours.** Bowyer only reads what the harnesses write. Folders, pins, names and notes live in Bowyer's own database; your agents keep working exactly as before, with or without the window open.
- **See what a session really costs.** The context window split by what fills it, token totals, cost at API prices, and whether the prompt cache is still warm, so you know when compacting pays off.
- **Keep going from the window.** Type a message and the harness runs the turn in the background, here or on an SSH host. The answer streams in; permission prompts and questions show up as buttons, not as a terminal you have to find.

## Features

<table>
  <tr>
    <td width="50%" valign="top">
      <h3>Every session in one tree</h3>
      Sessions grouped by project, environment or harness, with your own folders, drag and drop, pins, renames and four sort orders. Sub-agent and worktree sessions fold under the session that started them, long projects show the latest ten with <i>Show more</i>, and one button collapses everything. Live state for sessions running in a terminal elsewhere: <i>working</i>, <i>waiting</i> or <i>your turn</i>, with a taskbar flash when one needs you. <code>Ctrl+K</code> searches titles, prompts, folders and the whole conversation text (SQLite FTS5).
    </td>
    <td width="50%" valign="top">
      <img src=".github/readme/tree.png" alt="The session tree: shop-api with a Claude Code session and three agent sessions nested under it, web-dashboard and infra-scripts">
    </td>
  </tr>
  <tr>
    <td width="50%" valign="top">
      <img src=".github/readme/chat-light.png" alt="A conversation in the Light theme with folded thinking, a one-line summary of seven tool calls, highlighted code and a follow-up turn">
    </td>
    <td width="50%" valign="top">
      <h3>A chat, not a terminal dump</h3>
      Markdown, highlighted code blocks with Copy and Run, folded thinking, and tool calls folded into one line per run that open into commands, output and diffs. Background task notices fold into a short line. Paste screenshots, drop files, <code>/</code> for commands and skills, <code>@</code> for files of the session's folder. Select text across messages and ask about it. Claude Code and OpenCode ask before a command or edit their settings don't allow, right in the window, and Claude's questions come with their choices and a field for your own answer.
    </td>
  </tr>
  <tr>
    <td width="50%" valign="top">
      <h3>What a session holds</h3>
      Environment, folder, Git branch (worktrees too) and model. The context window split by what fills it: for Claude Code exactly as Claude counts it, with system prompt, tools, MCP servers, skills and messages. Token totals, cost at API prices per call, the price of the next turn and of a compaction at the current cache state, and a countdown for the prompt cache. Every file the session changed as a tree, collapsed until you open it, and every command and sub-agent it ran.
    </td>
    <td width="50%" valign="top">
      <img src=".github/readme/session.png" alt="The Session panel: context 51k of 1M split into six parts, session cost $0.227 with a warm prompt cache, token totals, changed files and commands" width="60%">
    </td>
  </tr>
  <tr>
    <td width="50%" valign="top">
      <img src=".github/readme/diff.png" alt="A diff of checkout.rs in the Changes tab with Stage and Discard for the hunk">
    </td>
    <td width="50%" valign="top">
      <h3>Files, Git and a terminal</h3>
      A Files tab with the folder's tree and a preview of text and pictures, locally or over SSH. A Changes tab with staged and unstaged files, diffs by hunk with Stage, Unstage and Discard, commit, fetch, pull, push and branch switching. A built-in terminal (<code>Ctrl+`</code>) opens in the session's folder, here or on the SSH host, so you never hunt for the right window to resume a session.
    </td>
  </tr>
</table>

And more:

- **SSH hosts from the window.** Add `user@host` with a key of its own or, on Windows, a password kept in Windows Credential Manager. The connection is checked before it is saved, an unknown host key is shown by its fingerprint before you trust it, and sessions on the host appear next to local ones.
- **Memory for your agents.** `bowyer mcp` is an MCP server: an agent searches and reads past sessions of the project and keeps notes there instead of carrying them in its context. One switch turns it off, another turns it off for a single project, and the window shows the command that connects it to Claude Code, Codex or OpenCode.
- **MCP servers and plugins** of each harness at a glance; Claude Code plugins switch on and off (Bowyer keeps a backup of the settings it changes).
- **Settings in one place:** language, appearance, sessions, harnesses, SSH hosts, terminal, search, memory and updates.
- **Themes.** Seven built in (Dark, Light, High Contrast, Retro, Nord, Mocha, Paper), your own as JSON with a live editor, and import of VS Code themes and IntelliJ `.icls` schemes. The interface speaks English, Simplified Chinese, Spanish, Brazilian Portuguese, German, French, Japanese and Russian.
- **Stays out of the way.** On Windows, closing the window can keep Bowyer in the notification area, watching your sessions.
- **Updates itself** from a signed feed: it checks on start and twice a day, and installs only when you click.

## Install

Download the package for your system from [Releases](https://github.com/PabloSensei/bowyer/releases/latest). Every file has a `.sha256` file next to it.

Bowyer does not bundle the agents: install Claude Code, Codex, OpenCode or DeepSeek Harness the usual way. Bowyer finds their history in your home folder (or on the SSH host) and calls their command line tools to run new turns.

### Windows 11 x86-64

1. Run `bowyer_<version>_x64-setup.exe`. It installs for the current user into `%LOCALAPPDATA%\Bowyer`, without administrator rights, and adds Start menu and desktop shortcuts.
2. The installer is not code-signed yet, so SmartScreen may stop it: choose **More info**, then **Run anyway**.

### Linux x86-64

The build needs glibc 2.35 or newer: Ubuntu 22.04, Debian 12, Fedora 36 or a later release.

On Debian and Ubuntu, install the package:

```shell
sudo apt install ./bowyer_<version>_amd64.deb
```

Anywhere else, use the AppImage:

```shell
chmod +x bowyer_<version>_x86_64.AppImage
./bowyer_<version>_x86_64.AppImage
```

The AppImage needs `libfuse2`; the window needs `libfontconfig`, `libwayland`, `libx11`, `libxcb` and `libxkbcommon` at run time.

### macOS (Apple silicon)

1. Open `Bowyer_<version>_aarch64.dmg` and drag `Bowyer` into Applications.
2. The app is not notarized, so Gatekeeper stops the first start. Control-click `Bowyer.app`, choose **Open** and confirm, or run `xattr -dr com.apple.quarantine /Applications/Bowyer.app`.

### Updates and limitations

Bowyer checks for a new version on start and twice a day (Settings → Updates turns that off) and installs it only after you click **Install and restart**. The package is accepted only when its signature matches the key built into Bowyer. The Windows installer, the AppImage and the macOS app update themselves; a `.deb` installation only tells you that a version is out.

Your library, themes and caches live in `%APPDATA%\Bowyer`, `~/.local/share/Bowyer` or `~/Library/Application Support/Bowyer` and survive uninstalling.

Known limitations of this preview:

- Linux and macOS are built and tested by CI, but used far less than the Windows build.
- Codex and DeepSeek Harness support is written but has not been tried against a real installation yet. Claude Code and OpenCode are used every day.
- The packages are not code-signed or notarized.
- The notification area icon is Windows only.

## Build from source

The Rust toolchain is pinned in [`rust-toolchain.toml`](rust-toolchain.toml). On Linux install `libfontconfig1-dev libwayland-dev libx11-dev libxcb1-dev libxkbcommon-dev libxkbcommon-x11-dev` first.

```shell
cargo run --release -p harness
cargo test --workspace --exclude ui-kit
```

The binary is `bowyer`. `--data-dir <folder>` keeps the database somewhere else, `BOWYER_LOG=info` (or `debug`) prints the log to standard error, and `bowyer mcp` runs the memory server instead of the window. Installers are built with cargo-packager; [packaging/README.md](packaging/README.md) explains how, including update signing.

## Status

| Area | State |
|---|---|
| Session tree, search, conversation view | ✅ Works every day |
| Claude Code: history, live turns, permissions, plugins | ✅ Works |
| OpenCode: history, live turns over the Agent Client Protocol | ✅ Works |
| Codex, DeepSeek Harness | 🚧 Written, untested |
| SSH hosts: sessions, files, Git, terminal, live turns | ✅ Works |
| Git changes, built-in terminal | ✅ Works |
| Memory MCP server | 🚧 New |
| Updates from a signed feed | 🚧 New |
| Windows 11 x86-64 | ✅ Supported |
| Linux x86-64 | 🚧 Preview release |
| macOS (Apple silicon) | 🚧 Preview release |

## Support

Bowyer is free. If it saves you time, you can support its development (mostly AI subscriptions) through [GitHub Sponsors](https://github.com/sponsors/PabloSensei).

Found a problem? [Open an issue](https://github.com/PabloSensei/bowyer/issues/new).

## Acknowledgements

Bowyer stands on the work of others: [GPUI](https://github.com/zed-industries/zed) by Zed Industries for the UI, [alacritty_terminal](https://github.com/alacritty/alacritty) for the terminal, [SQLite](https://sqlite.org) through [rusqlite](https://github.com/rusqlite/rusqlite) for the library and search, [minisign-verify](https://github.com/jedisct1/rust-minisign-verify) for update signatures, [cargo-packager](https://github.com/crabnebula-dev/cargo-packager) for the installers, [Lucide](https://lucide.dev) and the IntelliJ Platform icons for icon shapes, and the [Inter](https://rsms.me/inter/) and [JetBrains Mono](https://www.jetbrains.com/lp/mono/) fonts. Much of the window's foundation comes from [Arro](https://github.com/PabloSensei/arro).

## License

Bowyer is free software, licensed under the [GNU General Public License, version 3 or later](LICENSE) (GPL-3.0-or-later). You may use it for any purpose, including at work, study and change it, and share it, changed or not; whoever shares a changed version shares its source under the same license.

Third-party components keep their own licenses; every package carries them in its `licenses` folder, with `THIRD-PARTY.txt` listing every Rust crate in the binary. The `NOTICE-ARRO.md` files of the GPUI platform crates in [`vendor/`](vendor/) list the changes made to them.
