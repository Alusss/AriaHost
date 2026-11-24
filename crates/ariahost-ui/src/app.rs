//! 面板本体（eframe / egui）。
//!
//! M2-1 起面板显示**真数据**：Windows 服务状态（SCM 只读）+ aria2 状态（JSON-RPC），
//! 全部来自 [`crate::worker`] 的后台线程；UI 线程只做三件事：
//! 1. 每帧 `try_recv` 一次快照（非阻塞）；
//! 2. 把快照渲染成「徽章 + 错误码 + 下一步」；
//! 3. 关窗时把窗口几何原子写回 `window.json`。
//!
//! 另有两处 M2-0 留下的钩子继续保持：
//! - [`PanelApp::raw_input_hook`]：每帧把 `RawInput.max_texture_side` 压回图集上限（EG-7）；
//! - wgpu 未捕获错误回调：交换链尺寸被设备拒绝时计数，作为 `--panel-selftest` 的 `ok=` 判据。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ariahost_common::{ErrorCode, PRODUCT_NAME, VERSION};
use egui::{Color32, RichText};

use crate::instance::InstanceLock;
use crate::logbuf::LogBuffer;
use crate::metrics;
use crate::window::{self, Geometry};
use crate::worker::{self, Aria2Update, ServiceSnapshot, Worker};

/// 请求窗口尺寸后最多尝试多少帧（约 3 秒 @60fps）；超过就放弃并把 `ok=0`。
const MAX_SIZE_ATTEMPTS: u32 = 200;

/// 尺寸判定容差（物理像素）：DPI 换算与窗口边框会带来 ±1~2 px 误差。
const SIZE_TOLERANCE: i64 = 2;

/// 空闲时的重绘周期：worker 是 `mpsc`，没有事件能唤醒 egui，所以只能定时轮询一次
/// 「通道里有没有新快照」。500ms 足够跟手（刷新本身是 2s 一次），又不会烧 CPU。
const IDLE_REPAINT: Duration = Duration::from_millis(500);

/// 操作提示的显示时长（复制成功 / 打开目录失败等）。
const NOTICE_TIMEOUT: Duration = Duration::from_secs(8);

/// 起窗后收集到的运行事实：面板信息区显示，`--panel-selftest` 用它打印机器可读行。
#[derive(Debug, Clone, Default)]
pub struct RuntimeFacts {
    /// 渲染适配器名。
    pub adapter: String,
    /// 渲染后端（`Adapter::get_info().backend` 的调试字符串）。
    pub backend: String,
    /// 生效的字形图集上限（`ATLAS_MAX_SIDE_*`，不是设备上限）。
    pub atlas_side: u32,
    /// 字形图集纹理的实际尺寸 `[宽, 高]`（`FontsView::font_image_size`）。
    pub atlas_size: [usize; 2],
    /// 窗口客户区 / 交换链的物理像素尺寸 `[宽, 高]`。
    pub window_physical: [u32; 2],
    /// 系统字体解析结果（形如 `system:Microsoft YaHei UI:...`）。
    pub font_resolved: Option<String>,
    /// 收尾时量到的私有工作集（KB，EG-4 口径）。
    pub private_kb: u64,
    /// 已渲染帧数。
    pub frames: u64,
    /// wgpu 未捕获错误次数（含交换链配置失败）。
    pub wgpu_errors: u64,
    /// 自测结论：无 wgpu 错误，且请求的尺寸确实到达。
    pub ok: bool,
    /// `ok=0` 时的中文原因（同时打到 `SELFTEST_NOTE` 行）。
    pub reason: Option<String>,
}

/// 面板状态。
pub struct PanelApp {
    log: Arc<LogBuffer>,
    facts: Arc<Mutex<RuntimeFacts>>,
    lock: Option<InstanceLock>,
    /// 图集上限（EG-7），由 [`eframe::App::raw_input_hook`] 每帧写回 egui。
    atlas_cap: u32,
    /// 自测模式的帧数上限；`None` 为交互模式。
    selftest_frames: Option<u32>,
    /// 自测模式请求的窗口物理像素尺寸。
    requested_size: Option<(u32, u32)>,
    /// 请求的尺寸是否已经到位（到位后才开始计帧）。
    size_settled: bool,
    /// 已经发过多少次尺寸请求（用于超时放弃与限频）。
    size_attempts: u32,
    /// 尺寸到位后渲染的帧数（自测判据用它，而不是总帧数）。
    frames_after_ready: u64,
    frames: u64,
    selftest_reported: bool,
    /// IME 自检输入框内容（EG-2 门禁的真机验证载体）。
    ime_probe: String,
    last_memory_kb: Option<u64>,
    last_memory_at: Instant,
    /// wgpu 未捕获错误计数（与错误回调共享）。
    wgpu_errors: Arc<AtomicU64>,
    /// 中文字体接入结果（EG-5 的可见证据，信息区用 `FontState::is_system_font` 判定）。
    font_state: crate::fonts::FontState,
    /// 后台刷新线程（SCM + aria2）。
    worker: Worker,
    /// 最近一轮 aria2 结果（`None` = 还没刷新过）。
    aria2: Option<Aria2Update>,
    /// 最近一次服务状态。
    service: Option<ServiceSnapshot>,
    /// 一次性操作提示（文案 + 产生时刻）。
    notice: Option<(String, Instant)>,
    /// 上次成功写盘的窗口几何（没变就不重复写）。
    last_saved: Option<Geometry>,
    /// 本次启动是否已经写过窗口几何（只写一次）。
    geometry_saved: bool,
}

impl PanelApp {
    /// 建面板：先装字体、收集运行事实，再起后台刷新线程。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        log: Arc<LogBuffer>,
        facts: Arc<Mutex<RuntimeFacts>>,
        lock: Option<InstanceLock>,
        selftest_frames: Option<u32>,
        requested_size: Option<(u32, u32)>,
        atlas_cap: u32,
        saved_geometry: Option<Geometry>,
    ) -> Self {
        let font = crate::fonts::install(&cc.egui_ctx, &log);
        log.push(format!(
            "中文字体：{}",
            match font.resolved.as_deref() {
                Some(name) => format!("系统字体已解析（{name}，EG-5）"),
                None => "未确认（可能缺字，见上一条日志）".to_owned(),
            }
        ));
        log.push(format!(
            "图集上限 {atlas_cap} px 将通过 RawInput.max_texture_side 每帧写回（EG-7 解耦）"
        ));

        let wgpu_errors = Arc::new(AtomicU64::new(0));
        let mut runtime = RuntimeFacts {
            font_resolved: font.resolved.clone(),
            atlas_side: atlas_cap,
            ..RuntimeFacts::default()
        };
        match &cc.wgpu_render_state {
            Some(render_state) => {
                let info = render_state.adapter.get_info();
                runtime.adapter = info.name.clone();
                runtime.backend = format!("{:?}", info.backend);
                log.push(format!(
                    "渲染设备：{}（{:?}）；设备上限 {} px，图集上限 {} px",
                    info.name,
                    info.backend,
                    render_state.device.limits().max_texture_dimension_2d,
                    atlas_cap
                ));

                // EG-10：wgpu 未捕获错误（device lost、校验失败、交换链配置失败等）写内部日志，
                // 不 panic、不静默退出；同时计数供 `--panel-selftest` 判 ok。
                let error_log = Arc::clone(&log);
                let error_count = Arc::clone(&wgpu_errors);
                let handler: Arc<dyn eframe::wgpu::UncapturedErrorHandler> =
                    Arc::new(move |error: eframe::wgpu::Error| {
                        error_count.fetch_add(1, Ordering::Relaxed);
                        error_log.push(format!("wgpu 未捕获错误（EG-10，面板继续运行）：{error}"));
                    });
                render_state.device.on_uncaptured_error(handler);
            }
            None => {
                log.push(
                    "未拿到 wgpu 渲染状态：面板以非硬件渲染路径运行（不应出现在产品环境）"
                        .to_owned(),
                );
            }
        }
        *facts_guard(&facts) = runtime;

        if let Some((width, height)) = requested_size {
            log.push(format!("自测请求窗口物理尺寸 {width}×{height} px"));
        }

        let worker = Worker::spawn(Arc::clone(&log));
        log.push(format!(
            "后台刷新已启动：aria2 默认每 {}s（{} 可覆盖，失败退避 2→4→8→{}s），服务状态每 {}s",
            worker::ARIA2_REFRESH_INTERVAL.as_secs(),
            worker::REFRESH_INTERVAL_ENV,
            worker::BACKOFF_CAP.as_secs(),
            worker::SERVICE_REFRESH_INTERVAL.as_secs()
        ));

        Self {
            log,
            facts,
            lock,
            atlas_cap,
            selftest_frames,
            requested_size,
            size_settled: false,
            size_attempts: 0,
            frames_after_ready: 0,
            frames: 0,
            selftest_reported: false,
            ime_probe: String::new(),
            last_memory_kb: metrics::private_working_set_kb(),
            last_memory_at: Instant::now(),
            wgpu_errors,
            font_state: font,
            worker,
            aria2: None,
            service: None,
            notice: None,
            last_saved: saved_geometry,
            geometry_saved: false,
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

    /// 把 worker 通道里积压的更新收干净（非阻塞；只保留每类最后一条）。
    fn drain_worker(&mut self) {
        while let Some(update) = self.worker.try_recv() {
            match update {
                worker::Update::Aria2(update) => self.aria2 = Some(update),
                worker::Update::Service(snapshot) => self.service = Some(snapshot),
            }
        }
    }

    /// 记一条操作提示（8 秒后自动消失）。
    fn notify(&mut self, text: impl Into<String>) {
        self.notice = Some((text.into(), Instant::now()));
    }

    /// 状态区：服务 + aria2，两条都是「徽章 + 主文案 + 详情 + 下一步」。
    fn show_status(&mut self, ui: &mut egui::Ui) {
        let aria2 = worker::aria2_view(self.aria2.as_ref());
        let service = worker::service_view(self.service.as_ref());

        ui.horizontal(|ui| {
            ui.heading("Windows 服务");
            badge(ui, &service);
        });
        ui.label(RichText::new(&service.headline).strong());
        for line in &service.details {
            ui.label(RichText::new(line).weak());
        }
        if let Some(next) = &service.next_step {
            ui.label(RichText::new(format!("下一步：{next}")).color(tone_color(service.tone)));
        }

        ui.separator();

        ui.horizontal(|ui| {
            ui.heading("aria2 核心");
            badge(ui, &aria2);
        });
        ui.label(RichText::new(&aria2.headline).strong());
        if let Some(code) = aria2.code {
            ui.label(
                RichText::new(format!("错误码：{code}"))
                    .color(tone_color(aria2.tone))
                    .monospace(),
            );
        }
        for line in &aria2.details {
            ui.label(RichText::new(line).weak());
        }
        if let Some(next) = &aria2.next_step {
            ui.label(RichText::new(format!("下一步：{next}")).color(tone_color(aria2.tone)));
        }
        if !self.worker.is_running() {
            ui.label(
                RichText::new("自动刷新未启用（后台线程未启动，见启动日志）")
                    .color(tone_color(worker::Tone::Bad)),
            );
        }
    }

    /// 操作区：只放本批次真的做得了的四件事。
    fn show_actions(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui
                .button("立即刷新")
                .on_hover_text("立刻再拉一次 aria2 与服务状态（非阻塞，不会卡界面）")
                .clicked()
            {
                self.worker.request_refresh();
                self.notify("已请求立即刷新");
            }

            if ui
                .button("打开配置目录")
                .on_hover_text("用资源管理器打开 %APPDATA%\\AriaHost\\")
                .clicked()
            {
                match config_dir() {
                    Some(dir) => match open_in_file_manager(&dir) {
                        Ok(()) => self.notify(format!("已打开配置目录：{}", dir.display())),
                        Err(error) => {
                            let code = ErrorCode::Usage.code();
                            self.log.push(format!(
                                "打开配置目录失败（{code}）：{error}（{}）",
                                dir.display()
                            ));
                            self.notify(format!(
                                "{code} 无法打开配置目录（{error}）；可手动访问 {}",
                                dir.display()
                            ));
                        }
                    },
                    None => {
                        let code = ErrorCode::Usage.code();
                        self.log
                            .push(format!("打开配置目录失败（{code}）：%APPDATA% 不可用"));
                        self.notify(format!("{code} 找不到 %APPDATA%，无法打开配置目录"));
                    }
                }
            }

            if ui
                .button("复制 RPC 端点")
                .on_hover_text(worker::endpoint_text())
                .clicked()
            {
                ui.ctx().copy_text(worker::endpoint_text());
                self.notify(format!("已复制 RPC 端点：{}", worker::endpoint_text()));
            }

            if ui
                .button("退出")
                .on_hover_text("关闭面板（托盘与后台常驻是后续批次）")
                .clicked()
            {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });

        if let Some((text, at)) = &self.notice {
            if at.elapsed() < NOTICE_TIMEOUT {
                ui.label(RichText::new(text).color(Color32::from_rgb(140, 200, 255)));
            } else {
                self.notice = None;
            }
        }
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
                ui.label(format!(
                    "{} px（EG-7；设备/窗口上限 {} px，两者已解耦）",
                    facts.atlas_side,
                    crate::DEVICE_MAX_SIDE
                ));
                ui.end_row();

                ui.label("中文字体");
                // `FontState::is_system_font` 是 EG-5 的唯一判定入口：不是「有没有字符串」，
                // 而是「解析出的字体名是不是 system: 前缀」。
                ui.label(
                    match (
                        self.font_state.is_system_font(),
                        self.font_state.resolved.as_deref(),
                    ) {
                        (true, Some(name)) => format!("系统字体已解析：{name}（EG-5）"),
                        (true, None) => "系统字体已接入（EG-5）".to_owned(),
                        (false, _) => format!(
                            "未解析到系统字体：中文可能显示为方块（EG-5，locale={}）",
                            crate::fonts::LOCALE
                        ),
                    },
                );
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
        egui::ScrollArea::vertical()
            .id_salt("ariahost_log")
            .max_height(260.0)
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in self.log.snapshot() {
                    ui.monospace(line);
                }
            });
    }

    /// 当前窗口客户区的物理像素尺寸（= eframe 交给交换链的 extent）。
    fn physical_window_size(&self, ui: &egui::Ui) -> (u32, u32) {
        let scale = ui.ctx().pixels_per_point();
        let points = ui.max_rect().size();
        (
            (points.x * scale).round().max(0.0) as u32,
            (points.y * scale).round().max(0.0) as u32,
        )
    }

    /// 当前窗口几何（点）：位置取外框左上角，尺寸取客户区。
    fn current_geometry(ui: &egui::Ui) -> Option<Geometry> {
        ui.ctx().input(|input| {
            let viewport = input.viewport();
            let outer = viewport.outer_rect?;
            let inner = viewport.inner_rect?;
            Some(Geometry {
                x: outer.min.x,
                y: outer.min.y,
                width: inner.width(),
                height: inner.height(),
            })
        })
    }

    /// 关窗时把几何写回：只写一次，且自测模式完全不碰（否则一次 4K 自测会把面板拉成 4K）。
    fn persist_geometry(&mut self, ui: &egui::Ui) {
        if !should_persist_geometry(self.selftest_frames.is_some(), self.geometry_saved) {
            return;
        }
        let Some(path) = window::window_file() else {
            self.geometry_saved = true;
            self.log.push("窗口状态未保存：%LOCALAPPDATA% 不可用");
            return;
        };
        let Some(geometry) = Self::current_geometry(ui) else {
            // Wayland/Android 之类拿不到窗口位置：不猜，保持原文件不动。
            return;
        };
        let screen = ui
            .ctx()
            .input(|input| input.viewport().monitor_size)
            .map(|size| (size.x, size.y));
        let clean = window::sanitize(geometry, screen);
        if self
            .last_saved
            .is_some_and(|previous| previous.same_as(clean))
        {
            self.geometry_saved = true;
            return;
        }
        match window::save(&path, clean) {
            Ok(()) => {
                self.geometry_saved = true;
                self.last_saved = Some(clean);
                self.log.push(format!(
                    "窗口状态已保存：{:.0}×{:.0} pt @ ({:.0}, {:.0}) → {}",
                    clean.width,
                    clean.height,
                    clean.x,
                    clean.y,
                    path.display()
                ));
            }
            Err(error) => {
                self.geometry_saved = true;
                self.log
                    .push(format!("窗口状态保存失败：{error}（{}）", path.display()));
            }
        }
    }

    /// `--panel-selftest --size WxH`：把窗口调到指定物理像素（可以超出屏幕，用于验交换链）。
    fn apply_requested_size(&mut self, ui: &egui::Ui) {
        let Some((want_width, want_height)) = self.requested_size else {
            return;
        };
        if self.size_settled {
            return;
        }
        let (width, height) = self.physical_window_size(ui);
        if approx_eq(width, want_width) && approx_eq(height, want_height) {
            self.size_settled = true;
            self.log.push(format!(
                "窗口物理尺寸已到位：{width}×{height} px（目标 {want_width}×{want_height}）"
            ));
            return;
        }
        if self.size_attempts >= MAX_SIZE_ATTEMPTS {
            // 窗口管理器/DPI 上限不松口时不能死等：放弃调整、照常计帧收尾，
            // 由收尾逻辑把尺寸不匹配写进 `ok=0` 的原因，避免自测永久挂住。
            self.size_settled = true;
            self.log.push(format!(
                "窗口尺寸尝试 {MAX_SIZE_ATTEMPTS} 帧仍未到位，放弃调整（自测将报 ok=0）"
            ));
            return;
        }
        self.size_attempts += 1;
        if self.size_attempts > 1 && !self.size_attempts.is_multiple_of(20) {
            return;
        }
        let scale = ui.ctx().pixels_per_point().max(0.1);
        let target = egui::vec2(
            (want_width as f32 / scale).min(crate::MAX_WINDOW_SIDE),
            (want_height as f32 / scale).min(crate::MAX_WINDOW_SIDE),
        );
        self.log.push(format!(
            "请求窗口 {want_width}×{want_height} px（{:.0}×{:.0} pt @ {scale:.2}×，当前 {width}×{height} px）",
            target.x, target.y
        ));
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::InnerSize(target));
    }

    /// 自测收尾：记录图集/窗口/错误计数/内存，然后关窗让 `run_native` 返回。
    fn maybe_finish_selftest(&mut self, ui: &mut egui::Ui) {
        let Some(limit) = self.selftest_frames else {
            return;
        };
        if self.frames_after_ready < u64::from(limit) {
            // 自测要连续出帧，不能等用户输入触发重绘。
            ui.ctx().request_repaint();
            return;
        }
        if self.selftest_reported {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        self.selftest_reported = true;

        let atlas_size = ui.ctx().fonts(|fonts| fonts.font_image_size());
        let window_physical = self.physical_window_size(ui);
        let wgpu_errors = self.wgpu_errors.load(Ordering::Relaxed);
        let private_kb = metrics::private_working_set_kb().unwrap_or(0);
        let mut reason: Option<String> = None;
        if wgpu_errors > 0 {
            reason = Some(format!(
                "wgpu 报错 {wgpu_errors} 次（含交换链配置失败，见启动日志）"
            ));
        }
        let size_ok = match self.requested_size {
            None => true,
            Some((want_width, want_height)) => {
                let matched = approx_eq(window_physical.0, want_width)
                    && approx_eq(window_physical.1, want_height);
                if !matched && reason.is_none() {
                    let attempts = self.size_attempts;
                    let cap = crate::MAX_WINDOW_SIDE;
                    reason = Some(format!(
                        "窗口物理尺寸 {}×{} 未到达目标 {want_width}×{want_height}（尝试 {attempts} 次，\
                         受 DPI 上限 {cap} px 或窗口管理器限制）",
                        window_physical.0, window_physical.1,
                    ));
                }
                matched
            }
        };
        let ok = size_ok && wgpu_errors == 0;
        {
            let mut facts = facts_guard(&self.facts);
            facts.atlas_size = atlas_size;
            facts.window_physical = [window_physical.0, window_physical.1];
            facts.private_kb = private_kb;
            facts.frames = self.frames;
            facts.wgpu_errors = wgpu_errors;
            facts.ok = ok;
            facts.reason = reason.clone();
        }
        self.log.push(format!(
            "自测完成：{0} 帧，窗口 {1}×{2} px，图集 {3}×{4} px，wgpu 错误 {wgpu_errors}，\
             私有工作集 {private_kb} KB，ok={5}",
            self.frames,
            window_physical.0,
            window_physical.1,
            atlas_size[0],
            atlas_size[1],
            u8::from(ok),
        ));
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

impl eframe::App for PanelApp {
    /// EG-7 解耦点：每帧把图集上限写回 egui。
    ///
    /// eframe 在 `EpiIntegration::build_raw_input` 里逐帧调用本钩子
    /// （`eframe-0.37.0/src/native/epi_integration.rs:423`），此后 egui 才把
    /// `RawInput.max_texture_side` 落到 `TextOptions.max_texture_side`
    /// （`egui-0.37.0/src/context.rs:587`、`:628`）。设备上限保持 8192，
    /// 图集仍按 2048 建——窗口能开在 4K，显存不被 ×4。
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        let cap = self.atlas_cap as usize;
        raw_input.max_texture_side = Some(
            raw_input
                .max_texture_side
                .map_or(cap, |current| current.min(cap)),
        );
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.frames = self.frames.saturating_add(1);
        self.sample_memory();
        self.drain_worker();
        self.apply_requested_size(ui);
        if self.requested_size.is_none() || self.size_settled {
            self.frames_after_ready = self.frames_after_ready.saturating_add(1);
        }

        // 关窗时保存几何；eframe 本帧之后就退出主循环，所以必须在这里同步写完。
        if ui.ctx().input(|input| input.viewport().close_requested()) {
            self.persist_geometry(ui);
        }

        // 交互模式下 worker 是 mpsc，没有人能「推醒」egui，只能定时问一次（不是忙轮询）。
        ui.ctx().request_repaint_after(IDLE_REPAINT);

        ui.horizontal(|ui| {
            ui.heading(format!("{PRODUCT_NAME} {VERSION}"));
            ui.label(RichText::new("M2-1 状态面板").weak());
        });
        ui.add_space(4.0);

        egui::ScrollArea::vertical()
            .id_salt("ariahost_panel")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                self.show_status(ui);
                ui.separator();
                self.show_actions(ui);
                ui.separator();
                self.show_facts(ui);
                ui.separator();
                self.show_ime_probe(ui);
                ui.separator();
                self.show_log(ui);
            });

        self.maybe_finish_selftest(ui);
    }
}

/// 是否该写窗口几何：自测模式永不写，本进程只写一次。
fn should_persist_geometry(selftest: bool, already_saved: bool) -> bool {
    !selftest && !already_saved
}

/// 配置目录：`%APPDATA%\AriaHost\`（`paths::config_file` 的父目录）。
fn config_dir() -> Option<PathBuf> {
    ariahost_common::paths::config_file()
        .and_then(|file| file.parent().map(Path::to_path_buf))
        .filter(|dir| !dir.as_os_str().is_empty())
}

/// 用系统文件管理器打开目录（只要求「进程起来了」；explorer 的退出码不可靠）。
fn open_in_file_manager(dir: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        std::process::Command::new("explorer")
            .arg(dir)
            .spawn()
            .map(|_child| ())
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("xdg-open")
            .arg(dir)
            .spawn()
            .map(|_child| ())
    }
}

/// 状态徽章：底色按语气区分，文字永远是中文状态词。
fn badge(ui: &mut egui::Ui, view: &worker::StatusView) {
    let color = tone_color(view.tone);
    ui.label(
        RichText::new(format!("[{}]", view.badge))
            .color(color)
            .strong(),
    );
}

/// 语气 → 颜色（深色主题下也够对比）。
fn tone_color(tone: worker::Tone) -> Color32 {
    match tone {
        worker::Tone::Good => Color32::from_rgb(110, 205, 130),
        worker::Tone::Warn => Color32::from_rgb(240, 190, 90),
        worker::Tone::Bad => Color32::from_rgb(235, 110, 110),
        worker::Tone::Idle => Color32::from_rgb(170, 170, 170),
    }
}

/// 物理像素比较：允许 ±[`SIZE_TOLERANCE`] 的 DPI/边框换算误差。
fn approx_eq(value: u32, target: u32) -> bool {
    (i64::from(value) - i64::from(target)).abs() <= SIZE_TOLERANCE
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
        assert_eq!(facts.atlas_size, [0, 0]);
        assert!(facts.font_resolved.is_none());
        assert_eq!(facts.frames, 0);
        assert!(!facts.ok);
    }

    #[test]
    fn font_state_flags_system_fonts() {
        assert!(crate::fonts::FontState {
            resolved: Some("system:Microsoft YaHei:1:0".to_owned())
        }
        .is_system_font());
    }

    #[test]
    fn size_tolerance_is_tight_but_not_exact() {
        assert!(approx_eq(3840, 3840));
        assert!(approx_eq(3841, 3840));
        assert!(approx_eq(3838, 3840));
        assert!(!approx_eq(3837, 3840));
        assert!(!approx_eq(2000, 3840));
    }

    #[test]
    fn geometry_is_never_persisted_in_selftest_or_twice() {
        assert!(!should_persist_geometry(true, false));
        assert!(!should_persist_geometry(true, true));
        assert!(!should_persist_geometry(false, true));
        assert!(should_persist_geometry(false, false));
    }

    #[test]
    fn config_dir_points_at_appdata_ariahost() {
        // 测试机可能没有 %APPDATA%（非 Windows）：返回 None 是预期，不在这里断言平台
        // （纯常量断言会踩 clippy 的 `assertions_on_constants`）。
        if let Some(dir) = config_dir() {
            assert!(dir.ends_with("AriaHost"), "{dir:?}");
        }
    }
}
