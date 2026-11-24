//! 命令行契约（架构 §4.1.4）。
//!
//! 本批全部是桩，但**分支、错误码、退出码必须精确**：安装器、面板与 CI 都依赖这张表。
//!
//! 分派顺序硬约束：先解析 `argv[1..]`，再决定是否创建/检查单实例 mutex；
//! 短命助手（安装/卸载/AUMID/`--quit`）一律不参与 mutex。
//! 本批尚未实现 mutex 与控制管道，因此 `--quit` 固定按「无实例」返回；
//! M2 接入管道后按真实握手结果返回 0/1/2。

use std::path::PathBuf;

use ariahost_common::{ErrorCode, BINARY_NAME, EXIT_ERR, EXIT_OK, EXIT_TIMEOUT, VERSION};

/// 用法文本：`--help` 与用法错误共用，中文，列出全部子命令与退出码。
pub const USAGE: &str = "\
AriaHost — aria2 Windows 服务管理器

用法：
  ariahost                         启动面板（M2 实现）
  ariahost --service               以 Windows 服务方式运行（ServiceHost）
  ariahost --install-service       注册 Windows 服务（需管理员）
  ariahost --uninstall-service     注销 Windows 服务（需管理员）
  ariahost --write-aumid <lnk>     给 .lnk 快捷方式写入 AUMID
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
    /// 无参数：面板（M2 才建窗口）。
    Panel,
    /// `--service`：ServiceHost。
    Service,
    /// `--install-service`：注册 Windows 服务（短命助手）。
    InstallService,
    /// `--uninstall-service`：注销 Windows 服务（短命助手）。
    UninstallService,
    /// `--write-aumid <lnk>`：给快捷方式写 AUMID（短命助手）。
    WriteAumid {
        /// 目标 `.lnk` 路径。
        lnk: PathBuf,
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
        return Ok(Command::Panel);
    };
    let rest = &args[1..];

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
        "--install-service" => Command::InstallService,
        "--uninstall-service" => Command::UninstallService,
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

/// 执行一条命令并返回进程退出码。
///
/// 本批所有业务分支都是桩：只打印契约要求的占位信息，不做真实动作。
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

/// 分派到具体子命令（全部为桩实现）。
fn execute(command: &Command) -> i32 {
    match command {
        Command::Panel => {
            // M1 桩：图形面板在 M2 实现。双击快捷方式时这是用户唯一能看到的输出，所以：
            //   ① 必须是中文、可读、说明「下一步该看什么」；
            //   ② 不能一闪而过（v0.1.0 只 println 一行英文就 exit 0，用户报的是「启动闪退」，
            //      见 docs/dev/release.md §9 记录）。M2 面板落地后删掉本分支与等待逻辑。
            println!("AriaHost {VERSION}（预览版：命令行内核）");
            println!();
            println!("图形面板尚未实现（计划在 M2 提供）。当前版本可用的命令：");
            println!("  ariahost --help             查看全部命令与用法");
            println!("  ariahost --version          显示版本");
            println!("  ariahost --service          以 Windows 服务方式运行（ServiceHost）");
            println!();
            println!("说明：本程序是 aria2 的服务管理器，不是下载器本身；");
            println!("      面板、托盘、任务列表在 M2 交付，届时双击即开窗口。");
            wait_for_enter_if_console();
            EXIT_OK
        }
        Command::Service => match ariahost_svc::run() {
            Ok(()) => EXIT_OK,
            Err(error) => {
                eprintln!("{error}");
                EXIT_ERR
            }
        },
        Command::InstallService => {
            println!("install-service: not implemented (M1 skeleton)");
            EXIT_OK
        }
        Command::UninstallService => {
            println!("uninstall-service: not implemented (M1 skeleton)");
            EXIT_OK
        }
        Command::WriteAumid { lnk } => {
            println!(
                "write-aumid: not implemented (M1 skeleton): {}",
                lnk.display()
            );
            EXIT_OK
        }
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

/// 双击快捷方式时控制台是新建的，进程一退出窗口立刻消失——用户看到的就是「启动闪退」。
/// 只在 stdin 是终端时等待，避免 CI / 管道 / 服务场景（stdin 被重定向或为空）被阻塞。
fn wait_for_enter_if_console() {
    use std::io::{IsTerminal, Write};

    if !std::io::stdin().is_terminal() {
        return;
    }
    print!("按回车键退出…");
    let _ = std::io::stdout().flush();
    let mut discard = String::new();
    let _ = std::io::stdin().read_line(&mut discard);
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

    #[test]
    fn no_args_is_panel() {
        assert_eq!(parse_args(&[]), Ok(Command::Panel));
        assert_eq!(run_args(&[]), EXIT_OK);
    }

    #[test]
    fn service_is_dispatched_to_svc_stub() {
        assert_eq!(parse_args(&["--service"]), Ok(Command::Service));
        // M1 的 ServiceHost 桩返回 Unsupported（AH-E5001），因此退出码是 1。
        assert_eq!(run_args(&["--service"]), EXIT_ERR);
    }

    #[test]
    fn install_and_uninstall_are_short_lived_helpers() {
        assert_eq!(
            parse_args(&["--install-service"]),
            Ok(Command::InstallService)
        );
        assert_eq!(
            parse_args(&["--uninstall-service"]),
            Ok(Command::UninstallService)
        );
        assert_eq!(run_args(&["--install-service"]), EXIT_OK);
        assert_eq!(run_args(&["--uninstall-service"]), EXIT_OK);
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
        assert_eq!(run_args(&["--write-aumid", path]), EXIT_OK);
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
            "--write-aumid",
            "--quit",
            "--version",
            "--help",
            "退出码",
        ] {
            assert!(USAGE.contains(needle), "帮助文本缺少 {needle}");
        }
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
