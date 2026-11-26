//! Windows Apps Uninstaller: Microsoft Store and built-in apps for the current user.
//! List or tile view (with each app's logo and tile colour), filters, launch, desktop
//! shortcut, repair by re-registering, removal (optionally for all users and new
//! users) and export.
//!
//! Uses the Appx PowerShell module (stable property names, locale-independent JSON).

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use egui_extras::{Column, TableBuilder};
use serde::Deserialize;
use wm_core::{
    AppCtx, Category, Layout, Tool,
    egui::{self, Color32, Vec2},
    icons,
    process::{hidden_command, json_list, powershell_json, run_checked},
    registry,
    safety::UndoAction,
    shell::{KnownFolder, create_item_shortcut, known_folder},
    task::BackgroundTask,
    ui::{Confirm, Message, ToolButton, frame, show_message},
};

/// Packages that keep the Store, app installation and security UI working.
const PROTECTED: &[&str] = &[
    "Microsoft.WindowsStore",
    "Microsoft.StorePurchaseApp",
    "Microsoft.DesktopAppInstaller",
    "Microsoft.SecHealthUI",
    "Microsoft.Windows.ShellExperienceHost",
    "Microsoft.Windows.StartMenuExperienceHost",
    "Microsoft.WindowsAppRuntime",
];

const PROVISIONED_KEY: &str =
    r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Appx\AppxAllUserStore\Applications";

#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
pub struct Package {
    pub name: String,
    pub package_full_name: String,
    #[serde(default)]
    pub package_family_name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub install_location: Option<String>,
    #[serde(default)]
    pub publisher: String,
    #[serde(default)]
    pub signature_kind: String,
    #[serde(skip)]
    pub display_name: String,
    /// First application's id, for launching (`<family>!<id>`).
    #[serde(skip)]
    pub app_id: Option<String>,
    #[serde(skip)]
    pub logo: Option<PathBuf>,
    /// Tile background colour from the manifest (`None` = transparent).
    #[serde(skip)]
    pub tile_color: Option<[u8; 3]>,
    /// Installed with Windows for every user (provisioned).
    #[serde(skip)]
    pub built_in: bool,
}

impl Package {
    pub fn is_protected(&self) -> bool {
        PROTECTED.iter().any(|p| self.name.starts_with(p))
    }

    pub fn is_microsoft(&self) -> bool {
        self.publisher.contains("O=Microsoft Corporation")
    }

    /// `shell:AppsFolder` id used to launch the app or pin a shortcut to it.
    pub fn aumid(&self) -> Option<String> {
        self.app_id
            .as_ref()
            .map(|id| format!("{}!{id}", self.package_family_name))
    }

    pub fn publisher_name(&self) -> String {
        self.publisher
            .split(", ")
            .find_map(|part| part.strip_prefix("O=").or_else(|| part.strip_prefix("CN=")))
            .unwrap_or(&self.publisher)
            .trim_matches('"')
            .to_owned()
    }
}

/// `<DisplayName>` from the package manifest, unless it is an unresolved resource reference.
pub fn manifest_display_name(manifest: &str) -> Option<String> {
    let props = &manifest[manifest.find("<Properties>")?..];
    let start = props.find("<DisplayName>")? + "<DisplayName>".len();
    let end = start + props[start..].find("</DisplayName>")?;
    let name = props[start..end].trim();
    (!name.is_empty() && !name.starts_with("ms-resource:")).then(|| name.replace("&amp;", "&"))
}

/// The value of `name="…"` inside one XML start tag.
fn attr(tag: &str, name: &str) -> Option<String> {
    let needle = format!(" {name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let end = start + tag[start..].find('"')?;
    Some(tag[start..end].to_owned())
}

/// The start tag beginning at `from` (up to its `>`).
fn tag_at(text: &str, from: usize) -> &str {
    let end = text[from..].find('>').map_or(text.len(), |i| from + i);
    &text[from..end]
}

/// First `<Application>`: (id, square 44×44 logo, tile background colour).
pub fn manifest_application(manifest: &str) -> Option<(String, Option<String>, Option<String>)> {
    let app_start = manifest.find("<Application ")?;
    let id = attr(tag_at(manifest, app_start), "Id")?;
    let visual = manifest[app_start..]
        .find("VisualElements ")
        .map(|i| tag_at(manifest, app_start + i));
    Some((
        id,
        visual.and_then(|t| attr(t, "Square44x44Logo")),
        visual.and_then(|t| attr(t, "BackgroundColor")),
    ))
}

/// `#0078D4` → RGB; `transparent` and named colours → `None`.
pub fn parse_color(s: &str) -> Option<[u8; 3]> {
    let hex = s.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(hex, 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

/// Finds the file for a manifest asset such as `Assets\AppList.png`, which is stored
/// with qualifiers (`AppList.targetsize-48.png`, `AppList.scale-100.png`). Prefers a
/// size near 48 px and the unplated (transparent) variant.
pub fn resolve_asset(dir: &Path, rel: &str) -> Option<PathBuf> {
    let exact = dir.join(rel);
    if exact.is_file() {
        return Some(exact);
    }
    let folder = exact.parent()?;
    let stem = exact.file_stem()?.to_string_lossy().to_lowercase();
    let ext = exact
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let mut best: Option<(u32, PathBuf)> = None;
    for e in std::fs::read_dir(folder).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().to_lowercase();
        let Some(quals) = name
            .strip_prefix(&format!("{stem}."))
            .and_then(|r| r.strip_suffix(&format!(".{ext}")))
        else {
            continue;
        };
        if quals.contains("contrast-") || quals.contains("lightunplated") {
            continue;
        }
        let num = |key: &str| {
            quals
                .split(['_', '.'])
                .find_map(|q| q.strip_prefix(key).and_then(|n| n.parse::<u32>().ok()))
        };
        let mut score = match (num("targetsize-"), num("scale-")) {
            (Some(t), _) => t.abs_diff(48),
            (None, Some(s)) => 50 + s.abs_diff(100),
            _ => 200,
        };
        if !quals.contains("unplated") {
            score += 5;
        }
        if best.as_ref().is_none_or(|(b, _)| score < *b) {
            best = Some((score, e.path()));
        }
    }
    best.map(|(_, p)| p)
}

/// `Microsoft.ZuneMusic` → `ZuneMusic`; `5319275A.WhatsAppDesktop` → `WhatsAppDesktop`.
pub fn pretty_name(package_name: &str) -> String {
    package_name.rsplit('.').next().unwrap_or(package_name).to_owned()
}

fn provisioned_names() -> HashSet<String> {
    registry::open_read(PROVISIONED_KEY)
        .map(|k| {
            k.enum_keys()
                .flatten()
                .filter_map(|full| full.split('_').next().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

pub fn enumerate() -> Result<Vec<Package>> {
    let v = powershell_json(
        "@(Get-AppxPackage | Where-Object { -not $_.IsFramework -and -not $_.NonRemovable -and \
           \"$($_.SignatureKind)\" -ne 'System' } | \
         Select-Object Name, PackageFullName, PackageFamilyName, Version, InstallLocation, Publisher, \
           @{ n = 'SignatureKind'; e = { \"$($_.SignatureKind)\" } }) | ConvertTo-Json -Compress",
    )?;
    let mut packages: Vec<Package> = json_list(v)
        .into_iter()
        .filter_map(|o| serde_json::from_value(o).ok())
        .collect();
    let provisioned = provisioned_names();
    for p in &mut packages {
        p.built_in = provisioned.contains(&p.name);
        let dir = p.install_location.as_deref().map(PathBuf::from);
        let manifest = dir
            .as_ref()
            .and_then(|d| std::fs::read_to_string(d.join("AppxManifest.xml")).ok());
        p.display_name = manifest
            .as_deref()
            .and_then(manifest_display_name)
            .unwrap_or_else(|| pretty_name(&p.name));
        if let (Some(m), Some(d)) = (manifest.as_deref(), dir.as_ref())
            && let Some((id, logo, color)) = manifest_application(m)
        {
            p.app_id = Some(id);
            p.logo = logo.and_then(|l| resolve_asset(d, &l));
            p.tile_color = color.as_deref().and_then(parse_color);
        }
    }
    packages.sort_by_key(|p| p.display_name.to_lowercase());
    Ok(packages)
}

/// Package names contain only `[A-Za-z0-9._~-]`; anything else is refused before it
/// reaches PowerShell.
fn checked(s: &str) -> Result<&str> {
    if s.is_empty() || !s.chars().all(|c| c.is_ascii_alphanumeric() || "._~-".contains(c)) {
        bail!("unexpected package name {s}");
    }
    Ok(s)
}

fn powershell(script: &str) -> Result<()> {
    run_checked(hidden_command("powershell").args([
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        &format!("$ErrorActionPreference = 'Stop'; {script}"),
    ]))?;
    Ok(())
}

/// Removes the app for the current user, or with `all_users` for every account and
/// from the image so new accounts don't get it (administrator).
pub fn remove(package: &Package, all_users: bool) -> Result<()> {
    let full = checked(&package.package_full_name)?;
    if all_users {
        let name = checked(&package.name)?;
        powershell(&format!(
            "Get-AppxProvisionedPackage -Online | Where-Object DisplayName -eq '{name}' | \
             Remove-AppxProvisionedPackage -Online | Out-Null; \
             Remove-AppxPackage -Package '{full}' -AllUsers"
        ))
    } else {
        powershell(&format!("Remove-AppxPackage -Package '{full}'"))
    }
}

/// Re-registers the installed package from its manifest: repairs a broken
/// registration (app won't start, missing from Start) without touching its data.
pub fn reregister(package: &Package) -> Result<()> {
    let Some(loc) = &package.install_location else {
        bail!("{} has no install folder", package.display_name)
    };
    let manifest = Path::new(loc).join("AppxManifest.xml");
    if !manifest.is_file() {
        bail!("{} has no manifest", package.display_name);
    }
    let path = manifest.display().to_string().replace('\'', "''");
    powershell(&format!(
        "Add-AppxPackage -DisableDevelopmentMode -Register '{path}'"
    ))
}

pub fn launch(package: &Package) -> Result<()> {
    let Some(aumid) = package.aumid() else {
        bail!("{} has no app to start", package.display_name)
    };
    std::process::Command::new("explorer")
        .arg(format!("shell:AppsFolder\\{aumid}"))
        .spawn()
        .context("starting Explorer")?;
    Ok(())
}

fn safe_file_name(s: &str) -> String {
    s.chars()
        .map(|c| if "\\/:*?\"<>|".contains(c) { '_' } else { c })
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Creates a desktop shortcut and returns its path.
pub fn desktop_shortcut(package: &Package) -> Result<PathBuf> {
    let Some(aumid) = package.aumid() else {
        bail!("{} has no app to start", package.display_name)
    };
    let lnk =
        known_folder(KnownFolder::Desktop)?.join(format!("{}.lnk", safe_file_name(&package.display_name)));
    create_item_shortcut(&lnk, &format!("shell:AppsFolder\\{aumid}"))?;
    Ok(lnk)
}

pub fn export_csv(packages: &[&Package]) -> String {
    let field = |s: &str| {
        if s.contains([',', '"']) {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            s.to_owned()
        }
    };
    let mut out = String::from("App,Package,Version,Publisher,Built in,Source,Install folder\r\n");
    for p in packages {
        let _ = write!(
            out,
            "{},{},{},{},{},{},{}\r\n",
            field(&p.display_name),
            p.name,
            p.version,
            field(&p.publisher_name()),
            if p.built_in { "Yes" } else { "No" },
            p.signature_kind,
            field(p.install_location.as_deref().unwrap_or_default())
        );
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Microsoft,
    Others,
    BuiltIn,
    AddedLater,
}

impl Filter {
    const ALL: [Filter; 5] = [
        Filter::All,
        Filter::Microsoft,
        Filter::Others,
        Filter::BuiltIn,
        Filter::AddedLater,
    ];

    fn label(self) -> &'static str {
        match self {
            Filter::All => "All apps",
            Filter::Microsoft => "Microsoft",
            Filter::Others => "Other publishers",
            Filter::BuiltIn => "Built into Windows",
            Filter::AddedLater => "Installed later",
        }
    }

    fn keeps(self, p: &Package) -> bool {
        match self {
            Filter::All => true,
            Filter::Microsoft => p.is_microsoft(),
            Filter::Others => !p.is_microsoft(),
            Filter::BuiltIn => p.built_in,
            Filter::AddedLater => !p.built_in,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppSort {
    Name,
    Publisher,
    Version,
}

impl AppSort {
    const ALL: [AppSort; 3] = [AppSort::Name, AppSort::Publisher, AppSort::Version];

    fn label(self) -> &'static str {
        match self {
            AppSort::Name => "Name",
            AppSort::Publisher => "Publisher",
            AppSort::Version => "Version",
        }
    }
}

fn sort_packages(list: &mut [&Package], (key, asc): (AppSort, bool)) {
    list.sort_by(|a, b| {
        let o = match key {
            AppSort::Name => a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase()),
            AppSort::Publisher => a
                .publisher_name()
                .to_lowercase()
                .cmp(&b.publisher_name().to_lowercase()),
            AppSort::Version => a.version.cmp(&b.version),
        };
        if asc { o } else { o.reverse() }
    });
}

/// Per-app removal outcome (display name, result).
type RemovalResults = Vec<(String, Result<(), String>)>;
type LogoBatch = Vec<(String, Option<([usize; 2], Vec<u8>)>)>;

enum Ask {
    Remove(Vec<Package>, bool),
    Repair(Box<Package>),
}

fn load_logo(path: &Path) -> Option<([usize; 2], Vec<u8>)> {
    let img = image::open(path).ok()?.to_rgba8();
    let img = if img.width() > 64 || img.height() > 64 {
        image::imageops::thumbnail(&img, 48, 48)
    } else {
        img
    };
    Some(([img.width() as usize, img.height() as usize], img.into_raw()))
}

pub struct AppsUninstaller {
    load: Option<BackgroundTask<Vec<Package>>>,
    packages: Option<Result<Vec<Package>, String>>,
    working: Option<BackgroundTask<RemovalResults>>,
    working_label: &'static str,
    marked: HashSet<String>,
    selected: Option<String>,
    filter: String,
    kind: Filter,
    sort: (AppSort, bool),
    /// Batch Uninstall is on: apps get tick boxes and Uninstall takes the ticked ones.
    batch_mode: bool,
    tiles: bool,
    tile_colors: bool,
    logos: HashMap<String, egui::TextureHandle>,
    /// Logos already requested (failures are not retried every frame).
    logo_tried: HashSet<String>,
    logo_task: Option<BackgroundTask<LogoBatch>>,
    confirm: Confirm<Ask>,
    message: Message,
}

impl Default for AppsUninstaller {
    fn default() -> Self {
        Self {
            load: None,
            packages: None,
            working: None,
            working_label: "",
            marked: HashSet::new(),
            selected: None,
            filter: String::new(),
            kind: Filter::All,
            sort: (AppSort::Name, true),
            batch_mode: false,
            tiles: true,
            tile_colors: false,
            logos: HashMap::new(),
            logo_tried: HashSet::new(),
            logo_task: None,
            confirm: Confirm::default(),
            message: None,
        }
    }
}

impl AppsUninstaller {
    fn poll_logos(&mut self, ctx: &egui::Context, packages: &[Package]) {
        if let Some(t) = &mut self.logo_task {
            t.poll();
            if let Some(Ok(list)) = t.take_result() {
                self.logo_task = None;
                for (name, img) in list {
                    if let Some((size, rgba)) = img {
                        let image = egui::ColorImage::from_rgba_unmultiplied(size, &rgba);
                        let tex =
                            ctx.load_texture(format!("app-{name}"), image, egui::TextureOptions::LINEAR);
                        self.logos.insert(name, tex);
                    }
                }
            }
            return;
        }
        let wanted: Vec<(String, PathBuf)> = packages
            .iter()
            .filter(|p| !self.logo_tried.contains(&p.package_full_name))
            .filter_map(|p| p.logo.clone().map(|l| (p.package_full_name.clone(), l)))
            .collect();
        if !wanted.is_empty() {
            self.logo_tried.extend(wanted.iter().map(|(n, _)| n.clone()));
            self.logo_task = Some(BackgroundTask::spawn(ctx, move |_| {
                Ok(wanted.into_iter().map(|(n, p)| (n, load_logo(&p))).collect())
            }));
        }
    }

    fn handle_confirm(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        let Some(ask) = self.confirm.show(ui.ctx(), ctx.i18n) else {
            return;
        };
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            return;
        }
        let (list, label): (Vec<(Package, bool)>, &'static str) = match ask {
            Ask::Remove(list, all) => (list.into_iter().map(|p| (p, all)).collect(), "Removing"),
            Ask::Repair(p) => (vec![(*p, false)], "Repairing"),
        };
        let repair = label == "Repairing";
        self.working_label = label;
        self.working = Some(BackgroundTask::spawn(ui.ctx(), move |p| {
            let n = list.len() as f32;
            Ok(list
                .iter()
                .enumerate()
                .map(|(i, (pkg, all))| {
                    p.report(i as f32 / n, format!("{label} {}…", pkg.display_name));
                    let r = if repair {
                        reregister(pkg)
                    } else {
                        remove(pkg, *all)
                    };
                    (pkg.display_name.clone(), r.map_err(|e| format!("{e:#}")))
                })
                .collect())
        }));
    }

    fn poll_work(&mut self, ctx: &mut AppCtx<'_>) {
        let Some(t) = &mut self.working else { return };
        t.poll();
        let Some(r) = t.take_result() else { return };
        self.working = None;
        let Ok(results) = r else { return };
        let repair = self.working_label == "Repairing";
        let ok: Vec<&String> = results
            .iter()
            .filter(|(_, r)| r.is_ok())
            .map(|(n, _)| n)
            .collect();
        let failed: Vec<String> = results
            .iter()
            .filter_map(|(n, r)| r.as_ref().err().map(|e| format!("{n}: {e}")))
            .collect();
        for n in &ok {
            let _ = ctx.journal.record(
                "apps",
                if repair {
                    format!("Re-registered Windows app {n}")
                } else {
                    format!("Removed Windows app {n} (reinstall it from the Microsoft Store)")
                },
                UndoAction::None,
            );
        }
        let verb = if repair { "Repaired" } else { "Removed" };
        self.message = Some(if failed.is_empty() {
            Ok(format!("{verb} {} apps.", ok.len()))
        } else {
            Err(format!("{verb} {}; failed: {}", ok.len(), failed.join("; ")))
        });
        if !repair {
            self.marked.clear();
            self.packages = None;
        }
    }

    fn list_view(&mut self, ui: &mut egui::Ui, visible: &[&Package]) {
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
        let mut clicked = None;
        let batch = self.batch_mode;
        TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .column(Column::exact(if batch { 26.0 } else { 0.0 }))
            .column(Column::exact(22.0))
            .column(Column::initial(230.0).clip(true))
            .column(Column::initial(170.0).clip(true))
            .column(Column::initial(110.0).clip(true))
            .column(Column::initial(70.0))
            .column(Column::remainder().clip(true))
            .header(22.0, |mut h| {
                for t in ["", "", "App", "Publisher", "Version", "Built in", "Package"] {
                    h.col(|ui| {
                        ui.strong(t);
                    });
                }
            })
            .body(|body| {
                body.rows(22.0, visible.len(), |mut row| {
                    let p = visible[row.index()];
                    row.set_selected(self.selected.as_deref() == Some(p.package_full_name.as_str()));
                    row.col(|ui| {
                        if batch {
                            self.mark_box(ui, p);
                        }
                    });
                    row.col(|ui| {
                        if let Some(t) = self.logos.get(&p.package_full_name) {
                            let (r, _) = ui.allocate_exact_size(Vec2::splat(18.0), egui::Sense::hover());
                            if self.tile_colors
                                && let Some([cr, cg, cb]) = p.tile_color
                            {
                                ui.painter().rect_filled(r, 2.0, Color32::from_rgb(cr, cg, cb));
                            }
                            egui::Image::new(t).paint_at(ui, r.shrink(1.0));
                        }
                    });
                    row.col(|ui| {
                        ui.label(&p.display_name);
                    });
                    row.col(|ui| {
                        ui.weak(p.publisher_name());
                    });
                    row.col(|ui| {
                        ui.weak(&p.version);
                    });
                    row.col(|ui| {
                        ui.weak(if p.built_in { "Yes" } else { "" });
                    });
                    row.col(|ui| {
                        ui.weak(&p.name);
                    });
                    if row.response().clicked() {
                        clicked = Some(p.package_full_name.clone());
                    }
                });
            });
        if clicked.is_some() {
            self.selected = clicked;
        }
    }

    fn mark_box(&mut self, ui: &mut egui::Ui, p: &Package) {
        let mut on = self.marked.contains(&p.package_full_name);
        let r = ui.add_enabled(!p.is_protected(), egui::Checkbox::without_text(&mut on));
        if r.changed() {
            if on {
                self.marked.insert(p.package_full_name.clone());
            } else {
                self.marked.remove(&p.package_full_name);
            }
        }
        r.on_disabled_hover_text("Needed for the Store, app installation or Windows Security.");
    }

    fn tile_view(&mut self, ui: &mut egui::Ui, visible: &[&Package]) {
        let tile = Vec2::new(112.0, 100.0);
        let accent = wm_core::ui::ACCENT;
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::splat(6.0);
                for p in visible {
                    let (rect, resp) = ui.allocate_exact_size(tile, egui::Sense::click());
                    let key = &p.package_full_name;
                    let sel = self.selected.as_deref() == Some(key.as_str());
                    let painter = ui.painter_at(rect);
                    if sel {
                        painter.rect_filled(rect, 4.0, accent.gamma_multiply(0.25));
                    } else if resp.hovered() {
                        painter.rect_filled(rect, 4.0, ui.visuals().widgets.hovered.weak_bg_fill);
                    }
                    if self.marked.contains(key) {
                        painter.rect_stroke(
                            rect.shrink(1.0),
                            4.0,
                            egui::Stroke::new(2.0, accent),
                            egui::StrokeKind::Inside,
                        );
                    }
                    let square = egui::Rect::from_center_size(
                        rect.center_top() + Vec2::new(0.0, 30.0),
                        Vec2::splat(48.0),
                    );
                    if self.tile_colors
                        && let Some([r, g, b]) = p.tile_color
                    {
                        painter.rect_filled(square, 4.0, Color32::from_rgb(r, g, b));
                    }
                    match self.logos.get(key) {
                        Some(t) => {
                            egui::Image::new(t).paint_at(ui, square.shrink(6.0));
                        }
                        None => {
                            painter.text(
                                square.center(),
                                egui::Align2::CENTER_CENTER,
                                icons::GRID_FOUR,
                                egui::FontId::proportional(26.0),
                                ui.visuals().weak_text_color(),
                            );
                        }
                    }
                    let galley = ui.painter().layout(
                        p.display_name.clone(),
                        egui::FontId::proportional(11.5),
                        ui.visuals().text_color(),
                        tile.x - 8.0,
                    );
                    let pos = egui::pos2(rect.center().x - galley.size().x / 2.0, rect.top() + 60.0);
                    painter
                        .with_clip_rect(rect.shrink(2.0))
                        .galley(pos, galley, ui.visuals().text_color());
                    let resp =
                        resp.on_hover_text(format!("{}\n{}\n{}", p.display_name, p.publisher_name(), p.name));
                    if resp.clicked() {
                        if (self.batch_mode || ui.input(|i| i.modifiers.ctrl)) && !p.is_protected() {
                            if !self.marked.remove(key) {
                                self.marked.insert(key.clone());
                            }
                            self.batch_mode = true;
                        }
                        self.selected = Some(key.clone());
                    }
                    if resp.double_clicked() {
                        let _ = launch(p);
                    }
                }
            });
        });
    }
}

impl Tool for AppsUninstaller {
    fn id(&self) -> &'static str {
        "apps"
    }

    fn category(&self) -> Category {
        Category::Cleaner
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if self.load.is_none() && self.packages.is_none() {
            self.load = Some(BackgroundTask::spawn(ui.ctx(), |_| enumerate()));
        }
        if let Some(t) = &mut self.load {
            t.poll();
            if let Some(r) = t.take_result() {
                self.packages = Some(r);
                self.load = None;
            }
        }
        self.poll_work(ctx);
        self.handle_confirm(ui, ctx);

        let packages = match &self.packages {
            Some(Ok(p)) => p.clone(),
            Some(Err(e)) => {
                let e = e.clone();
                frame::central(ui, |ui| {
                    ui.colored_label(ui.visuals().error_fg_color, e);
                });
                return;
            }
            None => {
                frame::central(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.weak("Reading the installed apps…");
                    });
                });
                return;
            }
        };
        self.poll_logos(ui.ctx(), &packages);
        let selected = self
            .selected
            .as_ref()
            .and_then(|k| packages.iter().find(|p| &p.package_full_name == k))
            .cloned();
        let marked: Vec<Package> = packages
            .iter()
            .filter(|p| self.marked.contains(&p.package_full_name))
            .cloned()
            .collect();
        let targets: Vec<Package> = if marked.is_empty() {
            selected.iter().filter(|p| !p.is_protected()).cloned().collect()
        } else {
            marked
        };
        let busy = self.working.is_some();
        let has_app = selected.as_ref().is_some_and(|p| p.app_id.is_some());
        let uninstall_label = if targets.len() > 1 {
            format!("Uninstall {}", targets.len())
        } else {
            "Uninstall".to_owned()
        };
        let batch_label = if self.batch_mode {
            "Batch Uninstall ✓"
        } else {
            "Batch Uninstall"
        };

        let f = self.filter.to_lowercase();
        let mut visible: Vec<&Package> = packages
            .iter()
            .filter(|p| self.kind.keeps(p))
            .filter(|p| {
                f.is_empty()
                    || p.display_name.to_lowercase().contains(&f)
                    || p.name.to_lowercase().contains(&f)
            })
            .collect();
        sort_packages(&mut visible, self.sort);

        // The original's toolbar; its Filter, View, Sort and Background Color
        // drop-downs are the pickers in the strip below.
        let clicked = frame::toolbar(
            ui,
            &[
                Some(ToolButton::new(icons::ARROWS_CLOCKWISE, "Refresh").enabled(!busy)),
                Some(ToolButton::new(icons::UPLOAD, "Export")),
                None,
                Some(ToolButton::new(icons::PLAY, "Launch").enabled(has_app)),
                Some(ToolButton::new(icons::DESKTOP, "Create on Desktop").enabled(has_app)),
                Some(ToolButton::new(icons::WRENCH, "Re-Register").enabled(!busy && selected.is_some())),
                None,
                Some(ToolButton::new(icons::LIST_CHECKS, batch_label).enabled(!busy)),
                Some(ToolButton::new(icons::TRASH, &uninstall_label).enabled(!busy && !targets.is_empty())),
                Some(
                    ToolButton::new(icons::X, "Delete").enabled(!busy && !targets.is_empty() && ctx.elevated),
                ),
                None,
                Some(
                    ToolButton::new(icons::FOLDER_OPEN, "Open Folder")
                        .enabled(selected.as_ref().is_some_and(|p| p.install_location.is_some())),
                ),
                None,
                Some(ToolButton::new(icons::SIGN_OUT, "Exit")),
            ],
            |_| {},
        );
        match clicked {
            Some(0) => {
                self.packages = None;
                self.logos.clear();
                self.logo_tried.clear();
            }
            Some(1) => {
                if let Some(file) = rfd::FileDialog::new()
                    .set_file_name("Windows apps.csv")
                    .add_filter("CSV", &["csv"])
                    .save_file()
                {
                    self.message = Some(
                        std::fs::write(&file, export_csv(&visible))
                            .map(|_| format!("Exported {} apps to {}.", visible.len(), file.display()))
                            .map_err(|e| format!("{e:#}")),
                    );
                }
            }
            Some(3) => {
                if let Some(p) = &selected {
                    self.message = launch(p).err().map(|e| Err(format!("{e:#}")));
                }
            }
            Some(4) => {
                if let Some(p) = &selected {
                    self.message = Some(
                        desktop_shortcut(p)
                            .map(|l| format!("Created {}.", l.display()))
                            .map_err(|e| format!("{e:#}")),
                    );
                }
            }
            Some(5) => {
                if let Some(p) = &selected {
                    self.confirm.ask(
                        format!(
                            "Re-register {}? This repairs an app that won't start or is missing from Start. \
                             Its data is kept. Close the app first.",
                            p.display_name
                        ),
                        Ask::Repair(Box::new(p.clone())),
                    );
                }
            }
            Some(7) => {
                self.batch_mode = !self.batch_mode;
                if !self.batch_mode {
                    self.marked.clear();
                }
            }
            Some(i @ (8 | 9)) => {
                let all = i == 9;
                let names: Vec<&str> = targets.iter().map(|p| p.display_name.as_str()).collect();
                let scope = if all {
                    "for every account on this PC, and stop Windows adding them for new accounts"
                } else {
                    "for your account"
                };
                self.confirm.ask(
                    format!(
                        "Remove {} {scope}? They can be reinstalled from the Microsoft Store, but their local \
                         data will be deleted.",
                        names.join(", ")
                    ),
                    Ask::Remove(targets.clone(), all),
                );
            }
            Some(11) => {
                if let Some(l) = selected.as_ref().and_then(|p| p.install_location.clone()) {
                    let _ = std::process::Command::new("explorer").arg(l).spawn();
                }
            }
            Some(13) => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close),
            _ => {}
        }

        frame::options(ui, |ui| {
            ui.label("Filter:");
            egui::ComboBox::from_id_salt("apps-kind")
                .selected_text(self.kind.label())
                .show_ui(ui, |ui| {
                    for k in Filter::ALL {
                        ui.selectable_value(&mut self.kind, k, k.label());
                    }
                });
            ui.label("View:");
            egui::ComboBox::from_id_salt("apps-view")
                .selected_text(if self.tiles { "Large Icons" } else { "Details" })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.tiles, true, "Large Icons");
                    ui.selectable_value(&mut self.tiles, false, "Details");
                });
            ui.label("Sort:");
            egui::ComboBox::from_id_salt("apps-sort")
                .selected_text(self.sort.0.label())
                .show_ui(ui, |ui| {
                    for k in AppSort::ALL {
                        if ui.selectable_label(self.sort.0 == k, k.label()).clicked() {
                            self.sort = (k, true);
                        }
                    }
                });
            if ui
                .small_button(if self.sort.1 { "▲" } else { "▼" })
                .on_hover_text("Reverse the order")
                .clicked()
            {
                self.sort.1 = !self.sort.1;
            }
            ui.separator();
            ui.checkbox(&mut self.tile_colors, "Background colour")
                .on_hover_text("Show each app's own tile background colour behind its logo.");
        });
        let status = match &self.working {
            Some(t) => t.status.clone(),
            None => format!(
                "Loading completed  |  Total: {}{}",
                visible.len(),
                if self.marked.is_empty() {
                    String::new()
                } else {
                    format!("; {} ticked", self.marked.len())
                }
            ),
        };
        frame::status(ui, &status, |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.filter)
                    .hint_text(format!("{}  Search", icons::MAGNIFYING_GLASS))
                    .desired_width(180.0),
            );
            if busy {
                ui.spinner();
            }
        });
        frame::central_fixed(ui, |ui| {
            show_message(ui, &self.message);
            if self.tiles {
                self.tile_view(ui, &visible);
            } else {
                self.list_view(ui, &visible);
            }
        });
    }

    fn layout(&self) -> Layout {
        Layout::Fill
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parsing() {
        let m = "<Package><Properties><DisplayName>Clipchamp &amp; Co</DisplayName></Properties>\
                 <Applications><Application Id=\"App\" Executable=\"x.exe\">\
                 <uap:VisualElements DisplayName=\"ms-resource:A\" Square44x44Logo=\"Assets\\AppList.png\" \
                 BackgroundColor=\"#0078D4\"></uap:VisualElements></Application></Applications></Package>";
        assert_eq!(manifest_display_name(m).as_deref(), Some("Clipchamp & Co"));
        let r = "<Properties><DisplayName>ms-resource:AppName</DisplayName></Properties>";
        assert_eq!(manifest_display_name(r), None);
        assert_eq!(pretty_name("Microsoft.ZuneMusic"), "ZuneMusic");
        assert_eq!(
            manifest_application(m),
            Some((
                "App".into(),
                Some(r"Assets\AppList.png".into()),
                Some("#0078D4".into())
            ))
        );
        assert_eq!(manifest_application("<Package/>"), None);
        assert_eq!(parse_color("#0078D4"), Some([0, 0x78, 0xD4]));
        assert_eq!(parse_color("transparent"), None);
    }

    #[test]
    fn asset_qualifiers() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("Assets");
        std::fs::create_dir_all(&a).unwrap();
        for f in [
            "AppList.scale-200.png",
            "AppList.targetsize-48.png",
            "AppList.targetsize-48_altform-unplated.png",
            "AppList.targetsize-48_altform-lightunplated.png",
            "AppList.targetsize-256.png",
            "AppList.contrast-white_targetsize-48.png",
        ] {
            std::fs::write(a.join(f), b"x").unwrap();
        }
        let got = resolve_asset(dir.path(), r"Assets\AppList.png").unwrap();
        assert!(
            got.ends_with("AppList.targetsize-48_altform-unplated.png"),
            "{got:?}"
        );
        std::fs::write(a.join("Exact.png"), b"x").unwrap();
        assert!(
            resolve_asset(dir.path(), r"Assets\Exact.png")
                .unwrap()
                .ends_with("Exact.png")
        );
        assert_eq!(resolve_asset(dir.path(), r"Assets\Missing.png"), None);
    }

    #[test]
    fn protection_and_name_validation() {
        let p = Package {
            name: "Microsoft.WindowsStore".into(),
            package_full_name: "Microsoft.WindowsStore_1.0_x64__8wekyb3d8bbwe".into(),
            publisher: "CN=Microsoft Corporation, O=Microsoft Corporation, L=Redmond, S=Washington, C=US"
                .into(),
            ..Default::default()
        };
        assert!(p.is_protected() && p.is_microsoft());
        assert_eq!(p.publisher_name(), "Microsoft Corporation");
        let evil = Package {
            package_full_name: "x'; Remove-Item C:\\ -Recurse; '".into(),
            ..p.clone()
        };
        assert!(
            remove(&evil, false).is_err(),
            "injection-shaped names are refused before PowerShell runs"
        );
        let evil_name = Package {
            name: "a' -or 1 -eq 1 -or 'b".into(),
            ..p
        };
        assert!(remove(&evil_name, true).is_err());
    }

    #[test]
    fn lists_real_packages() {
        let packages = enumerate().unwrap();
        assert!(
            packages
                .iter()
                .all(|p| !p.package_full_name.is_empty() && !p.display_name.is_empty())
        );
        // Most PCs have built-in apps with launchable entries and logos.
        if let Some(p) = packages.iter().find(|p| p.built_in && p.app_id.is_some()) {
            assert!(p.aumid().unwrap().contains('!'));
            if let Some(logo) = &p.logo {
                assert!(load_logo(logo).is_some(), "{logo:?}");
            }
        }
    }
}

#[cfg(test)]
mod shortcut_tests {
    #[test]
    fn store_app_shortcut_is_written() {
        let packages = super::enumerate().unwrap();
        let Some(p) = packages.iter().find(|p| p.app_id.is_some()) else {
            return; // no launchable Store app on this PC
        };
        let dir = tempfile::tempdir().unwrap();
        let lnk = dir.path().join("app.lnk");
        wm_core::shell::create_item_shortcut(&lnk, &format!(r"shell:AppsFolder\{}", p.aumid().unwrap()))
            .unwrap();
        assert!(std::fs::metadata(&lnk).unwrap().len() > 0);
    }
}
