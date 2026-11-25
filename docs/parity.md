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
| 1-Click Fixer | ✅ | 10 repairs: component store (DISM), system files (SFC), WMI verify/salvage, system drive and other NTFS drives (CHKDSK /scan), internet (DNS, WinHTTP proxy, renew, lookup test), program file associations, personal folder permissions, registry permissions (unreadable HKCU keys), Windows shell (Winlogon Shell/Userinit); plus 9 quick fixes. Select all per group, link to Repair Center, Start with progress, collapsible log; setting changes journaled with undo (reg exports, saved SDDL) | D |
| 1-Click Cleaner | ✅ (registry defrag joins the item list with the Registry Defrag tool, E) | Remembered item choice: junk files, dead registry entries, five privacy traces (each, or all at once); counts after the scan; silent mode (scan and clean in one click, no review); shut down when done (60 s, cancellable); last-clean date and summary; scheduled `oneclick-clean` action in My Task uses the saved choice (never shuts down) | D |
| Repair Center | ✅ | 12 sections of described repair boxes on numbered pages (6 per page): system repair (DISM, SFC, WMI, CHKDSK, Windows Installer, memory test), misc (Explorer, clock, print queue, Windows Update, search index, font cache), Windows apps (Store reset and re-register, Start and taskbar, all apps), event viewer (save and clear logs, event log service), unlock system (11 blocking policies, each or all), security settings (UAC, Defender, firewall, SmartScreen, notifications, update policies), file associations, context menu (classic menu, blocked shell extensions), icon repair (system icon overrides, drive icons), network problems (firewall rules, hosts file), troubleshooting and system reset pages; status line on boxes that can tell whether they are needed; admin-only boxes greyed; changes journaled with undo; a log file per run with a Repair logs section | D |
| Optimization Wizard | ✅ (registry defrag step joins with the Registry Defrag tool, E) | 13 guided steps with a step list (done, skipped, opened): before you start, Process Manager, Windows Update, uninstaller, Windows apps, startup, scheduled tasks, services, junk, registry cleaner, Windows settings by goal (balanced, privacy, performance), drives (Optimize Drives for hard disks only), finish; detects SSDs per drive, touch screens and display scaling and adapts its advice (no defrag on SSDs, animations kept on touch screens); restore point; finish schedules a weekly 1-Click Clean task or opens My Task | D |

## Information

| Tool | Status | Missing | Wave |
|---|---|---|---|
| System Information | ✅ | Summary (OS, CPU, memory, board, graphics, storage, network, public IP on request) with save/copy; licence, activation, firmware key (hidden until shown) and Experience Index scores with a re-run; user accounts; installed updates with uninstall; printers; codecs; environment; disk health (SMART); disk speed test (sequential and 4K, unbuffered); battery, energy, sleep-study and performance reports | D |
| Process Manager | ✅ | User, company and signature columns (catalog-aware); end, end tree, suspend, resume, restart; priority; block / unblock through Image File Execution Options; end all unsigned, untrusted, high-CPU or high-memory processes (critical ones excluded); views (mine, not Microsoft, unsigned); window list to find a window's process (instead of a drag crosshair); details with command line, start time, parent, threads and loaded modules; open folder; online lookup | D |
| Event Viewer | ✅ | Time range (hour, day, week, month) in the query itself; level filters; group by source and event ID with counts; CSV export | D |
| Device Manager | ✅ | Disconnected devices shown; uninstall a device or every disconnected one; scan for hardware changes; driver backup (export) and restore (install from folder); USB history clearing on Device Security | D |
| Hardware Monitor | ✅ (motherboard voltages, fans and CPU package power ⛔: they need a kernel driver) | Charts (CPU with live clock, memory, network, disk); per-core load; sensor table with current, minimum, average and maximum, click a sensor to chart it, reset; log every reading to a CSV file; NVIDIA GPU load, memory, temperature, power, fan and clock (NVML); other graphics adapters (AMD, Intel) through Windows: temperature, fan speed, power draw, load and memory, where the driver reports them; SMART disk temperatures (administrator); battery; temperature alarm with sound | D |

## Optimizer

| Tool | Status | Missing | Wave |
|---|---|---|---|
| System Speed | ✅ (35 settings) | NTFS cache, kernel paging, Prefetcher, power throttling, Fast Startup, hibernation, power plans (Ultimate Performance, restore defaults), Group Policy network wait, disk-check countdown, verbose status, Last Known Good, boot file placement, crash dump type and options, TRIM, Automatic Maintenance, Delivery Optimization, GPU scheduling, game recording. Power-plan editor goes to D | C |
| Boot Configuration | ✅ | Timeout, default entry, boot order; add (copy of running Windows), rename, delete (store re-import undo); Safe Mode / with networking / with Command Prompt; classic menu, no GUI boot, boot log, base video; test signing and disabled driver signature checks behind warnings; back up / restore the boot store; restart into Advanced startup or UEFI settings. Live bcdedit path awaits the elevated test pass | D |
| Startup Manager | ✅ | Run/RunOnce keys (per user and machine), Startup folders and Windows apps with Task Manager-compatible enable flags; move, edit, delete (journaled), open location, online lookup, batch "disable others"; company, signature and running columns; advanced auto-start locations (read-only); block Run keys per scope | D |
| Service Manager | ✅ | Services and Drivers tabs; Non-Microsoft filter (catalog-aware signatures, svchost ServiceDll resolved); log-on, company and command-line details; file properties, open folder, online lookup; delete third-party services (registry backed up, optional file to Recycle Bin); back up / restore startup types (JSON, plus a first-run snapshot); restore Windows defaults for common services; Optimize questionnaire. Drivers are read-only | D |
| Task Scheduler Manager | ✅ | Folder tree with counts; trigger, next/last run, last result, author and description columns; create tasks (one time, daily, weekly, monthly, log on, startup, idle, event, lock/unlock; as SYSTEM / highest privileges) through locale-independent XML; import/export task XML; run, end, enable, disable, delete with XML backup; Optimize questionnaire for optional Windows tasks; open Task Scheduler | D |

## Cleaner

| Tool | Status | Missing | Wave |
|---|---|---|---|
| Disk Analyzer | ✅ | Drive picker with type and free space, or any folder; treemap and folder list; largest files (show, recycle); rescan one branch; explore; units (automatic, KB, MB, GB); export as HTML, CSV (every folder) or text tree; "Analyze with Win Manager" on folders and drives in Explorer (per user, journaled) | D |
| Component Store Cleaner | ✅ | Analyze; remove superseded components (optional /ResetBase); quick check, full scan and repair (optionally without Windows Update) with live progress and a verdict. Live DISM runs await the elevated test pass | D |
| Smart Uninstaller | ✅ | Batch uninstall with optional quiet mode and an automatic leftover scan; remove entry and rename (journaled); details pane with all values and Registry Editor link; sortable columns; list or icon view; online lookup; installed updates with uninstall; Windows apps; Program Files folders nothing installed refers to (portable programs flagged); uninstall OneDrive; HTML/CSV export | D |
| Windows Apps Uninstaller | ✅ | List or tile view with app logos and tile background colours; filters (Microsoft, other publishers, built in, installed later); batch removal, also for all users and new users (administrator); launch; desktop shortcut; repair by re-registering; open folder; CSV export. Removal for all users awaits the elevated test pass | D |
| Desktop Cleaner | ✅ | Size, owner, last used and launch-count columns (Explorer usage records; folders include what was run inside them); select items unused for N days; move them to a desktop folder or organize into a dated folder by type; broken shortcuts to the Recycle Bin; saved options; monthly scheduled cleanup (`--run desktop-unused`) | D |
| Junk File Cleaner | ✅ | Drive selection; broken shortcuts; invalid Start Menu items; empty Program Files folders; orphaned installer packages; empty folders and zero-byte files in user data (programs, games and source trees excluded; review list); disc-burning staging, ETL traces, servicing logs, Windows Update backups; saved skip list (folders or wildcards); leave system files alone; remembered selection. Administrator-only scans await the elevated test pass | D |
| Duplicate Files Finder | ✅ | Folders or whole drives with usage bars (system, program and game folders skipped); "only these folders"; match by content, name and content, name and size, or name; size range; type presets and include/exclude patterns; skip hidden, system or read-only; selection presets (oldest, newest, shortest/longest path, keep in folder, unmark, invert); recycle, move to folder, rename (journaled); remembered criteria | D |
| Registry Cleaner | ✅ | 22 categories in a grouped tree with per-category scan switches, counts and item lists; exclusion list (keys, values, wildcards; Ignore button); backup choices (journal, optional combined .reg file, restore point); import .reg with undoable backup. COM, type library and firewall categories are opt-in; Windows' and Store apps' own registrations are never touched | D |
| Registry Defrag | ❌ | Hive list with current and compacted size, rate and result; Analyze, then Compact at reboot | E |

## Customization

| Tool | Status | Missing | Wave |
|---|---|---|---|
| Custom System | 🟡 (34 settings) | Desktop icons and version, shortcut arrow, wallpaper quality, taskbar (combine, all displays, last window, preview delay, animations, Widgets, search), Start, snapping and Alt+Tab, notifications, accent colour, lock and sign-in screen, Num Lock, Windows keys, accessibility prompts, usage and icon-cache resets. Still missing: theme schedule (needs a scheduled task, D), Caps Lock remap (binary Scancode Map, D), fonts (Visual Customizer, D) | C→D |
| Windows App Settings | ✅ | Where desktop apps may be installed from, suggested-app reinstalls, background Store apps, welcome and finish-setup prompts, shortcuts to Windows' app pages | C |
| File Explorer Tweaker | 🟡 (29 settings) | Launch location, folder windows, status bar, info tips, name case, open limit, thumbnails and overlays, preview pane, label shadow, compressed colour, drives, sync ads, Sharing Wizard, merge prompt, folder-view memory and reset; Navigation Pane and This PC split tiles. Still missing: drag action and naming templates (per-file-type / undocumented values, D); Ribbon commands do not exist on Windows 11 24H2+ | C→D |
| Visual Customizer | 🟡 (5 of 9 sections) | Done: system settings (appearance switches); font and size (font, size and bold for title bars, tool windows, menus, message boxes, status bars, icon labels; title bar, menu, scroll bar and border sizes; applied live with Windows defaults and undo; text size; replace the system font); system icons (desktop items, per user); other icons (stock folder, drive and overlay icons); file-type icons (search 900 types, change and restore, per user). Still missing: folder colorizer, theme changer, lock-screen image saver, Bing images | D |
| JumpList Quick Launcher | ✅ | Several named lists (main list on the app, others via Start launchers with their own app ID); groups as categories, or under Tasks with separators when recent items are off; files, folders, URLs, Start items, utilities and Store apps; edit, reorder; maximum count; backup and restore | D |
| Hotkey Manager | ✅ | Targets: program, file, folder, web address, Start item, Windows tool, Store app; change, turn off/on (remembered); backup and restore; conflicts between shortcuts and with hotkeys other programs hold; "end not-responding programs" hotkey | D |
| Run Shortcut Creator | ✅ | Type and availability columns (missing in red); program, file, folder and web address targets; arguments; window mode; all-users scope for plain programs; collision check; run; remove; backup and restore | D |
| Context Menu Manager | ✅ | 14 sections (10 shell locations, any file type, Open with, New menu, Send To); type, Shift, position, company, signature and scope columns; properties; open in registry; Shift/position switches; delete with backup; editor with 41 built-in commands or any program (elevated, Shift-only, position, per user or all users); repair (re-enable, remove broken); classic Windows 11 menu switch | D |
| Start Menu Manager | ✅ | Folder tree with owner, availability and target columns; add; Pin Any (files, folders, web addresses as All apps entries); launch; rename; remove; remove broken; back up and restore taskbar pins and Start layout (undoable). Pinning on behalf of the user is ⛔: Windows blocks it for other programs. Live restore awaits the manual test pass | D |
| Win+X Menu Editor | ✅ | Hide/show; add a program, file, folder, Start item or one of 22 Windows tools (arguments, run as administrator); rename; move up/down and between groups; add/remove groups; restore defaults. Entries carry the WinX hash (verified against Windows' own shortcuts) | D |
| Navigation Pane Manager | ✅ | Add folders (name, target, icon) as per-user shell folders; show or hide Windows items (Home, Gallery, Libraries, Control Panel, Recycle Bin, user folder) and other programs' items; order, place on top; edit pane; open; restart Explorer; behaviour options | D |
| This PC Manager | ✅ | Add folders, files and programs (with arguments) as per-user shell items; 12 Windows tools (Control Panel, All Tasks, Recycle Bin, Network…); rename, icon, turn off/on, remove, open; Desktop Icon Settings; personal folders tab; restart Explorer | D |

## Security

| Tool | Status | Missing | Wave |
|---|---|---|---|
| System Security | 🟡 | UAC level and options, sign-in screen, password policy actions, restrictions (MSI, PowerShell scripts, MMC), components (File History, error reporting, Game Bar, Copilot), firewall and Group Policy backup / restore / reset. Restore points live on System Protection. Automatic sign-in (LSA secret) goes to D. Turning off Defender is not offered: Tamper Protection blocks it on current Windows, so the page links to Windows Security instead | C→D |
| Other Security | 🟡 | 20 per-app permission blocks, Windows Update (auto-update, install mode, restarts, drivers, Store apps), OneDrive off / uninstall, RegBack, Shutdown Event Tracker. Service log-on accounts go to D (Service Manager) | C→D |
| Settings Security | ✅ | Restrictions tab; Settings pages hidden or show-only (SettingsPageVisibility, 76 pages with sections, filter, open; administrator); Control Panel items hidden or show-only (DisallowCpl / RestrictCpl from the live Control Panel namespace; needs administrator unless the account's policy key already exists); shortcuts to any Settings page on the desktop or in Start | D |
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
