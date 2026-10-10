# claude-panel

A terminal dashboard for Claude Code: plan limits, token usage, skills, plugins and MCP servers.
It refreshes by itself whenever Claude Code writes its files. It never calls a model, so it uses no tokens.

## Install

**Windows**: download the `.msi` from the [latest release](https://github.com/ToroVtn/claude-panel/releases/latest) and run it.
It adds a Start Menu shortcut and puts `claude-panel` on your `PATH`.

**Linux / macOS**

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/ToroVtn/claude-panel/releases/latest/download/claude-panel-installer.sh | sh
```

On Linux the app-menu icon appears after the first run.

**Arch (AUR)**: `yay -S claude-panel-bin`

**Homebrew**: `brew install ToroVtn/tap/claude-panel`

**Cargo**: `cargo install claude-panel`

## Use

Run `claude-panel`, or click its icon. It needs the `claude` CLI on your `PATH` to install, uninstall or toggle plugins.
Press `q` to quit; the bottom line lists the keys for each tab.

## Uninstall

- **Windows**: Settings > Apps > Claude Panel > Uninstall.
- **AUR / Homebrew**: `pacman -R claude-panel-bin` / `brew uninstall claude-panel`.
- **Install script or cargo**: `claude-panel --uninstall` removes the binary and its app-menu icon.

## License

MIT
