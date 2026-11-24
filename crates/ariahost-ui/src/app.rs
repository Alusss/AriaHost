//! 面板本体（eframe / egui）。
//!
//! 只做技术闸门的可见证据：渲染后端、系统字体、单实例、内存读数、IME 自检输入框、启动日志。
//! 不接服务控制、不接 RPC、不做任务列表（M2-1）。

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ariahost_common::{PRODUCT_NAME, VERSION};
use egui::{Color32, RichText};

use crate::instance::InstanceLock;
use crate::logbuf::LogBuffer;
use crate::metrics;

/// 起窗后收集到的运行事实：面板信息区显示，`--panel-selftest` 用它打印机器可读行。
#[derive(Debug, Clone, Default)]
pub struct RuntimeFacts {
    /// wgpu adapter 名（`Adapter::get_info().name`）。
    pub adapter: String,
    /// 渲染后端（`Adapter::get_info().backend` 的调试字符串）。
    pub backend: String,
    /// 设备实际生效的字形图集上限（`Device::limits().max_texture_dimension_2d`）。
    pub atlas_side: u32,
    /// 系统字体解析结果（形如 `system:Microsoft YaHei:...`）。
    pub font_resolved: Option<String>,
    /// 收尾时量到的私有工作集（KB，EG-4 口径）。
    pub private_kb: u64,
    /// 已渲染帧数。
    pub frames: u64,
}

/// 面板状态。
pub struct PanelApp {
    log: Arc<LogBuffer>,
    facts: Arc<Mutex<RuntimeFacts>>,
    lock: Option<InstanceLock>,
    /// 自测模式的帧数上限；`None` 为交互模式。
    selftest_frames: Option<u32>,
    frames: u64,
    selftest_reported: bool,
    /// IME 自检输入框内容（EG-2 门禁的真机验证载体）。
    ime_probe: String,
    last_memory_kb: Option<u64>,
    last_memory_at: Instant,
    window_clamped: bool,
}

impl PanelApp {
    /// 起窗后第一件事：接入系统字体（EG-5/EG-6）、抓渲染后端事实、挂 wgpu 错误钩子（EG-10）。
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        log: Arc<LogBuffer>,
        facts: Arc<Mutex<RuntimeFacts>>,
        lock: Option<InstanceLock>,
        selftest_frames: Option<u32>,
    ) -> Self {
        let font = crate::fonts::install(&cc.egui_ctx, &log);
        log.push(format!(
            "中文字体来源：{}",
            if font.is_system_font() {
                "系统字体（EG-5 满足）"
            } else {
                "未确认（可能缺字，见上一条日志）"
            }
        ));

        let mut runtime = RuntimeFacts {
            font_resolved: font.resolved.clone(),
            ..RuntimeFacts::default()
        };
        match &cc.wgpu_render_state {
            Some(render_state) => {
                let info = render_state.adapter.get_info();
                runtime.adapter = info.name.clone();
                runtime.backend = format!("{:?}", info.backend);
                runtime.atlas_side = render_state.device.limits().max_texture_dimension_2d;
                log.push(format!(
                    "渲染设备：{}（{:?}）；生效的字形图集上限 {} px",
                    info.name, info.backend, runtime.atlas_side
                ));

                // EG-10：wgpu 未捕获错误（device lost、校验失败等）写内部日志，不 panic、不静默退出。
                let error_log = Arc::clone(&log);
                let handler: Arc<dyn eframe::wgpu::UncapturedErrorHandler> =
                    Arc::new(move |error: eframe::wgpu::Error| {
                        error_log.push(format!("wgpu 未捕获错误（EG-10，面板继续运行）：{error}"));
                    });
                render_state.device.on_uncaptured_error(handler);
            }
            None => {
                runtime.adapter = "未知（未拿到 wgpu 渲染状态）".to_owned();
                runtime.backend = "unknown".to_owned();
                log.push(
                    "未拿到 wgpu 渲染状态：渲染后端信息不可用（EG-10 的兜底归 M2-1）".to_owned(),
                );
            }
        }
        *facts_guard(&facts) = runtime;

        Self {
            log,
            facts,
            lock,
            selftest_frames,
            frames: 0,
            selftest_reported: false,
            ime_probe: String::new(),
            last_memory_kb: metrics::private_working_set_kb(),
            last_memory_at: Instant::now(),
            window_clamped: false,
        }
    }

    /// 取一份运行事实快照（避免绘制时长时间持有锁）。
    fn snapshot_facts(&self) -> RuntimeFacts {
        facts_guard(&self.facts).clone()
    }

    /// 内存读数每 2 秒采样一次：量测是系统调用，不要放进每帧路径。
    fn sample_memory(&mut self) {
        if self.last_memory_kb.is_some() && self.last_memory_at.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.last_memory_kb = metrics::private_working_set_kb();
        self.last_memory_at = Instant::now();
    }

    /// 信息区：渲染后端 / 图集上限 / 中文字体 / 单实例 / 内存。
    fn show_facts(&self, ui: &mut egui::Ui) {
        let facts = self.snapshot_facts();
        egui::Grid::new("ariahost_runtime_facts")
            .num_columns(2)
            .spacing([14.0, 6.0])
            .show(ui, |ui| {
                ui.label("渲染后端");
                ui.label(format!("{}（{}）", facts.adapter, facts.backend));
                ui.end_row();

                ui.label("字形图集上限");
                ui.label(format!("{} px（EG-7）", facts.atlas_side));
                ui.end_row();

                ui.label("中文字体");
                ui.label(match facts.font_resolved.as_deref() {
                    Some(name) => format!("系统字体已解析：{name}（EG-5）"),
                    None => format!(
                        "未解析到系统字体：中文可能显示为方块（EG-5，locale={}）",
                        crate::fonts::LOCALE
                    ),
                });
                ui.end_row();

                ui.label("单实例锁");
                ui.label(self.lock.as_ref().map_or_else(
                    || "不可用：本次启动未持锁（原因见启动日志）".to_owned(),
                    InstanceLock::describe,
                ));
                ui.end_row();

                ui.label("进程私有工作集");
                ui.label(match self.last_memory_kb {
                    Some(kb) => format!(
                        "{:.1} MB（EG-4 口径参考值；真机 60s 稳态由验收量测）",
                        kb as f64 / 1024.0
                    ),
                    None => "不可用".to_owned(),
                });
                ui.end_row();
            });
    }

    /// IME 自检区：M2 门禁的真机入口。
    fn show_ime_probe(&mut self, ui: &mut egui::Ui) {
        ui.label("输入法（IME）自检");
        ui.label(
            RichText::new(
                "M2 门禁要求：在这个输入框连续输入 30 字中文，不丢字、候选框跟随光标、Shift 中英切换正常。",
            )
            .weak(),
        );
        // EG-2 硬约束：本输入框**不**调用 request_focus / request_focus_next_frame。
        // egui 的 request_focus 会无条件 interrupt_ime()，每帧抢焦点等于每帧打断组合串
        // （egui Issue #8813 截至 2026-11-22 仍未修）。焦点只由用户点击决定。
        ui.add(
            egui::TextEdit::singleline(&mut self.ime_probe)
                .hint_text("点这里输入中文…")
                .desired_width(ui.available_width()),
        );
        ui.label(format!("已输入 {} 个字符", self.ime_probe.chars().count()));
    }

    /// 日志区：有界环形缓冲 + 滚动区（EG-9）。
    fn show_log(&mut self, ui: &mut egui::Ui) {
        ui.label(format!(
            "启动日志（内部环形缓冲，最多 {} 行）",
            crate::logbuf::CAPACITY
        ));
        let height = (ui.available_height() - 4.0).max(80.0);
        egui::ScrollArea::vertical()
            .max_height(height)
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in self.log.snapshot() {
                    ui.monospace(line);
                }
            });
    }

    /// EG-7/EG-11：窗口物理像素超过图集上限时收缩一次（否则交换链配置会失败、表现为黑窗）。
    fn clamp_window_once(&mut self, ui: &egui::Ui) {
        let facts = self.snapshot_facts();
        let cap = facts.atlas_side;
        if cap == 0 {
            return;
        }
        let ctx = ui.ctx();
        let scale = ctx.pixels_per_point();
        if scale <= 0.0 {
            return;
        }
        let points = ui.max_rect().size();
        let physical = points * scale;
        let cap_px = cap as f32;
        if physical.x <= cap_px && physical.y <= cap_px {
            return;
        }
        let target = egui::vec2(
            (points.x.min(cap_px / scale)).max(640.0),
            (points.y.min(cap_px / scale)).max(480.0),
        );
        self.log.push(format!(
            "窗口物理尺寸 {:.0}×{:.0} px 超过字形图集上限 {cap} px（EG-7/EG-11），请求收缩到 {:.0}×{:.0} pt；\
             需要更大窗口时设环境变量 {}=4096",
            physical.x,
            physical.y,
            target.x,
            target.y,
            crate::ATLAS_MAX_SIDE_ENV
        ));
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(target));
    }

    /// 自测收尾：记录内存读数与帧数，然后关窗让 `run_native` 返回。
    fn maybe_finish_selftest(&mut self, ctx: &egui::Context) {
        let Some(limit) = self.selftest_frames else {
            return;
        };
        if self.frames < u64::from(limit) {
            // 自测要连续出帧，不能等用户输入触发重绘。
            ctx.request_repaint();
            return;
        }
        if !self.selftest_reported {
            self.selftest_reported = true;
            let private_kb = metrics::private_working_set_kb().unwrap_or(0);
            {
                let mut facts = facts_guard(&self.facts);
                facts.private_kb = private_kb;
                facts.frames = self.frames;
            }
            self.log.push(format!(
                "自测完成：{} 帧，私有工作集 {private_kb} KB",
                self.frames
            ));
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

impl eframe::App for PanelApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.frames = self.frames.saturating_add(1);
        self.sample_memory();

        if !self.window_clamped {
            self.window_clamped = true;
            self.clamp_window_once(ui);
        }

        ui.horizontal(|ui| {
            ui.heading(format!("{PRODUCT_NAME} {VERSION}"));
            ui.label(RichText::new("M2 技术闸门切片").weak());
        });
        ui.label(
            RichText::new("面板处于 M2 开发中；服务控制与任务列表在 M2-1。")
                .color(Color32::from_rgb(240, 190, 90)),
        );
        ui.add_space(6.0);
        ui.separator();

        self.show_facts(ui);
        ui.separator();
        self.show_ime_probe(ui);
        ui.separator();
        self.show_log(ui);

        self.maybe_finish_selftest(ui.ctx());
    }
}

/// 取运行事实的锁；中毒（panic 过）也继续用内部值，不让面板因为日志/计数崩掉。
fn facts_guard(facts: &Mutex<RuntimeFacts>) -> MutexGuard<'_, RuntimeFacts> {
    facts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 面板标题里用的完整版本字符串（`AriaHost 0.1.1`），供测试与日志复用。
pub fn title_text() -> String {
    format!("{PRODUCT_NAME} {VERSION}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_carries_product_and_version() {
        let title = title_text();
        assert!(title.starts_with(PRODUCT_NAME), "{title}");
        assert!(title.contains(VERSION), "{title}");
    }

    #[test]
    fn facts_default_is_empty_but_typed() {
        let facts = RuntimeFacts::default();
        assert_eq!(facts.atlas_side, 0);
        assert!(facts.font_resolved.is_none());
        assert_eq!(facts.frames, 0);
    }

    #[test]
    fn font_state_flags_system_fonts() {
        assert!(crate::fonts::FontState {
            resolved: Some("system:Microsoft YaHei:1:0".to_owned())
        }
        .is_system_font());
    }
}
