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
| (hub) Component Store Cleaner | winsxs | | | | | ❌ |
| SmartUninstaller.exe | uninstaller | Large-icon grid grouped "Recently Installed" (names on red) / "Installed Programs", or a Details list; second row Find unavailable program folders, Features, Uninstall OneDrive, Export, search; status "Total installed programs: n; Total size: …" | Toolbar Refresh, Batch Uninstall, Uninstall, Remove Entry, Rename, Details, Query, Hotfix, Windows Apps, Exit | View, Sort and Options drop-downs are pickers in the second row; Hotfix, Windows Apps and unavailable folders open as pages with a back strip instead of new windows; Features opens our Windows Features tool; "Recently Installed" = last 30 days; the size total uses the registry estimates (the original's is larger); no 64-bit count | Kept: Open Folder, Leftovers scan, quiet uninstall | ✅ |
| WindowsAppUninstaller.exe | apps | Large-icon grid (or Details list) filling the window; status "Loading completed | Total: n" with the search box on the right | Toolbar Refresh, Export, Launch, Create on Desktop, Re-Register, Batch Uninstall, Uninstall, Delete, Exit | Filter, View, Sort and Background Color drop-downs are pickers under the toolbar (Background colour = the apps' own tile colours); Delete removes for every account (needs admin) | No total size (the WindowsApps folder can't be read without admin); kept Open Folder | ✅ |
| DesktopCleaner.exe | desktop | | | | | ❌ |
| JunkFileCleaner.exe | junk | | | | | ❌ |
| DuplicateFilesFinder.exe | duplicates | | | | | ❌ |
| RegistryCleaner.exe | regclean | | | | | ❌ |
| RegistryDefrag.exe | regdefrag | | | | | ❌ |
| (hub) Custom System | customsystem | | | | | ❌ |
| (hub) File Explorer Tweaker | explorertweaks | | | | | ❌ |
| NavigationPaneManager.exe | navpane | | | | | ❌ |
| ThisPCManager.exe | thispc | | | | | ❌ |
| WinXMenuEditor.exe | winx | | | | | ❌ |
| VisualCustomizer.exe | visual | | | | | ❌ |
| BingImagesDownloader.exe | bing | | | | | ❌ |
| JumpListQuickLauncher.exe | jumplist | | | | | ❌ |
| HotkeyManager.exe | hotkeys | | | | | ❌ |
| RunShortcutCreator.exe | runshortcuts | | | | | ❌ |
| ContextMenuManager.exe | contextmenu | | | | | ❌ |
| StartMenuManager.exe | startmenu | | | | | ❌ |
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
