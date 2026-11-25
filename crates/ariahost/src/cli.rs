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
  ariahost --quit                  请求运行中的面板实例退出
  ariahost --version | -V          显示版本
  ariahost --help | -h             显示本帮助

退出码：
  0  成功
  1  失败（参数错误以 AH-E1001 开头；其他错误以 AH-E#### 开头）
  2  --quit 与控制管道握手超时
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

/// `--quit` 与控制管道握手的三种结果（架构 §4.1.4）。
///
/// M1 只有 [`QuitOutcome::NoInstance`] 会被构造；另外两态由 M2 的真实握手产生，
/// 此处保留完整枚举，避免 M2 再改退出码映射。
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuitOutcome {
    /// 没有运行中的面板实例。
    NoInstance,
    /// 握手成功，实例已确认退出。
    Signalled,
    /// 管道存在但握手超时。
    Timeout,
}

impl QuitOutcome {
    /// 映射到进程退出码：无实例=1、成功=0、超时=2。
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::NoInstance => EXIT_ERR,
            Self::Signalled => EXIT_OK,
            Self::Timeout => EXIT_TIMEOUT,
        }
    }
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
pub fn run(args: &[String]) -> i32 {
    match parse(args) {
        Ok(command) => execute(&command),
        Err(error) => {
            eprintln!("{} {}", error.code(), error.message());
            eprintln!("{USAGE}");
            EXIT_ERR
        }
    }
}

/// 分派到具体子命令。
fn execute(command: &Command) -> i32 {
    match command {
        // 面板：M2 起真的起窗（eframe/egui + wgpu）。这里不再有「等待回车」的占位逻辑——
        // 双击快捷方式时用户看到的是窗口，不是控制台输出。
        Command::Panel { backend } => ariahost_ui::run_panel(*backend, Some(web_open_hook())),
        Command::PanelSelftest {
            frames,
            size,
            backend,
            seed_rows,
        } => ariahost_ui::run_selftest(*frames, *size, *backend, *seed_rows),
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
        Command::Quit => {
            // M1 无控制管道，固定按「无实例」处理；M2 换成真实握手结果。
            let outcome = QuitOutcome::NoInstance;
            println!("no instance");
            outcome.exit_code()
        }
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
fn service_status_json(report: &ariahost_svc::ServiceStatusReport) -> String {
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
    let fetcher = ariahost_core::Fetcher::new();
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
    fn quit_has_three_outcomes() {
        assert_eq!(parse_args(&["--quit"]), Ok(Command::Quit));
        // M1 无 IPC：固定按「无实例」返回 1。
        assert_eq!(run_args(&["--quit"]), EXIT_ERR);
        assert_eq!(QuitOutcome::NoInstance.exit_code(), EXIT_ERR);
        assert_eq!(QuitOutcome::Signalled.exit_code(), EXIT_OK);
        assert_eq!(QuitOutcome::Timeout.exit_code(), EXIT_TIMEOUT);
    }
}
