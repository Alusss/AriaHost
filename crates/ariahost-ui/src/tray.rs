//! 托盘（M2-3，仅 Windows）。
//!
//! 设计要点：
//! - **真实可用**，不是占位：菜单五项（显示面板 / 启动 aria2 / 停止 aria2 / 在浏览器打开 / 退出），
//!   启用状态跟随 aria2 状态；左键单击 = 显示/隐藏面板，右键 = 菜单。
//! - **不加解码依赖**：图标优先走 `Icon::from_resource` 读 exe 自己的资源（`installer/ariahost.ico`
//!   已由 `crates/ariahost/build.rs` 嵌进 exe，零额外字节）；资源取不到时用程序化兜底图标
//!   （纯 `from_rgba`，不引 `ico`/`png`/`image`）。
//! - 事件走 `tray-icon`/`muda` 的全局通道，面板每帧 `poll()` 非阻塞取一次；**托盘自己不起线程、
//!   不建消息循环**——它在 winit 的消息泵里活着（架构 §4.x：主线程只有一个循环）。
//!
//! 重建/释放：`TrayIcon` drop 即从托盘区移除；面板退出时随 `PanelApp` 一起销毁。
use crate::logbuf::LogBuffer;
#[cfg(windows)]
use crate::notify::{ChannelAvailability, NotifyLevel};

#[cfg(windows)]
use ariahost_common::{PRODUCT_NAME, VERSION};
#[cfg(windows)]
use tray_icon::menu::{IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
#[cfg(windows)]
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
#[cfg(windows)]
use windows_sys::Win32::Foundation::{GetLastError, HWND};
#[cfg(windows)]
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_INFO, NIF_TIP, NIIF_ERROR, NIIF_INFO, NIIF_RESPECT_QUIET_TIME,
    NIIF_WARNING, NIM_MODIFY, NOTIFYICONDATAW,
};

/// 托盘触发的动作（面板每帧处理，全部非阻塞）。
#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    /// 左键单击图标：显示/隐藏面板。
    ToggleWindow,
    /// 菜单「显示面板」。
    ShowWindow,
    Start,
    Stop,
    OpenWeb,
    Quit,
}

/// 非 Windows 平台的占位：没有托盘（本项目只发 Windows）。
///
/// 之所以留同名 API 而不是把调用点都 `#[cfg(windows)]`，是为了让面板代码只有一条路径，
/// 也让 WSL/CI 的 Linux check 不需要引入 gtk/libappindicator。
#[cfg(not(windows))]
pub struct Tray;

#[cfg(not(windows))]
impl Tray {
    pub fn new(_log: &LogBuffer) -> Option<Self> {
        None
    }
    pub fn set_state(&mut self, _running: bool, _busy: bool) {}
    pub fn poll(&self) -> Vec<TrayAction> {
        Vec::new()
    }
    /// 非 Windows 没有 `Shell_NotifyIconW`：气泡恒为「发不出去」，但不 panic、不改调用点。
    pub fn balloon(
        &mut self,
        _level: crate::notify::NotifyLevel,
        _title: &str,
        _body: &str,
    ) -> bool {
        false
    }
    pub fn can_notify(&self) -> bool {
        false
    }
    /// 非 Windows 永远没有托盘通道。
    pub fn channel(&self) -> crate::notify::ChannelAvailability {
        crate::notify::ChannelAvailability::unavailable("not-windows")
    }
}

/// Linux/macOS 上这些动作永远不会被构造（没有托盘），保留只为让调用点只有一条路径。
#[cfg(not(windows))]
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    ToggleWindow,
    ShowWindow,
    Start,
    Stop,
    OpenWeb,
    Quit,
}

#[cfg(not(windows))]
impl TrayAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::ToggleWindow => "切换面板显示",
            Self::ShowWindow => "显示面板",
            Self::Start => "启动 aria2",
            Self::Stop => "停止 aria2",
            Self::OpenWeb => "在浏览器打开",
            Self::Quit => "退出",
        }
    }
}

/// 托盘图标的 Win32 身份：气泡必须打在**同一个** `hWnd + uID` 上。
///
/// 这是本文件唯一与 `Shell_NotifyIconW` 的第二次交互（第一次是 `tray-icon` 内部的
/// `NIM_ADD`）。**绝不能再 `NIM_ADD`**：那会留下幽灵图标、托盘区出现两个。
#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ShellIcon {
    hwnd: HWND,
    uid: u32,
}

/// 托盘句柄：菜单项句柄 + 图标所有权。
#[cfg(windows)]
pub struct Tray {
    icon: TrayIcon,
    show: MenuItem,
    start: MenuItem,
    stop: MenuItem,
    web: MenuItem,
    quit: MenuItem,
    last_state: Option<(bool, bool)>,
    /// 气泡要用的图标身份；取不到就没法发气泡（只影响通知，不影响托盘本身）。
    shell: Option<ShellIcon>,
    /// 通知通道可用性（建托盘时定下，之后只读）。
    channel: ChannelAvailability,
}

#[cfg(windows)]
impl Tray {
    /// 建托盘。失败只写日志、返回 `None`（没有托盘不该拦住面板开窗）。
    pub fn new(log: &LogBuffer) -> Option<Self> {
        let menu = Menu::new();
        let show = MenuItem::new("显示面板", true, None);
        let start = MenuItem::new("启动 aria2", true, None);
        let stop = MenuItem::new("停止 aria2", false, None);
        let web = MenuItem::new("在浏览器打开", true, None);
        let quit = MenuItem::new("退出", true, None);
        let separator_a = PredefinedMenuItem::separator();
        let separator_b = PredefinedMenuItem::separator();
        let items: [&dyn IsMenuItem; 7] = [
            &show,
            &start,
            &stop,
            &separator_a,
            &web,
            &separator_b,
            &quit,
        ];
        for item in items {
            if let Err(error) = menu.append(item) {
                log.push(format!("托盘菜单项添加失败：{error}（托盘继续）"));
            }
        }

        let icon = match load_icon() {
            Ok(icon) => icon,
            Err(reason) => {
                log.push(format!("托盘图标不可用：{reason}（托盘不显示）"));
                return None;
            }
        };

        let builder = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_icon(icon)
            .with_tooltip(format!("{} {}", PRODUCT_NAME, VERSION))
            // 左键切显隐、右键出菜单（Windows 惯例）。
            .with_menu_on_left_click(false);
        match builder.build() {
            Ok(icon) => {
                // 气泡的身份必须与 `tray-icon` 内部那次 `NIM_ADD` 完全一致：同一个 hWnd + uID。
                // hWnd 由 crate 暴露（`window_handle()`），uID 没暴露——它来自 tray-icon 内部的
                // 原子计数器（首个托盘 = 1），所以用「改一次同样的 tooltip」探出来，探不到就不发气泡。
                let tooltip = format!("{} {}", PRODUCT_NAME, VERSION);
                let discovered = discover_shell_icon(icon.window_handle(), &tooltip);
                let (shell, channel) = match discovered {
                    Ok(identity) => {
                        log.push(format!(
                            "托盘已就绪（左键显示/隐藏，右键菜单；气泡身份 hwnd=0x{:X} uid={}）",
                            identity.hwnd as usize, identity.uid
                        ));
                        (Some(identity), ChannelAvailability::balloon())
                    }
                    Err((reason, code)) => {
                        log.push(format!(
                            "托盘已就绪，但气泡身份探测失败（{reason}，Win32 {code}；\
                             通知降级为只写日志，关闭逻辑不得「藏进托盘」）"
                        ));
                        (None, ChannelAvailability::unavailable(reason))
                    }
                };
                Some(Self {
                    icon,
                    show,
                    start,
                    stop,
                    web,
                    quit,
                    last_state: None,
                    shell,
                    channel,
                })
            }
            Err(error) => {
                log.push(format!("托盘创建失败：{error}（面板继续，无托盘）"));
                None
            }
        }
    }

    /// 在**同一个**托盘图标上弹一条气泡（`NIF_INFO` + `NIM_MODIFY`）。
    ///
    /// 只能在创建托盘的那条 UI 线程上调用：`Shell_NotifyIconW` 作用于该线程拥有的窗口，
    /// 跨线程发虽然多数情况能显示，但窗口消息泵不在那条线程时行为不可靠（架构 §4.x 单循环）。
    /// 后台线程请走 [`crate::notify::Notifier::offer`] 排队，UI 线程 `drain` 后在这里发。
    ///
    /// 返回值：`true` = 系统接受了这条气泡；`false` = 没有托盘身份或被系统拒绝（调用方落日志）。
    pub fn balloon(&mut self, level: NotifyLevel, title: &str, body: &str) -> bool {
        let Some(identity) = self.shell else {
            return false;
        };
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = identity.hwnd;
        data.uID = identity.uid;
        data.uFlags = NIF_INFO;
        data.dwInfoFlags = match level {
            NotifyLevel::Info => NIIF_INFO,
            NotifyLevel::Warn => NIIF_WARNING,
            NotifyLevel::Error => NIIF_ERROR,
        } | NIIF_RESPECT_QUIET_TIME;
        // 10 秒；Win10/11 会把气泡转成系统通知，实际停留时长由系统策略决定。
        data.Anonymous.uTimeout = 10_000;
        fill_wide(&mut data.szInfoTitle, title);
        fill_wide(&mut data.szInfo, body);
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) != 0 }
    }

    /// 托盘身份是否可用于发气泡（自检与诊断用）。
    pub fn can_notify(&self) -> bool {
        self.shell.is_some()
    }

    /// 通道可用性：**关闭逻辑要用**——气泡/托盘不可用时不许「藏进托盘」。
    ///
    /// 这里已经包含 `NIM_MODIFY` 的实测结果（建托盘时探过一次 tooltip）：`shell-refused`
    /// 说明图标可能压根没进托盘区，此时「藏进托盘」会让用户彻底找不到程序。
    /// 之后 `balloon()` 的真实返回值仍以每次发送为准（系统策略可能临时静音）。
    pub fn channel(&self) -> ChannelAvailability {
        self.channel
    }

    /// 让菜单项的启用状态跟随 aria2 状态（`busy` = 启动/停止进行中，全禁）。
    pub fn set_state(&mut self, running: bool, busy: bool) {
        if self.last_state == Some((running, busy)) {
            return;
        }
        self.last_state = Some((running, busy));
        self.start.set_enabled(!busy && !running);
        self.stop.set_enabled(!busy && running);
        self.icon
            .set_tooltip(Some(format!(
                "{} {}（aria2 {}）",
                PRODUCT_NAME,
                VERSION,
                if running { "运行中" } else { "已停止" }
            )))
            .ok();
    }

    /// 非阻塞取本帧的托盘事件（左键点击 + 菜单点击）。
    pub fn poll(&self) -> Vec<TrayAction> {
        let mut actions = Vec::new();
        while let Ok(event) = TrayIconEvent::receiver().try_recv() {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                actions.push(TrayAction::ToggleWindow);
            }
        }
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            let id = event.id();
            let action = if id == self.show.id() {
                Some(TrayAction::ShowWindow)
            } else if id == self.start.id() {
                Some(TrayAction::Start)
            } else if id == self.stop.id() {
                Some(TrayAction::Stop)
            } else if id == self.web.id() {
                Some(TrayAction::OpenWeb)
            } else if id == self.quit.id() {
                Some(TrayAction::Quit)
            } else {
                None
            };
            if let Some(action) = action {
                actions.push(action);
            }
        }
        actions
    }
}

/// 取托盘图标：先读 exe 资源（零额外字节），失败再程序化兜底。
#[cfg(windows)]
fn load_icon() -> Result<Icon, String> {
    // `installer/ariahost.ico` 由 build.rs 以 winresource 嵌入，主图标资源序号为 1；
    // 个别工具链会给 2 / IDI_APPLICATION(32512)，一并试。
    for (ordinal, size) in [(1u16, (32, 32)), (2, (32, 32)), (32512, (32, 32))] {
        if let Ok(icon) = Icon::from_resource(ordinal, Some(size)) {
            return Ok(icon);
        }
    }
    let (rgba, width, height) = fallback_rgba();
    Icon::from_rgba(rgba, width, height).map_err(|error| format!("兜底图标也失败了：{error}"))
}

/// 程序化兜底图标：32×32，品牌蓝底 + 白色下载箭头（不依赖任何解码器）。
#[cfg(windows)]
fn fallback_rgba() -> (Vec<u8>, u32, u32) {
    const SIDE: u32 = 32;
    let mut rgba = vec![0u8; (SIDE * SIDE * 4) as usize];
    let center = (SIDE as f32 - 1.0) / 2.0;
    for y in 0..SIDE {
        for x in 0..SIDE {
            let index = ((y * SIDE + x) * 4) as usize;
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let inside = dx * dx + dy * dy <= (center - 1.0) * (center - 1.0);
            if !inside {
                continue;
            }
            // 箭头：竖杆 + 三角头（纯几何，不引字体/位图）。
            let in_shaft = dx.abs() <= 2.5 && (-6.0..=3.0).contains(&dy);
            let in_head = dy > 3.0 && dy <= 10.0 && dx.abs() <= (10.0 - dy) * 1.4;
            let (r, g, b) = if in_shaft || in_head {
                (255u8, 255u8, 255u8)
            } else {
                (32u8, 96u8, 200u8)
            };
            rgba[index] = r;
            rgba[index + 1] = g;
            rgba[index + 2] = b;
            rgba[index + 3] = 255;
        }
    }
    (rgba, SIDE, SIDE)
}

/// 探测托盘图标的 `uID`：用「把 tooltip 改成同一个值」这条**无视觉副作用**的
/// `NIM_MODIFY` 试 1..=8，第一个返回 TRUE 的就是本进程那张图标。
///
/// 为什么不用常量 1：`tray-icon` 的 `internal_id` 取自进程内原子计数器，理论上被别处
/// 先建过一次图标就会错位。探测一次的成本是 8 次内核调用、零可见影响，比写死安全。
///
/// 失败返回 `(原因, 最后一个 Win32 错误码)`：只有「`NIM_MODIFY` 被拒」这一条能解释
/// 「托盘图标在、气泡发不出」，把这个码写进日志才不至于下次又要从头排查
/// （实测某环境 8 次全返回 0，`GetLastError`=5 拒绝访问）。
#[cfg(windows)]
fn discover_shell_icon(hwnd: HWND, tooltip: &str) -> Result<ShellIcon, (&'static str, u32)> {
    let mut last_error = 0u32;
    for uid in 1..=8u32 {
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = hwnd;
        data.uID = uid;
        data.uFlags = NIF_TIP;
        fill_wide(&mut data.szTip, tooltip);
        if unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) } != 0 {
            return Ok(ShellIcon { hwnd, uid });
        }
        last_error = unsafe { GetLastError() };
    }
    Err(("shell-refused", last_error))
}

/// 把 UTF-8 文本按宽字符写进定长缓冲区（截断 + NUL 结尾）。
///
/// 气泡正文上限 255 字符、标题 63 字符（Win32 结构体定长），超长只能截断——
/// 宁可少几个字，也不要因为超长被系统整条丢掉。
#[cfg(windows)]
fn fill_wide(destination: &mut [u16], text: &str) {
    let limit = destination.len().saturating_sub(1);
    let mut written = 0;
    for unit in text.encode_utf16() {
        if written == limit {
            break;
        }
        destination[written] = unit;
        written += 1;
    }
    destination[written] = 0;
}

/// 供面板把动作写进日志时用的中文名。
#[cfg(windows)]
impl TrayAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::ToggleWindow => "切换面板显示",
            Self::ShowWindow => "显示面板",
            Self::Start => "启动 aria2",
            Self::Stop => "停止 aria2",
            Self::OpenWeb => "在浏览器打开",
            Self::Quit => "退出",
        }
    }
}
