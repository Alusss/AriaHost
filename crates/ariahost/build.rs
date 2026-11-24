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

/// 图标绝对路径。rc.exe 只认反斜杠，所以在 Windows 上把分隔符统一成 `\`。
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
    abs.display().to_string().replace('/', "\\")
}
