//! 缓存与垃圾治理（M2-6）：**只扫描、只报告**；删除只走白名单枚举。
//!
//! 需求第 6 条后半 + 架构 §6「清理策略」：先检测 → 再提示 → 用户确认后清理。
//! 本模块**不认识任何 UI、不弹任何窗、不自动删**，只产出 [`CleanReport`]。
//!
//! ## 为什么不给「删任意路径」的入口
//!
//! [`clean`] 只接受 [`CleanItem`]，而 `CleanItem` **没有公共构造函数**——它只能由
//! [`scan`] 产生。所以「删 `config.toml`」「删 `session\aria2.session`」「删 `secret.bin`」
//! 这类误用**在类型层面写不出来**，不依赖调用方自觉，也不会被将来新写的代码路径绕过。
//!
//! 删除入口另有一条运行期复核：服务平面（`%ProgramData%`）的条目一律拒绝删除
//! （要提权、且服务可能正在用），见 [`CleanItem::deletable`] 与 [`clean_with`]。
//!
//! ## 五类可清目标（白名单，无第六类）
//!
//! | 枚举 | 是什么 | 判定 |
//! |---|---|---|
//! | `OrphanControlFile` | 下载目录里的孤儿 `*.aria2` | 见该变体文档的判定表（**最容易写错的一类**） |
//! | `RotatedLog` | 超出 `clean.log_keep_files` 的 `*.log.N` | 轮转序号 >= 保留份数（份数含当前文件） |
//! | `AbandonedVersion` | 版本库里既非 `active` 也非 `previous` 的版本目录 | **结构性判定**，不使用天数阈值 |
//! | `StaleStaging` | 安装残渣 `.staging-<ver>-<pid>-<seq>` | mtime 超 1 小时 **且** 目录名里的 pid 不存活 |
//! | `TmpFile` | `tmp\` 下 mtime 超 24 小时的文件 | [`TMP_MAX_AGE_HOURS`]（**本版不可配**） |
//!
//! ## 永远不删（硬边界，写进类型而不是注释）
//!
//! 已完成下载的目标文件、活动/排队任务、`config.toml`、`session\*`、`secret.bin`、
//! 激活版核心。前四类根本不在任何扫描根的范围内；`session`/`config`/`secret` 所在目录
//! 我们连 `read_dir` 都不做，「删」在代码里没有落点。
//!
//! ## 两个平面
//!
//! 版本库、日志、tmp 都**按平面分开**（R9-b/R11）：用户平面在 `%LOCALAPPDATA%`/`%APPDATA%`
//! 下可删；服务平面在 `%ProgramData%` 下**只报告不删**（UI 要据此告诉用户「这些需要管理员」）。
//!
//! ## 阈值来源
//!
//! 全部取自 `clean.*`（[`ScanLimits::from_config`]），模块里**不另写一套默认值**。
//! 只有两个常量不在 52 键 schema 里（本版故意不可配）：[`TMP_MAX_AGE_HOURS`]、
//! [`STAGING_MIN_AGE_SECS`]；打扰分级阈值 [SILENT_MAX_BYTES]/[BUBBLE_MAX_BYTES] 同理。

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use ariahost_aria2::parse_version_number;
use ariahost_common::config::{expand_user_path, CleanSection, Config};
use ariahost_common::logfile::rotated_index;
use ariahost_common::paths::{self, Plane};

use crate::store::{ActivePointer, ACTIVE_FILE};
use crate::CORE_EXE;

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// `tmp\` 下文件的清理门槛（小时）。
///
/// **本版不可配**：52 键 schema 里没有 `clean.tmp_hours`（见架构 §4.11.1 第 43–49 项）。
/// 已知后续项：与 `web.enabled`、`clean.abandoned_version_days` 一起作一次 schema 变更。
pub const TMP_MAX_AGE_HOURS: u64 = 24;

/// `.staging-*` 的最短保留时长（秒）。
///
/// 1 小时是为了避开「另一个实例正在安装」的窗口：安装本身是秒级，1 小时足够宽松。
/// **本版不可配**（同上）。
pub const STAGING_MIN_AGE_SECS: u64 = 3600;

/// 打扰分级下界：小于它按「静默」（invisible，不打扰）。
pub const SILENT_MAX_BYTES: u64 = 50 * 1024 * 1024;

/// 打扰分级上界：达到它按「提示卡」；中间是「托盘气泡一次/日」。
pub const BUBBLE_MAX_BYTES: u64 = 500 * 1024 * 1024;

/// 单次扫描最多访问的目录项数（防止用户把 `dl.dir` 指到盘根时卡死 UI）。
///
/// 定这个数的依据：本地盘实测约 1.8 µs/条目（合成 2900 条 5.3 ms），
/// 10 万条约 180 ms，与「单次扫描 < 200 ms」的目标对齐。
/// 撞到它会置 [`CleanReport::budget_exhausted`]，UI 据此提示「结果不完整」。
pub const SCAN_ENTRY_BUDGET: usize = 100_000;

/// 报告里最多保留几条「扫不动」的警告（防刷屏）。
const MAX_WARNINGS: usize = 8;

/// 安装暂存目录前缀（与 [`crate::store`] 的 `STAGING_PREFIX` 必须一致）。
const STAGING_PREFIX: &str = ".staging-";

/// 孤儿控制文件后缀。
const ORPHAN_SUFFIX: &str = ".aria2";

const BYTES_PER_GIB: u64 = 1024 * 1024 * 1024;
const SECONDS_PER_DAY: u64 = 86_400;

// ---------------------------------------------------------------------------
// 目标类型（白名单）
// ---------------------------------------------------------------------------

/// 允许清理的对象类型（白名单）。
///
/// 枚举之外没有第六种目标；`clean` 只处理这些。新增一类必须同时改这里、
/// [`CleanReport`] 的汇总与 UI 文案，避免「顺手加了条删除路径」。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CleanTarget {
    /// 孤儿 `*.aria2` 控制文件。
    ///
    /// 判定表（严格按此实现，宁可不删）：
    ///
    /// | # | 目标文件（去掉 `.aria2`） | aria2 状态 | 被活动任务引用 | 判定 |
    /// |---|---|---|---|---|
    /// | 1 | 存在 | 任意 | 任意 | **不删**——目标仍在，可能是可续传的残留（删了要重下几十 G） |
    /// | 2 | 不存在 | `Running`/`Unknown` | 任意 | **不删**——拿不到完整任务列表，可能是排队任务 |
    /// | 3 | 不存在 | `Stopped` | 是 | **不删**——有任务引用 |
    /// | 4 | 不存在 | `Stopped` | 否 | mtime 超 `clean.aria2_idle_days` → 可删；未到期不删 |
    ///
    /// 只有第 4 行会产出条目。`Unknown` 与 `Running` 在这里等价，因为两者都「不能确认」。
    OrphanControlFile,
    /// 超出 `clean.log_keep_files` 份数的轮转日志（`*.log.N`）。
    ///
    /// `log_keep_files` 是**总文件数**（当前 `*.log` + 轮转文件），所以保留 `.log.1..=keep-1`，
    /// 序号 >= keep 的都是超出份数。这样 `clean.log_max_size_mb × clean.log_keep_files`
    /// 就是该类日志的磁盘上限（默认 10 MB × 5 = 50 MB）——口径必须唯一，
    /// 否则这个乘法对不上。
    RotatedLog,
    /// 版本库里既非 `active` 也非 `previous` 的版本目录。
    ///
    /// **结构性判定**：只保留 `active.json` 的 `active`（当前）与 `previous`（单代回滚）。
    /// 不使用天数阈值（`current` 永不删，哪怕它很大）。额外两条：
    /// - 激活指针缺失或损坏 → 一切都不删（保守）；
    /// - 该目录里的 `aria2c.exe` 写打开被拒（正在运行）→ 跳过并标「使用中」。
    AbandonedVersion,
    /// 安装残留：`.staging-<version>-<pid>-<seq>/`。
    ///
    /// 删除需同时满足：mtime 超过 [`STAGING_MIN_AGE_SECS`]（1 小时）
    /// **且** 目录名里的 pid 不在存活进程里（解析不出 pid 时只看年龄）。
    StaleStaging,
    /// `tmp\` 下过期的临时文件（[`TMP_MAX_AGE_HOURS`]，本版不可配）。
    TmpFile,
}

impl CleanTarget {
    /// 全部目标（UI 遍历用）。
    pub const ALL: [Self; 5] = [
        Self::OrphanControlFile,
        Self::RotatedLog,
        Self::AbandonedVersion,
        Self::StaleStaging,
        Self::TmpFile,
    ];

    /// 中文短标签（UI 分类标题用）。
    pub fn label(self) -> &'static str {
        match self {
            Self::OrphanControlFile => "孤儿下载控制文件（.aria2）",
            Self::RotatedLog => "旧日志",
            Self::AbandonedVersion => "废弃的 aria2 版本",
            Self::StaleStaging => "安装残留",
            Self::TmpFile => "过期临时文件",
        }
    }
}

/// 打扰分级（本批只产出枚举，不做持久化 dismiss——那是 UI 层 M2-4 的事）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Urgency {
    /// 可释放 < [`SILENT_MAX_BYTES`]：静默，不打扰。
    Silent,
    /// [`SILENT_MAX_BYTES`] ≤ 可释放 < [`BUBBLE_MAX_BYTES`]：托盘气泡（UI 侧一次/日）。
    Bubble,
    /// 可释放 ≥ [`BUBBLE_MAX_BYTES`]，**或**下载盘剩余低于 `clean.min_free_space_gb`：提示卡。
    Card,
}

impl Urgency {
    /// 按总字节数粗分级（不含低盘判定；低盘由 [`CleanReport::urgency`] 叠加）。
    pub fn of_bytes(bytes: u64) -> Self {
        if bytes >= BUBBLE_MAX_BYTES {
            Self::Card
        } else if bytes >= SILENT_MAX_BYTES {
            Self::Bubble
        } else {
            Self::Silent
        }
    }

    /// 中文说明（UI 文案）。
    pub fn label(self) -> &'static str {
        match self {
            Self::Silent => "静默（可释放空间很小，不打扰）",
            Self::Bubble => "托盘提示（可释放空间中等）",
            Self::Card => "弹出提示卡（可释放空间大或下载盘空间紧张）",
        }
    }
}

/// aria2 运行状态。**由调用方告知**（面板/服务自己知道），引擎不主动探测。
///
/// 为什么不让引擎探测：探测要么连 RPC（端口无监听时会挂满超时，见 M2-5 复盘），
/// 要么枚举全进程（需要 `windows-sys` 的 ProcessStatus feature，本批不许改 Cargo.toml）。
/// 而孤儿控制文件的判定**只需要区分「确定停了」与「其他」**，所以由调用方注入最可靠。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Aria2State {
    /// 确定在跑。
    Running,
    /// 确定没跑（只有这个状态允许删孤儿 `.aria2`）。
    Stopped,
    /// 不确定（默认）——一律按「不能确认任务列表」处理，保守不删。
    Unknown,
}

// ---------------------------------------------------------------------------
// 宿主环境探测
// ---------------------------------------------------------------------------

/// 与宿主环境的交互点：进程存活、文件占用、剩余磁盘。
///
/// 抽成 trait 是为了让判定逻辑**能在 Linux 上跑真单测**（注入假实现），
/// 同时把仅 Windows 需要的系统调用集中到一个很小的实现里。
pub trait LiveState {
    /// 进程是否还活着。`pid == 0` 一律当作不存活。
    fn pid_alive(&self, pid: u32) -> bool;

    /// 该文件是否正被某个进程占用（写打开被拒）。
    ///
    /// 存在的文件返回 `true` 表示「删不掉/可能正在用」；文件不存在返回 `false`。
    fn exe_in_use(&self, path: &Path) -> bool;

    /// 该目录所在卷的剩余字节数；拿不到返回 `None`。
    ///
    /// 本版恒返回 `None`：`GetDiskFreeSpaceExW` 需要 `windows-sys` 的
    /// `Win32_Storage_FileSystem` feature，而本批不允许改 `Cargo.toml`。
    /// 结构已留好，接上后 [`CleanReport::low_disk`] 立刻生效。
    fn free_bytes(&self, dir: &Path) -> Option<u64> {
        let _ = dir;
        None
    }
}

#[cfg(windows)]
mod platform {
    use std::path::Path;

    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    use super::LiveState;

    /// 真实宿主实现（Windows）。
    pub struct RealLiveState;

    impl LiveState for RealLiveState {
        fn pid_alive(&self, pid: u32) -> bool {
            if pid == 0 {
                return false;
            }
            // SAFETY: 只做查询：`PROCESS_QUERY_LIMITED_INFORMATION` 不改进程状态，
            // 句柄在同一次调用内关闭，不存在泄漏。
            unsafe {
                let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
                if handle.is_null() {
                    // 打不开 = 进程不存在（或权限不够）：两种情况都按「不存活」处理，
                    // 但调用方只在「年龄也到 1 小时」时才会删，风险已收敛。
                    return false;
                }
                let mut code: u32 = 0;
                let queried = GetExitCodeProcess(handle, &mut code);
                CloseHandle(handle);
                if queried == 0 {
                    // 取不到退出码：保守当作还活着，不删。
                    return true;
                }
                code == STILL_ACTIVE as u32
            }
        }

        fn exe_in_use(&self, path: &Path) -> bool {
            if !path.is_file() {
                return false;
            }
            crate::store::probe_exe_writable(path).is_err()
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use std::path::Path;

    use super::LiveState;

    /// 真实宿主实现（非 Windows，仅单测/CI 用）。
    pub struct RealLiveState;

    impl LiveState for RealLiveState {
        fn pid_alive(&self, pid: u32) -> bool {
            if pid == 0 {
                return false;
            }
            // Linux：/proc/<pid> 存在即存活（单测里直接用 std::process::id()）。
            Path::new("/proc").join(pid.to_string()).exists()
        }

        fn exe_in_use(&self, path: &Path) -> bool {
            if !path.is_file() {
                return false;
            }
            // 非 Windows 上「写打开运行中的可执行文件」会返回 ETXTBSY，同样可用。
            crate::store::probe_exe_writable(path).is_err()
        }
    }
}

pub use platform::RealLiveState;

// ---------------------------------------------------------------------------
// 扫描阈值与上下文
// ---------------------------------------------------------------------------

/// 从 `clean.*` 映射来的阈值。**模块内不写第二套默认值**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanLimits {
    /// `clean.scan_on_start`：启动后延迟扫描（由调用方调度，本模块只透传）。
    pub scan_on_start: bool,
    /// `clean.scan_interval_hours`：定时扫描周期（同上）。
    pub scan_interval_hours: u32,
    /// `clean.aria2_idle_days`：孤儿 `.aria2` 判定天数。
    pub idle_days: u32,
    /// `clean.log_max_size_mb`：单日志轮转线（本模块不判定，只进报告给 UI 参考）。
    pub log_max_size_mb: u32,
    /// `clean.log_keep_files`：每类日志保留的轮转份数。
    pub log_keep_files: u32,
    /// `clean.cache_max_mb`：cache 总量**提示线**（不是删除线，只进报告）。
    pub cache_max_mb: u32,
    /// `clean.min_free_space_gb`：下载盘剩余空间告警线。
    pub min_free_space_gb: u32,
    /// `tmp\` 文件的过期门槛。
    ///
    /// **不是配置项**：来自 [`TMP_MAX_AGE_HOURS`] 常量（52 键 schema 里没有 `clean.tmp_hours`）。
    /// 放进结构里只是为了单测能注入更短的窗口，生产路径永远由 [`Self::from_config`] 决定。
    pub tmp_max_age: Duration,
    /// `.staging-*` 的最短保留时长。
    ///
    /// **不是配置项**：来自 [`STAGING_MIN_AGE_SECS`] 常量，理由同上。
    pub staging_min_age: Duration,
}

impl ScanLimits {
    /// 从配置的 `[clean]` 段取值（唯一来源）。
    pub fn from_config(section: &CleanSection) -> Self {
        Self {
            scan_on_start: section.scan_on_start,
            scan_interval_hours: section.scan_interval_hours,
            idle_days: section.aria2_idle_days,
            log_max_size_mb: section.log_max_size_mb,
            log_keep_files: section.log_keep_files,
            cache_max_mb: section.cache_max_mb,
            min_free_space_gb: section.min_free_space_gb,
            tmp_max_age: Duration::from_secs(TMP_MAX_AGE_HOURS * 3600),
            staging_min_age: Duration::from_secs(STAGING_MIN_AGE_SECS),
        }
    }

    /// 孤儿控制文件的过期门槛。
    pub fn orphan_max_age(&self) -> Duration {
        Duration::from_secs(u64::from(self.idle_days) * SECONDS_PER_DAY)
    }
}

/// 扫描根（默认由 [`paths`] 拼装；测试注入临时目录）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanRoots {
    /// 用户平面版本库（可删）。
    pub user_versions: Option<PathBuf>,
    /// 服务平面版本库（只报告）。
    pub service_versions: Option<PathBuf>,
    /// 用户平面日志目录。
    pub user_logs: Option<PathBuf>,
    /// 服务平面日志目录（只报告）。
    pub service_logs: Option<PathBuf>,
    /// 用户平面 tmp。
    pub user_tmp: Option<PathBuf>,
    /// 服务平面 tmp（只报告）。
    pub service_tmp: Option<PathBuf>,
    /// 下载目录（扫孤儿 `.aria2`）；`None` 表示不知道，跳过这一类。
    pub dl_dir: Option<PathBuf>,
}

impl ScanRoots {
    /// 按当前环境变量拼出真实路径（缺环境变量则为 `None`）。
    pub fn from_paths() -> Self {
        Self {
            user_versions: paths::versions_dir(Plane::User),
            service_versions: paths::versions_dir(Plane::Service),
            user_logs: paths::logs_dir(Plane::User),
            service_logs: paths::logs_dir(Plane::Service),
            user_tmp: paths::tmp_dir(Plane::User),
            service_tmp: paths::tmp_dir(Plane::Service),
            dl_dir: None,
        }
    }
}

/// 一次扫描的输入。
pub struct ScanContext<'a> {
    /// 阈值（来自 `clean.*`）。
    pub limits: ScanLimits,
    /// 要扫的根。
    pub roots: ScanRoots,
    /// aria2 当前状态（决定孤儿 `.aria2` 是否可删）。
    pub aria2: Aria2State,
    /// 已知被活动/排队任务引用的路径（目标文件或控制文件）。
    pub active_paths: BTreeSet<PathBuf>,
    /// 宿主探测（进程/占用/磁盘）。
    pub live: &'a dyn LiveState,
}

impl<'a> ScanContext<'a> {
    /// 用配置 + 真实宿主拼出上下文（路径取当前环境变量）。
    pub fn from_config(config: &Config, live: &'a dyn LiveState) -> Self {
        let mut roots = ScanRoots::from_paths();
        let raw = config.dl.dir.trim();
        if !raw.is_empty() {
            let expanded = expand_user_path(raw);
            if !expanded.as_os_str().is_empty() {
                roots.dl_dir = Some(expanded);
            }
        }
        Self {
            limits: ScanLimits::from_config(&config.clean),
            roots,
            aria2: Aria2State::Unknown,
            active_paths: BTreeSet::new(),
            live,
        }
    }

    /// 只换根（测试用）。
    pub fn with_roots(mut self, roots: ScanRoots) -> Self {
        self.roots = roots;
        self
    }

    /// 设定 aria2 状态。
    pub fn with_aria2(mut self, state: Aria2State) -> Self {
        self.aria2 = state;
        self
    }

    /// 设定被任务引用的路径。
    pub fn with_active_paths(mut self, paths: BTreeSet<PathBuf>) -> Self {
        self.active_paths = paths;
        self
    }
}

// ---------------------------------------------------------------------------
// 报告
// ---------------------------------------------------------------------------

/// 一个可清条目。
///
/// **没有公共构造函数**：字段私有，只能由 [`scan`] 产生。这是「删除只走白名单」的
/// 类型层保证——调用方拼不出一个指向 `config.toml` 的条目。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanItem {
    target: CleanTarget,
    plane: Plane,
    path: PathBuf,
    bytes: u64,
    modified: Option<SystemTime>,
    reason: &'static str,
}

impl CleanItem {
    /// 目标类型。
    pub fn target(&self) -> CleanTarget {
        self.target
    }

    /// 完整路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 所属平面。
    pub fn plane(&self) -> Plane {
        self.plane
    }

    /// 占用字节数（目录为递归求和；是扫描时的快照）。
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// 修改时间（拿不到为 `None`）。
    pub fn modified(&self) -> Option<SystemTime> {
        self.modified
    }

    /// 为什么判定它可清（一句话，UI 直接显示）。
    pub fn reason(&self) -> &'static str {
        self.reason
    }

    /// 本条目是否允许真的删除。
    ///
    /// 服务平面（`%ProgramData%`）**永远为 `false`**：要管理员权限，且服务可能正在用。
    pub fn deletable(&self) -> bool {
        self.plane == Plane::User
    }
}

/// 一条「没有选中」的记录，用来解释判定表（UI 的「为什么没清」）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedItem {
    target: CleanTarget,
    plane: Plane,
    path: PathBuf,
    reason: String,
}

impl SkippedItem {
    /// 目标类型。
    pub fn target(&self) -> CleanTarget {
        self.target
    }

    /// 所属平面。
    pub fn plane(&self) -> Plane {
        self.plane
    }

    /// 路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 跳过原因（含动态信息，如 pid）。
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// 一类目标的汇总。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CategorySummary {
    /// 条目数。
    pub count: usize,
    /// 合计字节。
    pub bytes: u64,
}

/// 扫描报告（只读产物）。
#[derive(Debug, Clone)]
pub struct CleanReport {
    items: Vec<CleanItem>,
    skipped: Vec<SkippedItem>,
    by_target: BTreeMap<CleanTarget, CategorySummary>,
    total_bytes: u64,
    free_bytes: Option<u64>,
    low_disk: bool,
    warnings: Vec<String>,
    pointer_corrupt: Vec<Plane>,
    entries_scanned: usize,
    budget_exhausted: bool,
    elapsed: Duration,
    limits: ScanLimits,
    aria2: Aria2State,
}

impl CleanReport {
    fn new(limits: ScanLimits, aria2: Aria2State) -> Self {
        Self {
            items: Vec::new(),
            skipped: Vec::new(),
            by_target: BTreeMap::new(),
            total_bytes: 0,
            free_bytes: None,
            low_disk: false,
            warnings: Vec::new(),
            pointer_corrupt: Vec::new(),
            entries_scanned: 0,
            budget_exhausted: false,
            elapsed: Duration::ZERO,
            limits,
            aria2,
        }
    }

    /// 全部可清条目。
    pub fn items(&self) -> &[CleanItem] {
        &self.items
    }

    /// 被跳过的记录（判定表取证，UI 可折叠显示）。
    pub fn skipped(&self) -> &[SkippedItem] {
        &self.skipped
    }

    /// 某类目标的汇总。
    pub fn summary(&self, target: CleanTarget) -> CategorySummary {
        self.by_target.get(&target).copied().unwrap_or_default()
    }

    /// 全部可清条目合计字节。
    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// 全部条目数。
    pub fn total_count(&self) -> usize {
        self.items.len()
    }

    /// 建议打扰级别（低盘优先于体积）。
    pub fn urgency(&self) -> Urgency {
        if self.low_disk {
            Urgency::Card
        } else {
            Urgency::of_bytes(self.total_bytes)
        }
    }

    /// 下载盘剩余字节（拿不到为 `None`）。
    pub fn free_bytes(&self) -> Option<u64> {
        self.free_bytes
    }

    /// 下载盘剩余是否低于 `clean.min_free_space_gb`。
    pub fn low_disk(&self) -> bool {
        self.low_disk
    }

    /// 扫描过程中的告警（读不了的目录等，最多 [`MAX_WARNINGS`] 条）。
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// 激活指针损坏的平面（此时该平面一切都不删）。
    pub fn pointer_corrupt(&self) -> &[Plane] {
        &self.pointer_corrupt
    }

    /// 实际访问的目录项数。
    pub fn entries_scanned(&self) -> usize {
        self.entries_scanned
    }

    /// 是否撞到 [`SCAN_ENTRY_BUDGET`]（撞到说明结果不完整）。
    pub fn budget_exhausted(&self) -> bool {
        self.budget_exhausted
    }

    /// 本报告的扫描耗时。
    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }

    /// 本次扫描使用的阈值。
    pub fn limits(&self) -> &ScanLimits {
        &self.limits
    }

    /// 本次扫描假定的 aria2 状态。
    pub fn aria2(&self) -> Aria2State {
        self.aria2
    }

    /// 是否什么都没找到。
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    fn push_item(
        &mut self,
        target: CleanTarget,
        plane: Plane,
        path: &Path,
        bytes: u64,
        modified: Option<SystemTime>,
        reason: &'static str,
    ) {
        self.items.push(CleanItem {
            target,
            plane,
            path: path.to_path_buf(),
            bytes,
            modified,
            reason,
        });
        let entry = self.by_target.entry(target).or_default();
        entry.count += 1;
        entry.bytes += bytes;
        self.total_bytes += bytes;
    }

    fn push_skip(&mut self, target: CleanTarget, plane: Plane, path: &Path, reason: String) {
        self.skipped.push(SkippedItem {
            target,
            plane,
            path: path.to_path_buf(),
            reason,
        });
    }

    fn push_warning(&mut self, message: String) {
        if self.warnings.len() < MAX_WARNINGS {
            self.warnings.push(message);
        }
    }
}

// ---------------------------------------------------------------------------
// 扫描
// ---------------------------------------------------------------------------

/// 扫描目录项预算。
struct Budget {
    left: usize,
}

impl Budget {
    fn new(limit: usize) -> Self {
        Self { left: limit }
    }

    fn take(&mut self) -> bool {
        if self.left == 0 {
            return false;
        }
        self.left -= 1;
        true
    }

    fn used(&self) -> usize {
        SCAN_ENTRY_BUDGET.saturating_sub(self.left)
    }
}

/// 扫描并产出报告（**只读，不删任何东西**）。
pub fn scan(context: &ScanContext<'_>) -> CleanReport {
    let started = Instant::now();
    let mut report = CleanReport::new(context.limits.clone(), context.aria2);
    let mut budget = Budget::new(SCAN_ENTRY_BUDGET);

    scan_versions(
        Plane::User,
        context.roots.user_versions.as_deref(),
        context,
        &mut report,
        &mut budget,
    );
    scan_versions(
        Plane::Service,
        context.roots.service_versions.as_deref(),
        context,
        &mut report,
        &mut budget,
    );
    scan_logs(
        Plane::User,
        context.roots.user_logs.as_deref(),
        context.limits.log_keep_files,
        &mut report,
        &mut budget,
    );
    scan_logs(
        Plane::Service,
        context.roots.service_logs.as_deref(),
        context.limits.log_keep_files,
        &mut report,
        &mut budget,
    );
    scan_tmp(
        Plane::User,
        context.roots.user_tmp.as_deref(),
        context.limits.tmp_max_age,
        &mut report,
        &mut budget,
    );
    scan_tmp(
        Plane::Service,
        context.roots.service_tmp.as_deref(),
        context.limits.tmp_max_age,
        &mut report,
        &mut budget,
    );
    scan_orphans(context, &mut report, &mut budget);

    report.entries_scanned = budget.used();
    report.budget_exhausted = budget.left == 0;

    let free = context
        .roots
        .dl_dir
        .as_deref()
        .and_then(|dir| context.live.free_bytes(dir));
    report.free_bytes = free;
    let threshold = u64::from(context.limits.min_free_space_gb) * BYTES_PER_GIB;
    report.low_disk = matches!(free, Some(bytes) if bytes < threshold);
    report.elapsed = started.elapsed();
    report
}

/// 版本库：废弃版本 + 安装残留。
fn scan_versions(
    plane: Plane,
    root: Option<&Path>,
    context: &ScanContext<'_>,
    report: &mut CleanReport,
    budget: &mut Budget,
) {
    let Some(root) = root else {
        return;
    };
    let pointer = read_pointer(root);
    if pointer.is_corrupt() {
        report.pointer_corrupt.push(plane);
    }

    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            report.push_warning(format!("读不了版本库目录 {}：{error}", root.display()));
            return;
        }
    };

    for entry in entries {
        if !budget.take() {
            return;
        }
        let Ok(entry) = entry else {
            continue;
        };
        let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
        if !is_dir {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();

        if name.starts_with(STAGING_PREFIX) {
            handle_staging(plane, &path, &name, context, report);
            continue;
        }

        if parse_version_number(&name).is_err() {
            report.push_skip(
                CleanTarget::AbandonedVersion,
                plane,
                &path,
                "目录名不是 aria2 版本号，不归本模块处理".to_owned(),
            );
            continue;
        }

        match &pointer {
            PointerState::Known { active, previous } => {
                if name.as_str() == active.as_str() {
                    report.push_skip(
                        CleanTarget::AbandonedVersion,
                        plane,
                        &path,
                        "激活版（active），永不删除".to_owned(),
                    );
                    continue;
                }
                if previous.as_deref() == Some(name.as_str()) {
                    report.push_skip(
                        CleanTarget::AbandonedVersion,
                        plane,
                        &path,
                        "上一代版本（previous，单代回滚用），保留".to_owned(),
                    );
                    continue;
                }
            }
            PointerState::Missing => {
                report.push_skip(
                    CleanTarget::AbandonedVersion,
                    plane,
                    &path,
                    "没有 active.json 激活指针，保守跳过".to_owned(),
                );
                continue;
            }
            PointerState::Corrupt => {
                report.push_skip(
                    CleanTarget::AbandonedVersion,
                    plane,
                    &path,
                    "激活指针损坏，保守跳过".to_owned(),
                );
                continue;
            }
        }

        if context.live.exe_in_use(&path.join(CORE_EXE)) {
            report.push_skip(
                CleanTarget::AbandonedVersion,
                plane,
                &path,
                "使用中：该目录的 aria2c 正在运行（写打开被拒）".to_owned(),
            );
            continue;
        }

        let bytes = dir_size(&path, budget);
        let modified = fs::metadata(&path)
            .ok()
            .and_then(|meta| meta.modified().ok());
        report.push_item(
            CleanTarget::AbandonedVersion,
            plane,
            &path,
            bytes,
            modified,
            "既不是激活版也不是上一代，可回收",
        );
    }
}

/// 安装残留的处理（年龄 + pid 双重排除）。
fn handle_staging(
    plane: Plane,
    path: &Path,
    name: &str,
    context: &ScanContext<'_>,
    report: &mut CleanReport,
) {
    let modified = fs::metadata(path)
        .ok()
        .and_then(|meta| meta.modified().ok());
    let age = age_seconds(modified, SystemTime::now());
    let min_age = context.limits.staging_min_age;
    if Duration::from_secs(age) < min_age {
        report.push_skip(
            CleanTarget::StaleStaging,
            plane,
            path,
            format!(
                "未满 {} 小时（实际 {age} 秒），可能是另一实例正在安装",
                min_age.as_secs() / 3600
            ),
        );
        return;
    }
    if let Some(pid) = staging_pid(name) {
        if context.live.pid_alive(pid) {
            report.push_skip(
                CleanTarget::StaleStaging,
                plane,
                path,
                format!("安装进程 {pid} 仍存活"),
            );
            return;
        }
    }
    let bytes = dir_size(path, &mut Budget::new(SCAN_ENTRY_BUDGET));
    report.push_item(
        CleanTarget::StaleStaging,
        plane,
        path,
        bytes,
        modified,
        "安装残留：进程已退出且超过 1 小时",
    );
}

/// 轮转日志。
fn scan_logs(
    plane: Plane,
    dir: Option<&Path>,
    keep_files: u32,
    report: &mut CleanReport,
    budget: &mut Budget,
) {
    let Some(dir) = dir else {
        return;
    };
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            report.push_warning(format!("读不了日志目录 {}：{error}", dir.display()));
            return;
        }
    };
    for entry in entries {
        if !budget.take() {
            return;
        }
        let Ok(entry) = entry else {
            continue;
        };
        if !entry
            .file_type()
            .map(|kind| kind.is_file())
            .unwrap_or(false)
        {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(index) = rotated_index(&name) else {
            continue;
        };
        // keep_files 是总份数（含当前 *.log），所以序号 >= keep_files 的轮转文件都超出。
        if index < keep_files {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        report.push_item(
            CleanTarget::RotatedLog,
            plane,
            &path,
            meta.len(),
            meta.modified().ok(),
            "轮转序号已达 clean.log_keep_files（总份数上限）",
        );
    }
}

/// `tmp\` 过期文件。
fn scan_tmp(
    plane: Plane,
    dir: Option<&Path>,
    max_age: Duration,
    report: &mut CleanReport,
    budget: &mut Budget,
) {
    let Some(dir) = dir else {
        return;
    };
    let now = SystemTime::now();
    walk_files(dir, budget, None, &mut |path, bytes, modified| {
        let age = Duration::from_secs(age_seconds(modified, now));
        if age >= max_age {
            report.push_item(
                CleanTarget::TmpFile,
                plane,
                path,
                bytes,
                modified,
                "临时目录里超过 24 小时的文件",
            );
        }
    });
}

/// 下载目录里的孤儿 `.aria2`（判定表见 [`CleanTarget::OrphanControlFile`]）。
fn scan_orphans(context: &ScanContext<'_>, report: &mut CleanReport, budget: &mut Budget) {
    let Some(root) = context.roots.dl_dir.as_deref() else {
        return;
    };
    let now = SystemTime::now();
    let max_age = context.limits.orphan_max_age();
    let idle_days = context.limits.idle_days;
    // 先按文件名筛，只有 `*.aria2` 才取元数据：大下载目录里这是数量级差异。
    let only_control = |path: &Path| is_orphan_control(path);
    walk_files(
        root,
        budget,
        Some(&only_control),
        &mut |control, bytes, modified| {
            let target = control.with_extension("");
            if is_referenced(&context.active_paths, &target, control) {
                report.push_skip(
                    CleanTarget::OrphanControlFile,
                    Plane::User,
                    control,
                    "有任务引用该文件，不删".to_owned(),
                );
                return;
            }
            if target.exists() {
                let reason = if context.aria2 == Aria2State::Running {
                    "目标文件仍在且 aria2 在跑（可能是暂停/排队任务），不删"
                } else {
                    "目标文件仍在，可能是可续传的残留，不删"
                };
                report.push_skip(
                    CleanTarget::OrphanControlFile,
                    Plane::User,
                    control,
                    reason.to_owned(),
                );
                return;
            }
            if context.aria2 != Aria2State::Stopped {
                report.push_skip(
                    CleanTarget::OrphanControlFile,
                    Plane::User,
                    control,
                    "无法确认 aria2 任务列表（状态未知/在跑），保守不删".to_owned(),
                );
                return;
            }
            let age = Duration::from_secs(age_seconds(modified, now));
            if age < max_age {
                report.push_skip(
                    CleanTarget::OrphanControlFile,
                    Plane::User,
                    control,
                    format!("目标文件已不在，但未满 {idle_days} 天"),
                );
                return;
            }
            report.push_item(
                CleanTarget::OrphanControlFile,
                Plane::User,
                control,
                bytes,
                modified,
                "目标文件已不在且超过 clean.aria2_idle_days",
            );
        },
    );
}

// ---------------------------------------------------------------------------
// 清理
// ---------------------------------------------------------------------------

/// 一条删除结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanResult {
    /// 目标类型。
    pub target: CleanTarget,
    /// 路径。
    pub path: PathBuf,
    /// 该条目声明的字节数。
    pub bytes: u64,
    /// `Ok(())` 表示删掉了；`Err(原因)` 表示跳过/失败（**不影响其他条目**）。
    pub outcome: Result<(), String>,
}

impl CleanResult {
    /// 是否成功。
    pub fn is_ok(&self) -> bool {
        self.outcome.is_ok()
    }
}

/// 一次清理的结果汇总。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanOutcome {
    /// 逐条结果（顺序与输入一致）。
    pub results: Vec<CleanResult>,
    /// 成功删除的字节数。
    pub removed_bytes: u64,
    /// 成功条数。
    pub removed_count: usize,
    /// 失败/跳过条数。
    pub failed_count: usize,
}

impl CleanOutcome {
    /// 是否全部成功。
    pub fn is_all_removed(&self) -> bool {
        self.failed_count == 0 && self.removed_count == self.results.len()
    }
}

/// 按报告条目清理（注入宿主探测，测试用）。
///
/// 逐项删除，**部分失败不影响其余项**，全部失败也不 panic。
/// 删除前会再核对一次「现在是否仍然可删」（防止扫描到用户点确认之间状态变了）。
pub fn clean_with(items: &[CleanItem], live: &dyn LiveState) -> CleanOutcome {
    let mut outcome = CleanOutcome::default();
    for item in items {
        let result = remove_one(item, live);
        if result.is_ok() {
            outcome.removed_count += 1;
            outcome.removed_bytes += item.bytes;
        } else {
            outcome.failed_count += 1;
        }
        outcome.results.push(CleanResult {
            target: item.target,
            path: item.path.clone(),
            bytes: item.bytes,
            outcome: result,
        });
    }
    outcome
}

/// 按报告条目清理（真实宿主）。
pub fn clean(items: &[CleanItem]) -> CleanOutcome {
    clean_with(items, &RealLiveState)
}

fn remove_one(item: &CleanItem, live: &dyn LiveState) -> Result<(), String> {
    if item.plane != Plane::User {
        return Err("在服务平面（%ProgramData%）里，需要管理员权限；本次只报告不删除".to_owned());
    }
    match item.target {
        CleanTarget::AbandonedVersion => {
            let root = item
                .path
                .parent()
                .ok_or_else(|| "路径异常：没有父目录".to_owned())?;
            let name = item
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .ok_or_else(|| "路径异常：没有目录名".to_owned())?;
            match read_pointer(root) {
                PointerState::Known { active, previous } => {
                    if name.as_str() == active.as_str()
                        || previous.as_deref() == Some(name.as_str())
                    {
                        return Err("这已经是激活版或上一代版本，拒绝删除".to_owned());
                    }
                }
                PointerState::Missing | PointerState::Corrupt => {
                    return Err("激活指针缺失或损坏，拒绝删除".to_owned());
                }
            }
            if live.exe_in_use(&item.path.join(CORE_EXE)) {
                return Err("使用中：该目录的 aria2c 正在运行".to_owned());
            }
            fs::remove_dir_all(&item.path).map_err(|error| format!("删除目录失败：{error}"))
        }
        CleanTarget::StaleStaging => {
            // 年龄不再复核：扫描时已判定超龄，而年龄只会随时间变大（单调），
            // 不存在「变得不够老」的情况。真正会变的是 pid——安装进程可能刚起来。
            let name = item
                .path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            if let Some(pid) = staging_pid(&name) {
                if live.pid_alive(pid) {
                    return Err(format!("安装进程 {pid} 仍存活"));
                }
            }
            fs::remove_dir_all(&item.path).map_err(|error| format!("删除目录失败：{error}"))
        }
        CleanTarget::OrphanControlFile => {
            let target = item.path.with_extension("");
            if target.exists() {
                return Err("目标文件又出现了（可能已续传），拒绝删控制文件".to_owned());
            }
            if !item.path.exists() {
                return Err("文件已不存在".to_owned());
            }
            fs::remove_file(&item.path).map_err(|error| format!("删除文件失败：{error}"))
        }
        CleanTarget::RotatedLog | CleanTarget::TmpFile => {
            if !item.path.exists() {
                return Err("文件已不存在".to_owned());
            }
            fs::remove_file(&item.path).map_err(|error| format!("删除文件失败：{error}"))
        }
    }
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

/// 激活指针的读取结果。
#[derive(Debug, Clone, PartialEq, Eq)]
enum PointerState {
    Known {
        active: String,
        previous: Option<String>,
    },
    Missing,
    Corrupt,
}

impl PointerState {
    fn is_corrupt(&self) -> bool {
        matches!(self, Self::Corrupt)
    }
}

fn read_pointer(root: &Path) -> PointerState {
    let path = root.join(ACTIVE_FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return PointerState::Missing,
        Err(_) => return PointerState::Corrupt,
    };
    match serde_json::from_slice::<ActivePointer>(&bytes) {
        Ok(pointer) => PointerState::Known {
            active: pointer.active,
            previous: pointer.previous,
        },
        Err(_) => PointerState::Corrupt,
    }
}

/// 递归求和目录内文件字节数（不跟进软链；撞预算就停下）。
fn dir_size(dir: &Path, budget: &mut Budget) -> u64 {
    let mut total = 0;
    walk_files(dir, budget, None, &mut |_, bytes, _| total += bytes);
    total
}

/// 遍历目录下的文件（不跟进软链/联接点）。读不了的子树静默跳过。
///
/// `filter` 在**取元数据之前**先用文件名筛一遍：大下载目录里 99.9% 的文件不是
/// `.aria2`，对它们调用 `metadata()` 是纯浪费（网络盘/9p 上一次 stat 能到毫秒级）。
fn walk_files(
    dir: &Path,
    budget: &mut Budget,
    filter: Option<&dyn Fn(&Path) -> bool>,
    visit: &mut dyn FnMut(&Path, u64, Option<SystemTime>),
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries {
        if !budget.take() {
            return;
        }
        let Ok(entry) = entry else {
            continue;
        };
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() {
            continue;
        }
        let path = entry.path();
        if kind.is_dir() {
            walk_files(&path, budget, filter, visit);
        } else if kind.is_file() {
            if let Some(filter) = filter {
                if !filter(&path) {
                    continue;
                }
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            visit(&path, meta.len(), meta.modified().ok());
        }
    }
}

/// 距今多少秒（未来时间/取不到都算 0，即「很新」——保守方向）。
fn age_seconds(modified: Option<SystemTime>, now: SystemTime) -> u64 {
    match modified.and_then(|time| now.duration_since(time).ok()) {
        Some(elapsed) => elapsed.as_secs(),
        None => 0,
    }
}

/// `.staging-<version>-<pid>-<seq>` 里解析 pid。
fn staging_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix(STAGING_PREFIX)?;
    let parts: Vec<&str> = rest.rsplit('-').collect();
    // rsplit 顺序：seq, pid, version(可能含 '-')
    parts.get(1)?.parse::<u32>().ok()
}

/// 是不是孤儿控制文件的候选名（`*.aria2`，大小写不敏感）。
fn is_orphan_control(path: &Path) -> bool {
    path.file_name()
        .map(|name| {
            name.to_string_lossy()
                .to_lowercase()
                .ends_with(ORPHAN_SUFFIX)
        })
        .unwrap_or(false)
}

/// 该路径是否被任务引用（Windows 路径大小写不敏感，这里做归一化比较）。
fn is_referenced(active: &BTreeSet<PathBuf>, target: &Path, control: &Path) -> bool {
    active
        .iter()
        .any(|known| same_path(known, target) || same_path(known, control))
}

fn same_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    let left = left.to_string_lossy().to_lowercase();
    let right = right.to_string_lossy().to_lowercase();
    left == right
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::sync::atomic::{AtomicU32, Ordering};

    use ariahost_common::config::{CleanSection, DlSection};

    use super::*;

    static SEQ: AtomicU32 = AtomicU32::new(0);

    /// 每个测试一个临时目录，Drop 时删掉（不碰真实用户目录）。
    struct TempDir {
        root: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir()
                .join(format!("ariahost-clean-{tag}-{}-{seq}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).expect("建临时目录");
            Self { root }
        }

        fn path(&self) -> &Path {
            &self.root
        }

        fn dir(&self, rel: &str) -> PathBuf {
            let path = self.root.join(rel);
            fs::create_dir_all(&path).expect("建子目录");
            path
        }

        fn file(&self, rel: &str, bytes: usize) -> PathBuf {
            let path = self.root.join(rel);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("建父目录");
            }
            let mut file = fs::File::create(&path).expect("建文件");
            file.write_all(&vec![b'x'; bytes]).expect("写文件");
            path
        }

        /// 把文件 mtime 往回拨（跨平台：`File::set_times`）。
        fn age_file(&self, path: &Path, age: Duration) {
            let modified = SystemTime::now().checked_sub(age).expect("时间下溢");
            let file = fs::File::options()
                .write(true)
                .open(path)
                .expect("打开设时间");
            file.set_times(fs::FileTimes::new().set_modified(modified))
                .expect("设 mtime");
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    /// 可编程的宿主探测。
    struct FakeLive {
        alive: BTreeSet<u32>,
        in_use: BTreeSet<PathBuf>,
        free: Option<u64>,
    }

    impl FakeLive {
        fn new() -> Self {
            Self {
                alive: BTreeSet::new(),
                in_use: BTreeSet::new(),
                free: None,
            }
        }

        fn with_alive(mut self, pid: u32) -> Self {
            self.alive.insert(pid);
            self
        }

        fn with_in_use(mut self, path: impl Into<PathBuf>) -> Self {
            self.in_use.insert(path.into());
            self
        }

        fn with_free(mut self, bytes: u64) -> Self {
            self.free = Some(bytes);
            self
        }
    }

    impl LiveState for FakeLive {
        fn pid_alive(&self, pid: u32) -> bool {
            self.alive.contains(&pid)
        }

        fn exe_in_use(&self, path: &Path) -> bool {
            self.in_use.contains(path)
        }

        fn free_bytes(&self, _dir: &Path) -> Option<u64> {
            self.free
        }
    }

    fn limits() -> ScanLimits {
        ScanLimits::from_config(&CleanSection::default())
    }

    fn context<'a>(live: &'a FakeLive, roots: ScanRoots, limits: ScanLimits) -> ScanContext<'a> {
        ScanContext {
            limits,
            roots,
            aria2: Aria2State::Unknown,
            active_paths: BTreeSet::new(),
            live,
        }
    }

    fn user_versions_roots(root: &Path) -> ScanRoots {
        ScanRoots {
            user_versions: Some(root.to_path_buf()),
            ..ScanRoots::default()
        }
    }

    fn install_version(root: &Path, version: &str) -> PathBuf {
        let dir = root.join(version);
        fs::create_dir_all(&dir).expect("建版本目录");
        fs::write(dir.join(CORE_EXE), b"exe").expect("写 exe");
        dir
    }

    fn write_pointer(root: &Path, active: &str, previous: Option<&str>) {
        let pointer = ActivePointer {
            active: active.to_owned(),
            previous: previous.map(str::to_owned),
        };
        fs::write(
            root.join(ACTIVE_FILE),
            serde_json::to_vec(&pointer).expect("序列化指针"),
        )
        .expect("写激活指针");
    }

    fn item_names(report: &CleanReport, target: CleanTarget) -> Vec<String> {
        let mut names: Vec<String> = report
            .items()
            .iter()
            .filter(|item| item.target() == target)
            .filter_map(|item| item.path().file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    // ---- 阈值 ----

    #[test]
    fn default_limits_come_from_clean_section() {
        let limits = limits();
        assert!(limits.scan_on_start);
        assert_eq!(limits.scan_interval_hours, 24);
        assert_eq!(limits.idle_days, 7);
        assert_eq!(limits.log_max_size_mb, 10);
        assert_eq!(limits.log_keep_files, 5);
        assert_eq!(limits.cache_max_mb, 500);
        assert_eq!(limits.min_free_space_gb, 5);
        assert_eq!(
            limits.tmp_max_age,
            Duration::from_secs(TMP_MAX_AGE_HOURS * 3600)
        );
        assert_eq!(
            limits.staging_min_age,
            Duration::from_secs(STAGING_MIN_AGE_SECS)
        );
        assert_eq!(
            limits.orphan_max_age(),
            Duration::from_secs(7 * SECONDS_PER_DAY)
        );
    }

    #[test]
    fn urgency_follows_bytes_and_low_disk() {
        assert_eq!(Urgency::of_bytes(0), Urgency::Silent);
        assert_eq!(Urgency::of_bytes(SILENT_MAX_BYTES - 1), Urgency::Silent);
        assert_eq!(Urgency::of_bytes(SILENT_MAX_BYTES), Urgency::Bubble);
        assert_eq!(Urgency::of_bytes(BUBBLE_MAX_BYTES - 1), Urgency::Bubble);
        assert_eq!(Urgency::of_bytes(BUBBLE_MAX_BYTES), Urgency::Card);

        // 低盘把「静默」抬成「提示卡」，哪怕一个字节都没找到。
        let temp = TempDir::new("urgency");
        let live = FakeLive::new().with_free(1024);
        let mut ctx = context(&live, ScanRoots::default(), limits());
        ctx.roots.dl_dir = Some(temp.path().to_path_buf());
        let report = scan(&ctx);
        assert!(report.low_disk());
        assert_eq!(report.urgency(), Urgency::Card);
        assert_eq!(report.free_bytes(), Some(1024));
    }

    #[test]
    fn context_from_config_reads_dl_dir_and_limits() {
        let temp = TempDir::new("cfg");
        let config = Config {
            dl: DlSection {
                dir: temp.path().to_string_lossy().into_owned(),
                ..DlSection::default()
            },
            clean: CleanSection {
                aria2_idle_days: 3,
                ..CleanSection::default()
            },
            ..Config::default()
        };
        let live = FakeLive::new();
        let ctx = ScanContext::from_config(&config, &live);
        assert_eq!(ctx.roots.dl_dir.as_deref(), Some(temp.path()));
        assert_eq!(ctx.limits.idle_days, 3);
        assert_eq!(
            ctx.limits.orphan_max_age(),
            Duration::from_secs(3 * SECONDS_PER_DAY)
        );
    }

    // ---- 轮转日志 ----

    #[test]
    fn rotated_logs_beyond_keep_files_are_collected() {
        let temp = TempDir::new("logs");
        let logs = temp.dir("logs");
        temp.file("logs/ariahost.log", 10);
        for index in 1..=7 {
            temp.file(&format!("logs/ariahost.log.{index}"), 100);
        }
        temp.file("logs/notes.txt", 10);
        let live = FakeLive::new();
        let roots = ScanRoots {
            user_logs: Some(logs),
            ..ScanRoots::default()
        };
        let report = scan(&context(&live, roots, limits()));
        // 总份数 = 5（当前 ariahost.log + .1..=4），所以 .5/.6/.7 超出。
        assert_eq!(
            item_names(&report, CleanTarget::RotatedLog),
            vec![
                "ariahost.log.5".to_owned(),
                "ariahost.log.6".to_owned(),
                "ariahost.log.7".to_owned()
            ]
        );
        assert_eq!(report.summary(CleanTarget::RotatedLog).bytes, 300);
    }

    #[test]
    fn log_keep_files_honours_config() {
        let temp = TempDir::new("logs2");
        let logs = temp.dir("logs");
        for index in 1..=4 {
            temp.file(&format!("logs/aria2.log.{index}"), 100);
        }
        let live = FakeLive::new();
        let roots = ScanRoots {
            user_logs: Some(logs),
            ..ScanRoots::default()
        };
        let mut limits = limits();
        limits.log_keep_files = 2;
        let report = scan(&context(&live, roots, limits));
        // keep_files = 2 → 总份数 2（当前 + .1），所以 .2/.3/.4 全部超出。
        assert_eq!(
            item_names(&report, CleanTarget::RotatedLog),
            vec![
                "aria2.log.2".to_owned(),
                "aria2.log.3".to_owned(),
                "aria2.log.4".to_owned()
            ]
        );
    }

    // ---- 废弃版本 ----

    #[test]
    fn abandoned_versions_keep_active_and_previous() {
        let temp = TempDir::new("versions");
        let root = temp.dir("versions");
        for version in ["1.37.0", "1.36.0", "1.35.0", "1.34.0"] {
            install_version(&root, version);
        }
        write_pointer(&root, "1.37.0", Some("1.36.0"));
        let live = FakeLive::new();
        let report = scan(&context(&live, user_versions_roots(&root), limits()));
        assert_eq!(
            item_names(&report, CleanTarget::AbandonedVersion),
            vec!["1.34.0".to_owned(), "1.35.0".to_owned()]
        );
        assert!(report
            .skipped()
            .iter()
            .any(|skip| skip.reason().contains("激活版")));
        assert!(report
            .skipped()
            .iter()
            .any(|skip| skip.reason().contains("上一代")));
        assert!(report.items().iter().all(CleanItem::deletable));
    }

    #[test]
    fn abandoned_version_bytes_are_summed_recursively() {
        let temp = TempDir::new("size");
        let root = temp.dir("versions");
        let old = install_version(&root, "1.35.0");
        fs::write(old.join("libaria2.dll"), vec![b'y'; 500]).expect("写 dll");
        install_version(&root, "1.37.0");
        write_pointer(&root, "1.37.0", None);
        let live = FakeLive::new();
        let report = scan(&context(&live, user_versions_roots(&root), limits()));
        assert_eq!(report.summary(CleanTarget::AbandonedVersion).bytes, 503);
    }

    #[test]
    fn missing_pointer_deletes_nothing() {
        let temp = TempDir::new("pointer-missing");
        let root = temp.dir("versions");
        install_version(&root, "1.37.0");
        install_version(&root, "1.35.0");
        let live = FakeLive::new();
        let report = scan(&context(&live, user_versions_roots(&root), limits()));
        assert_eq!(report.total_count(), 0);
        assert!(report
            .skipped()
            .iter()
            .any(|skip| skip.reason().contains("没有 active.json")));
    }

    #[test]
    fn corrupt_pointer_deletes_nothing() {
        let temp = TempDir::new("pointer-corrupt");
        let root = temp.dir("versions");
        install_version(&root, "1.37.0");
        install_version(&root, "1.35.0");
        fs::write(root.join(ACTIVE_FILE), b"not-json").expect("写坏指针");
        let live = FakeLive::new();
        let report = scan(&context(&live, user_versions_roots(&root), limits()));
        assert_eq!(report.total_count(), 0);
        assert_eq!(report.pointer_corrupt(), [Plane::User]);
        assert!(report
            .skipped()
            .iter()
            .any(|skip| skip.reason().contains("损坏")));
    }

    #[test]
    fn version_in_use_is_skipped() {
        let temp = TempDir::new("in-use");
        let root = temp.dir("versions");
        install_version(&root, "1.37.0");
        let old = install_version(&root, "1.35.0");
        write_pointer(&root, "1.37.0", None);
        let live = FakeLive::new().with_in_use(old.join(CORE_EXE));
        let report = scan(&context(&live, user_versions_roots(&root), limits()));
        assert_eq!(report.total_count(), 0);
        assert!(report
            .skipped()
            .iter()
            .any(|skip| skip.reason().contains("使用中")));
    }

    #[test]
    fn non_version_directories_are_ignored() {
        let temp = TempDir::new("non-version");
        let root = temp.dir("versions");
        install_version(&root, "1.37.0");
        write_pointer(&root, "1.37.0", None);
        temp.dir("versions/downloads");
        let live = FakeLive::new();
        let report = scan(&context(&live, user_versions_roots(&root), limits()));
        assert_eq!(report.total_count(), 0);
        assert!(report
            .skipped()
            .iter()
            .any(|skip| skip.reason().contains("不是 aria2 版本号")));
    }

    // ---- 安装残留 ----

    #[test]
    fn staging_with_live_pid_is_kept() {
        let temp = TempDir::new("staging-live");
        let root = temp.dir("versions");
        install_version(&root, "1.37.0");
        write_pointer(&root, "1.37.0", None);
        let staging = temp.dir("versions/.staging-1.38.0-4242-0");
        let live = FakeLive::new().with_alive(4242);
        let mut limits = limits();
        limits.staging_min_age = Duration::ZERO;
        let report = scan(&context(&live, user_versions_roots(&root), limits));
        assert_eq!(report.total_count(), 0);
        assert!(report
            .skipped()
            .iter()
            .any(|skip| skip.reason().contains("4242")));
        assert!(staging.exists());
    }

    #[test]
    fn staging_younger_than_one_hour_is_kept() {
        let temp = TempDir::new("staging-young");
        let root = temp.dir("versions");
        install_version(&root, "1.37.0");
        write_pointer(&root, "1.37.0", None);
        temp.dir("versions/.staging-1.38.0-9999-0");
        let live = FakeLive::new();
        // 默认阈值（1 小时）：刚建的目录不该被选中。
        let report = scan(&context(&live, user_versions_roots(&root), limits()));
        assert_eq!(report.total_count(), 0);
        assert!(report
            .skipped()
            .iter()
            .any(|skip| skip.reason().contains("未满 1 小时")));
    }

    #[test]
    fn stale_staging_is_collected_and_removed() {
        let temp = TempDir::new("staging-stale");
        let root = temp.dir("versions");
        install_version(&root, "1.37.0");
        write_pointer(&root, "1.37.0", None);
        let staging = temp.dir("versions/.staging-1.38.0-4242-0");
        fs::write(staging.join(CORE_EXE), b"exe").expect("写 exe");
        let live = FakeLive::new();
        let mut limits = limits();
        limits.staging_min_age = Duration::ZERO;
        let report = scan(&context(&live, user_versions_roots(&root), limits));
        assert_eq!(
            item_names(&report, CleanTarget::StaleStaging),
            vec![".staging-1.38.0-4242-0".to_owned()]
        );
        let outcome = clean_with(report.items(), &live);
        assert!(outcome.is_all_removed(), "{:?}", outcome.results);
        assert!(!staging.exists());
    }

    #[test]
    fn staging_without_usable_pid_falls_back_to_age() {
        let temp = TempDir::new("staging-nopid");
        let root = temp.dir("versions");
        install_version(&root, "1.37.0");
        write_pointer(&root, "1.37.0", None);
        temp.dir("versions/.staging-broken");
        let live = FakeLive::new();
        let mut limits = limits();
        limits.staging_min_age = Duration::ZERO;
        let report = scan(&context(&live, user_versions_roots(&root), limits));
        assert_eq!(
            item_names(&report, CleanTarget::StaleStaging),
            vec![".staging-broken".to_owned()]
        );
    }

    // ---- tmp ----

    #[test]
    fn fresh_tmp_files_are_kept() {
        let temp = TempDir::new("tmp-fresh");
        let tmp = temp.dir("tmp");
        temp.file("tmp/report.zip", 64);
        let live = FakeLive::new();
        let roots = ScanRoots {
            user_tmp: Some(tmp),
            ..ScanRoots::default()
        };
        let report = scan(&context(&live, roots, limits()));
        assert_eq!(report.total_count(), 0);
    }

    #[test]
    fn aged_tmp_files_are_collected_and_removed() {
        let temp = TempDir::new("tmp-aged");
        let tmp = temp.dir("tmp");
        let stale = temp.file("tmp/report.zip", 64);
        temp.file("tmp/nested/deep.bin", 32);
        let live = FakeLive::new();
        let roots = ScanRoots {
            user_tmp: Some(tmp),
            ..ScanRoots::default()
        };
        // 注入 0 秒窗口：不依赖真的把 mtime 拨回 24 小时（Windows 目录不便设时间）。
        let mut limits = limits();
        limits.tmp_max_age = Duration::ZERO;
        let report = scan(&context(&live, roots, limits));
        assert_eq!(report.summary(CleanTarget::TmpFile).count, 2);
        let outcome = clean_with(report.items(), &live);
        assert!(outcome.is_all_removed(), "{:?}", outcome.results);
        assert!(!stale.exists());
    }

    // ---- 孤儿 .aria2 判定表 ----

    #[test]
    fn orphan_with_existing_target_is_kept() {
        let temp = TempDir::new("orphan-target");
        let dl = temp.dir("dl");
        let control = temp.file("dl/movie.mkv.aria2", 32);
        temp.file("dl/movie.mkv", 128);
        temp.age_file(&control, Duration::from_secs(30 * SECONDS_PER_DAY));
        let live = FakeLive::new();
        let roots = ScanRoots {
            dl_dir: Some(dl),
            ..ScanRoots::default()
        };
        let mut ctx = context(&live, roots, limits());
        ctx.aria2 = Aria2State::Stopped;
        let report = scan(&ctx);
        assert_eq!(report.total_count(), 0, "目标文件还在就不许删（可续传）");
        assert!(report
            .skipped()
            .iter()
            .any(|skip| skip.reason().contains("可续传")));
    }

    #[test]
    fn orphan_referenced_by_task_is_kept() {
        let temp = TempDir::new("orphan-ref");
        let dl = temp.dir("dl");
        let control = temp.file("dl/movie.mkv.aria2", 32);
        temp.age_file(&control, Duration::from_secs(30 * SECONDS_PER_DAY));
        let live = FakeLive::new();
        let roots = ScanRoots {
            dl_dir: Some(dl),
            ..ScanRoots::default()
        };
        let mut ctx = context(&live, roots, limits());
        ctx.aria2 = Aria2State::Stopped;
        let mut active = BTreeSet::new();
        active.insert(temp.path().join("dl/movie.mkv"));
        ctx.active_paths = active;
        let report = scan(&ctx);
        assert_eq!(report.total_count(), 0);
        assert!(report
            .skipped()
            .iter()
            .any(|skip| skip.reason().contains("有任务引用")));
    }

    #[test]
    fn orphan_old_and_unreferenced_is_collected_then_removed() {
        let temp = TempDir::new("orphan-del");
        let dl = temp.dir("dl");
        let control = temp.file("dl/movie.mkv.aria2", 32);
        temp.age_file(&control, Duration::from_secs(8 * SECONDS_PER_DAY));
        let live = FakeLive::new();
        let roots = ScanRoots {
            dl_dir: Some(dl),
            ..ScanRoots::default()
        };
        let mut ctx = context(&live, roots, limits());
        ctx.aria2 = Aria2State::Stopped;
        let report = scan(&ctx);
        assert_eq!(report.total_count(), 1);
        assert_eq!(report.items()[0].bytes(), 32);
        assert!(report.items()[0].reason().contains("aria2_idle_days"));
        let outcome = clean_with(report.items(), &live);
        assert!(outcome.is_all_removed(), "{:?}", outcome.results);
        assert!(!control.exists());
    }

    #[test]
    fn orphan_younger_than_idle_days_is_kept() {
        let temp = TempDir::new("orphan-young");
        let dl = temp.dir("dl");
        let control = temp.file("dl/movie.mkv.aria2", 32);
        temp.age_file(&control, Duration::from_secs(SECONDS_PER_DAY));
        let live = FakeLive::new();
        let roots = ScanRoots {
            dl_dir: Some(dl),
            ..ScanRoots::default()
        };
        let mut ctx = context(&live, roots, limits());
        ctx.aria2 = Aria2State::Stopped;
        let report = scan(&ctx);
        assert_eq!(report.total_count(), 0);
        assert!(report
            .skipped()
            .iter()
            .any(|skip| skip.reason().contains("未满 7 天")));
    }

    #[test]
    fn orphan_is_kept_when_aria2_state_is_not_stopped() {
        for state in [Aria2State::Running, Aria2State::Unknown] {
            let temp = TempDir::new("orphan-state");
            let dl = temp.dir("dl");
            let control = temp.file("dl/movie.mkv.aria2", 32);
            temp.age_file(&control, Duration::from_secs(30 * SECONDS_PER_DAY));
            let live = FakeLive::new();
            let roots = ScanRoots {
                dl_dir: Some(dl),
                ..ScanRoots::default()
            };
            let mut ctx = context(&live, roots, limits());
            ctx.aria2 = state;
            let report = scan(&ctx);
            assert_eq!(report.total_count(), 0, "状态 {state:?} 下不许删");
            assert!(report
                .skipped()
                .iter()
                .any(|skip| skip.reason().contains("保守不删")));
        }
    }

    // ---- 双平面 ----

    #[test]
    fn service_plane_is_reported_but_never_deleted() {
        let temp = TempDir::new("service");
        let service = temp.dir("service-versions");
        install_version(&service, "1.37.0");
        let old = install_version(&service, "1.35.0");
        write_pointer(&service, "1.37.0", None);
        let live = FakeLive::new();
        let roots = ScanRoots {
            service_versions: Some(service),
            ..ScanRoots::default()
        };
        let report = scan(&context(&live, roots, limits()));
        assert_eq!(report.total_count(), 1);
        let item = &report.items()[0];
        assert_eq!(item.plane(), Plane::Service);
        assert!(!item.deletable());

        let outcome = clean_with(std::slice::from_ref(item), &live);
        assert_eq!(outcome.failed_count, 1);
        let reason = outcome.results[0]
            .outcome
            .as_ref()
            .expect_err("服务平面必须拒绝删除");
        assert!(reason.contains("管理员"), "{reason}");
        assert!(old.exists(), "服务平面目录必须原样保留");
    }

    // ---- 清理的复核与失败隔离 ----

    #[test]
    fn clean_refuses_orphan_when_target_reappeared() {
        let temp = TempDir::new("reappear");
        let dl = temp.dir("dl");
        let control = temp.file("dl/movie.mkv.aria2", 32);
        temp.age_file(&control, Duration::from_secs(30 * SECONDS_PER_DAY));
        let live = FakeLive::new();
        let roots = ScanRoots {
            dl_dir: Some(dl),
            ..ScanRoots::default()
        };
        let mut ctx = context(&live, roots, limits());
        ctx.aria2 = Aria2State::Stopped;
        let report = scan(&ctx);
        assert_eq!(report.total_count(), 1);

        // 用户点确认之前，下载续传了：目标文件又出现。
        temp.file("dl/movie.mkv", 64);
        let outcome = clean_with(report.items(), &live);
        assert_eq!(outcome.failed_count, 1);
        let reason = outcome.results[0]
            .outcome
            .as_ref()
            .expect_err("目标回来后必须拒绝");
        assert!(reason.contains("续传"), "{reason}");
        assert!(control.exists());
    }

    #[test]
    fn clean_refuses_version_that_became_active() {
        let temp = TempDir::new("became-active");
        let root = temp.dir("versions");
        install_version(&root, "1.37.0");
        let old = install_version(&root, "1.36.0");
        write_pointer(&root, "1.37.0", None);
        let live = FakeLive::new();
        let report = scan(&context(&live, user_versions_roots(&root), limits()));
        assert_eq!(report.total_count(), 1);

        // 扫描之后用户回滚到 1.36.0：它成了激活版。
        write_pointer(&root, "1.36.0", Some("1.37.0"));
        let outcome = clean_with(report.items(), &live);
        assert_eq!(outcome.failed_count, 1);
        let reason = outcome.results[0]
            .outcome
            .as_ref()
            .expect_err("已成激活版必须拒绝");
        assert!(
            reason.contains("激活版") || reason.contains("上一代"),
            "{reason}"
        );
        assert!(old.exists());
    }

    #[test]
    fn clean_keeps_going_after_a_failure() {
        let temp = TempDir::new("partial");
        let tmp = temp.dir("tmp");
        let first = temp.file("tmp/a.bin", 16);
        let second = temp.file("tmp/b.bin", 16);
        let live = FakeLive::new();
        let roots = ScanRoots {
            user_tmp: Some(tmp),
            ..ScanRoots::default()
        };
        let mut limits = limits();
        limits.tmp_max_age = Duration::ZERO;
        let report = scan(&context(&live, roots, limits));
        assert_eq!(report.total_count(), 2);

        // 手工删掉一个，制造「文件已不存在」的失败项。
        fs::remove_file(&first).expect("删第一个");
        let outcome = clean_with(report.items(), &live);
        assert_eq!(outcome.removed_count, 1);
        assert_eq!(outcome.failed_count, 1);
        assert!(!second.exists(), "后一项不该被前一项的失败拖累");
    }

    // ---- 硬边界 ----

    #[test]
    fn hard_boundaries_are_outside_every_scan_root() {
        let temp = TempDir::new("bounds");
        let appdata = temp.dir("appdata");
        temp.file("appdata/config.toml", 10);
        temp.file("appdata/secret.bin", 10);
        temp.file("appdata/session/aria2.session", 10);
        let logs = temp.dir("appdata/logs");
        temp.file("appdata/logs/ariahost.log.9", 10);
        let live = FakeLive::new();
        let roots = ScanRoots {
            user_logs: Some(logs),
            user_tmp: Some(temp.dir("appdata/tmp")),
            ..ScanRoots::default()
        };
        let report = scan(&context(&live, roots, limits()));

        for item in report.items() {
            let name = item
                .path()
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            assert_ne!(name, "config.toml", "config.toml 绝不能进删除列表");
            assert_ne!(name, "secret.bin", "secret.bin 绝不能进删除列表");
            assert_ne!(name, "aria2.session", "session 绝不能进删除列表");
        }
        assert_eq!(report.total_count(), 1, "只有超份数的日志该被选中");

        let outcome = clean_with(report.items(), &live);
        assert_eq!(outcome.removed_count, 1);
        assert!(appdata.join("config.toml").exists());
        assert!(appdata.join("secret.bin").exists());
        assert!(appdata.join("session").join("aria2.session").exists());
    }

    #[test]
    fn session_directory_is_never_scanned() {
        let temp = TempDir::new("session");
        let session = temp.dir("session");
        let file = temp.file("session/aria2.session", 64);
        temp.age_file(&file, Duration::from_secs(365 * SECONDS_PER_DAY));
        let live = FakeLive::new();
        // 即便把 session 误当 tmp 根传进来，也只会被 TmpFile 规则命中；
        // 真正的保护是「调用方不会这么传」——生产路径用的是 paths::tmp_dir。
        let report = scan(&context(&live, ScanRoots::default(), limits()));
        assert_eq!(report.total_count(), 0);
        assert!(file.exists());
        let _ = session;
    }

    // ---- 报告与性能 ----

    #[test]
    fn report_records_entries_and_elapsed() {
        let temp = TempDir::new("report");
        let tmp = temp.dir("tmp");
        temp.file("tmp/a.bin", 8);
        temp.file("tmp/b.bin", 8);
        let live = FakeLive::new();
        let roots = ScanRoots {
            user_tmp: Some(tmp),
            ..ScanRoots::default()
        };
        let report = scan(&context(&live, roots, limits()));
        assert!(report.entries_scanned() >= 2);
        assert!(!report.budget_exhausted());
        assert!(report.elapsed() < Duration::from_secs(5));
        assert!(report.is_empty());
        assert_eq!(report.total_bytes(), 0);
    }

    #[test]
    fn scan_stays_fast_on_thousands_of_files() {
        let temp = TempDir::new("perf");
        let versions = temp.dir("versions");
        for index in 0..200 {
            install_version(&versions, &format!("1.{}.0", 30 + index));
        }
        write_pointer(&versions, "1.30.0", None);
        let tmp = temp.dir("tmp");
        for index in 0..2000 {
            temp.file(&format!("tmp/blob-{index}.bin"), 64);
        }
        let dl = temp.dir("dl");
        for index in 0..500 {
            let control = temp.file(&format!("dl/file-{index}.mkv.aria2"), 32);
            temp.age_file(&control, Duration::from_secs(30 * SECONDS_PER_DAY));
        }

        let live = FakeLive::new();
        let roots = ScanRoots {
            user_versions: Some(versions),
            user_tmp: Some(tmp),
            dl_dir: Some(dl),
            ..ScanRoots::default()
        };
        let mut limits = limits();
        limits.tmp_max_age = Duration::ZERO;
        let mut ctx = context(&live, roots, limits);
        ctx.aria2 = Aria2State::Stopped;

        let started = Instant::now();
        let report = scan(&ctx);
        let elapsed = started.elapsed();

        // 200 个版本（199 个废弃）+ 2000 个 tmp + 500 个孤儿。
        assert_eq!(report.summary(CleanTarget::AbandonedVersion).count, 199);
        assert_eq!(report.summary(CleanTarget::TmpFile).count, 2000);
        assert_eq!(report.summary(CleanTarget::OrphanControlFile).count, 500);
        println!(
            "SCAN_PERF elapsed={elapsed:?} entries={} items={} bytes={}",
            report.entries_scanned(),
            report.total_count(),
            report.total_bytes()
        );
        // 宽松上限：真实耗时在报告里给，这里只防「退化成几秒」。
        assert!(elapsed < Duration::from_secs(3), "扫描过慢：{elapsed:?}");
    }

    /// 手工性能探针：拿一个真实大目录当下载目录扫一遍。
    ///
    /// ```text
    /// ARIAHOST_CLEAN_PROBE_DIR=/some/big/dir \
    ///   cargo test -p ariahost-core perf_probe_on_real_directory -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "手工性能探针，需要 ARIAHOST_CLEAN_PROBE_DIR"]
    fn perf_probe_on_real_directory() {
        let Ok(dir) = std::env::var("ARIAHOST_CLEAN_PROBE_DIR") else {
            println!("SCAN_PROBE 跳过：没有 ARIAHOST_CLEAN_PROBE_DIR");
            return;
        };
        let live = RealLiveState;
        let ctx = ScanContext {
            limits: limits(),
            roots: ScanRoots {
                dl_dir: Some(PathBuf::from(&dir)),
                ..ScanRoots::default()
            },
            aria2: Aria2State::Stopped,
            active_paths: BTreeSet::new(),
            live: &live,
        };
        let started = Instant::now();
        let report = scan(&ctx);
        let elapsed = started.elapsed();
        println!(
            "SCAN_PROBE dir={dir} elapsed={elapsed:?} entries={} items={} skipped={} truncated={}",
            report.entries_scanned(),
            report.total_count(),
            report.skipped().len(),
            report.budget_exhausted()
        );
    }

    #[test]
    fn parsed_helpers_are_pinned() {
        // 轮转序号**只有一份实现**（`arihost_common::logfile`）：本断言钉的是那一份。
        // 语义与 M2-6 初版逐字相同（按最后一个 `.log.` 切，后缀非空纯数字），
        // 换实现不换断言——删掉的只是副本，不是「语义没变」的证明。
        use ariahost_common::logfile::rotated_index as rotation_index;
        assert_eq!(rotation_index("ariahost.log.3"), Some(3));
        assert_eq!(rotation_index("ariahost.log"), None);
        assert_eq!(rotation_index("ariahost.log.abc"), None);
        assert_eq!(rotation_index("ariahost.log."), None);
        assert_eq!(staging_pid(".staging-1.38.0-4242-0"), Some(4242));
        assert_eq!(staging_pid(".staging-broken"), None);
        assert!(is_orphan_control(Path::new("a/movie.mkv.aria2")));
        assert!(is_orphan_control(Path::new("a/movie.MKV.ARIA2")));
        assert!(!is_orphan_control(Path::new("a/movie.mkv")));
        assert!(!is_orphan_control(Path::new("a/aria2.session")));
    }
}
