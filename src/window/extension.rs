//! Tauri window effects delegated to this runtime without changing Tauri.

use super::{background_material, EffectsTarget, WindowEffectsConfig, EFFECTS_TARGET};
use crate::{runtime::IPC_TIMEOUT, ElectronRuntime};
use std::{ops::Deref, sync::mpsc::sync_channel};
use tauri::{plugin::TauriPlugin, Manager};

/// An official Tauri window whose effect methods use Electron's compositor.
/// Other methods remain available through `Deref`.
#[derive(Clone)]
pub struct Window(tauri::Window<ElectronRuntime>);

impl From<tauri::Window<ElectronRuntime>> for Window {
  fn from(window: tauri::Window<ElectronRuntime>) -> Self {
    Self(window)
  }
}

impl From<tauri::WebviewWindow<ElectronRuntime>> for Window {
  fn from(window: tauri::WebviewWindow<ElectronRuntime>) -> Self {
    Self(window.as_ref().window())
  }
}

impl Deref for Window {
  type Target = tauri::Window<ElectronRuntime>;

  fn deref(&self) -> &Self::Target {
    &self.0
  }
}

impl Window {
  /// Applies the first supported effect. Windows 11 22H2+ supports Acrylic,
  /// Mica and Tabbed; other platforms and unsupported lists return an error.
  pub fn set_effects<E: Into<Option<WindowEffectsConfig>>>(&self, effects: E) -> tauri::Result<()> {
    set_effects(&self.0, effects)
  }

  /// Clears effects and restores the configured window background.
  pub fn clear_effects(&self) -> tauri::Result<()> {
    clear_effects(&self.0)
  }
}

pub fn set_input_shape(
    window: &tauri::Window<ElectronRuntime>,
    rectangles: Vec<electron_rs::rpc::Rectangle>,
) -> tauri::Result<()> {
    let (tx, rx) = sync_channel(1);
    window.run_on_main_thread(move || {
        let target = EFFECTS_TARGET.with(|slot| slot.borrow().clone());
        let result = target
            .ok_or_else(|| std::io::Error::other("Electron window route is unavailable"))
            .and_then(|target| target.set_input_shape(rectangles));
        let _ = tx.send(result);
    })?;
    rx.recv_timeout(IPC_TIMEOUT)
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Electron input shape timed out",
            )
        })?
        .map_err(Into::into)
}

pub fn set_fullscreenable(
    window: &tauri::Window<ElectronRuntime>,
    value: bool,
) -> tauri::Result<()> {
    let (tx, rx) = sync_channel(1);
    window.run_on_main_thread(move || {
        let target = EFFECTS_TARGET.with(|slot| slot.borrow().clone());
        let result = target
            .ok_or_else(|| std::io::Error::other("Electron window route is unavailable"))
            .and_then(|target| target.set_fullscreenable(value));
        let _ = tx.send(result);
    })?;
    rx.recv_timeout(IPC_TIMEOUT)
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Electron window configuration timed out",
            )
        })?
        .map_err(Into::into)
}



/// Applies effects to an existing official Tauri window on this runtime.
/// The scheduled task is scoped to that window and Electron connection.
pub fn set_effects<E: Into<Option<WindowEffectsConfig>>>(
  window: &tauri::Window<ElectronRuntime>,
  effects: E,
) -> tauri::Result<()> {
  let effects = effects.into();
  let material = background_material(effects.as_ref())?;
  let (tx, rx) = sync_channel(1);
  window.run_on_main_thread(move || {
    let target: Option<EffectsTarget> = EFFECTS_TARGET.with(|slot| slot.borrow().clone());
    let result = target
      .ok_or_else(|| std::io::Error::other("Electron window route is unavailable"))
      .and_then(|target| target.apply(material));
    let _ = tx.send(result);
  })?;
  rx.recv_timeout(IPC_TIMEOUT)
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "Electron window task did not finish before timeout"))?
    .map_err(Into::into)
}

/// Clears effects on an existing official Tauri window.
pub fn clear_effects(window: &tauri::Window<ElectronRuntime>) -> tauri::Result<()> {
  set_effects(window, None)
}

/// Shows a Tauri menu as a native context menu on this runtime.
///
/// Tauri's own `Window::popup_menu` reaches the platform through `ns_view()`/
/// `hwnd()`; an AppKit view pointer belongs to Electron's process, so macOS
/// returns `Unavailable` and the popup is silently skipped — the user sees no
/// menu at all (issue #1370). This routes the same menu through Electron's own
/// `Menu.popup`, which every platform supports.
///
/// Returns once the menu is up; `on_command` receives the activated item's id
/// after the user dismisses the menu.
pub fn popup_context_menu(
  window: &tauri::Window<ElectronRuntime>,
  menu: &tauri::menu::Menu<ElectronRuntime>,
  on_command: impl Fn(tauri::menu::MenuId) + Send + 'static,
) -> tauri::Result<()> {
  let (items, lookup) = menu_items(menu)?;
  let (tx, rx) = sync_channel(1);
  window.run_on_main_thread(move || {
    let target: Option<EffectsTarget> = EFFECTS_TARGET.with(|slot| slot.borrow().clone());
    let result = target
      .ok_or_else(|| std::io::Error::other("Electron window route is unavailable"))
      .and_then(|target| {
        target.popup_menu(items, move |command| {
          if let Some(id) = lookup.get(&command) {
            on_command(tauri::menu::MenuId::new(id));
          }
        })
      });
    let _ = tx.send(result);
  })?;
  rx.recv_timeout(IPC_TIMEOUT)
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "Electron window task did not finish before timeout"))?
    .map_err(Into::into)
}

/// Flattens a Tauri menu into the items Electron's `Menu.popup` accepts, plus the
/// command-id → item-id map that turns the answer back into a `MenuId`.
fn menu_items(
  menu: &tauri::menu::Menu<ElectronRuntime>,
) -> tauri::Result<(Vec<electron_rs::rpc::window::MenuItem>, std::collections::HashMap<u32, String>)> {
  use electron_rs::rpc::window::{MenuItem, MenuItemType};
  use std::collections::HashMap;
  use tauri::menu::MenuItemKind;

  fn convert(
    kind: &MenuItemKind<ElectronRuntime>,
    lookup: &mut HashMap<u32, String>,
  ) -> Option<MenuItem> {
    let command = |id: &tauri::menu::MenuId, lookup: &mut HashMap<u32, String>| {
      let value = command_id(id.as_ref());
      lookup.insert(value, id.as_ref().to_owned());
      value
    };
    match kind {
      MenuItemKind::MenuItem(item) => Some(MenuItem {
        label: Some(item.text().ok()?),
        enabled: Some(item.is_enabled().unwrap_or(true)),
        command_id: Some(command(item.id(), lookup)),
        ..MenuItem::default()
      }),
      MenuItemKind::Check(item) => Some(MenuItem {
        kind: Some(MenuItemType::Checkbox),
        label: Some(item.text().ok()?),
        enabled: Some(item.is_enabled().unwrap_or(true)),
        checked: Some(item.is_checked().unwrap_or(false)),
        command_id: Some(command(item.id(), lookup)),
        ..MenuItem::default()
      }),
      MenuItemKind::Icon(item) => Some(MenuItem {
        label: Some(item.text().ok()?),
        enabled: Some(item.is_enabled().unwrap_or(true)),
        command_id: Some(command(item.id(), lookup)),
        ..MenuItem::default()
      }),
      MenuItemKind::Submenu(item) => Some(MenuItem {
        label: Some(item.text().ok()?),
        enabled: Some(item.is_enabled().unwrap_or(true)),
        submenu: Some(
          item
            .items()
            .ok()?
            .iter()
            .filter_map(|child| convert(child, lookup))
            .collect(),
        ),
        ..MenuItem::default()
      }),
      // A predefined separator carries no id of its own and never returns a command.
      MenuItemKind::Predefined(_) => Some(MenuItem {
        kind: Some(MenuItemType::Separator),
        ..MenuItem::default()
      }),
    }
  }

  let mut lookup = HashMap::new();
  let items = menu
    .items()?
    .iter()
    .filter_map(|kind| convert(kind, &mut lookup))
    .collect();
  Ok((items, lookup))
}

/// Stable numeric id for a menu item, derived from its id.
fn command_id(id: &str) -> u32 {
  let mut hash: u32 = 0x811c_9dc5;
  for byte in id.as_bytes() {
    hash ^= u32::from(*byte);
    hash = hash.wrapping_mul(0x0100_0193);
  }
  hash
}

mod commands {
  use super::*;

  #[tauri::command]
  pub async fn set_effects(
    window: tauri::Window<ElectronRuntime>,
    label: Option<String>,
    value: Option<WindowEffectsConfig>,
  ) -> Result<(), String> {
    let target = match label.filter(|label| !label.is_empty()) {
      Some(label) => window.get_window(&label).ok_or(tauri::Error::WindowNotFound),
      None => Ok(window),
    }.map_err(|error| error.to_string())?;
    super::set_effects(&target, value).map_err(|error| error.to_string())
  }
}

/// Registers the command used by `tauri-runtime-electron/api/window`.
/// Declare an inlined `electron-window` plugin with command `set_effects` in
/// the application's `tauri-build` attributes, then explicitly grant
/// `electron-window:allow-set-effects` in its capabilities.
pub fn init() -> TauriPlugin<ElectronRuntime> {
  tauri::plugin::Builder::new("electron-window")
    .invoke_handler(tauri::generate_handler![commands::set_effects])
    .build()
}
