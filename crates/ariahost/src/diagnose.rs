//! 诊断包导出（`ariahost --diagnose [--out <dir>]`，需求 F11）。
//!
//! ## 为什么要有它
//!
//! 用户报「点 X 之后卡住」时，我们此前只能让他手工截图 + 满硬盘找日志；而日志里混着
//! RPC secret、代理密码、磁力 infohash。这个模块把「排障需要的东西」打成一个 zip：
//!
//! | zip 内路径 | 来源 | 脱敏 |
//! |---|---|---|
//! | `config.redacted.toml` | 当前配置 | **结构化替换**：`aria2.rpc.secret`、代理密码、tracker passkey |
//! | `logs/user/ariahost.log*`、`logs/user/aria2.log*` | 用户平面日志（含轮转件） | 逐文件过 [`scrub`] |
//! | `logs/service/service.log*`、`logs/service/aria2.log*` | 服务平面日志 | 同上 |
//! | `state.json` | 运行态快照（见下） | 只读：不联网、不启 aria2c |
//! | `env.json` | OS/架构/版本/安装形态/是否管理员 | 只留用户名，无机器名 |
//! | `README.txt` | 一行说明 + 生成时间 + 版本 | — |
//!
//! ## 三条硬纪律
//!
//! 1. **脱敏在打包前做**：zip 里的字节已经是安全的，不是「我保证不打开看」。
//! 2. **只读**：不写配置、不生成 secret（用 `config::peek_secret` 而不是 `resolve_secret`——
//!    后者会在 secret 缺失时造一个新的，那会让正在跑的 aria2c 全部鉴权失败）、
//!    不启动 aria2c。RPC 只探一次，带 2s 超时。
//! 3. **不覆盖**：文件名带秒级时间戳；真撞上同名（同一秒导两次）就退到 `-1`、`-2`，
//!    已有文件一个字节都不动。
//!
//! ## state.json 形状
//!
//! ```json
//! {
//!   "generated_at": "2026-11-25T03:12:45.000Z",
//!   "config_error": null,
//!   "aria2": {
//!     "versions": {
//!       "user":    {"root": "...", "installed": ["1.37.0"], "active": "1.37.0", "previous": null, "error": null},
//!       "service": {"root": "...", "installed": [], "active": null, "previous": null, "error": null}
//!     },
//!     "core": {"exe": "...", "origin": "bundled", "exists": true},
//!     "rpc":  {"port": 6800, "reachable": true, "version": "1.37.0", "active_tasks": 2, "error": null}
//!   },
//!   "service": {"installed": false, "state": "not-installed", "start_type": null, "pid": null},
//!   "clean": {"total_bytes": 123, "total_count": 4, "urgency": "静默（…）", "by_target": [...], ...}
//! }
//! ```
//!
//! `config_error` 非空表示「配置坏了，包里只有 `config.error.txt`」——**诊断包照样产出**，
//! 因为配置坏掉本身就是要诊断的那个问题。

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use ariahost_common::config::{self, Config};
use ariahost_common::paths::{self, Plane};
use ariahost_common::redact::redact;
use ariahost_common::time::utc_stamp;
use serde_json::{json, Value};

/// 诊断包里秘密值的替换文本（与日志脱敏的 `***` 区分：这里看得出「是被我们换掉的」）。
pub const REDACTED_VALUE: &str = "<已脱敏>";

/// 默认输出目录名（`%LOCALAPPDATA%\AriaHost\diagnose`）。
pub const DEFAULT_DIR_NAME: &str = "diagnose";

/// zip 内固定的四个顶层条目名。
const README_NAME: &str = "README.txt";
const CONFIG_NAME: &str = "config.redacted.toml";
const CONFIG_ERROR_NAME: &str = "config.error.txt";
const STATE_NAME: &str = "state.json";
const ENV_NAME: &str = "env.json";

/// 日志条目的公共前缀（`logs/user/...`、`logs/service/...`）。
const LOGS_PREFIX: &str = "logs";

/// RPC 只探一次，2s 超时（任务书原文：不联网、不启动 aria2c、只探一次）。
const RPC_TIMEOUT: Duration = Duration::from_secs(2);

/// 打包结果（供 `--diagnose` 上屏）。
#[derive(Debug, Clone)]
pub struct Report {
    /// zip 绝对路径。
    pub path: PathBuf,
    /// zip 字节数。
    pub bytes: u64,
    /// 包内相对路径清单（已排序）。
    pub entries: Vec<String>,
}

/// 一个待打包条目：`name` 是 zip 内相对路径（正斜杠），`bytes` 是**已脱敏**的内容。
struct Entry {
    name: String,
    bytes: Vec<u8>,
}

/// 采集到的原始素材（`run` 从真实环境读；单测直接构造，不碰机器）。
struct Sources {
    now: SystemTime,
    /// 配置：`Err` = 读不出来（诊断包照出，只写 `config.error.txt`）。
    config: Result<Config, String>,
    /// 日志：`(zip 内相对路径, 原文)`。
    logs: Vec<(String, String)>,
}

/// `--diagnose [--out <dir>]` 的入口。
///
/// 返回 `Err(人话)` 仅表示**包没产出**（目录建不出来 / 写盘失败）；日志缺失、配置坏掉、
/// RPC 连不上都只记进包内，不算失败。
pub fn run(out_dir: Option<&Path>) -> Result<Report, String> {
    let dir = match out_dir {
        Some(dir) => dir.to_path_buf(),
        None => default_out_dir().ok_or_else(|| {
            "取不到 %LOCALAPPDATA%：诊断包默认放在那里，请用 --out 指定一个目录".to_owned()
        })?,
    };
    fs::create_dir_all(&dir).map_err(|error| {
        format!(
            "建不了输出目录 {}：{error}；用 --out 换个可写目录再试",
            dir.display()
        )
    })?;

    let sources = Sources {
        now: SystemTime::now(),
        config: config::load().map_err(|error| error.to_string()),
        logs: read_logs(),
    };
    let entries = build_entries(&sources);
    write_zip(&dir, sources.now, entries)
}

/// 默认输出目录：`%LOCALAPPDATA%\AriaHost\diagnose`。
fn default_out_dir() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")?;
    Some(PathBuf::from(local).join("AriaHost").join(DEFAULT_DIR_NAME))
}

// ---------------------------------------------------------------------------
// 采集
// ---------------------------------------------------------------------------

/// 把素材变成 zip 条目（**纯函数**：同样的 `Sources` 出同样的包，单测的门就架在这里）。
fn build_entries(sources: &Sources) -> Vec<Entry> {
    let mut entries = Vec::new();

    match &sources.config {
        Ok(config) => entries.push(Entry {
            name: CONFIG_NAME.to_owned(),
            bytes: redacted_config_toml(config).into_bytes(),
        }),
        Err(detail) => entries.push(Entry {
            name: CONFIG_ERROR_NAME.to_owned(),
            bytes: format!(
                "配置读取失败（诊断包不因此中断）：\n{detail}\n\n\
                 提示：如果 config.toml 坏了，删掉它重启面板会按默认值重建；\n\
                 重建后的 secret 会变，正在跑的 aria2c 需要重启一次。\n"
            )
            .into_bytes(),
        }),
    }

    let config = sources.config.as_ref().ok();
    // 已知字面值只算一次：脱敏日志与配置正文共用同一份。
    let known = known_secrets(config);
    for (name, text) in &sources.logs {
        entries.push(Entry {
            name: name.clone(),
            bytes: scrub(text, &known).into_bytes(),
        });
    }

    entries.push(Entry {
        name: STATE_NAME.to_owned(),
        bytes: pretty_json(&state_json(sources.now, config, &sources.config)),
    });
    entries.push(Entry {
        name: ENV_NAME.to_owned(),
        bytes: pretty_json(&env_json(sources.now)),
    });
    entries.push(Entry {
        name: README_NAME.to_owned(),
        bytes: readme(sources.now).into_bytes(),
    });

    entries.sort_by(|left, right| left.name.cmp(&right.name));
    entries
}

/// 读两个平面的日志目录（**读不出来就跳过**，不是错误）。
fn read_logs() -> Vec<(String, String)> {
    let mut logs = Vec::new();
    for (plane_name, plane) in [("user", Plane::User), ("service", Plane::Service)] {
        let Some(dir) = paths::logs_dir(plane) else {
            continue;
        };
        let Ok(read) = fs::read_dir(&dir) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = read
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .collect();
        paths.sort();
        for path in paths {
            let Some(name) = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
            else {
                continue;
            };
            if !is_log_name(&name) {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else {
                continue;
            };
            logs.push((
                format!("{LOGS_PREFIX}/{plane_name}/{name}"),
                String::from_utf8_lossy(&bytes).into_owned(),
            ));
        }
    }
    logs
}

/// 只收这三类日志：`<名字>` 本体，或轮转件 `<名字>.<序号>`（序号规则与
/// [`arihost_common::logfile::rotated_index`] 一致：非空纯数字）。
///
/// 刻意收紧到「本体 + 纯数字后缀」：`ariahost.log.bak`、`ariahost.logger` 这类同名邻居
/// 不该被打进包里（它们可能是别人手工留的东西）。
fn is_log_name(name: &str) -> bool {
    const BASES: [&str; 3] = ["ariahost.log", "aria2.log", "service.log"];
    BASES.iter().any(|base| {
        if name == *base {
            return true;
        }
        name.strip_prefix(base)
            .and_then(|suffix| suffix.strip_prefix('.'))
            .is_some_and(|digits| {
                !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
            })
    })
}

// ---------------------------------------------------------------------------
// 脱敏
// ---------------------------------------------------------------------------

/// 配置 → 脱敏后的 TOML 文本。
///
/// 两级：**结构化**替换已知字段（secret / 代理密码），再对整段文本过一遍 [`scrub`]
/// （兜住藏在 `extra`/自由文本里的 tracker passkey 之类）。
pub fn redacted_config_toml(config: &Config) -> String {
    let mut safe = config.clone();
    if !safe.aria2.rpc.secret.trim().is_empty() {
        safe.aria2.rpc.secret = REDACTED_VALUE.to_owned();
    }
    safe.net.proxy = redact_proxy(&safe.net.proxy);

    let body = toml::to_string_pretty(&safe)
        .unwrap_or_else(|error| format!("# 序列化失败（原样记录错误，不影响其它条目）：{error}\n"));
    let mut out = String::from(
        "# AriaHost 诊断包：本条已脱敏\n\
         # 替换范围：aria2.rpc.secret、net.proxy 的密码、tracker passkey、磁力 infohash\n",
    );
    out.push_str(&scrub(&body, &known_secrets(Some(config))));
    out
}

/// 代理地址里的密码 → [`REDACTED_VALUE`]（保留协议、用户名、主机、端口）。
///
/// - `http://user:pass@127.0.0.1:11080` → `http://user:<已脱敏>@127.0.0.1:11080`
/// - `http://token@host:8080` → `http://<已脱敏>@host:8080`（没有 `:` 时整段 userinfo 都是凭据）
/// - 没有 `@` 或没有 `://` → 原样（`socks5://host:1080` 本来就无凭据）
fn redact_proxy(proxy: &str) -> String {
    let trimmed = proxy.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let Some(scheme_end) = trimmed.find("://") else {
        // 认不出形状：整段当凭据，宁可不给也不漏。
        return REDACTED_VALUE.to_owned();
    };
    let prefix = &trimmed[..scheme_end + 3];
    let rest = &trimmed[scheme_end + 3..];
    let Some(at) = rest.rfind('@') else {
        return trimmed.to_owned();
    };
    let userinfo = &rest[..at];
    let host = &rest[at..];
    let safe_user = match userinfo.split_once(':') {
        Some((user, _password)) => format!("{user}:{REDACTED_VALUE}"),
        None => REDACTED_VALUE.to_owned(),
    };
    format!("{prefix}{safe_user}{host}")
}

/// 文本级脱敏：两级。
///
/// 1. [`redact`]——标记式（`token:` / `secret=` / `passkey=` / 磁力 infohash / URL 密码），
///    这是日志与落盘共用的**唯一规则实现**。
/// 2. 按**已知字面值**整段替换（[`known_secrets`]）：标记式只认写法，日志里出现
///    「一个孤零零的凭据」（aria2 回显命令行、第三方库自己拼的字符串）就抓不到。
///    这一级不依赖任何前缀，代价是必须先知道值——而配置正好知道。
fn scrub(text: &str, known: &[String]) -> String {
    let mut out = redact(text);
    for value in known {
        if out.contains(value.as_str()) {
            out = out.replace(value.as_str(), REDACTED_VALUE);
        }
    }
    out
}

/// 本次诊断「必须从包里消失」的字面值：配置里的 secret（含 `secret.bin` 解出来的）、
/// 代理密码。
///
/// 只读：走 [`config::peek_secret`]，**不会**因为取不到就生成一个新的。
fn known_secrets(config: Option<&Config>) -> Vec<String> {
    let Some(config) = config else {
        return Vec::new();
    };
    let mut values = Vec::new();
    push_value(&mut values, &config.aria2.rpc.secret);
    if let Some(path) = config::default_secret_path() {
        if let Some(secret) = config::peek_secret(config, &path) {
            push_value(&mut values, &secret);
        }
    }
    push_value(&mut values, proxy_password(&config.net.proxy));
    // 长的先换：短值可能是长值的子串，先换短值会把长值切成两半。
    values.sort_by_key(|value| std::cmp::Reverse(value.len()));
    values
}

fn push_value(values: &mut Vec<String>, raw: &str) {
    let trimmed = raw.trim();
    // 太短的值不按字面替换：`abc` 这种会把普通单词也打码。
    if trimmed.chars().count() >= 4 && !values.iter().any(|existing| existing == trimmed) {
        values.push(trimmed.to_owned());
    }
}

/// `scheme://user:password@host` 里取出 password；认不出形状就返回空串。
fn proxy_password(proxy: &str) -> &str {
    let trimmed = proxy.trim();
    let Some((_, rest)) = trimmed.split_once("://") else {
        return "";
    };
    let Some((userinfo, _host)) = rest.rsplit_once('@') else {
        return "";
    };
    userinfo
        .split_once(':')
        .map_or("", |(_, password)| password)
}

// ---------------------------------------------------------------------------
// state.json / env.json / README.txt
// ---------------------------------------------------------------------------

/// 运行态快照（**不联网、不启动 aria2c**：RPC 只探一次，2s 超时）。
fn state_json(now: SystemTime, config: Option<&Config>, loaded: &Result<Config, String>) -> Value {
    let fallback;
    let config = match config {
        Some(config) => config,
        None => {
            fallback = Config::default();
            &fallback
        }
    };
    json!({
        "generated_at": utc_stamp(now),
        "config_error": match loaded {
            Ok(_) => Value::Null,
            Err(detail) => json!(detail),
        },
        "aria2": {
            "versions": {
                "user": plane_versions(Plane::User),
                "service": plane_versions(Plane::Service),
            },
            "core": core_status(config),
            "rpc": rpc_probe(config),
        },
        "service": service_status(),
        "clean": clean_summary(config),
    })
}

/// 一个平面的版本库状态（列表 + 激活指针）。
fn plane_versions(plane: Plane) -> Value {
    let Some(root) = paths::versions_dir(plane) else {
        return json!({"root": null, "installed": [], "active": null, "previous": null, "error": "取不到目录（缺环境变量）"});
    };
    let store = ariahost_core::VersionStore::new(&root);
    let installed = store.list();
    let active = store.active();
    let (installed_value, installed_error) = match &installed {
        Ok(list) => (json!(list), Value::Null),
        Err(error) => (json!([]), json!(error.to_string())),
    };
    let (active_value, previous_value, pointer_error) = match &active {
        Ok(Some(pointer)) => (json!(pointer.active), json!(pointer.previous), Value::Null),
        Ok(None) => (Value::Null, Value::Null, Value::Null),
        Err(error) => (Value::Null, Value::Null, json!(error.to_string())),
    };
    let error = if installed_error.is_null() {
        pointer_error
    } else {
        installed_error
    };
    json!({
        "root": root.display().to_string(),
        "installed": installed_value,
        "active": active_value,
        "previous": previous_value,
        "error": error,
    })
}

/// 当前生效的核心 exe（不启动、只解析路径 + 来源 + 存在性）。
///
/// `origin` 必须报：内置/托管/外部是三种**不同的故障归因**——「找不到核心」时，
/// 是安装包没带（发布事故）、版本库没激活（用户操作）、还是外部路径写错，
/// 全靠这一个字段区分；只给路径的话，用户报障时我们仍要反问一轮。
fn core_status(config: &Config) -> Value {
    match ariahost_core::resolve_core(config) {
        Ok(core) => json!({
            "exe": core.path.display().to_string(),
            "origin": core.origin.as_str(),
            "exists": core.path.is_file(),
        }),
        Err(error) => json!({
            "exe": null,
            "origin": null,
            "exists": false,
            "error": error.to_string(),
        }),
    }
}

/// RPC 探测：一次 `getVersion`（可达时再补一次 `tellActive` 取活动任务数）。
fn rpc_probe(config: &Config) -> Value {
    let secret = config::default_secret_path()
        .as_deref()
        .and_then(|path| config::peek_secret(config, path));
    let client = ariahost_aria2::RpcClient::local_with_timeout(
        config.aria2.rpc.port,
        secret.clone(),
        RPC_TIMEOUT,
    );
    let port = config.aria2.rpc.port;
    match client.get_version() {
        Ok(version) => {
            // 已经连上了：顺手取一次活动任务数（同样的 2s 超时，不重试）。
            let active = client.tell_active();
            let (tasks, tasks_error) = match &active {
                Ok(list) => (json!(list.len()), Value::Null),
                Err(error) => (Value::Null, json!(rpc_error_text(error))),
            };
            json!({
                "port": port,
                "reachable": true,
                "version": version.version,
                "active_tasks": tasks,
                "error": tasks_error,
                "secret_source": if secret.is_some() { "配置或 secret.bin" } else { "取不到（未带 token 探测）" },
            })
        }
        Err(error) => json!({
            "port": port,
            "reachable": false,
            "version": null,
            "active_tasks": null,
            "error": rpc_error_text(&error),
            "secret_source": if secret.is_some() { "配置或 secret.bin" } else { "取不到（未带 token 探测）" },
        }),
    }
}

/// RPC 错误的可读文本：有错误码就带码，没有就用原文（`--diagnose` 不是协议，人可以读）。
///
/// `RpcCallError` 的 `Display` 里已经带码，别再加一遍（真机上曾出现
/// `AH-E2003 AH-E2003 连接 aria2 RPC 失败`）。
fn rpc_error_text(error: &ariahost_aria2::RpcCallError) -> String {
    let text = error.to_string();
    match error.code() {
        Some(code) if !text.contains(code.code()) => format!("{} {text}", code.code()),
        _ => text,
    }
}

/// 服务状态：复用 `--service-status` 的**同一份** JSON（形状不许分叉）。
fn service_status() -> Value {
    match ariahost_svc::status_report() {
        Ok(report) => serde_json::from_str(&crate::cli::service_status_json(&report))
            .unwrap_or_else(|error| json!({"error": format!("状态 JSON 解析失败：{error}")})),
        Err(error) => json!({"error": format!("{} {}", error.code.code(), error.detail)}),
    }
}

/// 清理摘要（`CleanReport` 的体积分项）。**扫描是只读的**，删东西只走 UI 的显式确认。
fn clean_summary(config: &Config) -> Value {
    let report = ariahost_core::scan(&ariahost_core::ScanContext::from_config(
        config,
        &ariahost_core::RealLiveState,
    ));
    let by_target: Vec<Value> = ariahost_core::CleanTarget::ALL
        .iter()
        .map(|target| {
            let summary = report.summary(*target);
            json!({
                "target": format!("{target:?}"),
                "label": target.label(),
                "count": summary.count,
                "bytes": summary.bytes,
            })
        })
        .collect();
    json!({
        "total_bytes": report.total_bytes(),
        "total_count": report.total_count(),
        "urgency": report.urgency().label(),
        "free_bytes": report.free_bytes(),
        "low_disk": report.low_disk(),
        "by_target": by_target,
        "warnings": report.warnings(),
        "budget_exhausted": report.budget_exhausted(),
        "entries_scanned": report.entries_scanned(),
        "elapsed_ms": u64::try_from(report.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

/// 环境快照（只留用户名，不收集机器名等网络标识）。
fn env_json(now: SystemTime) -> Value {
    let exe = std::env::current_exe().ok();
    let exe_text = exe.as_ref().map(|path| path.display().to_string());
    json!({
        "generated_at": utc_stamp(now),
        "product": ariahost_common::PRODUCT_NAME,
        "version": ariahost_common::VERSION,
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "os_version": platform::os_version()
            .map(|version| json!({
                "major": version.major,
                "minor": version.minor,
                "build": version.build,
                // 读包的人必须知道这个数字可不可信（见 `platform::os_version` 的注释）。
                "source": version.source,
            }))
            .unwrap_or(Value::Null),
        "is_admin": platform::is_admin(),
        "username": std::env::var("USERNAME").or_else(|_| std::env::var("USER")).ok(),
        "exe_path": exe_text,
        "install_form": exe.as_deref().map(install_form),
        "config_file": paths::config_file().map(|path| path.display().to_string()),
        "logs": {
            "user": paths::logs_dir(Plane::User).map(|path| path.display().to_string()),
            "service": paths::logs_dir(Plane::Service).map(|path| path.display().to_string()),
        },
        "versions": {
            "user": paths::versions_dir(Plane::User).map(|path| path.display().to_string()),
            "service": paths::versions_dir(Plane::Service).map(|path| path.display().to_string()),
        },
    })
}

/// 安装形态推断（纯函数；`roots` 由调用方从环境变量取，便于单测）。
///
/// 判据是**路径前缀**，不是注册表：per-user 装在 `%LOCALAPPDATA%\Programs\AriaHost`，
/// per-machine 在 `%ProgramFiles%`（含 x86 变体）。认不出来就老实说 `portable-or-unknown`
/// —— 安装器将来换目录时，这个字段错了也不致命，但不要编。
fn install_form_with(
    exe: &str,
    local: Option<&str>,
    programs: Option<&str>,
    programs_x86: Option<&str>,
) -> &'static str {
    let exe = exe.to_lowercase();
    let starts_with = |root: Option<&str>| {
        root.map(|root| {
            let root = root.trim_end_matches(['\\', '/']).to_lowercase();
            !root.is_empty() && exe.starts_with(&root)
        })
        .unwrap_or(false)
    };
    if starts_with(local) && exe.contains("\\programs\\") {
        return "per-user";
    }
    if starts_with(programs_x86) {
        return "per-machine-x86";
    }
    if starts_with(programs) {
        return "per-machine";
    }
    "portable-or-unknown"
}

/// 用真实环境变量调 [`install_form_with`]。
fn install_form(exe: &Path) -> &'static str {
    let local = std::env::var("LOCALAPPDATA").ok();
    let programs = std::env::var("ProgramFiles").ok();
    let programs_x86 = std::env::var("ProgramFiles(x86)").ok();
    install_form_with(
        &exe.to_string_lossy(),
        local.as_deref(),
        programs.as_deref(),
        programs_x86.as_deref(),
    )
}

/// 包内说明（中文，一行讲清「能不能直接发」）。
fn readme(now: SystemTime) -> String {
    format!(
        "本包已脱敏，可直接发给开发者。\n\
         \n\
         生成时间：{}\n\
         版本：{} {}\n\
         包含：config.redacted.toml（或 config.error.txt）、logs/、state.json、env.json\n\
         \n\
         脱敏范围：aria2.rpc.secret、代理密码、tracker passkey、磁力 infohash、token。\n\
         若发现仍有敏感内容，请先不要外发，把文件名告诉我们。\n",
        utc_stamp(now),
        ariahost_common::PRODUCT_NAME,
        ariahost_common::VERSION
    )
}

/// JSON 美化输出（人读包；末尾带换行）。
fn pretty_json(value: &Value) -> Vec<u8> {
    let mut text = serde_json::to_string_pretty(value)
        .unwrap_or_else(|error| format!("{{\"error\":\"JSON 序列化失败：{error}\"}}"));
    text.push('\n');
    text.into_bytes()
}

// ---------------------------------------------------------------------------
// zip
// ---------------------------------------------------------------------------

/// 文件名主干：`AriaHost-diagnose-<yyyymmdd-HHMMSS>`（UTC，无冒号，按名字排序即按时间排序）。
fn zip_stem(now: SystemTime) -> String {
    let stamp = utc_stamp(now);
    if stamp.len() < 19 {
        // 理论上不会发生（utc_stamp 定宽 24）；真发生了也别 panic，退化成整串。
        return format!("AriaHost-{DEFAULT_DIR_NAME}-{stamp}");
    }
    format!(
        "AriaHost-{DEFAULT_DIR_NAME}-{}{}{}-{}{}{}",
        &stamp[0..4],
        &stamp[5..7],
        &stamp[8..10],
        &stamp[11..13],
        &stamp[14..16],
        &stamp[17..19]
    )
}

/// 挑一个没被占用的文件名：`<stem>.zip`，撞车就 `<stem>-1.zip`、`-2`……
///
/// **绝不覆盖**：已有文件一个字节都不动（用户的旧诊断包可能是排查线索）。
fn unique_path(dir: &Path, stem: &str) -> (PathBuf, PathBuf) {
    for index in 0..1000u32 {
        let name = if index == 0 {
            format!("{stem}.zip")
        } else {
            format!("{stem}-{index}.zip")
        };
        let path = dir.join(&name);
        if !path.exists() {
            let part = dir.join(format!("{name}.part"));
            return (path, part);
        }
    }
    // 同一秒导了 1000 次：加时间后缀兜底（仍然不覆盖）。
    let fallback = format!("{stem}-{}.zip", std::process::id());
    let path = dir.join(&fallback);
    let part = dir.join(format!("{fallback}.part"));
    (path, part)
}

/// 写 zip（先写 `.part` 再改名：中途失败不会留下一个「看起来完整」的坏包）。
fn write_zip(dir: &Path, now: SystemTime, entries: Vec<Entry>) -> Result<Report, String> {
    let stem = zip_stem(now);
    let (path, part) = unique_path(dir, &stem);

    let written = (|| -> Result<(), String> {
        let file = File::create(&part)
            .map_err(|error| format!("建不了临时文件 {}：{error}", part.display()))?;
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for entry in &entries {
            writer
                .start_file(&entry.name, options)
                .map_err(|error| format!("写 zip 条目 {} 失败：{error}", entry.name))?;
            writer
                .write_all(&entry.bytes)
                .map_err(|error| format!("写 zip 条目 {} 内容失败：{error}", entry.name))?;
        }
        writer
            .finish()
            .map_err(|error| format!("收尾 zip 失败：{error}"))?;
        Ok(())
    })();

    if let Err(detail) = written {
        let _ = fs::remove_file(&part);
        return Err(detail);
    }
    fs::rename(&part, &path).map_err(|error| {
        let _ = fs::remove_file(&part);
        format!(
            "把临时文件改名为 {} 失败：{error}；请用 --out 换个目录",
            path.display()
        )
    })?;

    let bytes = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
    Ok(Report {
        path,
        bytes,
        entries: entries.into_iter().map(|entry| entry.name).collect(),
    })
}

// ---------------------------------------------------------------------------
// 平台
// ---------------------------------------------------------------------------

mod platform {
    /// Windows 版本三件套 + **这个数字的取法**（`source` 会被写进 `env.json`）。
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct OsVersion {
        pub major: u32,
        pub minor: u32,
        pub build: u32,
        /// `ntdll-rtlgetversion` = 可信；`fallback-getversionex…` = 可能被 manifest 影响。
        pub source: &'static str,
    }

    /// 是否以管理员（提升）身份运行；取不到就 `None`（不猜）。
    #[cfg(windows)]
    pub fn is_admin() -> Option<bool> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Security::{
            GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

        // SAFETY: 只查询当前进程的令牌提升状态；两个句柄都在函数内关闭。
        unsafe {
            let mut token = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return None;
            }
            let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
            let mut returned: u32 = 0;
            let queried = GetTokenInformation(
                token,
                TokenElevation,
                std::ptr::addr_of_mut!(elevation).cast(),
                u32::try_from(std::mem::size_of::<TOKEN_ELEVATION>()).unwrap_or(0),
                &mut returned,
            );
            CloseHandle(token);
            if queried == 0 {
                return None;
            }
            Some(elevation.TokenIsElevated != 0)
        }
    }

    #[cfg(not(windows))]
    pub fn is_admin() -> Option<bool> {
        None
    }

    /// Windows 版本（主/次/构建）+ **这个数字是怎么来的**。
    ///
    /// 真机教训：`GetVersionExW` 在本程序上报的是 **6.2 / 9200（Windows 8）**——因为它
    /// 看的是进程 manifest 里声明的 `supportedOS`，而不是机器真实版本。诊断包把 OS 写错
    /// 会直接误导排障（「为什么 wgpu 在 Win11 上崩」这种问题的第一现场就是这里）。
    ///
    /// 所以先用 `ntdll!RtlGetVersion`：它不读 manifest，返回真实版本；动态解析是为了
    /// 不把 `ntdll` 变成链接期依赖（它本来也不在 import 表里）。解析失败才退回
    /// `GetVersionExW`，并**在 `source` 里老实标注**，让读包的人知道这个数字可能不准。
    #[cfg(windows)]
    pub fn os_version() -> Option<OsVersion> {
        use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
        use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;

        /// `NTSTATUS RtlGetVersion(PRTL_OSVERSIONINFOW) -> NTSTATUS`（0 = 成功）。
        type RtlGetVersionFn = unsafe extern "system" fn(*mut OSVERSIONINFOW) -> i32;

        // SAFETY: `ntdll.dll` 在 Windows 上恒已加载（只取模块句柄，不增引用计数）；
        // `RtlGetVersion` 的签名按 WinDDK 声明，调用方给出的结构体已填好大小字段。
        unsafe {
            let module = GetModuleHandleW(c"ntdll.dll".as_ptr().cast::<u16>());
            if !module.is_null() {
                if let Some(address) =
                    GetProcAddress(module, c"RtlGetVersion".as_ptr().cast::<u8>())
                {
                    // SAFETY: 地址来自 `GetProcAddress("RtlGetVersion")`，签名与别名一致。
                    let rtl: RtlGetVersionFn = std::mem::transmute(address);
                    let mut info = OSVERSIONINFOW {
                        dwOSVersionInfoSize: u32::try_from(std::mem::size_of::<OSVERSIONINFOW>())
                            .unwrap_or(0),
                        ..OSVERSIONINFOW::default()
                    };
                    if rtl(&mut info) == 0 {
                        return Some(OsVersion {
                            major: info.dwMajorVersion,
                            minor: info.dwMinorVersion,
                            build: info.dwBuildNumber,
                            source: "ntdll-rtlgetversion",
                        });
                    }
                }
            }
        }
        get_version_ex()
    }

    /// 兜底：`GetVersionExW`（可能因 manifest 谎报，`source` 会标出来）。
    #[cfg(windows)]
    fn get_version_ex() -> Option<OsVersion> {
        use windows_sys::Win32::System::SystemInformation::{GetVersionExW, OSVERSIONINFOW};

        let mut info = OSVERSIONINFOW {
            dwOSVersionInfoSize: u32::try_from(std::mem::size_of::<OSVERSIONINFOW>()).unwrap_or(0),
            ..OSVERSIONINFOW::default()
        };
        // SAFETY: 按 SDK 定义填好 `dwOSVersionInfoSize` 后只读查询，不持有任何句柄。
        let ok = unsafe { GetVersionExW(&mut info) };
        (ok != 0).then_some(OsVersion {
            major: info.dwMajorVersion,
            minor: info.dwMinorVersion,
            build: info.dwBuildNumber,
            source: "fallback-getversionex（可能受 manifest 影响而不准）",
        })
    }

    #[cfg(not(windows))]
    pub fn os_version() -> Option<OsVersion> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// 每个测试一个独立临时目录。
    fn temp_dir(tag: &str) -> PathBuf {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ariahost-diagnose-{tag}-{}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    /// 读回 zip 的全部条目（名字 → 解压后的文本）。
    fn read_zip(path: &Path) -> Vec<(String, String)> {
        let file = File::open(path).expect("打开 zip");
        let mut archive = zip::ZipArchive::new(file).expect("解析 zip");
        let mut out = Vec::new();
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).expect("取 zip 条目");
            // zip 9.x 的 `name()` 返回 `Result<Cow<str>>`（可能是非 UTF-8 名）：名字坏了就跳过，
            // 只影响断言的可读性，不影响「有没有泄露」这个判定。
            let Ok(name) = entry.name().map(|name| name.into_owned()) else {
                continue;
            };
            let mut text = String::new();
            entry.read_to_string(&mut text).expect("读条目内容");
            out.push((name, text));
        }
        out
    }

    fn sample_sources(now: SystemTime) -> Sources {
        let mut config = Config::default();
        config.aria2.rpc.secret = "abc123-secret".to_owned();
        config.net.proxy = "http://user:hunter2@127.0.0.1:11080".to_owned();
        Sources {
            now,
            config: Ok(config),
            logs: vec![
                (
                    "logs/user/ariahost.log".to_owned(),
                    "2026-11-25T03:12:45.000Z INFO ui: token:leak-me 不该出现\n\
                     2026-11-25T03:12:46.000Z INFO rpc: 用 --rpc-secret=xyzzy 也不行\n\
                     2026-11-25T03:12:47.000Z INFO bt: magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567\n"
                        .to_owned(),
                ),
                (
                    "logs/service/service.log.1".to_owned(),
                    "2026-11-25T03:12:45.000Z INFO 服务启动\n".to_owned(),
                ),
            ],
        }
    }

    #[test]
    fn zip_stem_is_sortable_and_has_no_colons() {
        let stem = zip_stem(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000));
        assert_eq!(stem, "AriaHost-diagnose-20231114-221320");
        assert!(!stem.contains(':'), "文件名里不能有冒号：{stem}");
        // 早一秒的名字字典序更小（按名字排序即按时间排序）。
        let earlier = zip_stem(SystemTime::UNIX_EPOCH + Duration::from_secs(1_699_999_999));
        assert!(earlier < stem, "{earlier} 应小于 {stem}");
    }

    #[test]
    fn redacted_config_hides_secret_and_proxy_password() {
        let mut config = Config::default();
        config.aria2.rpc.secret = "abc123-secret".to_owned();
        config.net.proxy = "http://user:hunter2@127.0.0.1:11080".to_owned();
        let text = redacted_config_toml(&config);
        assert!(!text.contains("abc123-secret"), "secret 泄露：{text}");
        assert!(!text.contains("hunter2"), "代理密码泄露：{text}");
        assert!(text.contains(REDACTED_VALUE));
        assert!(text.contains("127.0.0.1:11080"), "主机与端口应保留：{text}");
        assert!(text.contains("user:"), "用户名不算秘密，应保留：{text}");
    }

    #[test]
    fn redact_proxy_keeps_shape_and_hides_credentials() {
        assert_eq!(
            redact_proxy("http://user:pass@host:8080"),
            format!("http://user:{REDACTED_VALUE}@host:8080")
        );
        assert_eq!(
            redact_proxy("socks5h://token@host:1080"),
            format!("socks5h://{REDACTED_VALUE}@host:1080")
        );
        // 无凭据/空值原样
        assert_eq!(
            redact_proxy("socks5://127.0.0.1:1080"),
            "socks5://127.0.0.1:1080"
        );
        assert_eq!(redact_proxy("  "), "");
        // 认不出形状 → 整段当凭据
        assert_eq!(redact_proxy("pass@word"), REDACTED_VALUE);
    }

    #[test]
    fn scrub_removes_passkey_and_infohash_forms() {
        let text = "tracker: https://t.example/announce?passkey=deadbeef&x=1\n\
                    magnet:?xt=urn:btih:AAAABBBBCCCCDDDDEEEEFFFF0000111122223333\n\
                    ok: 普通正文不该动";
        let safe = scrub(text, &[]);
        assert!(!safe.contains("deadbeef"), "passkey 泄露：{safe}");
        assert!(!safe.contains("AAAABBBBCCCCDDDDEEEEFFFF0000111122223333"));
        assert!(safe.contains("tracker: https://t.example/announce?"));
        assert!(safe.contains("ok: 普通正文不该动"));
    }

    #[test]
    fn scrub_removes_known_values_even_without_any_marker() {
        // 真实事故形状：aria2 自己回显命令行，值前面没有任何 token:/secret= 前缀。
        let text = "11/25 03:00:00 [NOTICE] 上游 SUPERSECRET1234567890 已就绪";
        let safe = scrub(text, &["SUPERSECRET1234567890".to_owned()]);
        assert!(
            !safe.contains("SUPERSECRET1234567890"),
            "字面值没拦住：{safe}"
        );
        assert!(safe.contains(REDACTED_VALUE));
        assert!(safe.contains("[NOTICE] 上游 "), "上下文被切坏：{safe}");
    }

    #[test]
    fn scrub_keeps_case_insensitive_matches_aligned() {
        // 大写 marker + 中文前缀：不能把中文切坏。
        let text = "前缀中文 PASSKEY=Secret123 后缀";
        let safe = scrub(text, &[]);
        assert!(!safe.contains("Secret123"), "大写形式没拦住：{safe}");
        assert!(safe.contains("前缀中文 "), "中文前缀被切坏：{safe}");
        assert!(safe.contains(" 后缀"), "中文后缀被切坏：{safe}");
    }

    #[test]
    fn known_secrets_include_config_secret_and_proxy_password() {
        let mut config = Config::default();
        config.aria2.rpc.secret = "CONFIGSECRET01".to_owned();
        config.net.proxy = "http://alice:PROXYPASS77@127.0.0.1:11080".to_owned();
        let values = known_secrets(Some(&config));
        assert!(values.iter().any(|value| value == "CONFIGSECRET01"));
        assert!(values.iter().any(|value| value == "PROXYPASS77"));
        // 太短的值不参与字面替换（避免误伤普通单词）。
        assert!(!values.iter().any(|value| value.len() < 4));
    }

    /// **脱敏硬门禁**：产物 zip 里任何条目（解压后）都不许出现原始秘密串。
    #[test]
    fn diagnose_zip_contains_no_plaintext_secrets() {
        let dir = temp_dir("gate");
        let sources = sample_sources(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000));
        let entries = build_entries(&sources);
        let report = write_zip(&dir, sources.now, entries).expect("应能产出 zip");

        let files = read_zip(&report.path);
        assert!(!files.is_empty());
        let mut all = String::new();
        for (name, text) in &files {
            assert!(!name.contains(".."), "不安全的条目名：{name}");
            all.push_str(text);
        }
        for secret in [
            "abc123-secret",
            "hunter2",
            "leak-me",
            "xyzzy",
            "0123456789abcdef0123456789abcdef01234567",
        ] {
            assert!(
                !all.contains(secret),
                "zip 内出现原始秘密串 {secret:?}；条目：{:?}",
                files.iter().map(|(name, _)| name).collect::<Vec<_>>()
            );
        }
        // 条目清单必须含四类固定文件 + 两份日志。
        let names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();
        for expected in [CONFIG_NAME, STATE_NAME, ENV_NAME, README_NAME] {
            assert!(
                names.contains(&expected),
                "缺条目 {expected}；实际：{names:?}"
            );
        }
        assert!(names.contains(&"logs/user/ariahost.log"));
        assert!(names.contains(&"logs/service/service.log.1"));
    }

    #[test]
    fn broken_config_and_missing_logs_still_produce_a_bundle() {
        let dir = temp_dir("broken");
        let sources = Sources {
            now: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            config: Err("TOML 第 3 行解析失败".to_owned()),
            logs: Vec::new(),
        };
        let entries = build_entries(&sources);
        let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
        assert!(
            names.contains(&CONFIG_ERROR_NAME),
            "应写错误说明：{names:?}"
        );
        assert!(
            !names.contains(&CONFIG_NAME),
            "坏配置不该写 config.redacted.toml"
        );
        assert!(
            names.contains(&STATE_NAME)
                && names.contains(&ENV_NAME)
                && names.contains(&README_NAME)
        );

        let report = write_zip(&dir, sources.now, entries).expect("坏配置也要能出包");
        let files = read_zip(&report.path);
        let state = files
            .iter()
            .find(|(name, _)| name == STATE_NAME)
            .map(|(_, text)| text.clone())
            .expect("应有 state.json");
        assert!(
            state.contains("TOML 第 3 行解析失败"),
            "state 里要记下配置错误：{state}"
        );
        let error_note = files
            .iter()
            .find(|(name, _)| name == CONFIG_ERROR_NAME)
            .map(|(_, text)| text.clone())
            .expect("应有 config.error.txt");
        assert!(error_note.contains("TOML 第 3 行解析失败"));
    }

    #[test]
    fn existing_bundle_is_never_overwritten() {
        let dir = temp_dir("unique");
        let sources = sample_sources(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000));
        let first = write_zip(&dir, sources.now, build_entries(&sources)).expect("第一次");
        // 同一秒再导一次：必须换名字，且第一个包的字节不变。
        let before = fs::read(&first.path).expect("读第一个包");
        let second = write_zip(&dir, sources.now, build_entries(&sources)).expect("第二次");
        assert_ne!(first.path, second.path, "不许覆盖已有诊断包");
        assert_eq!(fs::read(&first.path).expect("再读第一个包"), before);
        assert!(second.path.exists());
    }

    #[test]
    fn unwritable_output_dir_reports_instead_of_panicking() {
        let dir = temp_dir("unwritable");
        let blocker = dir.join("not-a-dir");
        fs::write(&blocker, b"x").expect("写占位文件");
        // 把「一个文件」当输出目录 → create_dir_all 必失败 → Err(人话)
        let error = run(Some(&blocker)).expect_err("应报错而不是 panic");
        assert!(
            error.contains("建不了输出目录"),
            "错误文案应可执行：{error}"
        );
    }

    #[test]
    fn real_run_produces_a_readable_bundle() {
        // 真实入口（读真实环境）：只断言「能出包 + 四个固定条目 + 可解析」，不断言机器相关的值。
        let dir = temp_dir("real");
        let report = run(Some(&dir)).expect("真实环境应能出包");
        assert!(report.path.is_file());
        assert!(report.bytes > 0);
        let files = read_zip(&report.path);
        let names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();
        assert!(
            names.contains(&STATE_NAME)
                && names.contains(&ENV_NAME)
                && names.contains(&README_NAME)
        );
        let state: Value = serde_json::from_str(
            &files
                .iter()
                .find(|(name, _)| name == STATE_NAME)
                .map(|(_, text)| text.clone())
                .expect("state.json"),
        )
        .expect("state.json 必须是合法 JSON");
        for key in ["generated_at", "aria2", "service", "clean"] {
            assert!(state.get(key).is_some(), "state.json 缺 {key}：{state}");
        }
        let env: Value = serde_json::from_str(
            &files
                .iter()
                .find(|(name, _)| name == ENV_NAME)
                .map(|(_, text)| text.clone())
                .expect("env.json"),
        )
        .expect("env.json 必须是合法 JSON");
        assert_eq!(env["product"], ariahost_common::PRODUCT_NAME);
        assert_eq!(env["version"], ariahost_common::VERSION);
    }

    #[test]
    fn rpc_error_text_carries_the_code_only_once() {
        // 真机撞到的是 `AH-E2003 AH-E2003 连接 aria2 RPC 失败`：Display 已经带码，
        // 旧实现又拼了一遍。这里钉住「码最多出现一次」。
        let error = ariahost_aria2::RpcCallError::Connect {
            detail: "connection timed out".to_owned(),
            timed_out: true,
        };
        let text = rpc_error_text(&error);
        let code = error.code().expect("Connect 必须映射到错误码");
        assert_eq!(text.matches(code.code()).count(), 1, "错误码重复了：{text}");
        assert!(
            text.contains("connection timed out"),
            "原始细节丢了：{text}"
        );
    }

    #[test]
    fn install_form_is_inferred_from_the_path() {
        let local = Some(r"C:\Users\someone\AppData\Local");
        let programs = Some(r"C:\Program Files");
        let x86 = Some(r"C:\Program Files (x86)");
        assert_eq!(
            install_form_with(
                r"C:\Users\someone\AppData\Local\Programs\AriaHost\ariahost.exe",
                local,
                programs,
                x86
            ),
            "per-user"
        );
        assert_eq!(
            install_form_with(
                r"C:\Program Files\AriaHost\ariahost.exe",
                local,
                programs,
                x86
            ),
            "per-machine"
        );
        assert_eq!(
            install_form_with(
                r"C:\Program Files (x86)\AriaHost\ariahost.exe",
                local,
                programs,
                x86
            ),
            "per-machine-x86"
        );
        assert_eq!(
            install_form_with(r"D:\tools\ariahost.exe", local, programs, x86),
            "portable-or-unknown"
        );
        // 没有环境变量时不能瞎猜
        assert_eq!(
            install_form_with(r"C:\Program Files\AriaHost\ariahost.exe", None, None, None),
            "portable-or-unknown"
        );
    }

    #[test]
    fn log_name_filter_covers_rotated_files_only() {
        for name in [
            "ariahost.log",
            "ariahost.log.1",
            "aria2.log",
            "aria2.log.4",
            "service.log.2",
        ] {
            assert!(is_log_name(name), "应收：{name}");
        }
        for name in ["config.toml", "ariahost.log.bak", "notes.txt", "secret.bin"] {
            assert!(!is_log_name(name), "不该收：{name}");
        }
    }
}
