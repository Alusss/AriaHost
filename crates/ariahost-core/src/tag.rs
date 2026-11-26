//! 手工指定 tag 取核心（M5-b-G）：**不拉 GitHub API**，只按 tag 拼 release 下载 URL。
//!
//! # 为什么手工 tag 是一条独立路径
//!
//! 「拉 releases 列表 → 选最新」那条路（[`crate::fetch`] + [`crate::release`]）要过
//! `api.github.com`，在部分网络下会 401/403/限流，且**列表接口本身**就是被墙的重灾区。
//! 用户手上有具体版本号（例如 `1.37.0`）时，只需要一个**确定的下载地址**：
//!
//! ```text
//! https://github.com/aria2/aria2/releases/download/release-<ver>/aria2-<ver>-win-<bits>bit-build<N>.zip
//! ```
//!
//! 这条路径**一个 API 请求都不发**。代价是：GitHub 不提供哈希、也不提供本架构 build 号，
//! 所以要么用已知交叉参考值硬校验、要么如实标注「未校验」，并且 build 号必须挨个试。
//!
//! # tag 是安全边界，不是普通输入
//!
//! 归一化后的 tag 同时进两处：
//! 1. 下载 URL 的路径段；
//! 2. 版本库目录名 `versions/<ver>/`。
//!
//! 所以 [`normalize_tag`] 严格到「只接受 `^\d+\.\d+\.\d+$`」：`../etc/passwd`、`1.37.0/x`、
//! 含空白、非 ASCII、超长一律拒绝，**且非法输入绝不发起任何网络请求**（[`fetch_by_tag`]
//! 第一行就归一化，失败即返回）。
//!
//! # G5 拒装流水线（顺序固定）
//!
//! | 步 | 检查 | 不通过时 |
//! |---|---|---|
//! | 0 | build 探测（HEAD，仅 404 继续下一个 build） | 网络类/HTTP 类错误 / 专门错误「本架构无包」 |
//! | 1 | 魔数 `PK\x03\x04` + `Content-Type` 预检 | [`CoreError::BadPayload`]（**不给换代理 hint**） |
//! | 2 | SHA-256：有已知值就硬校验 | [`CoreError::PackageInvalid`]（装库之前失败） |
//! | 3 | 解压取 `aria2c.exe` | [`CoreError::PackageInvalid`] |
//! | 4 | 跑 `aria2c --version`，必须等于 tag | [`CoreError::VersionMismatch`] |
//! | 5 | [`VersionStore::install_core`]（原子） | 版本库 IO |
//! | 6 | [`VersionStore::activate`] | 指针不写，前 5 步的产物保留但不激活 |
//!
//! 1–5 全部通过才 `activate`：**装上一个跑不起来的核心比不装更糟**。
//!
//! # 与 `asset_for` 的分工
//!
//! [`crate::fetch::Fetcher::asset_for`] 走 releases API 目录，本批**禁止**用它取 tag
//! （手工 tag 存在的意义就是不碰 API）。这里自己拼名字，取自同一套白名单规则
//! （[`crate::release::match_win_asset`] 的构造口径）。
//!
//! # 平台说明
//!
//! 产品只发 Windows；**Linux 构建仅在开发期用于跑门禁**（fmt / check / clippy / test）。
//! 所以 G5 第 4 步（跑 `aria2c --version`）在 Linux 上必然失败——下到的是 Windows PE，
//! 执行会得到 `Permission denied`。那是平台事实，不是本模块的缺陷；对应的失败 hint
//! **不为 Linux 改口径**（用户只会在 Windows 上遇到）。

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ariahost_aria2::{parse_version_line, parse_version_number, Aria2Version};
use ariahost_common::config::{Config, CoreProxyMode};
use ariahost_common::paths::{versions_dir, Plane};
use serde::Serialize;

use crate::error::CoreError;
use crate::fetch::{sha256_hex, verify_hash, Fetcher, HashState};
use crate::http::ZIP_MAGIC;
use crate::net::{github_via, HttpProber, ProbeTransport, Prober, PROBE_TIMEOUT};
use crate::release::host_bits;
use crate::runtime;
use crate::store::{ActivePointer, Provenance, VersionStore};
use crate::zip_extract::{extract_core, ExtractedCore};
use crate::CORE_EXE;

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

/// GitHub 的 aria2 releases 页面（**后端不自己开浏览器**，只把地址交给 UI/CLI）。
pub const ARIA2_RELEASES_PAGE: &str = "https://github.com/aria2/aria2/releases";

/// release 下载地址前缀（后面接 `release-<ver>/<资产名>`）。
pub const DOWNLOAD_URL_BASE: &str = "https://github.com/aria2/aria2/releases/download/";

/// GitHub 上 aria2 的 tag 前缀（`release-1.37.0`）。
///
/// 归一化后的版本号进**资产名与目录名**，这个前缀只在拼下载 URL 时补上：
/// 真实 tag 是 `release-1.37.0`，资产名却是 `aria2-1.37.0-win-64bit-build1.zip`。
pub const RELEASE_TAG_PREFIX: &str = "release-";

/// 手工 tag 的字节上限（`trim` 之后）。
///
/// 32 字节对 `^\d+\.\d+\.\d+$` 是极宽松的上限，保留它只为给「粘贴了一整行日志」一个
/// 可读的拒绝原因，而不是让后续正则去处理任意长的字符串。
pub const MAX_TAG_BYTES: usize = 32;

/// 最多试几个 build（`build1` → `build2` → `build3`）。
///
/// 为什么是 3：aria2 的 Windows 包历史上最多到 `build3`（1.23.0 只有 `build2`）；
/// 试 4 个以上只会在「tag 打错」时多打几个 404，没有收益。
pub const MAX_BUILD: u32 = 3;

/// 版本探测（`aria2c --version`）的默认超时。
///
/// 实测 `aria2c --version` 在冷启动下约 20–60 ms；5 s 是「盘不快/杀软在扫」的上限，
/// 超时即视为「这个 exe 不可用」——**绝不允许 list_cores 被坏 exe 挂死**。
pub const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// 探测不出内置核心版本时的占位值（UI 显示「版本未知」，不是编造一个数字）。
pub const UNKNOWN_VERSION: &str = "unknown";

/// 校验临时目录前缀（跑 `aria2c --version` 前必须先落盘）。
///
/// 以 `.` 开头，且不是合法版本号：`VersionStore::list` 只接受能解析成三段数字的目录名，
/// 所以残留的验证目录不会混进「已安装版本」。
const VERIFY_PREFIX: &str = ".verify-";

/// 生成互不相同的验证目录名。
static VERIFY_SEQ: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// 1) tag 归一化与严格校验
// ---------------------------------------------------------------------------

/// tag 校验失败的原因（**每一种非法形态都有自己的变体**，便于单测与用户提示）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TagError {
    /// 空串（或只有空白）。
    #[error("tag 为空")]
    Empty,
    /// 超过 [`MAX_TAG_BYTES`]。
    #[error("tag 过长：{bytes} 字节（上限 {MAX_TAG_BYTES} 字节）")]
    TooLong {
        /// 实际字节数。
        bytes: usize,
    },
    /// 含非 ASCII 字符（非预期的 tag 一律拒绝，不做「宽容解码」）。
    #[error("tag 含非 ASCII 字符：{value}")]
    NonAscii {
        /// 原始输入（已 trim）。
        value: String,
    },
    /// 含路径分隔符或上跳段——会进 URL 与版本目录名，路径穿越的真实入口。
    #[error("tag 含路径分隔符或上跳段（会进下载 URL 与版本目录）")]
    PathTraversal,
    /// 含空白（内部空格；两端空白在归一化时已被 `trim` 去掉）。
    #[error("tag 含空白字符：{value}")]
    Whitespace {
        /// 原始输入（已 trim）。
        value: String,
    },
    /// 剥掉 `release-` 之后不是三段数字。
    #[error("tag 不是三段数字版本号（如 1.37.0）：{value}")]
    BadShape {
        /// 原始输入（已 trim）。
        value: String,
    },
}

impl TagError {
    /// 对应的既有错误码：tag 来自命令行/用户输入，归用法错误。
    pub const fn code(&self) -> ariahost_common::ErrorCode {
        ariahost_common::ErrorCode::Usage
    }
}

impl From<TagError> for CoreError {
    fn from(error: TagError) -> Self {
        Self::BadTag {
            reason: error.to_string(),
        }
    }
}

/// 手工 tag → 规范版本号字符串。
///
/// 规则（顺序即实现顺序）：
/// 1. `trim`（两端空白是粘贴噪音，接受）；
/// 2. 空 / ≥ [`MAX_TAG_BYTES`] / 非 ASCII → 拒绝；
/// 3. 含 `/`、`\`、`..` → [`TagError::PathTraversal`]（**这是安全边界**）；
/// 4. 含空白 → [`TagError::Whitespace`]；
/// 5. 剥**一次** `release-` 前缀；
/// 6. 剩余必须完全匹配 `^\d+\.\d+\.\d+$`，否则 [`TagError::BadShape`]。
///
/// 归一化产物是**规范形式**（`01.37.0` → `1.37.0`）：它既进资产名、也进目录名，
/// 两种写法必须是同一个版本目录，否则同一版本会装两份。
pub fn normalize_tag(input: &str) -> Result<String, TagError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(TagError::Empty);
    }
    let bytes = trimmed.len();
    if bytes >= MAX_TAG_BYTES {
        return Err(TagError::TooLong { bytes });
    }
    if !trimmed.is_ascii() {
        return Err(TagError::NonAscii {
            value: trimmed.to_owned(),
        });
    }
    if trimmed.contains('/') || trimmed.contains('\\') || trimmed.contains("..") {
        return Err(TagError::PathTraversal);
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Err(TagError::Whitespace {
            value: trimmed.to_owned(),
        });
    }
    // 只剥一次：`release-release-1.37.0` 会落到 BadShape（不递归剥，避免猜用户意图）。
    let stripped = trimmed.strip_prefix(RELEASE_TAG_PREFIX).unwrap_or(trimmed);
    match parse_strict_version(stripped) {
        Some(version) => Ok(version.to_string()),
        None => Err(TagError::BadShape {
            value: trimmed.to_owned(),
        }),
    }
}

/// 严格三段数字解析：**完全匹配** `^\d+\.\d+\.\d+$`（比 `parse_version_number` 更严：
/// 后者会 `trim` 输入，这里要求调用方已经保证没有空白）。
fn parse_strict_version(value: &str) -> Option<Aria2Version> {
    let mut parts = value.split('.');
    let major = parse_digits(parts.next()?)?;
    let minor = parse_digits(parts.next()?)?;
    let patch = parse_digits(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    Some(Aria2Version::new(major, minor, patch))
}

/// 一段必须是**纯 ASCII 数字**（空段、符号、Unicode 数字都不接受）。
fn parse_digits(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse::<u32>().ok()
}

/// 归一化后的版本号 → GitHub tag（`1.37.0` → `release-1.37.0`）。
fn release_tag_of(version: &str) -> String {
    format!("{RELEASE_TAG_PREFIX}{version}")
}

// ---------------------------------------------------------------------------
// 2) `aria2c --version` 探测（可注入，单测不真跑进程）
// ---------------------------------------------------------------------------

/// 版本探测器：跑一次 `aria2c --version`，返回**规范版本号字符串**。
///
/// 为什么是 trait 而不是直接 `Command`：G5 第 4 步与 [`list_cores`] 都要用它，
/// 而单测里造一个「能跑的 exe」既慢又依赖平台；注入假实现即可覆盖
/// 「版本不符 → 拒装」与「探测失败 → 不可选」两条分支。
pub trait CoreVersionProbe {
    /// 探测失败时的原因（只用于展示/日志，不影响错误分类）。
    fn probe_version(&self, exe: &Path) -> Result<String, String>;
}

/// 生产实现：真跑 `aria2c --version`，解析首行。
#[derive(Debug, Clone, Copy)]
pub struct ProcessVersionProbe {
    /// 超时上限（默认 [`VERSION_PROBE_TIMEOUT`]）。
    pub timeout: Duration,
}

impl Default for ProcessVersionProbe {
    fn default() -> Self {
        Self {
            timeout: VERSION_PROBE_TIMEOUT,
        }
    }
}

impl CoreVersionProbe for ProcessVersionProbe {
    fn probe_version(&self, exe: &Path) -> Result<String, String> {
        let first = run_exe_first_line(exe, "--version", self.timeout)?;
        let version = parse_version_line(&first)
            .map_err(|error| format!("`{} --version` 首行无法解析：{error}", exe.display()))?;
        Ok(version.to_string())
    }
}

/// 跑一个 exe 并取 stdout 首行（带硬超时；超时即 kill）。
///
/// 为什么不用 `Command::output()`：它**没有超时**。一个卡死的 exe 会让整个
/// 「核心列表」界面永久转圈——这比「探测失败」糟得多。
fn run_exe_first_line(exe: &Path, arg: &str, timeout: Duration) -> Result<String, String> {
    use std::process::{Command, Stdio};

    if !exe.is_file() {
        return Err(format!("文件不存在：{}", exe.display()));
    }
    let mut command = Command::new(exe);
    command
        .arg(arg)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        // 不弹黑框：面板与 list_cores 都在 GUI 进程里跑。
        command.creation_flags(runtime::CREATE_NO_WINDOW);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("启动 {} 失败：{error}", exe.display()))?;

    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "{} {arg} 在 {} ms 内没有退出",
                        exe.display(),
                        timeout.as_millis()
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                let _ = child.kill();
                return Err(format!("查询 {} 状态失败：{error}", exe.display()));
            }
        }
    }

    let output = child
        .wait_with_output()
        .map_err(|error| format!("读取 {} 输出失败：{error}", exe.display()))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Some(first) = stdout.lines().map(str::trim).find(|line| !line.is_empty()) {
        return Ok(first.to_owned());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    Err(if stderr.is_empty() {
        format!("{} {arg} 没有任何输出", exe.display())
    } else {
        format!("{} {arg} 的 stdout 为空（stderr：{stderr}）", exe.display())
    })
}

// ---------------------------------------------------------------------------
// 3) build-N 资产探测
// ---------------------------------------------------------------------------

/// 实际走的是哪条线路（**如实标注**，不把「代理」说成「直连」）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreLineKind {
    /// 直连 GitHub。
    Direct,
    /// 前缀代理：`base + 完整 GitHub URL`。
    Proxy {
        /// 前缀基址（规范化后的，末尾带 `/`）。
        base: String,
    },
}

impl CoreLineKind {
    /// 稳定文本（日志/JSON 用，不要解析中文）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Proxy { .. } => "proxy",
        }
    }

    /// 人读描述：`direct` / `proxy(https://ghproxy.net/)`。
    pub fn describe(&self) -> String {
        match self {
            Self::Direct => "direct".to_owned(),
            Self::Proxy { base } => format!("proxy({base})"),
        }
    }
}

/// 一次 build 探测的结果分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetAttemptOutcome {
    /// HTTP 404：这个 build 不存在 → 继续下一个。
    Missing,
    /// 2xx/3xx：命中。
    Hit,
    /// 其它非 404 状态码 → **立即停止**（不拿下一个 build 掩盖问题）。
    HttpError,
    /// 传输层失败（超时 / 连接 / TLS）→ 立即停止。
    TransportError,
}

impl AssetAttemptOutcome {
    /// 稳定文本。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::Hit => "hit",
            Self::HttpError => "http-error",
            Self::TransportError => "transport-error",
        }
    }
}

/// 一个 build 的探测记录（自检要逐个打印，**失败时也保留**）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetAttempt {
    /// build 号（1 起）。
    pub build: u32,
    /// 资产名（`aria2-1.37.0-win-64bit-build1.zip`）。
    pub asset_name: String,
    /// HTTP 状态码（传输失败时为 `None`）。
    pub status: Option<u16>,
    /// 结果分类。
    pub outcome: AssetAttemptOutcome,
    /// 诊断原文。
    pub detail: String,
}

/// 探测命中的资产。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagAsset {
    /// 规范化版本号（`1.37.0`）：进资产名与版本目录名。
    pub tag: String,
    /// GitHub 上的 tag（`release-1.37.0`）：进下载 URL 的路径段。
    pub release_tag: String,
    /// 位宽（本机目标架构）。
    pub bits: u32,
    /// 命中的 build 号。
    pub build: u32,
    /// 实际请求的 URL（**已按线路改写**：直连原样，代理则是 `base + github_url`）。
    pub url: String,
    /// 未改写的 GitHub 原始 URL（诊断/日志用）。
    pub github_url: String,
    /// 走了哪条线路。
    pub line: CoreLineKind,
    /// 逐个 build 的探测记录（含命中前那些 404）。
    pub attempts: Vec<AssetAttempt>,
}

/// 探测该 tag 在本机架构下的发布包（`Fetcher` 版本：用它的线路设置）。
pub fn probe_tag_asset(fetcher: &Fetcher, tag: &str) -> Result<TagAsset, CoreError> {
    let prober = HttpProber::with_client(fetcher.client().clone());
    let mut attempts = Vec::new();
    probe_tag_asset_tracked(
        &prober,
        fetcher.core_proxy_mode(),
        fetcher.core_proxy_url(),
        tag,
        &mut attempts,
    )
}

/// 同 [`probe_tag_asset`]，但把逐个 build 的探测记录**带出来**（自检入口用）。
pub fn probe_tag_asset_tracked<P: Prober>(
    prober: &P,
    mode: CoreProxyMode,
    proxy_url: &str,
    tag: &str,
    attempts: &mut Vec<AssetAttempt>,
) -> Result<TagAsset, CoreError> {
    attempts.clear();
    let version = normalize_tag(tag)?;
    let bits = host_bits();
    let release_tag = release_tag_of(&version);
    let line = match mode {
        CoreProxyMode::Direct => CoreLineKind::Direct,
        CoreProxyMode::Proxy => CoreLineKind::Proxy {
            base: proxy_url.trim().to_owned(),
        },
    };

    for build in 1..=MAX_BUILD {
        let asset_name = format!("aria2-{version}-win-{bits}bit-build{build}.zip");
        let github_url = format!("{DOWNLOAD_URL_BASE}{release_tag}/{asset_name}");
        let url = github_via(mode, proxy_url, &github_url);

        match prober.head(&url, PROBE_TIMEOUT) {
            Ok(status) if (200..400).contains(&status) => {
                attempts.push(AssetAttempt {
                    build,
                    asset_name,
                    status: Some(status),
                    outcome: AssetAttemptOutcome::Hit,
                    detail: format!("HTTP {status}"),
                });
                return Ok(TagAsset {
                    tag: version,
                    release_tag,
                    bits,
                    build,
                    url,
                    github_url,
                    line,
                    attempts: attempts.clone(),
                });
            }
            Ok(404) => {
                attempts.push(AssetAttempt {
                    build,
                    asset_name,
                    status: Some(404),
                    outcome: AssetAttemptOutcome::Missing,
                    detail: "HTTP 404（该 build 不存在，继续下一个）".to_owned(),
                });
                continue;
            }
            Ok(status) => {
                attempts.push(AssetAttempt {
                    build,
                    asset_name,
                    status: Some(status),
                    outcome: AssetAttemptOutcome::HttpError,
                    detail: format!("HTTP {status}（非 404：立即停止，不试下一个 build）"),
                });
                let error = if status == 429 {
                    CoreError::RateLimited {
                        detail: format!("探测 build{build} 时 GitHub 返回 429"),
                    }
                } else {
                    CoreError::HttpStatus { status }
                };
                return Err(error);
            }
            Err(transport) => {
                let (detail, detail_text) = match &transport {
                    ProbeTransport::Timeout(detail) => {
                        ("传输超时（立即停止，不试下一个 build）", detail)
                    }
                    ProbeTransport::Unreachable(detail) => {
                        ("连接失败（立即停止，不试下一个 build）", detail)
                    }
                };
                attempts.push(AssetAttempt {
                    build,
                    asset_name,
                    status: None,
                    outcome: AssetAttemptOutcome::TransportError,
                    detail: format!("{detail}：{detail_text}"),
                });
                return Err(CoreError::network(format!(
                    "探测 build{build} 的下载地址失败（{} 线路）：{detail_text}",
                    line.as_str()
                )));
            }
        }
    }

    // 三个 build 全 404：**不是网络错误**，是这个 tag 下确实没有本架构的包。
    Err(CoreError::NoReleaseAsset {
        tag: version,
        bits,
        builds: MAX_BUILD,
    })
}

// ---------------------------------------------------------------------------
// 4) G5 流水线
// ---------------------------------------------------------------------------

/// G5 的一步结论（自检逐条打印；生产调用方可用于日志）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct G5Step {
    /// 稳定步骤名（`1-magic` / `2-sha256` / …）。
    pub name: &'static str,
    /// 是否通过。
    pub ok: bool,
    /// 结论原文。
    pub detail: String,
}

impl G5Step {
    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            ok: true,
            detail: detail.into(),
        }
    }
}

/// 一次「按 tag 取核心」的成功结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagInstall {
    /// 装好并激活的版本（规范形式）。
    pub version: String,
    /// GitHub tag。
    pub release_tag: String,
    /// 位宽。
    pub bits: u32,
    /// 命中的 build 号。
    pub build: u32,
    /// 实际下载 URL（已按线路改写）。
    pub url: String,
    /// 用了哪条线路。
    pub line: CoreLineKind,
    /// 哈希状态（`Unverified` 时**不得声称已校验**）。
    pub hash: HashState,
    /// 实测版本（`aria2c --version`）。
    pub probed_version: String,
    /// 告警（未校验哈希、配置告警等，必须展示）。
    pub warnings: Vec<String>,
    /// 装好后的 exe 路径。
    pub exe_path: PathBuf,
    /// 写盘后的激活指针。
    pub pointer: ActivePointer,
    /// 逐步结论。
    pub steps: Vec<G5Step>,
}

/// 按 tag 取核心并安装激活（生产入口：真跑 `aria2c --version`）。
pub fn fetch_by_tag(
    fetcher: &Fetcher,
    store: &VersionStore,
    tag: &str,
) -> Result<TagInstall, CoreError> {
    let mut steps = Vec::new();
    fetch_by_tag_tracked(
        fetcher,
        store,
        tag,
        &ProcessVersionProbe::default(),
        &mut steps,
    )
}

/// 同 [`fetch_by_tag`]，但注入版本探测器并带出逐步结论（单测 + 自检入口）。
pub fn fetch_by_tag_tracked(
    fetcher: &Fetcher,
    store: &VersionStore,
    tag: &str,
    probe: &dyn CoreVersionProbe,
    steps: &mut Vec<G5Step>,
) -> Result<TagInstall, CoreError> {
    steps.clear();

    // 安全边界在最前面：非法 tag 一个网络请求都不发。
    let version = normalize_tag(tag)?;
    let version_number = parse_version_number(&version).map_err(|_| CoreError::BadTag {
        reason: format!("归一化后仍然不是合法版本号：{version}"),
    })?;
    let bits = host_bits();
    let mut warnings = fetcher.line_warnings().to_vec();

    // 步 0：build 探测。
    let mut attempts = Vec::new();
    let asset = probe_tag_asset_tracked(
        &HttpProber::with_client(fetcher.client().clone()),
        fetcher.core_proxy_mode(),
        fetcher.core_proxy_url(),
        &version,
        &mut attempts,
    )?;
    steps.push(G5Step::pass(
        "0-asset",
        format!(
            "build{} 命中（{} 线路）：{}",
            asset.build,
            asset.line.describe(),
            asset.url
        ),
    ));

    // 步 1：载荷预检（魔数 + Content-Type）。**这里不用 fetch_zip**：它把魔数不符归
    // `PackageInvalid`，而本批要求归 `BadPayload`（与线路无关，不给「换代理」hint）。
    let reply = fetcher.client().get(&asset.url)?;
    if !reply.is_success() {
        return Err(reply.failure());
    }
    let content_type = reply.content_type.clone().unwrap_or_default();
    if content_type.to_ascii_lowercase().contains("html") {
        return Err(CoreError::BadPayload {
            detail: format!("Content-Type 是 {content_type}：线路返回的是网页而不是 zip"),
        });
    }
    if let Some(length) = reply.content_length {
        if length as usize != reply.body.len() {
            return Err(CoreError::package(format!(
                "Content-Length 声明 {length} 字节，实收 {} 字节",
                reply.body.len()
            )));
        }
    }
    let bytes = reply.body;
    if !bytes.starts_with(ZIP_MAGIC) {
        let head: Vec<u8> = bytes.iter().copied().take(8).collect();
        return Err(CoreError::BadPayload {
            detail: format!(
                "下载内容不是 zip：magic 应为 PK\\x03\\x04，实际前 8 字节 {head:02x?}（Content-Type：{content_type}）"
            ),
        });
    }
    steps.push(G5Step::pass(
        "1-magic",
        format!(
            "PK\\x03\\x04 通过，Content-Type={}，{} 字节",
            if content_type.is_empty() {
                "（缺失）"
            } else {
                content_type.as_str()
            },
            bytes.len()
        ),
    ));

    // 步 2：SHA-256（有已知参考值就硬校验，没有则如实标注「未校验」）。
    let (hash, hash_warnings) = verify_hash(version_number, bits, &bytes)?;
    warnings.extend(hash_warnings);
    let hash_detail = match &hash {
        HashState::Verified { sha256 } => format!("已校验：sha256={sha256}"),
        HashState::Unverified { reason } => reason.clone(),
    };
    steps.push(G5Step::pass("2-sha256", hash_detail));

    // 步 3：解压取 aria2c.exe。
    let extracted = extract_core(&bytes)?;
    steps.push(G5Step::pass(
        "3-extract",
        format!(
            "aria2c.exe {} 字节，随包 DLL {} 个，跳过条目 {} 个",
            extracted.aria2c.len(),
            extracted.dlls.len(),
            extracted.skipped.len()
        ),
    ));

    // 步 4：跑 `aria2c --version`，必须等于归一化 tag（防包被替换 / build 名对不上）。
    let probed_version = probe_staged_core(store, &version, &extracted, probe)?;
    if probed_version != version {
        return Err(CoreError::VersionMismatch {
            expected: version.clone(),
            actual: probed_version,
        });
    }
    steps.push(G5Step::pass(
        "4-version",
        format!("`aria2c --version` = {probed_version}（与 tag 一致）"),
    ));

    // 步 5：原子装库。
    let installed = store.install_core(&version, &extracted)?;
    steps.push(G5Step::pass(
        "5-install",
        format!("已装到 {}", installed.display()),
    ));

    // 安装证据（provenance）：让 `list_cores` 能回答「这份 exe 与安装时是否逐字节一致」。
    //
    // 写失败**不让安装失败**：核心已经装好且验证过，证据文件只是旁证；缺了它的后果是
    // `hash_verified` 显示 false（可读告警会说出来），不是核心不可用。
    let provenance = Provenance {
        asset_name: format!("aria2-{version}-win-{bits}bit-build{}.zip", asset.build),
        line: asset.line.as_str().to_owned(),
        zip_sha256: sha256_hex(&bytes),
        exe_sha256: sha256_hex(&extracted.aria2c),
        known_sha256_verified: hash.is_verified(),
        installed_at: ariahost_common::time::utc_stamp(std::time::SystemTime::now()),
    };
    if let Err(error) = store.write_provenance(&version, &provenance) {
        warnings.push(format!(
            "写安装证据失败（hash_verified 会显示 false）：{error}"
        ));
    }

    // 步 6：只有 1–5 全过才激活。
    let pointer = store.activate(&version)?;
    steps.push(G5Step::pass(
        "6-activate",
        format!(
            "active={} previous={}",
            pointer.active,
            pointer.previous.as_deref().unwrap_or("（无）")
        ),
    ));

    Ok(TagInstall {
        version: version.clone(),
        release_tag: asset.release_tag,
        bits,
        build: asset.build,
        url: asset.url,
        line: asset.line,
        hash,
        probed_version,
        warnings,
        exe_path: store.exe_path(&version),
        pointer,
        steps: steps.clone(),
    })
}

/// 把解压结果临时落盘，跑 `aria2c --version`，然后删掉临时目录。
///
/// 为什么落盘而不是「装好再验」：装好再验一旦版本不符就得回滚版本目录，而
/// [`VersionStore::install_core`] 对已存在的完整版本是幂等的（不会覆盖）——那时
/// 验到的可能是**上一次安装的旧 exe**，完全失去意义。临时目录是唯一能「先验真、
/// 再安装」的做法。
fn probe_staged_core(
    store: &VersionStore,
    version: &str,
    extracted: &ExtractedCore,
    probe: &dyn CoreVersionProbe,
) -> Result<String, CoreError> {
    let sequence = VERIFY_SEQ.fetch_add(1, Ordering::Relaxed);
    let staging = store.root().join(format!(
        "{VERIFY_PREFIX}{version}-{}-{sequence}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|error| {
        CoreError::store_io(format!("建验证目录 {} 失败：{error}", staging.display()))
    })?;

    let write = || -> Result<(), CoreError> {
        write_bytes(&staging.join(CORE_EXE), &extracted.aria2c)?;
        for (name, bytes) in &extracted.dlls {
            write_bytes(&staging.join(name), bytes)?;
        }
        Ok(())
    };
    let result = write().and_then(|()| {
        probe
            .probe_version(&staging.join(CORE_EXE))
            .map_err(|detail| {
                CoreError::package(format!("下载的 aria2c.exe 无法验证版本：{detail}"))
            })
    });
    let _ = fs::remove_dir_all(&staging);
    result
}

/// 写一个文件并落盘（与 `store` 的暂存写入口径一致）。
fn write_bytes(path: &Path, bytes: &[u8]) -> Result<(), CoreError> {
    let mut file = fs::File::create(path)
        .map_err(|error| CoreError::store_io(format!("创建 {} 失败：{error}", path.display())))?;
    file.write_all(bytes)
        .map_err(|error| CoreError::store_io(format!("写入 {} 失败：{error}", path.display())))?;
    file.sync_all()
        .map_err(|error| CoreError::store_io(format!("落盘 {} 失败：{error}", path.display())))
}

// ---------------------------------------------------------------------------
// 5) `list_cores`：给 UI 的核心列表（UI 不自己拼逻辑）
// ---------------------------------------------------------------------------

/// 核心条目来源（**只有三类**；外部路径 `aria2.external_path` 不属于「核心库」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CoreEntryOrigin {
    /// exe 同目录的内置 `aria2c.exe`。
    Bundled,
    /// 版本库 `versions/<ver>/` 下已验真的目录。
    Managed,
    /// 代码里的已知版本（当前只有 1.37.0），尚未安装。
    Known,
}

impl CoreEntryOrigin {
    /// 稳定文本（UI 契约；不要解析中文）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bundled => "bundled",
            Self::Managed => "managed",
            Self::Known => "known",
        }
    }
}

/// 一条核心条目（UI 直接渲染；**不存在的版本不会出现在列表里**）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CoreEntry {
    /// 版本号（`bundled` 条目取实测版本；探测失败时为 [`UNKNOWN_VERSION`]）。
    pub version: String,
    /// 位宽（本机目标架构）。
    pub bits: u32,
    /// 来源。
    pub origin: CoreEntryOrigin,
    /// 磁盘上的 exe 与**安装时**记录的 `exe_sha256` 是否逐字节一致。
    ///
    /// 语义 = 「与安装时一致，防安装后篡改 / 磁盘损坏」，**不是上游签名**。
    /// 判据是 `versions/<ver>/provenance.json`：有且哈希一致 → `true`。
    /// **无 provenance**（bundled / external / 本功能上线前的旧安装）→ `false`。
    pub hash_verified: bool,
    /// 安装时是否用已知交叉参考值硬校验过**官方发布包**（provenance 的
    /// `known_sha256_verified`）。
    ///
    /// 与 [`CoreEntry::hash_verified`] 是两件事，UI 要分开显示：
    /// - `zip_verified = true`：安装那一刻对过官方 zip 的 SHA-256（强证据）；
    /// - 两者都 `false`：既没对过官方哈希，也没安装证据（只能靠版本探测）。
    pub zip_verified: bool,
    /// `aria2c --version` 实测版本（探测失败时 `None`）。
    pub probed_version: Option<String>,
    /// 可执行文件路径（`known` 未安装时为 `None`）。
    pub path: Option<PathBuf>,
    /// 当前实际会被选中的那条（**以 `resolve_core` 的真实结果为准**，不是配置猜测）。
    pub active: bool,
    /// 能否被选来用：文件在且探测成功，或（`known`）可以下载安装。
    pub selectable: bool,
}

/// 列核心（生产入口）：`plane` 决定版本库根（user/service 天然各一份）。
///
/// **注意成本**：每个条目都会真跑一次 `aria2c --version`（带 [`VERSION_PROBE_TIMEOUT`]
/// 硬超时）。UI 应在打开「核心」面板时调用一次并缓存，**不要每帧调用**。
pub fn list_cores(config: &Config, plane: Plane) -> Result<Vec<CoreEntry>, CoreError> {
    let versions_root = versions_dir(plane);
    let bundled = runtime::bundled_core_path();
    list_cores_in(
        config,
        versions_root.as_deref(),
        bundled.as_deref(),
        &ProcessVersionProbe::default(),
    )
}

/// 列核心（全部来源显式注入；单测入口）。
pub fn list_cores_in(
    config: &Config,
    versions_root: Option<&Path>,
    bundled: Option<&Path>,
    probe: &dyn CoreVersionProbe,
) -> Result<Vec<CoreEntry>, CoreError> {
    let bits = host_bits();
    let mut entries: Vec<CoreEntry> = Vec::new();

    // `active` 以真实解析结果为准：用户看到「当前使用」的那条，必须就是运行时会用的那条。
    let active_path = runtime::resolve_core_with(config, versions_root, bundled)
        .ok()
        .map(|core| core.path);

    // 1) bundled：exe 同目录。**不在版本库里 → 没有 provenance**，两个校验字段都如实给 false。
    if let Some(path) = bundled.filter(|candidate| candidate.is_file()) {
        let probed = probe.probe_version(path).ok();
        let version = probed.clone().unwrap_or_else(|| UNKNOWN_VERSION.to_owned());
        entries.push(CoreEntry {
            version,
            bits,
            origin: CoreEntryOrigin::Bundled,
            hash_verified: false,
            zip_verified: false,
            probed_version: probed.clone(),
            path: Some(path.to_path_buf()),
            active: active_path.as_deref() == Some(path),
            selectable: probed.is_some(),
        });
    }

    // 2) managed：版本库（`VersionStore::list` 已过滤掉 staging / 非版本名 / 没有 exe 的目录）。
    if let Some(root) = versions_root {
        let store = VersionStore::new(root);
        for version in store.list()? {
            let exe = store.exe_path(&version);
            let probed = probe.probe_version(&exe).ok();
            let (hash_verified, zip_verified) = provenance_state(&store, &version, &exe);
            entries.push(CoreEntry {
                version,
                bits,
                origin: CoreEntryOrigin::Managed,
                hash_verified,
                zip_verified,
                probed_version: probed.clone(),
                path: Some(exe.clone()),
                active: active_path.as_deref() == Some(exe.as_path()),
                selectable: probed.is_some(),
            });
        }
    }

    // 3) known：代码里的已知版本（**版本库已装才不列**；已装的那份由 managed 条目代表）。
    //
    // 为什么 bundled 不抑制 known：bundled 是 exe 旁的另一份文件，不占版本库；
    // known 条目的语义是「可以从 GitHub 拉到 versions/」，两者不是同一条。
    for (version, known_bits, _sha256) in crate::fetch::KNOWN_SHA256 {
        if *known_bits != bits {
            continue;
        }
        if entries
            .iter()
            .any(|entry| entry.origin == CoreEntryOrigin::Managed && entry.version == *version)
        {
            continue;
        }
        entries.push(CoreEntry {
            version: (*version).to_owned(),
            bits,
            origin: CoreEntryOrigin::Known,
            hash_verified: false,
            zip_verified: false,
            probed_version: None,
            path: None,
            active: false,
            selectable: true,
        });
    }

    Ok(entries)
}

/// 读安装证据并与磁盘上的 exe 对账：返回 `(hash_verified, zip_verified)`。
///
/// - `hash_verified` = 有 provenance **且** 磁盘 exe 的 SHA-256 与 `exe_sha256` 一致；
/// - `zip_verified` = provenance 里的 `known_sha256_verified`（安装时对过官方 zip 哈希）。
///
/// 无 provenance（bundled / external / 本功能上线前的旧安装 / 坏 JSON）→ `(false, false)`：
/// 「没有证据」就如实说没有，绝不冒充「校验通过」。
fn provenance_state(store: &VersionStore, version: &str, exe: &Path) -> (bool, bool) {
    let Some(provenance) = store.provenance(version) else {
        return (false, false);
    };
    let matches = matches!(sha256_of_file(exe), Some(actual) if actual == provenance.exe_sha256);
    (matches, provenance.known_sha256_verified)
}

/// 整文件 SHA-256（小写十六进制）；读盘失败 → `None`。
fn sha256_of_file(path: &Path) -> Option<String> {
    fs::read(path).ok().map(|bytes| sha256_hex(&bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- normalize_tag（验收 10+ 例）----

    #[test]
    fn normalize_accepts_plain_version() {
        assert_eq!(normalize_tag("1.37.0").expect("合法"), "1.37.0");
    }

    #[test]
    fn normalize_strips_release_prefix_once() {
        assert_eq!(normalize_tag("release-1.37.0").expect("合法"), "1.37.0");
        // 只剥一次：第二个前缀不剥，落到 BadShape。
        assert_eq!(
            normalize_tag("release-release-1.37.0").expect_err("不递归剥"),
            TagError::BadShape {
                value: "release-release-1.37.0".to_owned()
            }
        );
    }

    #[test]
    fn normalize_accepts_surrounding_whitespace() {
        assert_eq!(
            normalize_tag("  1.37.0  ").expect("两端空白是粘贴噪音"),
            "1.37.0"
        );
        assert_eq!(
            normalize_tag("\trelease-1.37.0\n").expect("制表/换行同理"),
            "1.37.0"
        );
    }

    #[test]
    fn normalize_rejects_empty() {
        assert_eq!(normalize_tag("").expect_err("空"), TagError::Empty);
        assert_eq!(normalize_tag("   ").expect_err("只有空白"), TagError::Empty);
    }

    #[test]
    fn normalize_rejects_incomplete_and_extra_segments() {
        assert_eq!(
            normalize_tag("1.2").expect_err("两段"),
            TagError::BadShape {
                value: "1.2".to_owned()
            }
        );
        assert_eq!(
            normalize_tag("1.2.3.4").expect_err("四段"),
            TagError::BadShape {
                value: "1.2.3.4".to_owned()
            }
        );
    }

    #[test]
    fn normalize_rejects_v_prefix() {
        // `v1.2.3` 不接受：GitHub 上 aria2 的 tag 是 `release-`，`v` 前缀一定打错地址。
        assert_eq!(
            normalize_tag("v1.2.3").expect_err("v 前缀"),
            TagError::BadShape {
                value: "v1.2.3".to_owned()
            }
        );
    }

    #[test]
    fn normalize_rejects_path_traversal_forms() {
        for bad in ["../etc/passwd", "1.37.0/x", "1.37.0\\x", "..", "1..0"] {
            assert_eq!(
                normalize_tag(bad).expect_err("穿越必须拒绝"),
                TagError::PathTraversal,
                "{bad:?}"
            );
        }
    }

    #[test]
    fn normalize_rejects_internal_whitespace() {
        assert_eq!(
            normalize_tag("1.3 7.0").expect_err("内部空格"),
            TagError::Whitespace {
                value: "1.3 7.0".to_owned()
            }
        );
    }

    #[test]
    fn normalize_rejects_non_ascii() {
        assert_eq!(
            normalize_tag("1.37.0版").expect_err("非 ASCII"),
            TagError::NonAscii {
                value: "1.37.0版".to_owned()
            }
        );
        // 全角数字也不行（看着像版本号，实际进 URL 会变成百分号编码）。
        assert!(matches!(
            normalize_tag("１.37.0"),
            Err(TagError::NonAscii { .. })
        ));
    }

    #[test]
    fn normalize_rejects_overlong_input() {
        let long = format!("1.37.0{}", "0".repeat(MAX_TAG_BYTES));
        let error = normalize_tag(&long).expect_err("超长");
        assert!(
            matches!(error, TagError::TooLong { bytes } if bytes >= MAX_TAG_BYTES),
            "{error:?}"
        );
    }

    #[test]
    fn normalize_canonicalizes_leading_zeroes() {
        // 归一化产物是规范形式：目录名必须唯一，否则同一版本会装两份。
        assert_eq!(normalize_tag("01.037.00").expect("前导零可解析"), "1.37.0");
    }

    #[test]
    fn tag_error_maps_to_usage_code_and_bad_tag() {
        let error: CoreError = TagError::PathTraversal.into();
        assert!(matches!(error, CoreError::BadTag { .. }), "{error}");
        assert_eq!(error.code(), Some(ariahost_common::ErrorCode::Usage));
    }

    #[test]
    fn release_tag_is_prefixed_but_asset_name_is_not() {
        assert_eq!(release_tag_of("1.37.0"), "release-1.37.0");
    }

    // ---- `aria2c --version` 探测（假 exe 不真跑）----

    #[test]
    fn probe_staged_core_cleans_its_staging_dir() {
        struct Fake(&'static str);
        impl CoreVersionProbe for Fake {
            fn probe_version(&self, exe: &Path) -> Result<String, String> {
                assert!(exe.is_file(), "探测前 exe 必须已落盘");
                Ok(self.0.to_owned())
            }
        }

        let dir = TempDir::new("probe-staged");
        let store = VersionStore::new(dir.path());
        let extracted = ExtractedCore {
            aria2c: b"fake exe".to_vec(),
            dlls: vec![("libssl-3-x64.dll".to_owned(), b"dll".to_vec())],
            skipped: Vec::new(),
        };
        let probed =
            probe_staged_core(&store, "1.37.0", &extracted, &Fake("1.37.0")).expect("假探测应成功");
        assert_eq!(probed, "1.37.0");
        let leftovers: Vec<_> = fs::read_dir(store.root())
            .expect("读根")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(leftovers.is_empty(), "验证目录必须删干净：{leftovers:?}");
    }

    #[test]
    fn probe_staged_core_propagates_probe_failure_as_package_invalid() {
        struct Broken;
        impl CoreVersionProbe for Broken {
            fn probe_version(&self, _exe: &Path) -> Result<String, String> {
                Err("跑不起来".to_owned())
            }
        }
        let dir = TempDir::new("probe-broken");
        let store = VersionStore::new(dir.path());
        let extracted = ExtractedCore {
            aria2c: b"fake".to_vec(),
            dlls: Vec::new(),
            skipped: Vec::new(),
        };
        let error =
            probe_staged_core(&store, "1.37.0", &extracted, &Broken).expect_err("跑不起来必须拒装");
        assert!(matches!(error, CoreError::PackageInvalid { .. }), "{error}");
        assert!(error.to_string().contains("无法验证版本"), "{error}");
    }

    // ---- build-N 探测（假传输：一次真实网络都不发）----

    /// 按脚本返回状态的假传输；同时记录每次请求的 URL 与调用次数。
    struct Scripted {
        replies: Vec<Result<u16, ProbeTransport>>,
        calls: std::cell::RefCell<Vec<String>>,
    }

    impl Scripted {
        fn new(replies: Vec<Result<u16, ProbeTransport>>) -> Self {
            Self {
                replies,
                calls: std::cell::RefCell::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl Prober for Scripted {
        fn head(&self, url: &str, _timeout: Duration) -> Result<u16, ProbeTransport> {
            let index = self.calls.borrow().len();
            self.calls.borrow_mut().push(url.to_owned());
            self.replies.get(index).cloned().unwrap_or(Ok(404))
        }
    }

    fn direct_probe(prober: &Scripted) -> Result<TagAsset, CoreError> {
        probe_tag_asset_tracked(
            prober,
            CoreProxyMode::Direct,
            "",
            "release-1.37.0",
            &mut Vec::new(),
        )
    }

    #[test]
    fn build_probe_continues_on_404_and_stops_on_hit() {
        let prober = Scripted::new(vec![Ok(404), Ok(200)]);
        let asset = direct_probe(&prober).expect("build2 应命中");

        assert_eq!(asset.tag, "1.37.0");
        assert_eq!(asset.release_tag, "release-1.37.0");
        assert_eq!(asset.build, 2);
        assert_eq!(asset.line, CoreLineKind::Direct);
        assert_eq!(
            asset.url,
            format!(
                "{DOWNLOAD_URL_BASE}release-1.37.0/aria2-1.37.0-win-{}bit-build2.zip",
                host_bits()
            )
        );
        assert_eq!(prober.calls().len(), 2, "404 之后要继续 build2，命中即停");
        assert!(prober.calls()[0].contains("build1.zip"));
    }

    #[test]
    fn build_probe_all_404_reports_the_dedicated_error() {
        let prober = Scripted::new(vec![Ok(404), Ok(404), Ok(404)]);
        let error = direct_probe(&prober).expect_err("全 404 必须报错");
        match &error {
            CoreError::NoReleaseAsset { tag, bits, builds } => {
                assert_eq!(tag, "1.37.0");
                assert_eq!(*bits, host_bits());
                assert_eq!(*builds, MAX_BUILD);
            }
            other => panic!("应是 NoReleaseAsset，实际 {other:?}"),
        }
        assert_eq!(prober.calls().len(), 3, "三个 build 都试过");
        assert_eq!(error.code(), None, "这不是网络错误");
    }

    #[test]
    fn build_probe_stops_immediately_on_server_error() {
        let prober = Scripted::new(vec![Ok(500), Ok(200)]);
        let error = direct_probe(&prober).expect_err("500 必须报错");
        assert!(
            matches!(error, CoreError::HttpStatus { status: 500 }),
            "{error}"
        );
        assert_eq!(prober.calls().len(), 1, "非 404 不许继续试下一个 build");
    }

    #[test]
    fn build_probe_timeout_is_a_network_error_and_stops() {
        let prober = Scripted::new(vec![Err(ProbeTransport::Timeout("8s".to_owned())), Ok(200)]);
        let error = direct_probe(&prober).expect_err("超时必须报错");
        assert!(
            matches!(error, CoreError::NetworkUnreachable { .. }),
            "超时归网络类：{error}"
        );
        assert_eq!(prober.calls().len(), 1, "超时不许继续试下一个 build");
    }

    // ---- list_cores（四类来源的可见性）----

    /// 造一个「内置核心」文件。
    fn fake_bundled(dir: &Path) -> PathBuf {
        let path = dir.join("install").join(CORE_EXE);
        fs::create_dir_all(path.parent().expect("父目录")).expect("建安装目录");
        fs::write(&path, b"MZ").expect("写内置核心");
        path
    }

    /// 装一个已激活的托管版本。
    fn install_managed(root: &Path, version: &str) -> VersionStore {
        let store = VersionStore::new(root);
        store
            .install_core(
                version,
                &ExtractedCore {
                    aria2c: b"MZ".to_vec(),
                    dlls: Vec::new(),
                    skipped: Vec::new(),
                },
            )
            .expect("装托管版本");
        store.activate(version).expect("激活");
        store
    }

    #[test]
    fn list_cores_with_only_bundled_still_lists_the_known_version_as_uninstalled() {
        let dir = TempDir::new("list-bundled");
        let bundled = fake_bundled(dir.path());
        let versions = dir.path().join("versions");

        let entries = list_cores_in(
            &Config::default(),
            Some(&versions),
            Some(&bundled),
            &FixedProbe("1.37.0"),
        )
        .expect("列核心");

        assert_eq!(entries.len(), 2, "{entries:?}");
        let bundled_entry = &entries[0];
        assert_eq!(bundled_entry.origin, CoreEntryOrigin::Bundled);
        assert_eq!(bundled_entry.version, "1.37.0");
        assert_eq!(bundled_entry.probed_version.as_deref(), Some("1.37.0"));
        assert_eq!(bundled_entry.path.as_deref(), Some(bundled.as_path()));
        assert!(bundled_entry.selectable);
        assert!(bundled_entry.active, "auto 下内置核心就是当前选中的那条");
        assert!(
            !bundled_entry.hash_verified,
            "内置核心不在版本库里，没有安装证据 → 必须 false"
        );
        assert!(
            !bundled_entry.zip_verified,
            "bundled 没有安装时的 zip 校验记录"
        );

        let known_entry = &entries[1];
        assert_eq!(known_entry.origin, CoreEntryOrigin::Known);
        assert_eq!(known_entry.version, "1.37.0");
        assert!(known_entry.path.is_none(), "known 未安装不得给路径");
        assert!(known_entry.selectable, "known 未安装仍可选（点了就下载）");
        assert!(!known_entry.active);
    }

    #[test]
    fn list_cores_merges_bundled_and_managed() {
        let dir = TempDir::new("list-merged");
        let bundled = fake_bundled(dir.path());
        let versions = dir.path().join("versions");
        install_managed(&versions, "1.36.0");

        let entries = list_cores_in(
            &Config::default(),
            Some(&versions),
            Some(&bundled),
            &FixedProbe("1.36.0"),
        )
        .expect("列核心");

        // bundled + managed(1.36.0) + known(1.37.0 未安装)
        assert_eq!(entries.len(), 3, "{entries:?}");
        let managed = entries
            .iter()
            .find(|entry| entry.origin == CoreEntryOrigin::Managed)
            .expect("必须有托管条目");
        assert_eq!(managed.version, "1.36.0");
        assert_eq!(managed.path, Some(versions.join("1.36.0").join(CORE_EXE)));
        assert!(managed.probed_version.is_some());
        assert!(!managed.active, "auto 下内置核心优先，托管不是 active");
        let known = entries
            .iter()
            .find(|entry| entry.origin == CoreEntryOrigin::Known)
            .expect("必须有 known 条目");
        assert_eq!(known.version, "1.37.0");
        assert!(known.path.is_none());
    }

    #[test]
    fn list_cores_skips_version_dirs_without_exe() {
        let dir = TempDir::new("list-broken");
        let versions = dir.path().join("versions");
        // 坏目录：有目录、没 exe（半装/被手删）。
        fs::create_dir_all(versions.join("1.35.0")).expect("造坏目录");
        fs::write(versions.join("1.35.0").join("junk.bin"), b"junk").expect("写垃圾");

        let entries = list_cores_in(
            &Config::default(),
            Some(&versions),
            None,
            &FixedProbe("1.0.0"),
        )
        .expect("列核心");

        assert!(
            entries.iter().all(|entry| entry.version != "1.35.0"),
            "没有 exe 的版本目录不得进列表：{entries:?}"
        );
        assert!(
            entries
                .iter()
                .all(|entry| entry.origin == CoreEntryOrigin::Known),
            "只剩 known 条目：{entries:?}"
        );
    }

    #[test]
    fn list_cores_hides_a_known_version_once_installed() {
        let dir = TempDir::new("list-known-installed");
        let versions = dir.path().join("versions");
        install_managed(&versions, "1.37.0");

        let entries = list_cores_in(
            &Config::default(),
            Some(&versions),
            None,
            &FixedProbe("1.37.0"),
        )
        .expect("列核心");

        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.version == "1.37.0")
                .count(),
            1,
            "同一版本只能出现一条（managed 代表它，known 不重复列）：{entries:?}"
        );
        assert_eq!(entries[0].origin, CoreEntryOrigin::Managed);
    }

    /// M5-c：`hash_verified` 的证据链——无证据 → false；证据与磁盘一致 → true；
    /// 安装后 exe 被改 → 立刻掉回 false（这正是这个字段存在的意义）。
    #[test]
    fn list_cores_hash_verified_requires_provenance_and_matching_bytes() {
        let dir = TempDir::new("list-provenance");
        let versions = dir.path().join("versions");
        let store = install_managed(&versions, "1.36.0");
        let exe = store.exe_path("1.36.0");
        let bytes = fs::read(&exe).expect("读已装的 exe");

        // ① 没有 provenance（本功能上线前的旧安装）：两项都 false。
        let entries = list_cores_in(
            &Config::default(),
            Some(&versions),
            None,
            &FixedProbe("1.36.0"),
        )
        .expect("列核心");
        assert_eq!(entries[0].version, "1.36.0");
        assert!(!entries[0].hash_verified, "没有证据必须如实 false");
        assert!(!entries[0].zip_verified);

        // ② 写入与磁盘一致的证据 → hash_verified=true；known 标志映射到 zip_verified。
        let provenance = Provenance {
            asset_name: "aria2-1.36.0-win-64bit-build1.zip".to_owned(),
            line: "direct".to_owned(),
            zip_sha256: "00".repeat(32),
            exe_sha256: sha256_hex(&bytes),
            known_sha256_verified: true,
            installed_at: "2026-11-26T02:00:00Z".to_owned(),
        };
        store
            .write_provenance("1.36.0", &provenance)
            .expect("写安装证据");
        let entries = list_cores_in(
            &Config::default(),
            Some(&versions),
            None,
            &FixedProbe("1.36.0"),
        )
        .expect("列核心");
        assert!(entries[0].hash_verified, "与安装时逐字节一致 → true");
        assert!(
            entries[0].zip_verified,
            "known_sha256_verified 要映射到 zip_verified"
        );

        // ③ 安装后 exe 被改（长度不变，改首字节）→ hash_verified 立刻 false。
        let mut tampered = bytes;
        tampered[0] ^= 0xFF;
        fs::write(&exe, &tampered).expect("改 exe");
        let entries = list_cores_in(
            &Config::default(),
            Some(&versions),
            None,
            &FixedProbe("1.36.0"),
        )
        .expect("列核心");
        assert!(
            !entries[0].hash_verified,
            "安装后被改动必须掉回 false（防篡改/磁盘损坏）"
        );
        assert!(
            entries[0].zip_verified,
            "zip_verified 是安装时的事实，不随当前文件变化"
        );
    }

    // ---- 测试夹具 ----

    /// 注入的版本探测：回一个固定版本（**不真跑 exe**）。
    struct FixedProbe(&'static str);

    impl CoreVersionProbe for FixedProbe {
        fn probe_version(&self, exe: &Path) -> Result<String, String> {
            assert!(exe.is_file(), "探测前 exe 必须已落盘：{}", exe.display());
            Ok(self.0.to_owned())
        }
    }

    struct TempDir(PathBuf);

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let sequence = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "ariahost-core-tag-{tag}-{}-{sequence}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("建临时目录");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
