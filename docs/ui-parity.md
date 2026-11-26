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
| 1-ClickCleaner.exe | oneclickclean | | | | | ❌ |
| 1-ClickFixer.exe | fixer | | | | | ❌ |
| 1-ClickKillProcesses.exe | killprocs | | | | | ❌ |
| OptimizationWizard.exe | wizard | | | | | ❌ |
| RepairCenter.exe | repair | | | | | ❌ |
| (hub) System Protection | systemprotection | | | | | ❌ |
| SystemInfo.exe | sysinfo | | | | | ❌ |
| ProcessManager.exe | procman | | | | | ❌ |
| EventViewer.exe | events | | | | | ❌ |
| DeviceManager.exe | devices | | | | | ❌ |
| HardwareMonitor.exe | hwmon | | | | | ❌ |
| (hub) System Speed | speed | | | | | ❌ |
| (hub) Boot Configuration | boot | | | | | ❌ |
| StartupManager.exe | startup | | | | | ❌ |
| ServiceManager.exe | services | | | | | ❌ |
| TaskSchedulerManager.exe | tasks | | | | | ❌ |
| DiskAnalyzer.exe | diskanalyzer | | | | | ❌ |
| (hub) Component Store Cleaner | winsxs | | | | | ❌ |
| SmartUninstaller.exe | uninstaller | | | | | ❌ |
| WindowsAppUninstaller.exe | apps | | | | | ❌ |
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
