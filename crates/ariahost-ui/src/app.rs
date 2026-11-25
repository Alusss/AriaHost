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
use crate::core_setup::{CoreGuide, GuideNotice};
use crate::instance::InstanceLock;
use crate::logbuf::LogBuffer;
use crate::metrics;
use crate::service::{self, ServiceCard};
use crate::tasks::{self, TaskAction, TaskRow, TaskStatus};
use crate::view::{tone_color, StatusView, Tone};
use crate::window::{self, Geometry};
use crate::worker::{
    self, ActionOutcome, AddOutcome, Aria2Update, ServiceSnapshot, TaskRequest, Worker,
    WorkerOptions,
};
use crate::{settings, CloseAction, UiPrefs};

/// 请求窗口尺寸后最多尝试多少帧（约 3 秒 @60fps）；超过就放弃并把 `ok=0`。
const MAX_SIZE_ATTEMPTS: u32 = 200;

/// 尺寸判定容差（物理像素）：DPI 换算与窗口边框会带来 ±1~2 px 误差。
const SIZE_TOLERANCE: i64 = 2;

/// 空闲时的重绘周期：worker 是 `mpsc`，没有事件能唤醒 egui，所以只能定时轮询一次
/// 「通道里有没有新结果」。500ms 足够跟手（刷新本身是 2s 一次），又不会烧 CPU。
const IDLE_REPAINT: Duration = Duration::from_millis(500);

/// 隐藏到托盘后的唤醒周期（2Hz：足够及时处理托盘点击，又不会常驻动画）。
const HIDDEN_WAKE_INTERVAL: Duration = Duration::from_millis(500);

/// 退出流程的总兜底时间（停止链自身有 15s+10s 两段超时，再加余量）。
const QUIT_TIMEOUT: Duration = Duration::from_secs(30);

/// 忙态的重绘周期（启动/停止/行操作期间）。
///
/// 停止链要等 aria2c 自己关闭（实测 4.1–4.3 秒），这段时间**必须**由帧循环自己
/// 排下一帧：后台线程只能往 `mpsc` 里塞结果，推不醒 egui。若沿用 500ms 闲时定时器，
/// 任何一次漏排都会让界面停在「已等待 N 秒」的旧帧上，看起来就是「停止卡住了」。
const BUSY_REPAINT: Duration = Duration::from_millis(100);

/// 核心安装期间的重绘间隔（M3-h）：`setup` 线程只能往 `mpsc` 里塞事件，
/// 推不醒 egui；300ms 足够跟手，又不会把下载线程的 CPU 抢掉。
const CORE_SETUP_REPAINT: Duration = Duration::from_millis(300);
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
/// 退出流程状态。
struct QuitFlow {
    started: Instant,
    /// 为真 = 退出时不停 aria2（让它继续下载）。
    keep_running: bool,
}

/// 「ask」档的面板内关闭对话框状态。
#[derive(Debug, Clone, Copy)]
struct ClosePrompt {
    /// 「记住我的选择」复选框。
    remember: bool,
}

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
    /// 界面行为（关闭三档 / 网页端口）。
    pub prefs: UiPrefs,
    /// 「在浏览器打开」的注入实现（`None` = 本构建没有网页管理页）。
    pub open_web: Option<crate::OpenWebFn>,
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
    /// 首次运行「核心未就绪」引导（M3-h）：在线下载 / 指定本地 / 稍后。
    guide: CoreGuide,
    /// 刷新运行时用（核心刚装好时要按新配置重建 host 与 worker）。
    worker_options: crate::worker::WorkerOptions,
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
    /// 界面行为偏好（关闭三档 / 网页端口）。
    prefs: UiPrefs,
    /// 托盘句柄（仅 Windows；建失败为 `None`，面板照常开）。
    tray: Option<crate::tray::Tray>,
    /// 托盘只尝试建一次（失败不要每帧重试）。
    tray_tried: bool,
    /// 面板窗口当前是否被藏到托盘（左键切换用）。
    window_hidden: bool,
    /// 退出流程（`Some` = 正在「保存会话并停止」）。
    quit: Option<QuitFlow>,
    /// 已提交关窗（自己发的那次 Close 要放行，否则会陷入 CancelClose 循环）。
    exit_committed: bool,
    /// 「ask」档的面板内关闭对话框。
    close_prompt: Option<ClosePrompt>,
    /// 启动时检测到的未完成任务数（信息条用；`None` = 不显示）。
    session_pending: Option<usize>,
    /// 「在浏览器打开」的注入回调（由 bin 提供：只有它链着 ariahost-web/rustls）。
    open_web: Option<crate::OpenWebFn>,
    /// 退出流程已经决定关窗：本帧要抢在 `Close` 生效前把窗口几何写盘。
    quit_save_pending: bool,
    /// 窗口藏到托盘后，后台唤醒线程的开关（`true` = 继续唤醒）。
    hidden_waker: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// 设置页是否打开（M2-4a：设置页替掉主体区，顶部标题栏不动）。
    settings_open: bool,
    /// 设置页编辑状态（首次打开才读盘，之后全是内存副本）。
    settings: crate::settings::Settings,
    /// 隐藏自检驱动用：本帧绘制出来的控件矩形（键 = 配置键 / 文案 / 分组名）。
    pub(crate) drive_targets: crate::drive::Targets,
    /// 设置页里「打开配置文件所在目录」等动作的提示（复用 notice 会串味，单独一条）。
    settings_flash: Option<(String, Instant)>,
    /// 服务管理页是否打开（M3-b：与设置页互斥，同时只画一页）。
    service_open: bool,
    /// 服务页状态机：等待态、结果提示、何时算完成（纯逻辑，见 crate::service）。
    service_card: ServiceCard,
    /// 正在飞的提权调用（后台线程 → mpsc）。Some = 有一条授权流程没结束。
    svc_rx: Option<std::sync::mpsc::Receiver<crate::elevate::ElevateOutcome>>,
    /// 本进程是否已是管理员（启动时算一次；只用于文案，不改变「统一走提权」的策略）。
    elevated: bool,
}

impl PanelApp {
    /// 建面板：先装字体、收集运行事实，再起后台线程。
    pub fn new(cc: &eframe::CreationContext<'_>, init: PanelInit) -> Self {
        Self::build(&cc.egui_ctx, cc.wgpu_render_state.as_ref(), init)
    }

    /// 隐藏自检驱动用：没有 eframe，也**没有 wgpu 渲染态**（驱动只关心控件与状态）。
    pub fn for_drive(ctx: &egui::Context, init: PanelInit) -> Self {
        Self::build(ctx, None, init)
    }

    fn build(
        ctx: &egui::Context,
        render_state: Option<&eframe::egui_wgpu::RenderState>,
        init: PanelInit,
    ) -> Self {
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
            prefs,
            open_web,
        } = init;

        let font = crate::fonts::install(ctx, &log);
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
        match render_state {
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
            core_probe: probe.clone(),
            // M3-h：核心不可用就展开引导卡（判定用的是启动时那次探测，不做每帧 IO）。
            // **只认 AH-E2001**：配置读不出来走它自己的错误路径，不让引导卡冒充。
            guide: CoreGuide::new(crate::core_setup::guide_needed(&probe)),
            worker_options: worker_options.clone(),
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
            prefs,
            tray: None,
            tray_tried: false,
            window_hidden: false,
            quit: None,
            exit_committed: false,
            close_prompt: None,
            session_pending: None,
            open_web,
            quit_save_pending: false,
            hidden_waker: None,
            settings_open: false,
            settings: crate::settings::Settings::new(),
            settings_flash: None,
            service_open: false,
            service_card: ServiceCard::new(),
            svc_rx: None,
            elevated: crate::elevate::is_elevated(),
            drive_targets: crate::drive::Targets::default(),
        }
    }

    /// 驱动断言用：只读访问设置页状态（**不给可变入口**，防止绕过输入路径改 draft）。
    pub fn settings_state(&self) -> &crate::settings::Settings {
        &self.settings
    }

    /// 设置页是否打开。
    pub fn settings_page_open(&self) -> bool {
        self.settings_open
    }

    /// 自测灌行模式（不连任何后端）。
    fn seed_mode(&self) -> bool {
        self.seed_rows.is_some()
    }

    /// 纯自测模式（`--panel-selftest`）：不建托盘、不拦关窗、不读 session。
    fn selftest_mode(&self) -> bool {
        self.selftest_frames.is_some()
    }

    /// 托盘：建一次（失败不重试），每帧同步菜单启用状态并取事件。
    fn pump_tray(&mut self, ctx: &egui::Context) {
        if self.seed_mode() || self.selftest_mode() {
            return;
        }
        if !self.tray_tried {
            self.tray_tried = true;
            self.tray = crate::tray::Tray::new(&self.log);
        }
        let Some(tray) = self.tray.as_mut() else {
            return;
        };
        tray.set_state(self.core.is_running(), self.core.is_busy());
        let actions = tray.poll();
        for action in actions {
            self.log
                .push(format!("托盘：{}", crate::tray::TrayAction::label(action)));
            match action {
                crate::tray::TrayAction::ToggleWindow => {
                    let hidden = self.window_hidden;
                    self.set_window_hidden(ctx, !hidden);
                }
                crate::tray::TrayAction::ShowWindow => self.set_window_hidden(ctx, false),
                crate::tray::TrayAction::Start => self.request_core_start(),
                crate::tray::TrayAction::Stop => self.request_core_stop(),
                crate::tray::TrayAction::OpenWeb => self.open_web_page(),
                crate::tray::TrayAction::Quit => self.begin_quit(ctx, false),
            }
        }
    }

    /// 显示/隐藏面板窗口（托盘左键与菜单「显示面板」共用）。
    fn set_window_hidden(&mut self, ctx: &egui::Context, hidden: bool) {
        self.window_hidden = hidden;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(!hidden));
        if !hidden {
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        if hidden {
            self.notify("已最小化到托盘（左键托盘图标可再打开）");
            self.start_hidden_waker(ctx);
        } else if let Some(flag) = self.hidden_waker.take() {
            // 告诉唤醒线程停：否则它会一直以 2Hz 空转。
            flag.store(true, Ordering::Relaxed);
            self.log.push("窗口恢复显示：已停止托盘唤醒".to_owned());
        }
    }

    /// 隐藏到托盘后仍要能收到托盘事件：窗口不可见时 eframe 通常不再出帧，
    /// 而托盘事件只能在帧回调里 `try_recv`（不能另起线程 `recv`，那是 tray-icon 的约束）。
    /// 所以这里起一个 **2Hz** 的唤醒线程：只 `request_repaint()`，不产生任何动画与绘制负载。
    ///
    /// 为什么不是每帧空转：架构要求「托盘常驻时禁止常驻动画」，2Hz 足够让菜单/左键点击
    /// 在一个周期内被取走，且窗口不可见时 eframe 会把这一帧降到极低开销。
    fn start_hidden_waker(&mut self, ctx: &egui::Context) {
        if self.hidden_waker.is_some() {
            return;
        }
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let ctx = ctx.clone();
        let log = Arc::clone(&self.log);
        let spawned = std::thread::Builder::new()
            .name("ariahost-tray-wake".to_owned())
            .spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    ctx.request_repaint();
                    std::thread::sleep(HIDDEN_WAKE_INTERVAL);
                }
            });
        match spawned {
            Ok(_handle) => {
                self.hidden_waker = Some(stop);
                self.log.push(format!(
                    "已隐藏到托盘：后台每 {}ms 唤醒一次以接收托盘事件（无动画）",
                    HIDDEN_WAKE_INTERVAL.as_millis()
                ));
            }
            Err(error) => {
                log.push(format!(
                    "托盘唤醒线程启动失败：{error}（托盘操作可能无响应）"
                ));
            }
        }
    }

    /// 启动核心（按钮/托盘的公共入口）。
    fn request_core_start(&mut self) {
        match &self.host {
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

    /// 停止核心（按钮/托盘/退出流程的公共入口）。
    fn request_core_stop(&mut self) {
        if let Some(host) = &self.host {
            host.stop();
            self.busy_since = Some(Instant::now());
            self.banner = None;
        }
    }

    /// 开始退出流程：先优雅停止 aria2（实测约 4 秒），停完再关窗。
    ///
    /// `keep_running = true` 用于「退出但让 aria2 继续下载」（本批默认不提供该档，
    /// 由配置 `ui.close_action` 三档决定，保留参数是为了将来加键时不改调用点）。
    fn begin_quit(&mut self, ctx: &egui::Context, keep_running: bool) {
        if self.quit.is_some() {
            return;
        }
        if !keep_running && self.host.is_some() && !self.core.is_stopped() {
            self.request_core_stop();
        }
        self.log.push(if keep_running {
            "退出：按配置不停 aria2（让它继续在后台下载）".to_owned()
        } else {
            format!(
                "退出：先优雅停止 aria2（约需 {} 秒），停完再关面板",
                core_host::STOP_EXPECTED_SECS
            )
        });
        self.quit = Some(QuitFlow {
            started: Instant::now(),
            keep_running,
        });
        self.close_prompt = None;
        // 退出流程要立刻出一帧，否则用户看到的是「点了没反应」。
        ctx.request_repaint();
    }

    /// 退出流程推进：核心停了（或超时兜底）就真的关窗。
    fn pump_quit(&mut self, ctx: &egui::Context) {
        let Some(flow) = &self.quit else {
            return;
        };
        let elapsed = flow.started.elapsed();
        let stopped = flow.keep_running || self.core.is_stopped();
        // 兜底：停止链本身有 15s+10s 的两段超时，再加 5s 余量后无论如何都关窗。
        let timed_out = elapsed > QUIT_TIMEOUT;
        if stopped || timed_out {
            if timed_out && !stopped {
                self.log.push(format!(
                    "退出：等待停止超时（{:.1}s），强制关窗（孤儿由 Job Object 兜底）",
                    elapsed.as_secs_f32()
                ));
            }
            self.quit = None;
            // 先让本帧把几何写盘，再关窗：退出后不该丢窗口位置。
            self.quit_save_pending = true;
            // 标记「已提交关窗」：下一次 close_requested 是我们自己发的，pump_close 必须放行。
            self.exit_committed = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        // 停止过程中持续出帧：既显示「正在保存会话并停止…」，也让忙态计数器走字。
        ctx.request_repaint_after(BUSY_REPAINT);
    }

    /// 点 X 的处理（三档；自测与灌行模式直接放行）。
    fn pump_close(&mut self, ctx: &egui::Context) {
        if self.selftest_mode() || self.seed_mode() || self.quit.is_some() {
            return;
        }
        // 退出流程已经决定关窗：这一帧的 close_requested 就是**我们自己**发的那次 Close，
        // 必须放行。否则会「CancelClose → 重新 begin_quit → 再 Close」无限循环：
        // 实测症状是 aria2 已停、面板停在「已停止」，但进程永不退出（M2-3 真机验收抓到的）。
        if self.exit_committed {
            return;
        }
        if !ctx.input(|input| input.viewport().close_requested()) {
            return;
        }
        // 先撤销本次关闭，改由三档策略决定（进程内对话框也不能靠系统 MessageBox）。
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        match self.prefs.close_action {
            CloseAction::Tray => {
                self.log
                    .push("关闭请求：ui.close_action = tray → 隐藏到托盘".to_owned());
                self.set_window_hidden(ctx, true);
            }
            CloseAction::Exit => {
                self.log
                    .push("关闭请求：ui.close_action = exit → 直接退出".to_owned());
                self.begin_quit(ctx, false);
            }
            CloseAction::Ask => {
                self.log
                    .push("关闭请求：ui.close_action = ask → 面板内询问".to_owned());
                self.close_prompt = Some(ClosePrompt { remember: false });
            }
        }
    }

    // -------------------------------------------------------------------
    // M2-4a 设置页
    // -------------------------------------------------------------------

    /// 打开/关闭设置页。关闭时若还有未保存改动，先问一句（不静默丢改动）。
    fn toggle_settings(&mut self) {
        if self.settings_open {
            if self.settings.is_dirty() {
                self.settings.leaving = true;
            } else {
                self.settings_open = false;
                self.settings.leaving = false;
            }
            return;
        }
        // 只在**第一次**打开时读盘：之后每帧只用内存副本（不许每帧读 config.toml）。
        self.settings.ensure_loaded();
        self.settings.leaving = false;
        self.settings_open = true;
        self.log.push("打开设置页：下载 / 网络 / 界面三组");
    }

    /// 保存设置：先校验（有错不写盘），再走 `config::save` 的原子写；成功后应用「立即生效」项。
    fn save_settings(&mut self) {
        let saved = self.settings.save().is_ok();
        let passed = self
            .settings
            .notice
            .as_ref()
            .is_some_and(|notice| notice.ok);
        if !saved || !passed {
            // 校验没过：notice 里已经说清是哪一项，直接上屏一条错误条。
            if let Some(notice) = self.settings.notice.clone() {
                self.set_banner(Banner::new(Tone::Bad, notice.headline.clone()));
            }
            return;
        }

        // 立即生效 1/2：关闭三档（面板当场按新值处理点 X）。
        let action = CloseAction::parse(&self.settings.draft().ui.close_action);
        self.prefs.close_action = action;
        self.log.push(format!(
            "设置已保存：ui.close_action = {}（立即生效）",
            action.as_str()
        ));

        // 立即生效 2/2：开机自启（HKCU Run，免提权；不用计划任务）。
        let want = self.settings.draft().ui.start_with_windows;
        match std::env::current_exe() {
            Ok(exe) => match settings::apply_start_with_windows(want, &exe) {
                Ok(()) => {
                    self.log.push(if want {
                        // 日志里带上真正写进注册表的值（含引号），排障时能直接复制核对。
                        format!("已写入 HKCU Run：{}", settings::run_value(&exe))
                    } else {
                        "已清除 HKCU Run（ui.start_with_windows = false）".to_owned()
                    });
                }
                Err(error) => {
                    let headline = format!("配置已保存，但开机自启写入失败：{error}");
                    self.log.push(headline.clone());
                    self.settings.notice = Some(settings::Notice::err(
                        headline,
                        "确认 reg.exe 可用（Windows 自带）；或先用「任务管理器 → 启动」手动加",
                    ));
                }
            },
            Err(error) => {
                self.log
                    .push(format!("取不到本程序路径，跳过开机自启：{error}"));
            }
        }

        self.set_banner(Banner::new(
            Tone::Good,
            "设置已保存到 config.toml（原子写）；重启后生效的项见列表标注",
        ));
    }

    // -------------------------------------------------------------------
    // M3-b 服务管理页
    // -------------------------------------------------------------------

    /// 打开/关闭服务管理页。与设置页互斥：同时只画一页。
    fn toggle_service(&mut self) {
        if self.service_open {
            self.service_open = false;
            if let Some(worker) = &self.worker {
                worker.set_service_page(false);
            }
            return;
        }
        // 进服务页 = 离开设置页（不静默丢掉草稿：设施页只是暂时不画，改动仍在内存里）。
        self.settings_open = false;
        self.settings.leaving = false;
        self.service_open = true;
        if let Some(worker) = &self.worker {
            // 切页时把服务轮询调到 2s，并立刻拉一次；**不是每帧**。
            worker.set_service_page(true);
        }
        self.log
            .push("打开服务管理页：六态卡片 + 装/卸/启停（都要一次 Windows 授权）".to_owned());
    }

    /// 点一个服务操作：统一走「提权助手」，一条路径一种文案。
    ///
    /// 四个动作都改 SCM，非管理员必然 `AH-E5002`（实测），所以**不做「先试库调用再提权」**：
    /// 那样会出现「有时弹框有时不弹」的两套体验。提权一律在后台线程里发，
    /// 因为 `ShellExecuteExW(runas)` 在本会话实测会阻塞约 122 秒才返回取消。
    fn start_service_op(&mut self, op: service::Op, ctx: &egui::Context) {
        if self.svc_rx.is_some() {
            self.log
                .push("上一条授权流程还没结束：先等它返回（重复点不会加速）".to_owned());
            return;
        }
        let Some(target) = elevate_target(op) else {
            self.service_card.settle(
                service::NoticeTone::Bad,
                "找不到可执行文件路径（current_exe / SystemRoot 不可用），无法发起授权。",
            );
            return;
        };

        let (tx, rx) = std::sync::mpsc::channel();
        let timeout = op.budget();
        let spawned = std::thread::Builder::new()
            .name("ariahost-svc-elevate".to_owned())
            .spawn(move || {
                let outcome = crate::elevate::run_elevated(&target.file, &target.args, timeout);
                let _ = tx.send(outcome);
            });
        if let Err(error) = spawned {
            self.service_card.settle(
                service::NoticeTone::Bad,
                format!("提权线程起不来：{error}（服务未变更）"),
            );
            return;
        }

        self.svc_rx = Some(rx);
        self.service_card.begin(op);
        self.log.push(format!(
            "{}：请求 Windows 授权（会弹一次 UAC；点取消不会改动任何东西）",
            op.button()
        ));
        ctx.request_repaint();
    }

    /// 提权子进程给出结论后的处理（取消 / 失败 / 成功提交）。
    fn on_elevate_outcome(&mut self, outcome: crate::elevate::ElevateOutcome, ctx: &egui::Context) {
        use crate::elevate::ElevateOutcome as E;
        let Some(op) = self.service_card.pending() else {
            self.log
                .push(format!("提权流程返回，但界面已无等待中的操作：{outcome:?}"));
            return;
        };
        match outcome {
            E::Completed {
                exit_code: 0,
                waited,
            } => {
                self.log.push(format!(
                    "{}：授权通过，助手进程成功返回（{:.1}s）；等 SCM 反映状态",
                    op.button(),
                    waited.as_secs_f32()
                ));
                // 立刻连拉三次（1s 一次），让等待态几秒内看到真实跳变。
                if let Some(worker) = &self.worker {
                    worker.burst_service();
                }
            }
            E::Completed { exit_code, waited } => {
                let (headline, next) = service::hint_for(ErrorCode::ServiceUnavailable);
                self.log.push(format!(
                    "{}：助手进程退出码 {exit_code}（{:.1}s）——{headline}",
                    op.button(),
                    waited.as_secs_f32()
                ));
                self.service_card.settle(
                    service::NoticeTone::Bad,
                    format!("{headline}（退出码 {exit_code}）"),
                );
                self.set_banner(
                    Banner::new(Tone::Bad, format!("{headline}（退出码 {exit_code}）"))
                        .and_next(next),
                );
            }
            E::Cancelled { waited } => {
                // 取消不是错误：只写信息条，**不弹红条**（task-46 硬要求）。
                self.log.push(format!(
                    "{}：授权被取消/未确认（{:.1}s），服务未变更",
                    op.button(),
                    waited.as_secs_f32()
                ));
                self.service_card.settle(
                    service::NoticeTone::Info,
                    service::elevation_cancelled_message(waited),
                );
            }
            E::LaunchFailed { error, detail } => {
                self.log
                    .push(format!("{}：授权框发不出来：{detail}", op.button()));
                let headline = "无法弹出 Windows 授权框：服务未变更";
                self.service_card.settle(service::NoticeTone::Bad, headline);
                self.set_banner(
                    Banner::new(Tone::Bad, format!("{headline}（Win32={error}）"))
                        .and_next("右键面板 →「以管理员身份运行」，再从服务页点一次"),
                );
            }
            E::WaitTimedOut { waited } => {
                self.log.push(format!(
                    "{}：助手进程未在预算内结束（{:.1}s）",
                    op.button(),
                    waited.as_secs_f32()
                ));
                self.service_card
                    .finish(op, service::NoticeOutcome::TimedOut);
            }
        }
        ctx.request_repaint();
    }

    /// 每帧一次的非阻塞收尾：提权结果 + SCM 快照 + 等待预算。
    fn pump_service(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.svc_rx {
            match rx.try_recv() {
                Ok(outcome) => {
                    self.svc_rx = None;
                    self.on_elevate_outcome(outcome, ctx);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    // 授权框开着的时候也要出帧：等待态里的「已等 X 秒」得走字。
                    ctx.request_repaint_after(Duration::from_millis(120));
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.svc_rx = None;
                    self.service_card.settle(
                        service::NoticeTone::Bad,
                        "授权流程意外中断（后台线程没了），服务未变更。",
                    );
                }
            }
        }

        // SCM 是唯一事实来源：达到目标状态就收尾（用户也可能在别处点了启动）。
        let state = self.service.and_then(|snapshot| snapshot.state());
        if let Some((op, _outcome)) = self.service_card.observe(state) {
            self.log
                .push(format!("{}：SCM 已到达目标状态", op.button()));
            if let Some(worker) = &self.worker {
                worker.burst_service();
            }
        }
        if let Some((op, _outcome)) = self.service_card.timeout_if_due(Instant::now()) {
            self.log.push(format!(
                "{}：超出等待预算，界面不再等（SCM 可能稍后完成，可点「刷新状态」）",
                op.button()
            ));
        }
    }

    /// 服务页：顶部说明 + 中间状态卡片 + 底部操作条。
    fn show_service_page(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("ariahost_service_head")
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("Windows 服务");
                    ui.label(
                        RichText::new("把 aria2 装成系统服务：开机自启，面板关掉也能继续下载")
                            .weak(),
                    );
                });
                ui.add_space(2.0);
            });

        egui::Panel::bottom("ariahost_service_actions")
            .show_separator_line(true)
            .show(ui, |ui| {
                self.show_service_buttons(ui);
            });

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                self.show_service_card(ui);
            });
        });
    }

    /// 六态状态卡片：状态 / 启动方式 / PID / 等待态 / 结果提示。
    fn show_service_card(&mut self, ui: &mut egui::Ui) {
        let snapshot = self.service;
        let state = snapshot.and_then(|snapshot| snapshot.state());

        let (label, color) = match (state, snapshot) {
            (Some(state), _) => (service::state_label(state), service_state_color(state)),
            (None, Some(ServiceSnapshot::Failed { .. })) => {
                ("查询失败", Color32::from_rgb(230, 140, 140))
            }
            (None, _) => ("读取中…", Color32::GRAY),
        };

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("状态").weak());
            let badge = ui.label(RichText::new(label).strong().size(20.0).color(color));
            self.drive_targets.note("服务状态", badge.rect);
            ui.label(
                RichText::new(format!("服务名 {}", ariahost_svc::SERVICE_NAME))
                    .weak()
                    .small(),
            );
        });

        if let Some(state) = state {
            ui.label(RichText::new(service::state_hint(state)).weak());
        }

        ui.add_space(6.0);
        egui::Grid::new("ariahost_service_facts")
            .num_columns(2)
            .spacing([12.0, 4.0])
            .show(ui, |ui| {
                ui.label(RichText::new("启动方式").weak());
                let start_mode = match snapshot {
                    Some(ServiceSnapshot::State(report)) => report.start_type,
                    _ => None,
                };
                ui.label(service::start_mode_label(start_mode));
                ui.end_row();

                ui.label(RichText::new("PID").weak());
                let pid = match snapshot {
                    Some(ServiceSnapshot::State(report)) => report.pid,
                    _ => None,
                };
                ui.label(pid.map_or_else(
                    || "—（未运行或 SCM 未给）".to_owned(),
                    |pid| pid.to_string(),
                ));
                ui.end_row();

                ui.label(RichText::new("查询失败原因").weak());
                match snapshot {
                    Some(ServiceSnapshot::Failed { code, .. }) => {
                        ui.label(
                            RichText::new(format!("{}（详见日志）", code.code()))
                                .color(Color32::from_rgb(230, 160, 140)),
                        );
                    }
                    _ => {
                        ui.label(RichText::new("—").weak());
                    }
                }
                ui.end_row();
            });

        // 等待态：进度 + 说明 + 计时（停止是两段式，见 `service::waiting_text`）。
        if let Some((progress, hint, counter)) =
            service::waiting_text(self.service_card.stage(), Instant::now())
        {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.spinner();
                let line = ui.label(RichText::new(progress).strong());
                self.drive_targets.note("服务等待态", line.rect);
            });
            ui.label(RichText::new(hint).weak());
            if let Some(counter) = counter {
                ui.label(RichText::new(counter).weak().small());
            }
        }

        // 结果提示：成功绿 / 信息灰（取消授权走这里，**不是红条**）/ 失败红。
        if let Some((tone, message)) = self.service_card.notice().cloned() {
            ui.add_space(8.0);
            let color = match tone {
                service::NoticeTone::Ok => Color32::from_rgb(140, 200, 160),
                service::NoticeTone::Info => Color32::from_rgb(150, 190, 240),
                service::NoticeTone::Bad => Color32::from_rgb(230, 140, 140),
            };
            let response = ui.label(RichText::new(message).color(color));
            self.drive_targets.note("服务结果提示", response.rect);
        }

        if !self.elevated {
            ui.add_space(6.0);
            ui.label(
                RichText::new(
                    "当前以普通权限运行：每次装/卸/启停都会弹一次 Windows 授权框。\
                     想免掉它，可右键面板 →「以管理员身份运行」。",
                )
                .weak()
                .small(),
            );
        }

        ui.add_space(10.0);
        ui.separator();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui.button("查看服务日志").clicked() {
                self.open_service_log();
            }
            if ui.button("刷新状态").clicked() {
                if let Some(worker) = &self.worker {
                    worker.burst_service();
                }
                self.log.push("手动刷新服务状态".to_owned());
            }
            ui.label(
                RichText::new("服务日志在 %ProgramData%\\AriaHost\\logs，需管理员权限才写得进去")
                    .weak()
                    .small(),
            );
        });
    }

    /// 底部四个按钮：可用性完全由六态决定（`service::buttons`），等待中一律禁用。
    fn show_service_buttons(&mut self, ui: &mut egui::Ui) {
        let state = self.service.and_then(|snapshot| snapshot.state());
        let allowed = state.map_or(service::Buttons::NONE, service::buttons);
        let busy = self.svc_rx.is_some() || self.service_card.pending().is_some();

        let mut clicked = None;
        ui.horizontal(|ui| {
            for (op, enabled) in [
                (service::Op::Install, allowed.install),
                (service::Op::Start, allowed.start),
                (service::Op::Stop, allowed.stop),
                (service::Op::Uninstall, allowed.uninstall),
            ] {
                let response = ui.add_enabled(enabled && !busy, egui::Button::new(op.button()));
                self.drive_targets.note(op.button(), response.rect);
                if response.clicked() {
                    clicked = Some(op);
                }
            }
            if busy {
                ui.label(RichText::new("有一条授权流程在进行…").weak());
            }
        });
        if let Some(op) = clicked {
            let ctx = ui.ctx().clone();
            self.start_service_op(op, &ctx);
        }
    }

    /// 打开服务日志目录（日志文件本身要管理员才读写，先开目录比开文件稳）。
    fn open_service_log(&mut self) {
        let dir = ariahost_common::paths::logs_dir(ariahost_common::paths::Plane::Service);
        match dir {
            Some(dir) => match open_in_file_manager(&dir) {
                Ok(()) => self
                    .log
                    .push(format!("已打开服务日志目录：{}", dir.display())),
                Err(error) => {
                    self.set_banner(
                        Banner::new(Tone::Bad, format!("打开服务日志目录失败：{error}"))
                            .and_next("手动打开 %ProgramData%\\AriaHost\\logs"),
                    );
                }
            },
            None => {
                self.set_banner(
                    Banner::new(Tone::Bad, "拿不到服务日志目录（%ProgramData% 不可用）")
                        .and_next("手动打开 %ProgramData%\\AriaHost\\logs"),
                );
            }
        }
    }

    /// 设置页：顶栏（配置文件路径）+ 未保存提示 + 左导航 + 右内容 + 底部操作条。
    fn show_settings(&mut self, ui: &mut egui::Ui) {
        // 底部固定操作条：先画，保证它永远在底部（不被内容顶走）。
        egui::Panel::bottom("ariahost_settings_actions")
            .show_separator_line(true)
            .show(ui, |ui| {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    let dirty = self.settings.is_dirty();
                    let has_error = self.settings.has_errors();
                    let can_save = dirty && !has_error;
                    let save_response = ui
                        .add_enabled(can_save, egui::Button::new("保存"))
                        .on_hover_text("写入 %APPDATA%\\AriaHost\\config.toml（原子写）");
                    self.drive_targets.note("保存", save_response.rect);
                    if save_response.clicked() {
                        self.save_settings();
                    }
                    let undo_response = ui.add_enabled(dirty, egui::Button::new("撤销更改"));
                    self.drive_targets.note("撤销更改", undo_response.rect);
                    if undo_response.clicked() {
                        self.settings.undo();
                    }
                    let reset_response = ui.button("恢复默认");
                    self.drive_targets.note("恢复默认", reset_response.rect);
                    if reset_response.clicked() {
                        self.settings.reset_defaults();
                    }
                    ui.separator();
                    // M3-h：核心来源的**常驻入口**——卡片收掉（「稍后再说」）之后，
                    // 用户还能从这里把引导叫回来，不必去找命令行。
                    let core_response = ui.button("重新配置下载核心");
                    if core_response
                        .on_hover_text(
                            "重新打开「下载核心未就绪」卡片（在线下载 / 指定已有 aria2c）",
                        )
                        .clicked()
                    {
                        self.guide.reopen();
                        self.log.push("设置页：重新打开核心引导卡".to_owned());
                    }
                    ui.separator();
                    if dirty {
                        ui.label(RichText::new("有未保存的改动").color(tone_color(Tone::Warn)));
                    } else {
                        ui.label(RichText::new("没有改动").weak());
                    }
                    if let Some((text, at)) = &self.settings_flash {
                        if at.elapsed() < NOTICE_TIMEOUT {
                            ui.label(RichText::new(text.clone()).weak());
                        }
                    }
                });
                ui.add_space(2.0);
            });

        // 顶栏：配置文件的真实位置 + 打开目录。
        ui.horizontal(|ui| {
            ui.label(RichText::new("配置文件").weak());
            match settings::Settings::config_path() {
                Some(path) => {
                    ui.monospace(path.display().to_string());
                    if ui.button("打开配置文件所在目录").clicked() {
                        let dir = path.parent().map(Path::to_path_buf);
                        match dir {
                            Some(dir) => match open_in_file_manager(&dir) {
                                Ok(()) => {
                                    self.settings_flash =
                                        Some(("已打开配置目录".to_owned(), Instant::now()))
                                }
                                Err(error) => {
                                    self.settings_flash =
                                        Some((format!("打开目录失败：{error}"), Instant::now()))
                                }
                            },
                            None => {
                                self.settings_flash =
                                    Some(("配置路径没有父目录".to_owned(), Instant::now()))
                            }
                        }
                    }
                }
                None => {
                    ui.label(
                        RichText::new("拿不到 %APPDATA%（配置不可用）")
                            .color(tone_color(Tone::Bad)),
                    );
                }
            }
        });

        // 保存/校验结果。
        if let Some(notice) = self.settings.notice.clone() {
            let color = if notice.ok {
                tone_color(Tone::Good)
            } else {
                tone_color(Tone::Bad)
            };
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(notice.headline).color(color));
                if let Some(next) = notice.next {
                    ui.label(RichText::new(format!("→ {next}")).weak());
                }
            });
        }

        // 未保存改动时离开：三个选择，绝不静默丢弃。
        if self.settings.leaving {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new("有未保存的改动，确定离开设置页？").color(tone_color(Tone::Warn)),
                );
                if ui.button("保存并离开").clicked() {
                    self.save_settings();
                    if self
                        .settings
                        .notice
                        .as_ref()
                        .is_some_and(|notice| notice.ok)
                    {
                        self.settings_open = false;
                        self.settings.leaving = false;
                    }
                }
                if ui.button("放弃改动并离开").clicked() {
                    self.settings.undo();
                    self.settings_open = false;
                    self.settings.leaving = false;
                }
                if ui.button("取消").clicked() {
                    self.settings.leaving = false;
                }
            });
            ui.separator();
        }

        ui.add_space(2.0);
        ui.horizontal_top(|ui| {
            // 左侧分组导航（固定宽，避免内容宽度变化时抖）。
            ui.allocate_ui_with_layout(
                egui::vec2(96.0, ui.available_height()),
                egui::Layout::top_down(egui::Align::Min),
                |ui| {
                    for group in settings::Group::ALL {
                        let selected = self.settings.group() == group;
                        // 用真 `Button`（`.selected()` 出高亮）而不是 `selectable_label`：
                        // 后者在 accesskit 里不作为可执行控件暴露，屏幕阅读器/UIA **点不动**，
                        // 等于用键盘或辅助技术的人无法切换分组（真机验收时正是卡在这里）。
                        let response = ui.add(egui::Button::new(group.label()).selected(selected));
                        self.drive_targets.note(group.label(), response.rect);
                        if response.clicked() {
                            self.settings.set_group(group);
                        }
                    }
                },
            );
            ui.separator();
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| match self.settings.group() {
                    settings::Group::Download => self.show_settings_download(ui),
                    settings::Group::Network => self.show_settings_network(ui),
                    settings::Group::Interface => self.show_settings_interface(ui),
                });
        });
    }

    /// 一组「标签 + 输入框 + 默认值 + 就地错误」的通用行。
    fn settings_text_row(
        &mut self,
        ui: &mut egui::Ui,
        key: &'static str,
        label: &str,
        hint: &str,
        width: f32,
    ) {
        ui.horizontal(|ui| {
            ui.add_sized([150.0, 18.0], egui::Label::new(label));
            let mut text = self.settings.text(key).to_owned();
            let response = ui.add(
                egui::TextEdit::singleline(&mut text)
                    .desired_width(width)
                    .hint_text(hint),
            );
            self.drive_targets.note(key, response.rect);
            if response.changed() {
                self.settings.set_text(key, text);
            }
            let default = settings::default_text(key);
            if !default.is_empty() {
                ui.label(RichText::new(format!("默认 {default}")).weak());
            }
        });
        if let Some(reason) = self.settings.error(key) {
            ui.horizontal(|ui| {
                ui.add_space(150.0);
                ui.label(RichText::new(reason.to_owned()).color(tone_color(Tone::Bad)));
            });
        }
    }

    /// 一行说明（生效时机 / 只存不执行的诚实标注）。
    fn settings_note(ui: &mut egui::Ui, key: &str, extra: &str) {
        let effect = settings::effect(key);
        let color = match effect {
            settings::Effect::Immediate => tone_color(Tone::Good),
            settings::Effect::Restart => tone_color(Tone::Warn),
            settings::Effect::NotWired => tone_color(Tone::Bad),
        };
        ui.horizontal(|ui| {
            ui.add_space(150.0);
            ui.label(RichText::new(effect.text()).color(color).small());
            if !extra.is_empty() {
                ui.label(RichText::new(extra).weak().small());
            }
        });
    }

    /// A 组：下载（`[dl]`）。
    fn show_settings_download(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("下载").strong());
        ui.label(
            RichText::new(
                "落盘目录与文件命名；这些值会渲染进 aria2.conf，改完需重启 aria2 才生效。",
            )
            .weak(),
        );
        ui.add_space(4.0);
        self.settings_text_row(
            ui,
            "dl.dir",
            "下载目录",
            "必须是已存在且可写的目录，例如 D:\\Downloads",
            280.0,
        );
        Self::settings_note(
            ui,
            "dl.dir",
            "本批不给「浏览…」按钮：仓库里没有 rfd，不为选目录引新依赖",
        );
        ui.add_space(6.0);

        let mut renamed = self.settings.draft().dl.auto_file_renaming;
        if ui
            .checkbox(
                &mut renamed,
                "重名文件自动改名（auto-file-renaming，默认 true）",
            )
            .changed()
        {
            self.settings.draft_mut().dl.auto_file_renaming = renamed;
        }
        let mut overwrite = self.settings.draft().dl.allow_overwrite;
        if ui
            .checkbox(
                &mut overwrite,
                "允许覆盖同名文件（allow-overwrite，默认 false）",
            )
            .changed()
        {
            self.settings.draft_mut().dl.allow_overwrite = overwrite;
        }
        let mut continue_dl = self.settings.draft().dl.continue_dl;
        if ui
            .checkbox(&mut continue_dl, "断点续传（continue，默认 true）")
            .changed()
        {
            self.settings.draft_mut().dl.continue_dl = continue_dl;
        }
        let mut always_resume = self.settings.draft().dl.always_resume;
        if ui
            .checkbox(&mut always_resume, "总是续传（always-resume，默认 true）")
            .changed()
        {
            self.settings.draft_mut().dl.always_resume = always_resume;
        }
    }

    /// B 组：网络（`[net]`）。
    fn show_settings_network(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("网络").strong());
        ui.label(
            RichText::new("并发、分片与限速。尺寸要写成「数字+单位」（20M / 512K / 1G）；这条规则是 aria2 的，写成 20MB 会被它拒掉。")
                .weak(),
        );
        ui.add_space(4.0);
        self.settings_text_row(ui, "net.split", "分片数 split", "1–16", 80.0);
        self.settings_text_row(
            ui,
            "net.max_connection_per_server",
            "单服务器连接数",
            "1–16",
            80.0,
        );
        self.settings_text_row(
            ui,
            "net.min_split_size",
            "最小分片 min-split-size",
            "≥ 1M",
            120.0,
        );
        self.settings_text_row(
            ui,
            "net.max_overall_download_limit",
            "全局限速（下载）",
            "0 = 不限速",
            120.0,
        );
        self.settings_text_row(
            ui,
            "net.max_download_limit",
            "单任务限速",
            "0 = 不限速",
            120.0,
        );
        self.settings_text_row(
            ui,
            "net.max_overall_upload_limit",
            "全局限速（上传）",
            "0 = 不限速",
            120.0,
        );
        self.settings_text_row(ui, "net.connect_timeout", "连接超时（秒）", "1–600", 80.0);
        self.settings_text_row(ui, "net.max_tries", "重试次数", "0–100（0 = 不限）", 80.0);
        self.settings_text_row(ui, "net.proxy", "代理", "空 = 不使用代理", 280.0);
        Self::settings_note(
            ui,
            "net.proxy",
            "形如 http://127.0.0.1:7890 或 socks5h://127.0.0.1:1080；任务级代理，与线路出口无关",
        );
        self.settings_text_row(
            ui,
            "net.disk_cache",
            "内存缓存 disk-cache",
            "16M / 0 = 关",
            120.0,
        );
    }

    /// C 组：界面（`[ui]` / `[lifecycle]`）。
    fn show_settings_interface(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("界面").strong());
        ui.label(
            RichText::new("关闭行为、渲染器与开机自启。标红的项本批只写配置、行为还没接线——不会骗你说已经生效。")
                .weak(),
        );
        ui.add_space(4.0);

        // ui.close_action：三档，保存后立即生效。
        // 用单选按钮而不是下拉：三项本来就该一眼看全，且 `ComboBox` 的弹出项在 accesskit 里
        // 不是可执行控件（UIA/屏幕阅读器点不动），真机验收与辅助技术都会卡住。
        ui.horizontal(|ui| {
            ui.add_sized([150.0, 18.0], egui::Label::new("关闭按钮行为"));
            let current = self.settings.draft().ui.close_action.clone();
            for (value, text) in [
                ("tray", "隐藏到托盘（默认）"),
                ("ask", "每次询问"),
                ("exit", "直接退出（按退出流程停 aria2）"),
            ] {
                let mut checked = current == value;
                let radio = ui.radio_value(&mut checked, true, text);
                self.drive_targets.note(text, radio.rect);
                if radio.changed() && checked {
                    self.settings.draft_mut().ui.close_action = value.to_owned();
                    self.settings.revalidate();
                }
            }
        });
        if let Some(reason) = self.settings.error("ui.close_action") {
            ui.label(RichText::new(reason.to_owned()).color(tone_color(Tone::Bad)));
        }
        Self::settings_note(ui, "ui.close_action", "保存后立刻按新值处理点 X");

        // ui.renderer：auto | wgpu | glow（架构 §4.11 第 34 项，逃生口；不是 --backend 的 dx12/vulkan）。
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add_sized([150.0, 18.0], egui::Label::new("渲染器 renderer"));
            let current = self.settings.draft().ui.renderer.clone();
            for (value, text) in [
                ("auto", "auto（默认）"),
                ("wgpu", "wgpu"),
                ("glow", "glow（逃生口，本构建未编译）"),
            ] {
                let mut checked = current == value;
                let radio = ui.radio_value(&mut checked, true, text);
                self.drive_targets.note(text, radio.rect);
                if radio.changed() && checked {
                    self.settings.draft_mut().ui.renderer = value.to_owned();
                    self.settings.revalidate();
                }
            }
        });
        if self.settings.draft().ui.renderer.trim() == "glow" {
            ui.horizontal(|ui| {
                ui.add_space(150.0);
                ui.label(
                    RichText::new(
                        "当前构建只带 wgpu（EG-3）：保存后仍按 wgpu 启动，日志会记一条回退说明",
                    )
                    .color(tone_color(Tone::Warn)),
                );
            });
        }
        Self::settings_note(ui, "ui.renderer", "重启面板生效");

        // ui.start_minimized / ui.minimize_to_tray：本批只写配置（架构标 ❌）。
        ui.add_space(6.0);
        let mut start_min = self.settings.draft().ui.start_minimized;
        if ui
            .checkbox(
                &mut start_min,
                "启动时最小化到托盘（start_minimized，默认 true）",
            )
            .changed()
        {
            self.settings.draft_mut().ui.start_minimized = start_min;
        }
        Self::settings_note(ui, "ui.start_minimized", "");
        let mut min_to_tray = self.settings.draft().ui.minimize_to_tray;
        if ui
            .checkbox(
                &mut min_to_tray,
                "允许最小化到托盘（minimize_to_tray，默认 true）",
            )
            .changed()
        {
            self.settings.draft_mut().ui.minimize_to_tray = min_to_tray;
        }
        Self::settings_note(ui, "ui.minimize_to_tray", "");

        // ui.start_with_windows：HKCU Run，保存后立即生效，不需要管理员。
        ui.add_space(6.0);
        let mut autostart = self.settings.draft().ui.start_with_windows;
        if ui
            .checkbox(
                &mut autostart,
                "开机自启（start_with_windows，写 HKCU Run，不用计划任务）",
            )
            .changed()
        {
            self.settings.draft_mut().ui.start_with_windows = autostart;
        }
        Self::settings_note(ui, "ui.start_with_windows", "保存时就写/删注册表值");

        // lifecycle.quit_default：三选一（架构 §7-R 的三种退出选项），本批只写配置。
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.add_sized([150.0, 18.0], egui::Label::new("退出时默认动作"));
            let current = self.settings.draft().lifecycle.quit_default.clone();
            for (value, text) in [
                ("tray", "收进托盘继续下载"),
                ("pause", "暂停任务后退出（默认）"),
                ("stop", "停止任务后退出"),
            ] {
                let mut checked = current == value;
                let radio = ui.radio_value(&mut checked, true, text);
                self.drive_targets.note(text, radio.rect);
                if radio.changed() && checked {
                    self.settings.draft_mut().lifecycle.quit_default = value.to_owned();
                    self.settings.revalidate();
                }
            }
        });
        Self::settings_note(
            ui,
            "lifecycle.quit_default",
            "值域取自架构 §7-R 的三种退出选项；文档没给枚举清单，已上报 Lead",
        );
    }

    /// 启动检测：`session\aria2.session` 非空 → 顶部信息条（不是弹窗）。
    fn detect_pending_session(&mut self) {
        if self.seed_mode() || self.selftest_mode() || self.session_pending.is_some() {
            return;
        }
        let count = pending_session_count();
        match count {
            Some(0) | None => {}
            Some(count) => {
                self.log
                    .push(format!("启动检测：session 里有 {count} 个未完成任务"));
                self.session_pending = Some(count);
            }
        }
    }

    /// 打开网页管理页：真正起服务由 bin 注入的回调负责（ui 不链 ariahost-web）。
    fn open_web_page(&mut self) {
        let Some(open) = &self.open_web else {
            self.set_banner(Banner::new(
                Tone::Warn,
                "当前构建没有接入网页管理页（命令行/自测模式下没有注入）。",
            ));
            return;
        };
        match open() {
            Ok(url) => {
                self.log.push(format!("网页管理页：{url}"));
                match open_url(&url) {
                    Ok(()) => self.notify(format!("已在浏览器打开 {url}")),
                    Err(error) => self.set_banner(Banner::new(
                        Tone::Warn,
                        format!("打不开浏览器（{error}）；手工访问 {url}"),
                    )),
                }
            }
            Err(detail) => {
                let code = ErrorCode::PortConflict;
                self.set_banner(
                    Banner::with_code(
                        Tone::Bad,
                        Some(code),
                        format!("{} 网页管理页启动失败：{detail}", code.code()),
                    )
                    .and_next("改 web.port 换一个端口，或关掉占着它的程序后重试"),
                );
            }
        }
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

    /// 退出流程的可见证据：**不能直接消失**（需求第 7 条）。
    fn show_quit_line(&mut self, ui: &mut egui::Ui) {
        let Some(flow) = &self.quit else {
            return;
        };
        let waited = flow.started.elapsed().as_secs_f32();
        let text = if flow.keep_running {
            "正在退出；aria2 继续在后台下载（按配置不停止）".to_owned()
        } else {
            format!(
                "正在保存会话并停止 aria2，约需 {} 秒…（已等待 {waited:.1} 秒）",
                core_host::STOP_EXPECTED_SECS
            )
        };
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(RichText::new(text).color(tone_color(Tone::Warn)).small());
        });
    }

    /// 启动检测信息条（不是弹窗）：继续 = 启动 aria2 读 session 续传；忽略 = 收起。
    fn show_session_bar(&mut self, ui: &mut egui::Ui) {
        let Some(count) = self.session_pending else {
            return;
        };
        let mut start = false;
        let mut dismiss = false;
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("检测到 {count} 个未完成任务，是否继续？"))
                    .color(tone_color(Tone::Good))
                    .small(),
            );
            if ui.small_button("继续").clicked() {
                start = true;
            }
            if ui.small_button("忽略").clicked() {
                dismiss = true;
            }
        });
        if start {
            self.session_pending = None;
            self.log
                .push("启动检测：用户选择继续未完成任务（启动 aria2 读 session 续传）".to_owned());
            self.request_core_start();
        }
        if dismiss {
            self.session_pending = None;
            self.log.push("启动检测：用户选择忽略未完成任务".to_owned());
        }
    }

    /// `ask` 档的面板内对话框（绝不用系统 MessageBox，且必须带真实任务数）。
    fn show_close_prompt(&mut self, ctx: &egui::Context) {
        let Some(mut prompt) = self.close_prompt.take() else {
            return;
        };
        let downloading = self.downloading_count();
        let mut to_tray = false;
        let mut quit = false;
        let mut cancel = false;
        egui::Window::new("退出 AriaHost？")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(format!(
                        "aria2 正在下载 {downloading} 个任务，退出会中断它们。"
                    ))
                    .small(),
                );
                ui.label(
                    RichText::new("最小化到托盘可以让下载继续；退出会先优雅停止 aria2。")
                        .weak()
                        .small(),
                );
                ui.checkbox(&mut prompt.remember, "记住我的选择");
                ui.horizontal(|ui| {
                    if ui.button("最小化到托盘").clicked() {
                        to_tray = true;
                    }
                    if ui.button("退出并停止下载").clicked() {
                        quit = true;
                    }
                    if ui.button("取消").clicked() {
                        cancel = true;
                    }
                });
            });
        self.close_prompt = if cancel || to_tray || quit {
            None
        } else {
            Some(prompt)
        };
        if to_tray {
            if prompt.remember {
                self.prefs.close_action = CloseAction::Tray;
                self.prefs
                    .remember_close_action(CloseAction::Tray, &self.log);
            }
            self.set_window_hidden(ctx, true);
        }
        if quit {
            if prompt.remember {
                self.prefs.close_action = CloseAction::Exit;
                self.prefs
                    .remember_close_action(CloseAction::Exit, &self.log);
            }
            self.begin_quit(ctx, false);
        }
    }

    /// 正在下载的任务数（对话框文案用真实数，不编）。
    fn downloading_count(&self) -> usize {
        let real = self.tasks.iter().filter(|row| row.kind.can_pause()).count();
        real.max(self.optimistic.len())
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
    /// 核心是否可用：`core_probe` 是**启动快照**，刚装好的情形靠 `guide` 补上。
    fn core_usable(&self) -> bool {
        self.core_probe.ready() || self.guide.core_usable()
    }

    /// 消费核心安装事件 + 走看门狗（每帧一次，全非阻塞）。
    fn pump_core_guide(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        self.guide.tick(now);
        let out = self.guide.poll(now);
        for line in out.logs {
            self.log.push(line);
        }
        if out.refresh_runtime {
            // 卡片在成功时会收起来 —— 收起来就**等于没有确认**。
            // 所以把结论同时写进顶部信息条：用户看到「装好了」而不是「卡片莫名消失」。
            let headline = match &self.guide.notice {
                Some(GuideNotice::Online {
                    version,
                    line_note,
                    hash_note,
                    ..
                }) => Some(format!(
                    "下载核心已就绪：aria2 {version}（{line_note}；{hash_note}）"
                )),
                Some(GuideNotice::External { version, path }) => Some(format!(
                    "已改用你指定的 aria2 {version}：{}",
                    path.display()
                )),
                _ => None,
            };
            if let Some(headline) = headline {
                self.set_banner(Banner::new(Tone::Good, headline));
            }
            self.refresh_runtime_after_core_setup();
        }
        if self.guide.busy() {
            ctx.request_repaint_after(CORE_SETUP_REPAINT);
        }
    }

    /// 核心刚就绪（装好或切到 external）：按**新配置**重建 host 与 worker。
    ///
    /// 不重建会出现最气人的那种 bug：卡片说「已就绪」，点启动仍报「核心未安装」
    /// ——因为 `HostSetup` 是启动时那一次 prepare 的快照。
    fn refresh_runtime_after_core_setup(&mut self) {
        if self.core.is_busy() {
            self.log
                .push("核心已装好，但 aria2 正在启停中，等这一轮结束再刷新运行时".to_owned());
            return;
        }
        let setup = core_host::HostSetup::prepare(&self.log);
        self.core_probe = setup.probe().clone();
        let options = setup.worker_options(&self.log);
        self.worker_options = options.clone();
        self.core = HostState::Idle;
        // 旧的 host 在这里被丢弃：`CoreHost::drop` 会发 `Shutdown`，线程自己收工。
        self.host = Some(CoreHost::spawn(Arc::clone(&self.log), setup));
        self.worker = Some(crate::worker::Worker::spawn(Arc::clone(&self.log), options));
        self.log
            .push("已按新核心重建运行时：点「启动 aria2」即可开始下载".to_owned());
    }

    /// 首屏引导卡：核心未就绪时才出现。
    ///
    /// 三条硬约束落在这一块上：
    /// - 取消**不假装即时**（文案是「当前步骤完成后停止」）；
    /// - 线路与回退显式可见（[`crate::core_setup::LineChoice::describe_hit`]）；
    /// - 看门狗超时后给「换线路 / 稍后再说」两个出口，不留转圈到地老天荒的界面。
    fn show_core_guide(&mut self, ui: &mut egui::Ui) {
        if !self.guide.open {
            return;
        }
        let mut start_online = false;
        let mut start_external = false;
        let mut cancel = false;
        let mut dismiss = false;
        let mut switch_line = false;
        let mut reopen_settings = false;

        egui::Frame::group(ui.style())
            .fill(ui.style().visuals.faint_bg_color)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("下载核心未就绪").strong());
                    ui.label(
                        RichText::new("（AriaHost 需要一个 aria2c 才能下载东西）")
                            .weak()
                            .small(),
                    );
                });
                ui.label(
                    RichText::new(
                        "第一次使用需要先准备好 aria2 核心。可以现在下载，也可以指定你已有的 aria2c。",
                    )
                    .small(),
                );
                ui.add_space(4.0);

                // —— 上一轮的结论 ——
                if let Some(notice) = &self.guide.notice {
                    match notice {
                        GuideNotice::Online {
                            version,
                            line_note,
                            hash_note,
                            warnings,
                        } => {
                            ui.label(
                                RichText::new(format!(
                                    "已就绪：aria2 {version}（{line_note}；{hash_note}）"
                                ))
                                .small(),
                            );
                            for warning in warnings {
                                ui.label(RichText::new(format!("· {warning}")).weak().small());
                            }
                        }
                        GuideNotice::External { version, path } => {
                            ui.label(
                                RichText::new(format!(
                                    "已改用你指定的 aria2 {version}：{}",
                                    path.display()
                                ))
                                .small(),
                            );
                        }
                        GuideNotice::Failed {
                            message,
                            hint,
                            retry_other,
                        } => {
                            // **不弹红条砸脸**：就地一行说明 + 可执行建议。
                            ui.label(RichText::new(message).small());
                            ui.label(RichText::new(format!("可以这样处理：{hint}")).small().weak());
                            if *retry_other {
                                ui.label(
                                    RichText::new(format!(
                                        "下一步：换 {} 再试",
                                        self.guide.line.other().label()
                                    ))
                                    .small()
                                    .weak(),
                                );
                            }
                        }
                        GuideNotice::CancelledTooLate { step } => {
                            ui.label(
                                RichText::new(format!(
                                    "{} 在取消前就已经完成，本轮结果仍然有效",
                                    step.label()
                                ))
                                .small()
                                .weak(),
                            );
                        }
                    }
                    ui.add_space(4.0);
                }

                if self.guide.busy() {
                    // —— 进行中 ——
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new(self.guide.step.label()).small());
                        ui.label(RichText::new(format!("· {}", self.guide.line.label())).weak().small());
                    });
                    if self.guide.cancel_requested {
                        // 诚实：取消不是即时的。
                        ui.label(
                            RichText::new("已请求取消：当前步骤完成后停止（下载并校验是一整步，不能中途掐断）")
                                .small()
                                .weak(),
                        );
                    } else if ui
                        .button("取消")
                        .on_hover_text("当前步骤（下载并校验）跑完后停止")
                        .clicked()
                    {
                        cancel = true;
                    }
                    if self.guide.slow {
                        ui.add_space(2.0);
                        ui.label(
                            RichText::new(
                                "已经 45 秒没有任何进展：网络慢或被拦截（GitHub 在部分网络下不可达）",
                            )
                            .small(),
                        );
                        ui.horizontal(|ui| {
                            if ui
                                .button(format!("换 {} 再试", self.guide.line.other().label()))
                                .clicked()
                            {
                                switch_line = true;
                            }
                            if ui.button("稍后再说").clicked() {
                                dismiss = true;
                            }
                        });
                    }
                } else {
                    // —— 三个入口 ——
                    ui.horizontal(|ui| {
                        if ui
                            .button(format!("在线下载（{}）", self.guide.line.label()))
                            .on_hover_text("从 GitHub 下载官方 aria2；失败会自动换另一条线路重试")
                            .clicked()
                        {
                            start_online = true;
                        }
                        if ui
                            .button(format!("改用{}", self.guide.line.other().label()))
                            .on_hover_text("把另一条线路排在前面（仍然保留自动回退）")
                            .clicked()
                        {
                            self.guide.line = self.guide.line.other();
                        }
                        if ui.button("稍后再说").on_hover_text("先不装；面板可看，下载按钮暂时禁用").clicked() {
                            dismiss = true;
                        }
                    });
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("已有 aria2c：").small());
                        let width = (ui.available_width() - 92.0).max(160.0);
                        ui.add(
                            egui::TextEdit::singleline(&mut self.guide.external_path)
                                .id_salt("ariahost_external_core")
                                .hint_text("粘贴 aria2c.exe 的完整路径")
                                .desired_width(width),
                        );
                        if ui
                            .button("校验并使用")
                            .on_hover_text("会跑一次 --version 确认它能用，然后写进配置")
                            .clicked()
                        {
                            start_external = true;
                        }
                    });
                }
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    if ui
                        .button("去设置页配置核心")
                        .on_hover_text("设置页「界面」组里有重新进入这块的入口")
                        .clicked()
                    {
                        reopen_settings = true;
                    }
                });
            });

        if start_online && self.guide.start_online() {
            self.log.push(format!(
                "核心引导：开始在线下载（{}）",
                self.guide.line.label()
            ));
        }
        if start_external {
            let raw = self.guide.external_path.trim().to_owned();
            let path = ariahost_common::config::expand_user_path(&raw);
            if self.guide.start_external(path) {
                self.log.push(format!("核心引导：校验本地 aria2c（{raw}）"));
            }
        }
        if cancel {
            self.guide.request_cancel();
            self.log
                .push("核心引导：已请求取消（当前步骤完成后停止）".to_owned());
        }
        if switch_line {
            self.log.push("核心引导：看门狗接管，换线路重试".to_owned());
            self.guide.switch_line();
        }
        if dismiss {
            self.guide.dismiss();
            self.log
                .push("核心引导：已收下这张卡；下载按钮暂时禁用，设置页可重新进入".to_owned());
        }
        if reopen_settings {
            self.settings_open = true;
        }
    }

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
            // M3-h：核心没就绪时「开始」置灰，并把同一句引导挂在悬浮提示上。
            let core_ok = self.core_usable();
            let start = ui.add_enabled(core_ok, egui::Button::new("开始"));
            let hint = if core_ok {
                "加入下载队列（aria2.addUri）"
            } else {
                "先准备好下载核心：见上方「下载核心未就绪」卡片"
            };
            if start.on_hover_text(hint).clicked() || (core_ok && enter) {
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
        let mut open_web = false;
        let mut quit_now = false;

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
                if ui
                    .button("退出")
                    .on_hover_text(format!(
                        "退出面板：先优雅停止 aria2（约需 {} 秒）再关窗",
                        core_host::STOP_EXPECTED_SECS
                    ))
                    .clicked()
                {
                    quit_now = true;
                }
                if ui
                    .button("在浏览器打开")
                    .on_hover_text(format!(
                        "起本机网页管理页（127.0.0.1:{}）并用默认浏览器打开",
                        self.prefs.web_port
                    ))
                    .clicked()
                {
                    open_web = true;
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

        if quit_now {
            self.begin_quit(ui.ctx(), false);
        }
        if open_web {
            self.open_web_page();
        }

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
        self.draw(ui);
    }
}

impl PanelApp {
    /// 把一帧画完：真实窗口（`eframe::App::ui`）与隐藏自检驱动
    /// （`--panel-selftest --drive`）**共用这一条绘制路径**，避免驱动测到的不是产品代码。
    pub fn draw(&mut self, ui: &mut egui::Ui) {
        self.frames = self.frames.saturating_add(1);
        // 控制管道（M3-f）：`ariahost --quit` 由 ctl 线程置位，这里消费并走退出档。
        // 必须放在最前面：晚一帧消费就可能让客户端多等一轮握手。
        if self.quit.is_none() && crate::ctl::take_quit_request() {
            self.log
                .push("控制管道：收到 quit，走退出档（保存会话 → 停 aria2c → 关窗）".to_owned());
            self.begin_quit(ui.ctx(), false);
        }
        self.sample_memory();
        self.drain_worker();
        self.drain_core();
        // M3-h：核心安装事件的消费 + 看门狗（线程侧不碰 UI，这里只 try_recv 与内存判断）。
        self.pump_core_guide(ui.ctx());
        // M3-b：提权结果 + SCM 快照 + 等待预算，全部非阻塞。
        self.pump_service(ui.ctx());
        self.expire_optimistic();
        // M2-3：托盘 / 关闭三档 / 退出流程 / 启动检测（全部非阻塞）。
        self.pump_tray(ui.ctx());
        self.pump_close(ui.ctx());
        self.pump_quit(ui.ctx());
        if self.frames <= 2 {
            self.detect_pending_session();
        }
        self.fix_restored_geometry(ui);
        self.apply_requested_size(ui);
        if self.requested_size.is_none() || self.size_settled {
            self.frames_after_ready = self.frames_after_ready.saturating_add(1);
        }

        // 关窗时保存几何；eframe 本帧之后就退出主循环，所以必须在这里同步写完。
        if self.quit_save_pending || ui.ctx().input(|input| input.viewport().close_requested()) {
            self.persist_geometry(ui);
            self.quit_save_pending = false;
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
            // 设置入口放标题栏右侧：不占任务表的地盘，也不改主界面布局。
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let label = if self.settings_open {
                    "返回主界面"
                } else {
                    "设置"
                };
                let entry = ui.button(label);
                self.drive_targets.note("设置入口", entry.rect);
                if entry.clicked() {
                    self.toggle_settings();
                }
                let service_label = if self.service_open {
                    "返回主界面"
                } else {
                    "服务"
                };
                let service_entry = ui.button(service_label);
                self.drive_targets.note("服务入口", service_entry.rect);
                if service_entry.clicked() {
                    self.toggle_service();
                }
            });
        });
        ui.add_space(2.0);

        // 服务页打开时同样替掉主体区（M3-b）。
        if self.service_open {
            self.show_service_page(ui);
            self.show_close_prompt(ui.ctx());
            self.maybe_finish_selftest(ui);
            return;
        }

        // 设置页打开时替掉主体区（顶部状态栏、任务表都不画）：既省内存，也不重排主界面。
        if self.settings_open {
            self.show_settings(ui);
            self.show_close_prompt(ui.ctx());
            self.maybe_finish_selftest(ui);
            return;
        }

        egui::Panel::top("ariahost_status")
            .show_separator_line(false)
            .show(ui, |ui| {
                self.show_status(ui);
                self.show_quit_line(ui);
                self.show_session_bar(ui);
                self.show_banner(ui);
                // M3-h：首屏引导卡放在最上面（不是模态：它不挡托盘、不挡关闭，其余页照常可看）。
                self.show_core_guide(ui);
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

        self.show_close_prompt(ui.ctx());
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
            "点面板上的「下载核心未就绪」卡片：可以就地在线下载，也可以指定你已有的 aria2c"
                .to_owned()
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

/// 用默认浏览器打开 URL（不新增依赖：explorer/xdg-open 都能吃 URL）。
fn open_url(url: &str) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        std::process::Command::new("explorer")
            .arg(url)
            .spawn()
            .map(|_| ())
    }
    #[cfg(not(windows))]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map(|_| ())
    }
}

/// `%APPDATA%\AriaHost\session\aria2.session` 里的未完成任务数（`None` = 文件不存在）。
///
/// aria2 的 session 文件一行一个 URI，`#` 开头是注释；只数非空非注释行。
fn pending_session_count() -> Option<usize> {
    let dir = ariahost_common::paths::session_dir(ariahost_common::paths::Plane::User)?;
    let file = dir.join(ariahost_core::SESSION_FILE_NAME);
    let text = std::fs::read_to_string(&file).ok()?;
    Some(count_session_lines(&text))
}

/// 数 session 文件里的任务行（`#` 注释与空行不算）——单独拆出来是为了能纯函数测。
fn count_session_lines(text: &str) -> usize {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .count()
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

/// 一次提权调用的目标（可执行文件 + 参数）。
struct ElevateTarget {
    file: std::path::PathBuf,
    args: Vec<String>,
}

/// 四个动作各自该拉谁：
/// - 装/卸：面板自己的 CLI 子命令（`--install-service` / `--uninstall-service`，契约已定版）；
/// - 启/停：**没有**对应的 CLI 子命令（cli.rs 不在本批写作用域），用系统自带的 `sc.exe`。
///
/// `sc start/stop` 只是「把请求交给 SCM」，真正的状态变化靠 SCM 轮询观察——
/// 这也正好让「启动中…/停止中…」反映真实状态，而不是我们自己编的进度条。
fn elevate_target(op: service::Op) -> Option<ElevateTarget> {
    match op {
        service::Op::Install => Some(ElevateTarget {
            file: crate::elevate::helper_exe()?,
            args: vec!["--install-service".to_owned()],
        }),
        service::Op::Uninstall => Some(ElevateTarget {
            file: crate::elevate::helper_exe()?,
            args: vec!["--uninstall-service".to_owned()],
        }),
        service::Op::Start => Some(ElevateTarget {
            file: crate::elevate::sc_exe()?,
            args: vec!["start".to_owned(), ariahost_svc::SERVICE_NAME.to_owned()],
        }),
        service::Op::Stop => Some(ElevateTarget {
            file: crate::elevate::sc_exe()?,
            args: vec!["stop".to_owned(), ariahost_svc::SERVICE_NAME.to_owned()],
        }),
    }
}

/// 六态 → 颜色（与状态徽章同一套语气：绿=好、黄=可操作、灰=没有、红=异常）。
fn service_state_color(state: ariahost_svc::ServiceState) -> Color32 {
    use ariahost_svc::ServiceState as S;
    match state {
        S::Running => Color32::from_rgb(140, 200, 160),
        S::Stopped => Color32::from_rgb(230, 200, 130),
        S::NotInstalled => Color32::from_rgb(170, 170, 170),
        S::StartPending | S::StopPending => Color32::from_rgb(150, 190, 240),
        S::Unknown => Color32::from_rgb(230, 160, 120),
    }
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
    fn close_action_parses_the_three_documented_modes() {
        assert_eq!(CloseAction::parse("tray"), CloseAction::Tray);
        assert_eq!(CloseAction::parse("ASK"), CloseAction::Ask);
        assert_eq!(CloseAction::parse(" exit "), CloseAction::Exit);
        // 拼错不能把面板搞成关不掉：退回默认 tray（架构 §4.11 默认值）。
        assert_eq!(CloseAction::parse("quit"), CloseAction::Tray);
        assert_eq!(CloseAction::Tray.as_str(), "tray");
    }

    #[test]
    fn session_line_count_ignores_comments_and_blank_lines() {
        let text = "# AriaHost session\n\nhttp://a/1.bin\ngid=abc\n  \nhttp://a/2.bin\n";
        // ariahost 的 session 一行一个 URI / 选项，注释与空行不计数。
        assert_eq!(count_session_lines(text), 3);
        assert_eq!(count_session_lines(""), 0);
        assert_eq!(count_session_lines("# only comments\n\n"), 0);
    }

    #[test]
    fn failure_next_steps_are_actionable() {
        assert!(failure_next_step(Some(ErrorCode::Aria2NotFound)).contains("下载核心未就绪"));
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
