//! 首次运行「核心未就绪」引导的后台安装（M3-h / task-52）。
//!
//! # 为什么是这个形状
//!
//! **一条铁律**：UI 线程不做网络、解压、起进程。全部在 [`SetupWorker`] 的后台线程里跑，
//! 主线程只 `try_recv` 事件。这与 M2-1 的 worker 是同一套模式。
//!
//! **复用而不是重写**：下载、sha256 校验、解压、切换 active、回滚指针，全在
//! `ariahost_core::Fetcher::fetch` 里；**线路与自动回退也在 core 里**
//! （`download::lines_from` 按 `net.source_order` 排线路，失败线路进
//! `FetchOutcome::warnings`，全失败是 `AH-E6007`）。本模块只做两件事：决定
//! **哪条线路排前面**（M4-2 起这个选择只来自配置，见 [`LineChoice::from_net`]）、
//! 把结果翻译成事件。这里**没有**第二份下载/校验/解压逻辑——那样将来必然与 core 漂移。
//!
//! # 进度与取消的诚实边界
//!
//! `Fetcher::fetch` 是一个整体阻塞调用，core 没有字节回调（`HttpReply` 把整包 body 一次性
//! 收进内存）。所以这里只暴露**可观测步骤**，并且：
//!
//! - 取消**在步骤边界生效**，不是即时。点取消后 UI 必须说「当前步骤完成后停止」，
//!   **不许**显示「已取消」——用户看到"已取消"而进程还在跑，下次就不信这个按钮了。
//! - [`SetupEvent::bytes`] 字段**预留不填**：core 未提供字节进度。填假值会让人以为它已在工作。
//!   将来 core 给出回调时填上即生效，卡片与取消逻辑都不用改。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use ariahost_aria2::{parse_version_line, Aria2Version};
use ariahost_common::config::{self, Aria2Source};
use ariahost_common::paths::Plane;
#[cfg(windows)]
use ariahost_core::CREATE_NO_WINDOW;
use ariahost_core::{CoreError, DownloadLine, Fetcher, HashState, HttpClient, VersionStore};

/// 一条**真实可用**的线路。
///
/// 只有这两种：core 的 [`ariahost_core::lines_from`] 也只认这两个关键字
/// （`cdn` / `github`）。UI **不再自己拼 URL**，也不再出现 `r.alusss.cn` 或
/// 「线路1 / 线路2」这类会与真实请求 host 对不上的说法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// 用户 CDN（`net.cdn_base`，默认 `https://ah.335772.xyz/releases/`）。
    Cdn,
    /// 官方 GitHub release 直连。
    Github,
}

impl LineKind {
    /// 与 core 的 `FetchOutcome::line` / `DownloadLine::name` **同一字面量**。
    ///
    /// 那两个常量是 core 的公开契约；名字对不上就等于把线路说错了。
    fn from_core_name(name: &str) -> Option<Self> {
        if name == ariahost_core::CDN_LINE_NAME {
            Some(Self::Cdn)
        } else if name == ariahost_core::DIRECT_LINE_NAME {
            Some(Self::Github)
        } else {
            None
        }
    }

    /// 用户看到的线路名：**必须能对上真实请求的 host**。
    pub const fn label(self) -> &'static str {
        match self {
            Self::Cdn => "CDN（ah.335772.xyz）",
            Self::Github => "直连 GitHub（github.com）",
        }
    }
}

/// 引导页的线路集合：**来自 `config.net`**，不是 UI 自己编的。
///
/// 为什么不再用「线路1 / 线路2」枚举：`task-72` 之后线路顺序由
/// `net.source_order` 决定，`net.cdn_base` 为空时 CDN 根本不参与。
/// UI 若写死两条线路，就会在「只剩直连」的机器上给出一个并不存在的选项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineChoice {
    /// 首选线路（配置顺序里的第一条可用线路）。
    primary: LineKind,
    /// 备选（配置顺序里的第二条；只有一条可用线路时为 `None`）。
    fallback: Option<LineKind>,
}

impl LineChoice {
    /// 产品默认（`DEFAULT_SOURCE_ORDER = cdn,github` 的顺序）。
    ///
    /// 只作 `CoreGuide::new` 的占位；真正开工前由 [`Self::reconcile`] 按配置重算。
    pub const DEFAULT: Self = Self {
        primary: LineKind::Cdn,
        fallback: Some(LineKind::Github),
    };

    /// 按配置算出可选线路：**唯一来源**是 core 的 [`ariahost_core::lines_from`]。
    ///
    /// 返回 `(线路集合, 配置告警)`。告警属于「配置没被完全照办」（非法关键字 /
    /// 无可用线路而回退），必须进用户可见的日志，不许吞。
    pub fn from_net(net: &ariahost_common::config::NetSection) -> (Self, Vec<String>) {
        let (lines, warnings) = ariahost_core::lines_from(&net.source_order, &net.cdn_base);
        let kinds: Vec<LineKind> = lines
            .iter()
            .filter_map(|line| LineKind::from_core_name(&line.name))
            .collect();
        let plan = match kinds.first() {
            Some(&primary) => Self {
                primary,
                fallback: kinds.get(1).copied(),
            },
            // 理论上不可达（`lines_from` 至少回退直连）；保守退回直连并留痕。
            None => Self {
                primary: LineKind::Github,
                fallback: None,
            },
        };
        (plan, warnings)
    }

    /// 用户看到的（首选线路的）名字。
    pub const fn label(self) -> &'static str {
        self.primary.label()
    }

    /// 「换一条」时的目标；只有一条可用线路时返回自己。
    ///
    /// 那种情况下 `app.rs` 的「换 X 再试」按钮文案会与当前线路相同——它应当在
    /// [`Self::has_alternative`] 为 `false` 时隐藏，那属于 `app.rs` 的一行改动，
    /// 本批（写作用域不含 `app.rs`）不动，已在交付报告里点出。
    pub const fn other(self) -> Self {
        match self.fallback {
            Some(fallback) => Self {
                primary: fallback,
                fallback: Some(self.primary),
            },
            None => self,
        }
    }

    /// 是否真有第二条线路可换（`app.rs` 可据此隐藏「换线路」按钮）。
    ///
    /// `allow(dead_code)` 的原因：目前只有单测在用它——产品侧接线（按钮显隐）要改
    /// `app.rs`，而本批写作用域**不含** `app.rs`，已在交付报告里点出。接线落地后
    /// 删掉这个标注（它是「待接线」的唯一痕迹，别让它悄悄变永久）。
    #[allow(dead_code)]
    pub const fn has_alternative(self) -> bool {
        self.fallback.is_some()
    }

    /// 在有 `available` 这套线路的前提下尽量保留当前选择（看门狗「换线路」不被配置重置）。
    pub fn reconcile(self, available: Self) -> Self {
        let slots = [Some(available.primary), available.fallback];
        if slots.contains(&Some(self.primary)) {
            let fallback = slots
                .into_iter()
                .flatten()
                .find(|kind| *kind != self.primary);
            Self {
                primary: self.primary,
                fallback,
            }
        } else {
            available
        }
    }

    /// 按本选择排出线路**优先顺序**，并带回配置告警。
    ///
    /// 线路对象全部来自 `lines_from`（UI 不拼任何 URL），这里只做重排。
    pub fn lines(
        self,
        net: &ariahost_common::config::NetSection,
    ) -> (Vec<DownloadLine>, Vec<String>) {
        let (mut lines, warnings) = ariahost_core::lines_from(&net.source_order, &net.cdn_base);
        let rank = |name: &str| -> u8 {
            match LineKind::from_core_name(name) {
                Some(kind) if kind == self.primary => 0,
                Some(kind) if Some(kind) == self.fallback => 1,
                _ => 2,
            }
        };
        lines.sort_by_key(|line| rank(&line.name));
        (lines, warnings)
    }

    /// 把「实际命中的线路名」翻译成一句人话，并显式说出是否发生了回退。
    ///
    /// `line` 是 [`ariahost_core::FetchOutcome::line`]（core 报的命中线路名）。
    pub fn describe_hit(self, line: &str, warnings: &[String]) -> String {
        let failed_first = warnings
            .iter()
            .any(|w| w.contains("失败") || w.contains("改用"));
        let hit = LineKind::from_core_name(line)
            .map_or_else(|| line.to_owned(), |kind| kind.label().to_owned());
        if failed_first {
            format!("{} 失败，已自动改用「{hit}」重试成功", self.primary.label())
        } else {
            format!("由「{hit}」提供")
        }
    }
}

/// 后台安装的可观测步骤。
///
/// 只有三步：core 把「下载/校验/解压/切换」压在 `fetch` 一个调用里，
/// 拆细就是**假装知道**它内部在哪一步——宁可步骤少也不许编。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// 读版本目录（这一步在网络上也最慢：GitHub API）。
    Checking,
    /// 下载 + 校验 + 解压 + 切换（core 一个调用，UI 观察不到内部）。
    Fetching,
    /// 装好后真跑一次 `--version`，证明它可执行（不是「文件存在」就算过）。
    HealthCheck,
    /// 指定本地 aria2c：校验可执行 + 写配置。
    Validating,
}

impl Step {
    /// 步骤的中文说法（卡片的进度行）。
    pub const fn label(self) -> &'static str {
        match self {
            Self::Checking => "正在读取核心版本目录",
            Self::Fetching => "正在下载并校验核心（下载、校验、解压、切换）",
            Self::HealthCheck => "正在验证核心可执行",
            Self::Validating => "正在校验你指定的 aria2c",
        }
    }
}

/// 后台线程发给 UI 的事件。
#[derive(Debug, Clone)]
pub enum SetupEvent {
    /// 进入某个步骤。UI 用它的到达时间驱动看门狗。
    Step(Step),
    /// 在线安装成功。
    Online {
        /// 版本号文本（例如 `1.37.0`）。
        version: String,
        /// 实际命中的线路名（来自 core）。
        line: String,
        /// 命中前的失败线路说明 + 未校验哈希告警（**必须展示**，不许吞）。
        warnings: Vec<String>,
        /// 装好的 exe。
        exe: PathBuf,
        /// 哈希状态的人话（`None` = 没法校验时必须如实说）。
        hash_note: String,
        /// 预留：`(已下载, 总计)` 字节。**core 未提供字节进度，恒为 `None`。**
        bytes: Option<(u64, u64)>,
    },
    /// 指定本地 aria2c 成功（配置已写盘）。
    External { version: String, path: PathBuf },
    /// 失败：`message` 用 core 的 `Display`（自带码+中文），`hint` 是**我们的**动作建议。
    Failed {
        step: Step,
        message: String,
        hint: String,
        /// 建议的下一个动作；看门狗与失败卡共用。
        retry_with_other_line: bool,
    },
    /// 取消在步骤边界生效。
    Cancelled { step: Step },
}

/// 后台安装线程。
pub struct SetupWorker {
    rx: Receiver<SetupEvent>,
    cancel: Arc<AtomicBool>,
    done: bool,
}

impl SetupWorker {
    /// 在线安装（按配置的线路顺序，首选在前、备选随后）。
    pub fn online(plan: LineChoice) -> Self {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        thread::Builder::new()
            .name("ariahost-core-setup".to_owned())
            .spawn(move || run_online(plan, &tx, &flag))
            .ok();
        Self {
            rx,
            cancel,
            done: false,
        }
    }

    /// 指定本地 `aria2c.exe`：校验后写 `aria2.source = external`。
    pub fn external(path: PathBuf) -> Self {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        thread::Builder::new()
            .name("ariahost-core-external".to_owned())
            .spawn(move || run_external(&path, &tx, &flag))
            .ok();
        Self {
            rx,
            cancel,
            done: false,
        }
    }

    /// 请求取消：**只置标志**，真正停下要等当前步骤结束（core 的 fetch 不可中断）。
    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// 非阻塞取事件；线程结束后返回 `None`（`done` 置位）。
    pub fn try_recv(&mut self) -> Option<SetupEvent> {
        match self.rx.try_recv() {
            Ok(event) => Some(event),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.done = true;
                None
            }
        }
    }

    /// 线程侧已收工（所有事件都取完了）。
    pub const fn is_done(&self) -> bool {
        self.done
    }
}

// ---------------------------------------------------------------------------
// 线路 → 事件
// ---------------------------------------------------------------------------

/// 版本库目录（用户平面：`%LOCALAPPDATA%\AriaHost\cache\aria2\versions`）。
pub fn user_store() -> Option<VersionStore> {
    ariahost_common::paths::versions_dir(Plane::User).map(VersionStore::new)
}

fn run_online(plan: LineChoice, tx: &mpsc::Sender<SetupEvent>, cancel: &AtomicBool) {
    let Some(store) = user_store() else {
        let _ = tx.send(SetupEvent::Failed {
            step: Step::Checking,
            message: "找不到版本库目录（%LOCALAPPDATA% 不可用）".to_owned(),
            hint: "先确认当前用户目录可写，再重试".to_owned(),
            retry_with_other_line: false,
        });
        return;
    };

    if cancel.load(Ordering::SeqCst) {
        let _ = tx.send(SetupEvent::Cancelled {
            step: Step::Checking,
        });
        return;
    }
    let _ = tx.send(SetupEvent::Step(Step::Checking));

    // 线路与回退**只**来自 `config.net`（经 core 的 `lines_from`），UI 不拼 URL。
    // 读配置失败不该挡住装核心：退回产品默认线路，但把这件事如实记进告警。
    let mut config_warnings = Vec::new();
    let net = match config::load() {
        Ok(config) => config.net,
        Err(error) => {
            config_warnings.push(format!("读配置失败，线路退回产品默认：{error}"));
            ariahost_common::config::NetSection::default()
        }
    };
    // 配置侧告警（非法关键字 / 无可用线路而回退）与下载阶段的线路失败一样必须可见。
    let (lines, line_warnings) = plan.lines(&net);
    config_warnings.extend(line_warnings);
    let fetcher = Fetcher::with_parts(HttpClient::new(), lines);

    // Checking 与 Fetching 之间没有可观测边界：core 的 fetch 内部才拉版本目录。
    // 所以这里**连着发**，让 UI 立刻显示「下载并校验」——不许假装中间有进度。
    if cancel.load(Ordering::SeqCst) {
        let _ = tx.send(SetupEvent::Cancelled {
            step: Step::Checking,
        });
        return;
    }
    let _ = tx.send(SetupEvent::Step(Step::Fetching));

    let outcome = match fetcher.fetch(&store, None) {
        Ok(outcome) => outcome,
        Err(error) => {
            // 取消请求撞上真实失败时，如实报失败：用户点了取消，但这次是真出错。
            let _ = tx.send(SetupEvent::Failed {
                step: Step::Fetching,
                message: error.to_string(),
                hint: with_config_warnings(hint_for(&error), &config_warnings),
                retry_with_other_line: matches!(
                    error,
                    CoreError::NetworkUnreachable { .. }
                        | CoreError::AllLinesFailed { .. }
                        | CoreError::RateLimited { .. }
                        | CoreError::HttpStatus { .. }
                ),
            });
            return;
        }
    };

    if cancel.load(Ordering::SeqCst) {
        // 已装好；取消晚了一步。**如实说装成了**，不要谎称取消成功。
        let _ = tx.send(SetupEvent::Online {
            version: outcome.version.to_string(),
            line: outcome.line.clone(),
            warnings: merged_warnings(&outcome.warnings, &config_warnings),
            exe: outcome.exe_path.clone(),
            hash_note: hash_note(&outcome.hash),
            bytes: None,
        });
        return;
    }

    let _ = tx.send(SetupEvent::Step(Step::HealthCheck));
    if let Err(detail) = probe_exe(&outcome.exe_path) {
        let _ = tx.send(SetupEvent::Failed {
            step: Step::HealthCheck,
            message: format!("装好了但跑不起来：{detail}"),
            hint: "可能是杀毒软件拦了；把 aria2c.exe 加白名单后重试".to_owned(),
            retry_with_other_line: false,
        });
        return;
    }

    let _ = tx.send(SetupEvent::Online {
        version: outcome.version.to_string(),
        line: outcome.line.clone(),
        warnings: merged_warnings(&outcome.warnings, &config_warnings),
        exe: outcome.exe_path.clone(),
        hash_note: hash_note(&outcome.hash),
        bytes: None,
    });
}

fn run_external(path: &Path, tx: &mpsc::Sender<SetupEvent>, cancel: &AtomicBool) {
    if cancel.load(Ordering::SeqCst) {
        let _ = tx.send(SetupEvent::Cancelled {
            step: Step::Validating,
        });
        return;
    }
    let _ = tx.send(SetupEvent::Step(Step::Validating));

    let version = match probe_external(path) {
        Ok(version) => version,
        Err(detail) => {
            let _ = tx.send(SetupEvent::Failed {
                step: Step::Validating,
                message: detail,
                hint: "填 aria2c.exe 的完整路径（不是文件夹、不是 zip）；可以先在命令行跑 `aria2c --version` 确认它能用"
                    .to_owned(),
                retry_with_other_line: false,
            });
            return;
        }
    };

    if let Err(detail) = switch_to_external(path) {
        let _ = tx.send(SetupEvent::Failed {
            step: Step::Validating,
            message: detail,
            hint: "确认 %APPDATA%\\AriaHost\\config.toml 可写后重试".to_owned(),
            retry_with_other_line: false,
        });
        return;
    }

    let _ = tx.send(SetupEvent::External {
        version: version.to_string(),
        path: path.to_path_buf(),
    });
}

// ---------------------------------------------------------------------------
// 探针 / 配置
// ---------------------------------------------------------------------------

/// 哈希状态的人话。**`Unverified` 不许说成「已校验」**。
pub fn hash_note(hash: &HashState) -> String {
    match hash {
        HashState::Verified { sha256 } => {
            let short: String = sha256.chars().take(12).collect();
            format!("sha256 已校验（{short}…）")
        }
        HashState::Unverified { reason } => format!("未能校验哈希：{reason}"),
    }
}

/// 失败后的**动作建议**（错误码与中文说明用 core 的 `Display`，这里只说下一步）。
pub fn hint_for(error: &CoreError) -> String {
    match error {
        CoreError::NetworkUnreachable { .. } | CoreError::AllLinesFailed { .. } => {
            "换另一条线路再试；如果你有现成的 aria2c，也可以直接指定它的路径".to_owned()
        }
        CoreError::RateLimited { .. } => "GitHub 限流了，等几分钟再试；或换另一条线路".to_owned(),
        CoreError::HttpStatus { .. } => "对端返回了错误状态，稍后重试或换线路".to_owned(),
        CoreError::PackageInvalid { .. } => {
            "下载到的包不完整或被篡改，**不会安装**；重试即可（连续失败请换线路）".to_owned()
        }
        CoreError::ExeInUse { .. } => "先把正在运行的 aria2c 停掉，再重试".to_owned(),
        CoreError::StoreIo { .. } => "检查版本库目录的写权限".to_owned(),
        _ => "稍后重试；如果一直不行，指定本地 aria2c 路径".to_owned(),
    }
}

/// 成功时把**配置侧告警**排在下载侧告警前面（配置是「一开始就不对」，先看到它）。
fn merged_warnings(download: &[String], config: &[String]) -> Vec<String> {
    let mut out = config.to_vec();
    out.extend(download.iter().cloned());
    out
}

/// 失败时没有 `warnings` 通道，配置告警只能并进这一行——**不许吞**。
fn with_config_warnings(hint: String, config: &[String]) -> String {
    if config.is_empty() {
        hint
    } else {
        format!("{hint}（另有配置告警：{}）", config.join("；"))
    }
}

/// 跑一次 `<exe> --version`，返回原始第一行（探针共用）。
///
/// 解析交给 `ariahost_aria2::parse_version_line`（`aria2 version X.Y.Z`），**不在这里重写**。
pub fn probe_version_line(path: &Path) -> Result<String, String> {
    if !path.exists() {
        return Err(format!("路径不存在：{}", path.display()));
    }
    if !path.is_file() {
        return Err(format!("不是文件：{}", path.display()));
    }
    let mut command = Command::new(path);
    command.arg("--version");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let output = command
        .output()
        .map_err(|error| format!("跑不起来（{}）：{error}", path.display()))?;
    if !output.status.success() {
        return Err(format!(
            "`--version` 退出码 {}（不是可用的 aria2c？）",
            output.status.code().unwrap_or(-1)
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .ok_or_else(|| "`--version` 没有任何输出".to_owned())?;
    Ok(line.to_owned())
}

/// 校验一个任意盘上的 `aria2c.exe` 可执行并解析出版本。
pub fn probe_external(path: &Path) -> Result<Aria2Version, String> {
    let line = probe_version_line(path)?;
    parse_version_line(&line)
        .map_err(|error| format!("不是 aria2 的版本行（{error}）；首行是：{line}"))
}

/// 内部健康检查：装好的 exe 真能跑（不是「文件在」）。
fn probe_exe(path: &Path) -> Result<Aria2Version, String> {
    probe_external(path)
}

/// 写 `aria2.source = external` + `external_path`（原子写由 `config::save` 负责）。
pub fn switch_to_external(path: &Path) -> Result<(), String> {
    let mut config = config::load().map_err(|error| format!("读取配置失败：{error}"))?;
    config.aria2.source = Aria2Source::External;
    config.aria2.external_path = path.to_string_lossy().into_owned();
    config::save(&config).map_err(|error| format!("保存配置失败：{error}"))
}

/// 判定是否该弹「核心未就绪」引导卡。
///
/// **只认 `AH-E2001`（核心缺失）**：配置读不出来是另一条错误路径，引导卡不许冒充它——
/// 否则用户会对着「核心未就绪」的卡片去修一个其实坏在 TOML 里的配置。
pub fn guide_needed(probe: &crate::core_host::CoreProbe) -> bool {
    probe.code == Some(ariahost_common::ErrorCode::Aria2NotFound)
}

/// 看门狗默认阈值：这么久没有任何新事件就认为「网络慢或 GitHub 被拦」。
///
/// 真机实测整条链 6.6 秒（5.6 MB 包），45 秒足够宽松，又不会让人干等到以为卡死。
pub const WATCHDOG: Duration = Duration::from_secs(45);

/// 判定看门狗是否该接管。
pub fn watchdog_expired(last_event_at: Instant, now: Instant) -> bool {
    now.duration_since(last_event_at) >= WATCHDOG
}

// ---------------------------------------------------------------------------
// 引导卡的状态机（不碰 egui：渲染在 app.rs，这里只管「发生了什么」）
// ---------------------------------------------------------------------------

/// 一轮结束后的落地结论（卡片上那行结论文案）。
#[derive(Debug, Clone)]
pub enum GuideNotice {
    /// 在线装好了。
    Online {
        /// 版本号。
        version: String,
        /// 「由哪条线路提供」或「线路X 失败，已自动改用线路Y」。
        line_note: String,
        /// 哈希状态的人话。
        hash_note: String,
        /// 必须展示的告警（失败线路 / 未校验哈希）。
        warnings: Vec<String>,
    },
    /// 指定本地 aria2c 成功，配置已写盘。
    External {
        /// 版本号。
        version: String,
        /// 用户给的路径。
        path: PathBuf,
    },
    /// 失败。
    Failed {
        /// core 的 `Display` 文本（自带码 + 中文）。
        message: String,
        /// 我们的动作建议。
        hint: String,
        /// 是否值得换线路再试。
        retry_other: bool,
    },
    /// 取消请求晚了一步：活已经干完了。**如实说**，不许谎称已取消。
    CancelledTooLate {
        /// 当时在哪个步骤。
        step: Step,
    },
}

/// [`CoreGuide::poll`] 的产出：要写进面板日志的行 + 是否该刷新运行时。
#[derive(Debug, Default)]
pub struct GuidePoll {
    /// 面板日志行。
    pub logs: Vec<String>,
    /// 装好/切换成功后重建 host 与 worker（否则「装好了但还是不能下载」）。
    pub refresh_runtime: bool,
}

/// 首次运行「核心未就绪」引导的全部状态。
///
/// 硬约束（Lead 裁定的三条）：
/// 1. 取消**不许假装即时**：`cancel_requested` 置位后文案必须是「当前步骤完成后停止」；
/// 2. 线路与回退必须显式可见（[`LineChoice::describe_hit`]）；
/// 3. 超过 [`WATCHDOG`] 没有新事件就切 [`Self::slow`]，给出「换线路 / 稍后再说」两个出口。
pub struct CoreGuide {
    /// 卡片是否还在界面上（装好/关掉后置 `false`）。
    pub open: bool,
    /// 用户选了「稍后再说」。
    pub dismissed: bool,
    /// 当前选中的线路（初值是产品默认；[`Self::start_online`] 开工前按配置重算）。
    pub line: LineChoice,
    /// 当前步骤（卡片进度行用）。
    pub step: Step,
    /// 是否已请求取消（文案据此变「当前步骤完成后停止」）。
    pub cancel_requested: bool,
    /// 看门狗是否已接管（[`WATCHDOG`] 内没有新事件）。
    pub slow: bool,
    /// 「指定已有 aria2c」的输入框内容。
    pub external_path: String,
    /// 上一轮的结论。
    pub notice: Option<GuideNotice>,
    /// 核心是否已可用（装好后置 `true`，让「开始」按钮从禁用变可用）。
    pub installed: bool,
    last_event_at: Instant,
    worker: Option<SetupWorker>,
}

impl CoreGuide {
    /// 按启动时的探测结果初始化：`missing = true` 时卡片直接展开。
    pub fn new(missing: bool) -> Self {
        Self {
            open: missing,
            dismissed: false,
            line: LineChoice::DEFAULT,
            step: Step::Checking,
            cancel_requested: false,
            slow: false,
            external_path: String::new(),
            notice: None,
            installed: false,
            last_event_at: Instant::now(),
            worker: None,
        }
    }

    /// 是否有后台安装在进行。
    pub fn busy(&self) -> bool {
        self.worker.is_some()
    }

    /// 后台装完之后是否还需要刷新运行时（[`GuidePoll::refresh_runtime`] 的持久化侧）。
    pub fn core_usable(&self) -> bool {
        self.installed
    }

    /// 「稍后再说」：收掉卡片但保留设置页入口；**不谎称已取消**。
    pub fn dismiss(&mut self) {
        self.dismissed = true;
        self.open = false;
        if let Some(worker) = &self.worker {
            worker.request_cancel();
            self.cancel_requested = true;
        }
    }

    /// 设置页的「重新配置核心」入口把卡片叫回来。
    pub fn reopen(&mut self) {
        self.dismissed = false;
        self.open = true;
        self.notice = None;
    }

    /// 开始在线下载。已在跑就返回 `false`（防连点重复开线程）。
    pub fn start_online(&mut self) -> bool {
        if self.worker.is_some() {
            return false;
        }
        self.notice = None;
        self.cancel_requested = false;
        self.slow = false;
        self.step = Step::Checking;
        self.last_event_at = Instant::now();
        self.open = true;
        self.dismissed = false;
        // 开工前按配置重算「有哪些线路可选」：改了 `net.source_order` 不必重启面板。
        // 这里的告警不丢——后台线程会用同一份配置再算一次线路，并把它带进事件流。
        let available = match config::load() {
            Ok(config) => LineChoice::from_net(&config.net).0,
            // 读不到配置不挡住开工：产品默认（CDN → GitHub）与 `NetSection::default()` 一致，
            // 真实的读盘失败由后台线程记进告警。
            Err(_) => LineChoice::DEFAULT,
        };
        self.line = self.line.reconcile(available);
        self.worker = Some(SetupWorker::online(self.line));
        true
    }

    /// 用给定路径开始校验本地 aria2c。
    pub fn start_external(&mut self, path: PathBuf) -> bool {
        if self.worker.is_some() || path.as_os_str().is_empty() {
            return false;
        }
        self.notice = None;
        self.cancel_requested = false;
        self.slow = false;
        self.step = Step::Validating;
        self.last_event_at = Instant::now();
        self.open = true;
        self.dismissed = false;
        self.worker = Some(SetupWorker::external(path));
        true
    }

    /// 请求取消：只置标志，**真正停下要等当前步骤结束**。
    pub fn request_cancel(&mut self) {
        if let Some(worker) = &self.worker {
            worker.request_cancel();
            self.cancel_requested = true;
        }
    }

    /// 看门狗兜底的「换线路」：作废本轮，换另一条线重开。
    pub fn switch_line(&mut self) -> bool {
        if let Some(worker) = &self.worker {
            worker.request_cancel();
        }
        self.worker = None;
        self.line = self.line.other();
        self.slow = false;
        self.cancel_requested = false;
        self.start_online()
    }

    /// 更新看门狗状态（每帧调用，纯内存判断）。
    pub fn tick(&mut self, now: Instant) {
        if self.worker.is_some() {
            self.slow = watchdog_expired(self.last_event_at, now);
        }
    }

    /// 收事件（非阻塞）。返回要写日志的行与「是否该刷新运行时」。
    pub fn poll(&mut self, now: Instant) -> GuidePoll {
        let mut out = GuidePoll::default();
        let Some(worker) = &mut self.worker else {
            return out;
        };
        loop {
            match worker.try_recv() {
                Some(SetupEvent::Step(step)) => {
                    self.step = step;
                    self.last_event_at = now;
                    self.slow = false;
                    out.logs.push(format!("核心引导：{}", step.label()));
                }
                Some(SetupEvent::Online {
                    version,
                    line,
                    warnings,
                    exe,
                    hash_note,
                    bytes,
                }) => {
                    let note = self.line.describe_hit(&line, &warnings);
                    out.logs
                        .push(format!("核心就绪：aria2 {version}（{note}；{hash_note}）"));
                    out.logs.push(format!("核心可执行文件：{}", exe.display()));
                    for warning in &warnings {
                        out.logs.push(format!("核心安装告警：{warning}"));
                    }
                    if let Some((done, total)) = bytes {
                        // core 目前不提供字节进度；真给了回调才走这里。
                        out.logs.push(format!("核心安装进度：{done}/{total} 字节"));
                    }
                    self.notice = Some(GuideNotice::Online {
                        version,
                        line_note: note,
                        hash_note,
                        warnings,
                    });
                    self.installed = true;
                    self.open = false;
                    self.cancel_requested = false;
                    self.slow = false;
                    out.refresh_runtime = true;
                    break;
                }
                Some(SetupEvent::External { version, path }) => {
                    out.logs.push(format!(
                        "核心已切换为用户指定的 aria2 {version}：{}",
                        path.display()
                    ));
                    self.notice = Some(GuideNotice::External { version, path });
                    self.installed = true;
                    self.open = false;
                    self.cancel_requested = false;
                    out.refresh_runtime = true;
                    break;
                }
                Some(SetupEvent::Failed {
                    step,
                    message,
                    hint,
                    retry_with_other_line,
                }) => {
                    out.logs
                        .push(format!("核心引导失败（{}）：{message}", step.label()));
                    self.notice = Some(GuideNotice::Failed {
                        message,
                        hint,
                        retry_other: retry_with_other_line,
                    });
                    self.slow = false;
                    break;
                }
                Some(SetupEvent::Cancelled { step }) => {
                    out.logs
                        .push(format!("核心引导已取消（停在：{}）", step.label()));
                    self.notice = Some(GuideNotice::CancelledTooLate { step });
                    self.cancel_requested = false;
                    self.slow = false;
                    break;
                }
                None => break,
            }
        }
        // 线程收工且没给出结论：不能就这么转圈下去。
        if self.worker.as_ref().is_some_and(SetupWorker::is_done) {
            if self.notice.is_none() {
                self.notice = Some(GuideNotice::Failed {
                    message: "后台安装线程没有给出结果就结束了".to_owned(),
                    hint: "重试一次；反复出现请把日志发给我们".to_owned(),
                    retry_other: false,
                });
            }
            self.worker = None;
            self.slow = false;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 产品默认（`cdn,github`）：CDN 首选、GitHub 备选，且文案对上真实 host。
    #[test]
    fn plan_from_default_net_is_cdn_then_github() {
        let net = ariahost_common::config::NetSection::default();
        let (plan, warnings) = LineChoice::from_net(&net);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(plan.label(), LineKind::Cdn.label());
        assert!(plan.label().contains("ah.335772.xyz"), "{}", plan.label());
        assert!(plan.has_alternative());
        assert_eq!(plan.other().label(), LineKind::Github.label());
        // 「线路1 / 线路2」这类与真实请求对不上的说法必须绝迹。
        assert!(!plan.label().contains("线路"), "{}", plan.label());
    }

    /// 用户把 `net.source_order` 改成 `github` 就只剩一条线路——UI 不许再给「换线路」。
    #[test]
    fn plan_without_cdn_has_no_alternative() {
        let net = ariahost_common::config::NetSection {
            source_order: "github".to_owned(),
            ..ariahost_common::config::NetSection::default()
        };
        let (plan, warnings) = LineChoice::from_net(&net);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(plan.label(), LineKind::Github.label());
        assert!(!plan.has_alternative());
        assert_eq!(plan.other(), plan, "没有备选时「换一条」只能返回自己");
    }

    /// `net.cdn_base = ""` 等同禁用 CDN（core 的 `lines_from` 就是这么做的）。
    #[test]
    fn empty_cdn_base_removes_the_cdn_line() {
        let net = ariahost_common::config::NetSection {
            source_order: "cdn,github".to_owned(),
            cdn_base: String::new(),
            ..ariahost_common::config::NetSection::default()
        };
        let (plan, _warnings) = LineChoice::from_net(&net);
        assert_eq!(plan.label(), LineKind::Github.label());
        assert!(!plan.has_alternative());
    }

    /// 非法关键字必须变成**用户可见**的告警，同时线路回退到直连。
    #[test]
    fn invalid_order_keyword_is_reported_not_swallowed() {
        let net = ariahost_common::config::NetSection {
            source_order: "mirror,github".to_owned(),
            ..ariahost_common::config::NetSection::default()
        };
        let (plan, warnings) = LineChoice::from_net(&net);
        assert_eq!(plan.label(), LineKind::Github.label());
        assert!(!warnings.is_empty(), "非法关键字必须留告警");
        assert!(
            warnings.iter().any(|w| w.contains("mirror")),
            "{warnings:?}"
        );
    }

    /// 实际下发的线路对象来自 core（模板里带 CDN base / `{url}`），UI 不拼 URL。
    #[test]
    fn lines_come_from_config_and_put_primary_first() {
        let net = ariahost_common::config::NetSection::default();
        let (plan, _) = LineChoice::from_net(&net);
        let (lines, warnings) = plan.lines(&net);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].name, ariahost_core::CDN_LINE_NAME);
        assert!(
            lines[0].template.contains(&net.cdn_base),
            "{}",
            lines[0].template
        );
        assert_eq!(lines[1].name, ariahost_core::DIRECT_LINE_NAME);
        assert!(lines[1].template.contains("{url}"), "{}", lines[1].template);

        // 选了 GitHub 之后顺序反过来（CDN 仍留着当备选）。
        let (flipped, _) = plan.other().lines(&net);
        assert_eq!(flipped[0].name, ariahost_core::DIRECT_LINE_NAME);
        assert_eq!(flipped[1].name, ariahost_core::CDN_LINE_NAME);
    }

    /// 看门狗「换线路」不能被下一轮配置重算抹掉。
    #[test]
    fn reconcile_keeps_the_user_choice_while_available() {
        let net = ariahost_common::config::NetSection::default();
        let (available, _) = LineChoice::from_net(&net);
        let chosen = available.other();
        let kept = chosen.reconcile(available);
        assert_eq!(kept.label(), LineKind::Github.label());

        // 换到只剩直连的机器上：选择必须被纠正，而不是继续指向不存在的 CDN。
        let single = LineChoice {
            primary: LineKind::Github,
            fallback: None,
        };
        let corrected = chosen.reconcile(single);
        assert_eq!(corrected.label(), LineKind::Github.label());
        assert!(!corrected.has_alternative());
    }

    /// 回退必须在文案里说出来，不能只写「由 X 提供」。
    #[test]
    fn fallback_is_spelled_out() {
        let note = LineChoice::DEFAULT.describe_hit(
            ariahost_core::DIRECT_LINE_NAME,
            &["线路「CDN ah.335772.xyz」失败：连接超时".to_owned()],
        );
        assert!(note.contains("失败"), "{note}");
        assert!(note.contains("已自动改用"), "{note}");
        assert!(note.contains(LineKind::Github.label()), "{note}");

        let clean = LineChoice::DEFAULT.describe_hit(ariahost_core::CDN_LINE_NAME, &[]);
        assert!(!clean.contains("失败"), "{clean}");
        assert!(clean.contains(LineKind::Cdn.label()), "{clean}");
    }

    /// 配置告警不许被下载告警挤掉，也不许在失败路径上消失。
    #[test]
    fn config_warnings_survive_both_paths() {
        let merged = merged_warnings(&["下载告警".to_owned()], &["配置告警".to_owned()]);
        assert_eq!(merged, vec!["配置告警", "下载告警"]);

        let hint = with_config_warnings("重试即可".to_owned(), &[]);
        assert_eq!(hint, "重试即可");

        let hint = with_config_warnings("重试即可".to_owned(), &["配置告警".to_owned()]);
        assert!(hint.contains("重试即可"), "{hint}");
        assert!(hint.contains("配置告警"), "{hint}");
    }

    /// `Unverified` 绝不能说成「已校验」。
    #[test]
    fn unverified_hash_is_reported_honestly() {
        let note = hash_note(&HashState::Unverified {
            reason: "上游没有提供 sha256".to_owned(),
        });
        assert!(!note.contains("已校验"), "{note}");
        assert!(note.contains("未能校验"), "{note}");

        let verified = hash_note(&HashState::Verified {
            sha256: "0123456789abcdef0123".to_owned(),
        });
        assert!(verified.contains("已校验"), "{verified}");
        assert!(verified.contains("0123456789ab"), "{verified}");
    }

    /// core 的错误码不要自己拼：文案里已有码，hint 只补动作。
    #[test]
    fn hint_does_not_duplicate_error_codes() {
        let error = CoreError::NetworkUnreachable {
            detail: "connection reset".to_owned(),
        };
        let hint = hint_for(&error);
        assert!(!hint.contains("AH-E"), "hint 不该自己拼错误码：{hint}");
        assert!(hint.contains("线路"), "{hint}");
    }

    #[test]
    fn watchdog_fires_only_after_budget() {
        let t0 = Instant::now();
        assert!(!watchdog_expired(t0, t0 + Duration::from_secs(1)));
        assert!(watchdog_expired(t0, t0 + WATCHDOG));
    }

    /// 卡片默认只在「核心缺失」时展开；`dismiss` 收掉后可以再叫回来。
    #[test]
    fn guide_opens_on_missing_and_can_be_reopened() {
        let mut guide = CoreGuide::new(true);
        assert!(guide.open, "核心缺失时卡片必须展开");
        assert!(!guide.busy());
        // 没有后台任务时点「稍后再说」：只收卡片，不应该伪造一个「已取消」。
        guide.dismiss();
        assert!(!guide.open);
        assert!(guide.dismissed);
        assert!(!guide.cancel_requested, "没有在跑的任务时不该置取消位");
        assert!(guide.notice.is_none(), "没跑过就不该有结论");
        guide.reopen();
        assert!(guide.open && !guide.dismissed, "设置页入口要能叫回来");
    }

    /// 核心本来就好的机器上，卡片不出现也不该有后台线程。
    #[test]
    fn guide_stays_closed_when_core_is_ready() {
        let guide = CoreGuide::new(false);
        assert!(!guide.open);
        assert!(!guide.busy());
        assert!(!guide.core_usable(), "没装过就还没「可用」");
    }

    /// 没有后台任务时 `poll` 必须什么都不做（不能凭空造事件）。
    #[test]
    fn poll_without_worker_is_a_noop() {
        let mut guide = CoreGuide::new(true);
        let out = guide.poll(Instant::now());
        assert!(out.logs.is_empty());
        assert!(!out.refresh_runtime);
    }

    /// 「换线路」在没有后台任务时也要能立刻开工（看门狗兜底路径）。
    #[test]
    fn switch_line_flips_and_starts() {
        let mut guide = CoreGuide::new(true);
        guide.request_cancel(); // 没任务时是空操作
        assert!(!guide.cancel_requested);
        assert_eq!(guide.line, LineChoice::DEFAULT);
        // 正式开工会起线程；这里只验线路切换本身（不调 switch_line，避免真联网）。
        guide.line = guide.line.other();
        assert_eq!(guide.line, LineChoice::DEFAULT.other());
    }

    #[test]
    fn probe_rejects_missing_and_directory() {
        let missing = PathBuf::from("C:/definitely/not/here/aria2c.exe");
        assert!(probe_external(&missing).is_err());

        let dir = std::env::temp_dir();
        let message = probe_external(&dir).unwrap_err();
        assert!(message.contains("不是文件"), "{message}");
    }

    /// 拿本机 exe 冒充 aria2c：必须报「不是 aria 的版本行」，而不是装上。
    #[test]
    fn probe_rejects_non_aria2_binary() {
        let exe = std::env::current_exe().expect("拿到测试自身路径");
        let result = probe_external(&exe);
        assert!(result.is_err(), "不该把非 aria2 程序当成核心");
    }
}
