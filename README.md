# Win Manager RS

An original, open-source Windows system-utility suite written in Rust with an
[egui](https://github.com/emilk/egui) interface: system information, optimisation,
cleaning, customisation, security, and networking tools in one native executable.

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
**Restart as administrator**.

## Layout

| Path | Purpose |
|------|---------|
| `app/` | eframe binary: window shell, sidebar, built-in Home / Change Journal / Settings |
| `crates/core/` | `Tool` trait, config, i18n (Fluent), elevation, background tasks, safety layer |
| `crates/tools-*/` | Tool crates, added phase by phase (info, optimizer, cleaner, …) |

## Safety model

Every change to the system goes through `wm_core::safety::ChangeJournal`:

- Registry keys are exported to `.reg` before modification (`backup_reg_key`).
- Files are moved to the Recycle Bin, never hard-deleted (`recycle`).
- Each change is journaled with an undo action, visible in **Change Journal**.
- Bulk operations create a System Restore point first (if enabled in Settings).
- **Dry-run** mode makes tools preview changes without applying them.

User data lives in `%APPDATA%\win-manager-rs\` (config, journal, backups, logs).

## Adding a tool

1. Implement `wm_core::Tool` (`id`, `category`, `ui`, optionally `requires_admin`).
2. Add `<id>-title = …` to `crates/core/locales/en-US.ftl`.
3. Register it in `app/src/tools/mod.rs`.
4. Run long work with `wm_core::task::BackgroundTask`; never block `ui()`.
