//! AriaHost 入口（架构 §4.1.4）。
//!
//! **面板入口自 M2 起真的起窗**（`crates/ariahost-ui`：eframe/egui + wgpu，只做技术闸门切片，
//! 不接服务/RPC/任务列表）。`--service` 与三个短命助手仍是桩。
//!
//! 分派顺序硬约束：先解析 argv，再决定是否进入单实例锁；短命助手不参与单实例。
//! 面板的单实例锁在 `ariahost_ui::run_panel` 内部获取（`%LOCALAPPDATA%\AriaHost\panel.lock`）。
//!
//! **子系统**：Windows 上是 GUI 子系统（双击不弹黑窗）。代价是 stdout 默认没接控制台，
//! 所以 CLI 分支在输出前按需附着父控制台——见 [`console`]。Cargo 的 `[[bin]]` 没有
//! 「按构建 profile 选子系统」的能力，只能在 crate 根下这个属性，两条路径靠代码弥合。
//!
//! **文件日志**（需求 F11）：进程一起来就按平面初始化 `ariahost.log` 写入器（见
//! [`ariyahost_common::logfile`]），短命助手也留一行痕迹——卸载/升级排障时「安装器到底调没调
//! `--quit`」正是要看的那一行。

// GUI 子系统：Windows 双击 exe 不再先弹一个控制台黑窗（本批 task-48 的起因）。
// 非 Windows 一律不加（那是 Windows-only 属性）。
#![cfg_attr(windows, windows_subsystem = "windows")]

mod aumid;
mod cli;
mod console;
mod diagnose;

use ariahost_common::logfile::{self, Level, LOG_FILE_NAME};
use ariahost_common::paths::{logs_dir, Plane};
use ariahost_common::VERSION;

fn main() {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();

    // 隐藏自检入口（与 `--panel-selftest` 同风格：不进 `--help`，只给无人值守取证用）。
    if args.first().map(String::as_str) == Some("--log-selftest") {
        std::process::exit(log_selftest(&args[1..]));
    }

    // 文件日志：只在真正干活的模式初始化。`--help`/`--version` 这类纯信息命令不该在用户盘上
    // 建目录。`--service` 落服务平面（`%ProgramData%`），其余落用户平面（`%APPDATA%`）。
    let informational = args
        .iter()
        .any(|arg| arg == "--help" || arg == "-h" || arg == "--version" || arg == "-V");
    if !informational {
        let plane = if args.iter().any(|arg| arg == "--service") {
            Plane::Service
        } else {
            Plane::User
        };
        logfile::init(plane);
        logfile::write(
            Level::Info,
            "main",
            &format!(
                "ariahost {VERSION} 启动（plane={plane:?}, argv={}）",
                if args.is_empty() {
                    "<无>".to_owned()
                } else {
                    args.join(" ")
                }
            ),
        );
    }

    std::process::exit(cli::run(&args));
}

/// `--log-selftest [--lines N] [--fill MIB] [--service-plane] [--dir <路径>]`：无人值守验文件日志。
///
/// 做三件事：写几行（含三种秘密形态）→ 可选灌 `--fill` MiB 逼出轮转 → 打印目录清单与
/// 「落盘内容里还有没有明文」的判定。输出前缀固定 `LOGTEST `，供脚本 grep。
/// `--dir` 只给取证/单机排障用（跳过 `%APPDATA%` 解析，避免环境权限问题挡住验证）。
fn log_selftest(args: &[String]) -> i32 {
    let mut lines = 6usize;
    let mut fill_mib = 0u64;
    let mut plane = Plane::User;
    let mut override_dir: Option<std::path::PathBuf> = None;
    let mut index = 0usize;
    while index < args.len() {
        match args[index].as_str() {
            "--lines" => match args.get(index + 1).and_then(|raw| raw.parse().ok()) {
                Some(value) => {
                    lines = value;
                    index += 2;
                }
                None => {
                    eprintln!("--log-selftest：--lines 需要一个整数");
                    return 1;
                }
            },
            "--fill" => match args.get(index + 1).and_then(|raw| raw.parse().ok()) {
                Some(value) => {
                    fill_mib = value;
                    index += 2;
                }
                None => {
                    eprintln!("--log-selftest：--fill 需要一个整数（MiB）");
                    return 1;
                }
            },
            "--dir" => match args.get(index + 1) {
                Some(value) => {
                    override_dir = Some(std::path::PathBuf::from(value));
                    index += 2;
                }
                None => {
                    eprintln!("--log-selftest：--dir 需要一个路径");
                    return 1;
                }
            },
            "--service-plane" => {
                plane = Plane::Service;
                index += 1;
            }
            other => {
                eprintln!("--log-selftest：不认识的参数 {other}");
                return 1;
            }
        }
    }

    let dir = match override_dir {
        Some(dir) => dir,
        None => match logs_dir(plane) {
            Some(dir) => dir,
            None => {
                eprintln!("--log-selftest：拿不到 {plane:?} 平面的日志目录");
                return 1;
            }
        },
    };
    logfile::init_at(&dir, logfile::Limits::default());
    println!("LOGTEST dir={}", dir.display());

    // 三种形态各一行（门槛测试要能 grep 到「没有明文」）+ 一条普通行。
    logfile::write(Level::Info, "selftest", "第 1 行：面板启动（普通日志）");
    logfile::write(
        Level::Info,
        "rpc",
        "第 2 行：call params=[\"token:selftest_token_zzz\"]",
    );
    logfile::write(
        Level::Warn,
        "conf",
        "第 3 行：aria2c --rpc-secret=selftest_secret_yyy",
    );
    logfile::write(
        Level::Info,
        "task",
        "第 4 行：magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=x",
    );
    logfile::write(Level::Error, "ui", "第 5 行：故意写入的 ERROR 级样例");
    for extra in 6..=lines {
        logfile::write(Level::Debug, "selftest", &format!("第 {extra} 行：填充"));
    }

    if fill_mib > 0 {
        // 每行 ~1 KiB：写满 fill_mib 个 MiB，逼出轮转（单文件上限 10 MiB）。
        let payload = "f".repeat(1000);
        let per_mib = 1024u64;
        let mut written = 0u64;
        for round in 0..(fill_mib * per_mib) {
            logfile::write(Level::Debug, "fill", &format!("fill {round:06} {payload}"));
            written += 1024;
        }
        println!("LOGTEST fill_bytes={written}");
    }

    let Some(current) = logfile::current_path() else {
        eprintln!("--log-selftest：初始化后仍拿不到日志路径（日志设施降级了？）");
        return 1;
    };
    let text = std::fs::read_to_string(&current).unwrap_or_default();
    let head = text.lines().next().unwrap_or_default().to_owned();
    println!("LOGTEST path={}", current.display());
    println!("LOGTEST head={head}");
    println!("LOGTEST head_len={}", head.len());
    println!("LOGTEST lines={}", text.lines().count());

    // 目录清单（按名字排序，便于与「保留总数 5」对照）。
    let mut files: Vec<(String, u64)> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .filter_map(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    if !name.starts_with(LOG_FILE_NAME) {
                        return None;
                    }
                    let size = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
                    Some((name, size))
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    let listing = files
        .iter()
        .map(|(name, size)| format!("{name}:{size}"))
        .collect::<Vec<_>>()
        .join(",");
    println!("LOGTEST files={listing}");
    println!("LOGTEST file_count={}", files.len());

    // 脱敏门禁：把目录里所有 ariahost.log* 拼起来找三种明文。
    let mut all = String::new();
    for (name, _) in &files {
        all.push_str(&std::fs::read_to_string(dir.join(name)).unwrap_or_default());
    }
    let secrets = [
        "selftest_token_zzz",
        "selftest_secret_yyy",
        "0123456789abcdef0123456789abcdef01234567",
    ];
    let leaked: Vec<&str> = secrets
        .iter()
        .copied()
        .filter(|secret| all.contains(secret))
        .collect();
    println!("LOGTEST leak={}", !leaked.is_empty());
    if !leaked.is_empty() {
        eprintln!("--log-selftest：落盘内容里仍有明文：{leaked:?}");
        return 1;
    }
    println!("LOGTEST placeholder={}", all.contains("***"));
    0
}
