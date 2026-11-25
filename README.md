# SessionShare

A lightweight Windows desktop tray app (**Tauri v2 + Rust + Svelte 5**) that syncs your **Claude Code** sessions, project memory, skills, agents, commands, `CLAUDE.md`, `settings.json`, and `history.jsonl` across your personal and office Windows PCs using an **already-approved local synced drive folder** (OneDrive, Google Drive, Dropbox, etc.).

## Highlights

- **Zero outbound network calls from SessionShare** — reads and writes atomically to a designated folder inside your approved synced drive.
- **Automatic Windows path remapping** — translates differing local paths (e.g., `C:\Users\Personal\Dev\App` ↔ `D:\Work\Repos\App`) in both `~/.claude/projects/<encoded-path>` folder names and `.jsonl` `cwd` fields.
- **Git remote auto-matching + manual folder mapping** — repos with the same Git remote auto-link; non-Git folders can be mapped in two clicks.
- **Safe append-only `.jsonl` merging** — deduplicates by message `uuid` and merges chronologically without losing messages, with automatic `.conflict` backups for non-JSONL files.
- **System tray background sync** — watches `%USERPROFILE%\.claude` and your synced drive folder with debouncing and Windows file-lock retry/backoff.

## Running Locally

```powershell
# Run desktop app in development mode
npm run tauri dev

# Build Windows installers (.msi / NSIS .exe)
npm run tauri build

# Run Rust core tests
cargo test --workspace

# Optional CLI usage
cargo run -p sessionshare-cli -- init --sync-dir "C:\Users\You\OneDrive\SessionShare" --machine-name "Personal-PC"
cargo run -p sessionshare-cli -- sync
```
