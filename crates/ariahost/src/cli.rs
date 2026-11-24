//! 命令行契约（架构 §4.1.4）。
//!
//! 分支、错误码、退出码必须精确：安装器、面板与 CI 都依赖这张表。
//!
//! 分派顺序硬约束：先解析 `argv[1..]`，再决定是否创建/检查单实例锁；
//! 短命助手（安装/卸载/AUMID/`--quit`）一律不参与单实例。
//!
//! M2 起「无参数」真的起窗（`crates/ariahost-ui`）；`--service` 与三个短命助手仍是桩。
//! `--panel-selftest` 是隐藏自测入口（不出现在 `--help` 里），供无人值守量测内存用。

use std::path::PathBuf;

use ariahost_common::{ErrorCode, BINARY_NAME, EXIT_ERR, EXIT_OK, EXIT_TIMEOUT, VERSION};

/// 用法文本：`--help` 与用法错误共用，中文，列出全部子命令与退出码。
pub const USAGE: &str = "\
AriaHost — aria2 Windows 服务管理器

用法：
  ariahost                         启动面板
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
    /// 无参数：启动面板（M2 起真的开窗）。
    Panel,
    /// `--panel-selftest [--frames N]`：隐藏自测入口，开窗渲染 N 帧后打印机器可读行并退出。
    PanelSelftest {
        /// 渲染帧数。
        frames: u32,
    },
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

    if first == "--panel-selftest" {
        return parse_selftest(rest);
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

/// 解析 `--panel-selftest` 的尾部参数：`[]` 或 `["--frames", N]`。
///
/// 这是隐藏入口（不进 `--help`），供验收/CI 在无人值守下量内存：
/// 开窗渲染 N 帧后打印 `SELFTEST adapter=... atlas=... private_kb=...` 再退出。
fn parse_selftest(rest: &[String]) -> Result<Command, UsageError> {
    match rest {
        [] => Ok(Command::PanelSelftest {
            frames: ariahost_ui::DEFAULT_SELFTEST_FRAMES,
        }),
        [flag, value] if flag == "--frames" => match value.parse::<u32>() {
            Ok(frames) if frames > 0 => Ok(Command::PanelSelftest { frames }),
            Ok(_) => Err(UsageError::new("--frames 必须大于 0")),
            Err(_) => Err(UsageError::new(format!(
                "--frames 需要正整数，收到：{value}"
            ))),
        },
        _ => Err(UsageError::new(
            "--panel-selftest 只接受可选的 --frames <帧数>",
        )),
    }
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
        Command::Panel => ariahost_ui::run_panel(),
        Command::PanelSelftest { frames } => ariahost_ui::run_selftest(*frames),
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
        assert_eq!(parse_args(&[]), Ok(Command::Panel));
    }

    #[test]
    fn panel_selftest_parses_with_optional_frames() {
        assert_eq!(
            parse_args(&["--panel-selftest"]),
            Ok(Command::PanelSelftest {
                frames: ariahost_ui::DEFAULT_SELFTEST_FRAMES
            })
        );
        assert_eq!(
            parse_args(&["--panel-selftest", "--frames", "5"]),
            Ok(Command::PanelSelftest { frames: 5 })
        );
    }

    #[test]
    fn panel_selftest_rejects_bad_frames() {
        assert!(parse_args(&["--panel-selftest", "--frames", "0"]).is_err());
        assert!(parse_args(&["--panel-selftest", "--frames", "abc"]).is_err());
        assert!(parse_args(&["--panel-selftest", "--frames", "-1"]).is_err());
        assert!(parse_args(&["--panel-selftest", "junk"]).is_err());
        assert!(parse_args(&["--panel-selftest", "--frames"]).is_err());
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
