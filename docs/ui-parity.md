# UI parity with the original

v0.3.0 compares every window with the original Windows Manager 1.6.3 (hub v2.4.5) and matches its
layout, labels, controls and columns while keeping our dark theme and our own wording for
descriptions (we copy names and structure, never prose or artwork).

How the original is captured: `scripts/capture-original.ps1 -Exe <tool>.exe -Clicks "x,y",…`
(elevated session; PrintWindow on the original's own windows; posted clicks on tabs and
navigation only, never on action buttons). Ours: `scripts/capture.ps1` / `scripts/capture-hub.ps1`.
The original's DotNetBar controls expose almost nothing to UI Automation, so the screenshots
are the reference.

Status: ✅ matched · 🟡 partly · ❌ not yet · ➖ no original window (ours only)

## Hub

| Part | Original | Ours | Status |
|---|---|---|---|
| Header | Logo, name, "for Windows 10/11", version; Update; About with ▾ menu | Same, plus search and the admin shield; Change Journal and Settings in the About ▾ menu | ✅ |
| Rail | 8 categories, OS name / bitness / build at the bottom | Same (Home removed, app opens on Automation) | ✅ |
| Automation | System Protection ▾, 1-Click Kill Processes, 1-Click Fixer, 1-Click Cleaner, Repair Center, Optimization Wizard | Same order | ✅ |
| Information | System Information, Process Manager, Event Viewer, Device Manager, Hardware Monitor | Same | ✅ |
| Optimizer | System Speed, Boot Configuration, Startup Manager, Service Manager, Task Scheduler Manager | Same | ✅ |
| Cleaner | Disk Analyzer, Component Store Cleaner, Smart Uninstaller, Windows Apps Uninstaller, Desktop Cleaner, Junk File Cleaner, Duplicate Files Finder, Registry Cleaner, Registry Defrag | Same | ✅ |
| Customization | Custom System, File Explorer Tweaker ▾, Visual Customizer, JumpList Quick Launcher, Hotkey Manager, Run Shortcut Creator, Context Menu Manager, Start Menu Manager | Same; our extra tools sit in tile dropdowns | ✅ |
| Security | System Security, Other Security, Settings Security, Files and Folders, Privacy Protector, Security Lock, Device Security, File Undelete | Same; Unlocker, Image Hijack, App Locker in dropdowns | ✅ |
| Network | System Network, URL Manager, IP Switcher, Wi-Fi Manager, Network Tools, Network Monitor | Same | ✅ |
| Misc. Utilities | My Task, Windows Utilities, File Splitter, Super Copy, Registry Tools; "Download Windows 11" link | Same; Move System Folders and Compatibility Manager in the Windows Utilities dropdown | ✅ |
| Tile descriptions | Two lines | Two lines, ending in "…" when longer | ✅ |

Tools reachable only from a dropdown: Windows Features (System Protection), App Settings
(Custom System), Navigation Pane / This PC / Win+X (File Explorer Tweaker), Bing Images
(Visual Customizer), Image Hijack (System Security), Unlocker (Files and Folders), App Locker
(Security Lock), Move System Folders and Compatibility Manager (Windows Utilities).

## Tool windows

| Original exe | Our id | Layout | Labels | Missing controls / columns | Missing features | Status |
|---|---|---|---|---|---|---|
| 1-ClickCleaner.exe | oneclickclean | Run list with Result column, Select All + last clean date, Other Options, Task / Start (n) / Exit bar | Item names as the original | Pause (runs aren't pausable); "no toast" option (we show no toasts) | Invalid services, invalid devices, component store and superseded components: admin-only, left for later | 🟡 |
| 1-ClickFixer.exe | fixer | Run list with Result column, collapsible Repair Logs, Select All / Repair Center / Start (n) / Exit | The ten repairs carry the original's names and order; our quick fixes follow | — | — | ✅ |
| 1-ClickKillProcesses.exe | killprocs | Not captured: the original may end processes as soon as it starts | | | | ➖ |
| OptimizationWizard.exe | wizard | "Step (n/N): name" band, icon + explanation, Create Task / Back / Next > / Cancel / Skip; Next opens the step's tool | Only the first step was captured (stepping further was blocked as unsafe) | — | Our wizard keeps extra steps (updates, tweaks, drives, finish) and Back | 🟡 |
| RepairCenter.exe | repair | Left categories, "n Section" tabs, three full-width boxes per tab with the button at the bottom right, section name in the status bar | Section names as the original | — | — | ✅ |
| (hub) System Protection | systemprotection | Not captured: the hub's tiles ignore posted clicks | | | | ➖ |
| SystemInfo.exe | sysinfo | Hardware / Software tree, foldable property groups, Export, date and uptime in the status bar | Page names as the original (we add Printers) | — | Microsoft Product Key shows the firmware key masked until asked | ✅ |
| ProcessManager.exe | procman | Process tree, original toolbar (overflowing into More), Process Logger, "Total Processes" + search | Columns as the original, plus PID | Columns, Options buttons; per-row checkboxes | — | 🟡 |
| EventViewer.exe | events | Grouped by event ID, foldable Query side panel, totals + Export to CSV + search | Columns as the original | — | — | ✅ |
| DeviceManager.exe | devices | Device tree under Windows' class names, original toolbar, Details panel, count + combo + search | As the original | Per-row checkboxes for batch actions | USB Storage History added | 🟡 |
| HardwareMonitor.exe | hwmon | Sensor tree with Current / Maximum, Options / Exit | Group names as the original (Load, Clocks, Temperatures, Data…) | — | Voltages, fan speeds and CPU power need a kernel driver | ✅ |
| (hub) System Speed | speed | Only reachable from the hub's tile, which ignores posted clicks, so it could not be captured | — | — | — | ➖ |
| (hub) Boot Configuration | boot | Only reachable from the hub's tile, which ignores posted clicks, so it could not be captured | — | — | — | ➖ |
| StartupManager.exe | startup | Location tree (General › Startup Folder / Registry › All Users / Current User › Run, Run Once, Run (Wow64); Windows Apps; Other › Advanced Startup, Service Manager, Task Scheduler Manager), list with details below, Total: n | Columns Name, Path, Available, Running, Digital Signature; toolbar Refresh, Start, Disable/Enable, Batch, Add, Delete, Move to, Open File Location, Edit, Query, Settings, Exit | Batch is one action (disable non-Microsoft items), not a menu | Run Once (Wow64) and the "Other" registry nodes: no entries to show here | ✅ |
| ServiceManager.exe | services | Services / Drivers toggle + search, list grouped under Non-Microsoft / Microsoft, details below, "Services Total: n" | Columns Name, Title, Status, Startup Type, Log On As, Creation Date, Command Line; toolbar Optimize, Refresh, File Properties, Open File Location, Query, Start, Stop, Restart, Backup, Restore, Defaults, Delete, Exit | Startup Type ▾ in the toolbar (ours is in the details pane); Backup/Delete are buttons, not menus | — | ✅ |
| TaskSchedulerManager.exe | tasks | One folder/task tree, details below, Total: n + search | Columns Name, Description, Status, Author, Next Run Time, Trigger; toolbar Refresh, Expand/Collapse All, Optimize Task, Create Task, Enable/Disable, Run/End, Delete, Import, Export, Task Scheduler, Exit | — | — | ✅ |
| DiskAnalyzer.exe | diskanalyzer | Folder tree on the left (root "name (Total: size)", folders "size - name" over a share bar, files "size - [n Files]"), pie chart of the selected folder with callout labels and a legend, status "Analysis completed. Target: … (Total: …)" + Add to Context Menu | Toolbar Analyze Drives, Analyze Folder, Rescan Branch, Open Folder, Explore Folder, Unit, Export, Close | The Analyze Drives and Unit drop-downs are pickers in a strip under the toolbar; callouts only when the window is wide enough (the legend always lists every slice); more than 14 folders share an "[Other folders]" slice | Kept: Largest files list (show / recycle) and HTML/CSV/text export | ✅ |
| (hub) Component Store Cleaner | winsxs | Hub-only window in the original; its tiles ignore posted clicks, so it could not be captured | | | | ➖ |
| SmartUninstaller.exe | uninstaller | Large-icon grid grouped "Recently Installed" (names on red) / "Installed Programs", or a Details list; second row Find unavailable program folders, Features, Uninstall OneDrive, Export, search; status "Total installed programs: n; Total size: …" | Toolbar Refresh, Batch Uninstall, Uninstall, Remove Entry, Rename, Details, Query, Hotfix, Windows Apps, Exit | View, Sort and Options drop-downs are pickers in the second row; Hotfix, Windows Apps and unavailable folders open as pages with a back strip instead of new windows; Features opens our Windows Features tool; "Recently Installed" = last 30 days; the size total uses the registry estimates (the original's is larger); no 64-bit count | Kept: Open Folder, Leftovers scan, quiet uninstall | ✅ |
| WindowsAppUninstaller.exe | apps | Large-icon grid (or Details list) filling the window; status "Loading completed | Total: n" with the search box on the right | Toolbar Refresh, Export, Launch, Create on Desktop, Re-Register, Batch Uninstall, Uninstall, Delete, Exit | Filter, View, Sort and Background Color drop-downs are pickers under the toolbar (Background colour = the apps' own tile colours); Delete removes for every account (needs admin) | No total size (the WindowsApps folder can't be read without admin); kept Open Folder | ✅ |
| DesktopCleaner.exe | desktop | One list filling the window (least recently used first); status "Selected: n/N items" with the search box on the right | Columns Name (tick box + icon), User (Current User / All Users), Size, Type, Last Access Time, Hits; toolbar Refresh, Select All, Launch, Delete, Open Folder, Organize, Task, Options, Exit | Organize and Task are plain buttons (organize the ticked items; schedule / unschedule the monthly cleanup) instead of drop-downs; the unused-item actions (Select unused, Move to unused folder, Broken shortcuts) sit in a strip under the toolbar; Options opens a side pane | Kept: Status column (broken / unused) | ✅ |
| JunkFileCleaner.exe | junk | Left pane "Selected Drives: C:\;D:\" with a free/used pie and legend, then the Drive | Free / Total list with tick boxes; the found files in one ticked list; status line + search box | Columns Files, Type (plus Size); toolbar Scan, Select All, Delete, Properties, Open Folder, Options, Add to Skip List, Exit | Our junk types (rules) are ticked in the left pane under the drives instead of in the Options dialog; Options opens a window (skip list, system files, Recycle Bin); the pane does not collapse | | ✅ |
| DuplicateFilesFinder.exe | duplicates | Collapsible "Select Drives" panel: Drive | Free / Total (Free%) list on the left, "Search only the specified folders" with the folder list and +/− on the right; results below grouped by duplicate set | Columns Name, Path, Size (plus a group header row); toolbar Scan, Selection, Expand All, Open Folder, Properties, Launch, Rename, Delete, Options, Exit | Selection toggles a strip of marking presets instead of a drop-down; the switch is a check box; Options opens the match / size / type / attribute criteria in a window; the date column is Date Modified (the original shows Creation Date) | Kept: Move marked to… | ✅ |
| RegistryCleaner.exe | regclean | Left "Scan Items" tree (Registry Entries > items with tick boxes), found entries on the right, status line + search box; no scan until Scan Registry | Columns Main Key, Sub Key, Value Name, Value; toolbar Scan Registry, Clean Registry, Select All, Open in Registry, Add to Exclusions, Registry Tools, Options, Exit | Our scan items are grouped (Programs, Startup, File types…) under the root; Options is a window that also holds the .reg import | Kept: backup choices and restore point in Options | ✅ |
| RegistryDefrag.exe | regdefrag | Dialog layout: heading band "Rebuild and defragment registry to fix errors and corruption", the hive list, Analyze / Compact / Cancel above the status line | Columns Registry Hives, Current Size, After Compacted, Rate (%), Result | Hives show friendly names (the file path on hover) instead of \REGISTRY\… paths; tick boxes choose which hives to compact | Kept: Delete backups | ✅ |
| (hub) Custom System | customsystem | Hub-only window in the original; could not be captured | | | | ➖ |
| (hub) File Explorer Tweaker | explorertweaks | Hub-only window in the original; could not be captured | | | | ➖ |
| NavigationPaneManager.exe | navpane | "Navigation pane" tree in pane order, action column on the right, Edit group (Title, Path) at the bottom, status line | Right column: Place on Top, Add Folder, Up, Down, Disable, Enable, Remove, This PC, Options | Add Utility is missing; the tree lists the items we can manage (This PC, Network and removable drives are not listed); Options opens our tweaks as a page with a back strip | Kept: Move to Top, Icon field, Open, Refresh, Restart Explorer | 🟡 |
| ThisPCManager.exe | thispc | List of what This PC shows, action column on the right, Edit group (Title, Parameter, Path) at the bottom, status line | Right column: Add File (+ Parameter box), Add Folder, Add Utility, Desktop Icon Settings, Enable, Remove, Remove All, This PC | Add Windows App is missing; Add Utility opens a list of Windows tools inside the column instead of a dialog | Kept: Personal Folders page, Icon field, Open, Refresh, Restart Explorer | 🟡 |
| WinXMenuEditor.exe | winx | The original exits at once without a window on this Windows build (26300), so it could not be captured | | | | ➖ |
| VisualCustomizer.exe | visual | Heading band "Customize Windows visual appearance and interface styles", section list on the left, page on the right with Restore / Save at the bottom | Sections System Settings, Font & Size, System Icon, Other Icon, File Type Icon, Folder Colorizer, Theme Changer, Lock Screen Extractor, Bing Images Downloader | System Settings groups its switches in a second list (Theme, Taskbar, Windows) instead of numbered Properties tabs; the footer says Restore defaults | | 🟡 |
| BingImagesDownloader.exe | bing | | | | | ❌ |
| JumpListQuickLauncher.exe | jumplist | Top row: list picker, Create, Delete, Backup, Restore, Exit; Title tree of groups and items; action column on the right; Edit group (Title, Parameter, Path) with Save; status "Name | Count" + Maximum allowable number | Right column: Add Group, Add File, Add Folder, Add URL, Add Start Menu, Add Utility, Add Windows app, Up, Down, Remove, Remove All | New list and group names are typed into fields (the original asks in dialogs); Add URL, Start Menu, Utility and Windows app open our add dialog | | ✅ |
| HotkeyManager.exe | hotkeys | List on the left, action column on the right, Edit group at the bottom, status line | Columns Title, Hotkey, Status (plus Where); right column Add File, Add Folder, Add URL, Add Start Menu, Add Utility, Add Windows App, Enable, Disable, Remove, Remove All, Backup, Restore | Options is replaced by Refresh (we have no hotkey options); the Edit group changes the selected hotkey (Ctrl/Alt/Shift + key) instead of two text fields; new hotkeys are set in our dialog | | ✅ |
| RunShortcutCreator.exe | runshortcuts | Heading band "Edits shortcuts that executed on the Run dialog box", ticked list, bottom bar Select All + Search, Refresh, Backup ▾, Add, Delete, Execute, Exit | Columns Name, Path, Type (System / User), Available (Yes / Deletable in red; broken names start ticked) | Names with no path show "No path" and are not ticked (the original calls them Deletable); no program icons in the Name column; Add opens our New Run name dialog | Kept: status line with counts | ✅ |
| ContextMenuManager.exe | contextmenu | | | | | ❌ |
| StartMenuManager.exe | startmenu | Ticked list of Start menu shortcuts (folders fold open), action column on the right with the search box, status "Total n" | Columns Name, User (All Users / Current User), Available ("No" in red; those start ticked); actions Rename, Remove, Open Folder, Launch | Pin to Taskbar is left out (Windows only allows pinning from Start itself); top-level shortcuts come before folders | Kept: Add Program, Add to All apps, Tick unavailable, Refresh; Taskbar and Start Backup and Start Options open as pages | ✅ |
| (hub) System Security | systemsecurity | | | | | ❌ |
| (hub) Other Security | othersecurity | | | | | ❌ |
| SettingsSecurity.exe | lockdown | | | | | ❌ |
| FileSecurity.exe | filesecurity, unlocker | | | | | ❌ |
| PrivacyProtector.exe | privacy | | | | | ❌ |
| SecurityLock.exe | securitylock, applocker | | | | | ❌ |
| (hub) Device Security | devicesecurity | | | | | ❌ |
| FileUndelete.exe | undelete | | | | | ❌ |
| (hub) System Network | systemnetwork | | | | | ❌ |
| URLManager.exe | hosts | | | | | ❌ |
| IPSwitcher.exe | ipswitcher | | | | | ❌ |
| WiFiManager.exe | wifi | | | | | ❌ |
| NetworkTools.exe | nettools | | | | | ❌ |
| NetworkMonitor.exe | netmonitor | | | | | ❌ |
| MyTask.exe | mytask | | | | | ❌ |
| WindowsUtilities.exe | utilities | Left category list (Shell Utilities, CP Applets, Common Utilities, Management Console, Other Utilities, CMD Utilities, Windows Apps, Known Folders, Control Panel, Settings, Settings Security) with a search box; large-icon grid grouped by headings; toolbar Create to Desktop, Add to Context Menu, Run as Administrator, Pin to Taskbar, Launch, View, Sort, Exit | | | | ❌ |
| FileSplitter.exe | splitter | | | | | ❌ |
| SuperCopy.exe | supercopy | | | | | ❌ |
| RegistryTools.exe | regtools | | | | | ❌ |
| MoveSystemFolders.exe | movefolders | | | | | ❌ |
| (hub) Download Windows 11 | downloadwin | | | | | ❌ |
| — | imagehijack, winfeatures, compat, appsettings | ours only | | | | ➖ |
