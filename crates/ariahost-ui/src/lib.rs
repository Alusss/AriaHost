//! AriaHost 面板（M2 技术闸门切片）。
//!
//! 本切片只验证四道技术闸门：**窗口能开、中文正常、IME 可输入、内存可控**；
//! 不接服务控制、不接 RPC、不做任务列表（那是 M2-1）。
//!
//! 设计约束编号 `EG-n` 来自 `docs/requirements-and-design/2026-11-22-wasm-架构设计.md` §4.1.2，
//! 每条落点都写在对应代码注释里，便于 M2-1 接手时逐条核对。
//!
//! 注意：本机开发环境没有 MSVC 链接器，本 crate 的编译验证**只能靠 CI**
//! （`.github/workflows/ci.yml` 的 windows job），不要以本地 `cargo build` 为据。

mod app;
mod fonts;
mod instance;
mod logbuf;
mod metrics;

use std::sync::{Arc, Mutex};

use ariahost_common::{ErrorCode, EXIT_ERR, EXIT_OK, PRODUCT_NAME, VERSION};
use egui::ViewportBuilder;

pub use app::RuntimeFacts;
pub use logbuf::LogBuffer;

/// 面板单实例已被占用时的提示（中文，退出码 [`EXIT_ERR`]）。
pub const DUPLICATE_INSTANCE_HINT: &str = "AriaHost 面板已在运行，本次不再重复开窗。";

/// 字形图集上限的默认值（EG-7）。
///
/// 2048 → epaint 的 `GlyphAtlas` 以 `max_texture_side` 作为图集纹理**宽度**、高度从 32 起按需增长，
/// 最坏 2048×2048×4B ≈ 16.8MB 显存；设备默认值 8192 是它的 16 倍。
///
/// 源码事实（M2 门禁时需再确认）：wgpu-core 30 的 `validate_surface_configuration`
/// （`wgpu-core/src/device/surface_config.rs`）用**同一个** `max_texture_dimension_2d`
/// 约束交换链尺寸，所以窗口物理像素超过本值时首帧配置会失败（表现为黑窗）。
/// 若真机是 1440p/4K 或 200% 缩放，设环境变量 [`ATLAS_MAX_SIDE_ENV`]`=4096` 放开上限，
/// 不需要重新构建；这条逃生口和设计里「必要时 4096，但需记录」的分支一致。
pub const ATLAS_MAX_SIDE_DEFAULT: u32 = 2048;

/// 环境变量名：临时放开字形图集上限（默认见 [`ATLAS_MAX_SIDE_DEFAULT`]）。
pub const ATLAS_MAX_SIDE_ENV: &str = "ARIAHOST_ATLAS_SIDE";

/// EG-11：窗口尺寸硬上限（逻辑点，对应物理像素上限 8191）。
///
/// 8191 是 winit/wgpu 侧的已知边界（egui #8361），超过会拖出窗口尺寸钳制类问题。
pub const MAX_WINDOW_SIDE: f32 = 8191.0;

/// `--panel-selftest` 的默认帧数：约 2 秒 @60fps，够走完首帧、字体图集预热与 wgpu 上传。
pub const DEFAULT_SELFTEST_FRAMES: u32 = 120;

/// 启动面板（无参数入口）；返回进程退出码。
pub fn run_panel() -> i32 {
    run(None)
}

/// `--panel-selftest [--frames N]`：开窗 → 渲染 N 帧 → 打印一行机器可读结果 → 退出码 0。
///
/// 输出格式（供无人值守量测解析）：`SELFTEST adapter=... atlas=... private_kb=...`
pub fn run_selftest(frames: u32) -> i32 {
    run(Some(frames))
}

/// 共用启动流程：单实例 → 建窗 → 渲染 → 返回退出码。
fn run(selftest_frames: Option<u32>) -> i32 {
    let log = Arc::new(LogBuffer::new());
    log.push(format!(
        "AriaHost {VERSION} 启动（{}）",
        mode_text(selftest_frames)
    ));

    let atlas_side = atlas_max_side(&log);

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
        "渲染配置：wgpu + MemoryHints::MemoryUsage（EG-3）；字形图集上限 {atlas_side} px（EG-7）"
    ));

    // EG-10：panic 也先进内部日志，再交给系统默认行为（不吞异常，但用户能在面板里看到记录）。
    install_panic_hook(&log);

    let options = native_options(atlas_side, &log);
    let facts = Arc::new(Mutex::new(RuntimeFacts::default()));
    let app_log = Arc::clone(&log);
    let app_facts = Arc::clone(&facts);

    let result = eframe::run_native(
        PRODUCT_NAME,
        options,
        Box::new(move |cc| {
            let panel = app::PanelApp::new(cc, app_log, app_facts, lock, selftest_frames);
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

/// `--panel-selftest` 的收尾输出：一行机器可读结果（键值对，值内空格替换为 `_`）。
fn report(selftest_frames: Option<u32>, facts: &Mutex<RuntimeFacts>) {
    if selftest_frames.is_none() {
        return;
    }
    let facts = facts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    println!(
        "SELFTEST adapter={} atlas={} private_kb={} backend={} frames={} font={}",
        sanitize(&facts.adapter),
        facts.atlas_side,
        facts.private_kb,
        sanitize(&facts.backend),
        facts.frames,
        sanitize(facts.font_resolved.as_deref().unwrap_or("none")),
    );
}

/// 让 `SELFTEST` 行保持「`key=value` 以空白分隔」可解析：值里的空白换成 `_`。
fn sanitize(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join("_")
}

/// 启动模式的中文描述（写进日志）。
fn mode_text(selftest_frames: Option<u32>) -> String {
    match selftest_frames {
        Some(frames) => format!("自测模式，渲染 {frames} 帧后退出"),
        None => "交互面板模式".to_owned(),
    }
}

/// 读取字形图集上限：默认 [`ATLAS_MAX_SIDE_DEFAULT`]，可用环境变量覆盖（见该常量注释）。
fn atlas_max_side(log: &LogBuffer) -> u32 {
    let Ok(raw) = std::env::var(ATLAS_MAX_SIDE_ENV) else {
        return ATLAS_MAX_SIDE_DEFAULT;
    };
    match raw.trim().parse::<u32>() {
        Ok(value) if (512..=16384).contains(&value) => {
            log.push(format!(
                "{ATLAS_MAX_SIDE_ENV}={value}：覆盖字形图集上限（默认 {ATLAS_MAX_SIDE_DEFAULT}）"
            ));
            value
        }
        _ => {
            log.push(format!(
                "{ATLAS_MAX_SIDE_ENV}={raw:?} 不是 512..=16384 的整数，忽略，沿用默认 {ATLAS_MAX_SIDE_DEFAULT}"
            ));
            ATLAS_MAX_SIDE_DEFAULT
        }
    }
}

/// 组装 eframe 原生窗口选项。
fn native_options(atlas_side: u32, log: &LogBuffer) -> eframe::NativeOptions {
    let icon = load_icon(log);
    eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_title(app::title_text())
            .with_app_id("AriaHost")
            .with_inner_size([1200.0, 760.0])
            .with_min_inner_size([760.0, 480.0])
            // EG-11：窗口尺寸钳制（恢复尺寸/用户拉伸都不得越过上限，egui #8361 未修）。
            .with_max_inner_size([MAX_WINDOW_SIDE, MAX_WINDOW_SIDE])
            .with_icon(icon),
        // EG-5：eframe 的默认 provider 不带 locale（会按系统 locale 选字，中文可能落到日文字形），
        // 所以这里关掉它，改由 `fonts::install` 注册带 `zh-CN` 的 provider（首帧前完成，EG-6）。
        system_font_fallback: false,
        centered: true,
        wgpu_options: wgpu_configuration(atlas_side),
        ..Default::default()
    }
}

/// wgpu 设备配置：内存策略与图集上限都在 device descriptor 里落实（EG-3 / EG-7）。
fn wgpu_configuration(atlas_side: u32) -> eframe::WgpuConfiguration {
    let mut setup = eframe::egui_wgpu::WgpuSetupCreateNew::without_display_handle();
    setup.device_descriptor = Arc::new(move |_adapter: &eframe::wgpu::Adapter| {
        eframe::wgpu::DeviceDescriptor {
            label: Some("ariahost wgpu device"),
            // EG-3：wgpu 默认 MemoryHints::Performance，Win11 上 eframe hello-world 开窗即 ~250MB
            // （wgpu #8453 实测）。改成 MemoryUsage 是硬要求，不是可选优化。
            memory_hints: eframe::wgpu::MemoryHints::MemoryUsage,
            required_limits: eframe::wgpu::Limits {
                // EG-7：字形图集上限（同时决定交换链像素尺寸上限，见常量注释）。
                max_texture_dimension_2d: atlas_side,
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
        assert_eq!(MAX_WINDOW_SIDE, 8191.0, "EG-11 要求窗口尺寸上限 8191 px");
    }

    #[test]
    fn atlas_default_is_the_design_value() {
        // 设计（EG-7）首选 2048；4096 只在真机窗口尺寸放不下时通过环境变量临时放开。
        assert_eq!(ATLAS_MAX_SIDE_DEFAULT, 2048);
    }

    #[test]
    fn mode_text_is_chinese() {
        assert!(mode_text(None).contains("面板"));
        assert!(mode_text(Some(3)).contains("3"));
    }
}
