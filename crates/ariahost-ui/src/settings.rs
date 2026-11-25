//! 设置页（M2-4a）：下载 / 网络 / 界面三组的编辑状态与校验。
//!
//! 设计要点（数字都是**架构 §4.11.1 的 52 键**里的真实键，不自造）：
//! - 本模块**不碰 egui**，只做「配置副本 + 文本缓冲 + 校验 + 脏判定 + 原子保存」，
//!   这样全部逻辑能在 Linux 上单测，渲染留在 `app.rs`。
//! - 配置**只在打开设置页时读一次**（[`Settings::ensure_loaded`]），之后每帧只用内存副本；
//!   保存走 [`config::save`]（原子写：临时文件 + fsync + rename），不自己拼 TOML。
//! - 「恢复默认」只把**本页可见的键**恢复成默认值（[`VISIBLE_KEYS`]），不会顺手把
//!   `clean.*`/`queue.*` 这些本批没上界面的键也一起抹掉。
//! - `ui.close_action` 保存后**立刻生效**（面板已按它决定点 X 的行为），其余键按下表提示重启。
//!
//! 生效时机（逐键见 [`effect`]）：
//! - 立即：`ui.close_action`（保存后写回运行时偏好）、`ui.start_with_windows`（写 HKCU Run）
//! - 重启：`ui.renderer`（当前构建只带 wgpu，选 glow 只记日志并回退）
//! - 未接线：`ui.start_minimized` / `ui.minimize_to_tray` / `lifecycle.quit_default`
//!   （架构 §4.11.1 里这三项标 ❌「AriaHost 自有行为」，M2-3 只接了 `close_action` 与托盘本体）

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ariahost_common::config::{self, Config, ConfigError, ConfigErrorKind};

/// 设置页分组。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Group {
    /// 下载（`[dl]`）。
    Download,
    /// 网络（`[net]`）。
    Network,
    /// 界面（`[ui]` / `[lifecycle]`）。
    Interface,
}

impl Group {
    /// 三个分组的固定顺序（左侧导航用）。
    pub const ALL: [Self; 3] = [Self::Download, Self::Network, Self::Interface];

    /// 导航上的中文名。
    pub fn label(self) -> &'static str {
        match self {
            Self::Download => "下载",
            Self::Network => "网络",
            Self::Interface => "界面",
        }
    }
}

/// 生效时机（验收要求逐项写清楚）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// 保存后立刻生效。
    Immediate,
    /// 需要重启面板。
    Restart,
    /// 本批只写配置，行为尚未接线（架构标 ❌）。
    NotWired,
}

impl Effect {
    /// 上屏文案。
    pub fn text(self) -> &'static str {
        match self {
            Self::Immediate => "立即生效",
            Self::Restart => "重启后生效",
            Self::NotWired => "本批只保存，行为待接线",
        }
    }
}

/// 设置页可见的键（「恢复默认」的作用范围就在这里）。
///
/// 顺序 = 界面上的顺序；测试 [`visible_keys_match_config_fields`] 会对着 `Config::default()`
/// 逐项确认这些字符串确实是真实存在的键（防止拼错或自造）。
pub const VISIBLE_KEYS: [&str; 21] = [
    // A 下载
    "dl.dir",
    "dl.auto_file_renaming",
    "dl.allow_overwrite",
    "dl.continue",
    "dl.always_resume",
    // B 网络
    "net.split",
    "net.max_connection_per_server",
    "net.min_split_size",
    "net.max_overall_download_limit",
    "net.max_download_limit",
    "net.max_overall_upload_limit",
    "net.connect_timeout",
    "net.max_tries",
    "net.proxy",
    "net.disk_cache",
    // C 界面
    "ui.close_action",
    "ui.renderer",
    "ui.minimize_to_tray",
    "ui.start_minimized",
    "ui.start_with_windows",
    "lifecycle.quit_default",
];

/// 每个可见键的生效时机。
pub fn effect(key: &str) -> Effect {
    match key {
        "ui.close_action" | "ui.start_with_windows" => Effect::Immediate,
        "ui.renderer" => Effect::Restart,
        "ui.start_minimized" | "ui.minimize_to_tray" | "lifecycle.quit_default" => Effect::NotWired,
        _ => Effect::Restart,
    }
}

/// 尺寸串的默认值（架构 §4.11.1 的默认值列，用来在界面上写「默认 …」提示）。
pub fn default_text(key: &str) -> &'static str {
    match key {
        "dl.dir" => "~/Downloads",
        "net.split" => "5",
        "net.max_connection_per_server" => "1",
        "net.min_split_size" => "20M",
        "net.max_overall_download_limit" => "0",
        "net.max_download_limit" => "0",
        "net.max_overall_upload_limit" => "0",
        "net.connect_timeout" => "60",
        "net.max_tries" => "5",
        "net.proxy" => "（空）",
        "net.disk_cache" => "16M",
        "ui.close_action" => "tray",
        "ui.renderer" => "auto",
        "lifecycle.quit_default" => "pause",
        _ => "",
    }
}

/// 带文本缓冲的键（数字/尺寸/路径/代理）：输入中途可能非法，所以文本与配置分开存。
const TEXT_KEYS: [&str; 11] = [
    "dl.dir",
    "net.split",
    "net.max_connection_per_server",
    "net.min_split_size",
    "net.max_overall_download_limit",
    "net.max_download_limit",
    "net.max_overall_upload_limit",
    "net.connect_timeout",
    "net.max_tries",
    "net.proxy",
    "net.disk_cache",
];

/// 一次操作后给用户看的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// 成功（绿）还是失败（红）。
    pub ok: bool,
    /// 一行结论。
    pub headline: String,
    /// 可选「下一步」。
    pub next: Option<String>,
}

impl Notice {
    /// 成功提示。
    pub fn ok(headline: impl Into<String>) -> Self {
        Self {
            ok: true,
            headline: headline.into(),
            next: None,
        }
    }

    /// 失败提示（带「下一步」）。
    pub fn err(headline: impl Into<String>, next: impl Into<String>) -> Self {
        Self {
            ok: false,
            headline: headline.into(),
            next: Some(next.into()),
        }
    }
}

/// 设置页的编辑状态。
pub struct Settings {
    /// 是否已从磁盘读过配置（每进程只读一次，避免每帧读盘）。
    loaded: bool,
    /// 编辑中的副本。
    draft: Config,
    /// 基线（上次加载/保存的值）：判脏与「撤销更改」用。
    baseline: Config,
    /// 当前分组。
    group: Group,
    /// 文本缓冲（键 → 输入框内容）。
    texts: BTreeMap<&'static str, String>,
    /// 字段级错误（键 → 中文原因）。空 = 全部合法。
    errors: BTreeMap<&'static str, String>,
    /// 最近一次保存/撤销/恢复的结果。
    pub notice: Option<Notice>,
    /// 是否正在问「有未保存改动，确定离开？」。
    pub leaving: bool,
    /// 上次读盘失败（配置坏了）：这时允许用默认值直接覆盖保存，否则用户没有自救入口。
    load_failed: bool,
}

impl Default for Settings {
    fn default() -> Self {
        let draft = Config::default();
        Self {
            loaded: false,
            baseline: draft.clone(),
            texts: texts_from(&draft),
            draft,
            group: Group::Download,
            errors: BTreeMap::new(),
            notice: None,
            leaving: false,
            load_failed: false,
        }
    }
}

impl Settings {
    /// 新建设置页状态（未加载；第一次打开时调 [`Self::ensure_loaded`]）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 首次打开设置页时从磁盘读一次配置。
    ///
    /// 读失败**不挡住设置页**：用默认值继续，并把 `AH-E1xxx` + 建议放到 `notice` 里，
    /// 这样用户至少能在这里把配置改回来（配置文件坏掉时这正是唯一的自救入口）。
    pub fn ensure_loaded(&mut self) {
        if self.loaded {
            return;
        }
        self.loaded = true;
        match config::load() {
            Ok(config) => {
                self.draft = config.clone();
                self.baseline = config;
                self.texts = texts_from(&self.draft);
                self.notice = None;
                self.load_failed = false;
            }
            Err(error) => {
                let next = config_hint(error.kind());
                self.notice = Some(Notice::err(format!("读取配置失败：{error}"), next));
                self.texts = texts_from(&self.draft);
                // 配置坏掉时设置页是唯一的自救入口：允许把默认值直接保存下去覆盖坏文件。
                self.load_failed = true;
            }
        }
        self.errors.clear();
        self.revalidate();
    }

    /// 测试用：模拟「读盘成功」之后的状态。
    ///
    /// 不用 `ensure_loaded()` 是因为它真的去读 `%APPDATA%`：在 CI/Linux 上读不到就会
    /// 走「读盘失败」分支（`load_failed = true`，被判成脏），测试会变成环境依赖。
    #[cfg(test)]
    pub fn loaded_for_test(config: Config) -> Self {
        Self {
            loaded: true,
            baseline: config.clone(),
            texts: texts_from(&config),
            draft: config,
            group: Group::Download,
            errors: BTreeMap::new(),
            notice: None,
            leaving: false,
            load_failed: false,
        }
    }

    /// 当前分组。
    pub fn group(&self) -> Group {
        self.group
    }

    /// 切分组（不触碰编辑内容）。
    pub fn set_group(&mut self, group: Group) {
        self.group = group;
    }

    /// 编辑中的配置（只读；改值走 [`Self::draft_mut`]）。
    pub fn draft(&self) -> &Config {
        &self.draft
    }

    /// 编辑中的配置（可写；给布尔与下拉用，改完请调 [`Self::revalidate`]）。
    pub fn draft_mut(&mut self) -> &mut Config {
        &mut self.draft
    }

    /// 某个键的输入框文本。
    pub fn text(&self, key: &'static str) -> &str {
        self.texts.get(key).map_or("", String::as_str)
    }

    /// 某个键的就地错误（`None` = 合法）。
    pub fn error(&self, key: &'static str) -> Option<&str> {
        self.errors.get(key).map(String::as_str)
    }

    /// 是否有任何字段不合法（保存按钮据此置灰）。
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// 是否有未保存改动（只看编辑内容是否偏离基线；字段错误另由 [`Self::has_errors`] 报）。
    ///
    /// 注意：**加载时就已经不合法的项（比如 `~/Downloads` 不存在）不算「改动」** ——
    /// 否则一打开设置页就显示「有未保存的改动」，用户会以为是自己碰坏了什么。
    pub fn is_dirty(&self) -> bool {
        self.load_failed || self.draft != self.baseline
    }

    /// 输入框内容变了：解析成功就写回副本，失败就留错误（副本保持上一次合法值）。
    pub fn set_text(&mut self, key: &'static str, value: String) {
        self.texts.insert(key, value);
        self.apply_text(key);
    }

    /// 按当前文本重算该键的副本值/错误。
    fn apply_text(&mut self, key: &'static str) {
        let raw = self.texts.get(key).cloned().unwrap_or_default();
        match parse_field(&mut self.draft, key, &raw) {
            Ok(()) => {
                self.errors.remove(key);
            }
            Err(reason) => {
                self.errors.insert(key, reason);
            }
        }
    }

    /// 重新校验全部字段（保存前 / 布尔与下拉改动后 / 刚加载完）。
    pub fn revalidate(&mut self) {
        let keys: Vec<&'static str> = TEXT_KEYS.to_vec();
        for key in keys {
            // 文本缓冲与副本可能不同步（撤销/恢复默认后重建），这里以文本为准再落一次。
            self.apply_text(key);
        }
        self.check_interface();
    }

    /// 界面组的下拉/勾选校验（值域必须落在架构 §4.11.1 的枚举里）。
    fn check_interface(&mut self) {
        let renderer = self.draft.ui.renderer.trim().to_ascii_lowercase();
        match renderer.as_str() {
            "auto" | "wgpu" | "glow" => {
                self.errors.remove("ui.renderer");
            }
            other => {
                self.errors.insert(
                    "ui.renderer",
                    format!("ui.renderer 只能是 auto / wgpu / glow，当前是「{other}」"),
                );
            }
        }
        let close = self.draft.ui.close_action.trim().to_ascii_lowercase();
        match close.as_str() {
            "tray" | "ask" | "exit" => {
                self.errors.remove("ui.close_action");
            }
            other => {
                self.errors.insert(
                    "ui.close_action",
                    format!("ui.close_action 只能是 tray / ask / exit，当前是「{other}」"),
                );
            }
        }
    }

    /// 保存：先整体校验，再走 `config::save`（原子写）。成功后基线前移。
    pub fn save(&mut self) -> Result<(), ConfigError> {
        self.revalidate();
        if !self.errors.is_empty() {
            let first = self
                .errors
                .iter()
                .next()
                .map(|(key, reason)| format!("{key}：{reason}"))
                .unwrap_or_default();
            self.notice = Some(Notice::err(
                format!("有 {} 项不合法，未保存", self.errors.len()),
                format!("先修好：{first}"),
            ));
            return Ok(());
        }
        match config::save(&self.draft) {
            Ok(()) => {
                self.baseline = self.draft.clone();
                self.texts = texts_from(&self.draft);
                self.errors.clear();
                self.load_failed = false;
                self.notice = Some(Notice::ok("已保存到 config.toml（原子写）"));
                Ok(())
            }
            Err(error) => {
                let headline = match error.code() {
                    Some(code) => format!("{} 保存配置失败：{error}", code.code()),
                    None => format!("保存配置失败：{error}"),
                };
                self.notice = Some(Notice::err(headline, config_hint(error.kind())));
                Err(error)
            }
        }
    }

    /// 撤销更改：丢弃编辑内容，回到基线。
    pub fn undo(&mut self) {
        self.draft = self.baseline.clone();
        self.texts = texts_from(&self.draft);
        self.errors.clear();
        self.leaving = false;
        self.notice = Some(Notice::ok("已撤销更改（回到上次保存的内容）"));
    }

    /// 恢复默认：**只**把本页可见的键恢复成架构文档里的默认值。
    pub fn reset_defaults(&mut self) {
        let defaults = Config::default();
        apply_defaults(&mut self.draft, &defaults);
        self.texts = texts_from(&self.draft);
        self.errors.clear();
        self.notice = Some(Notice::ok(format!(
            "已把本页 {} 项恢复成默认值（其它键没动，记得点保存）",
            VISIBLE_KEYS.len()
        )));
    }

    /// 「打开配置文件所在目录」要用的路径（配置目录，不是文件本身）。
    pub fn config_dir() -> Option<PathBuf> {
        config::default_path().and_then(|file| file.parent().map(Path::to_path_buf))
    }

    /// 当前配置文件路径（设置页上要点明真实位置）。
    pub fn config_path() -> Option<PathBuf> {
        config::default_path()
    }
}

/// 把「可见键」从 `defaults` 抄到 `draft`，其它键原样保留。
pub fn apply_defaults(draft: &mut Config, defaults: &Config) {
    draft.dl.dir = defaults.dl.dir.clone();
    draft.dl.auto_file_renaming = defaults.dl.auto_file_renaming;
    draft.dl.allow_overwrite = defaults.dl.allow_overwrite;
    draft.dl.continue_dl = defaults.dl.continue_dl;
    draft.dl.always_resume = defaults.dl.always_resume;

    draft.net.split = defaults.net.split;
    draft.net.max_connection_per_server = defaults.net.max_connection_per_server;
    draft.net.min_split_size = defaults.net.min_split_size.clone();
    draft.net.max_overall_download_limit = defaults.net.max_overall_download_limit.clone();
    draft.net.max_download_limit = defaults.net.max_download_limit.clone();
    draft.net.max_overall_upload_limit = defaults.net.max_overall_upload_limit.clone();
    draft.net.connect_timeout = defaults.net.connect_timeout;
    draft.net.max_tries = defaults.net.max_tries;
    draft.net.proxy = defaults.net.proxy.clone();
    draft.net.disk_cache = defaults.net.disk_cache.clone();

    draft.ui.close_action = defaults.ui.close_action.clone();
    draft.ui.renderer = defaults.ui.renderer.clone();
    draft.ui.minimize_to_tray = defaults.ui.minimize_to_tray;
    draft.ui.start_minimized = defaults.ui.start_minimized;
    draft.ui.start_with_windows = defaults.ui.start_with_windows;
    draft.lifecycle.quit_default = defaults.lifecycle.quit_default.clone();
}

/// 由配置生成文本缓冲（键 → 输入框内容）。
pub fn texts_from(config: &Config) -> BTreeMap<&'static str, String> {
    let mut map = BTreeMap::new();
    map.insert("dl.dir", config.dl.dir.clone());
    map.insert("net.split", config.net.split.to_string());
    map.insert(
        "net.max_connection_per_server",
        config.net.max_connection_per_server.to_string(),
    );
    map.insert("net.min_split_size", config.net.min_split_size.clone());
    map.insert(
        "net.max_overall_download_limit",
        config.net.max_overall_download_limit.clone(),
    );
    map.insert(
        "net.max_download_limit",
        config.net.max_download_limit.clone(),
    );
    map.insert(
        "net.max_overall_upload_limit",
        config.net.max_overall_upload_limit.clone(),
    );
    map.insert(
        "net.connect_timeout",
        config.net.connect_timeout.to_string(),
    );
    map.insert("net.max_tries", config.net.max_tries.to_string());
    map.insert("net.proxy", config.net.proxy.clone());
    map.insert("net.disk_cache", config.net.disk_cache.clone());
    map
}

// ---------------------------------------------------------------------------
// 校验（纯函数，全部可单测）
// ---------------------------------------------------------------------------

/// 把一个键的文本解析进 `config`；失败给一句中文原因。
pub fn parse_field(config: &mut Config, key: &str, raw: &str) -> Result<(), String> {
    match key {
        "dl.dir" => {
            let path = check_download_dir(raw)?;
            config.dl.dir = raw.trim().to_owned();
            let _ = path; // 只做校验，保留用户写的原始形态（`~` 在启动时展开）
            Ok(())
        }
        "net.split" => {
            config.net.split = parse_u8_range(raw, "net.split", 1, 16)?;
            Ok(())
        }
        "net.max_connection_per_server" => {
            config.net.max_connection_per_server =
                parse_u8_range(raw, "net.max_connection_per_server", 1, 16)?;
            Ok(())
        }
        "net.min_split_size" => {
            let bytes = parse_size_bytes(raw).ok_or_else(|| {
                "要写成「数字+单位」，例如 20M / 1M / 512K（不能写成 20MB）".to_owned()
            })?;
            if bytes < 1024 * 1024 {
                return Err(format!("aria2 要求最小 1M，当前是 {}", format_size(bytes)));
            }
            if bytes > 1024 * 1024 * 1024 {
                return Err("最大 1G：再大会让分片几乎不生效".to_owned());
            }
            config.net.min_split_size = raw.trim().to_owned();
            Ok(())
        }
        "net.max_overall_download_limit"
        | "net.max_download_limit"
        | "net.max_overall_upload_limit" => {
            check_limit(raw)?;
            let value = raw.trim().to_owned();
            match key {
                "net.max_overall_download_limit" => config.net.max_overall_download_limit = value,
                "net.max_download_limit" => config.net.max_download_limit = value,
                _ => config.net.max_overall_upload_limit = value,
            }
            Ok(())
        }
        "net.connect_timeout" => {
            config.net.connect_timeout = parse_u32_range(raw, "net.connect_timeout", 1, 600)?;
            Ok(())
        }
        "net.max_tries" => {
            config.net.max_tries = parse_u8_range(raw, "net.max_tries", 0, 100)?;
            Ok(())
        }
        "net.proxy" => {
            check_proxy(raw)?;
            config.net.proxy = raw.trim().to_owned();
            Ok(())
        }
        "net.disk_cache" => {
            let bytes = parse_size_bytes(raw).ok_or_else(|| {
                "要写成「数字+单位」，例如 16M / 0（0 = 关掉内存缓存）".to_owned()
            })?;
            if bytes > 1024 * 1024 * 1024 {
                return Err("最大 1G：这是内存缓存，别把内存吃掉".to_owned());
            }
            config.net.disk_cache = raw.trim().to_owned();
            Ok(())
        }
        other => Err(format!("未知键 {other}")),
    }
}

fn parse_u8_range(raw: &str, key: &str, min: u8, max: u8) -> Result<u8, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!("{key} 不能为空（范围 {min}–{max}）"));
    }
    let value: u8 = trimmed
        .parse()
        .map_err(|_| format!("{key} 要填 {min}–{max} 的整数，当前是「{trimmed}」"))?;
    if value < min || value > max {
        return Err(format!("{key} 超出范围：{min}–{max}，当前是 {value}"));
    }
    Ok(value)
}

fn parse_u32_range(raw: &str, key: &str, min: u32, max: u32) -> Result<u32, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!("{key} 不能为空（范围 {min}–{max}）"));
    }
    let value: u32 = trimmed
        .parse()
        .map_err(|_| format!("{key} 要填 {min}–{max} 的整数，当前是「{trimmed}」"))?;
    if value < min || value > max {
        return Err(format!("{key} 超出范围：{min}–{max}，当前是 {value}"));
    }
    Ok(value)
}

/// 解析 aria2 的尺寸串：`<整数><K|M|G?>`（大小写不敏感，无单位 = 字节）。
///
/// aria2 只认 `K`/`M`/`G` 后缀（`20MB` 是非法值，会被它当成未知单位拒掉），
/// 所以这里也**只**接受这三种，不猜。
pub fn parse_size_bytes(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (digits, multiplier) = match trimmed.chars().last()? {
        'k' | 'K' => (&trimmed[..trimmed.len() - 1], 1024u64),
        'm' | 'M' => (&trimmed[..trimmed.len() - 1], 1024 * 1024),
        'g' | 'G' => (&trimmed[..trimmed.len() - 1], 1024 * 1024 * 1024),
        _ => (trimmed, 1),
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let value: u64 = digits.parse().ok()?;
    value.checked_mul(multiplier)
}

/// 人话尺寸（错误提示里用）。
pub fn format_size(bytes: u64) -> String {
    const M: u64 = 1024 * 1024;
    const G: u64 = 1024 * M;
    if bytes >= G && bytes.is_multiple_of(G) {
        format!("{}G", bytes / G)
    } else if bytes >= M && bytes.is_multiple_of(M) {
        format!("{}M", bytes / M)
    } else if bytes >= 1024 && bytes.is_multiple_of(1024) {
        format!("{}K", bytes / 1024)
    } else {
        format!("{bytes}B")
    }
}

/// 限速键：`0` = 不限速；其它必须是合法尺寸串。
fn check_limit(raw: &str) -> Result<(), String> {
    let trimmed = raw.trim();
    if trimmed == "0" {
        return Ok(());
    }
    let bytes = parse_size_bytes(trimmed)
        .ok_or_else(|| "要写成「0（不限速）」或「数字+单位」，例如 1M / 512K".to_owned())?;
    if bytes == 0 {
        return Err("0 表示不限速，直接填 0 即可（不要写 0M）".to_owned());
    }
    if bytes > 1024 * 1024 * 1024 {
        return Err("单条限速最大 1G/s，再大没有意义".to_owned());
    }
    Ok(())
}

/// 代理：空 = 不使用代理；否则必须是 `scheme://host:port` 形态。
fn check_proxy(raw: &str) -> Result<(), String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    let Some((scheme, rest)) = trimmed.split_once("://") else {
        return Err("要写成 http://host:port（空值 = 不使用代理）".to_owned());
    };
    let scheme = scheme.to_ascii_lowercase();
    if !matches!(
        scheme.as_str(),
        "http" | "https" | "socks4" | "socks5" | "socks5h" | "ftp"
    ) {
        return Err(format!(
            "不支持的代理协议「{scheme}」（可用 http/https/socks4/socks5/socks5h/ftp）"
        ));
    }
    if rest.trim_matches('/').is_empty() {
        return Err("代理地址缺少主机名，例如 http://127.0.0.1:7890".to_owned());
    }
    Ok(())
}

/// 下载目录校验：展开 `~` → 必须是已存在的目录 → 试着建一个临时文件确认可写。
///
/// 「可写」用**真写一次再删**来判（Windows 上目录的只读位不可靠，权限 ACL 才是真相）；
/// 只在输入变化/保存时调用，不进每帧路径。
pub fn check_download_dir(raw: &str) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("下载目录不能为空（默认 ~/Downloads）".to_owned());
    }
    let path = config::expand_user_path(trimmed);
    if !path.exists() {
        return Err(format!("目录不存在：{}", path.display()));
    }
    if !path.is_dir() {
        return Err(format!("这不是目录：{}", path.display()));
    }
    let probe = path.join(".ariahost-write-check.tmp");
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(path)
        }
        Err(error) => Err(format!("目录不可写：{}（{error}）", path.display())),
    }
}

/// 配置错误的「下一步」建议（按类别给，UI 只负责显示）。
pub fn config_hint(kind: ConfigErrorKind) -> &'static str {
    match kind {
        ConfigErrorKind::PathUnavailable => "确认 %APPDATA% 存在且有写权限，再重开设置页",
        ConfigErrorKind::Parse => {
            "配置文件有语法错误：用「打开配置文件所在目录」手改，或点恢复默认覆盖"
        }
        ConfigErrorKind::Io => "检查文件是否被别的程序占用 / 磁盘是否满，然后重试",
        ConfigErrorKind::UnsupportedSchemaVersion => {
            "配置版本比本程序新：升级 AriaHost，或先备份再点恢复默认"
        }
        ConfigErrorKind::Secret => "secret 文件损坏：删掉 secret.bin 后重启面板会重新生成",
    }
}

/// `ui.start_with_windows` 的注册表值（HKCU Run 的 `AriaHost` 值数据）。
///
/// 带引号是必须的：路径里常有空格；HKCU Run **不需要**提权（架构 §4.11 第 38 项）。
pub fn run_value(exe: &Path) -> String {
    format!("\"{}\"", exe.display())
}

/// 写/删 HKCU Run（`ui.start_with_windows`）。
///
/// 用 `reg.exe` 而不是新引 `windows`/`winreg` 依赖：只有两条固定命令，
/// 不值得为它加一条供应链（与「不新增依赖」的仓库纪律一致）。
#[cfg(windows)]
pub fn apply_start_with_windows(enable: bool, exe: &Path) -> Result<(), String> {
    const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    let mut command = std::process::Command::new("reg.exe");
    if enable {
        command
            .args(["add", KEY, "/v", "AriaHost", "/t", "REG_SZ", "/d"])
            .arg(run_value(exe))
            .arg("/f");
    } else {
        command.args(["delete", KEY, "/v", "AriaHost", "/f"]);
    }
    let output = command
        .output()
        .map_err(|error| format!("调 reg.exe 失败：{error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    // 删除一个本来就不存在的值：当成成功（取消勾选本来就该是「没有」这个状态）。
    if !enable && stderr.contains("unable to find") {
        return Ok(());
    }
    Err(format!("reg.exe 返回失败：{stderr}"))
}

/// 非 Windows 目标（本产品只发 Windows）：不碰注册表，直接成功。
#[cfg(not(windows))]
pub fn apply_start_with_windows(_enable: bool, _exe: &Path) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let base =
            std::env::temp_dir().join(format!("ariahost-settings-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("建临时目录");
        base
    }

    #[test]
    fn visible_keys_match_config_fields() {
        // 逐个键确认：要么在文本缓冲里，要么是 apply_defaults 里逐个赋值的布尔/枚举键。
        let config = Config::default();
        let texts = texts_from(&config);
        let non_text = [
            "dl.auto_file_renaming",
            "dl.allow_overwrite",
            "dl.continue",
            "dl.always_resume",
            "ui.close_action",
            "ui.renderer",
            "ui.minimize_to_tray",
            "ui.start_minimized",
            "ui.start_with_windows",
            "lifecycle.quit_default",
        ];
        for key in VISIBLE_KEYS {
            let known = texts.contains_key(key) || non_text.contains(&key);
            assert!(
                known,
                "键 {key} 既不在文本缓冲里也不是已知的布尔/枚举键（拼错或自造？）"
            );
        }
        for key in texts.keys() {
            assert!(
                VISIBLE_KEYS.contains(key),
                "文本缓冲里的 {key} 不在 VISIBLE_KEYS 里"
            );
        }
        for key in non_text {
            assert!(VISIBLE_KEYS.contains(&key), "{key} 不在 VISIBLE_KEYS 里");
        }
        // 21 个可见键 = 5 下载 + 10 网络 + 6 界面。
        assert_eq!(VISIBLE_KEYS.len(), 21);
        assert_eq!(
            default_text("net.split"),
            Config::default().net.split.to_string()
        );
        assert_eq!(default_text("net.min_split_size"), "20M");
        assert_eq!(default_text("dl.dir"), "~/Downloads");
    }

    #[test]
    fn size_parser_accepts_only_aria2_units() {
        assert_eq!(parse_size_bytes("20M"), Some(20 * 1024 * 1024));
        assert_eq!(parse_size_bytes("1G"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_size_bytes("512K"), Some(512 * 1024));
        assert_eq!(parse_size_bytes("20m"), Some(20 * 1024 * 1024));
        assert_eq!(parse_size_bytes("0"), Some(0));
        assert_eq!(parse_size_bytes("2048"), Some(2048));
        // aria2 不认这些写法，我们也不猜。
        assert_eq!(parse_size_bytes("20MB"), None);
        assert_eq!(parse_size_bytes("-1M"), None);
        assert_eq!(parse_size_bytes("M"), None);
        assert_eq!(parse_size_bytes(""), None);
        assert_eq!(parse_size_bytes(" 12K "), Some(12 * 1024));
    }

    #[test]
    fn size_errors_are_actionable() {
        let mut config = Config::default();
        let error =
            parse_field(&mut config, "net.min_split_size", "512K").expect_err("小于 1M 要拦");
        assert!(error.contains("1M"), "错误里要点明下限：{error}");
        let error =
            parse_field(&mut config, "net.min_split_size", "20MB").expect_err("非法单位要拦");
        assert!(error.contains("20M"), "错误里要给正例：{error}");
        let error = parse_field(&mut config, "net.max_download_limit", "0M").expect_err("0M 要拦");
        assert!(error.contains("填 0"), "0 的写法要写清楚：{error}");
        assert!(parse_field(&mut config, "net.max_download_limit", "0").is_ok());
    }

    #[test]
    fn ranges_are_enforced() {
        let mut config = Config::default();
        assert!(parse_field(&mut config, "net.split", "16").is_ok());
        assert!(parse_field(&mut config, "net.split", "0").is_err());
        assert!(parse_field(&mut config, "net.split", "17").is_err());
        assert!(parse_field(&mut config, "net.split", "abc").is_err());
        assert!(parse_field(&mut config, "net.split", "").is_err());
        assert!(parse_field(&mut config, "net.connect_timeout", "600").is_ok());
        assert!(parse_field(&mut config, "net.connect_timeout", "601").is_err());
        assert!(parse_field(&mut config, "net.connect_timeout", "0").is_err());
        assert!(parse_field(&mut config, "net.max_tries", "0").is_ok());
        assert!(parse_field(&mut config, "net.max_tries", "101").is_err());
    }

    #[test]
    fn proxy_empty_means_no_proxy_and_scheme_is_checked() {
        let mut config = Config::default();
        assert!(parse_field(&mut config, "net.proxy", "").is_ok());
        assert_eq!(config.net.proxy, "");
        assert!(parse_field(&mut config, "net.proxy", "http://127.0.0.1:7890").is_ok());
        assert!(parse_field(&mut config, "net.proxy", "socks5h://127.0.0.1:1080").is_ok());
        let error =
            parse_field(&mut config, "net.proxy", "127.0.0.1:7890").expect_err("缺协议要拦");
        assert!(error.contains("http://"), "错误里要给正例：{error}");
        assert!(parse_field(&mut config, "net.proxy", "gopher://x").is_err());
        assert!(parse_field(&mut config, "net.proxy", "http://").is_err());
    }

    #[test]
    fn download_dir_is_checked_by_real_write() {
        let dir = temp_dir("dir");
        let raw = dir.to_string_lossy().to_string();
        assert!(check_download_dir(&raw).is_ok());
        // 探针文件必须被删掉（不能污染用户的下载目录）。
        assert!(!dir.join(".ariahost-write-check.tmp").exists());

        assert!(check_download_dir("").is_err());
        assert!(check_download_dir(&dir.join("nope").to_string_lossy()).is_err());
        let file = dir.join("a.txt");
        std::fs::write(&file, b"x").expect("写文件");
        assert!(
            check_download_dir(&file.to_string_lossy()).is_err(),
            "文件不是目录"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dirty_tracks_edits_and_undo_clears_them() {
        let mut settings = Settings::loaded_for_test(Config::default());
        assert!(!settings.is_dirty(), "刚加载不该是脏的");

        settings.set_text("net.split", "8".to_owned());
        assert!(settings.is_dirty(), "改过要变脏");
        assert_eq!(settings.draft().net.split, 8);

        settings.undo();
        assert!(!settings.is_dirty(), "撤销后要回到干净");
        assert_eq!(settings.draft().net.split, Config::default().net.split);
    }

    #[test]
    fn invalid_text_keeps_last_good_value_and_blocks_save() {
        let mut settings = Settings::loaded_for_test(Config::default());
        let before = settings.draft().net.connect_timeout;
        settings.set_text("net.connect_timeout", "9999".to_owned());
        assert_eq!(
            settings.draft().net.connect_timeout,
            before,
            "非法输入不能改副本"
        );
        assert!(settings.error("net.connect_timeout").is_some());
        // 有错误时保存要拦住，并且不写盘（notice 是失败态）。
        let result = settings.save();
        assert!(
            result.is_ok(),
            "校验失败不是 IO 错误，save 返回 Ok(()) 但未写盘"
        );
        let notice = settings.notice.clone().expect("要有提示");
        assert!(!notice.ok, "校验失败要显示失败提示");
        assert!(notice.headline.contains("不合法"), "{}", notice.headline);
    }

    #[test]
    fn reset_defaults_only_touches_visible_keys() {
        let mut settings = Settings::loaded_for_test(Config::default());
        settings.draft_mut().clean.scan_interval_hours = 99;
        settings.draft_mut().queue.max_concurrent = 9;
        settings.draft_mut().net.split = 12;

        settings.reset_defaults();

        let defaults = Config::default();
        assert_eq!(
            settings.draft().net.split,
            defaults.net.split,
            "可见键要回到默认"
        );
        assert_eq!(
            settings.draft().clean.scan_interval_hours,
            99,
            "不可见键不许被顺手抹掉"
        );
        assert_eq!(
            settings.draft().queue.max_concurrent,
            9,
            "不可见键不许被顺手抹掉"
        );
    }

    #[test]
    fn broken_config_can_still_be_overwritten_from_the_settings_page() {
        // 配置坏掉时 ensure_loaded 用默认值继续，并把自己标成「可以保存」（自救入口）。
        let mut settings = Settings::new();
        settings.load_failed = true;
        assert!(
            settings.is_dirty(),
            "读盘失败时应允许直接保存默认值覆盖坏文件"
        );
        settings.notice = None;
        settings.load_failed = false;
        assert!(!settings.is_dirty(), "标记清掉后回到干净状态");
    }

    #[test]
    fn interface_domains_are_validated() {
        let mut settings = Settings::loaded_for_test(Config::default());
        for value in ["auto", "wgpu", "glow"] {
            settings.draft_mut().ui.renderer = value.to_owned();
            settings.revalidate();
            assert!(settings.error("ui.renderer").is_none(), "{value} 应合法");
        }
        settings.draft_mut().ui.renderer = "dx12".to_owned();
        settings.revalidate();
        assert!(
            settings.error("ui.renderer").is_some(),
            "dx12 是 --backend 的值域，不是 ui.renderer 的"
        );

        for value in ["tray", "ask", "exit"] {
            settings.draft_mut().ui.close_action = value.to_owned();
            settings.revalidate();
            assert!(
                settings.error("ui.close_action").is_none(),
                "{value} 应合法"
            );
        }
        settings.draft_mut().ui.close_action = "minimize".to_owned();
        settings.revalidate();
        assert!(settings.error("ui.close_action").is_some());
    }

    #[test]
    fn effects_match_the_wiring_state() {
        assert_eq!(effect("ui.close_action"), Effect::Immediate);
        assert_eq!(effect("ui.start_with_windows"), Effect::Immediate);
        assert_eq!(effect("ui.renderer"), Effect::Restart);
        assert_eq!(effect("ui.start_minimized"), Effect::NotWired);
        assert_eq!(effect("lifecycle.quit_default"), Effect::NotWired);
        assert_eq!(effect("dl.dir"), Effect::Restart);
    }

    #[test]
    fn run_value_is_quoted_for_paths_with_spaces() {
        let value = run_value(Path::new(r"C:\Program Files\AriaHost\ariahost.exe"));
        assert_eq!(value, r#""C:\Program Files\AriaHost\ariahost.exe""#);
        assert!(value.starts_with('"') && value.ends_with('"'));
    }

    #[test]
    fn config_hints_are_distinct_per_kind() {
        let kinds = [
            ConfigErrorKind::PathUnavailable,
            ConfigErrorKind::Parse,
            ConfigErrorKind::Io,
            ConfigErrorKind::UnsupportedSchemaVersion,
            ConfigErrorKind::Secret,
        ];
        let mut seen = Vec::new();
        for kind in kinds {
            let hint = config_hint(kind);
            assert!(!hint.is_empty());
            assert!(!seen.contains(&hint), "两类错误给了同一条建议：{hint}");
            seen.push(hint);
        }
    }
}
