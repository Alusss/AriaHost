//! aria2c 运行时托管（面板侧，M2-2b）。
//!
//! [`ariahost_core::CoreRuntime`] 是**阻塞** API：`start` 会轮询到 RPC 就绪（上限 15s），
//! `stop` 会等三段优雅停止（实测停止一次 4.2s，其中 4.1s 是 aria2c 自己的关闭延迟）。
//! 因此它**不能**跑在 UI 线程上——UI 每帧只做非阻塞 `try_recv`。
//!
//! 线程模型：一条 `ariahost-core` 线程持有 `CoreRuntime`，收 [`HostCommand`]，
//! 把 `RuntimeEvent` 翻成 [`HostState`] 推回 UI。启动前的准备（定位 exe、
//! 建下载目录、渲染 conf）也在这条线程里做，失败同样变成一条 `Failed` 事件。
//!
//! 关窗时若 aria2 还在跑：`CoreHost::drop` 发 `Shutdown`，线程尽力 `stop`；
//! 但进程退得更快时由 M2-2a 的 Job Object（`KILL_ON_JOB_CLOSE`）兜底
//! （实测量级：父进程死后 0.5s 内 aria2c 消失）。

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use ariahost_common::config::{self as config_layer, Config};
use ariahost_common::ErrorCode;
use ariahost_core::{
    launch_spec, prepare_conf, resolve_core_exe, CoreRuntime, ProcessSpawner, RpcProbe,
    RuntimeEvent, RuntimePaths, Timeouts,
};

use crate::logbuf::LogBuffer;
use crate::view::{StatusView, Tone};

/// 停止一次预计要多久（秒）。
///
/// 实测（Lead 用真实 1.37.0）：`STATE stopping` → `STATE stopped` 共 4.2s，
/// 其中 `aria2.shutdown` 的 RPC 响应 0.002s、进程 4.108s 才退出；
/// `forceShutdown` 一样 4.113s（**强制也不更快**）。所以这 4 秒必须写进文案，
/// 否则用户会以为面板卡死。
pub const STOP_EXPECTED_SECS: u64 = 4;

/// 启动一次预计要多久（秒）：实测 `STATE starting` → `STATE running` 0.72s。
pub const START_EXPECTED_SECS: f64 = 1.0;

/// 面板侧看到的运行时状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostState {
    /// 还没启动过（面板刚开，或上次正常停止后）。
    Idle,
    /// 已 spawn，等 RPC 就绪。
    Starting,
    /// RPC 就绪。
    Running {
        /// 探测到的 aria2 版本。
        version: Arc<str>,
        /// 子进程 PID。
        pid: u32,
    },
    /// 正在停（保存会话）。
    Stopping,
    /// 已停。
    Stopped {
        /// 退出码（没拿到时为 `None`）。
        exit_code: Option<i32>,
        /// 是否走到了强杀。
        forced: bool,
    },
    /// 失败：带既有错误码（没有合适码时为 `None`）与中文原因。
    Failed {
        /// 既有错误码。
        code: Option<ErrorCode>,
        /// 中文原因（**不含**任何原始 stderr）。
        detail: Arc<str>,
    },
}

impl HostState {
    /// 是否处于「正在切换状态」的忙态（按钮置灰 + 转圈）。
    pub fn is_busy(&self) -> bool {
        matches!(self, Self::Starting | Self::Stopping)
    }

    /// 是否正在运行（RPC 应可用）。
    /// 是否处于「没在跑」的终态（`Idle` 也算：还没起过就没有可停的东西）。
    pub fn is_stopped(&self) -> bool {
        matches!(self, Self::Idle | Self::Stopped { .. })
    }

    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running { .. })
    }
}

/// 线程推给 UI 的一条更新。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostUpdate {
    /// 新状态。
    pub state: HostState,
    /// 附带的中文日志行（可选；UI 只用来写内部日志）。
    pub note: Option<Arc<str>>,
}

impl HostUpdate {
    fn bare(state: HostState) -> Self {
        Self { state, note: None }
    }

    fn noted(state: HostState, note: impl Into<Arc<str>>) -> Self {
        Self {
            state,
            note: Some(note.into()),
        }
    }
}

/// UI → 线程的命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostCommand {
    /// 启动（含定位 exe、渲染 conf）。
    Start,
    /// 优雅停止。
    Stop,
    /// 退出线程（关窗时尽力停一次）。
    Shutdown,
}

/// 启动前置条件的探测结果（面板一开机就能显示「未安装」而不是等第一次点击）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreProbe {
    /// 定位到的 aria2c（失败为 `None`）。
    pub exe: Option<PathBuf>,
    /// 下载目录（`dl.dir` 展开成绝对路径）。
    pub download_dir: Option<PathBuf>,
    /// RPC 端口（`aria2.rpc.port`）。
    pub port: u16,
    /// 前置条件不满足时的中文原因（`None` = 可以启动）。
    pub missing: Option<String>,
    /// 与 [`Self::missing`] 配套的既有错误码。
    pub code: Option<ErrorCode>,
}

impl Default for CoreProbe {
    fn default() -> Self {
        Self {
            exe: None,
            download_dir: None,
            port: ariahost_aria2::DEFAULT_PORT,
            missing: None,
            code: None,
        }
    }
}

impl CoreProbe {
    /// 前置条件是否齐备（按钮可用性用它，但按钮本身不因它置灰——
    /// 用户可能刚在引导卡里把核心装好，点击时线程会重新定位）。
    pub fn ready(&self) -> bool {
        self.missing.is_none()
    }
}

/// 线程真正需要的输入（缺一项就等于「不能启动」，原因记在 [`HostSetup::problem`]）。
struct HostInputs {
    config: Config,
    paths: RuntimePaths,
    secret: String,
    port: u16,
    download_dir: PathBuf,
}

/// 面板启动时的一次性准备结果。
///
/// **永不失败**：任何一步出问题都落到 [`Self::problem`]，面板照常开窗并把它显示出来
/// （一个坏掉的 config.toml 不应该让用户连窗口都看不到）。
pub struct HostSetup {
    inputs: Option<HostInputs>,
    problem: Option<(Option<ErrorCode>, String)>,
    probe: CoreProbe,
    /// RPC 凭据（worker 轮询任务列表要用；与启动用的 secret 是同一份）。
    rpc_secret: Option<String>,
}

impl HostSetup {
    /// 读配置 → 解 secret → 定核心/下载目录/端口。所有步骤都写启动日志。
    pub fn prepare(log: &LogBuffer) -> Self {
        let mut config = match config_layer::load() {
            Ok(config) => {
                log.push(format!(
                    "配置已读取：端口 {}，下载目录 {}，核心来源 {}",
                    config.aria2.rpc.port,
                    config.dl.dir,
                    config.aria2.source.as_str()
                ));
                config
            }
            Err(error) => {
                let code = error.code();
                let detail = error.to_string();
                log.push(format!(
                    "配置读取失败：{} {detail}（本批次不启动 aria2，先修配置）",
                    code.map_or("none", ErrorCode::code)
                ));
                return Self {
                    inputs: None,
                    problem: Some((code, format!("配置读取失败：{detail}"))),
                    probe: CoreProbe {
                        missing: Some(format!("配置读取失败：{detail}")),
                        code,
                        ..CoreProbe::default()
                    },
                    rpc_secret: None,
                };
            }
        };

        let mut problem: Option<(Option<ErrorCode>, String)> = None;

        // secret：配置非空即用，否则读/生成 secret.bin（DPAPI）。
        let secret = match config_layer::default_secret_path() {
            Some(path) => match config_layer::resolve_secret(&mut config, &path) {
                Ok(secret) => {
                    log.push(format!(
                        "RPC secret：{}（{} 字节；{}）",
                        if config.aria2.rpc.secret.trim().is_empty() {
                            "secret.bin 自动管理"
                        } else {
                            "config.toml 显式配置"
                        },
                        secret.len(),
                        path.display()
                    ));
                    Some(secret)
                }
                Err(error) => {
                    let code = error.code();
                    log.push(format!(
                        "RPC secret 解析失败：{} {error}",
                        code.map_or("none", ErrorCode::code)
                    ));
                    problem = Some((code, format!("RPC secret 解析失败：{error}")));
                    None
                }
            },
            None => {
                problem = Some((None, "拿不到 %APPDATA%（secret.bin 路径不可用）".to_owned()));
                None
            }
        };

        let paths = RuntimePaths::from_env();
        if paths.is_none() && problem.is_none() {
            problem = Some((None, "拿不到 %APPDATA%（运行时目录不可用）".to_owned()));
        }

        let download_dir = match config_layer::download_dir_absolute(&config) {
            Ok(dir) => Some(dir),
            Err(error) => {
                let code = error.code();
                log.push(format!(
                    "下载目录不可用：{} {error}",
                    code.map_or("none", ErrorCode::code)
                ));
                if problem.is_none() {
                    problem = Some((code, format!("下载目录不可用：{error}")));
                }
                None
            }
        };

        let exe = match resolve_core_exe(&config) {
            Ok(exe) => {
                log.push(format!("aria2 核心：{}", exe.display()));
                Some(exe)
            }
            Err(error) => {
                let code = error.code();
                log.push(format!(
                    "aria2 核心未就绪：{} {error}",
                    code.map_or("none", ErrorCode::code)
                ));
                if problem.is_none() {
                    problem = Some((code, error.to_string()));
                }
                None
            }
        };

        let port = config.aria2.rpc.port;
        let probe = CoreProbe {
            missing: problem.as_ref().map(|(_, detail)| detail.clone()),
            code: problem.as_ref().and_then(|(code, _)| *code),
            exe: exe.clone(),
            download_dir: download_dir.clone(),
            port,
        };

        let inputs = match (secret.clone(), paths, download_dir) {
            (Some(secret), Some(paths), Some(download_dir)) => Some(HostInputs {
                config,
                paths,
                secret,
                port,
                download_dir,
            }),
            _ => None,
        };

        Self {
            inputs,
            problem,
            probe,
            rpc_secret: secret,
        }
    }

    /// 前置条件不满足时的原因（`None` = 可以启动）。
    pub fn problem(&self) -> Option<(Option<ErrorCode>, &str)> {
        self.problem
            .as_ref()
            .map(|(code, detail)| (*code, detail.as_str()))
    }

    /// 启动前置条件的探测结果（面板开窗就显示「未安装」靠它）。
    pub fn probe(&self) -> &CoreProbe {
        &self.probe
    }

    /// worker 需要的 RPC 凭据与节流参数（端口 + secret + 刷新间隔）。
    pub fn worker_options(&self, log: &LogBuffer) -> crate::worker::WorkerOptions {
        crate::worker::WorkerOptions::detect(self.probe.port, self.rpc_secret.clone(), log)
    }
}

/// 面板持有的托管句柄。
pub struct CoreHost {
    commands: Option<Sender<HostCommand>>,
    updates: Receiver<HostUpdate>,
    /// 线程句柄：只持有所有权（drop 即 detach），不 join——`stop` 要 4 秒，不能卡关窗。
    _handle: Option<JoinHandle<()>>,
    probe: CoreProbe,
}

impl CoreHost {
    /// 起托管线程。`setup` 源自 [`HostSetup::prepare`]。
    pub fn spawn(log: Arc<LogBuffer>, setup: HostSetup) -> Self {
        let HostSetup {
            inputs,
            problem,
            probe,
            rpc_secret: _,
        } = setup;
        let (command_tx, command_rx) = mpsc::channel();
        let (update_tx, update_rx) = mpsc::channel();
        let thread_log = Arc::clone(&log);
        let spawned = thread::Builder::new()
            .name("ariahost-core".to_owned())
            .spawn(move || run(inputs, problem, &command_rx, &update_tx, thread_log));
        match spawned {
            Ok(handle) => Self {
                commands: Some(command_tx),
                updates: update_rx,
                _handle: Some(handle),
                probe,
            },
            Err(error) => {
                // 线程都起不来：面板仍要开，但按钮必须能说明为什么点了没反应。
                log.push(format!(
                    "aria2 托管线程启动失败：{error}（面板继续，无法启停核心）"
                ));
                Self {
                    commands: Some(command_tx),
                    updates: update_rx,
                    _handle: None,
                    probe,
                }
            }
        }
    }

    /// 启动前置条件的探测结果。
    pub fn probe(&self) -> &CoreProbe {
        &self.probe
    }

    /// 请求启动（非阻塞）。
    pub fn start(&self) {
        self.send(HostCommand::Start);
    }

    /// 请求停止（非阻塞；真正停下来要 4 秒，结果走事件）。
    pub fn stop(&self) {
        self.send(HostCommand::Stop);
    }

    /// 非阻塞取一条状态更新。
    pub fn try_recv(&self) -> Option<HostUpdate> {
        self.updates.try_recv().ok()
    }

    /// 托管线程是否还在（供状态区解释「按钮点了没反应」）。
    pub fn is_alive(&self) -> bool {
        self._handle.is_some()
    }

    fn send(&self, command: HostCommand) {
        if let Some(commands) = &self.commands {
            let _ = commands.send(command);
        }
    }
}

impl Drop for CoreHost {
    fn drop(&mut self) {
        // 尽力发一次 Stop 再走：线程收到 Shutdown 会 stop 一次。
        self.send(HostCommand::Shutdown);
    }
}

/// 线程主体：等命令 → 执行 → 把事件翻成 [`HostUpdate`]。
fn run(
    inputs: Option<HostInputs>,
    problem: Option<(Option<ErrorCode>, String)>,
    commands: &Receiver<HostCommand>,
    updates: &Sender<HostUpdate>,
    log: Arc<LogBuffer>,
) {
    let mut runtime: Option<CoreRuntime> = None;
    while let Ok(command) = commands.recv() {
        match command {
            HostCommand::Start => {
                let Some(inputs) = inputs.as_ref() else {
                    if let Some((code, detail)) = problem.clone() {
                        emit(
                            updates,
                            HostUpdate::noted(
                                HostState::Failed {
                                    code,
                                    detail: Arc::from(detail.as_str()),
                                },
                                "启动被拒：前置条件不满足",
                            ),
                        );
                    }
                    continue;
                };
                let host = runtime.get_or_insert_with(|| {
                    CoreRuntime::new(
                        Box::new(ProcessSpawner),
                        Box::new(RpcProbe::new(inputs.port, &inputs.secret)),
                        Timeouts::default(),
                    )
                });
                start_once(host, inputs, updates, &log);
            }
            HostCommand::Stop => match runtime.as_mut() {
                Some(host) => stop_once(host, updates, &log),
                // 没起过就直接回「已停止」，让 UI 的忙态能被解开。
                None => {
                    emit(
                        updates,
                        HostUpdate::bare(HostState::Stopped {
                            exit_code: None,
                            forced: false,
                        }),
                    );
                }
            },
            HostCommand::Shutdown => {
                if let Some(host) = runtime.as_mut() {
                    let _ = host.stop(&mut |event| {
                        forward(event, updates, &log);
                    });
                }
                return;
            }
        }
    }
    // 命令通道断开（UI 已 drop）：兜底停一次，别留孤儿。
    if let Some(host) = runtime.as_mut() {
        let _ = host.stop(&mut |_event| {});
    }
}

/// 一次启动：定位 → 建目录 → 渲染 conf → spawn → 等就绪。
fn start_once(
    runtime: &mut CoreRuntime,
    inputs: &HostInputs,
    updates: &Sender<HostUpdate>,
    log: &Arc<LogBuffer>,
) {
    if let Err(error) = std::fs::create_dir_all(&inputs.download_dir) {
        let detail = format!(
            "建下载目录失败：{error}（{}）；请在设置里改 dl.dir 或手工建目录",
            inputs.download_dir.display()
        );
        log.push(detail.clone());
        emit(
            updates,
            HostUpdate::noted(
                HostState::Failed {
                    code: None,
                    detail: Arc::from(detail.as_str()),
                },
                "启动失败：下载目录不可写",
            ),
        );
        return;
    }

    let exe = match resolve_core_exe(&inputs.config) {
        Ok(exe) => exe,
        Err(error) => {
            let detail = error.to_string();
            log.push(format!("启动失败：{detail}"));
            emit(
                updates,
                HostUpdate::noted(
                    HostState::Failed {
                        code: error.code(),
                        detail: Arc::from(detail.as_str()),
                    },
                    "启动失败：定位不到 aria2c",
                ),
            );
            return;
        }
    };

    if let Err(error) = prepare_conf(
        &inputs.config,
        &inputs.paths,
        &inputs.secret,
        &inputs.download_dir,
    ) {
        let detail = error.to_string();
        log.push(format!("启动失败：{detail}"));
        emit(
            updates,
            HostUpdate::noted(
                HostState::Failed {
                    code: error.code(),
                    detail: Arc::from(detail.as_str()),
                },
                "启动失败：渲染 aria2.conf",
            ),
        );
        return;
    }
    log.push(format!(
        "启动 aria2c：{}（conf {}）",
        exe.display(),
        inputs.paths.conf_path().display()
    ));

    let spec = launch_spec(&inputs.config, &inputs.paths, exe, &inputs.secret);
    match runtime.start(&spec, &mut |event| forward(event, updates, log)) {
        Ok(_) => {}
        Err(error) => {
            // `AlreadyRunning` 不会经事件上报，这里补一条，避免 UI 永远停在忙态。
            if matches!(error, ariahost_core::RuntimeError::AlreadyRunning) {
                let detail = error.to_string();
                log.push(format!("启动被拒：{detail}"));
                emit(
                    updates,
                    HostUpdate::bare(HostState::Failed {
                        code: error.code(),
                        detail: Arc::from(detail.as_str()),
                    }),
                );
            }
            // 其余错误已经由 RuntimeEvent::Failed 上报过，不重复推。
        }
    }
}

/// 一次停止：三段式（优雅 → force → 强杀），耗时由 core 决定。
fn stop_once(runtime: &mut CoreRuntime, updates: &Sender<HostUpdate>, log: &Arc<LogBuffer>) {
    match runtime.stop(&mut |event| forward(event, updates, log)) {
        Ok(status) => {
            log.push(format!(
                "aria2 已停止：状态 {}，退出码 {:?}，强制={}",
                status.state.as_str(),
                status.exit_code,
                status.forced
            ));
        }
        Err(error) => {
            let detail = error.to_string();
            log.push(format!("停止失败：{detail}"));
            emit(
                updates,
                HostUpdate::noted(
                    HostState::Failed {
                        code: error.code(),
                        detail: Arc::from(detail.as_str()),
                    },
                    "停止失败",
                ),
            );
        }
    }
}

/// `RuntimeEvent` → [`HostUpdate`]（唯一的状态翻译点）。
fn forward(event: RuntimeEvent, updates: &Sender<HostUpdate>, log: &Arc<LogBuffer>) {
    let update = match event {
        RuntimeEvent::Starting => {
            HostUpdate::noted(HostState::Starting, "正在启动 aria2，等待 RPC 就绪")
        }
        RuntimeEvent::Running { version, pid } => HostUpdate::noted(
            HostState::Running {
                version: Arc::from(version.as_str()),
                pid,
            },
            format!("aria2 已就绪：RPC {version}，PID {pid}"),
        ),
        RuntimeEvent::Stopping => {
            HostUpdate::noted(HostState::Stopping, "正在保存会话并停止 aria2")
        }
        RuntimeEvent::Stopped { exit_code, forced } => HostUpdate::noted(
            HostState::Stopped { exit_code, forced },
            format!("aria2 已停止（退出码 {exit_code:?}，强制={forced}）"),
        ),
        RuntimeEvent::Failed { code, hint } => HostUpdate::noted(
            HostState::Failed {
                code,
                detail: Arc::from(hint.as_str()),
            },
            format!("aria2 失败：{hint}"),
        ),
        RuntimeEvent::Warning { detail } => {
            // 告警不改状态（例如 Job Object 建不起来 → 孤儿守卫降级），只记日志。
            log.push(format!("aria2 告警：{detail}"));
            return;
        }
    };
    emit(updates, update);
}

/// 推一条更新；接收端没了（UI 已退）就返回 `false`。
fn emit(updates: &Sender<HostUpdate>, update: HostUpdate) -> bool {
    updates.send(update).is_ok()
}

/// 状态区文案（纯函数，单测覆盖每一种状态）。
pub fn status_view(state: &HostState, probe: &CoreProbe) -> StatusView {
    let core_line = match &probe.exe {
        Some(exe) => format!("核心：{}", exe.display()),
        None => format!(
            "版本库：%LOCALAPPDATA%\\AriaHost\\cache\\aria2\\versions（端口 {}）",
            probe.port
        ),
    };
    let dir_line = match &probe.download_dir {
        Some(dir) => format!("下载目录：{}", dir.display()),
        None => "下载目录：不可用（见上方原因）".to_owned(),
    };

    match state {
        HostState::Idle => match &probe.missing {
            Some(detail) => StatusView {
                badge: "未安装".to_owned(),
                tone: Tone::Warn,
                code: probe.code.map(ErrorCode::code),
                headline: detail.clone(),
                details: vec![dir_line],
                next_step: Some(install_hint(probe.code)),
            },
            None => StatusView {
                badge: "已停止".to_owned(),
                tone: Tone::Idle,
                code: None,
                headline: "aria2 核心未启动".to_owned(),
                details: vec![core_line, dir_line, rpc_line(probe.port)],
                next_step: Some("点「启动 aria2」开始下载。".to_owned()),
            },
        },
        HostState::Starting => StatusView {
            badge: "启动中".to_owned(),
            tone: Tone::Warn,
            code: None,
            headline: format!(
                "正在启动 aria2，等待 RPC 就绪…（正常 {START_EXPECTED_SECS} 秒内完成）"
            ),
            details: vec![core_line],
            next_step: None,
        },
        HostState::Running { version, pid } => StatusView {
            badge: "运行中".to_owned(),
            tone: Tone::Good,
            code: None,
            headline: format!("运行中（RPC {version}）"),
            details: vec![
                format!("进程 PID：{pid}"),
                core_line,
                dir_line,
                rpc_line(probe.port),
            ],
            next_step: None,
        },
        HostState::Stopping => StatusView {
            badge: "停止中".to_owned(),
            tone: Tone::Warn,
            code: None,
            headline: format!("正在保存会话并停止，约需 {STOP_EXPECTED_SECS} 秒…"),
            details: vec![format!(
                "这 {STOP_EXPECTED_SECS} 秒是 aria2c 自己的关闭延迟（实测 4.1 秒，强制 shutdown 也一样快不了），不是面板卡住"
            )],
            next_step: None,
        },
        HostState::Stopped { exit_code, forced } => StatusView {
            badge: "已停止".to_owned(),
            tone: if *forced { Tone::Warn } else { Tone::Idle },
            code: None,
            headline: if *forced {
                "aria2 核心已停止（优雅停止超时，走了强制路径）".to_owned()
            } else {
                "aria2 核心已停止".to_owned()
            },
            details: vec![
                format!(
                    "退出码：{}",
                    exit_code.map_or_else(|| "未拿到".to_owned(), |code| code.to_string())
                ),
                core_line,
                dir_line,
            ],
            next_step: Some("会话已保存；再次启动会从 session 续传。".to_owned()),
        },
        HostState::Failed { code, detail } => StatusView {
            badge: "失败".to_owned(),
            tone: Tone::Bad,
            code: code.map(ErrorCode::code),
            headline: detail.to_string(),
            details: vec![core_line, dir_line],
            next_step: Some(failure_hint(*code)),
        },
    }
}

/// RPC 端点行。
fn rpc_line(port: u16) -> String {
    format!("RPC：http://127.0.0.1:{port}{}", ariahost_aria2::RPC_PATH)
}

/// 「未安装核心」时的可执行建议。
fn install_hint(code: Option<ErrorCode>) -> String {
    match code {
        Some(ErrorCode::Aria2NotFound) => {
            "先装下载核心：面板首屏的「下载核心未就绪」卡片可在线下载，也可指定你已有的 aria2c。"
                .to_owned()
        }
        Some(ErrorCode::ConfigParseFailed) => {
            "修好或删除 %APPDATA%\\AriaHost\\config.toml 后重开面板（删掉不心疼：会按默认值重建）。"
                .to_owned()
        }
        Some(ErrorCode::ConfigSchemaTooNew) => {
            "配置文件比本程序新（降级过）；升级 AriaHost 或先备份再重置配置，**不要直接删**。"
                .to_owned()
        }
        Some(ErrorCode::CredentialCryptoFailed) => {
            "secret.bin 解不开（换了 Windows 用户或文件损坏）：删掉 %APPDATA%\\AriaHost\\secret.bin 后重开，会自动重建。"
                .to_owned()
        }
        _ => "看启动日志（面板底部）里的第一条失败原因，按提示修复后重开面板。".to_owned(),
    }
}

/// 启动/停止失败时的可执行建议（按码分流）。
fn failure_hint(code: Option<ErrorCode>) -> String {
    match code {
        Some(ErrorCode::Aria2NotFound) => {
            "aria2c 找不到：用首屏「下载核心未就绪」卡片装一次，或在设置页把核心来源改成 external 并填路径。"
                .to_owned()
        }
        Some(ErrorCode::Aria2StartTimeout) => {
            "就绪超时：看 %APPDATA%\\AriaHost\\logs\\aria2.log；端口被占时改 aria2.rpc.port 后重试。"
                .to_owned()
        }
        Some(ErrorCode::PortConflict) => {
            "端口被占：改 aria2.rpc.port（或关掉占着 6800 的另一个 aria2）后重试。".to_owned()
        }
        _ => {
            "看 %APPDATA%\\AriaHost\\logs\\aria2.log 与 runtime\\aria2.conf；本批不自动修复，可重试一次。"
                .to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe_ok() -> CoreProbe {
        CoreProbe {
            exe: Some(PathBuf::from("C:\\cache\\aria2c.exe")),
            download_dir: Some(PathBuf::from("C:\\Users\\Alusss\\Downloads")),
            port: 6800,
            missing: None,
            code: None,
        }
    }

    fn probe_missing() -> CoreProbe {
        CoreProbe {
            exe: None,
            download_dir: Some(PathBuf::from("C:\\Users\\Alusss\\Downloads")),
            port: 6800,
            missing: Some("找不到 aria2c：版本库下没有 active.json".to_owned()),
            code: Some(ErrorCode::Aria2NotFound),
        }
    }

    #[test]
    fn every_required_status_word_is_chinese_and_present() {
        let cases = [
            HostState::Idle,
            HostState::Starting,
            HostState::Stopping,
            HostState::Stopped {
                exit_code: Some(0),
                forced: false,
            },
        ];
        let badges: Vec<String> = cases
            .iter()
            .map(|state| status_view(state, &probe_ok()).badge)
            .collect();
        assert_eq!(
            badges,
            vec!["已停止", "启动中", "停止中", "已停止"],
            "四种非运行态都要有自己的中文状态词"
        );
    }

    #[test]
    fn idle_without_core_shows_not_installed_and_install_hint() {
        let view = status_view(&HostState::Idle, &probe_missing());
        assert_eq!(view.badge, "未安装");
        assert_eq!(view.tone, Tone::Warn);
        assert_eq!(view.code, Some("AH-E2001"));
        let next = view.next_step.unwrap_or_default();
        assert!(next.contains("下载核心未就绪"), "{next}");
    }

    #[test]
    fn stopped_state_says_it_saved_the_session() {
        let view = status_view(
            &HostState::Stopped {
                exit_code: Some(0),
                forced: false,
            },
            &probe_ok(),
        );
        assert_eq!(view.tone, Tone::Idle);
        assert_eq!(view.code, None);
        assert!(view.headline.contains("已停止"), "{}", view.headline);
        assert!(view.next_step.unwrap_or_default().contains("续传"));
    }

    #[test]
    fn forced_stop_is_a_warning_not_a_silent_success() {
        let view = status_view(
            &HostState::Stopped {
                exit_code: Some(1),
                forced: true,
            },
            &probe_ok(),
        );
        assert_eq!(view.tone, Tone::Warn);
        assert!(view.headline.contains("强制"), "{}", view.headline);
    }

    #[test]
    fn stopping_text_carries_the_measured_four_seconds() {
        let view = status_view(&HostState::Stopping, &probe_ok());
        assert!(view.headline.contains("4 秒"), "{}", view.headline);
        assert!(view.headline.contains("停止"), "{}", view.headline);
        // 详情要解释「不是卡住」——这是用户最常见的误判。
        let joined = view.details.join("\n");
        assert!(joined.contains("aria2c"), "{joined}");
    }

    #[test]
    fn running_state_puts_the_rpc_version_in_the_headline() {
        let view = status_view(
            &HostState::Running {
                version: Arc::from("1.37.0"),
                pid: 4242,
            },
            &probe_ok(),
        );
        assert_eq!(view.badge, "运行中");
        assert_eq!(view.tone, Tone::Good);
        assert!(view.headline.contains("1.37.0"), "{}", view.headline);
        let joined = view.details.join("\n");
        assert!(joined.contains("4242"), "{joined}");
        assert!(joined.contains("127.0.0.1:6800/jsonrpc"), "{joined}");
    }

    #[test]
    fn starting_state_promises_sub_second_readiness() {
        let view = status_view(&HostState::Starting, &probe_ok());
        assert!(view.headline.contains("1 秒"), "{}", view.headline);
        assert!(view.headline.contains("RPC"), "{}", view.headline);
    }

    #[test]
    fn failed_state_keeps_the_code_and_gives_a_next_step() {
        let view = status_view(
            &HostState::Failed {
                code: Some(ErrorCode::Aria2StartTimeout),
                detail: Arc::from("aria2c 启动后 15000ms 内 RPC 未就绪，已尝试清理子进程"),
            },
            &probe_ok(),
        );
        assert_eq!(view.badge, "失败");
        assert_eq!(view.tone, Tone::Bad);
        assert_eq!(view.code, Some("AH-E2002"));
        assert!(view.headline.contains("15000"), "{}", view.headline);
        let next = view.next_step.unwrap_or_default();
        assert!(next.contains("aria2.log"), "{next}");
    }

    #[test]
    fn failed_without_code_still_explains_what_to_do() {
        let view = status_view(
            &HostState::Failed {
                code: None,
                detail: Arc::from("建下载目录失败：拒绝访问（D:\\dl）"),
            },
            &probe_ok(),
        );
        assert_eq!(view.code, None, "没有合适码就不能编一个");
        assert!(view.next_step.is_some());
    }

    #[test]
    fn busy_states_are_reported_as_busy() {
        assert!(HostState::Starting.is_busy());
        assert!(HostState::Stopping.is_busy());
        assert!(!HostState::Idle.is_busy());
        assert!(!HostState::Running {
            version: Arc::from("1.37.0"),
            pid: 1
        }
        .is_busy());
    }

    #[test]
    fn probe_ready_only_when_nothing_is_missing() {
        assert!(probe_ok().ready());
        assert!(!probe_missing().ready());
    }

    #[test]
    fn config_errors_have_their_own_install_hints() {
        let parse = install_hint(Some(ErrorCode::ConfigParseFailed));
        assert!(parse.contains("config.toml"), "{parse}");
        // 「schema 过新」绝不能建议删文件（架构 §4.12 主按钮口径）。
        let newer = install_hint(Some(ErrorCode::ConfigSchemaTooNew));
        assert!(newer.contains("不要直接删"), "{newer}");
        let crypto = install_hint(Some(ErrorCode::CredentialCryptoFailed));
        assert!(crypto.contains("secret.bin"), "{crypto}");
    }

    /// 测试用 setup：不读配置、不碰 DPAPI，固定一条「前置条件不满足」。
    fn setup_failed() -> HostSetup {
        HostSetup {
            inputs: None,
            problem: Some((
                Some(ErrorCode::Aria2NotFound),
                "未安装 aria2 核心（测试桩）".to_owned(),
            )),
            probe: probe_ok(),
            rpc_secret: None,
        }
    }

    /// 回归防线（Lead 缺陷 1）：`Stop` 必须**每次**都回一条状态，且不能卡住不发。
    ///
    /// 曾经的症状：点「移除」后 30ms 点「停止」，界面冻在「已等待 4.2 秒」。
    /// 托管线程是「命令进 → 事件出」的单线程循环，任何时候 `recv` 到 Stop 都必须
    /// 走完并 emit；这条测试用 3 秒超时把「静默卡死」钉死在 CI 上。
    #[test]
    fn stop_always_answers_and_never_hangs() {
        use std::time::{Duration, Instant};

        let log = Arc::new(LogBuffer::new());
        let host = CoreHost::spawn(Arc::clone(&log), setup_failed());
        host.start(); // 前置条件不满足 → 必须回 Failed，而不是毫无反应
        host.stop(); // 没有 runtime → 直接 Stopped
        host.stop(); // 连点两次也必须各回一条

        let deadline = Instant::now() + Duration::from_secs(3);
        let (mut failed, mut stopped) = (0usize, 0usize);
        while Instant::now() < deadline && stopped < 2 {
            while let Some(update) = host.try_recv() {
                match &update.state {
                    HostState::Failed { .. } => failed += 1,
                    HostState::Stopped { .. } => stopped += 1,
                    _ => {}
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(failed, 1, "启动被拒必须有一条 Failed");
        assert_eq!(
            stopped, 2,
            "每次 Stop 都必须回一条 Stopped；少一条就是 UI 忙态永远解不开"
        );
    }
}
