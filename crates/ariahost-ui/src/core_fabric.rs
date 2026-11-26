//! 核心状态的**单一解释器**（task-81 / M5-a）。
//!
//! ## 为什么需要它
//!
//! 面板里有三条与「核心在哪跑」有关的事实来源，之前各说各话：
//!
//! 1. `ariahost-svc` 的 SCM 服务态（worker 每 2s 轮询一次）；
//! 2. 本进程托管的 `aria2c`（`core_host` 的 [`HostState`]）；
//! 3. 6800 端口到底有没有人听（服务 / 本机 / 外部程序）。
//!
//! 三条不合并就会出现两个真实缺陷：服务已经在跑，下载页仍说「核心未就绪」并给一个
//! 在线下载按钮（点了必然失败，因为 6800 被服务占着）；反过来，面板托管着 aria2 时
//! 用户去服务页点启动，服务起不来还报一堆听不懂的错。
//!
//! ## 判定顺序（顺序即优先级，不允许旁路）
//!
//! **服务运行中 > 面板托管活动 > 端口被外部程序占 > 未运行**
//!
//! 服务优先的理由：服务是「开机就在、跨会话」的那一份，面板只是旁观者；它一旦在跑，
//! 面板**绝不**再 spawn 自己的 aria2c（那会撞 6800，且两份核心抢同一个下载目录）。
//!
//! 本模块只有纯函数 + 一个缓存端口探测结果的薄壳（[`Fabric`]），不碰 egui、不做 IO，
//! 因此可以在 Linux 上直接单测（本机唯一能跑的闸门）。

use std::time::{Duration, Instant};

use ariahost_svc::ServiceState;

use crate::core_host::HostState;
use crate::view::{StatusView, Tone};
use crate::worker::ServiceSnapshot;

/// 配置里的默认 RPC 端口（`aria2.rpc.port`）。
pub const RPC_PORT_DEFAULT: u16 = 6800;

/// 端口探测的取样间隔：端口归属不会毫秒级变化，1s 足够，且避免每帧一次 connect。
const PORT_PROBE_INTERVAL: Duration = Duration::from_millis(1000);

/// 核心当前由谁提供。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Windows 服务（`AriaHost`）。
    Service,
    /// 本进程托管的 `aria2c`。
    Local,
    /// 没有任何在跑的核心。
    None,
}

impl Origin {
    /// 中文名（面板与日志共用，**不要**解析英文值）。
    pub const fn label_zh(self) -> &'static str {
        match self {
            Self::Service => "服务",
            Self::Local => "面板托管",
            Self::None => "未运行",
        }
    }
}

/// SCM 服务态折叠成三档（六态里只有 Running 算「在跑」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServicePhase {
    /// 查询成功且是 `Running`。
    Running,
    /// 查询成功，但不是 `Running`（未安装 / 已停止 / 过渡态）。
    NotRunning,
    /// 查不到（SCM 查询失败）：**不能当成「没装」**，要如实说「未知」。
    Unknown,
}

/// 从 worker 的服务快照里取服务态（纯函数）。
pub fn service_phase(snapshot: Option<&ServiceSnapshot>) -> ServicePhase {
    match snapshot {
        Some(ServiceSnapshot::State(report)) => match report.state {
            ServiceState::Running => ServicePhase::Running,
            _ => ServicePhase::NotRunning,
        },
        // 查询失败、或还没拿到第一份快照：都算「未知」。
        Some(ServiceSnapshot::Failed { .. }) | None => ServicePhase::Unknown,
    }
}

/// 本进程托管的状态折叠成判定只需要知道的几档。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalPhase {
    /// 还没启动过（或已正常停止）。
    Idle,
    /// 正在等 RPC 就绪。
    Starting,
    /// RPC 已就绪。
    Running,
    /// 正在优雅停止。
    Stopping,
    /// 上次启动失败。
    Failed,
}

impl LocalPhase {
    /// 是否算「面板正占着核心」（含过渡态：过渡中也不允许服务抢端口）。
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Stopping)
    }
}

/// 从 `HostState` 折叠（纯函数）。
pub fn local_phase(state: &HostState) -> LocalPhase {
    match state {
        HostState::Idle => LocalPhase::Idle,
        HostState::Starting => LocalPhase::Starting,
        HostState::Running { .. } => LocalPhase::Running,
        HostState::Stopping => LocalPhase::Stopping,
        HostState::Stopped { .. } => LocalPhase::Idle,
        HostState::Failed { .. } => LocalPhase::Failed,
    }
}

/// 6800 端口的占用归属（由「能不能连上」+ 已知归属推断）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortOwner {
    /// 没人监听。
    Free,
    /// 有人在监听，且（据服务/本机事实）是我们自己的一份核心。
    Ours,
    /// 有人在监听，但既不是服务也不是本机托管：**外部程序**。
    Foreign,
    /// 探测失败或还没探测过。
    Unknown,
}

/// 一次判定所需的全部事实。
#[derive(Debug, Clone)]
pub struct Facts {
    /// SCM 服务态。
    pub service: ServicePhase,
    /// 本进程托管态。
    pub local: LocalPhase,
    /// 端口占用归属。
    pub port: PortOwner,
    /// 端口是否有人在监听（探测结果；`false` = 确实没人听）。
    pub port_listening: bool,
    /// 定位到的 aria2c 是否可用（`core_probe.ready()`）。
    pub probe_ready: bool,
    /// 本机 aria2c 的版本（探测得到才有）。
    pub local_version: Option<String>,
    /// 服务侧版本：**本批拿不到**（服务用 SYSTEM 的 secret，面板没有凭据），如实留空。
    pub service_version: Option<String>,
    /// RPC 端口（`aria2.rpc.port`，默认 6800）。
    pub rpc_port: u16,
    /// 核心不可用的原因（`core_probe.missing`）。
    pub missing: Option<String>,
}

/// 主按钮的可用性（启动 / 停止）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartButton {
    /// 是否可点。
    pub enabled: bool,
    /// 按钮文字（标签随来源变化，避免「启动 aria2」在服务托管时骗人）。
    pub label: &'static str,
    /// 悬停说明（不可点时必须说清为什么）。
    pub tip: String,
}

/// 一次判定的结果（渲染层只读这个，不再自己拼）。
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// 谁在提供核心。
    pub origin: Origin,
    /// 状态区视图（徽章 / 语气 / 主文案 / 详情 / 下一步）。
    pub view: StatusView,
    /// 启动按钮。
    pub start: StartButton,
    /// RPC 端口（面板「RPC 端点」那一行用它，不再写死）。
    pub rpc_port: u16,
    /// 「为什么是这个状态」的一句话（没有就 `None`）。
    pub reason: Option<String>,
}

/// 端口被外部程序占用时的下一步（可执行，不甩锅）。
fn foreign_port_next(port: u16) -> String {
    format!("先在任务管理器里结束占用 {port} 的程序；或到「设置 → 下载 → 下载核心」改端口后重启")
}

/// 统一判定（纯函数）：**这是全产品唯一的「核心现在是什么状态」结论**。
pub fn decide(facts: &Facts) -> Decision {
    let port = facts.rpc_port;
    let version_text = |version: &Option<String>| match version {
        Some(version) => format!("v{version}"),
        None => "版本未知".to_owned(),
    };

    // ① 冲突：服务在跑，面板托管的也在跑/在过渡。6800 只有一个，必须喊出来。
    if facts.service == ServicePhase::Running && facts.local.is_active() {
        let headline = format!(
            "{}：运行中 {}（与面板托管的 aria2 冲突）",
            Origin::Service.label_zh(),
            version_text(&facts.service_version)
        );
        return Decision {
            origin: Origin::Service,
            view: StatusView {
                badge: "冲突".to_owned(),
                tone: Tone::Bad,
                code: None,
                headline,
                details: vec![format!("两个核心都在跑：6800 只能给一个，下载会互相干扰")],
                next_step: Some("先点「停止 aria2」停掉面板托管的那一份".to_owned()),
            },
            start: StartButton {
                enabled: false,
                label: "由服务托管",
                tip: "服务在跑；面板不再启动第二个核心".to_owned(),
            },
            rpc_port: port,
            reason: Some("服务与面板托管同时活动".to_owned()),
        };
    }

    // ② 服务运行中：只附着，不 spawn。
    if facts.service == ServicePhase::Running {
        let headline = format!(
            "{}：运行中 {}",
            Origin::Service.label_zh(),
            version_text(&facts.service_version)
        );
        let mut details = vec![format!(
            "服务 `AriaHost` 托管核心，RPC 端口 {port}（面板只读观察，不启动第二个 aria2c）"
        )];
        if facts.service_version.is_none() {
            details.push(
                "版本未读到：服务用 SYSTEM 的 RPC secret，面板没有该凭据（本批不猜）".to_owned(),
            );
        }
        return Decision {
            origin: Origin::Service,
            view: StatusView {
                badge: "运行中".to_owned(),
                tone: Tone::Good,
                code: None,
                headline,
                details,
                next_step: None,
            },
            start: StartButton {
                enabled: false,
                label: "由服务托管",
                tip: "启动/停止由 Windows 服务负责：到「服务」页操作".to_owned(),
            },
            rpc_port: port,
            reason: None,
        };
    }

    // ③ 面板托管活动：启动中 / 运行中 / 停止中。
    if facts.local.is_active() {
        let badge = match facts.local {
            LocalPhase::Starting => "启动中",
            LocalPhase::Stopping => "停止中",
            _ => "运行中",
        };
        let suffix = match facts.local {
            LocalPhase::Starting => "启动中".to_owned(),
            LocalPhase::Stopping => "停止中".to_owned(),
            _ => version_text(&facts.local_version),
        };
        return Decision {
            origin: Origin::Local,
            view: StatusView {
                badge: badge.to_owned(),
                tone: if facts.local == LocalPhase::Running {
                    Tone::Good
                } else {
                    Tone::Idle
                },
                code: None,
                headline: format!("{}：{suffix}", Origin::Local.label_zh()),
                details: vec![format!("面板托管的 aria2c，RPC 端口 {port}")],
                next_step: None,
            },
            start: StartButton {
                enabled: facts.local != LocalPhase::Stopping,
                label: if facts.local == LocalPhase::Stopping {
                    "正在停止"
                } else {
                    "停止 aria2"
                },
                tip: if facts.local == LocalPhase::Stopping {
                    "停止链在跑（实测 4.1–4.3 秒）".to_owned()
                } else {
                    "优雅停止（先存会话）；之后服务才能接管 6800".to_owned()
                },
            },
            rpc_port: port,
            reason: None,
        };
    }

    // ④ 没人跑，但 6800 有人听：外部程序（或半死不活的核心）占着。
    if facts.port_listening || facts.port == PortOwner::Foreign {
        return Decision {
            origin: Origin::None,
            view: StatusView {
                badge: "被占用".to_owned(),
                tone: Tone::Warn,
                code: None,
                headline: format!("未运行（{port} 端口被其它程序占用）"),
                details: vec![format!(
                    "监听 127.0.0.1:{port} 的不是 AriaHost 服务，也不是面板托管的核心"
                )],
                next_step: Some(foreign_port_next(port)),
            },
            start: StartButton {
                enabled: false,
                label: "启动核心（本机 aria2c）",
                tip: format!("{port} 被占用，先释放端口（启动必然失败）"),
            },
            rpc_port: port,
            reason: Some(format!("{port} 被外部程序占用")),
        };
    }

    // ⑤ 真的什么都没跑。
    let (badge, tone, headline, details, next_step, reason) = if let Some(missing) = &facts.missing
    {
        (
            "未运行",
            Tone::Warn,
            "未运行（核心不可用）".to_owned(),
            vec![missing.clone()],
            Some("到「设置 → 下载 → 下载核心」查看来源与安装状态".to_owned()),
            Some(missing.clone()),
        )
    } else if facts.local == LocalPhase::Failed {
        (
            "失败",
            Tone::Bad,
            "未运行（上次启动失败）".to_owned(),
            vec!["失败原因见上方错误条；修好后可再点一次启动".to_owned()],
            Some("按错误条里的「下一步」处理后重试".to_owned()),
            Some("上次启动失败".to_owned()),
        )
    } else if facts.service == ServicePhase::Unknown && facts.port == PortOwner::Unknown {
        (
            "未知",
            Tone::Idle,
            "未运行（服务状态查不到）".to_owned(),
            vec!["SCM 查询失败：可能没有管理员权限，或服务被禁用".to_owned()],
            Some("到「服务」页点「刷新状态」再核对一次".to_owned()),
            Some("服务状态未知".to_owned()),
        )
    } else {
        (
            "未运行",
            Tone::Idle,
            "未运行".to_owned(),
            vec![format!(
                "服务：{}；端口 {port}：空闲",
                match facts.service {
                    ServicePhase::Running => "运行中",
                    ServicePhase::NotRunning => "未运行",
                    ServicePhase::Unknown => "未知",
                }
            )],
            Some("点「启动核心（本机 aria2c）」；或到「服务」页启动 Windows 服务".to_owned()),
            None,
        )
    };

    Decision {
        origin: Origin::None,
        view: StatusView {
            badge: badge.to_owned(),
            tone,
            code: None,
            headline,
            details,
            next_step,
        },
        start: if facts.probe_ready {
            StartButton {
                enabled: true,
                label: "启动核心（本机 aria2c）",
                tip: "启动本机 aria2c 并等 RPC 就绪（实测 0.72 秒）".to_owned(),
            }
        } else {
            StartButton {
                enabled: false,
                label: "启动核心（本机 aria2c）",
                tip: "本机 aria2c 不可用：到「设置 → 下载 → 下载核心」处理".to_owned(),
            }
        },
        rpc_port: port,
        reason,
    }
}

/// 服务页「启动服务」的前置检查（**绝不抢 6800**）。
///
/// 返回 `Err` 时服务页必须直接把这句话显示出来并禁用按钮，而不是让用户点了再去
/// 看 UAC 失败——服务起不来的真实原因（端口被面板托管的核心占着）在错误码里读不出来。
pub fn can_start_service(facts: &Facts) -> Result<(), &'static str> {
    if facts.local.is_active() {
        return Err("面板托管的 aria2 正在使用 6800：先到下载页点「停止 aria2」再启动服务");
    }
    if facts.port_listening
        && facts.port != PortOwner::Ours
        && facts.service != ServicePhase::Running
    {
        return Err("6800 已被其它程序占用：先释放端口再启动服务");
    }
    Ok(())
}

/// 端口探测结果的薄缓存（唯一的「非纯」部分：只做节流的 connect）。
#[derive(Debug, Default)]
pub struct Fabric {
    /// 上次探测时间。
    probed_at: Option<Instant>,
    /// 上次探测结论：是否有人监听。
    listening: bool,
}

impl Fabric {
    /// 新实例（还没探测过）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 取「6800 是否有人监听」，最多每秒真探一次。
    ///
    /// 探测方式：`127.0.0.1:port` 上做一次 120ms 超时的 TCP connect。连上 = 有人在听
    /// （本产品的核心、或外部程序）；连不上 = 空闲。**不做 RPC**（没有 secret 也会被拒，
    /// 反而把「端口没人听」和「端口有人听但不认我们」混成一件事）。
    pub fn port_listening(&mut self, port: u16) -> bool {
        let now = Instant::now();
        if let Some(at) = self.probed_at {
            if now.duration_since(at) < PORT_PROBE_INTERVAL {
                return self.listening;
            }
        }
        let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        self.listening =
            std::net::TcpStream::connect_timeout(&address, Duration::from_millis(120)).is_ok();
        self.probed_at = Some(now);
        self.listening
    }

    /// 组装判定所需事实里的端口两项（供渲染层一次调用）。
    pub fn port_fact(&mut self, port: u16, ours: bool) -> (PortOwner, bool) {
        let listening = self.port_listening(port);
        let owner = if !listening {
            PortOwner::Free
        } else if ours {
            PortOwner::Ours
        } else {
            PortOwner::Foreign
        };
        (owner, listening)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(service: ServicePhase, local: LocalPhase, port: PortOwner, listening: bool) -> Facts {
        Facts {
            service,
            local,
            port,
            port_listening: listening,
            probe_ready: true,
            local_version: Some("1.37.0".to_owned()),
            service_version: None,
            rpc_port: RPC_PORT_DEFAULT,
            missing: None,
        }
    }

    #[test]
    fn service_running_wins_and_never_spawns_local() {
        let decision = decide(&facts(
            ServicePhase::Running,
            LocalPhase::Idle,
            PortOwner::Ours,
            true,
        ));
        assert_eq!(decision.origin, Origin::Service);
        assert!(
            !decision.start.enabled,
            "服务在跑时绝不能启动面板托管的第二个核心"
        );
        assert!(decision.view.headline.starts_with("服务：运行中"));
        assert_eq!(decision.rpc_port, 6800);
    }

    #[test]
    fn service_running_does_not_claim_a_local_version() {
        // 服务侧版本读不到时**不许**拿本机 aria2c 的版本冒充。
        let decision = decide(&facts(
            ServicePhase::Running,
            LocalPhase::Idle,
            PortOwner::Ours,
            true,
        ));
        assert!(decision.view.headline.contains("版本未知"));
        assert!(!decision.view.headline.contains("1.37.0"));
    }

    #[test]
    fn local_running_is_reported_as_panel_managed() {
        let decision = decide(&facts(
            ServicePhase::NotRunning,
            LocalPhase::Running,
            PortOwner::Ours,
            true,
        ));
        assert_eq!(decision.origin, Origin::Local);
        // 徽章已经写了「运行中」，主文案只补来源与版本，不重复同一个词。
        assert_eq!(decision.view.badge, "运行中");
        assert_eq!(decision.view.headline, "面板托管：v1.37.0");
        assert!(decision.start.enabled);
        assert_eq!(decision.start.label, "停止 aria2");
    }

    #[test]
    fn service_and_local_active_is_loudly_a_conflict() {
        let decision = decide(&facts(
            ServicePhase::Running,
            LocalPhase::Running,
            PortOwner::Ours,
            true,
        ));
        assert_eq!(decision.origin, Origin::Service);
        assert_eq!(decision.view.tone, Tone::Bad);
        assert!(decision.view.headline.contains("冲突"));
        assert!(decision.view.next_step.is_some());
    }

    #[test]
    fn foreign_port_owner_is_never_silently_reported_as_idle() {
        let decision = decide(&facts(
            ServicePhase::NotRunning,
            LocalPhase::Idle,
            PortOwner::Foreign,
            true,
        ));
        assert_eq!(decision.origin, Origin::None);
        assert_eq!(decision.view.tone, Tone::Warn);
        assert_eq!(decision.view.badge, "被占用");
        assert!(decision.view.headline.contains("占用"));
        assert!(
            !decision.start.enabled,
            "端口被外部占着时启动必然失败，按钮必须禁用"
        );
    }

    #[test]
    fn nothing_running_says_so_plainly() {
        let decision = decide(&facts(
            ServicePhase::NotRunning,
            LocalPhase::Idle,
            PortOwner::Free,
            false,
        ));
        assert_eq!(decision.view.headline, "未运行");
        assert!(decision.start.enabled);
        assert_eq!(decision.start.label, "启动核心（本机 aria2c）");
    }

    #[test]
    fn missing_core_blocks_the_start_button() {
        let mut f = facts(
            ServicePhase::NotRunning,
            LocalPhase::Idle,
            PortOwner::Free,
            false,
        );
        f.probe_ready = false;
        f.missing = Some("找不到 aria2c：exe 同目录没有 aria2c.exe".to_owned());
        let decision = decide(&f);
        assert!(!decision.start.enabled);
        assert!(decision.view.headline.contains("核心不可用"));
        assert!(decision
            .view
            .details
            .iter()
            .any(|line| line.contains("找不到 aria2c")));
    }

    #[test]
    fn service_start_is_rejected_while_local_is_active() {
        let f = facts(
            ServicePhase::NotRunning,
            LocalPhase::Running,
            PortOwner::Ours,
            true,
        );
        assert!(can_start_service(&f).is_err());
        let idle = facts(
            ServicePhase::NotRunning,
            LocalPhase::Idle,
            PortOwner::Free,
            false,
        );
        assert!(can_start_service(&idle).is_ok());
    }

    #[test]
    fn service_start_is_rejected_when_a_foreign_program_holds_the_port() {
        let f = facts(
            ServicePhase::NotRunning,
            LocalPhase::Idle,
            PortOwner::Foreign,
            true,
        );
        assert!(can_start_service(&f).is_err());
    }

    #[test]
    fn stopping_still_counts_as_active() {
        assert!(LocalPhase::Stopping.is_active());
        assert!(!LocalPhase::Idle.is_active());
        assert!(!LocalPhase::Failed.is_active());
    }

    #[test]
    fn service_phase_only_running_is_running() {
        for state in [
            ServiceState::NotInstalled,
            ServiceState::Stopped,
            ServiceState::StartPending,
            ServiceState::StopPending,
            ServiceState::Unknown,
        ] {
            assert_eq!(
                service_phase(Some(&snapshot(state))),
                ServicePhase::NotRunning,
                "{state:?}"
            );
        }
        assert_eq!(
            service_phase(Some(&snapshot(ServiceState::Running))),
            ServicePhase::Running
        );
        // 查不到 ≠ 没装：必须是 Unknown，否则界面会谎称「未运行」。
        assert_eq!(service_phase(None), ServicePhase::Unknown);
    }

    fn snapshot(state: ServiceState) -> ServiceSnapshot {
        ServiceSnapshot::State(service_report(state))
    }

    fn service_report(state: ServiceState) -> ariahost_svc::ServiceStatusReport {
        ariahost_svc::ServiceStatusReport {
            installed: state.is_installed(),
            state,
            start_type: None,
            pid: None,
        }
    }

    #[test]
    fn local_phase_folds_every_host_state() {
        assert_eq!(local_phase(&HostState::Idle), LocalPhase::Idle);
        assert_eq!(local_phase(&HostState::Starting), LocalPhase::Starting);
        assert_eq!(local_phase(&HostState::Stopping), LocalPhase::Stopping);
        assert_eq!(
            local_phase(&HostState::Stopped {
                exit_code: Some(0),
                forced: false,
            }),
            LocalPhase::Idle
        );
        assert_eq!(
            local_phase(&HostState::Failed {
                code: None,
                detail: "x".into(),
            }),
            LocalPhase::Failed
        );
    }

    #[test]
    fn origin_labels_are_pinned_for_the_ui() {
        assert_eq!(Origin::Service.label_zh(), "服务");
        assert_eq!(Origin::Local.label_zh(), "面板托管");
        assert_eq!(Origin::None.label_zh(), "未运行");
    }
}
