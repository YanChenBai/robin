use anyhow::Result;
use gpui_kit::{App, Window};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tray_icon::{
    Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};
use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_RESTORE, ShowWindow};

pub enum TrayCommand {
    Show,
    Disconnect,
    Quit,
}

pub struct Tray {
    icon: TrayIcon,
    show: MenuItem,
    disconnect: MenuItem,
    quit: MenuItem,
    tooltip: String,
}

impl Tray {
    pub fn new() -> Result<Self> {
        let menu = Menu::new();
        let show = MenuItem::new("打开 Robin", true, None);
        let disconnect = MenuItem::new("断开连接", false, None);
        let quit = MenuItem::new("退出 Robin", true, None);
        menu.append_items(&[&show, &disconnect, &PredefinedMenuItem::separator(), &quit])?;
        let icon = TrayIconBuilder::new()
            .with_icon(Icon::from_resource(1, Some((32, 32)))?)
            .with_tooltip("Robin · 等待连接")
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()?;
        Ok(Self {
            icon,
            show,
            disconnect,
            quit,
            tooltip: String::new(),
        })
    }

    pub fn poll(&self) -> Vec<TrayCommand> {
        let mut commands = Vec::new();
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if event.id == *self.show.id() {
                commands.push(TrayCommand::Show);
            }
            if event.id == *self.disconnect.id() {
                commands.push(TrayCommand::Disconnect);
            }
            if event.id == *self.quit.id() {
                commands.push(TrayCommand::Quit);
            }
        }
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
            ) {
                commands.push(TrayCommand::Show);
            }
        }
        commands
    }

    pub fn set_status(&mut self, status: &str, busy: bool) {
        self.disconnect.set_enabled(busy);
        let tooltip = format!("Robin · {status}");
        if self.tooltip != tooltip {
            let _ = self.icon.set_tooltip(Some(&tooltip));
            self.tooltip = tooltip;
        }
    }
}

pub fn hide(window: &Window) -> Result<()> {
    let handle = HasWindowHandle::window_handle(window)
        .map_err(|error| anyhow::anyhow!("无法隐藏窗口：{error}"))?;
    if let RawWindowHandle::Win32(handle) = handle.as_raw() {
        // GPUI 尚无隐藏单个窗口的接口，窗口仍保留以承载接收和托盘事件。
        unsafe {
            ShowWindow(handle.hwnd.get() as _, SW_HIDE);
        }
    }
    Ok(())
}

pub fn show(window: &Window, cx: &mut App) {
    if let Ok(handle) = HasWindowHandle::window_handle(window)
        && let RawWindowHandle::Win32(handle) = handle.as_raw()
    {
        unsafe {
            ShowWindow(handle.hwnd.get() as _, SW_RESTORE);
        }
    }
    window.activate_window();
    cx.activate(true);
}
