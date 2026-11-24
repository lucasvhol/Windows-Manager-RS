# Win Manager RS

An original, open-source Windows system-utility suite written in Rust with an
[egui](https://github.com/emilk/egui) interface: 50 tools for system information,
optimisation, cleaning, customisation, security and networking in one native executable.

It is a clean-room implementation. No code, data, or assets from any other product are used.

## Build & run

Requires the Rust toolchain (MSVC target) and the Visual Studio C++ Build Tools.

```powershell
cargo run -p win-manager            # debug build
cargo build -p win-manager --release
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

The app starts unelevated (`asInvoker`). Tools that need administrator rights offer
**Restart as administrator**. Some tests exercise administrator-only paths and skip
themselves when not elevated (e.g. `cargo test -p wm-tools-security real_volume`).

### Headless actions

`win-manager.exe --run <action>` performs a maintenance action without opening a window
and exits with 0 on success. **My Task** schedules these with Task Scheduler.

| Action | Effect | Admin |
|---|---|---|
| `junk-clean` | Junk File Cleaner default selection | no |
| `empty-recycle-bin` | Empty the Recycle Bin | no |
| `clear-traces` | Run history, address bar history, recent files | no |
| `flush-dns` | Flush the DNS cache | yes |
| `restore-point` | Create a System Restore point | yes |

## Tools

| Category | Tools |
|---|---|
| 1-Click | 1-Click Cleaner, 1-Click Fixer, Optimization Wizard, Repair Center |
| Information | System Information, Process Manager, Hardware Monitor, Event Viewer, Device Manager |
| Optimizer | Startup Manager, Service Manager, Task Scheduler, Boot Configuration, System Speed |
| Cleaner | Disk Analyzer, Junk File Cleaner, Duplicate Files Finder, Desktop Cleaner, Smart Uninstaller, Windows Apps Uninstaller, Component Store Cleaner, Registry Cleaner |
| Customization | Visual Customizer, File Explorer Tweaker, This PC Manager, Navigation Pane, Context Menu Manager, Win+X Menu Editor, Start Menu Manager, Hotkey Manager, Run Shortcut Creator, JumpList Quick Launcher |
| Security | Privacy Protector, Settings Security, File Security, Security Lock, File Undelete |
| Network | IP Switcher, Wi-Fi Manager, URL Manager, Network Tools, Network Monitor |
| Misc. Utilities | File Splitter, Super Copy, Registry Tools, Windows Utilities, My Task |

## Layout

| Path | Purpose |
|------|---------|
| `app/` | eframe binary: window shell, sidebar, Home / Change Journal / Settings, `--run` mode |
| `crates/core/` | `Tool` trait, config, i18n (Fluent), elevation, background tasks, registry/ACL/shell helpers, tweak engine, safety layer |
| `crates/tools-info/` | Information tools |
| `crates/tools-optimizer/` | Optimizer tools (`tweaks/speed.toml`) |
| `crates/tools-cleaner/` | Cleaner tools (`rules/junk.toml`) |
| `crates/tools-custom/` | Customization tools (`tweaks/*.toml`) |
| `crates/tools-security/` | Security tools, password encryption, read-only NTFS reader |
| `crates/tools-network/` | Network tools |
| `crates/tools-misc/` | Misc. utilities |
| `crates/tools-oneclick/` | 1-Click tools, headless actions, My Task |

## Safety model

Every change to the system goes through `wm_core::safety::ChangeJournal`, shown in
**Change Journal** with an Undo button:

- Registry values are snapshotted exactly (type and bytes, or "absent") and restored on undo;
  whole keys are exported to `.reg` first.
- Files are moved to the Recycle Bin, moved aside, or backed up; the Junk File Cleaner is the
  one tool that deletes permanently by default (it says so before running).
- File permissions are saved as SDDL (or with `icacls /save` for whole trees).
- Commands that change state record their exact reverse command.
- Bulk operations can create a System Restore point first (Settings).
- **Dry-run** mode makes tools preview changes without applying them.
- The journal file is shared safely between the app and scheduled `--run` processes.

User data lives in `%APPDATA%\win-manager-rs\` (config, journal, backups, logs).

## Adding a tool

1. Implement `wm_core::Tool` (`id`, `category`, `ui`, optionally `requires_admin`).
2. Add `<id>-title = …` to `crates/core/locales/en-US.ftl`.
3. Register it in `app/src/tools/mod.rs`.
4. Run long work with `wm_core::task::BackgroundTask` (or `Sampler` for live data);
   never block `ui()`. Journal every change and honour `config.dry_run`.
