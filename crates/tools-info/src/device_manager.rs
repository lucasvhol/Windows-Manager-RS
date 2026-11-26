//! Device Manager: list present devices grouped by class, show problems,
//! enable / disable / restart devices (via `pnputil`, journaled with the reverse command).

use std::collections::BTreeMap;

use anyhow::{Result, bail};
use windows::Win32::Devices::DeviceAndDriverInstallation::HDEVINFO;
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_DEVNODE_STATUS_FLAGS, CM_Get_DevNode_Status, CM_PROB, CM_PROB_DISABLED, CR_SUCCESS, DIGCF_ALLCLASSES,
    DIGCF_PRESENT, DN_HAS_PROBLEM, SETUP_DI_REGISTRY_PROPERTY, SP_DEVINFO_DATA, SPDRP_CLASS,
    SPDRP_DEVICEDESC, SPDRP_FRIENDLYNAME, SPDRP_MFG, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInfo,
    SetupDiGetClassDescriptionW, SetupDiGetClassDevsW, SetupDiGetDeviceInstanceIdW,
    SetupDiGetDeviceRegistryPropertyW,
};
use windows::core::PCWSTR;
use wm_core::{
    AppCtx, Category, Layout, Tool, egui, icons,
    process::hidden_command,
    safety::UndoAction,
    task::BackgroundTask,
    ui::{Confirm, Message, ToolButton, frame, show_message},
};

#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub instance_id: String,
    pub name: String,
    /// The class as Device Manager shows it ("Display adapters").
    pub class: String,
    /// The class's short name ("Display").
    pub class_id: String,
    pub manufacturer: String,
    /// Config Manager problem code, if the device has a problem.
    pub problem: Option<u32>,
    /// False for devices that were connected before but are not now.
    pub present: bool,
}

impl Device {
    pub fn is_disabled(&self) -> bool {
        self.problem == Some(CM_PROB_DISABLED.0)
    }
}

/// Human-readable text for common Config Manager problem codes.
pub fn problem_text(code: u32) -> &'static str {
    match code {
        1 => "The device is not configured correctly.",
        3 => "The driver may be corrupted, or the system is low on memory.",
        10 => "The device cannot start.",
        12 => "Not enough free resources for this device.",
        14 => "The device needs a restart to work properly.",
        18 => "Reinstall the drivers for this device.",
        19 => "Configuration information in the registry is incomplete or damaged.",
        21 => "Windows is removing this device.",
        22 => "The device is disabled.",
        24 => "The device is not present, not working, or missing drivers.",
        28 => "The drivers for this device are not installed.",
        29 => "The device is disabled by the firmware.",
        31 => "Windows cannot load the drivers required for this device.",
        32 => "The driver service for this device is disabled.",
        37 => "The device driver failed to initialise.",
        39 => "The driver is corrupted or missing.",
        43 => "Windows stopped this device because it reported problems.",
        45 => "The device is not connected to the computer.",
        47 => "The device was prepared for safe removal.",
        48 => "The driver software was blocked because of known issues.",
        52 => "Windows cannot verify the driver's digital signature.",
        _ => "The device has a problem.",
    }
}

/// Owns the device info set and destroys it on drop.
struct DevInfo(HDEVINFO);

impl Drop for DevInfo {
    fn drop(&mut self) {
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

fn reg_property(set: HDEVINFO, data: &SP_DEVINFO_DATA, prop: SETUP_DI_REGISTRY_PROPERTY) -> Option<String> {
    let mut buf = vec![0u8; 1024];
    let mut required = 0u32;
    unsafe {
        if SetupDiGetDeviceRegistryPropertyW(set, data, prop, None, Some(&mut buf), Some(&mut required))
            .is_err()
        {
            if required as usize <= buf.len() {
                return None;
            }
            buf.resize(required as usize, 0);
            SetupDiGetDeviceRegistryPropertyW(set, data, prop, None, Some(&mut buf), None).ok()?;
        }
    }
    let wide: Vec<u16> = buf
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    let s = String::from_utf16_lossy(&wide[..end]).trim().to_owned();
    (!s.is_empty()).then_some(s)
}

fn instance_id(set: HDEVINFO, data: &SP_DEVINFO_DATA) -> Option<String> {
    let mut buf = vec![0u16; 512];
    let mut required = 0u32;
    unsafe { SetupDiGetDeviceInstanceIdW(set, data, Some(&mut buf), Some(&mut required)).ok()? };
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

/// The class's name as Device Manager shows it ("Display adapters").
fn class_description(guid: &windows::core::GUID) -> Option<String> {
    if *guid == windows::core::GUID::zeroed() {
        return None;
    }
    let mut buf = [0u16; 256];
    unsafe { SetupDiGetClassDescriptionW(guid, &mut buf, None) }.ok()?;
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end])).filter(|s| !s.trim().is_empty())
}

/// USB storage devices this PC has seen, newest key order first: (name, serial).
/// Read from the registry, which every account can read.
pub fn usb_storage_history() -> Vec<(String, String)> {
    let root = r"HKLM\SYSTEM\CurrentControlSet\Enum\USBSTOR";
    let Ok(k) = wm_core::registry::open_read(root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for model in k.enum_keys().flatten() {
        let Ok(mk) = k.open_subkey(&model) else { continue };
        for serial in mk.enum_keys().flatten() {
            let key = format!(r"{root}\{model}\{serial}");
            let name = wm_core::registry::read_raw(&key, "FriendlyName")
                .map(|v| wm_core::registry::decode_string(&v))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| model.replace(['&', '_'], " "));
            // The part before "&" is the device's serial number; the rest is added by Windows.
            let serial = serial.split('&').next().unwrap_or(&serial).to_owned();
            out.push((name, serial));
        }
    }
    out.sort();
    out
}

pub fn enumerate(include_disconnected: bool) -> Result<Vec<Device>> {
    let set = unsafe {
        DevInfo(SetupDiGetClassDevsW(
            None,
            PCWSTR::null(),
            None,
            if include_disconnected {
                DIGCF_ALLCLASSES
            } else {
                DIGCF_ALLCLASSES | DIGCF_PRESENT
            },
        )?)
    };
    let mut devices = Vec::new();
    for index in 0.. {
        let mut data = SP_DEVINFO_DATA {
            cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        if unsafe { SetupDiEnumDeviceInfo(set.0, index, &mut data) }.is_err() {
            break; // ERROR_NO_MORE_ITEMS
        }
        let Some(id) = instance_id(set.0, &data) else {
            continue;
        };
        if id.starts_with(r"HTREE\ROOT") {
            continue; // device-tree root pseudo-node
        }
        let name = reg_property(set.0, &data, SPDRP_FRIENDLYNAME)
            .or_else(|| reg_property(set.0, &data, SPDRP_DEVICEDESC))
            .unwrap_or_else(|| id.clone());
        let mut status = CM_DEVNODE_STATUS_FLAGS(0);
        let mut problem = CM_PROB(0);
        let cr = unsafe { CM_Get_DevNode_Status(&mut status, &mut problem, data.DevInst, 0) };
        let problem = (cr == CR_SUCCESS && (status.0 & DN_HAS_PROBLEM.0) != 0).then_some(problem.0);
        let present = cr == CR_SUCCESS;
        let class_id = reg_property(set.0, &data, SPDRP_CLASS).unwrap_or_else(|| "Other devices".into());
        devices.push(Device {
            instance_id: id,
            name,
            class: class_description(&data.ClassGuid).unwrap_or_else(|| class_id.clone()),
            class_id,
            manufacturer: reg_property(set.0, &data, SPDRP_MFG).unwrap_or_default(),
            problem,
            present,
        });
    }
    if devices.is_empty() {
        bail!("no devices were enumerated");
    }
    Ok(devices)
}

/// Groups devices by class, sorted by class then name.
pub fn group_by_class(devices: &[Device]) -> BTreeMap<String, Vec<&Device>> {
    let mut map: BTreeMap<String, Vec<&Device>> = BTreeMap::new();
    for d in devices {
        map.entry(d.class.clone()).or_default().push(d);
    }
    for list in map.values_mut() {
        list.sort_by_key(|d| d.name.to_lowercase());
    }
    map
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Enable,
    Disable,
    Restart,
    Remove,
}

impl Action {
    fn flag(self) -> &'static str {
        match self {
            Action::Enable => "/enable-device",
            Action::Disable => "/disable-device",
            Action::Restart => "/restart-device",
            Action::Remove => "/remove-device",
        }
    }

    fn reverse(self) -> Option<Action> {
        match self {
            Action::Enable => Some(Action::Disable),
            Action::Disable => Some(Action::Enable),
            Action::Restart | Action::Remove => None,
        }
    }
}

/// pnputil exit code meaning "succeeded, reboot required".
const ERROR_SUCCESS_REBOOT_REQUIRED: i32 = 3010;

fn run_pnputil(action: Action, instance_id: &str) -> Result<bool> {
    let out = hidden_command("pnputil")
        .arg(action.flag())
        .arg(instance_id)
        .output()?;
    match out.status.code() {
        Some(0) => Ok(false),
        Some(ERROR_SUCCESS_REBOOT_REQUIRED) => Ok(true),
        _ => bail!("{}", String::from_utf8_lossy(&out.stdout).trim()),
    }
}

#[derive(Default)]
pub struct DeviceManager {
    task: Option<BackgroundTask<Vec<Device>>>,
    devices: Option<Result<Vec<Device>, String>>,
    filter: String,
    show: Show,
    selected: Option<String>,
    confirm: Confirm<(Action, Device)>,
    /// The USB Storage History list, while its window is open.
    usb: Option<Vec<(String, String)>>,
    batch_confirm: Confirm<Vec<Device>>,
    side_task: Option<BackgroundTask<String>>,
    message: Message,
}

impl DeviceManager {
    fn apply(&mut self, ctx: &AppCtx<'_>, action: Action, device: &Device) {
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            return;
        }
        self.message = Some(match run_pnputil(action, &device.instance_id) {
            Ok(reboot) => {
                let undo = match action.reverse() {
                    Some(rev) => UndoAction::RunCommand {
                        program: "pnputil".into(),
                        args: vec![rev.flag().into(), device.instance_id.clone()],
                    },
                    None => UndoAction::None,
                };
                let _ = ctx
                    .journal
                    .record("devices", format!("{action:?} device \"{}\"", device.name), undo);
                self.devices = None; // refresh
                Ok(format!(
                    "{action:?} succeeded for {}{}",
                    device.name,
                    if reboot {
                        " (restart Windows to finish)."
                    } else {
                        "."
                    }
                ))
            }
            Err(e) => Err(format!("{action:?} failed: {e}")),
        });
    }
}

impl Tool for DeviceManager {
    fn layout(&self) -> Layout {
        Layout::Fill
    }

    fn id(&self) -> &'static str {
        "devices"
    }

    fn category(&self) -> Category {
        Category::Information
    }

    fn requires_admin(&self) -> bool {
        true // to change devices; listing works without it
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if self.task.is_none() && self.devices.is_none() {
            let all = self.show == Show::Disconnected;
            self.task = Some(BackgroundTask::spawn(ui.ctx(), move |_| enumerate(all)));
        }
        if let Some(task) = &mut self.task {
            task.poll();
            if let Some(r) = task.take_result() {
                self.devices = Some(r);
                self.task = None;
            }
        }
        if let Some((action, device)) = self.confirm.show(ui.ctx(), ctx.i18n) {
            self.apply(ctx, action, &device);
        }
        if let Some(list) = self.batch_confirm.show(ui.ctx(), ctx.i18n) {
            if ctx.config.dry_run {
                self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            } else {
                let mut failed = Vec::new();
                for d in &list {
                    if let Err(e) = run_pnputil(Action::Remove, &d.instance_id) {
                        failed.push(format!("{}: {e}", d.name));
                    }
                }
                let _ = ctx.journal.record(
                    "devices",
                    format!("Removed {} disconnected devices", list.len() - failed.len()),
                    UndoAction::None,
                );
                self.message = Some(if failed.is_empty() {
                    Ok(format!("Removed {} disconnected devices.", list.len()))
                } else {
                    Err(format!(
                        "Some devices could not be removed: {}",
                        failed.join("; ")
                    ))
                });
                self.devices = None;
            }
        }
        if let Some(t) = &mut self.side_task {
            t.poll();
            if let Some(r) = t.take_result() {
                self.message = Some(r);
                self.side_task = None;
                self.devices = None;
            }
        }

        let busy = self.side_task.is_some();
        let loading = self.task.is_some();
        let selected = self
            .selected
            .as_ref()
            .and_then(|id| {
                self.devices
                    .as_ref()?
                    .as_ref()
                    .ok()?
                    .iter()
                    .find(|d| &d.instance_id == id)
            })
            .cloned();
        let can = ctx.elevated && !busy;
        let has = selected.is_some() && can;
        let disabled = selected.as_ref().is_some_and(Device::is_disabled);
        let present = selected.as_ref().is_some_and(|d| d.present);
        // The original's toolbar, plus our Restore drivers and Remove disconnected.
        let tools = [
            Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Refresh").enabled(!loading)),
            None,
            Some(ToolButton::new(icons::POWER, "Restart").enabled(has && present)),
            Some(
                ToolButton::new(
                    if disabled { icons::PLAY } else { icons::STOP_CIRCLE },
                    if disabled { "Enable" } else { "Disable" },
                )
                .enabled(has),
            ),
            Some(ToolButton::new(icons::TRASH, "Uninstall").enabled(has)),
            None,
            Some(ToolButton::new(icons::FLOPPY_DISK, "Backup").enabled(can)),
            Some(ToolButton::new(icons::UPLOAD, "Restore").enabled(can)),
            None,
            Some(ToolButton::new(icons::CROSSHAIR, "Scan for Hardware Changes").enabled(can)),
            Some(ToolButton::new(icons::FILE_X, "Remove disconnected").enabled(can)),
            Some(ToolButton::new(icons::USB, "USB Storage History")),
            None,
            Some(ToolButton::new(icons::SIGN_OUT, "Exit")),
        ];
        match frame::toolbar(ui, &tools, |_| {}) {
            Some(0) => self.devices = None,
            Some(2) => {
                if let Some(d) = &selected {
                    self.confirm
                        .ask(format!("Restart \"{}\"?", d.name), (Action::Restart, d.clone()));
                }
            }
            Some(3) => {
                if let Some(d) = &selected {
                    if d.is_disabled() {
                        self.apply(ctx, Action::Enable, d);
                    } else {
                        self.confirm.ask(
                            format!(
                                "Disable \"{}\"? Disabling system, storage or display devices can make \
                                 Windows unusable. You can re-enable it here or undo it from the Change Journal.",
                                d.name
                            ),
                            (Action::Disable, d.clone()),
                        );
                    }
                }
            }
            Some(4) => {
                if let Some(d) = &selected {
                    self.confirm.ask(
                        format!(
                            "Uninstall \"{}\"? Its driver stays installed, so Windows sets it up again \
                             after a hardware scan or restart.",
                            d.name
                        ),
                        (Action::Remove, d.clone()),
                    );
                }
            }
            Some(6) => {
                if let Some(dir) = rfd::FileDialog::new()
                    .set_title("Folder for the driver backup")
                    .pick_folder()
                {
                    self.side_task = Some(BackgroundTask::spawn(ui.ctx(), move |_| {
                        wm_core::process::run_checked(
                            hidden_command("pnputil").args(["/export-driver", "*"]).arg(&dir),
                        )?;
                        Ok(format!("Drivers exported to {}.", dir.display()))
                    }));
                }
            }
            Some(7) => {
                if let Some(dir) = rfd::FileDialog::new()
                    .set_title("Folder with a driver backup")
                    .pick_folder()
                {
                    let _ = ctx.journal.record(
                        "devices",
                        format!("Installed drivers from {}", dir.display()),
                        UndoAction::None,
                    );
                    self.side_task = Some(BackgroundTask::spawn(ui.ctx(), move |_| {
                        let out = hidden_command("pnputil")
                            .arg("/add-driver")
                            .arg(dir.join("*.inf"))
                            .args(["/subdirs", "/install"])
                            .output()?;
                        // pnputil returns non-zero when some drivers were already present.
                        let text = String::from_utf8_lossy(&out.stdout);
                        let summary = text
                            .lines()
                            .rev()
                            .find(|l| !l.trim().is_empty())
                            .unwrap_or("")
                            .trim()
                            .to_owned();
                        Ok(format!("Driver restore finished. {summary}"))
                    }));
                }
            }
            Some(9) => {
                self.side_task = Some(BackgroundTask::spawn(ui.ctx(), |_| {
                    wm_core::process::run_checked(hidden_command("pnputil").arg("/scan-devices"))?;
                    Ok("Scan finished.".to_owned())
                }));
            }
            Some(10) => {
                let all = enumerate(true).unwrap_or_default();
                let gone: Vec<Device> = all.into_iter().filter(|d| !d.present).collect();
                if gone.is_empty() {
                    self.message = Some(Ok("There are no disconnected devices.".into()));
                } else {
                    self.batch_confirm.ask(
                        format!(
                            "Remove {} devices that are not connected? Windows sets each one up again if it is \
                             plugged back in.",
                            gone.len()
                        ),
                        gone,
                    );
                }
            }
            Some(11) => self.usb = Some(usb_storage_history()),
            Some(13) => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
            _ => {}
        }
        self.usb_window(ui);

        // Status bar: count, which devices, search (like the original).
        let count = match &self.devices {
            Some(Ok(d)) => d.len(),
            _ => 0,
        };
        let search_hint = format!("{}  {}", icons::MAGNIFYING_GLASS, ctx.i18n.tr("common-filter"));
        let before = self.show;
        frame::status(ui, &format!("{}: {count}", self.show.label()), |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text(search_hint)
                    .desired_width(180.0),
            );
            egui::ComboBox::from_id_salt("devices-show")
                .selected_text(self.show.label())
                .show_ui(ui, |ui| {
                    for s in Show::ALL {
                        ui.selectable_value(&mut self.show, s, s.label());
                    }
                });
            if busy || loading {
                ui.spinner();
            }
        });
        if self.show != before && (self.show == Show::Disconnected) != (before == Show::Disconnected) {
            self.devices = None;
        }

        let devices = match &self.devices {
            Some(Ok(d)) => d.clone(),
            Some(Err(e)) => {
                let e = e.clone();
                frame::central(ui, |ui| {
                    show_message(ui, &self.message);
                    ui.colored_label(ui.visuals().error_fg_color, e);
                });
                return;
            }
            None => {
                frame::central(ui, |ui| {
                    ui.spinner();
                });
                return;
            }
        };
        let f = self.filter.to_lowercase();
        let visible: Vec<Device> = devices
            .iter()
            .filter(|d| match self.show {
                Show::All => true,
                Show::Problems => d.problem.is_some(),
                Show::Disconnected => !d.present,
            })
            .filter(|d| {
                f.is_empty()
                    || d.name.to_lowercase().contains(&f)
                    || d.class.to_lowercase().contains(&f)
                    || d.instance_id.to_lowercase().contains(&f)
            })
            .cloned()
            .collect();

        // The original's Details panel: Name / Value of the selected device.
        let warn = ui.visuals().warn_fg_color;
        egui::Panel::bottom("device_detail")
            .resizable(true)
            .default_size(150.0)
            .frame(egui::Frame::new().inner_margin(egui::Margin::symmetric(12, 6)))
            .show(ui, |ui| {
                ui.vertical_centered(|ui| ui.strong("Details"));
                ui.separator();
                let Some(d) = &selected else {
                    ui.weak("Select a device to see its details.");
                    return;
                };
                egui::Grid::new("device_props")
                    .num_columns(2)
                    .striped(true)
                    .spacing([24.0, 4.0])
                    .show(ui, |ui| {
                        ui.strong("Name");
                        ui.strong("Value");
                        ui.end_row();
                        for (k, v) in [
                            ("Device", d.name.clone()),
                            ("Class", format!("{} ({})", d.class, d.class_id)),
                            ("Manufacturer", d.manufacturer.clone()),
                            ("Instance ID", d.instance_id.clone()),
                            ("Connected", if d.present { "Yes" } else { "No" }.to_owned()),
                        ] {
                            ui.label(k);
                            ui.add(egui::Label::new(v).selectable(true));
                            ui.end_row();
                        }
                        ui.label("Status");
                        match d.problem {
                            Some(code) => {
                                ui.colored_label(warn, format!("{} (code {code})", problem_text(code)))
                            }
                            None => ui.label("Working properly"),
                        };
                        ui.end_row();
                    });
            });

        frame::central(ui, |ui| {
            show_message(ui, &self.message);
            let accent = ui.visuals().selection.bg_fill;
            for (class, list) in group_by_class(&visible) {
                let has_problem = list.iter().any(|d| d.problem.is_some());
                let title = format!("{} {class}", class_icon(&list[0].class_id));
                let header = if has_problem {
                    egui::RichText::new(format!("{title}  ⚠")).color(accent)
                } else {
                    egui::RichText::new(title).color(accent)
                };
                egui::CollapsingHeader::new(header)
                    .id_salt(&class)
                    .default_open(has_problem || !f.is_empty() || self.show != Show::All)
                    .show(ui, |ui| {
                        for d in list {
                            let icon = match d.problem {
                                Some(_) if d.is_disabled() => "⏸ ",
                                Some(_) => "⚠ ",
                                None => "",
                            };
                            let text = egui::RichText::new(format!("{icon}{}", d.name));
                            let text = if !d.present {
                                egui::RichText::new(format!("{} (disconnected)", d.name)).weak()
                            } else if d.problem.is_some() && !d.is_disabled() {
                                text.color(warn)
                            } else {
                                text
                            };
                            let is_sel = self.selected.as_deref() == Some(d.instance_id.as_str());
                            if ui.selectable_label(is_sel, text).clicked() {
                                self.selected = Some(d.instance_id.clone());
                            }
                        }
                    });
            }
        });
    }
}

/// Which devices the list shows (the original's status-bar combo).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Show {
    #[default]
    All,
    Problems,
    Disconnected,
}

impl Show {
    const ALL: [Show; 3] = [Show::All, Show::Problems, Show::Disconnected];

    fn label(self) -> &'static str {
        match self {
            Show::All => "All Devices",
            Show::Problems => "Devices with Problems",
            Show::Disconnected => "Disconnected Devices",
        }
    }
}

/// An icon for a device class.
fn class_icon(class_id: &str) -> &'static str {
    match class_id {
        "Display" | "Monitor" => icons::MONITOR,
        "DiskDrive" | "SCSIAdapter" | "HDC" | "Volume" | "VolumeSnapshot" => icons::HARD_DRIVES,
        "Net" | "NetService" | "NetTrans" => icons::NETWORK,
        "Keyboard" => icons::KEYBOARD,
        "Mouse" => icons::MOUSE_SIMPLE,
        "USB" | "USBDevice" => icons::USB,
        "Printer" | "PrintQueue" => icons::PRINTER,
        "MEDIA" | "AudioEndpoint" | "AudioProcessingObject" => icons::BELL,
        "Processor" | "Computer" | "System" | "Firmware" => icons::CPU,
        "Bluetooth" => icons::WIFI_HIGH,
        _ => icons::DEVICES,
    }
}

impl DeviceManager {
    fn usb_window(&mut self, ui: &egui::Ui) {
        let Some(list) = &self.usb else { return };
        let mut open = true;
        egui::Window::new("USB Storage History")
            .open(&mut open)
            .default_size([520.0, 320.0])
            .show(ui.ctx(), |ui| {
                ui.weak("USB drives and memory cards this PC has seen, from the registry.");
                ui.separator();
                if list.is_empty() {
                    ui.label("No USB storage devices have been connected.");
                }
                egui::ScrollArea::vertical().show(ui, |ui| {
                    egui::Grid::new("usb-history")
                        .striped(true)
                        .num_columns(2)
                        .spacing([24.0, 4.0])
                        .show(ui, |ui| {
                            ui.strong("Device");
                            ui.strong("Serial number");
                            ui.end_row();
                            for (name, serial) in list {
                                ui.label(name);
                                ui.add(
                                    egui::Label::new(egui::RichText::new(serial).monospace())
                                        .selectable(true),
                                );
                                ui.end_row();
                            }
                        });
                });
            });
        if !open {
            self.usb = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str, class: &str, problem: Option<u32>) -> Device {
        Device {
            instance_id: format!("X\\{name}"),
            name: name.into(),
            class: class.into(),
            class_id: class.into(),
            manufacturer: String::new(),
            problem,
            present: true,
        }
    }

    #[test]
    fn grouping_and_status() {
        let devices = [
            dev("b", "USB", None),
            dev("a", "USB", Some(22)),
            dev("z", "Audio", Some(28)),
        ];
        let groups = group_by_class(&devices);
        assert_eq!(groups.keys().collect::<Vec<_>>(), ["Audio", "USB"]);
        assert_eq!(
            groups["USB"].iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert!(devices[1].is_disabled());
        assert!(!devices[2].is_disabled());
        assert_eq!(problem_text(28), "The drivers for this device are not installed.");
        assert_eq!(Action::Disable.reverse(), Some(Action::Enable));
        assert_eq!(Action::Restart.reverse(), None);
    }

    #[test]
    fn enumerates_real_devices() {
        let devices = enumerate(false).unwrap();
        assert!(enumerate(true).unwrap().len() >= devices.len());
        assert!(devices.len() > 10, "expected many devices, got {}", devices.len());
        assert!(
            devices
                .iter()
                .all(|d| !d.instance_id.is_empty() && !d.name.is_empty())
        );
        assert!(
            devices
                .iter()
                .any(|d| d.class_id == "Processor" || d.class_id == "System")
        );
        // Classes carry Device Manager's names, not the short ones.
        assert!(
            devices.iter().any(|d| d.class != d.class_id),
            "no friendly class names"
        );
        let _ = usb_storage_history();
    }
}
