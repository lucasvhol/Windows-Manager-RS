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
| Header band with app name, version, Update and About | ❌ | B |
| Left category column with large icons; selected item highlighted with a notch | 🟡 (plain sidebar) | B |
| Two-column grid of tool tiles (icon, title, one-line description); split tiles with a dropdown | ❌ | B |
| Each tool opens in its own window; several can be open at once | ❌ | B |
| Tool window shell: description band with Help, icon-over-label toolbar, left section list or tree, numbered page tabs, footer buttons, status bar with count and search | ❌ | B |
| Tweak pages: staged changes with Save, plus Restore to Windows defaults | ❌ (changes apply instantly) | B |
| OS edition and build shown in the hub | ❌ | B |
| Settings: program password, minimise to tray, UI scale and font, list row height, language, light/dark | 🟡 (language, theme) | B |
| Settings: save all applied tweaks to a file and restore them later (e.g. after a reinstall) | ❌ | B |
| Settings: reset a tool's saved settings; back up and restore program settings | ❌ | B |
| Frequently used tools list | ❌ | B |
| Update check (own releases) | ❌ | E |
| Licensing, registration, trial countdown | ⛔ the original's business model, not a feature | — |

## Automation

| Tool | Status | Missing | Wave |
|---|---|---|---|
| System Protection | 🟡 (restore point in Settings) | Page listing restore points, with delete; System Protection on/off per drive; automatic registry backup (RegBack); recovery links | C |
| 1-Click Kill Processes | ❌ | Escalating kill tiers (standard, enhanced, maximum); protected list; log | E |
| 1-Click Fixer | 🟡 | 10 checked items in one list: component store health, system files, WMI rebuild, system partition scan, all-volume health, internet connection, file associations, folder permissions, registry permissions, Explorer/shell. Select All, a link to Repair Center, Start, and a collapsible log | D |
| 1-Click Cleaner | 🟡 | Choose the items (privacy, junk, registry, registry defrag); silent mode; shut down when done; last-clean date | D |
| Repair Center | 🟡 (SFC, DISM, CHKDSK) | Sections: system repair, misc repair, Windows apps, event viewer, unlock system, security lock, file associations, context menu, icon repair, network problems, troubleshooting, system reset. Numbered pages of described repair boxes; repair logs | D |
| Optimization Wizard | 🟡 (tweak goals) | 12-step guided sequence that launches each tool in turn (Process Manager, Windows Update, uninstaller, startup, tasks, services, junk, registry cleaner, registry defrag, defrag unless SSD); environment detection (SSD, touch, DPI); create a task | D |

## Information

| Tool | Status | Missing | Wave |
|---|---|---|---|
| System Information | 🟡 | Hardware/software tree; product key; codecs; printers; user accounts; environment; event log; HDD SMART report; disk benchmark; hardware score; hotfix list and uninstall; system report export; date and uptime status bar | D |
| Process Manager | 🟡 | Priority, suspend, restart, terminate variants (unsigned, untrusted, high CPU, high memory, kill and block); block list; details; open folder; online lookup; Window Finder; column chooser; view and filter; company and signature columns; lower pane with window titles, threads and modules; process logger | D |
| Event Viewer | 🟡 | Query side panel with time range (last N days or custom); grouping by event ID; export to CSV | D |
| Device Manager | 🟡 | Hidden and non-present devices; uninstall checked or all invalid; driver backup and restore; scan for hardware changes; USB storage history; details pane; filter combo | D |
| Hardware Monitor | 🟡 | Sensor tree (current and maximum); GPU via vendor libraries; SMART temperatures; battery; temperature alarm. Voltages, fans and CPU power need a kernel driver | D, ⛔ partly |

## Optimizer

| Tool | Status | Missing | Wave |
|---|---|---|---|
| System Speed | 🟡 (11 tweaks) | NTFS paged pool, Fast Startup, Group Policy wait, crash logging, Prefetcher, boot defrag, Last Known Good, disk-check cancel and countdown, process and service wait times, hibernate, power plan tweaker and restore, Delivery Optimization, SSD set, Automatic Maintenance | C |
| Boot Configuration | 🟡 | Add entry; boot order; back up and restore boot data; Safe Mode with Command Prompt; disable driver signing; restart into advanced startup | D |
| Startup Manager | 🟡 | Location tree (startup folders and Run keys per scope and bitness, Windows apps, advanced locations); move between locations; edit; online lookup; batch actions; signature and running columns; details pane; block all auto-start | D |
| Service Manager | 🟡 | Drivers tab; Non-Microsoft grouping; log-on and command-line columns; file properties; delete (also the file); back up and restore startup types; Optimize questionnaire; restore defaults | D |
| Task Scheduler Manager | 🟡 | Tree by folder; create task with all trigger types; Optimize Task questionnaire; description, author and trigger columns | D |

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
| Custom System | ❌ | Start/tile, desktop, taskbar, thumbnails, notification area, theme schedule, title-bar colours, fonts, lock screen, keys, feature-usage reset (100+ options) | C |
| File Explorer Tweaker | 🟡 (10 tweaks) | Navigation, launch location, folder views, thumbnails and borders, drag action, naming templates, Ribbon commands, open limit, window states. Split tile: Navigation Pane, This PC, Drag File Action | C |
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
| System Security | ❌ | Restore-point list; Group Policy and Firewall backup, restore and reset; restrictions; component switches; UAC; sign-in screen; password policy; automatic sign-in | C |
| Other Security | ❌ | App access blocks; RegBack; Shutdown Event Tracker; OneDrive; Windows Update; service accounts | C |
| Settings Security | 🟡 | Hide or show-only Settings pages; Control Panel items; shortcuts to settings pages | D |
| Files and Folders (File Security) | 🟡 | Tabs: encrypt, decrypt, shred, permission, unlocker; right action pane; add to context menu | D |
| Privacy Protector | 🟡 | System settings page; sections for URL, Edge, Chrome, folder history (shellbags), executed programs, recent documents, common dialog, jump lists, Prefetch, compatibility cache, feature usage, startup time, USB history; software and network history | D |
| Security Lock | 🟡 | Function limits (block the Registry Editor, Task Manager, cmd, PowerShell, msconfig, WSH, MMC, Defender…); lock system keys; App Locker (allow or lock by name, signature or hash); Image Hijack; Windows features; templates; backup and restore | D, E |
| Device Security | ❌ | Hide drives; disk immunity; write protection; removable storage policy; block device installs; USB history; restrict programs; mount and unmount | C |
| File Undelete | 🟡 | Formatted-partition scan; type filter combo; recover with folder structure | D |

## Network

| Tool | Status | Missing | Wave |
|---|---|---|---|
| System Network | ❌ | Browse list and discovery; time sync interval; Remote Desktop (allow, port, limits); Remote Assistance; share manager; admin shares; anonymous enumeration; network reset | C |
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
