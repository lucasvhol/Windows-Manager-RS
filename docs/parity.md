# Feature parity checklist

This file tracks parity between **Win Manager RS** and the features of a commercial Windows utility suite. Those features were taken from its public documentation and from watching its windows from the outside, never from its code or assets. Every entry is written in our own words.

**Status:** ✅ done · 🟡 partial · ❌ missing · ⛔ out of scope (reason given)

**Wave:** the work wave that closes the gap, from the parity plan.
- B: interface
- C: system tweak pages
- D: options in existing tools
- E: new tools

## Interface

| Feature | Status | Wave |
|---|---|---|
| Header band with app name, version, Update and About | ✅ (Update button arrives with the update checker, E) | B |
| Left category column with large icons; selected item highlighted with a notch | ✅ | B |
| Two-column grid of tool tiles (icon, title, one-line description); split tiles with a dropdown | ✅ | B |
| Each tool opens in its own window; several can be open at once | ✅ | B |
| Tool window shell: description band with Help, icon-over-label toolbar, left section list or tree, numbered page tabs, footer buttons, status bar with count and search | 🟡 (band on every window; toolbar, section list, numbered tabs and status bar widgets in `wm_core::ui`, adopted per tool as C/D rework each one) | B→C/D |
| Tweak pages: staged changes with Save, plus Restore to Windows defaults | ✅ (switches; number/choice settings still apply on their own button) | B |
| OS edition and build shown in the hub | ✅ | B |
| Settings: program password, minimise to tray, UI scale and font, list row height, language, light/dark | 🟡 (password, UI scale, language, theme; tray moves to D with the My Task background monitor, which needs the same tray icon) | B→D |
| Settings: save all applied tweaks to a file and restore them later (e.g. after a reinstall) | ✅ | B |
| Settings: reset a tool's saved settings; back up and restore program settings | 🟡 (back up / restore done; per-tool reset in D) | B→D |
| Frequently used tools list | ✅ | B |
| Update check (own releases) | ❌ | E |
| Licensing, registration, trial countdown | ⛔ the original's business model, not a feature | — |

## Automation

| Tool | Status | Missing | Wave |
|---|---|---|---|
| System Protection | ✅ | Restore points listed, created and deleted; protection on/off per drive; RegBack switch (Other Security); System Restore, recovery options, recovery drive, restart to advanced startup | C |
| 1-Click Kill Processes | ❌ | Escalating kill tiers (standard, enhanced, maximum); protected list; log | E |
| 1-Click Fixer | 🟡 | 10 checked items in one list: component store health, system files, WMI rebuild, system partition scan, all-volume health, internet connection, file associations, folder permissions, registry permissions, Explorer/shell. Select All, a link to Repair Center, Start, and a collapsible log | D |
| 1-Click Cleaner | 🟡 | Choose the items (privacy, junk, registry, registry defrag); silent mode; shut down when done; last-clean date | D |
| Repair Center | 🟡 (SFC, DISM, CHKDSK) | Sections: system repair, misc repair, Windows apps, event viewer, unlock system, security lock, file associations, context menu, icon repair, network problems, troubleshooting, system reset. Numbered pages of described repair boxes; repair logs | D |
| Optimization Wizard | 🟡 (tweak goals) | 12-step guided sequence that launches each tool in turn (Process Manager, Windows Update, uninstaller, startup, tasks, services, junk, registry cleaner, registry defrag, defrag unless SSD); environment detection (SSD, touch, DPI); create a task | D |

## Information

| Tool | Status | Missing | Wave |
|---|---|---|---|
| System Information | ✅ | Summary (OS, CPU, memory, board, graphics, storage, network, public IP on request) with save/copy; licence, activation, firmware key (hidden until shown) and Experience Index scores with a re-run; user accounts; installed updates with uninstall; printers; codecs; environment; disk health (SMART); disk speed test (sequential and 4K, unbuffered); battery, energy, sleep-study and performance reports | D |
| Process Manager | ✅ | User, company and signature columns (catalog-aware); end, end tree, suspend, resume, restart; priority; block / unblock through Image File Execution Options; end all unsigned, untrusted, high-CPU or high-memory processes (critical ones excluded); views (mine, not Microsoft, unsigned); window list to find a window's process (instead of a drag crosshair); details with command line, start time, parent, threads and loaded modules; open folder; online lookup | D |
| Event Viewer | ✅ | Time range (hour, day, week, month) in the query itself; level filters; group by source and event ID with counts; CSV export | D |
| Device Manager | ✅ | Disconnected devices shown; uninstall a device or every disconnected one; scan for hardware changes; driver backup (export) and restore (install from folder); USB history clearing on Device Security | D |
| Hardware Monitor | 🟡 | Charts; sensor table with current and maximum; NVIDIA GPU load, memory, temperature, power, fan and clock (NVML); SMART disk temperatures (administrator); battery; temperature alarm with sound. AMD/Intel GPU sensors not yet (no hardware to test ADLX / IGCL). Voltages, fans and CPU power need a kernel driver | D, ⛔ partly |

## Optimizer

| Tool | Status | Missing | Wave |
|---|---|---|---|
| System Speed | ✅ (35 settings) | NTFS cache, kernel paging, Prefetcher, power throttling, Fast Startup, hibernation, power plans (Ultimate Performance, restore defaults), Group Policy network wait, disk-check countdown, verbose status, Last Known Good, boot file placement, crash dump type and options, TRIM, Automatic Maintenance, Delivery Optimization, GPU scheduling, game recording. Power-plan editor goes to D | C |
| Boot Configuration | 🟡 | Add entry; boot order; back up and restore boot data; Safe Mode with Command Prompt; disable driver signing; restart into advanced startup | D |
| Startup Manager | ✅ | Run/RunOnce keys (per user and machine), Startup folders and Windows apps with Task Manager-compatible enable flags; move, edit, delete (journaled), open location, online lookup, batch "disable others"; company, signature and running columns; advanced auto-start locations (read-only); block Run keys per scope | D |
| Service Manager | ✅ | Services and Drivers tabs; Non-Microsoft filter (catalog-aware signatures, svchost ServiceDll resolved); log-on, company and command-line details; file properties, open folder, online lookup; delete third-party services (registry backed up, optional file to Recycle Bin); back up / restore startup types (JSON, plus a first-run snapshot); restore Windows defaults for common services; Optimize questionnaire. Drivers are read-only | D |
| Task Scheduler Manager | ✅ | Folder tree with counts; trigger, next/last run, last result, author and description columns; create tasks (one time, daily, weekly, monthly, log on, startup, idle, event, lock/unlock; as SYSTEM / highest privileges) through locale-independent XML; import/export task XML; run, end, enable, disable, delete with XML backup; Optimize questionnaire for optional Windows tasks; open Task Scheduler | D |

## Cleaner

| Tool | Status | Missing | Wave |
|---|---|---|---|
| Disk Analyzer | 🟡 | Analyze drives (dropdown) or folder; rescan branch; explore folder; unit selector; export (HTML, CSV, text tree); add to context menu | D |
| Component Store Cleaner | 🟡 | Superseded-component cleanup; ScanHealth and RestoreHealth | D |
| Smart Uninstaller | 🟡 | Batch uninstall; remove entry; rename; details; icon or list view; sort; online lookup; hotfixes; Windows apps; unavailable Program Files folders; uninstall OneDrive; export | D |
| Windows Apps Uninstaller | 🟡 | Icon grid; export; launch; desktop shortcut; re-register; filter (Microsoft, non-Microsoft, built-in); batch uninstall; force delete; background colour | D |
| Desktop Cleaner | 🟡 | Columns for size, user, last access and hits; select by usage; move to an "unused" folder; organize; schedule as a task; options | D |
| Junk File Cleaner | 🟡 | Drive selection; invalid Start Menu and Program Files folders; broken shortcuts; empty folders; zero-byte files; installer leftovers; hotfix backups; burn temp; ETL logs; skip list; skip system files | D |
| Duplicate Files Finder | 🟡 | Drive list with usage bars; "only these folders" toggle; match by name and/or content; size range; type patterns; skip hidden, system or read-only; selection presets; move to folder; rename | D |
| Registry Cleaner | 🟡 | Scan-item tree with about 22 categories; exclusion list; backup choices; import .reg | D |
| Registry Defrag | ❌ | Hive list with current and compacted size, rate and result; Analyze, then Compact at reboot | E |

## Customization

| Tool | Status | Missing | Wave |
|---|---|---|---|
| Custom System | 🟡 (34 settings) | Desktop icons and version, shortcut arrow, wallpaper quality, taskbar (combine, all displays, last window, preview delay, animations, Widgets, search), Start, snapping and Alt+Tab, notifications, accent colour, lock and sign-in screen, Num Lock, Windows keys, accessibility prompts, usage and icon-cache resets. Still missing: theme schedule (needs a scheduled task, D), Caps Lock remap (binary Scancode Map, D), fonts (Visual Customizer, D) | C→D |
| Windows App Settings | ✅ | Where desktop apps may be installed from, suggested-app reinstalls, background Store apps, welcome and finish-setup prompts, shortcuts to Windows' app pages | C |
| File Explorer Tweaker | 🟡 (29 settings) | Launch location, folder windows, status bar, info tips, name case, open limit, thumbnails and overlays, preview pane, label shadow, compressed colour, drives, sync ads, Sharing Wizard, merge prompt, folder-view memory and reset; Navigation Pane and This PC split tiles. Still missing: drag action and naming templates (per-file-type / undocumented values, D); Ribbon commands do not exist on Windows 11 24H2+ | C→D |
| Visual Customizer | 🟡 | Sections: system settings (title bar), font and size, system icons, other icons, file-type icons, folder colorizer, theme changer, lock-screen extractor, Bing images; numbered pages; Restore and Save | D |
| JumpList Quick Launcher | 🟡 | Several named lists; groups; files, folders, URLs, Start items, utilities and apps; edit pane; maximum count; backup and restore | D |
| Hotkey Manager | 🟡 | Targets: file, folder, URL, Start item, utility, Windows app; enable/disable; backup and restore; conflict detection; kill-all hotkey | D |
| Run Shortcut Creator | 🟡 | Type and available columns with broken entries in red; folder and URL targets; window mode; parameters; all-users scope; backup; execute | D |
| Context Menu Manager | 🟡 | Tree of 13 locations with company, Shift, position and signature columns; properties; open in registry; per file type; Open With list; add-menu editor with about 40 built-in commands; New menu; Send To; repair | D |
| Start Menu Manager | 🟡 | Tree list with user and available columns; rename; launch; pin to taskbar; Pin Any; backup and restore of taskbar pins and Start layout | D |
| Win+X Menu Editor | 🟡 | Groups (add, rename, move, remove); add file, Start item or utility; restore defaults; run as admin. Needs the WinX hash | D |
| Navigation Pane Manager | 🟡 (tweaks) | Add folders and utilities as items; order; enable/disable; place on top; edit pane | D |
| This PC Manager | 🟡 (tweaks) | Add file, folder, utility or app; rename; icon; enable/disable; Desktop Icon Settings | D |

## Security

| Tool | Status | Missing | Wave |
|---|---|---|---|
| System Security | 🟡 | UAC level and options, sign-in screen, password policy actions, restrictions (MSI, PowerShell scripts, MMC), components (File History, error reporting, Game Bar, Copilot), firewall and Group Policy backup / restore / reset. Restore points live on System Protection. Automatic sign-in (LSA secret) goes to D. Turning off Defender is not offered: Tamper Protection blocks it on current Windows, so the page links to Windows Security instead | C→D |
| Other Security | 🟡 | 20 per-app permission blocks, Windows Update (auto-update, install mode, restarts, drivers, Store apps), OneDrive off / uninstall, RegBack, Shutdown Event Tracker. Service log-on accounts go to D (Service Manager) | C→D |
| Settings Security | 🟡 | Hide or show-only Settings pages; Control Panel items; shortcuts to settings pages | D |
| Files and Folders (File Security) | 🟡 | Tabs: encrypt, decrypt, shred, permission, unlocker; right action pane; add to context menu | D |
| Privacy Protector | 🟡 | System settings page; sections for URL, Edge, Chrome, folder history (shellbags), executed programs, recent documents, common dialog, jump lists, Prefetch, compatibility cache, feature usage, startup time, USB history; software and network history | D |
| Security Lock | 🟡 | Function limits (block the Registry Editor, Task Manager, cmd, PowerShell, msconfig, WSH, MMC, Defender…); lock system keys; App Locker (allow or lock by name, signature or hash); Image Hijack; Windows features; templates; backup and restore | D, E |
| Device Security | 🟡 | Hide / block drives, block programs by name, removable-storage policies (all, read-only, no read, optical, phones, write-protect, USB driver), AutoPlay, block device installs, clear USB history, autorun protection for USB drives, drive letters (remove / assign). Blocking by hash or signer (Software Restriction Policies) goes to D | C→D |
| File Undelete | 🟡 | Formatted-partition scan; type filter combo; recover with folder structure | D |

## Network

| Tool | Status | Missing | Wave |
|---|---|---|---|
| System Network | 🟡 | Remote Desktop (allow, firewall, port, NLA, idle and disconnected limits), Remote Assistance, network discovery and file sharing, admin shares, anonymous enumeration, LLMNR, time server, interval and sync, Winsock / TCP/IP / network reset, Shared Folders console. A built-in share list goes to D; the browse list (Computer Browser) no longer exists on Windows 10/11 | C→D |
| URL Manager | 🟡 (hosts) | Favorites checker; per-browser URL blocker; cookie protection list | D |
| IP Switcher | 🟡 | Profile combo with New, Save, Delete and Apply; adapter status panel; start with Windows | D |
| Wi-Fi Manager | 🟡 | Priority up and down; connect; details; block or allow lists; backup and restore; last-connected column | D |
| Network Tools | 🟡 | Trace route with 3 round-trip times and location; IP scanner; port scan; geolocation; router access; WLAN report | D |
| Network Monitor | 🟡 | 60-second line chart (received and sent); bandwidth; table of current, maximum, total and average; adapter combo; Resource Monitor link; reset | D |

## Misc. Utilities

| Tool | Status | Missing | Wave |
|---|---|---|---|
| My Task | 🟡 (scheduled actions) | Sections: scheduled task, CPU usage, process usage, process I/O, input idle, network traffic, take rest, USB event, session event. Actions: power, lock, run, reminder, monitor off, restore point, mute. Repeat task; countdown; disable sleep. Live triggers need a background monitor | D |
| Windows Utilities | 🟡 | Sections: shell utilities, Control Panel applets, common utilities, management consoles, other, CMD utilities, Windows apps, known folders, Control Panel, Settings. Create on desktop, add to context menu, pin to taskbar, run as admin, view and sort; Store region; app repair | D |
| File Splitter | 🟡 | Wizard (split or merge); size presets; delete the source; add to context menu | D |
| Super Copy | 🟡 | Schemes; mirror, copy and move modes; thread count; levels; empty folders; directory timestamps; retries; file filters; log recorder; shut down when done | D |
| Registry Tools | 🟡 | Sections: finder (full match, case sensitive, type, scope), jumper, shortcut, permission, editor, export, import | D |
| Download Windows 11 | ❌ | Links to Microsoft's official download options | E |
| Bing Images Downloader | ❌ | Save folder; resolution; set as wallpaper; watermark options; schedule | E |
| Move System Folders | ❌ | List of known folders with current path and size; change location; select action; prevent changes | E |
| Image Hijack, Compatibility Manager, File & Folder Unlocker | ❌ | (described above) | E |

## Not in scope
- **Legacy Internet Explorer data** (`index.dat`, IE-only cleaning): IE is retired on Windows 11.
- **Kernel-level sensors** (voltages, fan speeds, CPU package power): these need a kernel driver, and the common one (WinRing0) is on Microsoft's vulnerable-driver blocklist.
- **Fullscreen frame-rate overlay**: needs injecting code into games.
