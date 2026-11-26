# SessionShare

A lightweight Windows desktop tray app (**Tauri v2 + Rust + Svelte 5**) that syncs your **Claude Code** sessions, project memory, skills, agents, commands, `CLAUDE.md`, `settings.json`, and `history.jsonl` across your personal and office Windows PCs using an **already-approved local synced drive folder** (OneDrive, Google Drive, Dropbox, etc.).

## Highlights

- **Zero outbound network calls from SessionShare** — reads and writes atomically to a designated folder inside your approved synced drive.
- **Automatic Windows path remapping** — translates differing local paths (e.g., `C:\Users\Personal\Dev\App` ↔ `D:\Work\Repos\App`) in both `~/.claude/projects/<encoded-path>` folder names and `.jsonl` `cwd` fields.
- **Git remote auto-matching + manual folder mapping** — repos with the same Git remote auto-link; non-Git folders can be mapped in two clicks.
- **Safe append-only `.jsonl` merging** — deduplicates by message `uuid` and merges chronologically without losing messages. Transcripts in active use (changed within the last 30 s by default) are never rewritten locally; incoming lines are applied once the session goes idle.
- **Graceful merging of edited files** — SessionShare keeps the last version both PCs agreed on and uses it for a 3-way merge. Edits made on different lines (or, for `settings.json`, to different keys) are merged automatically. Where both PCs changed the same lines, both copies are left untouched and the file appears in **Review → Files to merge**, which shows the two versions side by side with per-difference choices (this PC / other PC / both) and an editable result. Binary files fall back to keeping the newest copy with a backup.
- **Deletion sync with a safety net** — files deleted on one PC are removed on the other, but always moved to a trash folder first (`%APPDATA%\SessionShare\trash` locally, `.trash\` in the synced folder). If many files vanish at once, deletions are not propagated. Trash and conflict backups (`%APPDATA%\SessionShare\conflicts`) are purged after 30 days. Note: Claude Code's own cleanup of old transcripts (`cleanupPeriodDays`) is synced as a deletion too.
- **Hook approval** — incoming hook scripts and `settings.json` changes to `hooks`, `permissions`, `statusLine` or credential helpers wait in the **Review** tab until you approve them, because they can run commands on this PC.
- **Secrets stay local** — `settings.json` keys listed in Settings (default: `env`) are never written to the synced drive, and each PC keeps its own values.
- **System tray background sync** — watches only the synced parts of `%USERPROFILE%\.claude` and your synced drive folder with debouncing, plus a periodic fallback sync (default every 5 minutes). Runs as a single instance and can start at Windows sign-in.
- **Live sync progress** — while manual or background sync runs, the dashboard shows the file being checked, upcoming files in its current group, and recent changes. Group progress counts checked files rather than estimating an overall percentage.
- **System-aware appearance** — the desktop interface follows the Windows light or dark theme. Closing the window keeps sync running in the system tray; use the tray menu to quit.

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
cargo run -p sessionshare-cli -- pending
cargo run -p sessionshare-cli -- approve global/settings.json
cargo run -p sessionshare-cli -- conflicts
cargo run -p sessionshare-cli -- keep global/CLAUDE.md --side local
```
