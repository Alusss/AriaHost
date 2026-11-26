//! 构建脚本：把品牌图标与版本资源嵌进 `ariahost.exe`（仅 Windows 目标）。
//!
//! 资源来源与安装器**同一份** `installer/ariahost.ico`（16/24/32/48/64/128/256，32bpp），
//! 避免「exe 图标与安装器图标不一致」。
//!
//! 依赖：MSVC 目标下 `winresource` 要调用 Windows SDK 的 `rc.exe`。它按
//! `RC_PATH` → `WindowsSdkVerBinPath` → 注册表 `HKLM\SOFTWARE\Microsoft\Windows Kits\Installed Roots`
//! 的顺序自行定位（不走 PATH）；CI 上额外有 `ilammy/msvc-dev-cmd` 兜底，见 `.github/workflows/`。

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // 相对路径按本 crate 根解析（crates/ariahost）→ 仓库根的 installer/ariahost.ico。
    println!("cargo:rerun-if-changed=../../installer/ariahost.ico");

    // 不要用 `#[cfg(windows)]`：build.rs 编译到**宿主**上，`cfg(target_os)` 在 build 脚本里
    // 恒等于宿主系统，交叉编译时会把资源写进非 Windows 产物。这里读运行时的真正目标系统。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // 逃生开关：仅用于「在没有 Windows SDK 的宿主上做 windows 目标的 check/clippy」。
    //
    // 为什么需要它：`#[cfg(windows)]` 的代码在 Linux 目标下根本不编译，本地 clippy 看不见
    // 那些 lint（已被咬过两次：tray.rs 的 manual_range_contains、diagnose.rs 的四条）。
    // 想本地提前抓，就要 `cargo clippy --target x86_64-pc-windows-gnu`；而这条路上
    // `winresource` 会把路径里的 `/` 转成 `\`，windres 因此打不开 ICO 直接失败，
    // 于是整个 gate 被一个**与被测代码无关**的资源步骤挡住。
    //
    // 设 `ARIAHOST_SKIP_ICON=1` 只跳过资源嵌入，**不影响任何 Rust 代码的编译与 lint**；
    // 正式构建（CI/发布）不设该变量，资源照常嵌入。
    if std::env::var_os("ARIAHOST_SKIP_ICON").is_some() {
        println!(
            "cargo:warning=ARIAHOST_SKIP_ICON 已设置：跳过图标/版本资源嵌入（仅用于跨目标 lint）"
        );
        return;
    }

    let icon = icon_path();

    let mut res = winresource::WindowsResource::new();
    // 图标：多尺寸 ICO 由 Pillow 从 assets/branding/ariahost-icon-256.png 生成（见 docs/dev/build.md）。
    res.set_icon(&icon);
    res.set("ProductName", "AriaHost");
    res.set("FileDescription", "AriaHost");
    // FileVersion / ProductVersion 的字符串与数值字段默认都取 CARGO_PKG_VERSION；
    // 这里显式再写一次，免得将来 winresource 行为漂移时静默退化。
    res.set("FileVersion", env!("CARGO_PKG_VERSION"));
    res.set("ProductVersion", env!("CARGO_PKG_VERSION"));
    res.set("CompanyName", "AriaHost");
    res.set("LegalCopyright", "Copyright (c) 2026 Alusss");
    res.set("OriginalFilename", "ariahost.exe");
    res.set("InternalName", "ariahost");

    if let Err(err) = res.compile() {
        panic!(
            "嵌入 Windows 资源失败：{err}。\
             MSVC 目标需要 Windows SDK 的 rc.exe：可在 CI 里启用 ilammy/msvc-dev-cmd，\
             或设置 RC_PATH / WindowsSdkVerBinPath 指向 SDK 的 bin 目录。"
        );
    }
}

/// 图标绝对路径，**形态按「谁在编译资源」分流**。
///
/// - MSVC 目标：`rc.exe` 只认反斜杠，且这类构建只在 Windows 宿主上跑 → 统一换成 `\`。
/// - GNU 目标 + 非 Windows 宿主：`windres` 是 POSIX 工具，反斜杠会被当成字面字符，
///   实测报 `can't open icon file '\mnt\f\...'` → 必须给斜杠形态。
///
/// 这一条是 task-99 的实测产物：GNU release 全链接本身没问题，唯一挡住产包的是这个路径形态。
fn icon_path() -> String {
    let abs = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("installer")
        .join("ariahost.ico");
    assert!(
        abs.exists(),
        "缺少图标文件：{}（应由仓库内 installer/ariahost.ico 提供）",
        abs.display()
    );
    let raw = abs.display().to_string();
    if uses_posix_resource_tool() {
        return raw;
    }
    raw.replace('/', "\\")
}

/// 资源编译器是否是 POSIX 形态（GNU 目标 + 非 Windows 宿主）。
fn uses_posix_resource_tool() -> bool {
    let gnu_target = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu");
    let windows_host = std::env::var("HOST").is_ok_and(|host| host.contains("windows"));
    gnu_target && !windows_host
}
