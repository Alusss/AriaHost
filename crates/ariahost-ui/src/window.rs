//! 窗口几何持久化（M2-1）：`%LOCALAPPDATA%\AriaHost\window.json`。
//!
//! 单位是 **egui 点**（与 `ViewportInfo::inner_rect` / `ViewportCommand::InnerSize` 一致），
//! 不是物理像素：换了 125%/150% 缩放后同一份「点」几何仍然有意义，物理像素则不会。
//!
//! 三件事必须守住：
//! 1. **原子写**：先写 `window.json.tmp` 再 `rename` 覆盖，避免半截文件把下次启动卡在坏几何上；
//! 2. **越界校正**：读到屏幕外的坐标（拔了外接屏、改了分辨率）要能回到可见区；
//! 3. **EG-11 上限**：宽高仍受 [`crate::MAX_WINDOW_SIDE`] 钳制，不因为持久化而绕过。
//!
//! 多显示器的边界：egui 只暴露**当前**显示器的 `monitor_size`（没有全量 monitor 列表），
//! 所以这里做的是「单屏可见性校正」——完全跑到屏幕外的几何会回中心；
//! 负坐标（左侧副屏）只要仍有足够可见面积就原样保留。枚举全部显示器留到后续批次。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::MAX_WINDOW_SIDE;

/// 默认窗口宽（点）。
pub const DEFAULT_WIDTH: f32 = 1200.0;
/// 默认窗口高（点）。
pub const DEFAULT_HEIGHT: f32 = 760.0;
/// 最小窗口宽（点），与 `NativeOptions` 的 `min_inner_size` 一致。
pub const MIN_WIDTH: f32 = 760.0;
/// 最小窗口高（点）。
pub const MIN_HEIGHT: f32 = 480.0;
/// 判定「还在屏幕上」所需的可见宽（点）：至少露出这么多，用户才抓得住标题栏。
pub const MIN_VISIBLE_WIDTH: f32 = 160.0;
/// 判定「还在屏幕上」所需的可见高（点）。
pub const MIN_VISIBLE_HEIGHT: f32 = 60.0;
/// 坐标绝对值上限（点）：挡住手改 json 写进来的天文数字。
pub const MAX_COORDINATE: f32 = 20000.0;

/// 持久化的窗口几何（左上角 + 内尺寸，单位点）。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Geometry {
    /// 外框左上角 x（点）。
    pub x: f32,
    /// 外框左上角 y（点）。
    pub y: f32,
    /// 客户区宽（点）。
    pub width: f32,
    /// 客户区高（点）。
    pub height: f32,
}

impl Default for Geometry {
    fn default() -> Self {
        Self {
            x: 0.0,
            y: 0.0,
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
        }
    }
}

impl Geometry {
    /// 是否与另一份几何等价（用于「有没有变过」的廉价判断）。
    pub fn same_as(self, other: Self) -> bool {
        (self.x - other.x).abs() < 0.5
            && (self.y - other.y).abs() < 0.5
            && (self.width - other.width).abs() < 0.5
            && (self.height - other.height).abs() < 0.5
    }
}

/// `window.json` 的路径：`%LOCALAPPDATA%\AriaHost\window.json`。
///
/// 复用 [`ariahost_common::paths::cache_dir`] 的父目录，避免在 `ariahost-common` 里新增
/// 「窗口状态」这种只服务 UI 的路径约定（`ariahost-common` 不在本批写入范围）。
pub fn window_file() -> Option<PathBuf> {
    ariahost_common::paths::cache_dir()
        .and_then(|cache| cache.parent().map(Path::to_path_buf))
        .map(|root| root.join("window.json"))
}

/// 读取几何：文件不存在、JSON 坏掉、数值不可用时一律返回 `None`（调用方用默认几何）。
pub fn load(path: &Path) -> Option<Geometry> {
    let raw = fs::read_to_string(path).ok()?;
    let geometry: Geometry = serde_json::from_str(&raw).ok()?;
    geometry.is_finite().then_some(geometry)
}

/// 原子写入几何：父目录按需创建，先写 `.tmp` 再 `rename` 覆盖。
pub fn save(path: &Path, geometry: Geometry) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut tmp = path.to_path_buf();
    tmp.set_extension("json.tmp");
    let body = serde_json::to_string_pretty(&geometry).map_err(io::Error::other)?;
    fs::write(&tmp, body.as_bytes())?;
    // `rename` 在同目录内是原子的；Windows 上 std 会用 MOVEFILE_REPLACE_EXISTING 覆盖旧文件。
    fs::rename(&tmp, path)
}

/// 校正几何：钳制尺寸（`[MIN_*, MAX_WINDOW_SIDE]`）+ 越界回中心。
///
/// `screen` 是要显示的显示器尺寸（点）；`None` 表示拿不到显示器信息（例如 Wayland），
/// 此时只做尺寸钳制与坐标上限钳制，不猜位置。
pub fn sanitize(geometry: Geometry, screen: Option<(f32, f32)>) -> Geometry {
    let width = clamp_dimension(geometry.width, MIN_WIDTH);
    let height = clamp_dimension(geometry.height, MIN_HEIGHT);
    let mut x = clamp_coordinate(geometry.x);
    let mut y = clamp_coordinate(geometry.y);

    if let Some((screen_width, screen_height)) = screen {
        let (screen_width, screen_height) = (
            screen_width.clamp(MIN_WIDTH, MAX_WINDOW_SIDE),
            screen_height.clamp(MIN_HEIGHT, MAX_WINDOW_SIDE),
        );
        if !is_visible(x, y, width, height, screen_width, screen_height) {
            x = ((screen_width - width) / 2.0).max(0.0);
            y = ((screen_height - height) / 2.0).max(0.0);
        }
    }

    Geometry {
        x,
        y,
        width,
        height,
    }
}

impl Geometry {
    /// 四个分量都必须是有限数（NaN/∞ 从 JSON 里也能解析出来，必须在进门处挡住）。
    fn is_finite(self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && self.width.is_finite()
            && self.height.is_finite()
    }
}

fn clamp_dimension(value: f32, minimum: f32) -> f32 {
    if !value.is_finite() {
        return minimum;
    }
    value.clamp(minimum, MAX_WINDOW_SIDE)
}

fn clamp_coordinate(value: f32) -> f32 {
    if !value.is_finite() {
        return 0.0;
    }
    value.clamp(-MAX_COORDINATE, MAX_COORDINATE)
}

/// 窗口是否还有足够可见面积落在 `[0, screen]` 里。
fn is_visible(
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    screen_width: f32,
    screen_height: f32,
) -> bool {
    let visible_width = (x + width).min(screen_width) - x.max(0.0);
    let visible_height = (y + height).min(screen_height) - y.max(0.0);
    visible_width >= MIN_VISIBLE_WIDTH && visible_height >= MIN_VISIBLE_HEIGHT
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// 每个测试用独立目录，避免并行测试互踩（不引入 tempfile 依赖）。
    fn temp_path(tag: &str) -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!(
                "ariahost-window-{}-{tag}-{unique}",
                std::process::id()
            ))
            .join("window.json")
    }

    #[test]
    fn roundtrip_through_json() {
        let path = temp_path("roundtrip");
        let geometry = Geometry {
            x: -300.0,
            y: 120.0,
            width: 1400.0,
            height: 900.0,
        };
        save(&path, geometry).expect("保存应成功");
        let loaded = load(&path).expect("读回应成功");
        assert!(loaded.same_as(geometry), "{loaded:?}");
        let _ = fs::remove_dir_all(path.parent().expect("有父目录"));
    }

    #[test]
    fn load_returns_none_for_missing_file() {
        assert!(load(&temp_path("missing")).is_none());
    }

    #[test]
    fn load_returns_none_for_garbage_or_non_finite() {
        let path = temp_path("garbage");
        fs::create_dir_all(path.parent().expect("有父目录")).expect("建目录");
        fs::write(&path, b"{ this is not json }").expect("写坏文件");
        assert!(load(&path).is_none());

        // serde_json 会接受 null 之类吗？这里直接写 NaN 字面量：JSON 不允许，必须被拒。
        fs::write(&path, br#"{"x":0.0,"y":0.0,"width":1e999,"height":760.0}"#)
            .expect("写超范围文件");
        assert!(load(&path).is_none(), "无穷大必须当坏文件处理");
        let _ = fs::remove_dir_all(path.parent().expect("有父目录"));
    }

    #[test]
    fn sanitize_clamps_size_into_eg11_range() {
        let huge = Geometry {
            x: 0.0,
            y: 0.0,
            width: 99999.0,
            height: 99999.0,
        };
        let fixed = sanitize(huge, None);
        assert_eq!(fixed.width, MAX_WINDOW_SIDE);
        assert_eq!(fixed.height, MAX_WINDOW_SIDE);

        let tiny = Geometry {
            x: 0.0,
            y: 0.0,
            width: 10.0,
            height: 10.0,
        };
        let fixed = sanitize(tiny, None);
        assert_eq!(fixed.width, MIN_WIDTH);
        assert_eq!(fixed.height, MIN_HEIGHT);
    }

    #[test]
    fn sanitize_recenters_window_that_is_off_screen() {
        // 拔掉外接屏的典型形态：坐标留在已经不存在的右侧显示器上。
        let off_screen = Geometry {
            x: 5000.0,
            y: 300.0,
            width: 1200.0,
            height: 760.0,
        };
        let fixed = sanitize(off_screen, Some((1920.0, 1080.0)));
        assert!(
            fixed.x >= 0.0 && fixed.x + fixed.width <= 1920.0,
            "{fixed:?}"
        );
        assert!(
            fixed.y >= 0.0 && fixed.y + fixed.height <= 1080.0,
            "{fixed:?}"
        );
        // 居中而不是贴边
        assert!((fixed.x - (1920.0 - 1200.0) / 2.0).abs() < 1.0, "{fixed:?}");
    }

    #[test]
    fn sanitize_keeps_negative_position_when_still_visible() {
        // 左侧副屏：x 为负但右边缘在主屏可见区里，不该被强行挪回来。
        let on_left_monitor = Geometry {
            x: -400.0,
            y: 100.0,
            width: 1200.0,
            height: 800.0,
        };
        let fixed = sanitize(on_left_monitor, Some((1920.0, 1080.0)));
        assert!((fixed.x + 400.0).abs() < 0.5, "{fixed:?}");
        assert!((fixed.y - 100.0).abs() < 0.5, "{fixed:?}");
    }

    #[test]
    fn sanitize_without_screen_only_clamps_numbers() {
        let absurd = Geometry {
            x: 1.0e9,
            y: -1.0e9,
            width: 1200.0,
            height: 760.0,
        };
        let fixed = sanitize(absurd, None);
        assert_eq!(fixed.x, MAX_COORDINATE);
        assert_eq!(fixed.y, -MAX_COORDINATE);
        assert_eq!(fixed.width, 1200.0);
    }

    #[test]
    fn save_is_atomic_and_leaves_no_tmp_behind() {
        let path = temp_path("atomic");
        save(&path, Geometry::default()).expect("保存应成功");
        save(&path, Geometry::default()).expect("覆盖应成功");
        let mut tmp = path.clone();
        tmp.set_extension("json.tmp");
        assert!(!tmp.exists(), "临时文件必须被 rename 掉");
        assert!(path.exists());
        let _ = fs::remove_dir_all(path.parent().expect("有父目录"));
    }
}
