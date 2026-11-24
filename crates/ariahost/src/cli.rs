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
  ariahost --backend <name>        启动面板并指定渲染后端（auto|dx12|vulkan）
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
    /// 无参数（或只给 `--backend`）：启动面板（M2 起真的开窗）。
    Panel {
        /// 渲染后端（`--backend`，默认 [`ariahost_ui::Backend::Auto`]）。
        backend: ariahost_ui::Backend,
    },
    /// `--panel-selftest [--frames N] [--size WxH] [--backend ...]`：
    /// 隐藏自测入口，开窗渲染 N 帧后打印机器可读行并退出。
    PanelSelftest {
        /// 渲染帧数（尺寸到位后才开始计）。
        frames: u32,
        /// 目标窗口物理像素尺寸 `(宽, 高)`；用于验证大于 2048 的交换链。
        size: Option<(u32, u32)>,
        /// 渲染后端（`--backend`）。
        backend: ariahost_ui::Backend,
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

/// 解析 `--panel-selftest` 的尾部参数：`--frames N`、`--size WxH`、`--backend <name>`，
/// 顺序自由、各至多一次。
///
/// 这是隐藏入口（不进 `--help`），供验收/CI 在无人值守下量内存与交换链上限：
/// 开窗渲染 N 帧后打印 `SELFTEST adapter=... atlas=... surface=... backend=... ok=1` 再退出。
/// `--size` 是**物理像素**，窗口允许超出屏幕——这是验证 1440p/4K 交换链的手段；
/// `--backend` 用来把 DX12/Vulkan 单独钉死（本机 wgpu 自选 Vulkan，DX12 从未被验证过）。
fn parse_selftest(rest: &[String]) -> Result<Command, UsageError> {
    let mut frames = ariahost_ui::DEFAULT_SELFTEST_FRAMES;
    let mut frames_seen = false;
    let mut size = None;
    let mut backend = ariahost_ui::Backend::Auto;
    let mut backend_seen = false;
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
    })
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
        Command::Panel { backend } => ariahost_ui::run_panel(*backend),
        Command::PanelSelftest {
            frames,
            size,
            backend,
        } => ariahost_ui::run_selftest(*frames, *size, *backend),
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
            })
        );
        assert_eq!(
            parse_args(&["--panel-selftest", "--frames", "5"]),
            Ok(Command::PanelSelftest {
                frames: 5,
                size: None,
                backend: ariahost_ui::Backend::Auto,
            })
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
            })
        );
        assert_eq!(
            parse_args(&["--panel-selftest", "--size", "3840x2160", "--frames", "30"]),
            Ok(Command::PanelSelftest {
                frames: 30,
                size: Some((3840, 2160)),
                backend: ariahost_ui::Backend::Auto,
            })
        );
        assert_eq!(
            parse_args(&["--panel-selftest", "--frames", "30", "--size", "1920x1080"]),
            Ok(Command::PanelSelftest {
                frames: 30,
                size: Some((1920, 1080)),
                backend: ariahost_ui::Backend::Auto,
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
            })
        );
        assert_eq!(
            parse_args(&["--panel-selftest", "--frames", "3", "--backend", "vulkan"]),
            Ok(Command::PanelSelftest {
                frames: 3,
                size: None,
                backend: ariahost_ui::Backend::Vulkan,
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
