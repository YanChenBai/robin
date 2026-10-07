use crate::{
    capture,
    engine::{CaptureDevice, DesktopEngine, DesktopSnapshot},
    preferences::{self, Preferences},
    tray::{self, Tray, TrayCommand},
};
use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme, Disableable, Icon, IndexPath, Sizable,
    button::{Button, ButtonVariants},
    progress::Progress,
    select::{Select, SelectEvent, SelectState},
    spinner::Spinner,
    switch::Switch,
    tab::{Tab, TabBar},
    tag::Tag,
};
use gpui_kit::*;
use std::{path::PathBuf, time::Duration};

pub struct RobinView {
    engine: DesktopEngine,
    devices: Vec<CaptureDevice>,
    capture_select: Entity<SelectState<Vec<String>>>,
    preferences: Preferences,
    preferences_path: PathBuf,
    startup: bool,
    tray: Option<Tray>,
    page: usize,
    error: String,
    _subscription: Subscription,
    _refresh: Task<()>,
    _devices_refresh: Task<()>,
}

impl RobinView {
    pub fn new(
        engine: DesktopEngine,
        preferences_path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut error = String::new();
        let preferences = Preferences::load(&preferences_path).unwrap_or_else(|failure| {
            error = format!("无法读取设置：{failure}");
            Preferences::default()
        });
        *engine.selected_capture.lock().unwrap() = preferences.capture_id.clone();
        let devices = capture::devices().unwrap_or_else(|failure| {
            error = failure.to_string();
            Vec::new()
        });
        let initial = capture_index(&devices, &preferences.capture_id);
        let capture_select = cx.new(|cx| {
            SelectState::new(
                capture_labels(&devices),
                Some(IndexPath::new(initial)),
                window,
                cx,
            )
        });
        let subscription = cx.subscribe(&capture_select, |this, select, event, cx| {
            if let SelectEvent::Confirm(Some(_)) = event
                && let Some(ix) = select.read(cx).selected_index(cx)
            {
                let id = ix
                    .row
                    .checked_sub(1)
                    .and_then(|ix| this.devices.get(ix))
                    .map(|device| device.id.clone())
                    .unwrap_or_default();
                this.set_capture(id);
                cx.notify();
            }
        });
        let tray = Tray::new()
            .map_err(|failure| {
                error = format!("无法创建托盘：{failure}");
            })
            .ok();
        let startup = preferences::startup_enabled().unwrap_or_else(|failure| {
            error = format!("无法读取开机启动设置：{failure}");
            false
        });
        let owner = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            owner
                .update(cx, |this, _| {
                    if this.preferences.close_to_tray && this.tray.is_some() {
                        match tray::hide(window) {
                            Ok(()) => false,
                            Err(failure) => {
                                this.error = failure.to_string();
                                true
                            }
                        }
                    } else {
                        true
                    }
                })
                .unwrap_or(true)
        });
        let refresh = cx.spawn_in(window, async move |this, cx| {
            let mut previous = None;
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                if this
                    .update_in(cx, |this, window, cx| {
                        this.engine.poll_incoming();
                        if let Some(tray) = &mut this.tray {
                            let snapshot = this.engine.state.lock().unwrap().clone();
                            tray.set_status(state_label(&snapshot.state), is_busy(&snapshot.state));
                            for command in tray.poll() {
                                match command {
                                    TrayCommand::Show => tray::show(window, cx),
                                    TrayCommand::Disconnect => this.engine.disconnect(),
                                    TrayCommand::Quit => cx.quit(),
                                }
                            }
                        }
                        let snapshot = this.engine.state.lock().unwrap().clone();
                        let peers = this.engine.peers.lock().unwrap().clone();
                        let current = (snapshot, peers);
                        if previous.as_ref() != Some(&current) {
                            previous = Some(current);
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let devices_refresh = cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(2)).await;
                let (result, addresses) = cx
                    .background_executor()
                    .spawn(async { (capture::devices(), crate::engine::local_addresses()) })
                    .await;
                if this
                    .update_in(cx, |this, window, cx| {
                        this.apply_devices(result, window, cx);
                        if this.engine.addresses != addresses {
                            this.engine.addresses = addresses;
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        let mut view = Self {
            engine,
            devices,
            capture_select,
            preferences,
            preferences_path,
            startup,
            tray,
            page: 0,
            error,
            _subscription: subscription,
            _refresh: refresh,
            _devices_refresh: devices_refresh,
        };
        if !view.preferences.capture_id.is_empty() && initial == 0 && !view.devices.is_empty() {
            view.set_capture(String::new());
            view.error = "上次选择的输出设备不可用，已跟随系统默认设备。".into();
        }
        view
    }

    pub fn has_tray(&self) -> bool {
        self.tray.is_some()
    }

    fn set_capture(&mut self, id: String) {
        *self.engine.selected_capture.lock().unwrap() = id.clone();
        let mut next = self.preferences.clone();
        next.capture_id = id;
        match next.save(&self.preferences_path) {
            Ok(()) => {
                self.preferences = next;
                self.error.clear();
            }
            Err(failure) => {
                self.preferences = next;
                self.error = format!("无法保存声音来源：{failure}");
            }
        }
    }

    fn apply_devices(
        &mut self,
        result: anyhow::Result<Vec<CaptureDevice>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(devices) if devices != self.devices => {
                if !self.preferences.capture_id.is_empty()
                    && !devices
                        .iter()
                        .any(|device| device.id == self.preferences.capture_id)
                {
                    self.set_capture(String::new());
                    self.error = "输出设备已移除，已跟随系统默认设备。".into();
                }
                let ix = capture_index(&devices, &self.preferences.capture_id);
                self.capture_select.update(cx, |select, cx| {
                    select.set_items(capture_labels(&devices), window, cx);
                    select.set_selected_index(Some(IndexPath::new(ix)), window, cx);
                });
                self.devices = devices;
                cx.notify();
            }
            Err(failure) => {
                self.error = format!("无法读取输出设备：{failure}");
                cx.notify();
            }
            _ => {}
        }
    }

    fn refresh_devices(&mut self, window: &Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async { capture::devices() })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.apply_devices(result, window, cx)
            });
        })
        .detach();
    }

    fn render_session(&self, snapshot: &DesktopSnapshot, cx: &mut Context<Self>) -> AnyElement {
        let busy = is_busy(&snapshot.state);
        let streaming = snapshot.state == "streaming";
        let waiting = matches!(
            snapshot.state.as_str(),
            "connecting" | "pending" | "reconnecting"
        );
        let theme = cx.theme();
        let tag = if streaming {
            Tag::success()
        } else {
            Tag::secondary()
        };
        let mut header = div()
            .flex()
            .items_center()
            .gap_3()
            .child(if waiting {
                Spinner::new().small().into_any_element()
            } else {
                Icon::new(IconName::Headphones).into_any_element()
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(if busy {
                                snapshot.peer_name.clone()
                            } else {
                                "电脑声音，随身聆听".into()
                            }),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme.muted_foreground)
                            .child(if busy {
                                "48 kHz · 立体声 · 加密传输"
                            } else {
                                "连接一台手机，开始播放电脑声音。"
                            }),
                    ),
            )
            .child(tag.outline().small().child(state_label(&snapshot.state)));
        if busy {
            header = header.child(
                Button::new("disconnect")
                    .outline()
                    .small()
                    .icon(IconName::Unlink)
                    .label("断开")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.engine.disconnect();
                        cx.notify();
                    })),
            );
        }
        let mut session = div().flex().flex_col().gap_4().child(header).child(
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child("采集音量"),
                )
                .child(
                    Progress::new("capture-level")
                        .small()
                        .accessibility_label("采集音量")
                        .value(if streaming {
                            snapshot.audio_level * 100.0
                        } else {
                            0.0
                        })
                        .flex_1(),
                ),
        );
        if snapshot.state == "pending" && !snapshot.code.is_empty() {
            session = session.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .pt_3()
                    .border_t_1()
                    .border_color(theme.border)
                    .child(div().text_sm().child("在手机核对配对码，并允许连接"))
                    .child(
                        div()
                            .text_xl()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.primary)
                            .child(snapshot.code.clone()),
                    ),
            );
        }
        session.into_any_element()
    }

    fn render_playback(&self, snapshot: &DesktopSnapshot, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let busy = is_busy(&snapshot.state);
        let streaming = snapshot.state == "streaming";
        let source = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(section_title("声音来源"))
                    .child(
                        Button::new("refresh-devices")
                            .ghost()
                            .small()
                            .icon(IconName::RefreshCw)
                            .label("刷新")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.refresh_devices(window, cx)),
                            ),
                    ),
            )
            .child(Select::new(&self.capture_select).w_full())
            .child(div().text_xs().text_color(theme.muted_foreground).child(
                if self.preferences.capture_id.is_empty() {
                    "跟随系统默认输出；切换耳机或音箱时自动跟随。"
                } else {
                    "已固定声音来源；这个选择会在下次启动时保留。"
                },
            ));
        let peers = self
            .engine
            .peers
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut devices = div().flex().flex_col().gap_3().child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(section_title("附近的手机"))
                .child(
                    div()
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(format!("{} 台", peers.len())),
                ),
        );
        if peers.is_empty() {
            devices = devices.child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .py_4()
                    .child(Icon::new(IconName::Smartphone).text_color(theme.muted_foreground))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_sm().child("等待发现手机"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child("在同一网络的手机上打开 Robin，并开启接收。"),
                            ),
                    ),
            );
        }
        for peer in peers {
            let active = busy && snapshot.peer_id == peer.id;
            devices = devices.child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .py_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(Icon::new(IconName::Smartphone).text_color(theme.muted_foreground))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_sm().child(peer.name.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(peer.address.to_string()),
                            ),
                    )
                    .child(
                        Button::new(SharedString::from(format!("device-{}", peer.id)))
                            .outline()
                            .small()
                            .icon(IconName::Link)
                            .label(if active {
                                state_label(&snapshot.state)
                            } else {
                                "连接"
                            })
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.engine.connect(peer.clone());
                                cx.notify();
                            })),
                    ),
            );
        }
        let diagnostics = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(section_title("连接详情"))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .gap_4()
                            .child(metric(
                                "网络往返",
                                snapshot
                                    .rtt_ms
                                    .map(|value| format!("{value:.1} ms"))
                                    .unwrap_or_else(|| "—".into()),
                                cx,
                            ))
                            .child(metric(
                                "播放缓冲",
                                if streaming {
                                    format!("{} ms", snapshot.buffer_ms)
                                } else {
                                    "—".into()
                                },
                                cx,
                            ))
                            .child(metric(
                                "已发送数据包",
                                if streaming {
                                    snapshot.sent.to_string()
                                } else {
                                    "—".into()
                                },
                                cx,
                            )),
                    )
                    .child(div().text_xs().text_color(theme.muted_foreground).child(
                        if streaming {
                            format!(
                                "{} · 采集周期 {:.1} ms",
                                snapshot.capture_name, snapshot.capture_period_ms
                            )
                        } else {
                            "连接后显示实时指标；播放缓冲在手机端调整。".into()
                        },
                    )),
            );
        div()
            .flex()
            .flex_col()
            .gap_6()
            .child(self.render_session(snapshot, cx))
            .child(source)
            .child(devices)
            .child(diagnostics)
            .child(self.render_addresses(cx))
            .into_any_element()
    }

    fn render_addresses(&self, cx: &App) -> AnyElement {
        let theme = cx.theme();
        let mut addresses = div()
            .flex()
            .flex_col()
            .gap_2()
            .pt_4()
            .border_t_1()
            .border_color(theme.border)
            .child(section_title("手动连接"))
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child("未发现设备时，在手机输入下面的电脑地址。"),
            );
        for address in &self.engine.addresses {
            let value = address.clone();
            addresses = addresses.child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().flex_1().text_sm().child(value.clone()))
                    .child(
                        Button::new(SharedString::from(format!("copy-{address}")))
                            .ghost()
                            .small()
                            .icon(IconName::Copy)
                            .label("复制")
                            .on_click(move |_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(value.clone()))
                            }),
                    ),
            );
        }
        if self.engine.addresses.is_empty() {
            addresses = addresses.child(
                div()
                    .text_sm()
                    .text_color(theme.warning)
                    .child("没有可用的局域网地址，请检查网络。"),
            );
        }
        addresses.into_any_element()
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let startup = Switch::new("startup")
            .label("开机启动")
            .checked(self.startup)
            .on_change(cx.listener(|this, enabled, _, cx| {
                match preferences::set_startup(*enabled) {
                    Ok(()) => {
                        this.startup = *enabled;
                        this.error.clear();
                    }
                    Err(failure) => this.error = format!("无法设置开机启动：{failure}"),
                }
                cx.notify();
            }));
        let close_to_tray = Switch::new("close-to-tray")
            .label("关闭窗口后继续运行")
            .checked(self.preferences.close_to_tray)
            .disabled(self.tray.is_none())
            .on_change(cx.listener(|this, enabled, _, cx| {
                let mut next = this.preferences.clone();
                next.close_to_tray = *enabled;
                match next.save(&this.preferences_path) {
                    Ok(()) => {
                        this.preferences = next;
                        this.error.clear();
                    }
                    Err(failure) => this.error = format!("无法保存托盘设置：{failure}"),
                }
                cx.notify();
            }));
        div().flex().flex_col().gap_6()
            .child(div().flex().flex_col().gap_1().child(section_title("日常使用"))
                .child(div().text_sm().text_color(theme.muted_foreground).child("让电脑声音随时可以连接。")))
            .child(setting_row(startup, "登录 Windows 后启动 Robin；托盘可用时在后台运行。", cx))
            .child(setting_row(close_to_tray, "串流保持运行。点击托盘图标打开窗口，右键菜单可以退出。", cx))
            .child(div().flex().flex_col().gap_2().pt_4().border_t_1().border_color(theme.border)
                .child(section_title("自动连接"))
                .child(div().text_sm().text_color(theme.muted_foreground).child("在手机选择要自动连接的电脑。地址变化后，手机会在同一网络重新寻找已信任的电脑。"))
                .child(div().text_xs().text_color(theme.muted_foreground).child("首次配对需在手机确认；主动断开后不会自动连接。")))
            .child(div().flex().items_center().justify_between().pt_4().border_t_1().border_color(theme.border)
                .child(div().text_sm().text_color(theme.muted_foreground).child("Robin · 知更鸟"))
                .child(Button::new("quit").outline().icon(IconName::Power).label("退出 Robin").on_click(|_, _, cx| cx.quit())))
            .into_any_element()
    }
}

impl Render for RobinView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = self.engine.state.lock().unwrap().clone();
        let theme = cx.theme();
        let background = theme.background;
        let foreground = theme.foreground;
        let border = theme.border;
        let danger = theme.danger;
        let content = if self.page == 0 {
            self.render_playback(&snapshot, cx)
        } else {
            self.render_settings(cx)
        };
        let error = if self.error.is_empty() {
            snapshot.error.clone()
        } else {
            self.error.clone()
        };
        let mut workspace = div()
            .w_full()
            .max_w(rems(50.0))
            .mx_auto()
            .p_6()
            .flex()
            .flex_col()
            .gap_4();
        if !error.is_empty() {
            workspace = workspace.child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(div().flex_1().text_sm().text_color(danger).child(error))
                    .child(
                        Button::new("dismiss-error")
                            .ghost()
                            .small()
                            .icon(IconName::X)
                            .label("关闭提示")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.error.clear();
                                this.engine.state.lock().unwrap().error.clear();
                                cx.notify();
                            })),
                    ),
            );
        }
        workspace = workspace.child(content);
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(background)
            .text_color(foreground)
            .child(
                div()
                    .id(if self.page == 0 {
                        "playback-workspace"
                    } else {
                        "settings-workspace"
                    })
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(workspace),
            )
            .child(
                div().px_6().py_2().border_t_1().border_color(border).child(
                    TabBar::new("navigation")
                        .underline()
                        .w_full()
                        .selected_index(self.page)
                        .child(Tab::new().label("播放").icon(IconName::Headphones).flex_1())
                        .child(Tab::new().label("设置").icon(IconName::Settings).flex_1())
                        .on_click(cx.listener(|this, ix, _, cx| {
                            this.page = *ix;
                            cx.notify();
                        })),
                ),
            )
    }
}

fn capture_labels(devices: &[CaptureDevice]) -> Vec<String> {
    std::iter::once("跟随系统默认输出".into())
        .chain(devices.iter().map(|device| {
            format!(
                "{}{}",
                device.name,
                if device.is_default {
                    "（当前默认）"
                } else {
                    ""
                }
            )
        }))
        .collect()
}
fn capture_index(devices: &[CaptureDevice], id: &str) -> usize {
    devices
        .iter()
        .position(|device| device.id == id)
        .map(|ix| ix + 1)
        .unwrap_or(0)
}
fn section_title(label: &'static str) -> impl IntoElement {
    div()
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .child(label)
}
fn setting_row(control: impl IntoElement, description: &'static str, cx: &App) -> impl IntoElement {
    div().flex().flex_col().gap_2().child(control).child(
        div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(description),
    )
}
fn metric(label: &'static str, value: String, cx: &App) -> impl IntoElement {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child(label),
        )
        .child(div().text_lg().child(value))
}
fn is_busy(state: &str) -> bool {
    matches!(
        state,
        "connecting" | "pending" | "streaming" | "reconnecting"
    )
}
fn state_label(state: &str) -> &'static str {
    match state {
        "connecting" => "正在连接",
        "pending" => "等待手机确认",
        "streaming" => "正在播放",
        "reconnecting" => "正在重连",
        "rejected" => "手机拒绝了连接",
        _ => "等待连接",
    }
}
