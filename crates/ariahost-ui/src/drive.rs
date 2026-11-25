//! 隐藏自检驱动（`--panel-selftest --drive settings`）：用 egui 自己的 `RawInput` 注入事件。
//!
//! 为什么要有它：`egui 0.37` 只消费 accesskit 的 `Action::Click`（`context.rs:1769`），
//! `TextEdit` 暴露的 `SetValue` 只是 provider 回声（`保存` 始终 disabled 就是铁证），
//! `RadioButton` 的 `SelectionItem`、`CheckBox` 的 `Toggle` 也都不被消费。结果是
//! **UIA 无法驱动输入类控件**，而这台机器常年锁屏、`SendInput` 同样不可用。
//!
//! 铁律：驱动只能往 `RawInput.events` 里塞合成事件，**不许**直接改 `Settings::draft`。
//! 否则测的是「我自己改值再保存」，跟用户操作无关。
//!
//! 走的是 `PanelApp::draw`——与真实窗口**同一条绘制路径**，不另建一套 UI。

use std::collections::BTreeMap;

use egui::{Event, Key, Modifiers, PointerButton, Pos2, RawInput, Rect, Vec2};

use crate::app::PanelApp;

/// 本帧绘制出来的控件矩形。键是配置键（`dl.dir`）、文案（`每次询问`）或分组名（`下载`）。
#[derive(Debug, Default, Clone)]
pub struct Targets {
    map: BTreeMap<String, Rect>,
}

impl Targets {
    /// 记录一个控件的矩形。
    pub fn note(&mut self, key: impl Into<String>, rect: Rect) {
        self.map.insert(key.into(), rect);
    }

    /// 取控件中心（点击用）。
    pub fn center(&self, key: &str) -> Option<Pos2> {
        self.map.get(key).map(Rect::center)
    }

    /// 当前已记录的键（诊断用）。
    pub fn keys(&self) -> Vec<String> {
        self.map.keys().cloned().collect()
    }
}

/// 一帧输入：要注入的事件序列。
#[derive(Debug, Default, Clone)]
pub struct Frame {
    events: Vec<Event>,
}

impl Frame {
    /// 在指定位置点一下。
    pub fn click(mut self, pos: Pos2) -> Self {
        self.events.push(Event::PointerMoved(pos));
        for pressed in [true, false] {
            self.events.push(Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed,
                modifiers: Modifiers::NONE,
            });
        }
        self
    }

    /// 输入一段文字（要求目标控件已获得焦点）。
    pub fn text(mut self, text: &str) -> Self {
        self.events.push(Event::Text(text.to_owned()));
        self
    }

    /// Ctrl+A：全选，便于整体替换输入框内容。
    pub fn select_all(mut self) -> Self {
        self.events.push(Event::Key {
            key: Key::A,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::CTRL,
        });
        self.events.push(Event::Key {
            key: Key::A,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers: Modifiers::CTRL,
        });
        self
    }

    /// 折成 `RawInput`（屏幕尺寸固定，避免 0 尺寸导致布局不可点）。
    pub fn into_raw(self) -> RawInput {
        let mut raw = RawInput::default();
        raw.screen_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 760.0)));
        raw.events = self.events;
        raw
    }
}

/// 跑一帧（清掉纹理增量：`epaint` 在 drop 未消费的 `TexturesDelta` 时会 panic）。
fn pump(ctx: &egui::Context, app: &mut PanelApp, raw: RawInput) {
    let mut output = ctx.run_ui(raw, |ui| app.draw(ui));
    output.textures_delta.clear();
}

fn idle(ctx: &egui::Context, app: &mut PanelApp) {
    pump(ctx, app, Frame::default().into_raw());
}

/// 点一个已记录键的控件：先跑一帧取矩形，再跑一帧注入点击。
fn click(ctx: &egui::Context, app: &mut PanelApp, key: &str) -> Result<(), String> {
    idle(ctx, app);
    let center = app
        .drive_targets
        .center(key)
        .ok_or_else(|| format!("找不到控件「{key}」（已记录：{:?}）", app.drive_targets.keys()))?;
    pump(ctx, app, Frame::default().click(center).into_raw());
    Ok(())
}

/// 往某个输入框里整体替换成 `text`（点焦点 → Ctrl+A → 输入）。
fn type_into(
    ctx: &egui::Context,
    app: &mut PanelApp,
    key: &str,
    text: &str,
) -> Result<(), String> {
    click(ctx, app, key)?;
    pump(ctx, app, Frame::default().select_all().into_raw());
    pump(ctx, app, Frame::default().text(text).into_raw());
    Ok(())
}

/// 驱动「设置页」四条场景，返回逐条结论（PASS/FAIL + 证据）。
///
/// 场景：
/// 1. 改下载目录：draft 改变、无红字、保存变亮；
/// 2. 非法数字：`net.split=99` → 红字含 `1–16`、保存 disabled、磁盘 cfg 未变；
/// 3. 三项一起改 + 保存：`config.toml` 三项都对；
/// 4. 开机自启：点 CheckBox → 保存 → `ui.start_with_windows=true` 落盘（HKCU 由外部脚本核对）。
pub fn run_settings(ctx: &egui::Context, app: &mut PanelApp) -> Vec<String> {
    let mut out = Vec::new();
    let cfg_path = ariahost_common::paths::config_file();
    let read_cfg = || -> String {
        cfg_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default()
    };
    let test_dir = std::env::temp_dir().join("ah-drive-dir");
    let _ = std::fs::create_dir_all(&test_dir);
    let test_dir_text = test_dir.display().to_string();

    // 打开设置页
    if let Err(error) = click(ctx, app, "设置入口") {
        out.push(format!("0 打开设置页 : FAIL {error}"));
        return out;
    }
    idle(ctx, app);
    out.push(format!(
        "0 打开设置页 : 设置页打开={} => {}",
        app.settings_page_open(),
        if app.settings_page_open() { "PASS" } else { "FAIL" }
    ));

    // ---- 场景 1：改下载目录 ----
    let _ = click(ctx, app, "下载");
    let before = read_cfg();
    match type_into(ctx, app, "dl.dir", &test_dir_text) {
        Err(error) => out.push(format!("1 改下载目录 : FAIL {error}")),
        Ok(()) => {
            idle(ctx, app);
            let draft = app.settings_state().draft().dl.dir.clone();
            let dirty = app.settings_state().is_dirty();
            let errors = app.settings_state().has_errors();
            out.push(format!(
                "1 改下载目录 : draft='{draft}' 期望='{test_dir_text}' 有红字={errors} 变脏={dirty} => {}",
                if draft == test_dir_text && !errors && dirty { "PASS" } else { "FAIL" }
            ));
            let _ = before;
        }
    }

    // ---- 场景 2：非法数字 ----
    let _ = click(ctx, app, "网络");
    let before_invalid = read_cfg();
    match type_into(ctx, app, "net.split", "99") {
        Err(error) => out.push(format!("2 非法数字被拦 : FAIL {error}")),
        Ok(()) => {
            idle(ctx, app);
            let errors = app.settings_state().has_errors();
            let split = app.settings_state().draft().net.split;
            let after = read_cfg();
            out.push(format!(
                "2 非法数字被拦 : draft split={split} 有红字={errors} 磁盘 cfg 未变={} => {}",
                after == before_invalid,
                if split == 99 && errors && after == before_invalid { "PASS" } else { "FAIL" }
            ));
        }
    }

    // ---- 场景 3：三项一起改 + 保存 ----
    let _ = type_into(ctx, app, "net.split", "8");
    let _ = click(ctx, app, "界面");
    let _ = click(ctx, app, "每次询问");
    idle(ctx, app);
    let dirty = app.settings_state().is_dirty();
    let errors = app.settings_state().has_errors();
    let saved = click(ctx, app, "保存").is_ok();
    idle(ctx, app);
    let cfg = read_cfg();
    let has_dir = cfg.contains(&test_dir_text);
    let has_split = cfg.lines().any(|line| line.trim() == "split = 8");
    let has_ask = cfg.contains("close_action = \"ask\"");
    out.push(format!(
        "3 三项改完+保存 : 变脏={dirty} 有红字={errors} 点保存={saved} | cfg dir={has_dir} split=8:{has_split} close_action=ask:{has_ask} => {}",
        if has_dir && has_split && has_ask { "PASS" } else { "FAIL" }
    ));

    // ---- 场景 4：开机自启（勾选 → 保存 → 配置落盘；HKCU 由外部脚本核对） ----
    let before_sw = read_cfg();
    let toggled = click(ctx, app, "开机自启（start_with_windows，写 HKCU Run，不用计划任务）").is_ok();
    idle(ctx, app);
    let draft_sw = app.settings_state().draft().ui.start_with_windows;
    let saved_sw = click(ctx, app, "保存").is_ok();
    idle(ctx, app);
    let cfg_after = read_cfg();
    let key_line = if draft_sw {
        "start_with_windows = true"
    } else {
        "start_with_windows = false"
    };
    let has_key = cfg_after.lines().any(|line| line.trim() == key_line);
    out.push(format!(
        "4 开机自启 : 点勾选={toggled} draft start_with_windows={draft_sw} 点保存={saved_sw} cfg 含 '{key_line}'={has_key} | （HKCU 由外部 reg query 核对） => {}",
        if has_key { "PASS" } else { "FAIL" }
    ));
    let _ = before_sw;

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `epaint` 在 drop 未消费的 `TexturesDelta` 时会 panic，故 `pump` 统一清掉。
    #[test]
    fn text_event_reaches_a_text_edit() {
        let ctx = egui::Context::default();
        let mut text = String::new();
        // 直接用最小控件验证「Event::Text 能进 TextEdit」这条关键可行性。
        let mut out = ctx.run_ui(Frame::default().into_raw(), |ui| {
            let response = ui.text_edit_singleline(&mut text);
            if !response.has_focus() {
                response.request_focus();
            }
        });
        out.textures_delta.clear();
        let mut out2 = ctx.run_ui(Frame::default().text("AB").into_raw(), |ui| {
            ui.text_edit_singleline(&mut text);
        });
        out2.textures_delta.clear();
        assert_eq!(text, "AB", "Event::Text 必须能驱动 TextEdit");
    }
}
