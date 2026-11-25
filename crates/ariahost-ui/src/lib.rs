//! AriaHost 面板。
//!
//! 演进线索（每批都留了落点注释，便于逐条核对）：
//! - **M2-0**：四道技术闸门——窗口能开、中文正常、IME 可输入、内存 <100MB；
//! - **M2-1**：面板显示真数据（SCM 服务状态 + aria2 RPC 状态，后台线程 + 退避刷新）；
//! - **M2-2b**：面板能**下载**——本进程托管 aria2c（启停）、任务列表（EG-8 虚拟化）、
//!   新建/暂停/继续/移除、200 行内存验收（`--panel-selftest --seed-rows 200`）。
//!
//! 设计约束编号 `EG-n` 来自 `docs/requirements-and-design/2026-11-22-wasm-架构设计.md` §4.1.2。
//!
//! **设备上限与字形图集上限是两件事**（M2-0b 拆开，详见 [`DEVICE_MAX_SIDE`] 与
//! [`ATLAS_MAX_SIDE_DEFAULT`] 的注释）：设备上限管交换链/窗口物理像素，图集上限管显存，
//! 后者经 `App::raw_input_hook` 每帧压回 egui，不再倒过来把窗口锁死。
//!
//! 注意：本机开发环境没有 MSVC 链接器，本 crate 的编译验证**只能靠 CI**
//! （`.github/workflows/ci.yml` 的 windows job），不要以本地 `cargo build` 为据。

mod app;
mod backend;
mod core_host;
mod fonts;
mod instance;
mod logbuf;
mod metrics;
mod notify;
mod settings;
mod tasks;
mod tray;
mod view;
mod window;
mod worker;

use std::sync::{Arc, Mutex};

use ariahost_common::{ErrorCode, EXIT_ERR, EXIT_OK, PRODUCT_NAME, VERSION};
use egui::ViewportBuilder;

pub use app::{Banner, PanelInit, RuntimeFacts};
pub use backend::Backend;
pub use core_host::{CoreHost, CoreProbe, HostState};
pub use logbuf::LogBuffer;
pub use notify::{
    ChannelAvailability, Notification, Notifier, NotifyKind, NotifyLevel, NotifyOutcome,
    NotifyStats, SuppressReason, COOLDOWN as NOTIFY_COOLDOWN, RATE_CAP as NOTIFY_RATE_CAP,
    RATE_WINDOW as NOTIFY_RATE_WINDOW,
};
pub use settings::{Effect, Group, Notice, Settings};
pub use tasks::{TaskAction, TaskRow, TaskStatus};
// 托盘与通知的组合入口对外可见：`examples/notify_selftest.rs` 用它做人工目视自检
// （M3-c 验收要「气泡真的出现」的证据），面板内部本来也走同一个类型。
pub use tray::Tray;
pub use view::{StatusView, Tone};
pub use window::Geometry;
pub use worker::{
    endpoint_text, Aria2Failure, Aria2Snapshot, ServiceSnapshot, TaskRequest, Worker, WorkerOptions,
};

/// 面板单实例已被占用时的提示（中文，退出码 [`EXIT_ERR`]）。
pub const DUPLICATE_INSTANCE_HINT: &str = "AriaHost 面板已在运行，本次不再重复开窗。";

/// 「在浏览器打开」的注入点：由 bin 提供实现（ui 不链 ariahost-web）。
///
/// 返回网页管理页的 URL；失败给一句人话原因（面板会按 `AH-E3001` 上屏）。
pub type OpenWebFn = Box<dyn Fn() -> Result<String, String> + Send + Sync>;

/// 点 X 时的三档行为（配置 `ui.close_action`，架构 §4.11 第 37 项：`tray | ask | exit`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseAction {
    /// 隐藏到托盘，不退出（默认）。
    Tray,
    /// 面板内对话框询问。
    Ask,
    /// 直接退出（按退出流程处理 aria2）。
    Exit,
}

impl CloseAction {
    /// 按配置字符串解析；非法值退回默认 `tray`（不因为一个拼错的值让面板关不掉）。
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "ask" => Self::Ask,
            "exit" => Self::Exit,
            _ => Self::Tray,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tray => "tray",
            Self::Ask => "ask",
            Self::Exit => "exit",
        }
    }
}

/// 面板行为偏好（从 `config.toml` 的 `[ui]`/`[web]` 读；读不到就用默认值）。
///
/// 说明：`ui.quit_stops_aria2` 在定版 schema（52 项）里**没有**这个键，按需求第 7 条
/// 「退出时承诺不中断下载」的口径固定为「先优雅停止、再退出」；是否真退出由
/// [`CloseAction`] 决定，所以不需要自造配置键。
#[derive(Debug, Clone)]
pub struct UiPrefs {
    /// 点 X 的行为。
    pub close_action: CloseAction,
    /// 网页管理页端口（`web.port`，默认 6875）。
    pub web_port: u16,
}

impl Default for UiPrefs {
    fn default() -> Self {
        Self {
            close_action: CloseAction::Tray,
            web_port: 6875,
        }
    }
}

impl UiPrefs {
    /// 读配置；任何失败都退回默认值并写日志（面板行为不该因为配置坏了就失灵）。
    pub fn load(log: &LogBuffer) -> Self {
        match ariahost_common::config::load() {
            Ok(config) => {
                let action = CloseAction::parse(&config.ui.close_action);
                log.push(format!(
                    "界面行为：关闭动作 {}（ui.close_action）、网页端口 {}（web.port）",
                    action.as_str(),
                    config.web.port
                ));
                // ui.renderer 是渲染器逃生口（auto | wgpu | glow，架构 §4.11 第 34 项）。
                // 本构建只带 wgpu（eframe 默认 features，EG-3 的 MemoryHints 也走它），
                // 所以选 glow 只能如实回退 + 记一条日志，不能假装切过去。
                let renderer = config.ui.renderer.trim().to_ascii_lowercase();
                match renderer.as_str() {
                    "wgpu" => log.push("渲染器：wgpu（ui.renderer）".to_owned()),
                    "glow" => log.push(
                        "ui.renderer = glow，但本构建只带 wgpu（未编入 glow feature）：已按 wgpu 启动；要真用 glow 需带 glow feature 重编译"
                            .to_owned(),
                    ),
                    "auto" | "" => log.push("渲染器：auto → wgpu（本构建唯一可用）".to_owned()),
                    other => log.push(format!(
                        "ui.renderer = {other} 不是 auto/wgpu/glow：按 auto（wgpu）启动，请到设置页改正"
                    )),
                }
                Self {
                    close_action: action,
                    web_port: config.web.port,
                }
            }
            Err(error) => {
                log.push(format!("读取界面行为失败：{error}（用默认值）"));
                Self::default()
            }
        }
    }

    /// 把「记住我的选择」写回 `ui.close_action`（失败只记日志，不打断退出）。
    pub fn remember_close_action(&self, action: CloseAction, log: &LogBuffer) {
        let mut config = match ariahost_common::config::load() {
            Ok(config) => config,
            Err(error) => {
                log.push(format!("记住关闭选择失败（配置读取）：{error}"));
                return;
            }
        };
        config.ui.close_action = action.as_str().to_owned();
        match ariahost_common::config::save(&config) {
            Ok(()) => log.push(format!(
                "已记住关闭选择：ui.close_action = {}",
                action.as_str()
            )),
            Err(error) => log.push(format!("记住关闭选择失败（配置保存）：{error}")),
        }
    }
}

/// wgpu 设备的 `max_texture_dimension_2d`（交换链/窗口物理像素的上限）。
///
/// **为什么不是 2048**：wgpu-core 30 的 `validate_surface_configuration`
/// （`wgpu-core-30.0.2/src/device/surface_config.rs:44-58`）用设备上限校验交换链尺寸，
/// 而 `egui-wgpu` 的 `max_texture_side()`（`egui-wgpu-0.37.0/src/winit.rs:333`）**直接返回**
/// 同一个值，于是「压图集」会连坐「压窗口」：2048 的窗口在 1440p（2560）/4K（3840）上必然
/// `Surface::configure` 失败（错误经 `handle_error_nolabel` 进设备错误回调，表现为黑窗）。
/// 8192 是 wgpu 的 downlevel 保证上限，也是 egui-wgpu 自己的默认请求值
/// （`egui-wgpu-0.37.0/src/setup.rs:272`），覆盖 4K 与 200% 缩放；
/// 显存由 [`ATLAS_MAX_SIDE_DEFAULT`] 单独管住，两者不再耦合。
pub const DEVICE_MAX_SIDE: u32 = 8192;

/// 字形图集上限的默认值（EG-7），经 `RawInput.max_texture_side` 每帧压回 egui。
///
/// epaint 的分配语义（`epaint-0.37.0/src/text/glyph_atlas.rs:217-224` +
/// `src/texture_atlas.rs:82-90`、`:270-282`）：图集纹理**宽度 = 上限**、高度从 **32**
/// 起按需翻倍（上限由 `max_height() = max(height, width)` 夹住，见 `texture_atlas.rs:175-178`），
/// 所以 2048 的稳态开销是「2048 × 实际用到的高度 × 4B」，最坏 16.8MB；若把上限放到 8192，
/// 宽度直接 ×4（最坏 268MB），这就是 EG-7 要钉 2048 的原因。
///
/// 解耦路径（源码依据）：eframe 在 `EpiIntegration::build_raw_input` 里逐帧调用
/// `app.raw_input_hook(&ctx, &mut raw_input)`（`eframe-0.37.0/src/native/epi_integration.rs:423`），
/// `RawInput.max_texture_side: Option<usize>`（`egui-0.37.0/src/data/input/raw_input.rs:45`）
/// 经 `InputState::begin_pass`（`input_state/mod.rs:485`）落到
/// `Context::update_fonts_mut` 的 `text_options.max_texture_side`（`context.rs:587`、`:628`）。
/// 在 hook 里每帧写回 2048 即可让图集按 2048 建，而设备/交换链仍按 [`DEVICE_MAX_SIDE`]。
pub const ATLAS_MAX_SIDE_DEFAULT: u32 = 2048;

/// 环境变量名：临时放开字形图集上限（默认见 [`ATLAS_MAX_SIDE_DEFAULT`]）。
///
/// 现在它**只影响图集**，不再影响窗口尺寸；留下来是给「图集撑爆/需要更大图集」的临时手段。
pub const ATLAS_MAX_SIDE_ENV: &str = "ARIAHOST_ATLAS_SIDE";

/// EG-11：窗口尺寸硬上限（逻辑点，对应物理像素上限 8191）。
///
/// 8191 是 winit/wgpu 侧的已知边界（egui #8361），超过会拖出窗口尺寸钳制类问题。
pub const MAX_WINDOW_SIDE: f32 = 8191.0;

/// `--panel-selftest --size` 允许请求的物理像素上限（受 [`DEVICE_MAX_SIDE`] 约束）。
pub const MAX_SELFTEST_SIDE: u32 = 8192;

/// `--panel-selftest` 的默认帧数：约 2 秒 @60fps，够走完首帧、字体图集预热与 wgpu 上传。
pub const DEFAULT_SELFTEST_FRAMES: u32 = 120;

/// `--panel-selftest --seed-rows` 允许的最大行数。
///
/// 内存闸门的验收量级是 200；上限只是防手滑（`--seed-rows 100000000` 会把机器拖死）。
pub const MAX_SEED_ROWS: usize = 5000;

/// 启动面板（无参数入口）；返回进程退出码。
pub fn run_panel(backend: Backend, open_web: Option<OpenWebFn>) -> i32 {
    run(None, None, backend, None, open_web)
}

/// `--panel-selftest [--frames N] [--size WxH] [--backend ...] [--seed-rows N]`：
/// 开窗 → 渲染 N 帧 → 打印一行机器可读结果 → 退出码 0。
///
/// `size` 是**物理像素**（窗口客户区，等于交换链 extent）；窗口可以比屏幕大，用来验证
/// 「超过 2048 的交换链在真机上能不能建起来」。`seed_rows` 灌 N 行假任务（EG-8/EG-9 的
/// 200 行内存闸门），此时**不连 RPC、不启动 aria2**。输出格式见 [`report`]。
pub fn run_selftest(
    frames: u32,
    size: Option<(u32, u32)>,
    backend: Backend,
    seed_rows: Option<usize>,
) -> i32 {
    run(Some(frames), size, backend, seed_rows, None)
}

/// 共用启动流程：单实例 → 建窗 → 渲染 → 返回退出码。
fn run(
    selftest_frames: Option<u32>,
    requested_size: Option<(u32, u32)>,
    backend: Backend,
    seed_rows: Option<usize>,
    open_web: Option<OpenWebFn>,
) -> i32 {
    let log = Arc::new(LogBuffer::new());
    log.push(format!(
        "AriaHost {VERSION} 启动（{}）",
        mode_text(selftest_frames, requested_size, seed_rows)
    ));
    log.push(format!(
        "渲染后端选择：--backend {}（{}）",
        backend.as_str(),
        backend.describe()
    ));

    let atlas_side = atlas_max_side(&log);

    // 窗口几何：自测模式不读也不写（否则一次 4K 自测会把用户的面板永久拉成 4K）。
    let saved_geometry = if selftest_frames.is_none() {
        window::window_file()
            .inspect(|path| log.push(format!("窗口状态文件：{}", path.display())))
            .and_then(|path| window::load(&path))
    } else {
        None
    };
    if let Some(geometry) = saved_geometry {
        log.push(format!(
            "恢复窗口几何：{:.0}×{:.0} pt @ ({:.0}, {:.0})",
            geometry.width, geometry.height, geometry.x, geometry.y
        ));
    }

    // EG-12：单实例。本切片用 std::fs::File::try_lock 锁文件（Rust 1.89+ 稳定 API），
    // M2-1 换成 CreateMutexW + ERROR_ALREADY_EXISTS + 命令行转发（见架构 §4.1.4）。
    let lock = match instance::acquire() {
        instance::LockOutcome::Acquired(lock) => {
            log.push(format!("单实例锁：已获取 {}", lock.path().display()));
            Some(lock)
        }
        instance::LockOutcome::AlreadyRunning { path } => {
            println!(
                "{} {}",
                ErrorCode::DuplicateInstance.code(),
                DUPLICATE_INSTANCE_HINT
            );
            println!("锁文件：{}", path.display());
            return EXIT_ERR;
        }
        instance::LockOutcome::Unavailable { path, reason } => {
            log.push(format!(
                "单实例锁不可用（继续启动）：{reason}（{}）",
                path.display()
            ));
            None
        }
    };

    log.push(format!(
        "渲染配置：wgpu + MemoryHints::MemoryUsage（EG-3）；设备上限 {DEVICE_MAX_SIDE} px（窗口/交换链）、\
         字形图集上限 {atlas_side} px（EG-7，两者已解耦）"
    ));

    // EG-10：panic 也先进内部日志，再交给系统默认行为（不吞异常，但用户能在面板里看到记录）。
    install_panic_hook(&log);

    // 运行时准备：读配置 → 解 secret → 定位核心。灌行自测不需要（也不该）碰磁盘上的配置。
    let (setup, worker_options) = match seed_rows {
        Some(_) => {
            log.push("自测灌行模式：跳过配置/secret/核心定位（不连 RPC）".to_owned());
            (None, worker::WorkerOptions::default())
        }
        None => {
            let setup = core_host::HostSetup::prepare(&log);
            let options = setup.worker_options(&log);
            (Some(setup), options)
        }
    };

    // 界面行为（关闭三档 / 网页端口）：自测与灌行模式不读配置，用默认值。
    let prefs = if selftest_frames.is_none() && seed_rows.is_none() {
        UiPrefs::load(&log)
    } else {
        UiPrefs::default()
    };

    let options = native_options(&log, backend, saved_geometry);
    let facts = Arc::new(Mutex::new(RuntimeFacts::default()));
    let app_log = Arc::clone(&log);
    let app_facts = Arc::clone(&facts);

    let result = eframe::run_native(
        PRODUCT_NAME,
        options,
        Box::new(move |cc| {
            let panel = app::PanelApp::new(
                cc,
                PanelInit {
                    log: app_log,
                    facts: app_facts,
                    lock,
                    selftest_frames,
                    requested_size,
                    atlas_cap: atlas_side,
                    saved_geometry,
                    seed_rows,
                    setup,
                    worker_options,
                    prefs,
                    open_web,
                },
            );
            Ok(Box::new(panel))
        }),
    );

    match result {
        Ok(()) => {
            report(selftest_frames, &facts);
            EXIT_OK
        }
        Err(error) => {
            log.push(format!("窗口启动失败：{error}"));
            eprintln!("AriaHost 面板启动失败：{error}");
            if selftest_frames.is_some() {
                println!("SELFTEST failed reason={}", sanitize(&error.to_string()));
            }
            // 面板起不来多半是显卡/驱动环境问题（EG-10 的兜底路径归 M2-1），按一般失败返回。
            EXIT_ERR
        }
    }
}

/// `--panel-selftest` 的收尾输出：一行机器可读结果（键值对，值内空白替换为 `_`）。
///
/// 关键字段：
/// - `surface=` / `window=`：窗口客户区物理像素，eframe 用它作为 `SurfaceConfiguration.extent`
///   （egui-wgpu 的 `SurfaceState::configure`），因此两者同值；**真正的证据是 `ok=1`**——
///   交换链尺寸被设备拒绝时，wgpu 会经 `handle_error_nolabel` 走到 `on_uncaptured_error`
///   （`wgpu-30.0.2/src/backend/wgpu_core.rs:3978-3981`），`wgpu_errors` 会大于 0。
/// - `atlas=`：字形图集纹理的**实际尺寸**（`FontsView::font_image_size`），`atlas_cap=` 是配置上限。
/// - `ok=1`：没有 wgpu 未捕获错误，且（若请求了 `--size`）窗口物理尺寸确实到达目标。
/// - `rows=`：结束时任务表的行数（`--seed-rows 200` 用它证明「真的画了 200 行」）。
fn report(selftest_frames: Option<u32>, facts: &Mutex<RuntimeFacts>) {
    if selftest_frames.is_none() {
        return;
    }
    let facts = facts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    println!(
        "SELFTEST adapter={} atlas={}x{} atlas_cap={} surface={}x{} window={}x{} backend={} \
         frames={} rows={} ok={} wgpu_errors={} private_kb={}",
        sanitize(&facts.adapter),
        facts.atlas_size[0],
        facts.atlas_size[1],
        facts.atlas_side,
        facts.window_physical[0],
        facts.window_physical[1],
        facts.window_physical[0],
        facts.window_physical[1],
        sanitize(&facts.backend),
        facts.frames,
        facts.rows,
        u8::from(facts.ok),
        facts.wgpu_errors,
        facts.private_kb,
    );
    if let Some(reason) = facts.reason.as_deref() {
        println!("SELFTEST_NOTE {reason}");
    }
}

/// 让 `SELFTEST` 行保持「`key=value` 以空白分隔」可解析：值里的空白换成 `_`。
fn sanitize(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join("_")
}

/// 启动模式的中文描述（写进日志）。
fn mode_text(
    selftest_frames: Option<u32>,
    requested_size: Option<(u32, u32)>,
    seed_rows: Option<usize>,
) -> String {
    match (selftest_frames, requested_size) {
        (Some(frames), Some((width, height))) => {
            format!(
                "自测模式{}，目标窗口 {width}×{height} px，渲染 {frames} 帧后退出",
                seed_text(seed_rows)
            )
        }
        (Some(frames), None) => format!("自测模式{}，渲染 {frames} 帧后退出", seed_text(seed_rows)),
        (None, _) => "交互面板模式".to_owned(),
    }
}

/// 灌行后缀（`--seed-rows N`）。
fn seed_text(seed_rows: Option<usize>) -> String {
    match seed_rows {
        Some(count) => format!("（灌 {count} 行假任务）"),
        None => String::new(),
    }
}

/// 读取字形图集上限：默认 [`ATLAS_MAX_SIDE_DEFAULT`]，可用环境变量覆盖（见该常量注释）。
fn atlas_max_side(log: &LogBuffer) -> u32 {
    let Ok(raw) = std::env::var(ATLAS_MAX_SIDE_ENV) else {
        return ATLAS_MAX_SIDE_DEFAULT;
    };
    match raw.trim().parse::<u32>() {
        Ok(value) if (1024..=DEVICE_MAX_SIDE).contains(&value) => {
            log.push(format!(
                "{ATLAS_MAX_SIDE_ENV}={value}：覆盖字形图集上限（默认 {ATLAS_MAX_SIDE_DEFAULT}）"
            ));
            value
        }
        _ => {
            log.push(format!(
                "{ATLAS_MAX_SIDE_ENV}={raw:?} 不是 1024..={DEVICE_MAX_SIDE} 的整数，忽略，\
                 沿用默认 {ATLAS_MAX_SIDE_DEFAULT}"
            ));
            ATLAS_MAX_SIDE_DEFAULT
        }
    }
}

/// 组装 eframe 原生窗口选项。
///
/// `geometry` 是上次退出时保存的窗口几何（点）；为 `None` 时保持「默认尺寸 + 居中」。
fn native_options(
    log: &LogBuffer,
    backend: Backend,
    geometry: Option<Geometry>,
) -> eframe::NativeOptions {
    let icon = load_icon(log);
    let mut viewport = ViewportBuilder::default()
        .with_title(app::title_text())
        .with_app_id("AriaHost")
        .with_min_inner_size([window::MIN_WIDTH, window::MIN_HEIGHT])
        // EG-11：窗口尺寸钳制（用户拉伸也不得越过 8191，egui #8361 未修）。
        .with_max_inner_size([MAX_WINDOW_SIDE, MAX_WINDOW_SIDE])
        .with_icon(icon);
    let mut centered = true;
    if let Some(geometry) = geometry {
        // 持久化几何按「点」存；尺寸再夹一次，避免手改 json 绕过 EG-11。
        let clean = window::sanitize(geometry, None);
        viewport = viewport
            .with_inner_size([clean.width, clean.height])
            .with_position([clean.x, clean.y]);
        centered = false;
    } else {
        viewport = viewport.with_inner_size([window::DEFAULT_WIDTH, window::DEFAULT_HEIGHT]);
    }
    eframe::NativeOptions {
        viewport,
        // EG-5：eframe 的默认 provider 不带 locale（会按系统 locale 选字，中文可能落到日文字形），
        // 所以这里关掉它，改由 `fonts::install` 注册带 `zh-CN` 的 provider（首帧前完成，EG-6）。
        system_font_fallback: false,
        centered,
        wgpu_options: wgpu_configuration(backend),
        ..Default::default()
    }
}

/// wgpu 设备配置：内存策略见 EG-3，设备上限见 [`DEVICE_MAX_SIDE`]（与图集上限解耦），
/// 后端位集见 [`Backend`]（`--backend` 唯一的落点）。
fn wgpu_configuration(backend: Backend) -> eframe::WgpuConfiguration {
    let mut setup = eframe::egui_wgpu::WgpuSetupCreateNew::without_display_handle();
    // `InstanceDescriptor` 在 wgpu 30 里没有 `Default`：`without_display_handle()` 已经给出
    // `new_without_display_handle()` 的实例，这里只改唯一关心的字段（后端位集）。
    setup.instance_descriptor.backends = backend.backends();
    setup.device_descriptor = Arc::new(move |_adapter: &eframe::wgpu::Adapter| {
        eframe::wgpu::DeviceDescriptor {
            label: Some("ariahost wgpu device"),
            // EG-3：wgpu 默认 MemoryHints::Performance，Win11 上 eframe hello-world 开窗即 ~250MB
            // （wgpu #8453 实测）。改成 MemoryUsage 是硬要求，不是可选优化。
            memory_hints: eframe::wgpu::MemoryHints::MemoryUsage,
            required_limits: eframe::wgpu::Limits {
                // 交换链/窗口物理像素上限（不是图集上限——图集另经 raw_input_hook 压回）。
                max_texture_dimension_2d: DEVICE_MAX_SIDE,
                ..eframe::wgpu::Limits::default()
            },
            ..Default::default()
        }
    });
    eframe::WgpuConfiguration {
        wgpu_setup: eframe::egui_wgpu::WgpuSetup::CreateNew(setup),
        ..Default::default()
    }
}

/// 窗口图标：品牌 PNG 在编译期内嵌（`include_bytes!`），解码失败也不阻塞开窗。
fn load_icon(log: &LogBuffer) -> Arc<egui::IconData> {
    const ICON_PNG: &[u8] = include_bytes!("../../../assets/branding/ariahost-icon-256.png");
    match eframe::icon_data::from_png_bytes(ICON_PNG) {
        Ok(icon) => {
            log.push(format!(
                "窗口图标：{}×{} px（assets/branding/ariahost-icon-256.png）",
                icon.width, icon.height
            ));
            Arc::new(icon)
        }
        Err(error) => {
            log.push(format!("窗口图标解码失败，沿用 egui 默认图标：{error}"));
            Arc::new(egui::IconData::default())
        }
    }
}

/// EG-10：panic 与 wgpu 未捕获错误都先进内部日志（后者在 `app::PanelApp::new` 里挂）。
fn install_panic_hook(log: &Arc<LogBuffer>) {
    let log = Arc::clone(log);
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log.push(format!(
            "panic（EG-10：先记内部日志，再按默认行为处理）：{info}"
        ));
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selftest_line_stays_parseable() {
        assert_eq!(
            sanitize("NVIDIA GeForce RTX 4060"),
            "NVIDIA_GeForce_RTX_4060"
        );
        assert_eq!(sanitize("  多  空白  "), "多_空白");
        assert_eq!(sanitize(""), "");
    }

    #[test]
    fn window_side_limit_matches_eg11() {
        // 常量断言放进 const 块：既在编译期就卡住回归，也避开 clippy 的
        // `assertions_on_constants`（运行期再断言一次纯常量没有意义）。
        const { assert!(MAX_WINDOW_SIDE == 8191.0, "EG-11 要求窗口尺寸上限 8191 px") };
    }

    /// 解耦的核心约束：设备上限必须覆盖 4K（3840）与 200% 缩放，图集上限必须留在 2048。
    #[test]
    fn device_and_atlas_limits_are_decoupled() {
        const {
            assert!(ATLAS_MAX_SIDE_DEFAULT == 2048, "EG-7 首选 2048");
            assert!(
                DEVICE_MAX_SIDE >= 4096,
                "设备上限必须覆盖 4K（3840）窗口，否则 1440p/4K 黑窗"
            );
            assert!(DEVICE_MAX_SIDE == 8192);
            assert!(ATLAS_MAX_SIDE_DEFAULT < DEVICE_MAX_SIDE);
            assert!(MAX_SELFTEST_SIDE == DEVICE_MAX_SIDE);
        }
    }

    #[test]
    fn mode_text_is_chinese() {
        assert!(mode_text(None, None, None).contains("面板"));
        assert!(mode_text(Some(3), None, None).contains("3"));
        assert!(mode_text(Some(3), Some((3840, 2160)), None).contains("3840×2160"));
        // 灌行模式必须在日志里点明「假数据」，不能被误读成真任务。
        let seeded = mode_text(Some(60), None, Some(200));
        assert!(seeded.contains("200"), "{seeded}");
        assert!(seeded.contains("假任务"), "{seeded}");
    }
}
