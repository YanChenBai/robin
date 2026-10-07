use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, rgb};

pub fn install(cx: &mut App) {
    Theme::change(ThemeMode::Dark, None, cx);
    Theme::update(cx, |theme| {
        theme.colors.background = rgb(0x101014).into();
        theme.colors.foreground = rgb(0xeeeef3).into();
        theme.colors.primary = rgb(0xa78bfa).into();
        theme.colors.primary_foreground = rgb(0x100b18).into();
        theme.colors.primary_hover = rgb(0xc4b5fd).into();
        theme.colors.primary_active = rgb(0x8b5cf6).into();
        theme.colors.muted = rgb(0x22222b).into();
        theme.colors.muted_foreground = rgb(0x9898a8).into();
        theme.colors.border = rgb(0x30303b).into();
        theme.colors.group_box = rgb(0x19191f).into();
        theme.colors.popover = rgb(0x17141f).into();
        theme.colors.sidebar = rgb(0x0e0c13).into();
        theme.colors.input = rgb(0x30303b).into();
    });
}
