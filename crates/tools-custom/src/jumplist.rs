//! JumpList Quick Launcher: put your favourite programs, folders and files in this
//! app's taskbar jump list. Pin Win Manager RS to the taskbar, right-click it, and the
//! items appear under "Tasks" — even when the app is closed.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use windows::Win32::Storage::EnhancedStorage::PKEY_Title;
use windows::Win32::System::Com::StructuredStorage::{
    PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Shell::Common::{IObjectArray, IObjectCollection};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::Win32::UI::Shell::{
    DestinationList, EnumerableObjectCollection, ICustomDestinationList, SHStrDupW,
};
use windows::core::{HSTRING, Interface, PCWSTR, PWSTR};
use wm_core::{
    AppCtx, Category, Tool, egui, paths,
    safety::UndoAction,
    shell::{ShortcutSpec, ensure_com, make_link},
    ui::{Message, show_message},
};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Item {
    pub title: String,
    pub target: PathBuf,
    #[serde(default)]
    pub args: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    item: Vec<Item>,
}

fn store_path() -> Result<PathBuf> {
    Ok(paths::config_dir()?.join("jumplist.toml"))
}

pub fn load_from(path: &std::path::Path) -> Vec<Item> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| toml::from_str::<Saved>(&t).ok())
        .map(|s| s.item)
        .unwrap_or_default()
}

pub fn save_to(path: &std::path::Path, items: &[Item]) -> Result<()> {
    std::fs::write(path, toml::to_string_pretty(&Saved { item: items.to_vec() })?)
        .context("saving jump list items")
}

/// Folders and documents open through Explorer; programs run directly.
fn spec_for(item: &Item) -> ShortcutSpec {
    let is_program = item
        .target
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"));
    if is_program {
        ShortcutSpec {
            target: item.target.clone(),
            args: item.args.clone(),
            ..Default::default()
        }
    } else {
        ShortcutSpec {
            target: PathBuf::from(paths::expand_env(r"%WINDIR%\explorer.exe")),
            args: format!("\"{}\"", item.target.display()),
            ..Default::default()
        }
    }
}

/// Replaces the taskbar jump list tasks with `items` (an empty list clears it).
pub fn apply(items: &[Item]) -> Result<()> {
    ensure_com();
    unsafe {
        let list: ICustomDestinationList = CoCreateInstance(&DestinationList, None, CLSCTX_INPROC_SERVER)?;
        if items.is_empty() {
            list.DeleteList(PCWSTR::null())?;
            return Ok(());
        }
        let mut slots = 0u32;
        let _removed: IObjectArray = list.BeginList(&mut slots)?;
        let tasks: IObjectCollection =
            CoCreateInstance(&EnumerableObjectCollection, None, CLSCTX_INPROC_SERVER)?;
        for item in items {
            let link = make_link(&spec_for(item))?;
            // Jump list tasks take their caption from PKEY_Title (a VT_LPWSTR).
            // PROPVARIANT's Drop calls PropVariantClear, which CoTaskMemFree()s the string,
            // so it must be CoTaskMem-allocated: SHStrDupW does exactly that.
            let title: PWSTR = SHStrDupW(&HSTRING::from(item.title.as_str()))?;
            let value = PROPVARIANT {
                Anonymous: PROPVARIANT_0 {
                    Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                        vt: VT_LPWSTR,
                        Anonymous: PROPVARIANT_0_0_0 { pwszVal: title },
                        ..Default::default()
                    }),
                },
            };
            let store: IPropertyStore = link.cast()?;
            store.SetValue(&PKEY_Title, &value)?;
            store.Commit()?;
            tasks.AddObject(&link)?;
        }
        list.AddUserTasks(&tasks.cast::<IObjectArray>()?)?;
        list.CommitList()?;
    }
    Ok(())
}

#[derive(Default)]
pub struct JumpListLauncher {
    items: Vec<Item>,
    loaded: bool,
    title: String,
    message: Message,
}

impl JumpListLauncher {
    fn save_and_apply(&mut self, ctx: &AppCtx<'_>, what: String) {
        if ctx.config.dry_run {
            self.message = Some(Ok(ctx.i18n.tr("common-dry-run-skip")));
            return;
        }
        let r = store_path()
            .and_then(|p| save_to(&p, &self.items))
            .and_then(|()| apply(&self.items));
        self.message = Some(
            r.map(|()| {
                let _ = ctx.journal.record("jumplist", what.clone(), UndoAction::None);
                format!("{what}. Pin Win Manager RS to the taskbar and right-click it to use the list.")
            })
            .map_err(|e| format!("{e:#}")),
        );
    }

    fn add(&mut self, ctx: &AppCtx<'_>, target: PathBuf) {
        let title = if self.title.trim().is_empty() {
            target
                .file_stem()
                .or(target.file_name())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            self.title.trim().to_owned()
        };
        self.title.clear();
        self.items.push(Item {
            title: title.clone(),
            target,
            args: String::new(),
        });
        self.save_and_apply(ctx, format!("Added \"{title}\" to the jump list"));
    }
}

impl Tool for JumpListLauncher {
    fn id(&self) -> &'static str {
        "jumplist"
    }

    fn category(&self) -> Category {
        Category::Customization
    }

    fn ui(&mut self, ui: &mut egui::Ui, ctx: &mut AppCtx<'_>) {
        if !self.loaded {
            self.items = store_path().map(|p| load_from(&p)).unwrap_or_default();
            self.loaded = true;
        }
        ui.label(
            "Add programs, folders or files, pin Win Manager RS to the taskbar, then right-click its \
             taskbar button to launch them. The list works even when the app is closed.",
        );
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.title)
                    .hint_text("Title (optional)")
                    .desired_width(160.0),
            );
            if ui.button("Add program or file…").clicked()
                && let Some(p) = rfd::FileDialog::new().pick_file()
            {
                self.add(ctx, p);
            }
            if ui.button("Add folder…").clicked()
                && let Some(p) = rfd::FileDialog::new().pick_folder()
            {
                self.add(ctx, p);
            }
        });
        show_message(ui, &self.message);
        ui.separator();
        if self.items.is_empty() {
            ui.weak("The jump list is empty.");
            return;
        }
        let mut remove = None;
        let mut swap = None;
        let n = self.items.len();
        for (i, item) in self.items.iter().enumerate() {
            ui.horizontal(|ui| {
                if ui.add_enabled(i > 0, egui::Button::new("⬆").small()).clicked() {
                    swap = Some((i, i - 1));
                }
                if ui
                    .add_enabled(i + 1 < n, egui::Button::new("⬇").small())
                    .clicked()
                {
                    swap = Some((i, i + 1));
                }
                ui.strong(&item.title);
                ui.weak(item.target.display().to_string());
                if ui.small_button("Remove").clicked() {
                    remove = Some(i);
                }
            });
        }
        if let Some((a, b)) = swap {
            self.items.swap(a, b);
            self.save_and_apply(ctx, "Reordered the jump list".into());
        }
        if let Some(i) = remove {
            let removed = self.items.remove(i);
            self.save_and_apply(ctx, format!("Removed \"{}\" from the jump list", removed.title));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persistence_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j.toml");
        assert!(load_from(&path).is_empty());
        let items = vec![
            Item {
                title: "Notepad".into(),
                target: r"C:\Windows\notepad.exe".into(),
                args: String::new(),
            },
            Item {
                title: "Temp".into(),
                target: std::env::temp_dir(),
                args: String::new(),
            },
        ];
        save_to(&path, &items).unwrap();
        assert_eq!(load_from(&path), items);
    }

    #[test]
    fn folders_open_through_explorer() {
        let s = spec_for(&Item {
            title: "T".into(),
            target: r"C:\Temp".into(),
            args: String::new(),
        });
        assert!(s.target.ends_with("explorer.exe"));
        assert_eq!(s.args, "\"C:\\Temp\"");
        let p = spec_for(&Item {
            title: "N".into(),
            target: r"C:\Windows\notepad.exe".into(),
            args: "x".into(),
        });
        assert_eq!(p.args, "x");
    }

    #[test]
    fn apply_and_clear_jump_list() {
        // Affects only the jump list of the test executable.
        let items = vec![Item {
            title: "Notepad".into(),
            target: r"C:\Windows\notepad.exe".into(),
            args: String::new(),
        }];
        apply(&items).unwrap();
        apply(&[]).unwrap();
    }
}
