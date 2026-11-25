//! 面板本体（eframe / egui）。
//!
//! M2-2b 起面板能**下载**：顶部是 aria2 核心的状态机与启停按钮，中间是虚拟化任务表
//! （EG-8，200 行只用 `body().rows()` 画可见那几十行），下面是新建下载入口，
//! 折叠区里留着运行信息、输入法自检与内部日志。
//!
//! 三条线程纪律：
//! 1. UI 线程只做非阻塞 `try_recv`，RPC 与进程托管都在后台（[`crate::worker`] /
//!    [`crate::core_host`]）；
//! 2. 任务行的所有字符串在 worker 里就格式化好（`Arc<str>`），本文件**不对行做
//!    `format!`**（EG-9：200 行 × 每帧都格式化 = 掉帧）；
//! 3. 键盘焦点只在第一帧请求一次（EG-2：`request_focus` 会 `interrupt_ime()`，
//!    每帧请求 = 中文一个字符都打不进去，egui #8813 至今未修）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use ariahost_common::{ErrorCode, PRODUCT_NAME, VERSION};
use egui::{Color32, RichText};
use egui_extras::{Column, TableBuilder};

use crate::core_host::{self, CoreHost, CoreProbe, HostState};
use crate::instance::InstanceLock;
use crate::logbuf::LogBuffer;
use crate::metrics;
use crate::tasks::{self, TaskAction, TaskRow, TaskStatus};
use crate::view::{tone_color, StatusView, Tone};
use crate::window::{self, Geometry};
use crate::worker::{
    self, ActionOutcome, AddOutcome, Aria2Update, ServiceSnapshot, TaskRequest, Worker,
    WorkerOptions,
};

/// 请求窗口尺寸后最多尝试多少帧（约 3 秒 @60fps）；超过就放弃并把 `ok=0`。
const MAX_SIZE_ATTEMPTS: u32 = 200;

/// 尺寸判定容差（物理像素）：DPI 换算与窗口边框会带来 ±1~2 px 误差。
const SIZE_TOLERANCE: i64 = 2;

/// 空闲时的重绘周期：worker 是 `mpsc`，没有事件能唤醒 egui，所以只能定时轮询一次
/// 「通道里有没有新结果」。500ms 足够跟手（刷新本身是 2s 一次），又不会烧 CPU。
const IDLE_REPAINT: Duration = Duration::from_millis(500);

/// 忙态的重绘周期（启动/停止/行操作期间）。
///
/// 停止链要等 aria2c 自己关闭（实测 4.1–4.3 秒），这段时间**必须**由帧循环自己
/// 排下一帧：后台线程只能往 `mpsc` 里塞结果，推不醒 egui。若沿用 500ms 闲时定时器，
/// 任何一次漏排都会让界面停在「已等待 N 秒」的旧帧上，看起来就是「停止卡住了」。
const BUSY_REPAINT: Duration = Duration::from_millis(100);

/// 恢复窗口几何后最多检查多少帧：首帧窗口位置可能还没上报（部分平台先给 None），
/// 不能因为第一次拿不到就永久放弃校正，也不能无限等。
const MAX_GEOMETRY_CHECKS: u32 = 30;

/// 操作提示的显示时长（复制成功 / 打开目录失败等）。
const NOTICE_TIMEOUT: Duration = Duration::from_secs(8);

/// 错误条自动消失时长（坏消息不自动消失，只让用户点「知道了」）。
const BANNER_TIMEOUT: Duration = Duration::from_secs(12);

/// 「乐观行」的存活上限：`addUri` 成功到真实数据出现通常 < 2s；
/// 超过这个时长还没被真实行接管，就当它没成功（避免界面上留一条永远「等待中」的幽灵）。
const OPTIMISTIC_TTL: Duration = Duration::from_secs(15);

/// 任务行的固定行高（虚拟化步长；`body().rows()` 必须是常数行高才能算可见区间）。
const ROW_HEIGHT: f32 = 22.0;

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
    /// 渲染结束时任务表里的行数（`--seed-rows` 的内存验收靠它证明「真的画了 200 行」）。
    pub rows: usize,
    /// wgpu 未捕获错误次数（含交换链配置失败）。
    pub wgpu_errors: u64,
    /// 自测结论：无 wgpu 错误，且请求的尺寸确实到达。
    pub ok: bool,
    /// `ok=0` 时的中文原因（同时打到 `SELFTEST_NOTE` 行）。
    pub reason: Option<String>,
}

/// 内联错误条（面板内的一条，不弹窗）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Banner {
    /// 语气（决定颜色）。
    pub tone: Tone,
    /// 既有错误码（没有合适码时为 `None`）。
    pub code: Option<&'static str>,
    /// 一句人话。
    pub headline: String,
    /// 可执行建议。
    pub next: Option<String>,
}

impl Banner {
    fn new(tone: Tone, headline: impl Into<String>) -> Self {
        Self {
            tone,
            code: None,
            headline: headline.into(),
            next: None,
        }
    }

    fn with_code(tone: Tone, code: Option<ErrorCode>, headline: impl Into<String>) -> Self {
        Self {
            tone,
            code: code.map(ErrorCode::code),
            headline: headline.into(),
            next: None,
        }
    }

    fn and_next(mut self, next: impl Into<String>) -> Self {
        self.next = Some(next.into());
        self
    }
}

/// 「乐观行」：`addUri` 刚成功、真实列表还没刷回来的那一两秒。
struct Optimistic {
    row: TaskRow,
    added_at: Instant,
}

/// 建面板所需的一切（参数多到必须打包，`too_many_arguments` 会让 CI 的 clippy 失败）。
pub struct PanelInit {
    /// 内部日志环。
    pub log: Arc<LogBuffer>,
    /// 运行事实（`--panel-selftest` 的输出）。
    pub facts: Arc<Mutex<RuntimeFacts>>,
    /// 单实例锁（持有到进程退出）。
    pub lock: Option<InstanceLock>,
    /// 自测帧数（`None` = 交互模式）。
    pub selftest_frames: Option<u32>,
    /// 自测请求的窗口物理像素尺寸。
    pub requested_size: Option<(u32, u32)>,
    /// 字形图集上限。
    pub atlas_cap: u32,
    /// 启动时从 `window.json` 恢复的几何。
    pub saved_geometry: Option<Geometry>,
    /// `--seed-rows N`：灌 N 行假数据、不连 RPC（内存验收用）。
    pub seed_rows: Option<usize>,
    /// 核心托管的前置准备（`seed_rows` 为 `Some` 时不需要）。
    pub setup: Option<core_host::HostSetup>,
    /// worker 参数（端口 / secret / 刷新间隔）。
    pub worker_options: WorkerOptions,
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
    /// 中文字体接入结果（EG-5 的可见证据）。
    font_state: crate::fonts::FontState,
    /// 后台刷新线程（RPC / 任务列表 / 行操作）。
    worker: Option<Worker>,
    /// aria2c 运行时托管（启停）。
    host: Option<CoreHost>,
    /// 核心状态镜像（由 `core_host` 事件驱动）。
    core: HostState,
    /// 启动前置条件探测（决定「未安装」文案与按钮提示）。
    core_probe: CoreProbe,
    /// 忙态的起点（用来显示「已等待 X 秒」）。
    busy_since: Option<Instant>,
    /// 最近一轮 aria2 全局结果。
    aria2: Option<Aria2Update>,
    /// 最近一次服务状态。
    service: Option<ServiceSnapshot>,
    /// 任务表（worker 送来的真行）。
    tasks: Vec<TaskRow>,
    /// 任务表本轮失败原因（`None` = 正常）。
    tasks_error: Option<worker::Aria2Failure>,
    /// 是否触到每类上限（界面给一行提示）。
    tasks_truncated: bool,
    /// 乐观行（等真实数据接管）。
    optimistic: Vec<Optimistic>,
    /// 新建下载的输入框。
    url: String,
    /// 已经发出去但还没回结果的链接（失败时填回输入框）。
    pending_add: Option<String>,
    /// EG-2：焦点只请求一次。
    focus_requested: bool,
    /// 内联错误条（文案 + 产生时刻）。
    banner: Option<(Banner, Instant)>,
    /// 一次性操作提示（文案 + 产生时刻）。
    notice: Option<(String, Instant)>,
    /// 上次成功写盘的窗口几何（没变就不重复写）。
    last_saved: Option<Geometry>,
    /// 本次启动是否已经写过窗口几何（只写一次）。
    geometry_saved: bool,
    /// 恢复的几何是否已经按当前显示器校正过（只做一次）。
    geometry_checked: bool,
    /// 已经为校正尝试过多少帧（拿不到窗口位置时不会无限重试）。
    geometry_checks: u32,
    /// 启动时是否真的从 `window.json` 恢复了几何（没恢复就不需要校正）。
    geometry_restored: bool,
    /// `--seed-rows N`：自测假数据行数（`None` = 正常模式）。
    seed_rows: Option<usize>,
}

impl PanelApp {
    /// 建面板：先装字体、收集运行事实，再起后台线程。
    pub fn new(cc: &eframe::CreationContext<'_>, init: PanelInit) -> Self {
        let PanelInit {
            log,
            facts,
            lock,
            selftest_frames,
            requested_size,
            atlas_cap,
            saved_geometry,
            seed_rows,
            setup,
            worker_options,
        } = init;

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

        // 自测灌行模式：不连 RPC、不托管核心，只压渲染路径（200 行 + idle 的内存闸门）。
        let seeded: Vec<TaskRow> = match seed_rows {
            Some(count) => {
                log.push(format!(
                    "自测种子行：{count} 行假数据（不连 RPC、不启动 aria2；EG-8 虚拟化验收用）"
                ));
                (0..count).map(TaskRow::sample).collect()
            }
            None => Vec::new(),
        };

        let (host, probe, core) = match (&seeded.is_empty(), setup) {
            (true, Some(setup)) => {
                let probe = setup.probe().clone();
                if let Some((code, detail)) = setup.problem() {
                    log.push(format!(
                        "启动前置条件不满足：{} {detail}",
                        code.map_or("none", ErrorCode::code)
                    ));
                }
                let host = CoreHost::spawn(Arc::clone(&log), setup);
                (Some(host), probe, HostState::Idle)
            }
            _ => (None, CoreProbe::default(), HostState::Idle),
        };

        let worker = if host.is_some() {
            let worker = Worker::spawn(Arc::clone(&log), worker_options.clone());
            log.push(format!(
                "后台刷新已启动：aria2 每 {}s（{} 可覆盖，失败退避 2→4→8→{}s），服务状态每 {}s",
                worker_options.interval.as_secs(),
                worker::REFRESH_INTERVAL_ENV,
                worker::BACKOFF_CAP.as_secs(),
                worker::SERVICE_REFRESH_INTERVAL.as_secs()
            ));
            Some(worker)
        } else {
            None
        };

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
            host,
            core,
            core_probe: probe,
            busy_since: None,
            aria2: None,
            service: None,
            tasks: seeded,
            tasks_error: None,
            tasks_truncated: false,
            optimistic: Vec::new(),
            url: String::new(),
            pending_add: None,
            focus_requested: false,
            banner: None,
            notice: None,
            last_saved: saved_geometry,
            geometry_saved: false,
            geometry_checked: false,
            geometry_checks: 0,
            geometry_restored: saved_geometry.is_some(),
            seed_rows,
        }
    }

    /// 自测灌行模式（不连任何后端）。
    fn seed_mode(&self) -> bool {
        self.seed_rows.is_some()
    }

    /// 表里实际渲染的行数（乐观行 + 真实行）。
    fn row_count(&self) -> usize {
        self.optimistic.len() + self.tasks.len()
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

    /// 收 worker 的结果（非阻塞；每类只留最后一条）。
    fn drain_worker(&mut self) {
        // 先把通道收干净再处理：处理时要 `&mut self`，不能与 `&self.worker` 的借用重叠。
        let mut pending = Vec::new();
        match &self.worker {
            Some(worker) => {
                while let Some(update) = worker.try_recv() {
                    pending.push(update);
                }
            }
            None => return,
        }
        for update in pending {
            match update {
                worker::Update::Aria2(update) => {
                    let was_down = !matches!(
                        self.aria2.as_ref().map(|previous| &previous.outcome),
                        Some(Ok(_))
                    );
                    let now_up = update.outcome.is_ok();
                    self.aria2 = Some(update);
                    // RPC 从「不通」变「通」：可能是用户自己起了外部 aria2，立刻拉一次列表。
                    if was_down && now_up {
                        if let Some(worker) = &self.worker {
                            worker.request_refresh();
                        }
                    }
                }
                worker::Update::Service(snapshot) => self.service = Some(snapshot),
                worker::Update::Tasks(tasks) => {
                    self.tasks_truncated = tasks.truncated;
                    match tasks.outcome {
                        Ok(rows) => {
                            self.tasks_error = None;
                            self.tasks = rows;
                            // 真实行接管乐观行。
                            let tasks = &self.tasks;
                            self.optimistic
                                .retain(|entry| !tasks.iter().any(|row| row.gid == entry.row.gid));
                            self.pending_add = None;
                        }
                        Err(failure) => {
                            self.tasks_error = Some(failure);
                            self.tasks.clear();
                        }
                    }
                }
                worker::Update::Action(outcome) => self.on_action(outcome),
                worker::Update::Added(outcome) => self.on_added(outcome),
            }
        }
    }

    /// 收核心状态（非阻塞）。
    fn drain_core(&mut self) {
        let mut pending = Vec::new();
        match &self.host {
            Some(host) => {
                while let Some(update) = host.try_recv() {
                    pending.push(update);
                }
            }
            None => return,
        }
        for update in pending {
            if let Some(note) = &update.note {
                self.log.push(format!("核心：{note}"));
            }
            let was_running = self.core.is_running();
            let now_running = update.state.is_running();
            let state_changed =
                std::mem::discriminant(&self.core) != std::mem::discriminant(&update.state);
            self.core = update.state;
            if state_changed {
                // 状态机成功走到新一档：上一档留下的内联提示条一律作废，
                // 否则「已停止」之后还挂着停止前的旧错误（缺陷 2 的前半）。
                self.banner = None;
                // 核心不在运行时，任务列表本来就连不上，这类「连接类」错误是噪音；
                // 用户主动「立即刷新」的真错误仍由 worker 单独回，不受这里影响。
                if !self.core.is_running() {
                    self.tasks_error = None;
                }
            }
            if self.core.is_busy() {
                if self.busy_since.is_none() {
                    self.busy_since = Some(Instant::now());
                }
            } else {
                self.busy_since = None;
            }
            if let HostState::Failed { code, detail } = &self.core {
                let (code, detail) = (*code, detail.to_string());
                self.set_banner(
                    Banner::with_code(Tone::Bad, code, detail).and_next(failure_next_step(code)),
                );
            }
            // 核心刚就绪：立刻刷一次，别等最多 30s 的退避窗口。
            if !was_running && now_running {
                if let Some(worker) = &self.worker {
                    worker.request_refresh();
                }
            }
        }
    }

    /// 乐观行过期清理（`addUri` 成功但真实数据一直没出现）。
    fn expire_optimistic(&mut self) {
        let now = Instant::now();
        let before = self.optimistic.len();
        self.optimistic
            .retain(|entry| now.duration_since(entry.added_at) < OPTIMISTIC_TTL);
        if self.optimistic.len() != before {
            self.set_banner(
                Banner::new(
                    Tone::Warn,
                    "新建的任务没有出现在列表里（aria2 可能拒收了链接）",
                )
                .and_next("看底部日志里「新建下载失败」那一条的原因"),
            );
        }
    }

    /// 记一条操作提示（8 秒后自动消失）。
    fn notify(&mut self, text: impl Into<String>) {
        self.notice = Some((text.into(), Instant::now()));
    }

    /// 记一条内联错误条。
    fn set_banner(&mut self, banner: Banner) {
        self.banner = Some((banner, Instant::now()));
    }

    /// 行操作结果。
    fn on_action(&mut self, outcome: ActionOutcome) {
        match outcome.result {
            Ok(()) => self.notify(format!("已{}任务 {}", outcome.action.label(), outcome.gid)),
            Err(failure) => self.set_banner(
                Banner::with_code(
                    Tone::Bad,
                    Some(failure.code()),
                    format!("{}任务失败：{}", outcome.action.label(), failure.summary()),
                )
                .and_next(failure.next_step()),
            ),
        }
    }

    /// 新建下载结果。
    fn on_added(&mut self, outcome: AddOutcome) {
        match outcome.result {
            Ok(gid) => {
                self.optimistic.push(Optimistic {
                    row: TaskRow::optimistic(&gid, &outcome.uri),
                    added_at: Instant::now(),
                });
                self.pending_add = None;
                self.notify(format!("已加入下载：{}", short_name(&outcome.uri)));
                // 2s 周期之外立刻拉一次，让真实行尽快接管。
                if let Some(worker) = &self.worker {
                    worker.request_refresh();
                }
            }
            Err(failure) => {
                // 失败时把用户输入还回去（输入框已经清空过），别让人重打一遍。
                if let Some(pending) = self.pending_add.take() {
                    self.url = pending;
                }
                self.set_banner(
                    Banner::with_code(
                        Tone::Bad,
                        Some(failure.code()),
                        format!("新建下载失败：{}", failure.summary()),
                    )
                    .and_next(failure.next_step()),
                );
            }
        }
    }

    /// 提交输入框里的链接。
    fn submit_url(&mut self) {
        let uri = self.url.trim().to_owned();
        if uri.is_empty() {
            self.set_banner(Banner::new(
                Tone::Warn,
                "先粘贴一个下载链接，再点「开始」或按回车。",
            ));
            return;
        }
        if !looks_like_uri(&uri) {
            self.set_banner(
                Banner::new(Tone::Warn, format!("「{}」不像下载地址", short_name(&uri)))
                    .and_next("支持 http:// https:// ftp:// 与 magnet: 开头的链接"),
            );
            return;
        }
        let Some(worker) = &self.worker else {
            self.set_banner(Banner::new(
                Tone::Warn,
                "当前是自测模式，没有连 aria2，无法新建下载。",
            ));
            return;
        };

        // 本地先判一次「核起来没有」：比等 5 秒 RPC 超时友好得多。
        // 判据用最近一次真实 RPC 结果（外部 aria2 也算通），不只看本进程托管的状态。
        let rpc_up = matches!(
            self.aria2.as_ref().map(|update| &update.outcome),
            Some(Ok(_))
        );
        if !rpc_up && !self.core.is_running() {
            self.set_banner(
                Banner::with_code(
                    Tone::Bad,
                    Some(ErrorCode::RpcHandshakeFailed),
                    "aria2 还没启动，现在加任务会失败",
                )
                .and_next("先点左上「启动 aria2」，等状态变成「运行中」再试"),
            );
            return;
        }

        self.pending_add = Some(uri.clone());
        self.url.clear();
        worker.request_add(uri);
    }

    /// 状态区：核心状态机 + 主按钮。
    fn show_status(&mut self, ui: &mut egui::Ui) {
        if self.seed_mode() {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("[自测]")
                        .color(tone_color(Tone::Idle))
                        .strong(),
                );
                ui.label(
                    RichText::new(match self.seed_rows {
                        Some(count) => format!("{count} 行假数据（不连 RPC、不启动 aria2）"),
                        None => "无数据".to_owned(),
                    })
                    .strong(),
                );
            });
            return;
        }

        let view = core_host::status_view(&self.core, &self.core_probe);
        let busy = self.core.is_busy();
        let running = self.core.is_running();
        let ready = self.core_probe.ready();

        ui.horizontal(|ui| {
            badge(ui, &view);
            ui.label(RichText::new(&view.headline).strong());
            if busy {
                ui.add(egui::Spinner::new().size(14.0));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                self.main_buttons(ui, running, busy, ready);
            });
        });

        if let Some(code) = view.code {
            ui.label(
                RichText::new(format!("错误码：{code}"))
                    .color(tone_color(view.tone))
                    .monospace(),
            );
        }
        for line in &view.details {
            ui.label(RichText::new(line).weak().small());
        }
        if let Some(next) = &view.next_step {
            ui.label(
                RichText::new(format!("下一步：{next}"))
                    .color(tone_color(view.tone))
                    .small(),
            );
        }
        if let (Some(since), true) = (self.busy_since, busy) {
            ui.label(
                RichText::new(format!("已等待 {:.1} 秒", since.elapsed().as_secs_f32()))
                    .weak()
                    .small(),
            );
        }
        if self.host.as_ref().is_some_and(|host| !host.is_alive()) {
            ui.label(
                RichText::new("核心托管线程未启动：按钮点击不会有反应（见启动日志）")
                    .color(tone_color(Tone::Bad)),
            );
        }
    }

    /// 主按钮组（右到左布局里调用：先画最右的「打开下载目录」）。
    fn main_buttons(&mut self, ui: &mut egui::Ui, running: bool, busy: bool, ready: bool) {
        let dir = self.core_probe.download_dir.clone();
        let open = ui.add_enabled(
            dir.is_some(),
            egui::Button::new("打开下载目录").min_size(egui::vec2(104.0, 26.0)),
        );
        let open = match &dir {
            Some(path) => open.on_hover_text(format!("用资源管理器打开 {}", path.display())),
            None => open.on_hover_text("下载目录不可用（见状态区的原因）"),
        };
        if open.clicked() {
            if let Some(path) = &dir {
                match open_in_file_manager(path) {
                    Ok(()) => self.notify(format!("已打开下载目录：{}", path.display())),
                    Err(error) => {
                        let code = ErrorCode::Usage.code();
                        self.set_banner(
                            Banner::with_code(
                                Tone::Bad,
                                Some(ErrorCode::Usage),
                                format!("{code} 无法打开下载目录（{error}）"),
                            )
                            .and_next(format!("手工访问 {}", path.display())),
                        );
                    }
                }
            }
        }

        let (label, tip) = if running {
            (
                "停止 aria2",
                format!(
                    "优雅停止（保存会话后退出）。实测约 {} 秒，这期间界面会显示「停止中」。",
                    core_host::STOP_EXPECTED_SECS
                ),
            )
        } else {
            (
                "启动 aria2",
                if ready {
                    "启动本机 aria2c 并等 RPC 就绪（实测 0.72 秒）".to_owned()
                } else {
                    "前置条件不满足（见下方「下一步」）；点击会再试一次".to_owned()
                },
            )
        };
        let button = ui.add_enabled(
            !busy,
            egui::Button::new(RichText::new(label).strong()).min_size(egui::vec2(116.0, 26.0)),
        );
        let button = button.on_hover_text(tip);
        if button.clicked() {
            match &self.host {
                Some(host) if running => {
                    host.stop();
                    self.busy_since = Some(Instant::now());
                    self.banner = None;
                }
                Some(host) => {
                    host.start();
                    self.busy_since = Some(Instant::now());
                    self.banner = None;
                }
                None => self.set_banner(Banner::new(
                    Tone::Bad,
                    "当前是自测模式（--seed-rows），没有连接 aria2 核心。",
                )),
            }
        }
    }

    /// 内联错误条（不弹窗）。
    fn show_banner(&mut self, ui: &mut egui::Ui) {
        let Some((banner, at)) = &self.banner else {
            return;
        };
        // 坏消息不自动消失（用户可能正看着别处），提示类 12 秒后自己走。
        if banner.tone != Tone::Bad && at.elapsed() > BANNER_TIMEOUT {
            self.banner = None;
            return;
        }
        let color = tone_color(banner.tone);
        let mut dismiss = false;
        egui::Frame::NONE
            .fill(color.gamma_multiply(0.14))
            .inner_margin(egui::Margin::same(6))
            .corner_radius(egui::CornerRadius::same(4))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if let Some(code) = banner.code {
                        ui.label(RichText::new(code).monospace().color(color).strong());
                    }
                    ui.label(RichText::new(&banner.headline).color(color));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("知道了").clicked() {
                            dismiss = true;
                        }
                    });
                });
                if let Some(next) = &banner.next {
                    ui.label(RichText::new(format!("怎么做：{next}")).small().weak());
                }
            });
        if dismiss {
            self.banner = None;
        }
    }

    /// 新建下载：输入框 + 开始。
    ///
    /// **EG-2 硬约束**：本输入框只在第一帧请求一次焦点，之后永不调用
    /// `request_focus`（它会 `interrupt_ime()`，每帧调用等于每帧打断中文组合串，
    /// egui Issue #8813）。焦点之后完全由用户点击决定。
    fn show_new_task(&mut self, ui: &mut egui::Ui) {
        let mut submit = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new("新建下载").strong());
            let width = (ui.available_width() - 88.0).max(200.0);
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.url)
                    .id_salt("ariahost_url_input")
                    .hint_text("粘贴下载链接（http/https/ftp/magnet），回车开始")
                    .desired_width(width),
            );
            // 只在第一帧请求一次焦点（用控件自己上报的 id，不手搓 Id——0.37 已弃用 `Id::new`）。
            // **不要**把它挪进每帧路径（EG-2）。
            if !self.focus_requested {
                self.focus_requested = true;
                let id = response.id;
                ui.ctx().memory_mut(|memory| memory.request_focus(id));
            }
            let enter =
                response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
            if ui
                .button("开始")
                .on_hover_text("加入下载队列（aria2.addUri）")
                .clicked()
                || enter
            {
                submit = true;
            }
        });
        if submit {
            self.submit_url();
        }
        if let Some(pending) = &self.pending_add {
            ui.label(
                RichText::new(format!("正在提交：{}", short_name(pending)))
                    .weak()
                    .small(),
            );
        }
    }

    /// 任务表（EG-8：`body().rows()` 只画可见区间）。
    fn show_tasks(&mut self, ui: &mut egui::Ui) {
        let total = self.row_count();
        let mut refresh = false;
        let mut open_config = false;
        let mut copy_endpoint = false;

        ui.horizontal(|ui| {
            ui.heading(format!("任务（{total}）"));
            if self.tasks_truncated {
                ui.label(
                    RichText::new(format!("仅显示每类前 {} 条", tasks::MAX_ROWS_PER_BUCKET))
                        .weak()
                        .small(),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("退出").on_hover_text("关闭面板").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if ui
                    .button("打开配置目录")
                    .on_hover_text("用资源管理器打开 %APPDATA%\\AriaHost\\")
                    .clicked()
                {
                    open_config = true;
                }
                if ui
                    .button("复制 RPC 端点")
                    .on_hover_text(worker::endpoint_text_for(self.core_probe.port))
                    .clicked()
                {
                    copy_endpoint = true;
                }
                if ui.button("立即刷新").clicked() {
                    refresh = true;
                }
            });
        });

        if let Some((text, at)) = &self.notice {
            if at.elapsed() < NOTICE_TIMEOUT {
                ui.label(
                    RichText::new(text)
                        .color(Color32::from_rgb(140, 200, 255))
                        .small(),
                );
            } else {
                self.notice = None;
            }
        }

        if let Some(failure) = self.tasks_error {
            // 核心没在跑时，刷新失败是「预期内的连不上」（停止后第一次轮询必然命中），
            // 只在真在跑（或错误不是连接类）时才打扰用户。
            if !tasks_error_is_noise(self.core.is_running(), failure.code()) {
                ui.label(
                    RichText::new(format!(
                        "任务列表读取失败：{} {}（{}）",
                        failure.code().code(),
                        failure.summary(),
                        failure.next_step()
                    ))
                    .color(tone_color(Tone::Warn))
                    .small(),
                );
            }
        }

        if total == 0 {
            self.show_empty_tasks(ui);
        } else {
            self.show_task_table(ui, total);
        }

        if refresh {
            if let Some(worker) = &self.worker {
                worker.request_refresh();
                self.notify("已请求立即刷新");
            }
        }
        if open_config {
            match config_dir() {
                Some(dir) => match open_in_file_manager(&dir) {
                    Ok(()) => self.notify(format!("已打开配置目录：{}", dir.display())),
                    Err(error) => self.set_banner(
                        Banner::with_code(
                            Tone::Bad,
                            Some(ErrorCode::Usage),
                            format!("{} 无法打开配置目录（{error}）", ErrorCode::Usage.code()),
                        )
                        .and_next(format!("手工访问 {}", dir.display())),
                    ),
                },
                None => {
                    self.set_banner(Banner::new(Tone::Bad, "找不到 %APPDATA%，无法打开配置目录"));
                }
            }
        }
        if copy_endpoint {
            let text = worker::endpoint_text_for(self.core_probe.port);
            ui.ctx().copy_text(text.clone());
            self.notify(format!("已复制 RPC 端点：{text}"));
        }
    }

    /// 空态：告诉用户下一步做什么，而不是留一块空白。
    fn show_empty_tasks(&mut self, ui: &mut egui::Ui) {
        ui.add_space(24.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("还没有下载任务").strong());
            ui.label(
                RichText::new(if self.seed_mode() {
                    "（自测模式：本来就没有真任务）"
                } else if self.core.is_running() {
                    "把链接粘到上面的输入框，回车就开始下载。"
                } else {
                    "先点左上「启动 aria2」，再把链接粘到上面的输入框。"
                })
                .weak(),
            );
        });
    }

    /// 虚拟化表格本体。
    fn show_task_table(&mut self, ui: &mut egui::Ui, total: usize) {
        // 行操作先在闭包里攒起来，出了表格再发（闭包只借用不可变数据，避免与 &mut self 打架）。
        let mut action: Option<(TaskAction, Arc<str>, TaskStatus)> = None;

        let optimistic = &self.optimistic;
        let rows = &self.tasks;
        let optimistic_len = optimistic.len();

        TableBuilder::new(ui)
            .id_salt("ariahost_tasks")
            .striped(true)
            .resizable(true)
            .cell_layout(egui::Layout::left_to_right(egui::Align::Center))
            .column(Column::remainder().at_least(180.0).clip(true))
            .column(Column::initial(88.0).at_least(64.0))
            .column(Column::initial(108.0).at_least(88.0))
            .column(Column::initial(96.0).at_least(72.0))
            .column(Column::initial(104.0).at_least(72.0))
            .column(Column::initial(64.0).at_least(48.0))
            .column(Column::initial(120.0).at_least(96.0))
            .header(20.0, |mut header| {
                for title in ["名称", "大小", "进度", "速度", "剩余", "状态", "操作"]
                {
                    header.col(|ui| {
                        ui.label(RichText::new(title).strong().small());
                    });
                }
            })
            .body(|body| {
                // EG-8：`rows()` 只对可见区间调用闭包；200 行时每帧实际只画几十行。
                body.rows(ROW_HEIGHT, total, |mut table_row| {
                    let index = table_row.index();
                    let row: &TaskRow = if index < optimistic_len {
                        &optimistic[index].row
                    } else {
                        match rows.get(index - optimistic_len) {
                            Some(row) => row,
                            None => return,
                        }
                    };

                    table_row.col(|ui| {
                        ui.label(RichText::new(&*row.name).small());
                    });
                    table_row.col(|ui| {
                        ui.label(RichText::new(&*row.size).small());
                    });
                    table_row.col(|ui| {
                        ui.add(
                            egui::ProgressBar::new(row.progress)
                                .desired_width(96.0)
                                .show_percentage(),
                        );
                    });
                    table_row.col(|ui| {
                        ui.label(RichText::new(&*row.speed).small());
                    });
                    table_row.col(|ui| {
                        ui.label(RichText::new(&*row.eta).small());
                    });
                    table_row.col(|ui| {
                        ui.label(
                            RichText::new(&*row.status)
                                .small()
                                .color(status_color(row.kind)),
                        );
                    });
                    table_row.col(|ui| {
                        ui.horizontal(|ui| {
                            if row.kind.can_pause() && ui.small_button("暂停").clicked() {
                                action = Some((TaskAction::Pause, Arc::clone(&row.gid), row.kind));
                            }
                            if row.kind.can_resume() && ui.small_button("继续").clicked() {
                                action =
                                    Some((TaskAction::Unpause, Arc::clone(&row.gid), row.kind));
                            }
                            if row.kind.can_remove() && ui.small_button("移除").clicked() {
                                action = Some((TaskAction::Remove, Arc::clone(&row.gid), row.kind));
                            }
                        });
                    });
                });
            });

        if let Some((kind, gid, status)) = action {
            match &self.worker {
                Some(worker) => {
                    worker.request_task(TaskRequest {
                        action: kind,
                        gid: Arc::clone(&gid),
                        kind: status,
                    });
                    self.notify(format!("正在{}任务 {gid}…", kind.label()));
                }
                None => self.set_banner(Banner::new(
                    Tone::Warn,
                    "自测模式没有连接 aria2，行操作不可用。",
                )),
            }
        }
    }

    /// 折叠区：运行信息 / 输入法自检 / 内部日志。
    fn show_footer(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("运行信息、输入法自检与日志")
            .id_salt("ariahost_diag")
            .show(ui, |ui| {
                self.show_facts(ui);
                ui.separator();
                self.show_probe(ui);
                ui.separator();
                self.show_log(ui);
            });
    }

    /// 信息区：渲染后端 / 图集上限 / 中文字体 / 单实例 / 内存 / RPC / SCM。
    fn show_facts(&mut self, ui: &mut egui::Ui) {
        let facts = self.snapshot_facts();
        egui::Grid::new("ariahost_runtime_facts")
            .num_columns(2)
            .spacing([14.0, 4.0])
            .show(ui, |ui| {
                ui.label("渲染后端");
                ui.label(RichText::new(format!("{}（{}）", facts.adapter, facts.backend)).small());
                ui.end_row();

                ui.label("字形图集上限");
                ui.label(
                    RichText::new(format!(
                        "{} px（EG-7；设备/窗口上限 {} px，两者已解耦）",
                        facts.atlas_side,
                        crate::DEVICE_MAX_SIDE
                    ))
                    .small(),
                );
                ui.end_row();

                ui.label("中文字体");
                ui.label(
                    RichText::new(
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
                    )
                    .small(),
                );
                ui.end_row();

                ui.label("单实例锁");
                ui.label(
                    RichText::new(self.lock.as_ref().map_or_else(
                        || "不可用：本次启动未持锁（原因见启动日志）".to_owned(),
                        InstanceLock::describe,
                    ))
                    .small(),
                );
                ui.end_row();

                ui.label("进程私有工作集");
                ui.label(
                    RichText::new(match self.last_memory_kb {
                        Some(kb) => format!(
                            "{:.1} MB（EG-4 口径参考值；真机 60s 稳态由验收量测）",
                            kb as f64 / 1024.0
                        ),
                        None => "不可用".to_owned(),
                    })
                    .small(),
                );
                ui.end_row();

                ui.label("Windows 服务");
                ui.label(RichText::new(self.service_line()).small());
                ui.end_row();
            });

        // RPC 明细：`aria2_view` 是「aria2 原始输出绝不上面板」这条规则的唯一落点
        // （它只产出固定中文 + 码 + 下一步，detail 里的 IO 原文永远进不来）。
        let rpc = worker::aria2_view(self.aria2.as_ref());
        ui.horizontal(|ui| {
            badge(ui, &rpc);
            ui.label(RichText::new(&rpc.headline).small());
        });
        if let Some(code) = rpc.code {
            ui.label(RichText::new(format!("错误码：{code}")).monospace().small());
        }
        for line in &rpc.details {
            ui.label(RichText::new(line).weak().small());
        }
        if let Some(next) = &rpc.next_step {
            ui.label(
                RichText::new(format!("下一步：{next}"))
                    .color(tone_color(rpc.tone))
                    .small(),
            );
        }
    }

    /// SCM 一行摘要。
    fn service_line(&self) -> String {
        match &self.service {
            None => "等待首次刷新…".to_owned(),
            Some(snapshot) => worker::service_view(Some(snapshot)).headline,
        }
    }

    /// 输入法自检区。
    fn show_probe(&mut self, ui: &mut egui::Ui) {
        ui.label("输入法（IME）自检");
        ui.label(
            RichText::new(
                "M2 门禁要求：在这个输入框连续输入 30 字中文，不丢字、候选框跟随光标、Shift 中英切换正常。",
            )
            .weak()
            .small(),
        );
        // EG-2 硬约束：本输入框**不**调用 request_focus / request_focus_next_frame。
        ui.add(
            egui::TextEdit::singleline(&mut self.ime_probe)
                .hint_text("点这里输入中文…")
                .desired_width(ui.available_width()),
        );
        ui.label(
            RichText::new(format!("已输入 {} 个字符", self.ime_probe.chars().count())).small(),
        );
    }

    /// 日志区：有界环形缓冲 + 滚动区（EG-9）。
    fn show_log(&mut self, ui: &mut egui::Ui) {
        ui.label(
            RichText::new(format!(
                "启动日志（内部环形缓冲，最多 {} 行）",
                crate::logbuf::CAPACITY
            ))
            .small(),
        );
        egui::ScrollArea::vertical()
            .id_salt("ariahost_log")
            .max_height(150.0)
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in self.log.snapshot() {
                    ui.label(RichText::new(line).monospace().small());
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

    /// 从 `window.json` 恢复的几何只按「点」钳制过，真正落到屏幕前还要用**当前显示器**再校一次。
    fn fix_restored_geometry(&mut self, ui: &egui::Ui) {
        if self.geometry_checked || !self.geometry_restored {
            return;
        }
        let Some((screen_width, screen_height)) = ui
            .ctx()
            .input(|input| input.viewport().monitor_size)
            .map(|size| (size.x, size.y))
        else {
            return;
        };
        let Some(current) = Self::current_geometry(ui) else {
            self.geometry_checks = self.geometry_checks.saturating_add(1);
            if self.geometry_checks >= MAX_GEOMETRY_CHECKS {
                self.geometry_checked = true;
                self.log.push(format!(
                    "窗口几何：{MAX_GEOMETRY_CHECKS} 帧内没拿到窗口位置，跳过越界校正（沿用文件里的值）"
                ));
            }
            return;
        };
        self.geometry_checked = true;
        let fixed = window::sanitize(current, Some((screen_width, screen_height)));
        if fixed.same_as(current) {
            return;
        }
        self.log.push(format!(
            "窗口几何越界（{:.0}×{:.0} pt @ ({:.0}, {:.0})，显示器 {screen_width:.0}×{screen_height:.0}），\
             已校正到 {:.0}×{:.0} pt @ ({:.0}, {:.0})",
            current.width,
            current.height,
            current.x,
            current.y,
            fixed.width,
            fixed.height,
            fixed.x,
            fixed.y
        ));
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                fixed.width,
                fixed.height,
            )));
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
                fixed.x, fixed.y,
            )));
        self.geometry_checked = true;
    }

    /// 关窗时把几何写回：只写一次，且自测模式完全不碰。
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

    /// `--panel-selftest --size WxH`：把窗口调到指定物理像素。
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

    /// 自测收尾：记录图集/窗口/行数/错误计数/内存，然后关窗让 `run_native` 返回。
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
        let rows = self.row_count();
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
            facts.rows = rows;
            facts.wgpu_errors = wgpu_errors;
            facts.ok = ok;
            facts.reason = reason.clone();
        }
        self.log.push(format!(
            "自测完成：{0} 帧，窗口 {1}×{2} px，图集 {3}×{4} px，任务行 {rows}，\
             wgpu 错误 {wgpu_errors}，私有工作集 {private_kb} KB，ok={5}",
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
        self.drain_core();
        self.expire_optimistic();
        self.fix_restored_geometry(ui);
        self.apply_requested_size(ui);
        if self.requested_size.is_none() || self.size_settled {
            self.frames_after_ready = self.frames_after_ready.saturating_add(1);
        }

        // 关窗时保存几何；eframe 本帧之后就退出主循环，所以必须在这里同步写完。
        if ui.ctx().input(|input| input.viewport().close_requested()) {
            self.persist_geometry(ui);
        }

        // 交互模式下通道是 mpsc，没有人能「推醒」egui，只能定时问一次（不是忙轮询）。
        // 忙态（启动/停止/行操作）必须自己驱动重绘：停止要等 aria2c 4 秒关闭，
        // 期间若靠 500ms 闲时定时器，一旦某帧没排上就会停在「已等待 4.2 秒」的旧帧上。
        let repaint_after = if self.core.is_busy() {
            BUSY_REPAINT
        } else {
            IDLE_REPAINT
        };
        ui.ctx().request_repaint_after(repaint_after);

        ui.horizontal(|ui| {
            ui.heading(format!("{PRODUCT_NAME} {VERSION}"));
            ui.label(RichText::new("M2-2b 下载面板").weak());
        });
        ui.add_space(2.0);

        egui::Panel::top("ariahost_status")
            .show_separator_line(false)
            .show(ui, |ui| {
                self.show_status(ui);
                self.show_banner(ui);
                if !self.seed_mode() {
                    self.show_new_task(ui);
                }
                ui.add_space(2.0);
            });

        egui::Panel::bottom("ariahost_diag")
            .show_separator_line(true)
            .show(ui, |ui| {
                self.show_footer(ui);
            });

        egui::CentralPanel::default().show(ui, |ui| {
            self.show_tasks(ui);
        });

        self.maybe_finish_selftest(ui);
    }
}

/// 任务列表读取失败是否只是噪音（核心没在跑时的连接类报错）。
///
/// 规则（Lead 缺陷 2）：`已停止` 之后不该继续挂着「连不上 aria2」；但用户主动刷新
/// 撞上的真错误、以及核心在跑时的一切错误，都必须照实显示，不能被吞掉。
fn tasks_error_is_noise(running: bool, code: ErrorCode) -> bool {
    if running {
        return false;
    }
    matches!(
        code,
        ErrorCode::RpcHandshakeFailed | ErrorCode::RpcReconnectFailed
    )
}

/// 是否该写窗口几何：自测模式永不写，本进程只写一次。
fn should_persist_geometry(selftest: bool, already_saved: bool) -> bool {
    !selftest && !already_saved
}

/// 链接是否像下载地址（本地先挡一次，省一轮 5 秒的 RPC 超时）。
fn looks_like_uri(raw: &str) -> bool {
    let lowered = raw.trim().to_ascii_lowercase();
    [
        "http://",
        "https://",
        "ftp://",
        "ftps://",
        "magnet:",
        "sftp://",
        "thunder://",
    ]
    .iter()
    .any(|prefix| lowered.starts_with(prefix))
}

/// 链接的短展示名（通知与错误条里用；超长 URL 不整条糊到界面上）。
fn short_name(uri: &str) -> String {
    let trimmed = uri.trim();
    let without_query = trimmed.split(['?', '#']).next().unwrap_or(trimmed);
    let last = without_query
        .rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or(trimmed);
    if last.chars().count() > 60 {
        let head: String = last.chars().take(60).collect();
        format!("{head}…")
    } else {
        last.to_owned()
    }
}

/// 核心失败时的下一步（与 `core_host` 的文案同源，这里只做 UI 侧兜底）。
fn failure_next_step(code: Option<ErrorCode>) -> String {
    match code {
        Some(ErrorCode::Aria2NotFound) => {
            "先安装核心：`ariahost --aria2-fetch latest`（面板不代装）".to_owned()
        }
        Some(ErrorCode::Aria2StartTimeout) => {
            "看 %APPDATA%\\AriaHost\\logs\\aria2.log；端口被占时改 aria2.rpc.port".to_owned()
        }
        _ => "看底部日志里的第一条失败原因；可重试一次".to_owned(),
    }
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
fn badge(ui: &mut egui::Ui, view: &StatusView) {
    let color = tone_color(view.tone);
    ui.label(
        RichText::new(format!("[{}]", view.badge))
            .color(color)
            .strong(),
    );
}

/// 任务状态 → 颜色（下载中绿、已暂停黄、出错红、其余灰）。
fn status_color(kind: TaskStatus) -> Color32 {
    match kind {
        TaskStatus::Active => tone_color(Tone::Good),
        TaskStatus::Waiting => Color32::from_rgb(150, 190, 240),
        TaskStatus::Paused => tone_color(Tone::Warn),
        TaskStatus::Error => tone_color(Tone::Bad),
        TaskStatus::Complete => Color32::from_rgb(140, 200, 160),
        TaskStatus::Removed | TaskStatus::Unknown => tone_color(Tone::Idle),
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
        assert_eq!(facts.rows, 0);
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
        // 测试机可能没有 %APPDATA%（非 Windows）：返回 None 是预期，不在这里断言平台。
        if let Some(dir) = config_dir() {
            assert!(dir.ends_with("AriaHost"), "{dir:?}");
        }
    }

    #[test]
    fn uri_guard_accepts_the_documented_schemes() {
        for good in [
            "http://127.0.0.1:8080/a.bin",
            "HTTPS://example.com/a.bin",
            "ftp://example.com/a.bin",
            "magnet:?xt=urn:btih:abc",
            "  http://x/y  ",
        ] {
            assert!(looks_like_uri(good), "{good} 应当被接受");
        }
        for bad in ["", "not a url", "C:\\Users\\a.bin", "//example.com/a"] {
            assert!(!looks_like_uri(bad), "{bad} 应当被拒");
        }
    }

    #[test]
    fn short_name_strips_query_and_truncates() {
        assert_eq!(short_name("http://127.0.0.1:8080/a/b.bin?t=1"), "b.bin");
        assert_eq!(short_name("http://127.0.0.1:8080/"), "127.0.0.1:8080");
        let long = format!("http://h/{}", "x".repeat(120));
        let shown = short_name(&long);
        assert!(shown.ends_with('…'), "{shown}");
        assert!(shown.chars().count() <= 61, "{}", shown.chars().count());
    }

    #[test]
    fn status_colors_distinguish_error_from_success() {
        assert_ne!(
            status_color(TaskStatus::Error),
            status_color(TaskStatus::Active)
        );
        assert_eq!(status_color(TaskStatus::Paused), tone_color(Tone::Warn));
    }

    #[test]
    fn banner_helpers_carry_code_and_next_step() {
        let plain = Banner::new(Tone::Warn, "先粘贴链接");
        assert!(plain.code.is_none());
        assert!(plain.next.is_none());

        let coded = Banner::with_code(
            Tone::Bad,
            Some(ErrorCode::RpcHandshakeFailed),
            "连不上 aria2",
        )
        .and_next("先启动 aria2");
        assert_eq!(coded.code, Some("AH-E2003"));
        assert_eq!(coded.next.as_deref(), Some("先启动 aria2"));
    }

    #[test]
    fn failure_next_steps_are_actionable() {
        assert!(failure_next_step(Some(ErrorCode::Aria2NotFound)).contains("--aria2-fetch"));
        assert!(failure_next_step(Some(ErrorCode::Aria2StartTimeout)).contains("aria2.log"));
        assert!(!failure_next_step(None).is_empty());
    }

    #[test]
    fn stopped_core_hides_connection_noise_but_keeps_real_errors() {
        // 已停止时的连接类报错是预期内的噪音（缺陷 2）。
        assert!(tasks_error_is_noise(false, ErrorCode::RpcHandshakeFailed));
        assert!(tasks_error_is_noise(false, ErrorCode::RpcReconnectFailed));
        // 核心在跑时一律照实显示。
        assert!(!tasks_error_is_noise(true, ErrorCode::RpcHandshakeFailed));
        // 非连接类（配置/权限/端口）什么状态下都不能吞。
        assert!(!tasks_error_is_noise(false, ErrorCode::PortConflict));
        assert!(!tasks_error_is_noise(false, ErrorCode::ConfigParseFailed));
    }
}
