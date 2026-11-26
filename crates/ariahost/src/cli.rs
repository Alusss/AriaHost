//! 命令行契约（架构 §4.1.4）。
//!
//! 分支、错误码、退出码必须精确：安装器、面板与 CI 都依赖这张表。
//!
//! 分派顺序硬约束：先解析 `argv[1..]`，再决定是否创建/检查单实例锁；
//! 短命助手（安装/卸载/AUMID/`--quit`）一律不参与单实例。
//!
//! M2 起「无参数」真的起窗（`crates/ariahost-ui`）；`--service` 与三个短命助手仍是桩。
//! `--panel-selftest` 是隐藏自测入口（不出现在 `--help` 里），供无人值守量测内存用。

use std::path::{Path, PathBuf};

use ariahost_common::{ErrorCode, BINARY_NAME, EXIT_ERR, EXIT_OK, EXIT_TIMEOUT, VERSION};

/// 用法文本：`--help` 与用法错误共用，中文，列出全部子命令与退出码。
pub const USAGE: &str = "\
AriaHost — aria2 Windows 服务管理器

用法：
  ariahost                         启动面板
  ariahost --backend <name>        启动面板并指定渲染后端（auto|dx12|vulkan）
  ariahost --service               以 Windows 服务方式运行（ServiceHost）
  ariahost --install-service       注册 Windows 服务（需管理员）
  ariahost --uninstall-service     注销 Windows 服务（需管理员）
  ariahost --write-aumid <lnk>     给快捷方式写 AUMID（安装器调用；Toast 的前提）
  ariahost --service-status        打印服务状态（单行 JSON，机器可读；未安装也退出 0）
  ariahost --aria2-fetch [版本]    下载并安装 aria2 核心（省略版本 = 最新，如 1.37.0）
  ariahost --quit                  请求运行中的面板实例退出（没实例也算成功，安装器用）
  ariahost --diagnose [--out 目录] 导出脱敏诊断包（zip；默认放 %LOCALAPPDATA%\\AriaHost\\diagnose）
  ariahost --version | -V          显示版本
  ariahost --help | -h             显示本帮助

退出码：
  0  成功（含 --quit 的「没有实例」与「已确认退出」）
  1  失败（参数错误以 AH-E1001 开头；其他错误以 AH-E#### 开头；--quit 被拒绝也回 1）
  2  --quit 与控制管道握手/等退出超时
";

/// 解析后的子命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// 无参数（或只给 `--backend`）：启动面板（M2 起真的开窗）。
    Panel {
        /// 渲染后端（`--backend`，默认 [`ariahost_ui::Backend::Auto`]）。
        backend: ariahost_ui::Backend,
    },
    /// `--panel-selftest [--frames N] [--size WxH] [--backend ...] [--seed-rows N]`：
    /// 隐藏自测入口，开窗渲染 N 帧后打印机器可读行并退出。
    PanelSelftest {
        /// 渲染帧数（尺寸到位后才开始计）。
        frames: u32,
        /// 目标窗口物理像素尺寸 `(宽, 高)`；用于验证大于 2048 的交换链。
        size: Option<(u32, u32)>,
        /// 渲染后端（`--backend`）。
        backend: ariahost_ui::Backend,
        /// 灌入多少行假任务（EG-8/EG-9 的 200 行内存闸门）；`None` = 不灌。
        seed_rows: Option<usize>,
    },
    /// `--panel-selftest --drive <场景>`：隐藏自检入口（不进 `--help`）。
    ///
    /// 用 egui 自己的 `RawInput` 注入事件驱动面板的真实绘制路径 —— 不开窗、不要 wgpu、
    /// 不需要解锁桌面，因此**可以在 CI 里跑**。存在的理由：`egui 0.37` 只消费 accesskit
    /// 的 `Click`，UIA 对 `TextEdit`/`RadioButton`/`CheckBox` 的驱动全部无效。
    PanelDrive {
        /// 场景名（目前只有 `settings`）。
        scenario: String,
    },
    /// `--atlas-probe [--chars N[,N...]]`：汉字字形图集容量探针（隐藏入口，不进 `--help`）。
    ///
    /// 不开窗、不要 wgpu：`Context::run_ui` 排版 + `Context::tessellate` 触发光栅化，
    /// 因此 CI 与用户机器都能跑；字段口径见 `ariahost_ui::atlas_probe` 模块文档。
    AtlasProbe {
        /// 要测的字数阶梯；`None` = 默认阶梯（500 → 3755）。
        chars: Option<Vec<usize>>,
    },
    /// `--web-selftest`：隐藏自检入口，跑一遍网页管理页的端到端自检（不出现在 `--help`）。
    WebSelftest,
    /// `--service`：ServiceHost。
    Service,
    /// `--install-service`：注册 Windows 服务（短命助手）。
    InstallService,
    /// `--uninstall-service`：注销 Windows 服务（短命助手）。
    UninstallService,
    /// `--service-status`：只读查询服务状态（短命助手，单行 JSON；面板/安装器/脚本共用）。
    ServiceStatus,
    /// `--write-aumid <lnk>`：给快捷方式写 AUMID（短命助手）。
    WriteAumid {
        /// 目标 `.lnk` 路径。
        lnk: PathBuf,
    },
    /// `--aria2-fetch [版本|latest]`：取核心 → 校验 → 装库 → 激活（M1-3 的可跑入口）。
    Aria2Fetch {
        /// 目标版本：`None`/`latest` 取最新，`1.37.0` 取指定版本。
        version: Option<String>,
    },
    /// `--aria2-runtime-selftest [--seconds N]`：隐藏自检入口（不出现在 `--help` 里），
    /// 真拉起 aria2c、打印状态序列、优雅停止（M2-2a 的可跑入口）。
    RuntimeSelftest {
        /// 就绪后停留多少秒再停（默认 5）。
        seconds: u64,
    },
    /// `--quit`：请求已有实例退出（短命助手）。
    Quit,
    /// `--diagnose [--out <dir>]`：导出脱敏诊断包（F11；纯只读 + 写一个 zip）。
    Diagnose {
        /// 输出目录；`None` = `%LOCALAPPDATA%\AriaHost\diagnose`。
        out: Option<PathBuf>,
    },
    /// `--core-line-selftest [--out <dir>]`：隐藏自检入口（不进 `--help`），
    /// 真跑一遍「直连 / 代理前缀」两档可用性探测，把 JSON 报告落盘并打印路径。
    ///
    /// 为什么需要它：M5-b 之后「用户网络到底通不通哪一档」必须能一键取证，
    /// 否则只能靠用户口述（已知：GitHub 直连在部分网络下完全不可用）。
    NetSelftest {
        /// 报告输出目录；`None` = `%LOCALAPPDATA%\AriaHost\diagnose`。
        out: Option<PathBuf>,
    },
    /// `--core-tag-selftest <tag> [--dry-run]`：隐藏自检入口（不进 `--help`）。
    ///
    /// 按手工 tag 走一遍「归一化 → build-N 探测 → G5 流水线」，**逐行**打印结论
    /// （tag/线路/每个 build 的状态码/最终 URL/G5 每步的 ok 与 detail），供人工核对。
    /// `--dry-run` 只做到探测：不下载、不落盘、不激活。
    CoreTagSelftest {
        /// 原始 tag 输入（未归一化）。
        tag: String,
        /// 只探测，不下载/不落盘/不激活。
        dry_run: bool,
    },
    /// `--core-list-selftest`：隐藏自检入口（不进 `--help`），打印 [`ariahost_core::list_cores`]
    /// 的 JSON，供 UI 契约核对（字段名/来源/可选择性都是面板要用的）。
    CoreListSelftest,
    /// `--version` / `-V`。
    Version,
    /// `--help` / `-h`。
    Help,
}

/// 用法错误，统一映射到 `AH-E1001` 与退出码 [`EXIT_ERR`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError {
    message: String,
}

impl UsageError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    /// 错误码（固定 `AH-E1001`）。
    pub fn code(&self) -> &'static str {
        ErrorCode::Usage.code()
    }

    /// 面向用户的错误描述（不含错误码）。
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// `--quit` 与控制管道握手的结果（架构 §4.1.4）。
///
/// **退出码口径**（M3-f 定版，与 `--help` 文本一致）：
/// - `NoInstance` → **0**：安装器的卸载/覆盖升级流程会调 `--quit`，机器上本来就没面板
///   在跑正是它想要的结果；这里回 1 会让安装报假错（M3-f 之前就是这个 bug）。
/// - `Signalled` → 0：收到 `ok pid=` 且该进程已消失。
/// - `Refused` → 1：管道回了 `refused ...`（对端不认识 `quit`，通常是版本不匹配）。
/// - `Timeout` → 2：连上了但握手/等退出超时；调用方应「转人工」而不是重试到底。
///
/// 非 Windows 上 `platform_quit` 只会回 `NoInstance`，其余三态在那里「构造不出来」，
/// 但退出码映射必须与平台无关地保持四态（测试也按四态断言）——所以按平台放行死代码检查。
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuitOutcome {
    /// 没有运行中的面板实例（**算成功**）。
    NoInstance,
    /// 握手成功，实例已确认退出。
    Signalled,
    /// 管道存在但拒绝了请求（原因为对端应答原文）。
    Refused(String),
    /// 管道存在但握手或等待退出超时；附**是哪一段**超时。
    Timeout(TimeoutPhase),
}

/// 超时发生在哪一段——**只影响 stderr 的人话，不碰 stdout 契约与退出码**。
///
/// 为什么要把一个码分成两种事实：`quit: timeout` 既可能是「请求根本没送达」（握手就
/// 没完成，面板可能压根没收到 quit），也可能是「请求送达了、面板在退但没退完」
/// （在存会话 / 停 aria2c）。前者该查管道、权限与版本；后者该再等或转人工。
/// 压成一个码，排障时只能猜；而 stdout 又不能加字段（安装器按全等比对）。
///
/// 三个变体归两类事实：`Handshake` 与 `Watchdog` 都属「没送达」（管道连不上/读完无应答，
/// 或应答一直不返回），`ExitWait` 才是「送达但没退完」。用户可见文案按这两类写。
#[cfg_attr(not(windows), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutPhase {
    /// 握手阶段超时：没拿到 `ok pid=` 应答（或读管道出错）。
    Handshake,
    /// 已收到 `ok pid=`，等该进程消失超时。
    ExitWait,
    /// 外层看门狗到期（真机最常见的成因：服务端接了管道却不回应答，读应答永久阻塞），
    /// 或起不了工作线程——两种都属于「没送达」。
    Watchdog,
}

impl QuitOutcome {
    /// 映射到进程退出码：无实例=0、成功=0、被拒=1、超时=2。
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::NoInstance | Self::Signalled => EXIT_OK,
            Self::Refused(_) => EXIT_ERR,
            // 两个超时段共用同一个退出码：安装器只看 0/1/2，不解析细分。
            Self::Timeout(_) => EXIT_TIMEOUT,
        }
    }

    /// 稳定文本前缀（脚本/安装器按它判断，别解析中文）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NoInstance => "no-instance",
            Self::Signalled => "accepted",
            Self::Refused(_) => "refused",
            // 细分只进 stderr，键值仍是 timeout（契约不变）。
            Self::Timeout(_) => "timeout",
        }
    }
}

/// `--atlas-probe --chars` 的取值：逗号分隔的字数，每项 1..=[`ariahost_ui::atlas_probe::MAX_ATLAS_CHARS`]。
fn parse_atlas_chars(value: &str) -> Result<Vec<usize>, UsageError> {
    let mut out = Vec::new();
    for item in value.split(',') {
        let text = item.trim();
        if text.is_empty() {
            return Err(UsageError::new("--chars 里有空项"));
        }
        let count: usize = text
            .parse()
            .map_err(|_| UsageError::new(format!("--chars 需要正整数：{text}")))?;
        if count == 0 || count > ariahost_ui::atlas_probe::MAX_ATLAS_CHARS {
            return Err(UsageError::new(format!(
                "--chars 取值越界（1..={}）：{count}",
                ariahost_ui::atlas_probe::MAX_ATLAS_CHARS
            )));
        }
        out.push(count);
    }
    Ok(out)
}

/// 解析参数（不含程序名）。
///
/// 严格模式：已识别的子命令不接受额外参数；未知参数一律 [`ErrorCode::Usage`]。
pub fn parse(args: &[String]) -> Result<Command, UsageError> {
    let Some(first) = args.first() else {
        return Ok(Command::Panel {
            backend: ariahost_ui::Backend::Auto,
        });
    };
    let rest = &args[1..];

    if first == "--panel-selftest" {
        return parse_selftest(rest);
    }

    // 图集容量探针（task-106）：隐藏入口，不出现在 `--help`。
    // 它不开窗、不读配置、不碰 SCM，只量「这台机器的字体链 + 图集上限」。
    if first == "--atlas-probe" {
        let mut chars: Option<Vec<usize>> = None;
        let mut index = 0;
        while index < rest.len() {
            match rest[index].as_str() {
                "--chars" => {
                    let value = rest
                        .get(index + 1)
                        .ok_or_else(|| UsageError::new("--chars 缺少取值"))?;
                    if chars.is_some() {
                        return Err(UsageError::new("--chars 重复"));
                    }
                    chars = Some(parse_atlas_chars(value)?);
                    index += 2;
                }
                other => {
                    return Err(UsageError::new(format!(
                        "--atlas-probe 不认识的参数：{other}"
                    )));
                }
            }
        }
        return Ok(Command::AtlasProbe { chars });
    }

    // 面板的 `--backend`：只影响渲染后端，不影响任何退出码。
    if first == "--backend" {
        let (backend, consumed) = parse_backend_arg(args, 0)?;
        let leftover = &args[consumed..];
        if !leftover.is_empty() {
            return Err(UsageError::new(format!(
                "--backend 只接受一个取值：{}",
                leftover.join(" ")
            )));
        }
        return Ok(Command::Panel { backend });
    }

    // `--aria2-fetch [版本|latest]`：唯一的可选尾参就是版本号。
    if first == "--aria2-fetch" {
        return match rest {
            [] => Ok(Command::Aria2Fetch { version: None }),
            [version] => Ok(Command::Aria2Fetch {
                version: Some(version.clone()),
            }),
            _ => Err(UsageError::new(format!(
                "--aria2-fetch 只接受一个可选版本号参数：{}",
                rest.join(" ")
            ))),
        };
    }

    // `--aria2-runtime-selftest [--seconds N]`：隐藏自检入口。
    if first == "--aria2-runtime-selftest" {
        return parse_runtime_selftest(rest);
    }

    if first == "--write-aumid" {
        return match rest {
            [] => Err(UsageError::new("--write-aumid 缺少 <lnk 路径> 参数")),
            [path] if path.is_empty() => {
                Err(UsageError::new("--write-aumid 的 <lnk 路径> 不能为空"))
            }
            [path] => Ok(Command::WriteAumid {
                lnk: PathBuf::from(path),
            }),
            _ => Err(UsageError::new("--write-aumid 只接受一个 <lnk 路径> 参数")),
        };
    }

    let command = match first.as_str() {
        "--service" => Command::Service,
        "--web-selftest" => Command::WebSelftest,
        "--install-service" => Command::InstallService,
        "--uninstall-service" => Command::UninstallService,
        "--service-status" => Command::ServiceStatus,
        "--diagnose" => return parse_diagnose(rest),
        "--core-line-selftest" => return parse_core_line_selftest(rest),
        "--core-tag-selftest" => return parse_core_tag_selftest(rest),
        "--core-list-selftest" => Command::CoreListSelftest,
        "--quit" => Command::Quit,
        "--version" | "-V" => Command::Version,
        "--help" | "-h" => Command::Help,
        other => return Err(UsageError::new(format!("未知参数：{other}"))),
    };

    if !rest.is_empty() {
        return Err(UsageError::new(format!(
            "{first} 不接受额外参数：{}",
            rest.join(" ")
        )));
    }
    Ok(command)
}

/// 解析 `--diagnose [--out <dir>]`：唯一的可选参数是输出目录（两种写法都收）。
fn parse_diagnose(rest: &[String]) -> Result<Command, UsageError> {
    Ok(Command::Diagnose {
        out: parse_out_only("--diagnose", rest)?,
    })
}

/// 解析 `--core-line-selftest [--out <dir>]`：与 `--diagnose` 同一套可选目录语法。
fn parse_core_line_selftest(rest: &[String]) -> Result<Command, UsageError> {
    Ok(Command::NetSelftest {
        out: parse_out_only("--core-line-selftest", rest)?,
    })
}

/// 解析 `--core-tag-selftest <tag> [--dry-run]`（`--dry-run` 可前可后）。
fn parse_core_tag_selftest(rest: &[String]) -> Result<Command, UsageError> {
    let mut tag: Option<String> = None;
    let mut dry_run = false;
    for argument in rest {
        if argument == "--dry-run" {
            if dry_run {
                return Err(UsageError::new("--dry-run 只能写一次"));
            }
            dry_run = true;
            continue;
        }
        if tag.is_some() {
            return Err(UsageError::new(format!(
                "--core-tag-selftest 只接受一个 <tag> 参数：{}",
                rest.join(" ")
            )));
        }
        tag = Some(argument.clone());
    }
    let Some(tag) = tag else {
        return Err(UsageError::new(
            "--core-tag-selftest 缺少 <tag> 参数（如 1.37.0 或 release-1.37.0）",
        ));
    };
    Ok(Command::CoreTagSelftest { tag, dry_run })
}

/// `--out <目录>` / `--out=<目录>` 的唯一解析处（诊断包与线路自检共用）。
///
/// 两份实现必然漂移（一边收了 `--out=` 一边没收，脚本就会时而有效），所以抽成一处。
fn parse_out_only(flag: &str, rest: &[String]) -> Result<Option<PathBuf>, UsageError> {
    match rest {
        [] => Ok(None),
        [name, dir] if name == "--out" => {
            if dir.trim().is_empty() {
                return Err(UsageError::new("--out 的目录不能为空"));
            }
            Ok(Some(PathBuf::from(dir)))
        }
        [single] => match single.strip_prefix("--out=") {
            Some(dir) if !dir.trim().is_empty() => Ok(Some(PathBuf::from(dir))),
            Some(_) => Err(UsageError::new("--out= 后面的目录不能为空")),
            None => Err(UsageError::new(format!(
                "{flag} 只接受 --out <目录>，收到：{single}"
            ))),
        },
        _ => Err(UsageError::new(format!(
            "{flag} 只接受 --out <目录>，收到：{}",
            rest.join(" ")
        ))),
    }
}

/// 解析 `--aria2-runtime-selftest` 的尾部参数：`--seconds N`（默认 5，上限 3600）。
///
/// 隐藏入口，**不进 [`USAGE`]**（与 `--panel-selftest` 同处理）。
fn parse_runtime_selftest(rest: &[String]) -> Result<Command, UsageError> {
    const MAX_SECONDS: u64 = 3600;
    let mut seconds: u64 = 5;
    let mut index = 0usize;
    while index < rest.len() {
        let argument = &rest[index];
        let raw = if argument == "--seconds" {
            let value = rest
                .get(index + 1)
                .ok_or_else(|| UsageError::new("--seconds 缺少数值（秒）"))?;
            index += 2;
            value.as_str()
        } else if let Some(value) = argument.strip_prefix("--seconds=") {
            index += 1;
            value
        } else {
            return Err(UsageError::new(format!(
                "--aria2-runtime-selftest 不认识的参数：{argument}"
            )));
        };
        let parsed = raw
            .parse::<u64>()
            .map_err(|_| UsageError::new(format!("--seconds 需要非负整数，收到：{raw}")))?;
        if parsed > MAX_SECONDS {
            return Err(UsageError::new(format!(
                "--seconds 上限 {MAX_SECONDS}，收到：{parsed}"
            )));
        }
        seconds = parsed;
    }
    Ok(Command::RuntimeSelftest { seconds })
}

/// 解析 `--backend <name>`：`args[index]` 必须是 `--backend`，返回后端与消耗到的下标。
fn parse_backend_arg(
    args: &[String],
    index: usize,
) -> Result<(ariahost_ui::Backend, usize), UsageError> {
    let value = args
        .get(index + 1)
        .ok_or_else(|| UsageError::new("--backend 缺少取值（auto|dx12|vulkan）"))?;
    let backend = ariahost_ui::Backend::parse(value).ok_or_else(|| {
        UsageError::new(format!("--backend 只接受 auto|dx12|vulkan，收到：{value}"))
    })?;
    Ok((backend, index + 2))
}

/// 解析 `--panel-selftest` 的尾部参数：`--frames N`、`--size WxH`、`--backend <name>`、
/// `--seed-rows N`，顺序自由、各至多一次。
///
/// 这是隐藏入口（不进 `--help`），供验收/CI 在无人值守下量内存与交换链上限：
/// 开窗渲染 N 帧后打印 `SELFTEST ... rows=N ok=1` 再退出。
/// `--size` 是**物理像素**，窗口允许超出屏幕——这是验证 1440p/4K 交换链的手段；
/// `--backend` 用来把 DX12/Vulkan 单独钉死；`--seed-rows` 灌假任务行（不连 RPC），
/// 用来验 EG-8 虚拟化在 200 行时的内存。
fn parse_selftest(rest: &[String]) -> Result<Command, UsageError> {
    let mut frames = ariahost_ui::DEFAULT_SELFTEST_FRAMES;
    let mut frames_seen = false;
    let mut size = None;
    let mut backend = ariahost_ui::Backend::Auto;
    let mut backend_seen = false;
    let mut seed_rows = None;
    let mut index = 0;
    while index < rest.len() {
        match rest[index].as_str() {
            "--frames" => {
                let value = rest
                    .get(index + 1)
                    .ok_or_else(|| UsageError::new("--frames 缺少帧数"))?;
                if frames_seen {
                    return Err(UsageError::new("--frames 重复"));
                }
                frames_seen = true;
                frames = parse_frames(value)?;
                index += 2;
            }
            "--size" => {
                let value = rest
                    .get(index + 1)
                    .ok_or_else(|| UsageError::new("--size 缺少 WxH（如 2560x1440）"))?;
                if size.is_some() {
                    return Err(UsageError::new("--size 重复"));
                }
                size = Some(parse_size(value)?);
                index += 2;
            }
            "--backend" => {
                if backend_seen {
                    return Err(UsageError::new("--backend 重复"));
                }
                backend_seen = true;
                let (parsed, consumed) = parse_backend_arg(rest, index)?;
                backend = parsed;
                index = consumed;
            }
            "--drive" => {
                // 隐藏入口（不进 --help）：用 egui 自己的 RawInput 注入事件跑设置页场景。
                let Some(value) = rest.get(index + 1) else {
                    return Err(UsageError::new(
                        "--drive 需要一个场景名（目前只有 settings）",
                    ));
                };
                return Ok(Command::PanelDrive {
                    scenario: value.clone(),
                });
            }
            "--seed-rows" => {
                let value = rest
                    .get(index + 1)
                    .ok_or_else(|| UsageError::new("--seed-rows 缺少行数"))?;
                if seed_rows.is_some() {
                    return Err(UsageError::new("--seed-rows 重复"));
                }
                seed_rows = Some(parse_seed_rows(value)?);
                index += 2;
            }
            other => {
                return Err(UsageError::new(format!(
                    "--panel-selftest 不认识的参数：{other}"
                )));
            }
        }
    }
    Ok(Command::PanelSelftest {
        frames,
        size,
        backend,
        seed_rows,
    })
}

/// `--seed-rows` 的取值：0..=[`ariahost_ui::MAX_SEED_ROWS`]（0 = 空表，用来验空态）。
fn parse_seed_rows(value: &str) -> Result<usize, UsageError> {
    match value.parse::<usize>() {
        Ok(count) if count <= ariahost_ui::MAX_SEED_ROWS => Ok(count),
        Ok(count) => Err(UsageError::new(format!(
            "--seed-rows 上限 {}，收到：{count}",
            ariahost_ui::MAX_SEED_ROWS
        ))),
        Err(_) => Err(UsageError::new(format!(
            "--seed-rows 需要非负整数，收到：{value}"
        ))),
    }
}

/// `--frames` 的取值：正整数。
fn parse_frames(value: &str) -> Result<u32, UsageError> {
    match value.parse::<u32>() {
        Ok(frames) if frames > 0 => Ok(frames),
        Ok(_) => Err(UsageError::new("--frames 必须大于 0")),
        Err(_) => Err(UsageError::new(format!(
            "--frames 需要正整数，收到：{value}"
        ))),
    }
}

/// `--size` 的取值：`WxH`（物理像素），每边 320..=`MAX_SELFTEST_SIDE`。
fn parse_size(value: &str) -> Result<(u32, u32), UsageError> {
    let Some((width, height)) = value.split_once(['x', 'X']) else {
        return Err(UsageError::new(format!(
            "--size 需要 WxH（如 2560x1440），收到：{value}"
        )));
    };
    Ok((
        parse_dimension(width, "宽")?,
        parse_dimension(height, "高")?,
    ))
}

/// 单个边长：320..=[`ariahost_ui::MAX_SELFTEST_SIDE`]。
fn parse_dimension(raw: &str, label: &str) -> Result<u32, UsageError> {
    raw.parse::<u32>()
        .ok()
        .filter(|dimension| (320..=ariahost_ui::MAX_SELFTEST_SIDE).contains(dimension))
        .ok_or_else(|| {
            UsageError::new(format!(
                "--size 的{label}必须在 320..={}，收到：{raw}",
                ariahost_ui::MAX_SELFTEST_SIDE
            ))
        })
}

/// 执行一条命令并返回进程退出码。
///
/// 面板分支自 M2 起真的起窗（阻塞到窗口关闭）；`--service` 与三个短命助手仍是占位实现。
///
/// **控制台**：Windows 是 GUI 子系统，stdout 默认没接控制台。凡是「要人/脚本看得见输出」
/// 的命令，在输出前按需附着父控制台（[`crate::console::ensure_for_cli_output`]）；
/// 面板与服务两条路径**不附着**——面板要绝对避免「关掉 cmd 窗口就被连带杀掉」，
/// 服务由 SCM 在 Session 0 拉起、那里根本没有控制台。
/// 这一次调用要不要初始化文件日志（`ariahost.log`，需求 F11）。
///
/// 判据只有一条：**长驻形态才写**。面板（无参或只给 `--backend`）与 `--service` 是用户/SCM
/// 长期持有的进程，它们的历史值得留；其余全是**安装器或脚本调用的短命助手**（`--quit`、
/// `--install-service`、`--uninstall-service`、`--service-status`、`--write-aumid`、
/// `--diagnose`、`--aria2-fetch` 与各 `*-selftest`），它们：
///
/// 1. 每次调用都会往**用户看的那份日志**里塞一行，一次卸载就能灌好几行 `quit:`；
/// 2. 常常跑在权限未定的上下文里（卸载时可能正被拒访问），"打不开日志文件"那条 stderr
///    噪音会让安装器日志看起来像出错。
///
/// `--log-selftest` 不在这里判断：它天生就是测写日志的，在 `main` 里更早的分支就已经初始化。
pub fn inits_file_log(args: &[String]) -> bool {
    matches!(parse(args), Ok(Command::Panel { .. } | Command::Service))
}

pub fn run(args: &[String]) -> i32 {
    match parse(args) {
        Ok(command) => {
            prepare_console(&command);
            execute(&command)
        }
        Err(error) => {
            // 用法错误也要看得见：这一支没有 Command，按「要输出」处理。
            crate::console::ensure_for_cli_output();
            eprintln!("{} {}", error.code(), error.message());
            eprintln!("{USAGE}");
            EXIT_ERR
        }
    }
}

/// 这条命令要不要为输出准备控制台。
///
/// 只有两条路径不要：**面板**（GUI 常驻，附着后关 cmd 会连带杀进程）与
/// **服务**（SCM/Session 0，没有父控制台）。其余（CLI、自检、短命助手）都要。
fn prepare_console(command: &Command) {
    let wants_console = !matches!(command, Command::Panel { .. } | Command::Service);
    if wants_console {
        crate::console::ensure_for_cli_output();
    }
}

/// 分派到具体子命令。
fn execute(command: &Command) -> i32 {
    match command {
        // 面板：M2 起真的起窗（eframe/egui + wgpu）。这里不再有「等待回车」的占位逻辑——
        // 双击快捷方式时用户看到的是窗口，不是控制台输出。
        Command::Panel { backend } => ariahost_ui::run_panel(*backend, Some(web_open_hook())),
        Command::PanelDrive { scenario } => ariahost_ui::run_selftest(
            0,
            None,
            ariahost_ui::Backend::Auto,
            None,
            Some(scenario.clone()),
        ),
        Command::PanelSelftest {
            frames,
            size,
            backend,
            seed_rows,
        } => ariahost_ui::run_selftest(*frames, *size, *backend, *seed_rows, None),
        Command::AtlasProbe { chars } => ariahost_ui::run_atlas_probe(chars.as_deref()),
        Command::Service => match ariahost_svc::run() {
            Ok(()) => EXIT_OK,
            Err(error) => {
                eprintln!("{error}");
                EXIT_ERR
            }
        },
        Command::WebSelftest => run_web_selftest(),
        Command::InstallService => run_install_service(),
        Command::UninstallService => run_uninstall_service(),
        Command::ServiceStatus => run_service_status(),
        Command::WriteAumid { lnk } => run_write_aumid(lnk),
        Command::Aria2Fetch { version } => run_aria2_fetch(version.as_deref()),
        Command::RuntimeSelftest { seconds } => run_runtime_selftest(*seconds),
        Command::Quit => run_quit(),
        Command::Diagnose { out } => run_diagnose(out.as_deref()),
        Command::NetSelftest { out } => run_core_line_selftest(out.as_deref()),
        Command::CoreTagSelftest { tag, dry_run } => run_core_tag_selftest(tag, *dry_run),
        Command::CoreListSelftest => run_core_list_selftest(),
        Command::Version => {
            println!("{BINARY_NAME} {VERSION}");
            EXIT_OK
        }
        Command::Help => {
            println!("{USAGE}");
            EXIT_OK
        }
    }
}

/// `--quit` 的握手与等退出总预算。
///
/// **三个预算的大小关系（都不是拍脑袋，各有实测来源）**：
/// - **面板自己**的退出上限 30s（`ariahost-ui` 的 ctl 收尾：存会话 → 优雅停 aria2c →
///   forceShutdown）；实测最坏约 **19s**（面板 19s 级收尾 + `--quit` 客户端 10s 旧的
///   早期放弃，见 task-65 背景）。
/// - **本客户端 25s**：必须**大于**面板的实测最坏 19s，留 6s 余量——10s 时出现过
///   「面板其实正在退，CLI 先放弃回 timeout，安装器随即判定仍在运行」的假失败。
/// - **安装器**在 `--quit` 返回后自己再等 20s（`STOP_APP_BUDGET_MS`）；它串在本预算
///   **之后**，不重叠，所以总最坏 ≈ 25s + 20s。
///
/// 知情接受：25s **不覆盖**面板理论硬上限 30s——极端慢（接近 30s）时本客户端仍会先回
/// `quit: timeout`。那一刻面板通常已经在退了，安装器的 20s 复查会把这段时间吃掉。
const QUIT_BUDGET: std::time::Duration = std::time::Duration::from_secs(25);

/// `--quit`：请运行中的面板退出（架构 §4.1.4 + 控制管道 §3.1）。
///
/// 输出**一行**且带稳定前缀（脚本按它判断，别解析中文）：
/// `quit: no-instance` / `quit: accepted` / `quit: refused <应答>` / `quit: timeout`。
/// 失败分支才多打一行中文建议。**故意不带 AH 码**：错误码注册表里没有「控制管道」段，
/// 硬套 E1/E2/E5 会让排障时按码搜到错的文档；语义已经由退出码 0/1/2 承担。
fn run_quit() -> i32 {
    let outcome = quit_running_panel();
    println!("{}", quit_stdout_line(&outcome));
    for line in quit_stderr_lines(&outcome) {
        eprintln!("{line}");
    }
    outcome.exit_code()
}

/// `--quit` 的 stdout **只放这一行**：稳定键 + 可选对端应答，纯 ASCII、单行、可全等比对。
///
/// 为什么要这条纪律：安装器 `nsExec::ExecToStack` 抓到 stdout 后会经 codepage 转换，
/// 中文在非 UTF-8 控制台下变成乱码字节；脚本又爱写 `$out -eq 'quit: no-instance'`。
/// 人看的解释一律走 stderr（与 git 把进度写 stderr 同一个道理）。
fn quit_stdout_line(outcome: &QuitOutcome) -> String {
    match outcome {
        QuitOutcome::Refused(reason) => format!("quit: refused {reason}"),
        other => format!("quit: {}", other.as_str()),
    }
}

/// `--quit` 的中文解释（**全部走 stderr**，四态一致）。
fn quit_stderr_lines(outcome: &QuitOutcome) -> Vec<String> {
    match outcome {
        QuitOutcome::NoInstance => {
            vec!["没有运行中的实例（安装器/升级流程视为成功）".to_owned()]
        }
        QuitOutcome::Signalled => Vec::new(),
        QuitOutcome::Refused(reason) => vec![
            format!("退出请求被拒绝：{reason}"),
            "  → 面板与命令行版本可能不一致：装回同一版本后重试".to_owned(),
        ],
        // 「没送达」与「送达但没退完」是两种完全不同的处置，文案必须分开。
        QuitOutcome::Timeout(TimeoutPhase::Handshake) => vec![
            format!(
                "与控制管道握手超时（{}s 内没收到面板应答）",
                QUIT_BUDGET.as_secs()
            ),
            "  → 退出请求可能根本没送达：先确认面板还在跑（任务管理器里有 ariahost.exe）；\
             仍不行就结束它，下次启动会自动降级未保存的会话"
                .to_owned(),
        ],
        QuitOutcome::Timeout(TimeoutPhase::ExitWait) => vec![
            format!(
                "已收到面板应答，但它在 {}s 内没退完（多在保存会话 / 停 aria2c）",
                QUIT_BUDGET.as_secs()
            ),
            "  → 请求已送达、退出未完成：再等一会通常就退了；**别急着强杀**，\
             强杀会丢会话并留下 aria2c 孤儿"
                .to_owned(),
        ],
        // Watchdog 与 Handshake **同属「没送达」**（实测：服务端接了管道却不回应答时，
        // 读应答会一直阻塞，只能由看门狗收尾）。区别只在检测路径，处置口径必须一致——
        // 若在这里说「面板卡在保存会话」就是把没送达误报成退得慢，用户会照着错的方向处置。
        QuitOutcome::Timeout(TimeoutPhase::Watchdog) => vec![
            format!(
                "与控制管道握手超时（{}s 内没收到面板应答；读应答一直没返回）",
                QUIT_BUDGET.as_secs()
            ),
            "  → 与「握手超时」同类：退出请求可能根本没送达。先确认面板还在跑（任务管理器里有 \
             ariahost.exe）；仍不行就结束它，下次启动会自动降级未保存的会话"
                .to_owned(),
        ],
    }
}

/// `--diagnose [--out <dir>]`：导出脱敏诊断包（需求 F11）。
///
/// 失败**不带 AH 码**：错误码注册表里没有「诊断包导出」段，硬套 E1/E3 会让排障时按码搜到
/// 错的文档；`--quit` 同此处理。成功时打印绝对路径 + 字节数 + 包内清单（脚本要能拿到路径）。
fn run_diagnose(out: Option<&Path>) -> i32 {
    match crate::diagnose::run(out) {
        Ok(report) => {
            println!("诊断包已导出：{}", report.path.display());
            println!("大小：{} 字节", report.bytes);
            println!("包含 {} 个文件：", report.entries.len());
            for name in &report.entries {
                println!("  {name}");
            }
            EXIT_OK
        }
        Err(detail) => {
            eprintln!("导出诊断包失败：{detail}");
            eprintln!(
                "排查：--out 指到一个存在且可写的目录；不给 --out 时默认放 %LOCALAPPDATA%\\AriaHost\\diagnose。"
            );
            EXIT_ERR
        }
    }
}

/// 连控制管道、发 `quit`、等目标进程消失（平台分派）。
fn quit_running_panel() -> QuitOutcome {
    platform_quit::quit(QUIT_BUDGET)
}

#[cfg(not(windows))]
mod platform_quit {
    use super::QuitOutcome;
    use std::time::Duration;

    /// 非 Windows：没有命名管道，当作「没有实例」（成功）。
    pub fn quit(_budget: Duration) -> QuitOutcome {
        QuitOutcome::NoInstance
    }
}

#[cfg(windows)]
mod platform_quit {
    use super::{QuitOutcome, TimeoutPhase};
    use std::io::{Read, Write};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use ariahost_ui::ctl::parse_ack_pid;

    /// 握手 + 等退出的整体入口。整体耗时严格受 `budget` 约束（含线程调度）。
    pub fn quit(budget: Duration) -> QuitOutcome {
        let deadline = Instant::now() + budget;
        let (sender, receiver) = mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("ariahost-quit".to_owned())
            .spawn(move || {
                let _ = sender.send(handshake(deadline));
            });
        if spawned.is_err() {
            return QuitOutcome::Timeout(TimeoutPhase::Watchdog);
        }
        receiver
            .recv_timeout(budget + Duration::from_millis(500))
            .unwrap_or(QuitOutcome::Timeout(TimeoutPhase::Watchdog))
    }

    /// 与**当前用户**的控制管道握手。
    ///
    /// 管道名按用户 SID 隔离（`ariahost_common::paths::control_pipe_name`）：
    /// - 打不开 = 当前用户没有面板在跑 → [`QuitOutcome::NoInstance`]（退出码 0，安装器视为成功）；
    /// - **绝不回退到旧固定名**：那个名字只可能属于别的会话/别的用户，回退就是误杀；
    /// - OS 服务实例不走这条管道，所以服务模式与这套隔离无关。
    fn handshake(deadline: Instant) -> QuitOutcome {
        let name = ariahost_common::paths::control_pipe_name();
        let mut pipe = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(name)
        {
            Ok(pipe) => pipe,
            // 打不开 = 管道不存在 = 没有实例在跑（**不是错误**）
            Err(_) => return QuitOutcome::NoInstance,
        };
        if pipe.write_all(b"quit\n").is_err() || pipe.flush().is_err() {
            // 连上又被立刻断：按「没有可用实例」处理，让安装器继续往下走
            return QuitOutcome::NoInstance;
        }
        let mut buffer = [0u8; 128];
        let read = match pipe.read(&mut buffer) {
            Ok(0) => return QuitOutcome::Refused("对端没应答就断开".to_owned()),
            Ok(read) => read,
            Err(_) => return QuitOutcome::Timeout(TimeoutPhase::Handshake),
        };
        let ack = String::from_utf8_lossy(&buffer[..read]).trim().to_owned();
        if ack.starts_with("refused") {
            return QuitOutcome::Refused(ack);
        }
        let Some(pid) = parse_ack_pid(&ack) else {
            return QuitOutcome::Refused(format!("应答不可解析：{ack}"));
        };
        // `ok` 只代表「收到了」，要等那个进程真的消失才算成功。
        wait_for_exit(pid, deadline)
    }

    /// 等 pid 退出到 `deadline` 为止。
    ///
    /// 已知窄窗口：pid 可能在「拿到 ack」与「OpenProcess」之间被系统复用。
    /// 本机、10s 内复用到同一个 pid 的概率可忽略，而且即便复用最多误判一次成功——
    /// 安装器随后还会自己检查文件占用，兜的住。
    fn wait_for_exit(pid: u32, deadline: Instant) -> QuitOutcome {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
        use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject};

        let handle = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            // 打不开：进程已经没了（这正是我们要的结果）
            return QuitOutcome::Signalled;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let millis = u32::try_from(remaining.as_millis()).unwrap_or(u32::MAX);
        let waited = unsafe { WaitForSingleObject(handle, millis) };
        unsafe {
            CloseHandle(handle);
        }
        if waited == WAIT_OBJECT_0 {
            QuitOutcome::Signalled
        } else {
            // 应答收到了、进程却到点还在：这是「送达但没退完」，处置与握手超时不同。
            QuitOutcome::Timeout(TimeoutPhase::ExitWait)
        }
    }
}

/// 面板「在浏览器打开」的实现：真正起 `Server` 的是 bin（ui 层不链 ariahost-web）。
///
/// 惰性启动 + 只启动一次：第一次点按钮才建服务，之后复用同一个 URL。
fn web_open_hook() -> ariahost_ui::OpenWebFn {
    let holder: std::sync::Arc<std::sync::Mutex<Option<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    Box::new(move || {
        if let Some(url) = holder.lock().ok().and_then(|guard| guard.clone()) {
            return Ok(url);
        }
        let mut config = ariahost_common::config::load().map_err(|error| error.to_string())?;
        let secret = ariahost_common::config::resolve_secret(
            &mut config,
            &ariahost_common::config::default_secret_path()
                .ok_or_else(|| "取不到 secret.bin 路径".to_owned())?,
        )
        .map_err(|error| error.to_string())?;
        let web = ariahost_web::WebConfig {
            bind: "127.0.0.1".to_owned(),
            port: config.web.port,
            rpc_port: config.aria2.rpc.port,
            secret,
        };
        let url = web.url();
        // Server 一旦 drop 就停服：面板活的这段时间必须一直持有，所以漏给一个常驻线程。
        let server = ariahost_web::Server::start(web).map_err(|error| error.detail())?;
        std::thread::spawn(move || {
            let _keep = server;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            }
        });
        if let Ok(mut guard) = holder.lock() {
            *guard = Some(url.clone());
        }
        Ok(url)
    })
}

/// `--web-selftest`：跑一遍网页管理页自检（建服务 → 拉页面 → 反代一条只读 RPC）。
fn run_web_selftest() -> i32 {
    match ariahost_web::selftest() {
        Ok(()) => {
            println!("网页管理页自检通过：静态页 + 反代 + 只读方法白名单都正常。");
            EXIT_OK
        }
        Err(detail) => {
            eprintln!("{} 网页管理页自检失败：{detail}", ErrorCode::Usage.code());
            eprintln!("排查：确认 web.port（默认 6875）没被别的程序占着；面板「在浏览器打开」用同一个端口。");
            EXIT_ERR
        }
    }
}

/// 安装结果 → (上屏文案, 退出码)。
///
/// **纯函数**：单测只覆盖到这一层。`run_install_service()` 会真的向 SCM 注册服务，
/// 单测里**绝不能调**——CI runner 是管理员（装成功、退出 0），开发者机器多半不是
/// （退出 1），同一条断言在两种身份下结果不同。
fn install_report(outcome: ariahost_svc::InstallOutcome) -> (&'static str, i32) {
    match outcome {
        ariahost_svc::InstallOutcome::Created => ("服务已注册（AriaHost）", EXIT_OK),
        ariahost_svc::InstallOutcome::AlreadyExisted => {
            ("服务本来就在（已注册，无需重装）", EXIT_OK)
        }
    }
}

/// 卸载结果 → (上屏文案, 退出码)。`NotInstalled` 是**成功**（幂等），不是错误。
///
/// 同样是纯函数：`run_uninstall_service()` 在装了 AriaHost 的机器上会删掉**真实的**
/// 用户服务，单测里绝不能调。
fn uninstall_report(outcome: ariahost_svc::UninstallOutcome) -> (&'static str, i32) {
    match outcome {
        ariahost_svc::UninstallOutcome::Removed => ("服务已注销（AriaHost）", EXIT_OK),
        ariahost_svc::UninstallOutcome::NotInstalled => ("服务本来就没装（幂等成功）", EXIT_OK),
    }
}

/// 服务类失败 → 退出码：**一律非 0**（具体 `AH-E` 码由 [`report_service_error`] 上屏）。
const fn service_failure_exit_code() -> i32 {
    EXIT_ERR
}

/// `--install-service`：注册 Windows 服务（需要管理员）。
fn run_install_service() -> i32 {
    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!(
                "{} 取不到本程序路径：{error}",
                ErrorCode::ServiceRegisterPrereq.code()
            );
            return EXIT_ERR;
        }
    };
    match ariahost_svc::install(&exe) {
        Ok(outcome) => {
            let (text, exit) = install_report(outcome);
            println!("{text}：{}", exe.display());
            // 启动类型也是安装器与排障要看的事实：首版**固定手动（按需）启动**，不随系统自启
            // （需求第 5 条：不偷偷开机启服务）。要改得走 SCM 工具，CLI 不提供 `--auto`。
            println!(
                "启动方式：手动（按需）启动，不随系统自启；服务名 AriaHost，账户 LocalSystem。"
            );
            println!(
                "下一步：`ariahost --service` 由系统拉起；面板里点「启动 aria2」不需要管理员。"
            );
            exit
        }
        Err(error) => {
            report_service_error("注册服务失败", &error);
            service_failure_exit_code()
        }
    }
}

/// `--uninstall-service`：注销 Windows 服务（需要管理员）。
fn run_uninstall_service() -> i32 {
    match ariahost_svc::uninstall() {
        Ok(outcome) => {
            let (text, exit) = uninstall_report(outcome);
            println!("{text}");
            exit
        }
        Err(error) => {
            report_service_error("注销服务失败", &error);
            service_failure_exit_code()
        }
    }
}

/// `--write-aumid <lnk>`：给快捷方式写 `System.AppUserModel.ID`（安装器每条 `.lnk` 调一次）。
///
/// 失败**只影响通知**（Toast 降级为托盘气泡），所以这里不打 usage、不返回用法错误码，
/// 而是给既有错误码 + 一句人话 + 建议；安装器把非 0 退出码当「警告」记日志后继续。
fn run_write_aumid(lnk: &Path) -> i32 {
    match crate::aumid::write_aumid(lnk) {
        Ok(()) => {
            println!("AUMID 已写入：{}（{}）", lnk.display(), crate::aumid::AUMID);
            EXIT_OK
        }
        Err(error) => {
            eprintln!("{} {}", error.code().code(), error.message());
            eprintln!("{}", error.suggestion());
            EXIT_ERR
        }
    }
}

/// `--service-status`：只读查询，输出**单行 JSON**（字段名与取值都是稳定英文/数字）。
///
/// 为什么不打印中文：面板要轮询它，解析中文文案等于把 UI 文案变成协议；
/// 未安装也退出 0（那是常态，不是错误）。
fn run_service_status() -> i32 {
    match ariahost_svc::status_report() {
        Ok(report) => {
            println!("{}", service_status_json(&report));
            EXIT_OK
        }
        Err(error) => {
            eprintln!("{} 查询服务状态失败：{}", error.code.code(), error.detail);
            EXIT_ERR
        }
    }
}

/// 把状态报告编成单行 JSON（**稳定契约**：字段顺序固定、缺值一律 `null`）。
///
/// 形状：`{"installed":bool,"state":str,"start_type":str|null,"pid":number|null}`
/// - `state` ∈ `not-installed` / `stopped` / `start-pending` / `running` / `stop-pending` / `unknown`
/// - `start_type` ∈ `auto` / `demand` / `disabled` / `system` / `boot`（未安装为 `null`）
///
/// 取值一律 **kebab-case**：与 `InstallOutcome`（`created`/`already-existed`）、
/// `UninstallOutcome`（`removed`/`not-installed`）同一套风格，消费者不用猜大小写。
pub(crate) fn service_status_json(report: &ariahost_svc::ServiceStatusReport) -> String {
    let start_type = match report.start_type {
        Some(mode) => format!("\"{}\"", mode.as_str()),
        None => "null".to_owned(),
    };
    let pid = match report.pid {
        Some(pid) => pid.to_string(),
        None => "null".to_owned(),
    };
    format!(
        "{{\"installed\":{},\"state\":\"{}\",\"start_type\":{},\"pid\":{}}}",
        report.installed,
        report.state.as_str(),
        start_type,
        pid
    )
}

/// 服务操作失败的统一上屏口径：`AH-E` 码 + 一句人话 + 可执行建议。
fn report_service_error(headline: &str, error: &ariahost_svc::ServiceError) {
    let code = error.code();
    eprintln!("{} {headline}：{}", code.code(), error.detail());
    if matches!(error, ariahost_svc::ServiceError::NeedsAdmin { .. }) {
        eprintln!("请以管理员身份运行（右键「以管理员身份运行」或从提升的命令行执行）。");
        return;
    }
    match error {
        ariahost_svc::ServiceError::NotFromScm { .. } => {
            eprintln!("提示：`--service` 只由 Windows 服务控制管理器拉起，不要手工双击运行。");
        }
        ariahost_svc::ServiceError::NotInstalled { .. } => {
            eprintln!("下一步：先 `ariahost --install-service`（需要管理员）。");
        }
        ariahost_svc::ServiceError::Unsupported => {
            eprintln!("本产品只发 Windows；当前平台不支持注册服务。");
        }
        ariahost_svc::ServiceError::RegisterPrereq { .. } => {
            eprintln!("下一步：确认 %ProgramData%\\AriaHost 可写，并以管理员身份重试。");
        }
        ariahost_svc::ServiceError::StartupFailed { .. } => {
            eprintln!("下一步：看 %ProgramData%\\AriaHost\\logs 下的服务日志。");
        }
        _ => {
            eprintln!("下一步：以管理员身份重试；仍失败就看事件查看器里 AriaHost 的条目。");
        }
    }
}

/// `--aria2-fetch [版本|latest]`：把「取核心 → 校验 → 装库 → 激活」真跑一遍。
///
/// 这是 M1-3 库代码的可跑入口（面板尚未接）。输出刻意机器可读：
/// 成功打印 `已安装 aria2 <ver>（线路：<线路名>）`，失败按既有错误码打印到 stderr。
fn run_aria2_fetch(version: Option<&str>) -> i32 {
    use ariahost_common::paths::{versions_dir, Plane};

    let Some(dir) = versions_dir(Plane::User) else {
        eprintln!(
            "{} 配置路径不可用：拿不到 %LOCALAPPDATA%",
            ariahost_common::ErrorCode::Usage.code()
        );
        return EXIT_ERR;
    };

    let requested = match version {
        None => None,
        Some(raw) => match ariahost_core::parse_version_arg(raw) {
            Ok(parsed) => parsed,
            Err(error) => {
                eprintln!("{error}");
                return EXIT_ERR;
            }
        },
    };

    let store = ariahost_core::VersionStore::new(&dir);
    // 线路顺序与 CDN 地址**来自配置**（task-75）：面板读 `config.net`，命令行也必须读同一份，
    // 否则「设置里把 source_order 改成 github」之后 `--aria2-fetch` 仍走 CDN，行为对不上。
    let fetcher = match ariahost_common::config::load() {
        Ok(config) => ariahost_core::Fetcher::with_net(&config.net),
        // 读不到配置不挡下载：退回产品默认（CDN → GitHub），但必须说出来。
        Err(error) => {
            eprintln!("警告：读配置失败，线路退回产品默认（CDN → GitHub）：{error}");
            ariahost_core::Fetcher::new()
        }
    };
    match fetcher.fetch(&store, requested) {
        Ok(outcome) => {
            println!("已安装 aria2 {}（线路：{}）", outcome.version, outcome.line);
            println!("exe：{}", outcome.exe_path.display());
            println!("版本库：{}", dir.display());
            for warning in &outcome.warnings {
                eprintln!("警告：{warning}");
            }
            EXIT_OK
        }
        Err(error) => {
            // Display 已带 `AH-Exxxx` 前缀。
            eprintln!("{error}");
            // 失败路径没有 `warnings` 通道：线路配置本身的告警必须在这里补出来，不许吞。
            for warning in fetcher.line_warnings() {
                eprintln!("配置告警：{warning}");
            }
            EXIT_ERR
        }
    }
}

/// `--aria2-runtime-selftest [--seconds N]`：真拉起 aria2c → 等就绪 → 停留 N 秒 → 优雅停止。
///
/// 输出格式**逐行稳定**（供 CI/人工核对）：
/// ```text
/// CONFIG exe=<路径> dir=<下载目录> port=<端口>
/// CONF <aria2.conf 路径>
/// STATE starting
/// STATE running rpc=1.37.0
/// INFO pid=12345
/// STATE stopping
/// STATE stopped exit=0
/// INFO forced=false
/// ```
/// 失败时打印 `ERROR <错误码|none> <可读原因>` 并以非 0 退出。
/// `--core-line-selftest [--out <dir>]`：真跑「直连 / 代理前缀」两档可用性探测，落一份 JSON 报告。
///
/// 为什么要真跑：用户网络下 GitHub 直连是否可用**只能实测**（已有实测反例：
/// 直连整体超时、issue 里 s3 段被墙）。设置页的「测一下」按钮与这条命令共用
/// `ariahost_core::net` 的同一份判定，所以这里测出来的结论就是产品结论。
///
/// 退出码：`0` = 至少一档可达（有可用的获取路径）；`1` = 两档都不通。
fn run_core_line_selftest(out: Option<&Path>) -> i32 {
    let mut notes: Vec<String> = Vec::new();
    let config = match ariahost_common::config::load() {
        Ok(config) => config,
        Err(error) => {
            notes.push(format!(
                "读配置失败（按默认值探测，结论只代表默认线路）：{error}"
            ));
            ariahost_common::config::Config::default()
        }
    };

    let net = &config.net;
    let timeout = ariahost_core::net::PROBE_TIMEOUT;
    let modes: &[(ariahost_common::config::CoreProxyMode, &str)] = &[
        (ariahost_common::config::CoreProxyMode::Direct, ""),
        (
            ariahost_common::config::CoreProxyMode::Proxy,
            net.core_proxy_url.as_str(),
        ),
    ];

    let mut lines = Vec::new();
    let mut any_ok = false;
    for (mode, proxy_url) in modes {
        // 代理档在前缀为空时没有可打的地址：如实记一条「未配置」，不假装失败。
        if *mode == ariahost_common::config::CoreProxyMode::Proxy && proxy_url.trim().is_empty() {
            lines.push(serde_json::json!({
                "mode": "proxy",
                "configured": false,
                "ok": false,
                "hint": "没配前缀代理基址（net.core_proxy_url 为空）：要么填一个可用的 GitHub 前缀，要么接受只能直连",
            }));
            continue;
        }
        let final_url =
            ariahost_core::net::github_via(*mode, proxy_url, ariahost_core::net::PROBE_URL);
        match ariahost_core::net::probe_core_line(*mode, proxy_url, timeout) {
            Ok(ok) => {
                any_ok = true;
                lines.push(serde_json::json!({
                    "mode": mode.as_str(),
                    "configured": true,
                    "ok": true,
                    "http_status": ok.http_status,
                    "latency_ms": ok.latency_ms,
                    "url": final_url,
                }));
            }
            Err(error) => {
                lines.push(serde_json::json!({
                    "mode": mode.as_str(),
                    "configured": true,
                    "ok": false,
                    "fault": error.fault.as_str(),
                    "hint": error.hint,
                    "detail": error.detail,
                    "url": final_url,
                }));
            }
        }
    }

    let report = serde_json::json!({
        "generated_at": ariahost_common::time::utc_stamp(std::time::SystemTime::now()),
        "probe_url": ariahost_core::net::PROBE_URL,
        "timeout_ms": timeout.as_millis(),
        "core_proxy_mode": net.core_proxy_mode.as_str(),
        "core_proxy_url": net.core_proxy_url,
        "lines": lines,
        "notes": notes,
    });

    let dir = out
        .map(std::path::Path::to_path_buf)
        .or_else(default_report_dir);
    let Some(dir) = dir else {
        eprintln!("拿不到输出目录（%LOCALAPPDATA% 不可用）：用 --out <目录> 指定一个");
        return EXIT_ERR;
    };
    if let Err(error) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "建不了输出目录 {}：{error}；用 --out 换个可写目录再试",
            dir.display()
        );
        return EXIT_ERR;
    }
    let stamp =
        ariahost_common::time::utc_stamp(std::time::SystemTime::now()).replace([':', '.'], "-");
    let path = dir.join(format!("net-selftest-{stamp}.json"));
    let body = serde_json::to_string_pretty(&report).unwrap_or_else(|_| report.to_string());
    if let Err(error) = std::fs::write(&path, body) {
        eprintln!("写报告失败 {}：{error}", path.display());
        return EXIT_ERR;
    }

    println!("线路自检报告：{}", path.display());
    for line in report["lines"].as_array().into_iter().flatten() {
        let mode = line["mode"].as_str().unwrap_or("?");
        if line["ok"].as_bool().unwrap_or(false) {
            println!(
                "  {mode}: 可达（HTTP {}，{} ms）",
                line["http_status"], line["latency_ms"]
            );
        } else {
            println!(
                "  {mode}: 不可达 → {}",
                line["hint"].as_str().unwrap_or("（无提示）")
            );
        }
    }
    for note in &notes {
        println!("  注意：{note}");
    }

    if any_ok {
        EXIT_OK
    } else {
        eprintln!("两档线路都不可达：设置页里换前缀代理，或改用「指定本地 aria2c」");
        EXIT_ERR
    }
}

/// `--core-tag-selftest <tag> [--dry-run]`：手工 tag 路径的一键取证（M5-b-G）。
///
/// 输出**逐行稳定**（`KEY ...`），方便原样贴进 issue/日志；失败时最后一行一定是
/// `RESULT failed`，成功是 `RESULT ok`，脚本按它判断，不用解析中文。
///
/// `--dry-run` 只做到探测（归一化 + build-N HEAD）：**不下载、不落盘、不激活**——
/// 用来在用户的真实网络下验证「这个 tag 到底能不能拿到」，而不改动任何本地状态。
fn run_core_tag_selftest(input: &str, dry_run: bool) -> i32 {
    use ariahost_common::config::{load, Config, CoreProxyMode};
    use ariahost_core::net::{hint_of, HttpProber};
    use ariahost_core::tag;

    println!("TAG input={input:?}");
    let normalized = match tag::normalize_tag(input) {
        Ok(version) => version,
        Err(error) => {
            println!("ERROR {} {error}", error.code().code());
            println!("RESULT failed");
            return EXIT_ERR;
        }
    };
    let bits = ariahost_core::host_bits();
    println!(
        "TAG normalized={normalized:?} release={:?} bits={bits} page={}",
        format!("release-{normalized}"),
        tag::ARIA2_RELEASES_PAGE
    );

    let mut notes = Vec::new();
    let config = match load() {
        Ok(config) => config,
        Err(error) => {
            notes.push(format!(
                "读配置失败，按默认线路探测（结论只代表默认值）：{error}"
            ));
            Config::default()
        }
    };
    let mode = config.net.core_proxy_mode;
    println!(
        "LINE mode={} base={}",
        mode.as_str(),
        if mode == CoreProxyMode::Proxy {
            config.net.core_proxy_url.as_str()
        } else {
            "（直连，忽略代理基址）"
        }
    );

    // 探测：逐 build 记录（失败时也打印，这是自检与普通调用的区别）。
    let mut attempts = Vec::new();
    let asset = match tag::probe_tag_asset_tracked(
        &HttpProber::new(),
        mode,
        &config.net.core_proxy_url,
        &normalized,
        &mut attempts,
    ) {
        Ok(asset) => asset,
        Err(error) => {
            for attempt in &attempts {
                print_asset_attempt(attempt);
            }
            println!("ERROR {} {error}", code_text(&error));
            println!("HINT {}", hint_of(&error, mode));
            for note in &notes {
                println!("NOTE {note}");
            }
            println!("RESULT failed");
            return EXIT_ERR;
        }
    };
    for attempt in &attempts {
        print_asset_attempt(attempt);
    }
    println!(
        "ASSET build={} url={} github={} line={}",
        asset.build,
        asset.url,
        asset.github_url,
        asset.line.describe()
    );

    if dry_run {
        println!("DRYRUN 不下载、不落盘、不激活");
        for note in &notes {
            println!("NOTE {note}");
        }
        println!("RESULT ok");
        return EXIT_OK;
    }

    let Some(dir) = ariahost_common::paths::versions_dir(ariahost_common::paths::Plane::User)
    else {
        println!("ERROR none 拿不到版本库根目录（%LOCALAPPDATA% 不可用）");
        println!("RESULT failed");
        return EXIT_ERR;
    };
    let store = ariahost_core::VersionStore::new(&dir);
    let fetcher = ariahost_core::Fetcher::with_net(&config.net);
    let mut steps = Vec::new();
    let outcome = tag::fetch_by_tag_tracked(
        &fetcher,
        &store,
        &normalized,
        &ariahost_core::ProcessVersionProbe::default(),
        &mut steps,
    );
    for step in &steps {
        print_g5_step(step);
    }
    match outcome {
        Ok(install) => {
            println!(
                "INSTALL version={} build={} exe={} active={}",
                install.version,
                install.build,
                install.exe_path.display(),
                install.pointer.active
            );
            for warning in &install.warnings {
                println!("WARN {warning}");
            }
            for note in &notes {
                println!("NOTE {note}");
            }
            println!("RESULT ok");
            EXIT_OK
        }
        Err(error) => {
            println!("ERROR {} {error}", code_text(&error));
            println!("HINT {}", hint_of(&error, mode));
            for note in &notes {
                println!("NOTE {note}");
            }
            println!("RESULT failed");
            EXIT_ERR
        }
    }
}

/// `--core-list-selftest`：打印 [`ariahost_core::list_cores`] 的 JSON（stdout 只有 JSON）。
///
/// 存在的理由：面板的核心列表要按字段渲染（`origin`/`selectable`/`active`…），
/// 这份 JSON 就是 UI 契约的**可核对样本**，不必开窗也能看。
fn run_core_list_selftest() -> i32 {
    use ariahost_common::paths::Plane;

    let config = match ariahost_common::config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("警告：读配置失败，按默认值列（结论只代表默认配置）：{error}");
            ariahost_common::config::Config::default()
        }
    };
    match ariahost_core::list_cores(&config, Plane::User) {
        Ok(entries) => match serde_json::to_string_pretty(&entries) {
            Ok(json) => {
                println!("{json}");
                EXIT_OK
            }
            Err(error) => {
                eprintln!("序列化核心列表失败：{error}");
                EXIT_ERR
            }
        },
        Err(error) => {
            eprintln!("{} {error}", code_text(&error));
            EXIT_ERR
        }
    }
}

/// 打印一个 build 的探测结果（自检专用行）。
fn print_asset_attempt(attempt: &ariahost_core::AssetAttempt) {
    println!(
        "BUILD build={} status={} outcome={} asset={} detail={}",
        attempt.build,
        attempt
            .status
            .map(|status| status.to_string())
            .unwrap_or_else(|| "none".to_owned()),
        attempt.outcome.as_str(),
        attempt.asset_name,
        attempt.detail
    );
}

/// 打印一个 G5 步骤的结论（自检专用行）。
fn print_g5_step(step: &ariahost_core::G5Step) {
    println!(
        "G5 step={} ok={} detail={}",
        step.name, step.ok, step.detail
    );
}

/// 错误码文本：没有语义对应的既有码时输出 `none`（与其它自检输出同一口径）。
fn code_text(error: &ariahost_core::CoreError) -> &'static str {
    error.code().map(|code| code.code()).unwrap_or("none")
}

/// 自检报告的默认目录（与诊断包同处，用户找得到）。
fn default_report_dir() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(local).join("AriaHost").join("diagnose"))
}

fn run_runtime_selftest(seconds: u64) -> i32 {
    use std::time::Duration;

    use ariahost_common::config as config_layer;
    use ariahost_core::{
        CoreRuntime, ProcessSpawner, RpcProbe, RuntimeEvent, RuntimePaths, Timeouts,
    };

    let mut config = match config_layer::load() {
        Ok(config) => config,
        Err(error) => {
            report_error(error.code(), &error.to_string());
            return EXIT_ERR;
        }
    };
    let Some(paths) = RuntimePaths::from_env() else {
        report_error(None, "拿不到 %APPDATA%（运行时目录不可用）");
        return EXIT_ERR;
    };
    let Some(secret_path) = config_layer::default_secret_path() else {
        report_error(None, "拿不到 %APPDATA%（secret.bin 路径不可用）");
        return EXIT_ERR;
    };
    let secret = match config_layer::resolve_secret(&mut config, &secret_path) {
        Ok(secret) => secret,
        Err(error) => {
            report_error(error.code(), &error.to_string());
            return EXIT_ERR;
        }
    };
    let download_dir = match config_layer::download_dir_absolute(&config) {
        Ok(dir) => dir,
        Err(error) => {
            report_error(error.code(), &error.to_string());
            return EXIT_ERR;
        }
    };
    let exe = match ariahost_core::resolve_core_exe(&config) {
        Ok(exe) => exe,
        Err(error) => {
            report_error(error.code(), &error.to_string());
            return EXIT_ERR;
        }
    };
    println!(
        "CONFIG exe={} dir={} port={}",
        exe.display(),
        download_dir.display(),
        config.aria2.rpc.port
    );

    let conf = match ariahost_core::prepare_conf(&config, &paths, &secret, &download_dir) {
        Ok(conf) => conf,
        Err(error) => {
            report_error(error.code(), &error.to_string());
            return EXIT_ERR;
        }
    };
    println!("CONF {}", conf.display());

    let spec = ariahost_core::launch_spec(&config, &paths, exe, &secret);
    let probe = RpcProbe::new(config.aria2.rpc.port, &secret);
    let mut runtime = CoreRuntime::new(
        Box::new(ProcessSpawner),
        Box::new(probe),
        Timeouts::default(),
    );

    let mut print_event = |event: RuntimeEvent| print_runtime_event(&event);

    if let Err(error) = runtime.start(&spec, &mut print_event) {
        report_error(error.code(), &error.to_string());
        return EXIT_ERR;
    }

    std::thread::sleep(Duration::from_secs(seconds));

    match runtime.stop(&mut print_event) {
        Ok(status) => {
            println!("INFO forced={}", status.forced);
            EXIT_OK
        }
        Err(error) => {
            report_error(error.code(), &error.to_string());
            EXIT_ERR
        }
    }
}

/// 把一次状态转换打成稳定的若干行（格式见 [`run_runtime_selftest`] 文档）。
///
/// 单独抽成纯函数是为了可单测：CLI 的输出格式是验收项（Lead 会照它抓行），
/// 不能只靠「跑一次看看」。
fn format_runtime_event(event: &ariahost_core::RuntimeEvent) -> String {
    use ariahost_core::RuntimeEvent;
    match event {
        RuntimeEvent::Starting => "STATE starting".to_owned(),
        RuntimeEvent::Running { version, pid } => {
            format!("STATE running rpc={version}\nINFO pid={pid}")
        }
        RuntimeEvent::Stopping => "STATE stopping".to_owned(),
        RuntimeEvent::Stopped { exit_code, .. } => match exit_code {
            Some(code) => format!("STATE stopped exit={code}"),
            None => "STATE stopped exit=none".to_owned(),
        },
        RuntimeEvent::Failed { code, hint } => {
            format!("STATE failed code={} hint={hint}", describe_code(*code))
        }
        RuntimeEvent::Warning { detail } => format!("WARN {detail}"),
    }
}

/// 打印一行（或多行）状态转换。
fn print_runtime_event(event: &ariahost_core::RuntimeEvent) {
    println!("{}", format_runtime_event(event));
}

/// 错误输出：`ERROR <码|none> <原因>`。
fn report_error(code: Option<ariahost_common::ErrorCode>, detail: &str) {
    eprintln!("ERROR {} {detail}", describe_code(code));
}

/// 错误码文本（没有合适码时统一 `none`，不编造 `AH-E####`）。
fn describe_code(code: Option<ariahost_common::ErrorCode>) -> &'static str {
    match code {
        Some(code) => code.code(),
        None => "none",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_owned()).collect()
    }

    fn parse_args(args: &[&str]) -> Result<Command, UsageError> {
        parse(&owned(args))
    }

    fn run_args(args: &[&str]) -> i32 {
        run(&owned(args))
    }

    /// 无参数仍是面板分支。**注意**：这里只断言解析结果——`run(&[])` 会真的开窗，
    /// 在 CI / 无人值守环境调用它会阻塞或失败（M2 起面板是真的 GUI）。
    #[test]
    fn no_args_is_panel() {
        assert_eq!(
            parse_args(&[]),
            Ok(Command::Panel {
                backend: ariahost_ui::Backend::Auto
            })
        );
    }

    #[test]
    fn file_log_inits_only_for_long_lived_forms() {
        // 长驻：面板（无参 / 只给 --backend）与服务。
        for args in [vec![], vec!["--backend", "dx12"], vec!["--service"]] {
            assert!(
                inits_file_log(&owned(&args)),
                "长驻形态应初始化文件日志：{args:?}"
            );
        }
        // 短命助手与自检：一律不写文件日志（安装器噪音 / 权限未定上下文）。
        for args in [
            vec!["--quit"],
            vec!["--install-service"],
            vec!["--uninstall-service"],
            vec!["--service-status"],
            vec!["--write-aumid", r"C:\x.lnk"],
            vec!["--diagnose"],
            vec!["--diagnose", "--out", r"C:\d"],
            vec!["--aria2-fetch"],
            vec!["--aria2-runtime-selftest"],
            vec!["--core-tag-selftest", "1.37.0"],
            vec!["--core-list-selftest"],
            vec!["--panel-selftest"],
            vec!["--panel-selftest", "--drive", "settings"],
            vec!["--web-selftest"],
            vec!["--version"],
            vec!["--help"],
            // 用法错误也不写（这一支只打印用法）。
            vec!["--bogus"],
        ] {
            assert!(
                !inits_file_log(&owned(&args)),
                "短命形态不该初始化文件日志：{args:?}"
            );
        }
    }

    #[test]
    fn panel_selftest_parses_with_optional_frames() {
        assert_eq!(
            parse_args(&["--panel-selftest"]),
            Ok(Command::PanelSelftest {
                frames: ariahost_ui::DEFAULT_SELFTEST_FRAMES,
                size: None,
                backend: ariahost_ui::Backend::Auto,
                seed_rows: None,
            })
        );
        assert_eq!(
            parse_args(&["--panel-selftest", "--frames", "5"]),
            Ok(Command::PanelSelftest {
                frames: 5,
                size: None,
                backend: ariahost_ui::Backend::Auto,
                seed_rows: None,
            })
        );
    }

    #[test]
    fn panel_selftest_parses_seed_rows() {
        assert_eq!(
            parse_args(&["--panel-selftest", "--seed-rows", "200"]),
            Ok(Command::PanelSelftest {
                frames: ariahost_ui::DEFAULT_SELFTEST_FRAMES,
                size: None,
                backend: ariahost_ui::Backend::Auto,
                seed_rows: Some(200),
            })
        );
        // 0 是合法的（空表 → 验空态），不是错误。
        assert_eq!(
            parse_args(&["--panel-selftest", "--frames", "5", "--seed-rows", "0"]),
            Ok(Command::PanelSelftest {
                frames: 5,
                size: None,
                backend: ariahost_ui::Backend::Auto,
                seed_rows: Some(0),
            })
        );
        // 越界 / 非数字 / 重复都要拒，且带 AH-E1001。
        for bad in [
            vec!["--panel-selftest", "--seed-rows", "999999"],
            vec!["--panel-selftest", "--seed-rows", "-1"],
            vec!["--panel-selftest", "--seed-rows", "abc"],
            vec!["--panel-selftest", "--seed-rows"],
            vec!["--panel-selftest", "--seed-rows", "1", "--seed-rows", "2"],
        ] {
            let error = parse_args(&bad).expect_err(&format!("{bad:?} 应当被拒"));
            assert_eq!(error.code(), "AH-E1001", "{bad:?}");
        }
        assert_eq!(
            parse_seed_rows(&ariahost_ui::MAX_SEED_ROWS.to_string()),
            Ok(ariahost_ui::MAX_SEED_ROWS)
        );
    }

    #[test]
    fn panel_selftest_parses_size_in_any_order() {
        assert_eq!(
            parse_args(&["--panel-selftest", "--size", "2560x1440"]),
            Ok(Command::PanelSelftest {
                frames: ariahost_ui::DEFAULT_SELFTEST_FRAMES,
                size: Some((2560, 1440)),
                backend: ariahost_ui::Backend::Auto,
                seed_rows: None,
            })
        );
        assert_eq!(
            parse_args(&["--panel-selftest", "--size", "3840x2160", "--frames", "30"]),
            Ok(Command::PanelSelftest {
                frames: 30,
                size: Some((3840, 2160)),
                backend: ariahost_ui::Backend::Auto,
                seed_rows: None,
            })
        );
        assert_eq!(
            parse_args(&["--panel-selftest", "--frames", "30", "--size", "1920x1080"]),
            Ok(Command::PanelSelftest {
                frames: 30,
                size: Some((1920, 1080)),
                backend: ariahost_ui::Backend::Auto,
                seed_rows: None,
            })
        );
    }

    #[test]
    fn backend_flag_selects_the_render_backend() {
        // 面板：裸 `--backend` 与 selftest 内的 `--backend` 指向同一个枚举。
        assert_eq!(
            parse_args(&["--backend", "dx12"]),
            Ok(Command::Panel {
                backend: ariahost_ui::Backend::Dx12
            })
        );
        assert_eq!(
            parse_args(&["--backend", "vulkan"]),
            Ok(Command::Panel {
                backend: ariahost_ui::Backend::Vulkan
            })
        );
        assert_eq!(
            parse_args(&["--backend", "auto"]),
            Ok(Command::Panel {
                backend: ariahost_ui::Backend::Auto
            })
        );
        // 大小写不敏感（运维手打不会记得小写）。
        assert_eq!(
            parse_args(&["--backend", "DX12"]),
            Ok(Command::Panel {
                backend: ariahost_ui::Backend::Dx12
            })
        );
        // selftest：与 --frames/--size 任意顺序组合。
        assert_eq!(
            parse_args(&[
                "--panel-selftest",
                "--backend",
                "dx12",
                "--frames",
                "3",
                "--size",
                "1920x1080"
            ]),
            Ok(Command::PanelSelftest {
                frames: 3,
                size: Some((1920, 1080)),
                backend: ariahost_ui::Backend::Dx12,
                seed_rows: None,
            })
        );
        assert_eq!(
            parse_args(&["--panel-selftest", "--frames", "3", "--backend", "vulkan"]),
            Ok(Command::PanelSelftest {
                frames: 3,
                size: None,
                backend: ariahost_ui::Backend::Vulkan,
                seed_rows: None,
            })
        );
    }

    #[test]
    fn backend_flag_rejects_bad_values() {
        assert!(parse_args(&["--backend", "metal"]).is_err());
        assert!(parse_args(&["--backend", "gl"]).is_err());
        assert!(parse_args(&["--backend"]).is_err());
        // 面板分支：多给参数是用法错误（不再有「忽略多余参数」的宽松路径）。
        assert!(parse_args(&["--backend", "dx12", "extra"]).is_err());
        assert!(parse_args(&["--panel-selftest", "--backend"]).is_err());
        assert!(parse_args(&["--panel-selftest", "--backend", "dx11"]).is_err());
        assert!(parse_args(&[
            "--panel-selftest",
            "--backend",
            "dx12",
            "--backend",
            "vulkan"
        ])
        .is_err());
    }

    #[test]
    fn aria2_fetch_takes_zero_or_one_version() {
        assert_eq!(
            parse_args(&["--aria2-fetch"]),
            Ok(Command::Aria2Fetch { version: None })
        );
        assert_eq!(
            parse_args(&["--aria2-fetch", "latest"]),
            Ok(Command::Aria2Fetch {
                version: Some("latest".to_owned()),
            })
        );
        assert_eq!(
            parse_args(&["--aria2-fetch", "1.37.0"]),
            Ok(Command::Aria2Fetch {
                version: Some("1.37.0".to_owned()),
            })
        );
        // 只允许一个可选尾参。
        assert!(parse_args(&["--aria2-fetch", "1.37.0", "extra"]).is_err());
    }

    #[test]
    fn runtime_selftest_parses_seconds() {
        assert_eq!(
            parse_args(&["--aria2-runtime-selftest"]),
            Ok(Command::RuntimeSelftest { seconds: 5 })
        );
        assert_eq!(
            parse_args(&["--aria2-runtime-selftest", "--seconds", "8"]),
            Ok(Command::RuntimeSelftest { seconds: 8 })
        );
        assert_eq!(
            parse_args(&["--aria2-runtime-selftest", "--seconds=0"]),
            Ok(Command::RuntimeSelftest { seconds: 0 })
        );
        assert!(parse_args(&["--aria2-runtime-selftest", "--seconds"]).is_err());
        assert!(parse_args(&["--aria2-runtime-selftest", "--seconds", "abc"]).is_err());
        assert!(parse_args(&["--aria2-runtime-selftest", "--seconds", "3601"]).is_err());
        assert!(parse_args(&["--aria2-runtime-selftest", "junk"]).is_err());
    }

    #[test]
    fn runtime_selftest_output_format_is_stable() {
        use ariahost_core::RuntimeEvent;
        assert_eq!(
            format_runtime_event(&RuntimeEvent::Starting),
            "STATE starting"
        );
        assert_eq!(
            format_runtime_event(&RuntimeEvent::Running {
                version: "1.37.0".to_owned(),
                pid: 4242,
            }),
            "STATE running rpc=1.37.0\nINFO pid=4242"
        );
        assert_eq!(
            format_runtime_event(&RuntimeEvent::Stopping),
            "STATE stopping"
        );
        assert_eq!(
            format_runtime_event(&RuntimeEvent::Stopped {
                exit_code: Some(0),
                forced: false,
            }),
            "STATE stopped exit=0"
        );
        assert_eq!(
            format_runtime_event(&RuntimeEvent::Stopped {
                exit_code: None,
                forced: true,
            }),
            "STATE stopped exit=none"
        );
        assert_eq!(
            format_runtime_event(&RuntimeEvent::Failed {
                code: Some(ariahost_common::ErrorCode::Aria2StartTimeout),
                hint: "超时".to_owned(),
            }),
            "STATE failed code=AH-E2002 hint=超时"
        );
    }

    #[test]
    fn panel_selftest_rejects_bad_frames_and_size() {
        assert!(parse_args(&["--panel-selftest", "--frames", "0"]).is_err());
        assert!(parse_args(&["--panel-selftest", "--frames", "abc"]).is_err());
        assert!(parse_args(&["--panel-selftest", "--frames", "-1"]).is_err());
        assert!(parse_args(&["--panel-selftest", "junk"]).is_err());
        assert!(parse_args(&["--panel-selftest", "--frames"]).is_err());
        // --size 的各种坏值
        assert!(parse_args(&["--panel-selftest", "--size", "3840"]).is_err());
        assert!(parse_args(&["--panel-selftest", "--size", "3840x"]).is_err());
        assert!(parse_args(&["--panel-selftest", "--size", "0x1080"]).is_err());
        assert!(parse_args(&["--panel-selftest", "--size", "100x1080"]).is_err());
        assert!(parse_args(&["--panel-selftest", "--size", "9999x1080"]).is_err());
        assert!(parse_args(&["--panel-selftest", "--size"]).is_err());
        // 重复参数
        assert!(parse_args(&[
            "--panel-selftest",
            "--size",
            "1920x1080",
            "--size",
            "1920x1080"
        ])
        .is_err());
        assert!(parse_args(&["--panel-selftest", "--frames", "5", "--frames", "6"]).is_err());
    }

    #[test]
    fn service_is_dispatched_to_svc_stub() {
        assert_eq!(parse_args(&["--service"]), Ok(Command::Service));
        // M1 的 ServiceHost 桩返回 Unsupported（AH-E5001），因此退出码是 1。
        assert_eq!(run_args(&["--service"]), EXIT_ERR);
    }

    /// `--install-service` / `--uninstall-service` **只测解析**。
    ///
    /// 单测**不得**执行这两个命令：它们真的改 SCM。
    /// - CI runner 是管理员 → 会把测试二进制注册成 `deps\ariahost-<hash>.exe` 服务且退出 0；
    /// - 非管理员 → 退出 1；结果依赖运行身份，不确定；
    /// - 更危险的是 `--uninstall-service`：在装了 AriaHost 的机器上会删掉**用户真实的**服务。
    ///
    /// 退出码/上屏文案的映射改用下面的纯函数测试（`install_report`/`uninstall_report`）。
    #[test]
    fn install_and_uninstall_are_parsed_only() {
        assert_eq!(
            parse_args(&["--install-service"]),
            Ok(Command::InstallService)
        );
        assert_eq!(
            parse_args(&["--uninstall-service"]),
            Ok(Command::UninstallService)
        );
    }

    /// 结果 → (文案, 退出码) 的纯函数契约：装成功/已存在都是 0，卸成功/本来没装也都是 0。
    ///
    /// 判据：这条用例在**管理员与非管理员身份下结果相同**，因为整条路径不碰 SCM。
    #[test]
    fn install_and_uninstall_outcomes_map_to_exit_codes() {
        for outcome in [
            ariahost_svc::InstallOutcome::Created,
            ariahost_svc::InstallOutcome::AlreadyExisted,
        ] {
            let (text, exit) = install_report(outcome);
            assert_eq!(exit, EXIT_OK, "{outcome:?} 必须是成功");
            assert!(!text.is_empty(), "{outcome:?} 必须有上屏文案");
        }
        // 幂等：服务本来在，重装不报错。
        assert!(install_report(ariahost_svc::InstallOutcome::AlreadyExisted)
            .0
            .contains("本来就在"));

        for outcome in [
            ariahost_svc::UninstallOutcome::Removed,
            ariahost_svc::UninstallOutcome::NotInstalled,
        ] {
            let (text, exit) = uninstall_report(outcome);
            assert_eq!(exit, EXIT_OK, "{outcome:?} 必须是成功");
            assert!(!text.is_empty(), "{outcome:?} 必须有上屏文案");
        }
        // 卸载的「本来没装」是幂等成功，文案要把这件事说清楚。
        assert!(
            uninstall_report(ariahost_svc::UninstallOutcome::NotInstalled)
                .0
                .contains("幂等")
        );
    }

    /// 服务类失败：**一律非 0 退出**，且必须带 `AH-E` 码（不许裸 Win32 码）。
    #[test]
    fn service_errors_always_fail_with_a_code() {
        use ariahost_svc::ServiceError;

        let cases = [
            ServiceError::Unsupported,
            ServiceError::NotFromScm {
                detail: "手工运行（Win32 1063）".to_owned(),
            },
            ServiceError::NeedsAdmin {
                detail: "打开本机 SCM（Win32 5）".to_owned(),
            },
            ServiceError::NotInstalled {
                detail: "服务不存在（Win32 1060）".to_owned(),
            },
            ServiceError::Unavailable {
                detail: "控制被拒".to_owned(),
            },
            ServiceError::RegisterPrereq {
                detail: "ProgramData 不可写".to_owned(),
            },
        ];
        for error in cases {
            let code = error.code().code();
            assert!(
                code.starts_with("AH-E"),
                "{error:?} 的错误码必须是 AH-E 前缀，实际 {code}"
            );
            assert_eq!(service_failure_exit_code(), EXIT_ERR, "服务类失败一律非 0");
        }
        assert_eq!(
            ServiceError::NeedsAdmin {
                detail: String::new()
            }
            .code()
            .code(),
            "AH-E5002"
        );
    }

    #[test]
    fn write_aumid_takes_one_path() {
        let path = r"C:\Users\Public\Desktop\AriaHost.lnk";
        assert_eq!(
            parse_args(&["--write-aumid", path]),
            Ok(Command::WriteAumid {
                lnk: PathBuf::from(path)
            })
        );
        // 路径不存在 → 必须**明确失败**（AH-E1001），不能假装写过。
        assert_eq!(run_args(&["--write-aumid", path]), EXIT_ERR);
    }

    #[test]
    fn diagnose_takes_an_optional_out_dir() {
        assert_eq!(
            parse_args(&["--diagnose"]),
            Ok(Command::Diagnose { out: None })
        );
        assert_eq!(
            parse_args(&["--diagnose", "--out", r"F:\tmp\diag"]),
            Ok(Command::Diagnose {
                out: Some(PathBuf::from(r"F:\tmp\diag"))
            })
        );
        assert_eq!(
            parse_args(&["--diagnose", "--out=F:/tmp/diag2"]),
            Ok(Command::Diagnose {
                out: Some(PathBuf::from("F:/tmp/diag2"))
            })
        );
        // 缺目录值 / 空值 / 不认识的尾参：一律 AH-E1001
        for bad in [
            vec!["--diagnose", "--out"],
            vec!["--diagnose", "--out", "  "],
            vec!["--diagnose", "--out="],
            vec!["--diagnose", "extra"],
            vec!["--diagnose", "--out", "a", "b"],
        ] {
            let error = parse_args(&bad).expect_err(&format!("应报用法错误：{bad:?}"));
            assert_eq!(error.code(), "AH-E1001");
        }
    }

    /// `--core-tag-selftest`：tag 必填，`--dry-run` 位置自由但只许一次。
    #[test]
    fn core_tag_selftest_requires_exactly_one_tag() {
        assert_eq!(
            parse_args(&["--core-tag-selftest", "1.37.0"]),
            Ok(Command::CoreTagSelftest {
                tag: "1.37.0".to_owned(),
                dry_run: false
            })
        );
        assert_eq!(
            parse_args(&["--core-tag-selftest", "--dry-run", "release-1.37.0"]),
            Ok(Command::CoreTagSelftest {
                tag: "release-1.37.0".to_owned(),
                dry_run: true
            })
        );
        assert_eq!(
            parse_args(&["--core-tag-selftest", "1.37.0", "--dry-run"]),
            Ok(Command::CoreTagSelftest {
                tag: "1.37.0".to_owned(),
                dry_run: true
            })
        );
        for bad in [
            vec!["--core-tag-selftest"],
            vec!["--core-tag-selftest", "1.37.0", "1.36.0"],
            vec!["--core-tag-selftest", "1.37.0", "--dry-run", "--dry-run"],
            vec!["--core-tag-selftest", "1.37.0", "--bogus"],
        ] {
            let error = parse_args(&bad).expect_err(&format!("应报用法错误：{bad:?}"));
            assert_eq!(error.code(), "AH-E1001");
        }
    }

    /// `--core-list-selftest` 不接受任何尾参（它就是一份 JSON 输出）。
    #[test]
    fn core_list_selftest_takes_no_arguments() {
        assert_eq!(
            parse_args(&["--core-list-selftest"]),
            Ok(Command::CoreListSelftest)
        );
        let error = parse_args(&["--core-list-selftest", "extra"]).expect_err("尾参应报错");
        assert_eq!(error.code(), "AH-E1001");
    }

    /// `--quit` 的两条通道分工：stdout 只有一行纯 ASCII 稳定键，中文解释一律 stderr。
    ///
    /// 这条测试钉的是**通道**而不是文案：安装器抓 stdout 后会经 codepage 转换，
    /// 中文落到 stdout 会让「全等比对」的脚本永远假红。
    #[test]
    fn quit_stdout_is_one_ascii_key_line_and_chinese_goes_to_stderr() {
        let outcomes = [
            QuitOutcome::NoInstance,
            QuitOutcome::Signalled,
            QuitOutcome::Refused("version-mismatch".to_owned()),
            QuitOutcome::Timeout(TimeoutPhase::Handshake),
            QuitOutcome::Timeout(TimeoutPhase::ExitWait),
            QuitOutcome::Timeout(TimeoutPhase::Watchdog),
        ];
        for outcome in &outcomes {
            let line = quit_stdout_line(outcome);
            assert!(
                line.starts_with("quit: "),
                "stdout 行必须以 `quit: ` 开头：{line:?}"
            );
            assert!(!line.contains('\n'), "stdout 只能是单行：{line:?}");
            assert!(
                line.is_ascii(),
                "stdout 只放稳定键（中文会让安装器 codepage 转换变乱码）：{line:?}"
            );
            let stderr = quit_stderr_lines(outcome);
            match outcome {
                QuitOutcome::Signalled => assert!(stderr.is_empty(), "成功且无需解释"),
                _ => assert!(
                    stderr.iter().any(|text| !text.is_ascii()),
                    "中文解释必须走 stderr：{stderr:?}"
                ),
            }
        }
        // 三种 timeout 共用同一个 stdout 键（安装器契约不变），但 stderr 的处置建议必须不同——
        // 「没送达」要用户去确认面板在不在，「送达未退完」要用户别强杀。
        assert_eq!(
            quit_stdout_line(&QuitOutcome::Timeout(TimeoutPhase::Handshake)),
            quit_stdout_line(&QuitOutcome::Timeout(TimeoutPhase::ExitWait))
        );
        let handshake =
            quit_stderr_lines(&QuitOutcome::Timeout(TimeoutPhase::Handshake)).join("\n");
        let exit_wait = quit_stderr_lines(&QuitOutcome::Timeout(TimeoutPhase::ExitWait)).join("\n");
        assert_ne!(
            handshake, exit_wait,
            "「没送达」与「送达未退完」必须给出不同处置文案"
        );
        assert!(
            handshake.contains("没收到"),
            "未送达要说明是握手阶段：{handshake}"
        );
        assert!(
            exit_wait.contains("已收到面板应答"),
            "退不完要说明应答已收到：{exit_wait}"
        );
        // Watchdog 与 Handshake 同属「没送达」（真机实测：服务端接管道不回应答 → 读阻塞 →
        // 看门狗收尾）。它绝不能说成「卡在保存会话」，那是把没送达误报成退得慢。
        let watchdog = quit_stderr_lines(&QuitOutcome::Timeout(TimeoutPhase::Watchdog)).join("\n");
        assert_eq!(
            quit_stdout_line(&QuitOutcome::Timeout(TimeoutPhase::Watchdog)),
            "quit: timeout"
        );
        assert!(
            watchdog.contains("没收到面板应答"),
            "看门狗到期也要说明没收到应答：{watchdog}"
        );
        assert!(
            !watchdog.contains("保存会话"),
            "看门狗到期不属于「送达未退完」，不得沿用延后处置文案：{watchdog}"
        );
        assert_ne!(watchdog, handshake, "两条发现路径的文案仍应可区分");
        // 四态的键值本身也是契约（安装器按它判断）。
        assert_eq!(
            quit_stdout_line(&QuitOutcome::NoInstance),
            "quit: no-instance"
        );
        assert_eq!(quit_stdout_line(&QuitOutcome::Signalled), "quit: accepted");
        assert_eq!(
            quit_stdout_line(&QuitOutcome::Timeout(TimeoutPhase::Handshake)),
            "quit: timeout"
        );
        assert!(
            quit_stdout_line(&QuitOutcome::Refused("x".to_owned())).starts_with("quit: refused ")
        );
    }

    /// JSON 形状是面板/安装器/脚本的契约：字段顺序、取值、缺值写法都不能变。
    #[test]
    fn service_status_json_shape_is_stable() {
        let not_installed = ariahost_svc::ServiceStatusReport {
            installed: false,
            state: ariahost_svc::ServiceState::NotInstalled,
            start_type: None,
            pid: None,
        };
        assert_eq!(
            service_status_json(&not_installed),
            r#"{"installed":false,"state":"not-installed","start_type":null,"pid":null}"#
        );

        let running = ariahost_svc::ServiceStatusReport {
            installed: true,
            state: ariahost_svc::ServiceState::Running,
            start_type: Some(ariahost_svc::ServiceStartMode::Demand),
            pid: Some(4242),
        };
        assert_eq!(
            service_status_json(&running),
            r#"{"installed":true,"state":"running","start_type":"demand","pid":4242}"#
        );
    }

    #[test]
    fn service_status_is_parsed_and_never_prints_chinese_state() {
        assert_eq!(
            parse_args(&["--service-status"]),
            Ok(Command::ServiceStatus)
        );
        // 平台相关：非 Windows 上必然失败（AH-E5001），Windows 上未装也是成功（installed:false）。
        let code = run_args(&["--service-status"]);
        if cfg!(windows) {
            assert_eq!(code, EXIT_OK, "Windows 上未装服务也必须退出 0");
        } else {
            assert_eq!(code, EXIT_ERR, "非 Windows 上没有 SCM，必须明确失败");
        }
    }

    #[test]
    fn web_selftest_is_parsed_and_runs() {
        assert_eq!(parse_args(&["--web-selftest"]), Ok(Command::WebSelftest));
        // 自检自己会绑一个临时端口；通了就是 0，占端口等环境问题就是 1。
        let code = run_args(&["--web-selftest"]);
        assert!(code == EXIT_OK || code == EXIT_ERR);
    }

    #[test]
    fn write_aumid_without_path_is_usage_error() {
        let error = parse_args(&["--write-aumid"]).expect_err("缺参数应为用法错误");
        assert_eq!(error.code(), ErrorCode::Usage.code());
        assert_eq!(run_args(&["--write-aumid"]), EXIT_ERR);
    }

    #[test]
    fn write_aumid_rejects_empty_or_extra_paths() {
        assert!(parse_args(&["--write-aumid", ""]).is_err());
        assert!(parse_args(&["--write-aumid", "a.lnk", "b.lnk"]).is_err());
    }

    #[test]
    fn version_flags_are_equivalent() {
        for flag in ["--version", "-V"] {
            assert_eq!(parse_args(&[flag]), Ok(Command::Version));
            assert_eq!(run_args(&[flag]), EXIT_OK);
        }
    }

    #[test]
    fn help_lists_all_subcommands_and_exit_codes() {
        for flag in ["--help", "-h"] {
            assert_eq!(parse_args(&[flag]), Ok(Command::Help));
            assert_eq!(run_args(&[flag]), EXIT_OK);
        }
        for needle in [
            "--service",
            "--install-service",
            "--uninstall-service",
            "--service-status",
            "--write-aumid",
            "--quit",
            "--version",
            "--help",
            "--backend",
            "退出码",
        ] {
            assert!(USAGE.contains(needle), "帮助文本缺少 {needle}");
        }
        // `--panel-selftest` 是隐藏入口：帮助里不出现（但它必须真能工作，测试见上）。
        assert!(
            !USAGE.contains("--panel-selftest"),
            "自测入口不该出现在 --help 里"
        );
    }

    #[test]
    fn unknown_argument_is_usage_error() {
        let error = parse_args(&["--bogus"]).expect_err("未知参数应为用法错误");
        assert_eq!(error.code(), ErrorCode::Usage.code());
        assert_eq!(run_args(&["--bogus"]), EXIT_ERR);
    }

    #[test]
    fn recognized_flags_reject_extra_arguments() {
        assert!(parse_args(&["--service", "extra"]).is_err());
        assert!(parse_args(&["--version", "extra"]).is_err());
    }

    #[test]
    fn quit_has_four_outcomes_with_pinned_exit_codes() {
        assert_eq!(parse_args(&["--quit"]), Ok(Command::Quit));
        // 没有实例 = 安装器想要的结果 = 成功（M3-f 之前回 1，会让卸载/升级报假错）。
        assert_eq!(QuitOutcome::NoInstance.exit_code(), EXIT_OK);
        assert_eq!(QuitOutcome::Signalled.exit_code(), EXIT_OK);
        assert_eq!(
            QuitOutcome::Refused("refused unknown-command:x".to_owned()).exit_code(),
            EXIT_ERR
        );
        // 三种超时阶段共用同一个退出码（安装器只认 0/1/2）。
        for phase in [
            TimeoutPhase::Handshake,
            TimeoutPhase::ExitWait,
            TimeoutPhase::Watchdog,
        ] {
            assert_eq!(QuitOutcome::Timeout(phase).exit_code(), EXIT_TIMEOUT);
        }
    }

    #[test]
    fn quit_text_prefixes_are_stable() {
        // 脚本/安装器按前缀判断，不接受本地化改动。
        assert_eq!(QuitOutcome::NoInstance.as_str(), "no-instance");
        assert_eq!(QuitOutcome::Signalled.as_str(), "accepted");
        assert_eq!(
            QuitOutcome::Refused("refused x".to_owned()).as_str(),
            "refused"
        );
        assert_eq!(
            QuitOutcome::Timeout(TimeoutPhase::Handshake).as_str(),
            "timeout"
        );
    }

    #[test]
    fn quit_without_instance_succeeds() {
        // 本机（含 CI）通常没有面板在跑：这条路径必须回 0，且只打一行 quit: 前缀。
        // 若恰好有实例在跑，真实握手也可能回 0（accepted）或 2（超时）——所以只断言「不是被拒」。
        let code = run_args(&["--quit"]);
        assert!(
            code == EXIT_OK || code == EXIT_TIMEOUT,
            "--quit 在没有实例时必须回 0（实际 {code}）"
        );
    }
}
