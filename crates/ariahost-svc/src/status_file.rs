//! 服务侧状态文件（M5-e）：`%ProgramData%\AriaHost\status.json`，**单向**——服务只写，
//! 用户态面板与诊断包只读。
//!
//! # 为什么是「服务主动写」而不是「面板去读服务」
//!
//! 面板要显示服务档的核心来源/版本，就得读服务侧的东西。服务 RPC 受 `secret.bin` 保护，
//! 而 `secret.bin` 在 `%ProgramData%`、SYSTEM 私有：为了一行文案去放宽它的 DACL 或给面板
//! 提权，是**拿攻击面换文案**，不划算（task-81 收口时的定论）。所以反过来做——服务在自己
//! 的生命周期关键点把**状态元数据**写成一个可读文件，谁都不用提权。
//!
//! # 内容边界（这是本模块唯一的安全约束）
//!
//! 文件里**只有**九个字段：`schema_version` / `service_version` / `core_source` /
//! `core_version` / `aria2_pid` / `started_at` / `updated_at`（M5-e）+ `config_path` /
//! `download_dir`（M5-f）。**绝不含** RPC secret、token、密码——少一个字段就少一条泄漏链。
//! 新增字段前先问：这条信息公开给本机任何用户看，会不会缩小攻击面？不会就别加
//! （单测 `serialized_form_carries_no_secrets` 钉住机密形态的子串）。
//!
//! M5-f 追加的两个字段是**路径**而非机密：`config_path` 回答「服务用的是哪份配置」（`null`
//! = 用的是内置默认值，面板据此提示「你的设置不生效」），`download_dir` 回答「下载到哪」。
//! 它们是排障必需的信息，且同一台机器上的用户本来就能从 `aria2.conf` 读到同样的值。
//!
//! # 机密为什么可以放宽读权限
//!
//! 与 `secret.bin`（SYSTEM 私有）不同，这份文件是**给普通用户的只读状态**：没有它面板就
//! 只能写「服务（运行中）」而说不出是哪个核心。内容已按上面的边界裁剪过，放宽「Users 读 +
//! SYSTEM 写」不构成新弱点。
//!
//! 目前的落地口径：**依赖 `%ProgramData%` 的默认继承**——ProgramData 下新建项默认
//! `Users` 可读、仅 `SYSTEM`/`Administrators` 可写，与仓库里既有的 `secret.bin` 口径一致
//! （全仓没有 `SetNamedSecurityInfo` 调用）。显式收紧（`SetNamedSecurityInfoW`）需要给
//! `windows-sys` 增加 `Win32_Security_Authorization` feature，属 workspace 依赖变更，
//! 已上报 Lead 待裁；一旦批准，只需在 [`apply_acl`] 的 Windows 分支里补实现，调用方零改动。
//!
//! # 写不出来绝不能让服务失败
//!
//! 状态文件是**观测手段**，不是服务能否跑的前提：目录不可写、磁盘满、被 AV 锁住，一律
//! 降级为一条 `WARN` 日志后继续（服务仍 `RUNNING`）。所以本模块所有入口都返回
//! `Result<_, String>`，由调用方决定「记日志」而不是「传播错误」。
//!
//! # 卸载清理
//!
//! 本模块**不负责**删除；`status.json` 与 `active.json`/`last_good.json` 同属数据平面，
//! 卸载处置统一由 `task-88` 管的清理链负责（挂钩点见 `ariahost-svc` 的 uninstall 路径）。

use std::path::{Path, PathBuf};

use ariahost_core::CoreOrigin;
use serde::Serialize;

/// 文件名（`%ProgramData%\AriaHost\status.json`）。
pub const STATUS_FILE_NAME: &str = "status.json";

/// 契约版本。**改字段必升这个数**——面板与诊断包按它决定怎么解析。
///
/// - 1：M5-e 初版（七字段）。
/// - 2：M5-f 追加 `config_path` / `download_dir`（服务模式「用的是哪份配置、下载到哪」）。
/// - 3：M6-bA 追加 `config_error`（配置坏到服务起不来时，面板要能看到原因，
///   而不是只看到「服务没起来」）。
pub const STATUS_SCHEMA_VERSION: u32 = 3;

/// 核心来源（写进文件的 `core_source`）。
///
/// 为什么自己定义而不是直接序列化 [`CoreOrigin`]：`CoreOrigin` 只有三个「有核心」的
/// 取值，而状态文件要能表达「**当前没有在跑的核心**」（服务刚起、核心刚停）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CoreSource {
    /// 安装包自带（exe 同目录）。
    Bundled,
    /// 版本库托管（`versions/<ver>/aria2c.exe`）。
    Managed,
    /// 用户指定的 `aria2.external_path`。
    External,
    /// 没有核心在跑（也是 `Default`：新建快照默认「还没有核心」）。
    #[default]
    None,
}

impl CoreSource {
    /// 稳定文本（与 serde 的 `rename_all = "lowercase"` 一致；日志与单测都按它比较）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bundled => "bundled",
            Self::Managed => "managed",
            Self::External => "external",
            Self::None => "none",
        }
    }

    /// 解析结果 → 文件取值（解析失败时调用方给 [`CoreSource::None`]）。
    pub const fn from_origin(origin: CoreOrigin) -> Self {
        match origin {
            CoreOrigin::Bundled => Self::Bundled,
            CoreOrigin::Managed => Self::Managed,
            CoreOrigin::External => Self::External,
        }
    }
}

/// 一份状态快照（**只有**这七个字段，见模块文档的内容边界）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServiceStatus {
    /// 契约版本（[`STATUS_SCHEMA_VERSION`]）。
    pub schema_version: u32,
    /// 服务二进制版本（`env!("CARGO_PKG_VERSION")`，由调用方给）。
    pub service_version: String,
    /// 核心来源。
    pub core_source: CoreSource,
    /// 核心版本（没有核心运行时为 `null`）。
    pub core_version: Option<String>,
    /// aria2c 子进程 PID（没有在跑时为 `null`）。
    pub aria2_pid: Option<u32>,
    /// **本次服务启动**的时刻（RFC3339 UTC，定宽 24 字节）。
    ///
    /// 整个服务会话内**保持不变**（面板靠它区分「同一次启动」）；只有下一次服务启动才换。
    pub started_at: String,
    /// 本次写入的时刻（RFC3339 UTC）。
    pub updated_at: String,
    /// 服务实际读到的那份配置的绝对路径（M5-f）。
    ///
    /// - `null` = 三级都没命中，服务用的是**内置默认值**（这时面板应提示用户「设置不生效」，
    ///   这正是 M5-f 那个 P0 要暴露的情形）。
    pub config_path: Option<String>,
    /// 服务实际下发给 aria2c 的下载目录绝对路径（M5-f）。
    ///
    /// - `null` = 没有核心在跑，或下载目录被护栏拒绝而没下发（原因在 `service.log`）。
    pub download_dir: Option<String>,
    /// 配置坏到服务起不来时的原因摘要（M6-bA）。
    ///
    /// - `null` = 配置没问题（或还没到读配置那一步）。
    /// - 非空 = 服务在 `drive_service` 之前就退出了；面板据此直接告诉用户「配置坏了，
    ///   坏在哪」——否则用户只会看到「服务没起来」而不知道去看 `config.toml`。
    ///   文案与 `service.log` 里的 ERROR 行同源（同一段字符串，不许两边各写一份）。
    pub config_error: Option<String>,
}

impl ServiceStatus {
    /// 服务刚启动、核心还没起：`core_source = none`、版本与 PID 都是 `null`。
    ///
    /// `updated_at` 先盖成「现在」（[`stamped_at`](Self::stamped_at) 可覆盖）——宁可多盖一次，
    /// 也不让漏调用时写出一个空字符串（那是坏数据，不是「没更新」）。
    pub fn starting(service_version: impl Into<String>, started_at: impl Into<String>) -> Self {
        let started_at = started_at.into();
        Self {
            schema_version: STATUS_SCHEMA_VERSION,
            service_version: service_version.into(),
            core_source: CoreSource::None,
            core_version: None,
            aria2_pid: None,
            started_at,
            updated_at: ariahost_common::time::utc_stamp(std::time::SystemTime::now()),
            config_path: None,
            download_dir: None,
            config_error: None,
        }
    }

    /// 填「服务用的是哪份配置、下载到哪」（M5-f）。
    ///
    /// 这两个值在一次服务会话内是**常量**（配置只在服务启动时读一次），
    /// 与核心三项不同——它们不需要每次现取。
    pub fn with_paths(mut self, config_path: Option<String>, download_dir: Option<String>) -> Self {
        self.config_path = config_path;
        self.download_dir = download_dir;
        self
    }

    /// 填「配置坏在哪」（M6-bA）。
    pub fn with_config_error(mut self, config_error: Option<String>) -> Self {
        self.config_error = config_error;
        self
    }

    /// 填「核心正在跑」的三个字段（来源/版本/PID）。
    ///
    /// 为什么单独一个方法：这三项必须**同时**来自同一次现读，不能一个缓存一个现取
    /// （那会写出「source=managed 但 pid 是上一个核心」这类自相矛盾的状态）。
    pub fn with_running_core(
        mut self,
        source: CoreSource,
        version: Option<String>,
        pid: Option<u32>,
    ) -> Self {
        self.core_source = source;
        self.core_version = version;
        self.aria2_pid = pid;
        self
    }

    /// 核心已停：来源回到 `none`，版本与 PID 清空。
    pub fn without_core(mut self) -> Self {
        self.core_source = CoreSource::None;
        self.core_version = None;
        self.aria2_pid = None;
        self
    }

    /// 盖上本次写入时刻。`updated_at` 每次写都要现取（不是缓存值）。
    pub fn stamped_at(mut self, updated_at: impl Into<String>) -> Self {
        self.updated_at = updated_at.into();
        self
    }
}

/// 状态文件的完整路径（`dir` 通常是 `%ProgramData%\AriaHost`）。
pub fn status_path(dir: &Path) -> PathBuf {
    dir.join(STATUS_FILE_NAME)
}

/// 原子写一份状态：`status.json.tmp` → `rename`。
///
/// 失败返回可读原因，**由调用方记 WARN 后继续**（见模块文档：写不出不影响服务）。
///
/// 为什么 `rename`：面板随时可能读到一半，绝不能让它读到半截 JSON。
/// `rename` 在同一目录、同一卷上是原子的（Windows 上 `MoveFileEx` 语义）。
pub fn write(dir: &Path, status: &ServiceStatus) -> Result<PathBuf, String> {
    let final_path = status_path(dir);
    let tmp_path = final_path.with_extension("json.tmp");
    let json = serde_json::to_string_pretty(status)
        .map_err(|error| format!("序列化服务状态失败：{error}"))?;
    // 末尾补换行：纯文本文件便于 `type`/`cat`/diff 查看。
    std::fs::write(&tmp_path, format!("{json}\n")).map_err(|error| {
        format!(
            "写临时状态文件 {} 失败：{error}（目录不可写/磁盘满/被占用）",
            tmp_path.display()
        )
    })?;
    // 权限收紧是 best-effort：设不上也要把状态写出去（观测手段不该反过来阻断服务）。
    // 失败细节由 [`write_or_warn`] 记进 WARN 日志；内核层不因它停手。
    let _acl = apply_acl(&tmp_path);
    if let Err(detail) = std::fs::rename(&tmp_path, &final_path) {
        // 失败时自己清掉半成品，不给下次写入留障碍。
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!(
            "替换 {} 失败：{detail}（临时文件已清理）",
            final_path.display()
        ));
    }
    Ok(final_path)
}

/// 生产入口：写状态文件，**永不失败**（失败只记一条 WARN 后返回 `None`）。
///
/// 这是服务生命周期里唯一该被调用的形式——状态文件是观测手段，写不出来不能影响
/// 服务是否 `RUNNING`（任务书口径：目录不可写/磁盘满 → WARN 后继续）。
pub fn write_or_warn(
    dir: &Path,
    status: &ServiceStatus,
    log: &dyn crate::log::LogSink,
) -> Option<PathBuf> {
    use crate::log::Level;

    match write(dir, status) {
        Ok(path) => Some(path),
        Err(detail) => {
            log.record(
                Level::Warn,
                &format!(
                    "服务状态文件写不出去（降级，服务继续运行）：{detail}；目标目录 {}",
                    dir.display()
                ),
            );
            None
        }
    }
}

/// **启动前**就失败时落的那一份快照（M5-f 护栏 / M6-bA 坏配置）。
///
/// 为什么需要：这类失败发生在 `drive_service` 之前——「四个挂钩点」那份状态文件根本不会
/// 产生。面板会看到「服务没起来」却不知道为什么；写一份 `core_source=none` +
/// `config_path` + `download_dir=null` + `config_error` 就能直接指向问题。
///
/// 关于「无配置文件 → 内置默认 + WARN」：它**看起来**像兜底，但内置默认的 `dl.dir`
/// 是 `~/Downloads`，按 SYSTEM 展开就是 `…\config\systemprofile\Downloads` → 一定命中
/// `systemprofile` 守卫 → 服务照样起不来。也就是说：**没有配置文件时服务不会靠默认值
/// 成功启动**，那条 WARN 只是把「为什么起不来」说清楚。
///
/// 参数显式化（目录 + 日志 + 三个字段）是为了 Linux 门禁能单测它，不必起真服务。
pub fn write_startup_status(
    dir: &Path,
    log: &dyn crate::log::LogSink,
    config_path: Option<&Path>,
    download_dir: Option<&Path>,
    config_error: Option<&str>,
) {
    let status = ServiceStatus::starting(
        env!("CARGO_PKG_VERSION"),
        ariahost_common::time::utc_stamp(std::time::SystemTime::now()),
    )
    .with_paths(
        config_path.map(|path| path.display().to_string()),
        download_dir.map(|path| path.display().to_string()),
    )
    .with_config_error(config_error.map(str::to_owned));
    write_or_warn(dir, &status, log);
}

/// 给状态文件设「Users 读 + SYSTEM 写」的显式 DACL（**best-effort**）。
///
/// 现状：Windows 分支尚未启用显式 DACL（见模块文档「机密为什么可以放宽读权限」）——
/// 需要 `windows-sys` 的 `Win32_Security_Authorization` feature，属 workspace 依赖变更，
/// 已上报 Lead。当前依赖 `%ProgramData%` 的默认继承，与 `secret.bin` 同口径。
#[cfg(not(windows))]
fn apply_acl(_path: &Path) -> Result<(), String> {
    // Linux 只在开发期跑门禁（产品只发 Windows），这里不模拟 Windows ACL。
    Ok(())
}

#[cfg(windows)]
fn apply_acl(_path: &Path) -> Result<(), String> {
    // **故意不显式设 DACL**（M6-bA 裁定）：这份文件靠 `%ProgramData%\AriaHost\` 的**继承**
    // ACL 就够——继承结果恰好是 `SYSTEM:F` + `BUILTIN\Users:(RX)`，也就是「服务能写、
    // 面板（普通用户）能读」。显式设 DACL 只有坏处、没有安全收益：
    //   * 写错一个 ACE，面板就读不到 `status.json`（用户侧功能直接坏），
    //   * 文件内容本来就按模块文档的边界裁剪过（无 secret/token/密码/下载凭据），
    //     放宽「Users 读」不构成新弱点。
    // 与 `secret.bin` 的分野：那个是**密文**且只有 SYSTEM 解（M5-g 才需要显式收紧并去继承）。
    // 一句话：**status.json 依赖继承，故意不设**；别照着 secret.bin 的样子给它加 DACL。
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每个用例一个独立目录（`target/tmp` 下，避免跨用例互相踩）。
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir()
                .join("ariahost-svc-status")
                .join(format!("{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("建临时目录");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sample() -> ServiceStatus {
        ServiceStatus::starting("0.1.3", "2026-11-26T02:33:34.510Z")
            .with_running_core(CoreSource::Managed, Some("1.37.0".to_owned()), Some(1234))
            .with_paths(
                Some("C:\\Users\\alice\\AppData\\Roaming\\AriaHost\\config.toml".to_owned()),
                Some("D:\\Downloads\\aria2".to_owned()),
            )
    }

    /// ① 字段完整 + `schema_version` 是契约值；`core_source` 序列化成稳定小写文本。
    #[test]
    fn writes_the_documented_contract_fields() {
        let dir = TempDir::new("fields");
        let mut status = sample();
        status.updated_at = "2026-11-26T02:34:00.000Z".to_owned();
        let path = write(dir.path(), &status).expect("写状态文件");

        let text = std::fs::read_to_string(&path).expect("读回");
        println!("STATUS_JSON {text}");
        let value: serde_json::Value = serde_json::from_str(&text).expect("合法 JSON");

        assert_eq!(value["schema_version"], serde_json::json!(3));
        assert_eq!(value["service_version"], serde_json::json!("0.1.3"));
        assert_eq!(value["core_source"], serde_json::json!("managed"));
        assert_eq!(value["core_version"], serde_json::json!("1.37.0"));
        assert_eq!(value["aria2_pid"], serde_json::json!(1234));
        assert_eq!(
            value["started_at"],
            serde_json::json!("2026-11-26T02:33:34.510Z")
        );
        assert_eq!(
            value["updated_at"],
            serde_json::json!("2026-11-26T02:34:00.000Z")
        );
        // M5-f：两个新字段必须真的落盘（面板据此提示「设置不生效」）。
        assert_eq!(
            value["config_path"],
            serde_json::json!("C:\\Users\\alice\\AppData\\Roaming\\AriaHost\\config.toml")
        );
        assert_eq!(
            value["download_dir"],
            serde_json::json!("D:\\Downloads\\aria2")
        );
        // 契约就是这十个键，多一个少一个都不行。
        let keys: Vec<&String> = value.as_object().expect("对象").keys().collect();
        assert_eq!(keys.len(), 10, "契约字段数变了：{keys:?}");
    }

    /// M5-f：内置默认 / 没下发下载目录时，两个字段都必须是 `null`（不是空串）。
    #[test]
    fn config_and_download_dir_are_null_when_absent() {
        let dir = TempDir::new("paths-absent");
        let status = ServiceStatus::starting("0.1.3", "2026-11-26T02:33:34.510Z")
            .stamped_at("2026-11-26T02:34:00.000Z");
        let path = write(dir.path(), &status).expect("写");
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("读")).expect("JSON");
        assert!(value["config_path"].is_null(), "{value}");
        assert!(value["download_dir"].is_null(), "{value}");
        assert_eq!(value["core_source"], serde_json::json!("none"));
    }

    /// ② 原子写：成功后不留 `.tmp`；重复写也只留一个正式文件。
    #[test]
    fn atomic_write_leaves_no_tmp_behind() {
        let dir = TempDir::new("atomic");
        let status = sample();
        write(dir.path(), &status).expect("首次写");
        write(dir.path(), &status.stamped_at("2026-11-26T02:35:00.000Z")).expect("再次写");

        let names: Vec<String> = std::fs::read_dir(dir.path())
            .expect("列目录")
            .map(|entry| {
                entry
                    .expect("项")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, vec![STATUS_FILE_NAME.to_owned()], "残留：{names:?}");
    }

    /// ③ 写不出来必须**返回 Err 而不是 panic**（调用方据此记 WARN 后继续跑）。
    ///
    /// 用「父路径是个普通文件」模拟「目录不可写」：这是跨平台可复现的构造
    /// （只读目录在以 root 跑的单测里不生效，权限位会被绕过）。
    #[test]
    fn unwritable_target_is_an_error_not_a_panic() {
        let dir = TempDir::new("unwritable");
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("造占位文件");

        let error = write(&blocker, &sample()).expect_err("父路径是文件时必须失败");
        assert!(
            error.contains("失败"),
            "失败原因要可读（进 WARN 日志）：{error}"
        );

        // 不存在的父目录同理：不静默创建，如实报错。
        let missing = dir.path().join("no-such-dir");
        let error = write(&missing, &sample()).expect_err("父目录缺失时必须失败");
        assert!(error.contains("失败"), "{error}");
    }

    /// 记账日志（只收 message，按 `Level` 分开存）。
    #[derive(Default)]
    struct RecordingLog {
        lines: std::sync::Mutex<Vec<(crate::log::Level, String)>>,
    }

    impl RecordingLog {
        fn warn_lines(&self) -> Vec<String> {
            self.lines
                .lock()
                .expect("锁")
                .iter()
                .filter(|(level, _)| *level == crate::log::Level::Warn)
                .map(|(_, line)| line.clone())
                .collect()
        }
    }

    impl crate::log::LogSink for RecordingLog {
        fn record(&self, level: crate::log::Level, message: &str) {
            self.lines
                .lock()
                .expect("锁")
                .push((level, message.to_owned()));
        }
    }

    /// ③（生产口径）写不出去 → **不失败**、不 panic，只留一条 WARN；服务仍能继续跑。
    #[test]
    fn write_or_warn_degrades_with_a_warning() {
        let dir = TempDir::new("degrade");
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("造占位文件");
        let log = RecordingLog::default();

        // 返回 None（不是 Err、不 panic）——调用方原样往下跑。
        let written = write_or_warn(
            &blocker,
            &sample().stamped_at("2026-11-26T02:33:34.510Z"),
            &log,
        );
        assert!(written.is_none(), "写不出去时不该谎报成功");

        let warns = log.warn_lines();
        assert_eq!(warns.len(), 1, "必须且只记一条 WARN：{warns:?}");
        let line = &warns[0];
        assert!(line.contains("降级"), "WARN 要说清是降级：{line}");
        assert!(
            line.contains("服务继续运行"),
            "WARN 要说清不影响服务：{line}"
        );
        assert!(line.contains("blocker"), "WARN 要带上目标：{line}");

        // 正常路径下不该有任何 WARN。
        let ok = TempDir::new("no-warn");
        let quiet = RecordingLog::default();
        let written = write_or_warn(
            ok.path(),
            &sample().stamped_at("2026-11-26T02:33:34.510Z"),
            &quiet,
        );
        assert!(written.is_some());
        assert!(quiet.warn_lines().is_empty(), "成功时不该有 WARN");
    }

    /// M6-bA：启动前失败（护栏拒绝 / 坏配置）那份状态必须是「无核心 + 下载目录 null +
    /// 带原因」，否则面板只能看到「服务没起来」。
    #[test]
    fn startup_failure_status_is_null_download_dir_plus_the_reason() {
        let dir = TempDir::new("startup-failure");
        let log = RecordingLog::default();
        let config = std::path::Path::new(r"C:\Users\alice\AppData\Roaming\AriaHost\config.toml");
        let reason = "下载目录被拒绝（fail-closed，AH-E1001）：落在 systemprofile";

        write_startup_status(dir.path(), &log, Some(config), None, Some(reason));

        let value: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join(STATUS_FILE_NAME)).expect("读回"),
        )
        .expect("合法 JSON");

        assert_eq!(value["schema_version"], serde_json::json!(3));
        assert_eq!(value["core_source"], serde_json::json!("none"));
        assert!(value["aria2_pid"].is_null(), "启动前失败不该有 PID");
        assert!(
            value["download_dir"].is_null(),
            "被守卫拒绝时 download_dir 必须是 null，不能是那个被拒的路径：{value}"
        );
        assert_eq!(
            value["config_path"],
            serde_json::json!(r"C:\Users\alice\AppData\Roaming\AriaHost\config.toml"),
            "配置路径要留着——面板才知道去看哪份文件"
        );
        assert_eq!(value["config_error"], serde_json::json!(reason));
        assert!(log.warn_lines().is_empty(), "正常写入不该有 WARN");
    }

    /// 写失败时也只是 WARN（启动前失败这条路径同样不能反过来把服务搞挂）。
    #[test]
    fn startup_failure_status_degrades_to_a_warning() {
        let dir = TempDir::new("startup-failure-unwritable");
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("造占位文件");
        let log = RecordingLog::default();

        write_startup_status(&blocker, &log, None, None, Some("坏配置"));

        let warns = log.warn_lines();
        assert_eq!(warns.len(), 1, "必须且只记一条 WARN：{warns:?}");
        assert!(warns[0].contains("降级"), "{warns:?}");
    }

    /// 序列化结果**绝不**出现机密词（模块文档的内容边界，回归即失败）。
    ///
    /// 注意 `download_dir` 字段名本身含 `download`（M5-f 明确要求加它），所以关键词表
    /// 不再包含 `download`；但**值**里也不许出现机密形态（secret/token/password）。
    #[test]
    fn serialized_form_carries_no_secrets() {
        let status = sample();
        let text = serde_json::to_string(&status).expect("序列化");
        let lower = text.to_ascii_lowercase();
        for word in ["secret", "token", "password", "passwd", "auth"] {
            assert!(!lower.contains(word), "状态文件不该出现 {word:?}：{text}");
        }
    }

    /// `core_source` 四个取值与 `CoreOrigin` 的映射都要稳（面板按字符串分支）。
    #[test]
    fn core_source_has_stable_strings_and_maps_every_origin() {
        assert_eq!(CoreSource::Bundled.as_str(), "bundled");
        assert_eq!(CoreSource::Managed.as_str(), "managed");
        assert_eq!(CoreSource::External.as_str(), "external");
        assert_eq!(CoreSource::None.as_str(), "none");
        assert_eq!(
            CoreSource::from_origin(CoreOrigin::Bundled),
            CoreSource::Bundled
        );
        assert_eq!(
            CoreSource::from_origin(CoreOrigin::Managed),
            CoreSource::Managed
        );
        assert_eq!(
            CoreSource::from_origin(CoreOrigin::External),
            CoreSource::External
        );
        // 序列化文本与 as_str 必须一致，否则日志与文件会说两套话。
        for source in [
            CoreSource::Bundled,
            CoreSource::Managed,
            CoreSource::External,
            CoreSource::None,
        ] {
            let json = serde_json::to_string(&source).expect("序列化来源");
            assert_eq!(json, format!("\"{}\"", source.as_str()));
        }
    }

    /// 「没有核心」的两种态写出来的字段都要是 null，而不是空串/0。
    #[test]
    fn absent_core_is_null_not_empty_or_zero() {
        let dir = TempDir::new("absent");
        let status = ServiceStatus::starting("0.1.3", "2026-11-26T02:33:34.510Z")
            .stamped_at("2026-11-26T02:40:00.000Z");
        let path = write(dir.path(), &status).expect("写");
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("读")).expect("JSON");
        assert_eq!(value["core_source"], serde_json::json!("none"));
        assert!(value["core_version"].is_null());
        assert!(value["aria2_pid"].is_null());

        // 停核心后从「有核心」转「无核心」，三项必须一起清空。
        let stopped = sample().without_core();
        assert_eq!(stopped.core_source, CoreSource::None);
        assert!(stopped.core_version.is_none());
        assert!(stopped.aria2_pid.is_none());
    }
}
