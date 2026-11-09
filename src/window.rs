// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use crate::{
  platform::monitor_from_display,
  runtime::{
    create_webview_impl, create_window_err, create_window_impl,
    size_constraints_payload, ElectronRuntime, Message, RuntimeContext,
  },
  window_builder::ElectronWindowBuilder,
};
use crate::{rpc, rpc_runtime::Ipc as ElectronProcess};
use electron_rs::RpcRequest;
use std::{cell::RefCell, sync::Weak};

pub use tauri_utils::{
  config::{Color, WindowEffectsConfig},
  WindowEffect as Effect, WindowEffectState as EffectState,
};

#[cfg(feature = "tauri")]
mod extension;
#[cfg(feature = "tauri")]
pub use extension::{clear_effects, init, popup_context_menu, set_effects, set_input_shape, set_fullscreenable, Window};
#[cfg(feature = "tauri")]
pub use tauri::window::EffectsBuilder;
use tauri_runtime::{
  dpi::{PhysicalPosition, PhysicalSize, Position, Size},
  monitor::Monitor,
  webview::{DetachedWebview, PendingWebview},
  window::{
    CursorIcon, DetachedWindow, PendingWindow, RawWindow, WindowEvent, WindowId,
    WindowSizeConstraints,
  },
  Icon, ProgressBarState, ProgressBarStatus, Result, UserAttentionType, UserEvent, WindowDispatch,
  WindowEventId,
};
use tauri_utils::Theme;

/// A thread-safe route bound to one window and one Electron connection.
#[derive(Debug, Clone)]
pub(crate) struct EffectsTarget {
  serial: u32,
  ipc: Weak<ElectronProcess>,
}

impl EffectsTarget {
  fn set_fullscreenable(&self, fullscreenable: bool) -> std::io::Result<()> {
    self.ipc.upgrade()
      .ok_or_else(|| std::io::Error::other("Electron runtime has exited"))?
      .call(electron_rs::rpc::window::SetFullscreenable { window_id: self.serial, fullscreenable })
      .map_err(|error| std::io::Error::other(error.to_string()))
  }
  fn set_input_shape(&self, rectangles: Vec<electron_rs::rpc::Rectangle>) -> std::io::Result<()> {
    self.ipc.upgrade()
      .ok_or_else(|| std::io::Error::other("Electron runtime has exited"))?
      .call(electron_rs::rpc::window::SetInputShape { window_id: self.serial, rectangles })
      .map_err(|error| std::io::Error::other(error.to_string()))
  }
  pub(crate) fn new(serial: u32, ipc: &std::sync::Arc<ElectronProcess>) -> Self {
    Self {
      serial,
      ipc: std::sync::Arc::downgrade(ipc),
    }
  }

  /// Shows `items` as a native context menu and reports the chosen command id.
  ///
  /// Runs the popup through a worker thread: a native menu keeps tracking until
  /// the user dismisses it, and this call must not stall the runtime's main
  /// thread while that happens.
  fn popup_menu(
    &self,
    items: Vec<rpc::window::MenuItem>,
    on_command: impl Fn(u32) + Send + 'static,
  ) -> std::io::Result<()> {
    let ipc = self
      .ipc
      .upgrade()
      .ok_or_else(|| std::io::Error::other("Electron runtime has exited"))?;
    let serial = self.serial;
    std::thread::spawn(move || {
      let request = rpc::WindowPopupMenu {
        window_id: serial,
        menu: items,
        x: None,
        y: None,
      };
      match ipc.call(request) {
        Ok(Some(choice)) => on_command(choice),
        _ => {}
      }
    });
    Ok(())
  }

  fn apply(&self, material: rpc::window::BackgroundMaterial) -> std::io::Result<()> {
    self
      .ipc
      .upgrade()
      .ok_or_else(|| std::io::Error::other("Electron runtime has exited"))?
      .call(rpc::BackgroundMaterial {
        window_id: self.serial,
        material,
      })
      .map(|_| ())
      .map_err(|error| std::io::Error::other(error.to_string()))
  }
}

// The official Window exposes scheduling, but deliberately hides its dispatcher.
// Bind only the scheduled task's lifetime, never a global 'current' runtime.
thread_local! {
  static EFFECTS_TARGET: RefCell<Option<EffectsTarget>> = const { RefCell::new(None) };
}

fn with_effects_target<R>(target: EffectsTarget, task: impl FnOnce() -> R) -> R {
  struct Restore(Option<EffectsTarget>);
  impl Drop for Restore {
    fn drop(&mut self) {
      EFFECTS_TARGET.with(|slot| slot.replace(self.0.take()));
    }
  }
  let _restore = Restore(EFFECTS_TARGET.with(|slot| slot.replace(Some(target))));
  task()
}

fn background_material(effects: Option<&WindowEffectsConfig>) -> std::io::Result<rpc::window::BackgroundMaterial> {
  match effects.and_then(|config| {
    config
      .effects
      .iter()
      .find(|effect| matches!(effect, Effect::Acrylic | Effect::Mica | Effect::Tabbed))
  }) {
    Some(Effect::Acrylic) => Ok(rpc::window::BackgroundMaterial::Acrylic),
    Some(Effect::Mica) => Ok(rpc::window::BackgroundMaterial::Mica),
    Some(Effect::Tabbed) => Ok(rpc::window::BackgroundMaterial::Tabbed),
    _ if effects.is_none_or(|config| config.effects.is_empty()) => Ok(rpc::window::BackgroundMaterial::None),
    _ => Err(std::io::Error::new(
      std::io::ErrorKind::Unsupported,
      "Unsupported window effect: Electron supports Acrylic, Mica and Tabbed only",
    )),
  }
}

#[test]
fn selects_supported_background_material_or_rejects() {
  assert_eq!(background_material(None).unwrap(), rpc::window::BackgroundMaterial::None);
  let mut config = WindowEffectsConfig::default();
  assert_eq!(background_material(Some(&config)).unwrap(), rpc::window::BackgroundMaterial::None);
  for (effect, material) in [
    (Effect::Acrylic, rpc::window::BackgroundMaterial::Acrylic),
    (Effect::Mica, rpc::window::BackgroundMaterial::Mica),
    (Effect::Tabbed, rpc::window::BackgroundMaterial::Tabbed),
  ] {
    config.effects = vec![Effect::Blur, effect];
    assert_eq!(background_material(Some(&config)).unwrap(), material);
  }
  config.effects = vec![Effect::Blur];
  assert!(background_material(Some(&config))
    .unwrap_err()
    .to_string()
    .contains("Unsupported window effect"));
}

#[derive(Debug, Clone)]
pub struct ElectronWindowDispatcher {
  pub(crate) id: WindowId,
  pub(crate) context: RuntimeContext,
  pub(crate) effects_target: EffectsTarget,
}

impl ElectronWindowDispatcher {
  /// Apply Windows backdrop materials through Electron, which also synchronizes
  /// Chromium's compositor. Applying window-vibrancy to its HWND alone does not.
  pub fn set_window_effects(&self, effects: Option<WindowEffectsConfig>) -> Result<()> {
    background_material(effects.as_ref())
      .and_then(|material| self.effects_target.apply(material))
      .map_err(|error| create_window_err(error.to_string()))
  }

  fn rpc_call<R: RpcRequest>(&self, request: R) -> Result<R::Response> {
    self
      .context
      .ipc
      .call(request)
      .map_err(|error| create_window_err(error.to_string()))
  }

  fn window_call<R: rpc::WindowRequest>(&self, mut request: R) -> Result<R::Response> {
    let serial = self
      .context
      .serial_for_window(self.id)
      .ok_or_else(|| create_window_err("window is no longer tracked".into()))?;
    request.set_window_id(serial);
    self.rpc_call(request)
  }

  /// Queries the live Electron window geometry and state.
  fn geometry(&self) -> Result<rpc::window::WindowGeometry> {
    self.window_call(rpc::Geometry { window_id: 0 })
  }
}

impl<T: UserEvent> WindowDispatch<T> for ElectronWindowDispatcher {
  type Runtime = ElectronRuntime;

  type WindowBuilder = ElectronWindowBuilder;

  fn run_on_main_thread<F: FnOnce() + Send + 'static>(&self, f: F) -> Result<()> {
    let target = self.effects_target.clone();
    self.context.send_message(Message::Task(Box::new(move || {
      with_effects_target(target, f)
    })))
  }

  fn on_window_event<F: Fn(&WindowEvent) + Send + 'static>(&self, f: F) -> WindowEventId {
    let id = self.context.next_window_event_id();
    self.context.add_window_event_handler(self.id, id, f);
    id
  }

  fn scale_factor(&self) -> Result<f64> {
    Ok(self.geometry()?.scale_factor)
  }

  fn inner_position(&self) -> Result<PhysicalPosition<i32>> {
    let bounds = self.geometry()?.content_bounds;
    Ok(PhysicalPosition::new(bounds.x, bounds.y))
  }

  fn outer_position(&self) -> Result<PhysicalPosition<i32>> {
    let bounds = self.geometry()?.bounds;
    Ok(PhysicalPosition::new(bounds.x, bounds.y))
  }

  fn inner_size(&self) -> Result<PhysicalSize<u32>> {
    let bounds = self.geometry()?.content_bounds;
    Ok(PhysicalSize::new(bounds.width, bounds.height))
  }

  fn outer_size(&self) -> Result<PhysicalSize<u32>> {
    let bounds = self.geometry()?.bounds;
    Ok(PhysicalSize::new(bounds.width, bounds.height))
  }

  fn is_fullscreen(&self) -> Result<bool> {
    Ok(self.geometry()?.fullscreen)
  }

  fn is_minimized(&self) -> Result<bool> {
    Ok(self.geometry()?.minimized)
  }

  fn is_maximized(&self) -> Result<bool> {
    #[cfg(windows)]
    return imp::is_maximized(self.context.native_handle(self.id)?);
    #[cfg(not(windows))]
    Ok(self.geometry()?.maximized)
  }

  fn is_focused(&self) -> Result<bool> {
    Ok(self.geometry()?.focused)
  }

  fn is_decorated(&self) -> Result<bool> {
    #[cfg(target_os = "linux")]
    return crate::platform::linux_window::is_decorated(self.context.x11_window(self.id)?)
      .map_err(create_window_err);
    #[cfg(not(target_os = "linux"))]
    Ok(self.geometry()?.decorated)
  }

  fn is_resizable(&self) -> Result<bool> {
    Ok(self.geometry()?.resizable)
  }

  fn is_maximizable(&self) -> Result<bool> {
    Ok(self.geometry()?.maximizable)
  }

  fn is_minimizable(&self) -> Result<bool> {
    Ok(self.geometry()?.minimizable)
  }

  fn is_closable(&self) -> Result<bool> {
    #[cfg(target_os = "linux")]
    return crate::platform::linux_window::is_closable(self.context.x11_window(self.id)?)
      .map_err(create_window_err);
    #[cfg(not(target_os = "linux"))]
    Ok(self.geometry()?.closable)
  }

  fn is_visible(&self) -> Result<bool> {
    Ok(self.geometry()?.visible)
  }

  fn title(&self) -> Result<String> {
    Ok(self.geometry()?.title)
  }

  fn current_monitor(&self) -> Result<Option<Monitor>> {
    Ok(Some(monitor_from_display(
      &self.window_call(rpc::WindowCurrentDisplay { window_id: 0 })?,
    )))
  }

  fn primary_monitor(&self) -> Result<Option<Monitor>> {
    let displays = self.rpc_call(rpc::Displays {})?;
    Ok(Some(monitor_from_display(&displays.primary)))
  }

  fn monitor_from_point(&self, x: f64, y: f64) -> Result<Option<Monitor>> {
    Ok(self.rpc_call(rpc::DisplayForPoint { x, y })?.as_ref().map(monitor_from_display))
  }

  fn available_monitors(&self) -> Result<Vec<Monitor>> {
    let displays = self.rpc_call(rpc::Displays {})?;
    Ok(
      displays
        .displays
        .iter()
        .map(monitor_from_display)
        .collect(),
    )
  }

  fn theme(&self) -> Result<Theme> {
    let query = self.window_call(rpc::WindowTheme { window_id: 0 })?;
    Ok(match query.theme {
      electron_rs::rpc::ColorScheme::Dark => Theme::Dark,
      electron_rs::rpc::ColorScheme::Light => Theme::Light,
    })
  }

  #[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
  ))]
  fn gtk_window(&self) -> Result<gtk::ApplicationWindow> {
    Err(create_window_err(
      "Electron owns the window in another process; no local GTK window is available".into(),
    ))
  }

  #[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
  ))]
  fn default_vbox(&self) -> Result<gtk::Box> {
    Err(create_window_err(
      "Electron has no local GTK widget tree".into(),
    ))
  }

  #[cfg(target_os = "android")]
  fn activity_name(&self) -> Result<String> {
    unimplemented!("tauri-runtime-electron: Android is not a supported target")
  }

  #[cfg(target_os = "ios")]
  fn scene_identifier(&self) -> Result<String> {
    unimplemented!("tauri-runtime-electron: iOS is not a supported target")
  }

  fn window_handle(
    &self,
  ) -> std::result::Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
    // An AppKit handle contains an NSView pointer, not a cross-process ID.
    // Electron's pointer is never a valid object in this Rust process.
    #[cfg(any(target_os = "macos", not(any(windows, unix))))]
    {
      Err(raw_window_handle::HandleError::Unavailable)
    }

    #[cfg(any(windows, all(unix, not(target_os = "macos"))))]
    {
      #[cfg(windows)]
      let handle = self
        .context
        .menu_owner_window(self.id)
        .map_err(|_| raw_window_handle::HandleError::Unavailable)?;
      #[cfg(not(windows))]
      let handle = self
        .context
        .native_handle(self.id)
        .map_err(|_| raw_window_handle::HandleError::Unavailable)?;
      let handle =
        std::num::NonZeroIsize::new(handle).ok_or(raw_window_handle::HandleError::Unavailable)?;
      #[cfg(windows)]
      let raw = raw_window_handle::RawWindowHandle::Win32(
        raw_window_handle::Win32WindowHandle::new(handle),
      );
      #[cfg(all(unix, not(target_os = "macos")))]
      let raw = raw_window_handle::RawWindowHandle::Xlib(raw_window_handle::XlibWindowHandle::new(
        handle.get() as std::os::raw::c_ulong,
      ));
      // SAFETY: HWND and X11 window IDs can be used across processes; unlike an
      // AppKit pointer they do not refer to Electron's process-local memory.
      Ok(unsafe { raw_window_handle::WindowHandle::borrow_raw(raw) })
    }
  }

  fn center(&self) -> Result<()> {
    self.window_call(rpc::WindowCenter { window_id: 0 })
  }

  fn request_user_attention(&self, request_type: Option<UserAttentionType>) -> Result<()> {
    let flash = request_type.is_some();
    self.window_call(rpc::WindowAttention {
      window_id: 0,
      flash,
    })
  }

  fn create_window<F: Fn(RawWindow<'_>) + Send + 'static>(
    &mut self,
    pending: PendingWindow<T, Self::Runtime>,
    after_window_creation: Option<F>,
  ) -> Result<DetachedWindow<T, Self::Runtime>> {
    create_window_impl(&self.context, pending, after_window_creation)
  }

  fn create_webview(
    &mut self,
    pending: PendingWebview<T, Self::Runtime>,
  ) -> Result<DetachedWebview<T, Self::Runtime>> {
    create_webview_impl(&self.context, self.id, pending)
  }

  fn set_resizable(&self, resizable: bool) -> Result<()> {
    self.window_call(rpc::WindowResizable {
      window_id: 0,
      resizable,
    })
  }

  fn set_maximizable(&self, maximizable: bool) -> Result<()> {
    self.window_call(rpc::WindowMaximizable {
      window_id: 0,
      maximizable,
    })
  }

  fn set_minimizable(&self, minimizable: bool) -> Result<()> {
    self.window_call(rpc::WindowMinimizable {
      window_id: 0,
      minimizable,
    })
  }

  fn set_closable(&self, closable: bool) -> Result<()> {
    #[cfg(target_os = "linux")]
    crate::platform::linux_window::set_closable(self.context.x11_window(self.id)?, closable)
      .map_err(create_window_err)?;
    self.window_call(rpc::WindowClosable {
      window_id: 0,
      closable,
    })
  }

  fn set_title<S: Into<String>>(&self, title: S) -> Result<()> {
    self.window_call(rpc::WindowTitle {
      window_id: 0,
      title: title.into(),
    })
  }

  fn maximize(&self) -> Result<()> {
    #[cfg(windows)]
    return imp::set_maximized(self.context.native_handle(self.id)?, true);
    #[cfg(not(windows))]
    self.window_call(rpc::WindowMaximize { window_id: 0 })
  }

  fn unmaximize(&self) -> Result<()> {
    #[cfg(windows)]
    return imp::set_maximized(self.context.native_handle(self.id)?, false);
    #[cfg(not(windows))]
    self.window_call(rpc::WindowUnmaximize { window_id: 0 })
  }

  fn minimize(&self) -> Result<()> {
    self.window_call(rpc::WindowMinimize { window_id: 0 })
  }

  fn unminimize(&self) -> Result<()> {
    self.window_call(rpc::WindowUnminimize { window_id: 0 })
  }

  fn show(&self) -> Result<()> {
    self.window_call(rpc::WindowShow { window_id: 0 })
  }

  fn hide(&self) -> Result<()> {
    self.window_call(rpc::WindowHide { window_id: 0 })
  }

  fn close(&self) -> Result<()> {
    // Let Electron tear the window down; it reports back with `window.closed`,
    // which drives the `CloseRequested` event in the run loop.
    self.window_call(rpc::WindowClose { window_id: 0 })?;
    self.context.send_message(Message::CloseWindow(self.id))?;
    Ok(())
  }

  fn destroy(&self) -> Result<()> {
    self.window_call(rpc::WindowDestroy { window_id: 0 })?;
    self.context.send_message(Message::DestroyWindow(self.id))?;
    Ok(())
  }

  // Electron cannot flip the frame through its own API. Change the native
  // caption/hints without rewriting the independent button capabilities.
  fn set_decorations(&self, decorations: bool) -> Result<()> {
    #[cfg(windows)]
    {
      imp::set_decorations(self.context.native_handle(self.id)?, decorations)?;
      self
        .window_call(rpc::WindowDecorations {
          window_id: 0,
          value: decorations,
        })
        .map(|_| ())
    }
    #[cfg(target_os = "linux")]
    {
      crate::platform::linux_window::set_decorations(
        self.context.x11_window(self.id)?,
        decorations,
      )
      .map_err(create_window_err)?;
      self.window_call(rpc::WindowDecorations {
        window_id: 0,
        value: decorations,
      })
    }
    #[cfg(target_os = "macos")]
    return self.window_call(rpc::WindowDecorations {
      window_id: 0,
      value: decorations,
    });
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
      let _ = decorations;
      crate::runtime::unsupported(
        "WindowDispatch::set_decorations",
        "Electron cannot change the frame after creation",
      );
      Ok(())
    }
  }

  // Electron has no per-window drop shadow API.
  fn set_shadow(&self, shadow: bool) -> Result<()> {
    self.window_call(rpc::WindowShadow {
      window_id: 0,
      value: shadow,
    })
  }

  // Electron has no "always at the bottom" flag; push the native window to the
  // bottom of the z-order when it is requested.
  fn set_always_on_bottom(&self, always_on_bottom: bool) -> Result<()> {
    #[cfg(windows)]
    if always_on_bottom {
      imp::send_to_bottom(self.context.native_handle(self.id)?);
    }
    #[cfg(not(windows))]
    let _ = always_on_bottom;
    Ok(())
  }

  fn set_always_on_top(&self, always_on_top: bool) -> Result<()> {
    // Keep Chromium's z-order state consistent with the native HWND; otherwise
    // a later widget update can undo the cross-process SetWindowPos call.
    self.window_call(rpc::WindowAlwaysOnTop {
      window_id: 0,
      always_on_top,
    })?;
    #[cfg(windows)]
    imp::set_always_on_top(self.context.native_handle(self.id)?, always_on_top)?;
    Ok(())
  }

  fn set_visible_on_all_workspaces(&self, visible_on_all_workspaces: bool) -> Result<()> {
    self.window_call(rpc::WindowVisibleOnAllWorkspaces {
      window_id: 0,
      value: visible_on_all_workspaces,
    })
  }

  fn set_content_protected(&self, protected: bool) -> Result<()> {
    self.window_call(rpc::WindowContentProtected {
      window_id: 0,
      protected,
    })
  }

  fn set_size(&self, size: Size) -> Result<()> {
    #[cfg(windows)]
    {
      let size = imp::constrained_client_size(size, &self.geometry()?)?;
      imp::set_client_size(self.context.native_handle(self.id)?, size)
    }
    #[cfg(not(windows))]
    self.window_call(rpc::window_size(size))
  }

  fn set_min_size(&self, size: Option<Size>) -> Result<()> {
    self.window_call(rpc::window_min_size(size))
  }

  fn set_max_size(&self, size: Option<Size>) -> Result<()> {
    self.window_call(rpc::window_max_size(size))
  }

  fn set_position(&self, position: Position) -> Result<()> {
    #[cfg(windows)]
    {
      let scale = <Self as WindowDispatch<T>>::scale_factor(self)?;
      if !scale.is_finite() || scale <= 0.0 {
        return Err(create_window_err("invalid window scale factor".into()));
      }
      imp::set_position(
        self.context.native_handle(self.id)?,
        position.to_physical::<i32>(scale),
      )
    }
    #[cfg(not(windows))]
    self.window_call(rpc::window_position(position))
  }

  fn set_fullscreen(&self, fullscreen: bool) -> Result<()> {
    self.window_call(rpc::WindowFullscreen {
      window_id: 0,
      fullscreen,
      x: None,
      y: None,
    })
  }

  fn set_fullscreen_on_monitor(&self, position: PhysicalPosition<f64>) -> Result<()> {
    self.window_call(rpc::WindowFullscreen {
      window_id: 0,
      fullscreen: true,
      x: Some(position.x),
      y: Some(position.y),
    })
  }

  #[cfg(target_os = "macos")]
  fn set_simple_fullscreen(&self, enable: bool) -> Result<()> {
    self.window_call(rpc::WindowSimpleFullscreen {
      window_id: 0,
      fullscreen: enable,
    })
  }

  fn set_focus(&self) -> Result<()> {
    self.window_call(rpc::WindowFocus { window_id: 0 })
  }

  fn set_focusable(&self, focusable: bool) -> Result<()> {
    self.window_call(rpc::WindowFocusable {
      window_id: 0,
      focusable,
    })
  }

  fn set_icon(&self, icon: Icon<'_>) -> Result<()> {
    self.window_call(rpc::WindowIcon {
      window_id: 0,
      icon: icon_payload(icon),
    })
  }

  fn set_skip_taskbar(&self, skip: bool) -> Result<()> {
    self.window_call(rpc::WindowSkipTaskbar {
      window_id: 0,
      skip,
    })
  }

  fn set_cursor_grab(&self, grab: bool) -> Result<()> {
    #[cfg(windows)]
    return imp::clip_cursor(self.context.native_handle(self.id)?, grab);
    #[cfg(not(windows))]
    {
      let _ = grab;
      crate::runtime::unsupported(
        "WindowDispatch::set_cursor_grab",
        "Electron has no native pointer confinement API on this platform",
      );
      Ok(())
    }
  }

  fn set_cursor_visible(&self, visible: bool) -> Result<()> {
    self.window_call(rpc::WindowCursorVisible {
      window_id: 0,
      visible,
    })
  }

  fn set_cursor_icon(&self, icon: CursorIcon) -> Result<()> {
    self.window_call(rpc::WindowCursorIcon {
      window_id: 0,
      cursor: css_cursor(icon).to_owned(),
    })
  }

  fn set_cursor_position<Pos: Into<Position>>(&self, position: Pos) -> Result<()> {
    let scale = <Self as WindowDispatch<T>>::scale_factor(self)?;
    let position = position.into();
    if let Position::Logical(point) = position {
      if ![point.x * scale, point.y * scale].iter().all(|axis| {
        axis.is_finite() && *axis >= f64::from(i32::MIN) && *axis <= f64::from(i32::MAX)
      }) {
        return Err(create_window_err(
          "invalid or overflowing cursor position".into(),
        ));
      }
    }
    let offset = position.to_physical::<i32>(scale);
    #[cfg(windows)]
    let origin = imp::client_origin(self.context.native_handle(self.id)?)?;
    #[cfg(not(windows))]
    let origin = <Self as WindowDispatch<T>>::inner_position(self)?;
    let translate = |origin: i32, offset: i32| {
      origin
        .checked_add(offset)
        .ok_or_else(|| create_window_err("cursor position exceeds screen coordinates".into()))
    };
    let screen = PhysicalPosition::new(
      translate(origin.x, offset.x)?,
      translate(origin.y, offset.y)?,
    );
    crate::platform::cursor::warp(screen, scale).map_err(create_window_err)
  }

  fn set_ignore_cursor_events(&self, ignore: bool) -> Result<()> {
    self.window_call(rpc::WindowIgnoreCursor {
      window_id: 0,
      ignore,
    })
  }

  // Win32 hands the window a caption drag; AppKit needs the live press event,
  // so macOS routes the same request through the renderer's window.
  fn start_dragging(&self) -> Result<()> {
    #[cfg(windows)]
    {
      imp::start_dragging(self.context.native_handle(self.id)?)
    }
    #[cfg(target_os = "macos")]
    {
      self.window_call(rpc::WindowStartDrag { window_id: 0 })
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
      crate::runtime::unsupported("WindowDispatch::start_dragging", "no native drag on this platform");
      Ok(())
    }
  }

  fn start_resize_dragging(&self, direction: tauri_runtime::ResizeDirection) -> Result<()> {
    #[cfg(windows)]
    {
      imp::start_resize_dragging(self.context.native_handle(self.id)?, direction)
    }
    #[cfg(not(windows))]
    {
      let _ = direction;
      crate::runtime::unsupported(
        "WindowDispatch::start_resize_dragging",
        "only the Win32 handle can start a native resize drag",
      );
      Ok(())
    }
  }

  fn set_progress_bar(&self, progress_state: ProgressBarState) -> Result<()> {
    let state = match progress_state.status {
      None | Some(ProgressBarStatus::None) => rpc::window::ProgressState::None,
      Some(ProgressBarStatus::Normal | ProgressBarStatus::Indeterminate) => rpc::window::ProgressState::Normal,
      Some(ProgressBarStatus::Paused) => rpc::window::ProgressState::Paused,
      Some(ProgressBarStatus::Error) => rpc::window::ProgressState::Error,
    };
    self.window_call(rpc::WindowProgressBar {
      window_id: 0,
      state,
      // Electron's documented >1 convention is indeterminate on Windows;
      // other platforms clamp it to 100% (docs/tutorial/progress-bar.md).
      progress: if matches!(progress_state.status, Some(ProgressBarStatus::Indeterminate)) { 2.0 }
        else { progress_state.progress.unwrap_or(0) as f64 / 100.0 },
    })
  }

  fn set_badge_count(&self, count: Option<i64>, _desktop_filename: Option<String>) -> Result<()> {
    let count = u32::try_from(count.unwrap_or(0).max(0))
      .map_err(|_| create_window_err("badge count must fit an unsigned 32-bit integer".into()))?;
    self.rpc_call(rpc::WindowBadgeCount { count })
  }

  // macOS badge labels need a string API Electron does not expose; the dock
  // badge goes through `set_badge_count`.
  fn set_badge_label(&self, label: Option<String>) -> Result<()> {
    self.rpc_call(rpc::AppBadgeLabel { label: label.unwrap_or_default() })
  }

  fn set_overlay_icon(&self, icon: Option<Icon<'_>>) -> Result<()> {
    self.window_call(rpc::WindowOverlayIcon {
      window_id: 0,
      icon: icon.map(icon_payload),
      description: String::new(),
    })
  }

  fn set_title_bar_style(&self, style: tauri_utils::TitleBarStyle) -> Result<()> {
    #[cfg(target_os = "macos")]
    return self.window_call(rpc::WindowTitleBarStyle {
      window_id: 0,
      style: rpc::title_bar_style(style)?,
    });
    #[cfg(not(target_os = "macos"))]
    {
      let _ = style;
      Ok(())
    }
  }

  fn set_traffic_light_position(&self, position: Position) -> Result<()> {
    self.window_call(rpc::window_traffic_light(position))
  }

  fn set_size_constraints(&self, constraints: WindowSizeConstraints) -> Result<()> {
    self.window_call(rpc::WindowSizeConstraints {
      window_id: 0,
      constraints: size_constraints_payload(constraints),
    })
  }

  fn set_theme(&self, theme: Option<Theme>) -> Result<()> {
    let theme = match theme {
      Some(Theme::Dark) => electron_rs::Theme::Dark,
      Some(_) => electron_rs::Theme::Light,
      None => electron_rs::Theme::System,
    };
    self.window_call(rpc::WindowSetTheme {
      window_id: 0,
      theme,
    })
  }

  fn set_enabled(&self, enabled: bool) -> Result<()> {
    self.window_call(rpc::WindowEnabled {
      window_id: 0,
      enabled,
    })
  }

  fn is_enabled(&self) -> Result<bool> {
    self.window_call(rpc::WindowEnabledQuery { window_id: 0 })
  }

  fn is_always_on_top(&self) -> Result<bool> {
    #[cfg(windows)]
    return Ok(imp::is_always_on_top(self.context.native_handle(self.id)?));
    #[cfg(not(windows))]
    Ok(self.geometry()?.always_on_top)
  }

  fn set_background_color(&self, color: Option<tauri_utils::config::Color>) -> Result<()> {
    self.window_call(rpc::WindowBackgroundColor {
      window_id: 0,
      color: color.map(|color| electron_rs::rpc::Color::Rgba([color.0, color.1, color.2, color.3])),
    })
  }
}

fn icon_payload(icon: Icon<'_>) -> rpc::window::Icon {
  rpc::window::Icon { width: icon.width, height: icon.height, rgba: icon.rgba.into_owned() }
}

/// Win32 tweaks that Electron has no API for but the window still supports.
#[cfg(windows)]
pub(crate) mod imp {
  use tauri_runtime::ResizeDirection;
  use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
  use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetCursorPos, GetWindowLongPtrW, GetWindowRect, IsWindow, IsZoomed,
    PostMessageW, SetWindowLongPtrW, SetWindowPos, ShowWindowAsync, GWL_EXSTYLE, GWL_STYLE,
    HTCAPTION, HWND_BOTTOM, HWND_NOTOPMOST, HWND_TOPMOST, SC_MOVE, SC_SIZE, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_MAXIMIZE, SW_RESTORE, WMSZ_BOTTOM,
    WMSZ_BOTTOMLEFT, WMSZ_BOTTOMRIGHT, WMSZ_LEFT, WMSZ_RIGHT, WMSZ_TOP, WMSZ_TOPLEFT,
    WMSZ_TOPRIGHT, WM_CANCELMODE, WM_SYSCOMMAND, WS_CAPTION, WS_CHILD, WS_EX_TOPMOST,
    WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU, WS_THICKFRAME,
  };

  fn hwnd(handle: isize) -> HWND {
    HWND(handle as *mut std::ffi::c_void)
  }

  pub(crate) fn set_theme(handle: isize, dark: bool) -> tauri_runtime::Result<()> {
    use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
    let dark = windows::core::BOOL::from(dark);
    // SAFETY: DWM copies this BOOL synchronously for the borrowed Electron HWND.
    unsafe {
      DwmSetWindowAttribute(
        hwnd(handle),
        DWMWA_USE_IMMERSIVE_DARK_MODE,
        &dark as *const _ as *const std::ffi::c_void,
        std::mem::size_of_val(&dark) as u32,
      )
    }
    .map_err(|error| super::create_window_err(error.to_string()))
  }

  /// Muda needs an HWND owned by this UI thread to install its menu subclass.
  #[derive(Debug)]
  pub(crate) struct MenuOwner {
    pub(crate) handle: isize,
    pub(crate) mirrored: serde_json::Value,
  }

  thread_local! {
    static MENU_ACTIONS: std::cell::RefCell<Vec<(isize, &'static str)>> = const { std::cell::RefCell::new(Vec::new()) };
  }

  unsafe extern "system" fn menu_owner_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
  ) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::{
      Foundation::LRESULT,
      UI::WindowsAndMessaging::{
        DefWindowProcW, SIZE_MAXIMIZED, SIZE_MINIMIZED, SWP_SHOWWINDOW, WINDOWPOS, WM_CLOSE,
        WM_SIZE, WM_WINDOWPOSCHANGING,
      },
    };
    let action = match message {
      WM_CLOSE => Some("window.close"),
      WM_SIZE if wparam.0 == SIZE_MINIMIZED as usize => Some("window.minimize"),
      WM_SIZE if wparam.0 == SIZE_MAXIMIZED as usize => Some("window.maximize"),
      _ => None,
    };
    if let Some(action) = action {
      MENU_ACTIONS.with(|actions| actions.borrow_mut().push((window.0 as isize, action)));
    }
    if message == WM_CLOSE {
      return LRESULT(0);
    }
    if message == WM_WINDOWPOSCHANGING {
      // Predefined muda items call ShowWindow on the proxy; mirror the action
      // while keeping its HWND invisible and unable to steal focus.
      if let Some(position) = unsafe { (lparam.0 as *mut WINDOWPOS).as_mut() } {
        position.flags = (position.flags & !SWP_SHOWWINDOW) | SWP_NOACTIVATE;
      }
    }
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
  }

  impl MenuOwner {
    pub(crate) fn new() -> Option<Self> {
      use windows::Win32::{
        Foundation::HINSTANCE,
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::{
          CreateWindowExW, RegisterClassW, WINDOW_EX_STYLE, WNDCLASSW, WS_POPUP,
        },
      };
      static CLASS: std::sync::OnceLock<u16> = std::sync::OnceLock::new();
      let instance = HINSTANCE(unsafe { GetModuleHandleW(None) }.ok()?.0);
      let class = windows::core::w!("TauriElectronMenuOwner");
      if *CLASS.get_or_init(|| unsafe {
        RegisterClassW(&WNDCLASSW {
          lpfnWndProc: Some(menu_owner_proc),
          hInstance: instance,
          lpszClassName: class,
          ..Default::default()
        })
      }) == 0
      {
        return None;
      }
      let window = unsafe {
        CreateWindowExW(
          WINDOW_EX_STYLE(0),
          class,
          windows::core::w!(""),
          WS_POPUP,
          0,
          0,
          1,
          1,
          None,
          None,
          Some(instance),
          None,
        )
      }
      .ok()?;
      Some(Self {
        handle: window.0 as isize,
        mirrored: serde_json::Value::Null,
      })
    }
  }

  #[test]
  fn predefined_menu_actions_keep_proxy_hidden_and_repeatable() {
    use windows::Win32::UI::WindowsAndMessaging::{
      DestroyWindow, IsWindowVisible, SendMessageW, ShowWindow, SW_MINIMIZE, WM_CLOSE,
    };
    let owner = MenuOwner::new().unwrap();
    let _ = take_menu_actions();
    for command in [SW_MINIMIZE, SW_MAXIMIZE, SW_MINIMIZE, SW_MAXIMIZE] {
      unsafe {
        let _ = ShowWindow(hwnd(owner.handle), command);
      }
      let method = if command == SW_MINIMIZE {
        "window.minimize"
      } else {
        "window.maximize"
      };
      assert_eq!(take_menu_actions(), [(owner.handle, method)]);
      assert!(!unsafe { IsWindowVisible(hwnd(owner.handle)) }.as_bool());
    }
    unsafe {
      SendMessageW(
        hwnd(owner.handle),
        WM_CLOSE,
        Some(WPARAM(0)),
        Some(LPARAM(0)),
      );
    }
    assert_eq!(take_menu_actions(), [(owner.handle, "window.close")]);
    assert!(unsafe { IsWindow(Some(hwnd(owner.handle))) }.as_bool());
    // No muda object references this test proxy.
    unsafe { DestroyWindow(hwnd(owner.handle)) }.unwrap();
  }

  // ponytail: retain closed menu HWNDs until process exit because muda still
  // retains their associations; destroy after a future menu-lifetime detach hook.

  pub(crate) fn take_menu_actions() -> Vec<(isize, &'static str)> {
    use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_SHOWNOACTIVATE};
    let actions = MENU_ACTIONS.with(|actions| std::mem::take(&mut *actions.borrow_mut()));
    for (owner, action) in &actions {
      if *action != "window.close" {
        unsafe {
          let _ = ShowWindow(hwnd(*owner), SW_SHOWNOACTIVATE);
          let _ = SetWindowPos(
            hwnd(*owner),
            None,
            0,
            0,
            1,
            1,
            SWP_NOACTIVATE | SWP_NOZORDER,
          );
        }
      }
    }
    actions
  }

  pub(crate) fn menu_snapshot(handle: isize) -> std::result::Result<serde_json::Value, String> {
    use windows::Win32::UI::WindowsAndMessaging::{
      GetMenu, GetMenuItemCount, GetMenuItemInfoW, GetMenuStringW, HMENU, MENUITEMINFOW,
      MFS_CHECKED, MFS_DISABLED, MFT_SEPARATOR, MF_BYPOSITION, MIIM_FTYPE, MIIM_ID, MIIM_STATE,
      MIIM_SUBMENU,
    };
    fn items(menu: HMENU, depth: usize) -> std::result::Result<serde_json::Value, String> {
      if depth > 64 {
        return Err("menu nesting exceeds 64 levels".into());
      }
      let count = unsafe { GetMenuItemCount(Some(menu)) };
      if count < 0 {
        return Err("could not read native menu".into());
      }
      let mut result = Vec::new();
      for index in 0..count as u32 {
        let mut info = MENUITEMINFOW {
          cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
          fMask: MIIM_FTYPE | MIIM_ID | MIIM_STATE | MIIM_SUBMENU,
          ..Default::default()
        };
        // SAFETY: read muda's live menu on the UI thread that owns its proxy.
        unsafe { GetMenuItemInfoW(menu, index, true, &mut info) }
          .map_err(|error| error.to_string())?;
        if info.fType.0 & MFT_SEPARATOR.0 != 0 {
          result.push(serde_json::json!({ "type": "separator" }));
          continue;
        }
        let length = unsafe { GetMenuStringW(menu, index, None, MF_BYPOSITION) };
        let mut text = vec![0; length.max(0) as usize + 1];
        let length = unsafe { GetMenuStringW(menu, index, Some(&mut text), MF_BYPOSITION) };
        let text = String::from_utf16_lossy(&text[..length.max(0) as usize]);
        let (label, accelerator) = text.split_once('\t').unwrap_or((&text, ""));
        let mut item = serde_json::json!({
          "commandId": info.wID, "label": label, "accelerator": accelerator,
          "enabled": info.fState.0 & MFS_DISABLED.0 == 0,
          "checked": info.fState.0 & MFS_CHECKED.0 != 0,
        });
        if !info.hSubMenu.0.is_null() {
          item["submenu"] = items(info.hSubMenu, depth + 1)?;
        }
        result.push(item);
      }
      Ok(serde_json::Value::Array(result))
    }
    let menu = unsafe { GetMenu(hwnd(handle)) };
    if menu.0.is_null() {
      Ok(serde_json::Value::Null)
    } else {
      items(menu, 0)
    }
  }

  pub(crate) fn dispatch_menu_command(handle: isize, command: u32) {
    use windows::Win32::UI::WindowsAndMessaging::{SendMessageW, WM_COMMAND};
    // SAFETY: the runtime validates this against the current native menu before
    // invoking muda. No RefCell borrow spans its potentially reentrant callback.
    unsafe {
      SendMessageW(
        hwnd(handle),
        WM_COMMAND,
        Some(WPARAM(command as usize)),
        Some(LPARAM(0)),
      );
    }
  }

  pub(crate) fn client_origin(
    handle: isize,
  ) -> tauri_runtime::Result<tauri_runtime::dpi::PhysicalPosition<i32>> {
    let mut point = POINT::default();
    // SAFETY: copies the live Electron HWND's client origin into a stack value.
    if !unsafe { windows::Win32::Graphics::Gdi::ClientToScreen(hwnd(handle), &mut point) }.as_bool()
    {
      return Err(super::create_window_err(
        "could not locate cursor's window client area".into(),
      ));
    }
    Ok(tauri_runtime::dpi::PhysicalPosition::new(point.x, point.y))
  }

  pub(crate) fn clip_cursor(handle: isize, grab: bool) -> tauri_runtime::Result<()> {
    use windows::Win32::{Graphics::Gdi::ClientToScreen, UI::WindowsAndMessaging::ClipCursor};
    // SAFETY: only copies the live client rectangle and applies an OS clip.
    unsafe {
      if !grab {
        return ClipCursor(None).map_err(|error| super::create_window_err(error.to_string()));
      }
      let mut rect = RECT::default();
      GetClientRect(hwnd(handle), &mut rect)
        .map_err(|error| super::create_window_err(error.to_string()))?;
      let mut top_left = POINT {
        x: rect.left,
        y: rect.top,
      };
      let mut bottom_right = POINT {
        x: rect.right,
        y: rect.bottom,
      };
      if !ClientToScreen(hwnd(handle), &mut top_left).as_bool()
        || !ClientToScreen(hwnd(handle), &mut bottom_right).as_bool()
      {
        return Err(super::create_window_err(
          "could not locate cursor confinement area".into(),
        ));
      }
      ClipCursor(Some(&RECT {
        left: top_left.x,
        top: top_left.y,
        right: bottom_right.x,
        bottom: bottom_right.y,
      }))
      .map_err(|error| super::create_window_err(error.to_string()))
    }
  }

  pub(crate) fn validate_window_handle(handle: isize) -> tauri_runtime::Result<()> {
    if handle == 0 || !unsafe { IsWindow(Some(hwnd(handle))) }.as_bool() {
      return Err(super::create_window_err(
        "invalid native window handle".into(),
      ));
    }
    Ok(())
  }

  pub(crate) fn set_owner(handle: isize, owner: isize) -> tauri_runtime::Result<()> {
    use windows::Win32::{
      Foundation::{GetLastError, SetLastError, WIN32_ERROR},
      UI::WindowsAndMessaging::GWLP_HWNDPARENT,
    };
    validate_window_handle(handle)?;
    validate_window_handle(owner)?;
    // SAFETY: HWND ownership changes do not transfer memory; zero may also be a
    // successful previous owner, so only GetLastError determines failure.
    unsafe {
      SetLastError(WIN32_ERROR(0));
      SetWindowLongPtrW(hwnd(handle), GWLP_HWNDPARENT, owner);
      let error = GetLastError();
      if error.0 != 0 {
        return Err(super::create_window_err(
          std::io::Error::from_raw_os_error(error.0 as i32).to_string(),
        ));
      }
    }
    Ok(())
  }

  pub(crate) fn set_parent(handle: isize, parent: isize) -> tauri_runtime::Result<()> {
    use windows::Win32::{
      Foundation::{GetLastError, SetLastError, WIN32_ERROR},
      UI::WindowsAndMessaging::{SetParent, GWL_STYLE},
    };
    validate_window_handle(handle)?;
    validate_window_handle(parent)?;
    let window = hwnd(handle);
    // SetParent changes the relationship but not the styles. Convert the popup
    // to a real child while leaving every unrelated capability bit untouched.
    unsafe {
      SetLastError(WIN32_ERROR(0));
      let style = GetWindowLongPtrW(window, GWL_STYLE) as u32;
      let error = GetLastError();
      if error.0 != 0 {
        return Err(super::create_window_err(
          std::io::Error::from_raw_os_error(error.0 as i32).to_string(),
        ));
      }
      SetLastError(WIN32_ERROR(0));
      SetWindowLongPtrW(
        window,
        GWL_STYLE,
        (style & !WS_POPUP.0 | WS_CHILD.0) as isize,
      );
      let error = GetLastError();
      if error.0 != 0 {
        return Err(super::create_window_err(
          std::io::Error::from_raw_os_error(error.0 as i32).to_string(),
        ));
      }
      SetLastError(WIN32_ERROR(0));
      let _ = SetParent(window, Some(hwnd(parent)));
      let error = GetLastError();
      if error.0 != 0 {
        return Err(super::create_window_err(
          std::io::Error::from_raw_os_error(error.0 as i32).to_string(),
        ));
      }
      SetWindowPos(
        window,
        None,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
      )
      .map_err(|error| super::create_window_err(error.to_string()))?;
    }
    Ok(())
  }

  #[test]
  fn native_parent_is_a_child_and_invalid_handles_are_rejected() {
    use windows::Win32::UI::WindowsAndMessaging::{DestroyWindow, GetParent, GetWindow, GW_OWNER};
    let parent = MenuOwner::new().unwrap();
    let child = MenuOwner::new().unwrap();
    set_parent(child.handle, parent.handle).unwrap();
    let style = unsafe { GetWindowLongPtrW(hwnd(child.handle), GWL_STYLE) } as u32;
    assert_ne!(style & WS_CHILD.0, 0);
    assert_eq!(style & WS_POPUP.0, 0);
    assert_eq!(
      unsafe { GetParent(hwnd(child.handle)) }.unwrap().0 as isize,
      parent.handle
    );
    assert!(set_parent(child.handle, 0).is_err());

    let owner = MenuOwner::new().unwrap();
    set_owner(owner.handle, parent.handle).unwrap();
    let owner_style = unsafe { GetWindowLongPtrW(hwnd(owner.handle), GWL_STYLE) } as u32;
    assert_eq!(owner_style & WS_CHILD.0, 0);
    assert_eq!(
      unsafe { GetWindow(hwnd(owner.handle), GW_OWNER) }
        .unwrap()
        .0 as isize,
      parent.handle
    );
    assert!(set_owner(owner.handle, 0).is_err());
    unsafe {
      DestroyWindow(hwnd(child.handle)).unwrap();
      DestroyWindow(hwnd(owner.handle)).unwrap();
      DestroyWindow(hwnd(parent.handle)).unwrap();
    }
  }

  pub(super) fn is_maximized(handle: isize) -> tauri_runtime::Result<bool> {
    let window = hwnd(handle);
    // SAFETY: query only; an invalid HWND must not look like a restored window.
    unsafe {
      if !IsWindow(Some(window)).as_bool() {
        return Err(tauri_runtime::Error::WindowNotFound);
      }
      Ok(IsZoomed(window).as_bool())
    }
  }

  pub(super) fn set_maximized(handle: isize, maximized: bool) -> tauri_runtime::Result<()> {
    let window = hwnd(handle);
    // SAFETY: enqueue on Electron's owning UI thread instead of its widget cache.
    unsafe {
      if !IsWindow(Some(window)).as_bool() {
        return Err(tauri_runtime::Error::WindowNotFound);
      }
      // Do not read IsZoomed here: an earlier maximize may still be queued on
      // the owner thread. Its restore must remain behind it in the same queue.
      if ShowWindowAsync(window, if maximized { SW_MAXIMIZE } else { SW_RESTORE }).as_bool() {
        Ok(())
      } else {
        Err(tauri_runtime::Error::FailedToSendMessage)
      }
    }
  }

  pub(super) fn constrained_client_size(
    size: tauri_runtime::dpi::Size,
    geometry: &crate::rpc::window::WindowGeometry,
  ) -> tauri_runtime::Result<tauri_runtime::dpi::PhysicalSize<u32>> {
    let scale = geometry.scale_factor;
    if !scale.is_finite() || scale <= 0.0 {
      return Err(super::create_window_err(
        "invalid window scale factor".into(),
      ));
    }
    let size = size.to_physical::<u32>(scale);
    if size.width == 0
      || size.height == 0
      || size.width > i32::MAX as u32
      || size.height > i32::MAX as u32
    {
      return Err(super::create_window_err(
        "invalid or overflowing window size".into(),
      ));
    }
    let limit = |key: crate::rpc::window::ConstraintAxis| -> tauri_runtime::Result<u32> {
      let value = geometry.content_constraints.get(&key)
        .map(|value| (value * scale).round())
        .filter(|value| value.is_finite() && *value >= 0.0 && *value <= f64::from(i32::MAX))
        .ok_or_else(|| {
          super::create_window_err("invalid or overflowing window size constraint".into())
        })?;
      Ok(value as u32)
    };
    let clamp = |value: u32, minimum: crate::rpc::window::ConstraintAxis, maximum: crate::rpc::window::ConstraintAxis| -> tauri_runtime::Result<u32> {
      let minimum = limit(minimum)?;
      let maximum = limit(maximum)?;
      Ok(
        if maximum > 0 {
          value.min(maximum)
        } else {
          value
        }
        .max(minimum),
      )
    };
    Ok(tauri_runtime::dpi::PhysicalSize::new(
      clamp(size.width, crate::rpc::window::ConstraintAxis::MinWidth, crate::rpc::window::ConstraintAxis::MaxWidth)?,
      clamp(size.height, crate::rpc::window::ConstraintAxis::MinHeight, crate::rpc::window::ConstraintAxis::MaxHeight)?,
    ))
  }

  fn outer_size_for_client(
    size: tauri_runtime::dpi::PhysicalSize<u32>,
    outer: RECT,
    client: RECT,
  ) -> Option<(i32, i32)> {
    let dimension = |requested: u32, outer: i64, client: i64| {
      if requested == 0 || outer <= 0 || client < 0 || outer < client {
        return None;
      }
      i32::try_from(i64::from(requested) + outer - client).ok()
    };
    Some((
      dimension(
        size.width,
        i64::from(outer.right) - i64::from(outer.left),
        i64::from(client.right) - i64::from(client.left),
      )?,
      dimension(
        size.height,
        i64::from(outer.bottom) - i64::from(outer.top),
        i64::from(client.bottom) - i64::from(client.top),
      )?,
    ))
  }

  pub(super) fn set_client_size(
    handle: isize,
    size: tauri_runtime::dpi::PhysicalSize<u32>,
  ) -> tauri_runtime::Result<()> {
    let window = hwnd(handle);
    let mut outer = RECT::default();
    let mut client = RECT::default();
    // SAFETY: live pixel rectangles include native menu/frame insets that
    // Electron's content-to-window conversion cannot see for Rust-owned menus.
    unsafe {
      GetWindowRect(window, &mut outer).map_err(|_| tauri_runtime::Error::WindowNotFound)?;
      GetClientRect(window, &mut client).map_err(|_| tauri_runtime::Error::WindowNotFound)?;
      let (width, height) = outer_size_for_client(size, outer, client)
        .ok_or_else(|| super::create_window_err("invalid or overflowing window size".into()))?;
      SetWindowPos(
        window,
        None,
        0,
        0,
        width,
        height,
        SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
      )
      .map_err(|_| tauri_runtime::Error::FailedToSendMessage)
    }
  }

  #[test]
  fn native_client_size_honors_current_content_constraints() {
    use crate::rpc::window::{ConstraintAxis, WindowGeometry};
    use tauri_runtime::dpi::{LogicalSize, PhysicalSize, Size};
    let mut geometry: WindowGeometry = serde_json::from_value(serde_json::json!({
      "scaleFactor": 1.25, "contentConstraints": {
        "minWidth": 300, "minHeight": 320, "maxWidth": 800, "maxHeight": 640,
      },
      "bounds": {"x": 0, "y": 0, "width": 100, "height": 100},
      "contentBounds": {"x": 0, "y": 0, "width": 100, "height": 100},
      "maximized": false, "minimized": false, "focused": false, "visible": true,
      "fullscreen": false, "resizable": true, "alwaysOnTop": false, "decorated": true,
      "title": "constraints", "maximizable": true, "minimizable": true, "closable": true,
    })).unwrap();
    let physical = Size::Physical(PhysicalSize::new(1200, 100));
    assert_eq!(
      constrained_client_size(physical, &geometry).unwrap(),
      PhysicalSize::new(1000, 400)
    );
    assert_eq!(
      constrained_client_size(Size::Logical(LogicalSize::new(400.0, 400.0)), &geometry).unwrap(),
      PhysicalSize::new(500, 500)
    );
    geometry.content_constraints.values_mut().for_each(|limit| *limit = 0.0);
    assert_eq!(
      constrained_client_size(physical, &geometry).unwrap(),
      PhysicalSize::new(1200, 100)
    );
    geometry.content_constraints.insert(ConstraintAxis::MinHeight, -1.0);
    assert!(constrained_client_size(physical, &geometry).is_err());
    geometry.scale_factor = 0.0;
    assert!(constrained_client_size(physical, &geometry).is_err());
  }

  #[test]
  fn client_size_keeps_live_native_insets() {
    use tauri_runtime::dpi::PhysicalSize;
    let outer = RECT {
      left: -1000,
      top: -200,
      right: 222,
      bottom: 758,
    };
    let client = RECT {
      left: 0,
      top: 0,
      right: 1200,
      bottom: 872,
    };
    assert_eq!(
      outer_size_for_client(PhysicalSize::new(1440, 1200), outer, client),
      Some((1462, 1286))
    );
    assert_eq!(
      outer_size_for_client(PhysicalSize::new(1440, 1200), client, client),
      Some((1440, 1200))
    );
    assert_eq!(
      outer_size_for_client(PhysicalSize::new(0, 1200), outer, client),
      None
    );
    assert_eq!(
      outer_size_for_client(PhysicalSize::new(u32::MAX, 1200), outer, client),
      None
    );
    assert_eq!(
      outer_size_for_client(PhysicalSize::new(1440, 1200), client, outer),
      None
    );
  }

  pub(crate) fn send_to_bottom(handle: isize) {
    // SAFETY: z-order only, no geometry or focus change.
    unsafe {
      let _ = SetWindowPos(
        hwnd(handle),
        Some(HWND_BOTTOM),
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
      );
    }
  }

  pub(super) fn set_always_on_top(handle: isize, always_on_top: bool) -> tauri_runtime::Result<()> {
    // SAFETY: z-order only, no geometry or focus change.
    unsafe {
      SetWindowPos(
        hwnd(handle),
        Some(if always_on_top {
          HWND_TOPMOST
        } else {
          HWND_NOTOPMOST
        }),
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
      )
      .map_err(|_| tauri_runtime::Error::FailedToSendMessage)
    }
  }

  pub(super) fn set_position(
    handle: isize,
    position: tauri_runtime::dpi::PhysicalPosition<i32>,
  ) -> tauri_runtime::Result<()> {
    // SAFETY: a move must not round-trip the live size through Chromium's DIP
    // conversion, especially when native menu/frame insets are present.
    unsafe {
      SetWindowPos(
        hwnd(handle),
        None,
        position.x,
        position.y,
        0,
        0,
        SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
      )
      .map_err(|_| tauri_runtime::Error::FailedToSendMessage)
    }
  }

  pub(super) fn is_always_on_top(handle: isize) -> bool {
    let style = unsafe { GetWindowLongPtrW(hwnd(handle), GWL_EXSTYLE) } as u32;
    style & WS_EX_TOPMOST.0 != 0
  }

  pub(super) fn set_decorations(handle: isize, decorations: bool) -> tauri_runtime::Result<()> {
    use windows::Win32::Foundation::{GetLastError, SetLastError, WIN32_ERROR};
    let window = hwnd(handle);
    // SAFETY: HWND styles are integer values, not process-local pointers. Zero
    // can be a successful Get/SetWindowLongPtr result, so clear/check last error.
    unsafe {
      SetLastError(WIN32_ERROR(0));
      let original = GetWindowLongPtrW(window, GWL_STYLE) as u32;
      let error = GetLastError();
      if error.0 != 0 {
        return Err(super::create_window_err(
          std::io::Error::from_raw_os_error(error.0 as i32).to_string(),
        ));
      }
      // Keep THICKFRAME, MIN/MAXIMIZEBOX and SYSMENU (including SC_CLOSE state):
      // removing decorations must not rewrite independent native capabilities.
      let style = if decorations {
        original | WS_CAPTION.0
      } else {
        original & !WS_CAPTION.0
      };
      SetLastError(WIN32_ERROR(0));
      SetWindowLongPtrW(window, GWL_STYLE, style as isize);
      let error = GetLastError();
      if error.0 != 0 {
        return Err(super::create_window_err(
          std::io::Error::from_raw_os_error(error.0 as i32).to_string(),
        ));
      }
      SetWindowPos(
        window,
        None,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
      )
      .map_err(|error| super::create_window_err(error.to_string()))?;
      SetLastError(WIN32_ERROR(0));
      let applied = GetWindowLongPtrW(window, GWL_STYLE) as u32;
      let error = GetLastError();
      if error.0 != 0 {
        return Err(super::create_window_err(
          std::io::Error::from_raw_os_error(error.0 as i32).to_string(),
        ));
      }
      let checked = WS_CAPTION | WS_THICKFRAME | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX;
      if applied & checked.0 != style & checked.0 {
        return Err(super::create_window_err(
          "native window did not retain its frame and capability styles".into(),
        ));
      }
    }
    Ok(())
  }

  #[test]
  fn decorations_preserve_native_capability_styles_and_reject_stale_handles() {
    use windows::Win32::UI::WindowsAndMessaging::DestroyWindow;
    let owner = MenuOwner::new().unwrap();
    let window = hwnd(owner.handle);
    let capabilities = WS_THICKFRAME | WS_MINIMIZEBOX | WS_MAXIMIZEBOX;
    for enabled in [false, true] {
      let original = unsafe { GetWindowLongPtrW(window, GWL_STYLE) } as u32;
      let style = (original & !capabilities.0)
        | WS_CAPTION.0
        | WS_SYSMENU.0
        | if enabled { capabilities.0 } else { 0 };
      unsafe {
        SetWindowLongPtrW(window, GWL_STYLE, style as isize);
      }
      for decorated in [false, true, false, true] {
        set_decorations(owner.handle, decorated).unwrap();
        let applied = unsafe { GetWindowLongPtrW(window, GWL_STYLE) } as u32;
        assert_eq!(applied & capabilities.0, style & capabilities.0);
        assert_eq!(applied & WS_SYSMENU.0, style & WS_SYSMENU.0);
        assert_eq!(
          applied & WS_CAPTION.0,
          if decorated { WS_CAPTION.0 } else { 0 }
        );
      }
    }
    // No muda object references this test proxy.
    unsafe { DestroyWindow(window) }.unwrap();
    assert!(set_decorations(owner.handle, true).is_err());
  }

  fn screen_position_lparam(point: POINT) -> LPARAM {
    // Mouse-initiated WM_SYSCOMMAND packs signed screen coordinates into 16-bit words.
    LPARAM(((point.x as u16 as u32) | ((point.y as u16 as u32) << 16)) as isize)
  }

  fn start_non_client_drag(handle: isize, command: u32) -> tauri_runtime::Result<()> {
    let mut point = POINT::default();
    // SAFETY: both messages contain only integer values. Cancel capture on the
    // owning Electron UI thread, then enter the OS move/resize loop directly;
    // Chromium may consume a synthetic WM_NCLBUTTONDOWN as a page mouse event.
    unsafe {
      GetCursorPos(&mut point).map_err(|_| tauri_runtime::Error::FailedToGetCursorPosition)?;
      PostMessageW(Some(hwnd(handle)), WM_CANCELMODE, WPARAM(0), LPARAM(0))
        .map_err(|_| tauri_runtime::Error::FailedToSendMessage)?;
      PostMessageW(
        Some(hwnd(handle)),
        WM_SYSCOMMAND,
        WPARAM(command as usize),
        screen_position_lparam(point),
      )
      .map_err(|_| tauri_runtime::Error::FailedToSendMessage)
    }
  }

  pub(super) fn start_dragging(handle: isize) -> tauri_runtime::Result<()> {
    start_non_client_drag(handle, SC_MOVE | HTCAPTION)
  }

  /// Resizes the way dragging a frame edge does, by handing the window the
  /// matching non-client resize code.
  pub(super) fn start_resize_dragging(
    handle: isize,
    direction: ResizeDirection,
  ) -> tauri_runtime::Result<()> {
    let edge = match direction {
      ResizeDirection::East => WMSZ_RIGHT,
      ResizeDirection::North => WMSZ_TOP,
      ResizeDirection::NorthEast => WMSZ_TOPRIGHT,
      ResizeDirection::NorthWest => WMSZ_TOPLEFT,
      ResizeDirection::South => WMSZ_BOTTOM,
      ResizeDirection::SouthEast => WMSZ_BOTTOMRIGHT,
      ResizeDirection::SouthWest => WMSZ_BOTTOMLEFT,
      ResizeDirection::West => WMSZ_LEFT,
    };
    start_non_client_drag(handle, SC_SIZE | edge)
  }

  #[test]
  fn packs_signed_screen_coordinates() {
    for (x, y, expected) in [
      (0, 0, 0),
      (-1, 1, 0x0001_ffff),
      (1, -1, 0xffff_0001),
      (-32768, 32767, 0x7fff_8000),
      (32767, -32768, 0x8000_7fff),
    ] {
      let packed = screen_position_lparam(POINT { x, y }).0 as u32;
      assert_eq!(packed, expected);
      assert_eq!(packed as i16 as i32, x);
      assert_eq!((packed >> 16) as i16 as i32, y);
    }
  }
}

/// The page owns the cursor over its own content, which is where a user sees it,
/// so a Tauri cursor icon becomes a CSS `cursor` value.
fn css_cursor(icon: CursorIcon) -> &'static str {
  match icon {
    CursorIcon::Default | CursorIcon::Arrow => "default",
    CursorIcon::Crosshair => "crosshair",
    CursorIcon::Hand => "pointer",
    CursorIcon::Move => "move",
    CursorIcon::Text => "text",
    CursorIcon::Wait => "wait",
    CursorIcon::Help => "help",
    CursorIcon::Progress => "progress",
    CursorIcon::NotAllowed => "not-allowed",
    CursorIcon::ContextMenu => "context-menu",
    CursorIcon::Cell => "cell",
    CursorIcon::VerticalText => "vertical-text",
    CursorIcon::Alias => "alias",
    CursorIcon::Copy => "copy",
    CursorIcon::NoDrop => "no-drop",
    CursorIcon::Grab => "grab",
    CursorIcon::Grabbing => "grabbing",
    CursorIcon::AllScroll => "all-scroll",
    CursorIcon::ZoomIn => "zoom-in",
    CursorIcon::ZoomOut => "zoom-out",
    CursorIcon::EResize => "e-resize",
    CursorIcon::NResize => "n-resize",
    CursorIcon::NeResize => "ne-resize",
    CursorIcon::NwResize => "nw-resize",
    CursorIcon::SResize => "s-resize",
    CursorIcon::SeResize => "se-resize",
    CursorIcon::SwResize => "sw-resize",
    CursorIcon::WResize => "w-resize",
    CursorIcon::EwResize => "ew-resize",
    CursorIcon::NsResize => "ns-resize",
    CursorIcon::NeswResize => "nesw-resize",
    CursorIcon::NwseResize => "nwse-resize",
    CursorIcon::ColResize => "col-resize",
    CursorIcon::RowResize => "row-resize",
    // The enum is non-exhaustive; a cursor Tauri adds later still points.
    _ => "default",
  }
}
