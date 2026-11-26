//! 字体接入（EG-5 / EG-6 / UI-5）：三条独立链 + **真粗体**。
//!
//! ## 一、为什么「按族名选字体」必须靠注入，而不是靠提问
//!
//! `egui_system_font_provider` 的 `font_for` 把 `FontFamily::Name(_)` 直接映射成
//! `GenericFamily::SansSerif`，再叠加脚本回退（`egui_system_font_provider-0.37.0/src/lib.rs:115`），
//! **命名族本身不参与选择**。所以「按 Inter / 思源黑体的族名去问」拿到的一定是同一个答案——
//! 名字是我们自己传进去的，属于**回声式假证据**（2026-11-26 实测纠正）。
//!
//! 想让某一族真的赢，只有一条路：**自己把字体文件读进来**，用
//! [`egui::FontInsert`] + [`egui::FontPriority::Highest`] 注册到目标族上。
//! 本文件就是干这件事的：
//!
//! - [`LATIN_CHAIN`] / [`HAN_CHAIN`] / [`MONO_CHAIN`] 是**声明顺序**（设计意图 + 排障依据）；
//! - 链上第一档有**可读字体文件**时走注入（真接线）；
//! - 没有文件时退回系统 provider（脚本回退），此时日志/info 区如实写「系统」，不冒充注入成功。
//!
//! ## 二、三条链互不干扰（UI-5 §2.1）
//!
//! 拉丁、汉字、等宽各自一条链：拉丁走 Inter（无则 Segoe UI），汉字走思源黑体
//! （无则微软雅黑 UI），等宽走 Consolas。混成一条链的旧写法会让「拉丁字形变了」
//! 和「汉字字形变了」两件事互相掩盖。
//!
//! ## 三、真粗体
//!
//! 粗体不是把字号调大、也不是 `strong()`（那只是加粗**假象**）。本文件把真实的粗体面
//! （拉丁粗 + 汉字粗）注册进命名族 [`BOLD_FAMILY`]，`theme::bold()` 直接用这一族。
//! 注入失败时该族为空，渲染退回系统回退链——`--panel-selftest` 的 `FONTS ... |bold=no`
//! 会把这件事如实报出来。
//!
//! ## 四、内存红线（EG-4）
//!
//! 所有注入都过 [`INJECT_BUDGET`] 单文件预算：汉字面动辄 10–17MB，两个大面同时注入就会
//! 吃掉 <100MB 的余量。超预算的面**不注入**并记一条日志，退回系统 provider。

use std::path::PathBuf;
use std::sync::Arc;

use egui::{
    FallbackRequest, FontData, FontFamily, FontInsert, FontPriority, FontProvider as _,
    InsertFontFamily,
};
use egui_system_font_provider::SystemFontProvider;

use crate::logbuf::LogBuffer;

/// 字体回退 locale：简体中文（EG-5 指定值）。
pub const LOCALE: &str = "zh-CN";

/// 真粗体用的命名族名（`theme::bold` 是唯一消费者）。
pub const BOLD_FAMILY: &str = "AriaBold";

/// 汉字探测字：会触发 Han 脚本的系统字体回退查询。
const HAN_CLUSTER: &str = "直";

/// 拉丁探测字：用来分辨「这一次给的是拉丁字形还是汉字字形」。
const LATIN_CLUSTER: &str = "A";

/// 单个字体文件的注入预算。
///
/// 为什么是 17MB：实测汉字粗体面（`msyhbd.ttc` 15.7MB / 思源黑体 Bold≈10MB）都在这个量级，
/// 而稳态余量只有约 21MB。超预算的面不注入——宁可少一层优先级，也不破 EG-4 闸门。
pub const INJECT_BUDGET: u64 = 17 * 1024 * 1024;

/// 拉丁链（声明顺序，UI-5 §2.1）：`Inter` 打头，系统 `Segoe UI` 兜底。
pub const LATIN_CHAIN: [&str; 2] = ["Inter", "Segoe UI"];

/// 汉字链（声明顺序）：`Source Han Sans SC`（思源黑体）打头，`Microsoft YaHei UI` 兜底。
///
/// 为什么不写 `Noto Sans CJK SC`：思源黑体与 Noto Sans CJK 是同一套字形的两个发行名，
/// 但 Noto 常是用户级安装、换机就没了；写成同一档会让「到底用的哪套」说不清。
pub const HAN_CHAIN: [&str; 2] = ["Source Han Sans SC", "Microsoft YaHei UI"];

/// 等宽链（Lead 裁决）：`Consolas` 打头。
///
/// `Cascadia Mono` 不进链：本机注册表 0 项、字体目录无文件，装了 Windows Terminal
/// 也不带它——写进必需链等于在多数机器上白探测一次。
pub const MONO_CHAIN: [&str; 2] = ["Consolas", "Courier New"];

/// 一个「可能有文件」的字体面。
#[derive(Debug, Clone, Copy)]
pub struct Face {
    /// 所属族（写进日志用的声明名）。
    pub family: &'static str,
    /// 文件名（在字体目录里找）。
    pub file: &'static str,
    /// TTC 内的 face 下标（非 TTC 一律 0）。
    pub index: u32,
}

/// 拉丁常规面候选（链上第一档 `Inter` 的现实文件名）。
const LATIN_REGULAR: [Face; 3] = [
    Face {
        family: "Inter",
        file: "Inter-Regular.ttf",
        index: 0,
    },
    Face {
        family: "Inter",
        file: "Inter.ttf",
        index: 0,
    },
    Face {
        family: "Inter",
        file: "InterVariable.ttf",
        index: 0,
    },
];

/// 拉丁粗体面候选。
///
/// `segoeuib.ttf`（Segoe UI Bold）放在最后：它**几乎总在** `C:\Windows\Fonts`，
/// 于是「拉丁真粗体」在裸 Windows 上也成立，不依赖用户装 Inter。
const LATIN_BOLD: [Face; 3] = [
    Face {
        family: "Inter",
        file: "Inter-Bold.ttf",
        index: 0,
    },
    Face {
        family: "Inter",
        file: "Inter-SemiBold.ttf",
        index: 0,
    },
    Face {
        family: "Segoe UI",
        file: "segoeuib.ttf",
        index: 0,
    },
];

/// 汉字常规面候选（思源黑体的常见发行名）。
const HAN_REGULAR: [Face; 4] = [
    Face {
        family: "Source Han Sans SC",
        file: "SourceHanSansSC-Regular.otf",
        index: 0,
    },
    Face {
        family: "Source Han Sans SC",
        file: "SourceHanSansSC-Normal.otf",
        index: 0,
    },
    Face {
        family: "Source Han Sans SC",
        file: "NotoSansSC-Regular.otf",
        index: 0,
    },
    Face {
        family: "Source Han Sans SC",
        file: "NotoSansCJKsc-Regular.otf",
        index: 0,
    },
];

/// 汉字粗体面候选。
///
/// `msyhbd.ttc` 里两个 face 都是**粗体**（雅黑 Bold / 雅黑 UI Bold），索引 0 取第一个，
/// 视觉上都是粗体，取错也不会退化成常规字重——这是本表唯一允许「索引不确定」的项。
/// `Dengb.ttf`（等线 Bold）是单面文件，作第二档。
const HAN_BOLD: [Face; 4] = [
    Face {
        family: "Microsoft YaHei",
        file: "msyhbd.ttc",
        index: 0,
    },
    Face {
        family: "DengXian",
        file: "Dengb.ttf",
        index: 0,
    },
    Face {
        family: "Source Han Sans SC",
        file: "SourceHanSansSC-Bold.otf",
        index: 0,
    },
    Face {
        family: "Source Han Sans SC",
        file: "NotoSansSC-Bold.otf",
        index: 0,
    },
];

/// 系统**实际**用哪一族供应这三类字形。
///
/// 全部 `None` = 查询没给出结果（非 Windows 目标，或 provider 未就绪），
/// 展示层必须写 `miss`；**不要**用它回填请求过的族名。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Supply {
    /// 拉丁探测字（`A`）落到的字体。
    pub latin: Option<String>,
    /// 汉字探测字（`直`）落到的字体。
    pub han: Option<String>,
    /// `Monospace` 泛族落到的字体（「等宽用的是谁」）。
    pub mono: Option<String>,
}

/// 一次「字体面注入」的记账（`fonts_line()` 里 `inject=` 的唯一数据源）。
///
/// 为什么要记字节数：只报「注入了几面」读不出「真接线还是空壳」，也读不出
/// 「预算吃了多少」。用户报「粗体没生效」时，第一句要能问的是
/// 「哪个面真读进来了、多少字节」——那才是可核对的机器事实。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InjectRecord {
    /// 角色：`拉丁` / `汉字` / `拉丁粗` / `汉字粗`。
    pub role: &'static str,
    /// 字体文件里登记的内部族名（**不是**我们请求的名字）。
    pub family: String,
    /// 真正读进来的字体文件名。
    pub file: String,
    /// 实际读入字节数。
    pub bytes: u64,
}

/// 字体注册结果，面板信息区显示「字体是否来自系统」「实际供应族」「注入成功哪些面」。
#[derive(Debug, Clone, Default)]
pub struct FontState {
    /// 系统字体解析结果（形如 `system:Microsoft YaHei:400:0`），`None` 表示没找到。
    pub resolved: Option<String>,
    /// 三类字形的实际供应族。
    pub supply: Supply,
    /// 注入成功的面（按注入顺序）。
    pub injected: Vec<InjectRecord>,
    /// 注入被跳过/失败的原因（`角色:原因`），一条一因，不许吞。
    pub inject_skipped: Vec<String>,
    /// 本次注入读入的总字节数（`budget=` 的分子）。
    pub inject_used: u64,
    /// 真粗体族是否真的可用（拉丁粗或汉字粗至少一面注入成功）。
    pub bold_ready: bool,
}

impl FontState {
    /// 一行摘要，日志 / 信息区 / 诊断包共用同一份措辞。
    ///
    /// 空结果写「未探测」——那是「我们没问到」，**不等于**「这台机器没有中文字体」。
    pub fn supply_summary(&self) -> String {
        if self.supply == Supply::default() {
            return "（未探测：本目标没有系统字体枚举）".to_owned();
        }
        format!(
            "实际供应 {}{}",
            supply_line(&self.supply).trim_start_matches("："),
            ""
        )
    }

    /// `--panel-selftest` 用的机器可读字体事实（判据：`fonts=latin=…|han=…`）。
    ///
    /// 口径——四个「谁给的」必须能分开读，不许混成一个名字：
    /// - `latin=`：拉丁探测字 `A` 的**实际**供应族（系统 provider 返回的名字）；
    /// - `han=`：汉字探测字 `直` 的**实际**供应族，也就是「中文用的是谁」；
    /// - `mono=`：`Monospace` 泛族的落点（「等宽用的是谁」）；
    /// - `resolved=`：预热查询的整串（`system:` 前缀 = 系统 provider 给的），
    ///   与 selftest 顶层的 `fb=` **同值**——保留两处只为让 `fonts=` 单段可解析；
    /// - `bold=`：真粗体族（`AriaBold`）是否可用（`yes`/`no`，省略会让判据没法读）；
    /// - `inject=`：注入成功的面，`族名:字节` 多条以 `;` 分隔，无则 `miss`；
    /// - `budget=`：`已注入字节/单文件预算上限`（字节，EG-4 口径）；
    /// - `inject_skip=`：跳过/失败原因（`角色:原因`，多条 `;` 分隔），无则 `none`。
    ///
    /// 值里绝不回填我们请求的声明名；未命中一律写 `miss`。
    pub fn fonts_line(&self) -> String {
        let show = |value: &Option<String>| match value {
            Some(name) => name.clone(),
            None => "miss".to_owned(),
        };
        let inject = if self.injected.is_empty() {
            "miss".to_owned()
        } else {
            self.injected
                .iter()
                .map(|record| format!("{}:{}", record.family, record.bytes))
                .collect::<Vec<_>>()
                .join(";")
        };
        let inject_skip = if self.inject_skipped.is_empty() {
            "none".to_owned()
        } else {
            self.inject_skipped.join(";")
        };
        format!(
            "latin={}|han={}|mono={}|resolved={}|bold={}|inject={}|budget={}/{}|inject_skip={}",
            show(&self.supply.latin),
            show(&self.supply.han),
            show(&self.supply.mono),
            show(&self.resolved),
            if self.bold_ready { "yes" } else { "no" },
            inject,
            self.inject_used,
            INJECT_BUDGET,
            inject_skip
        )
    }

    /// 真正给出拉丁字形的族（系统返回的名字，不是我们请求的名字）。
    pub fn latin_supplier(&self) -> Option<&str> {
        self.supply.latin.as_deref()
    }

    /// 真正给出汉字字形的族（系统返回的名字，不是我们请求的名字）。
    pub fn han_supplier(&self) -> Option<&str> {
        self.supply.han.as_deref()
    }

    /// 中文是否确实由系统字体提供（EG-5 的可见证据）。
    pub fn is_system_font(&self) -> bool {
        self.resolved
            .as_deref()
            .is_some_and(|name| name.starts_with("system:"))
    }
}

/// 首帧前注册字体：注入链上有文件的面、注册系统 provider、并做一次预热查询。
///
/// 为什么预热（EG-6）：`SystemFontProvider::new()` 在后台线程枚举系统字体（可能数百毫秒），
/// 未完成时的查询会**阻塞**。提前查一次能让枚举在首帧前结束，避免第一帧卡顿。
pub fn install(ctx: &egui::Context, log: &LogBuffer) -> FontState {
    // 必须包 `Arc`：`Context::add_font_provider` 收的是 `Arc<dyn FontProvider>`，
    // 而探测要在注册前对**同一个实例**做完（预热），否则会二次枚举系统字体。
    let provider = Arc::new(SystemFontProvider::new().with_locale(Some(LOCALE)));
    let supply = probe_supply(&provider);
    log.push(format!("字体实际供应{}", supply_line(&supply)));

    // 1) 常规面：拉丁 + 汉字（链上第一个**有文件**的档注入 Highest，真接线）。
    let mut injected: Vec<InjectRecord> = Vec::new();
    let mut inject_skipped: Vec<String> = Vec::new();
    inject_first(
        ctx,
        log,
        &mut injected,
        &mut inject_skipped,
        "拉丁",
        &LATIN_REGULAR,
        &FontFamily::Proportional,
    );
    inject_first(
        ctx,
        log,
        &mut injected,
        &mut inject_skipped,
        "汉字",
        &HAN_REGULAR,
        &FontFamily::Proportional,
    );

    // 2) 真粗体：注册进命名族 `AriaBold`，`theme::bold()` 直接用。
    let bold_family = FontFamily::Name(Arc::from(BOLD_FAMILY));
    let latin_bold = inject_first(
        ctx,
        log,
        &mut injected,
        &mut inject_skipped,
        "拉丁粗",
        &LATIN_BOLD,
        &bold_family,
    );
    let han_bold = inject_first(
        ctx,
        log,
        &mut injected,
        &mut inject_skipped,
        "汉字粗",
        &HAN_BOLD,
        &bold_family,
    );
    let bold_ready = latin_bold || han_bold;
    // 预算分子只算**真读进来**的字节（跳过的不算），所以它和 `inject=` 的明细自洽。
    let inject_used: u64 = injected.iter().map(|record| record.bytes).sum();

    // 3) 链的落点：注入命中就写「注入」，没文件就写「系统 provider」——两件事不许混为一谈。
    // 只报**真的做过的事**。正文面从不做文件注入（走系统 provider），所以这里
    // 不能写链上的目标族名：真机实测这行曾写「拉丁 Inter（文件注入）」，而同一份
    // 日志的上一行是「拉丁面未注入：链上候选 Inter-Regular.ttf / Inter.ttf 都不存在」，
    // 两行结论互相矛盾，属于假事实。实际命中的族由 SELFTEST 的 `fonts=` 字段给出。
    log.push(format!(
        "字体注入结果：拉丁粗 {}；汉字粗 {}；等宽与正文面均走系统 provider（目标链：拉丁={} / 汉字={} / 等宽={}；实际命中的族见 SELFTEST 的 fonts=）",
        hit_word(latin_bold),
        hit_word(han_bold),
        LATIN_CHAIN[0],
        HAN_CHAIN[0],
        MONO_CHAIN[0],
    ));
    if !bold_ready {
        log.push(
            "真粗体不可用：链上的粗体面都没读到；主题的 bold 会退回正文族（界面不会假装加粗）"
                .to_owned(),
        );
    }

    // 4) 兜底：连汉字都没问到，也要给面板一个「默认族」的答案（与 eframe 常规路径一致）。
    let request = FallbackRequest {
        cluster: HAN_CLUSTER,
        family: &FontFamily::Proportional,
    };
    let resolved = supply
        .han
        .clone()
        .or_else(|| provider.font_for(&request).map(|insert| insert.name));
    match &resolved {
        Some(name) => log.push(format!(
            "系统字体已接入（locale={LOCALE}）：「{HAN_CLUSTER}」→ {name}"
        )),
        None => log.push(format!(
            "系统字体未解析到「{HAN_CLUSTER}」（locale={LOCALE}）：中文可能显示为方块（EG-5）"
        )),
    }
    // 注册的 provider 必须是同一个实例：预热已经让它把字体枚举跑完，避免二次枚举。
    ctx.add_font_provider(provider);

    let state = FontState {
        resolved,
        supply,
        injected,
        inject_skipped,
        inject_used,
        bold_ready,
    };
    // 落点只经由 `latin_supplier`/`han_supplier` 这两条**同一个判据**输出：日志与面板
    // 因此不可能各说一套（别在这里再写一遍 `map(...)`）。
    log.push(format!(
        "字体落点：拉丁→{}；汉字→{}",
        state.latin_supplier().unwrap_or("（未解析）"),
        state.han_supplier().unwrap_or("（未解析）"),
    ));
    // 注入账：字节数与跳过原因都落盘——用户报「粗体没生效 / 中文发虚」时，
    // 第一句能照抄的就是这一行（与 `--panel-selftest` 的 `inject=` / `inject_skip=` 同源）。
    log.push(format!(
        "字体注入账：成功 {} 面 / {} B（单文件预算 {} MB）；跳过/失败 {} 条{}",
        state.injected.len(),
        state.inject_used,
        INJECT_BUDGET / (1024 * 1024),
        state.inject_skipped.len(),
        if state.inject_skipped.is_empty() {
            String::new()
        } else {
            format!("（{}）", state.inject_skipped.join("；"))
        },
    ));
    state
}

/// 注入词：命中了就说注入，没命中就说系统 provider。
const fn hit_word(injected: bool) -> &'static str {
    if injected {
        "文件注入"
    } else {
        "系统 provider"
    }
}

/// 按表逐个找文件，**第一个存在的**读进来注入目标族；返回是否注入成功。
///
/// 失败不是错误：没有文件就交给系统 provider（脚本回退），但要在日志里说清是哪一档，
/// 并把原因记进 `skipped`——`--panel-selftest` 的 `inject_skip=` 就是这份账，
/// 它让「粗体没生效」这类报障不必靠猜（是没文件、还是超了 EG-4 预算）。
fn inject_first(
    ctx: &egui::Context,
    log: &LogBuffer,
    injected: &mut Vec<InjectRecord>,
    skipped: &mut Vec<String>,
    role: &'static str,
    faces: &[Face],
    into: &FontFamily,
) -> bool {
    for face in faces {
        let Some(path) = find_font_file(face.file) else {
            continue;
        };
        match std::fs::metadata(&path) {
            Ok(meta) if meta.len() > INJECT_BUDGET => {
                log.push(format!(
                    "{role}面超预算不注入：{} 有 {} MB，超过单文件预算 {} MB（EG-4）",
                    face.file,
                    meta.len() / (1024 * 1024),
                    INJECT_BUDGET / (1024 * 1024),
                ));
                skipped.push(format!(
                    "{role}:超预算({}MB>{}MB)",
                    meta.len() / (1024 * 1024),
                    INJECT_BUDGET / (1024 * 1024),
                ));
                continue;
            }
            Ok(_) => {}
            Err(error) => {
                log.push(format!("{role}面读取失败：{}（{error}）", path.display()));
                skipped.push(format!("{role}:元数据失败({error})"));
                continue;
            }
        }
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                log.push(format!("{role}面读取失败：{}（{error}）", path.display()));
                skipped.push(format!("{role}:读取失败({error})"));
                continue;
            }
        };
        let len = bytes.len() as u64;
        let mut data = FontData::from_owned(bytes);
        data.index = face.index;
        // 名字只在本进程内当索引用（egui 会把字体按名字登记），带上字重角色便于日志辨认。
        let insert_name = format!("{role}-{}", face.family);
        ctx.add_font(FontInsert::new(
            &insert_name,
            data,
            vec![InsertFontFamily {
                family: into.clone(),
                priority: FontPriority::Highest,
            }],
        ));
        injected.push(InjectRecord {
            role,
            family: face.family.to_owned(),
            file: face.file.to_owned(),
            bytes: len,
        });
        log.push(format!(
            "{role}面已注入（真接线）：{} ← {}（face {}，{len} B）",
            face.file, face.family, face.index
        ));
        return true;
    }
    skipped.push(format!(
        "{role}:候选文件都不存在({})",
        faces
            .iter()
            .map(|face| face.file)
            .collect::<Vec<_>>()
            .join("/")
    ));
    log.push(format!(
        "{role}面未注入：链上候选 {} 在本机字体目录都不存在",
        faces
            .iter()
            .map(|face| face.file)
            .collect::<Vec<_>>()
            .join(" / ")
    ));
    false
}

/// 在「用户级字体目录」与「系统字体目录」里找文件（Windows 两处都可能有）。
fn find_font_file(file: &str) -> Option<PathBuf> {
    for dir in font_dirs() {
        let candidate = dir.join(file);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// 字体目录候选（用户安装的字体在 `%LOCALAPPDATA%`，系统字体在 `%WINDIR%\Fonts`）。
fn font_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(
            PathBuf::from(local)
                .join("Microsoft")
                .join("Windows")
                .join("Fonts"),
        );
    }
    if let Some(windir) = std::env::var_os("WINDIR") {
        dirs.push(PathBuf::from(windir).join("Fonts"));
    }
    dirs
}

/// 日志用的单行摘要（拉丁 / 汉字 / 等宽，未命中写 `miss`）。
fn supply_line(supply: &Supply) -> String {
    let show = |value: &Option<String>| match value {
        Some(name) => name.clone(),
        None => "miss".to_owned(),
    };
    format!(
        "：拉丁={} 汉字={} 等宽={}",
        show(&supply.latin),
        show(&supply.han),
        show(&supply.mono)
    )
}

/// 问系统「这三类字形实际用谁」（纯探测，不改任何全局状态）。
///
/// 只问**泛族**（`Proportional` / `Monospace`）——这正是渲染时 egui 真正会传的族；
/// 用 `FontFamily::Name(_)` 问出来的是同一个答案，问三遍只会制造「逐族命中」的错觉。
fn probe_supply(provider: &SystemFontProvider) -> Supply {
    let ask = |cluster: &'static str, family: &FontFamily| {
        provider
            .font_for(&FallbackRequest { cluster, family })
            .map(|insert| insert.name)
    };
    Supply {
        latin: ask(LATIN_CLUSTER, &FontFamily::Proportional),
        han: ask(HAN_CLUSTER, &FontFamily::Proportional),
        mono: ask(LATIN_CLUSTER, &FontFamily::Monospace),
    }
}

/// 探测辅助：给定字体目录（测试用，避免依赖真实系统目录）。
#[cfg(test)]
fn find_in(dirs: &[PathBuf], file: &str) -> Option<PathBuf> {
    dirs.iter()
        .map(|dir| dir.join(file))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_is_simplified_chinese() {
        assert_eq!(LOCALE, "zh-CN");
    }

    #[test]
    fn system_font_detection_requires_system_prefix() {
        let state = FontState {
            resolved: Some("system:Microsoft YaHei:0:0".to_owned()),
            ..FontState::default()
        };
        assert!(state.is_system_font());
        assert!(!FontState::default().is_system_font());
        assert!(!FontState {
            resolved: Some("内嵌字体".to_owned()),
            ..FontState::default()
        }
        .is_system_font());
    }

    #[test]
    fn three_chains_are_declared_separately() {
        // UI-5 §2.1：三条链各自成立，不许混成一条。
        assert_eq!(LATIN_CHAIN[0], "Inter");
        assert_eq!(HAN_CHAIN[0], "Source Han Sans SC");
        assert_eq!(MONO_CHAIN[0], "Consolas");
        // 思源黑体与 Noto CJK 是同一套字形的两个发行名，不进同一条链的两档。
        assert!(!HAN_CHAIN.contains(&"Noto Sans CJK SC"));
        // 进链的每一档都必须有文件候选表，否则那一档永远只能靠 provider（等于白写）。
        for family in LATIN_CHAIN {
            assert!(
                LATIN_REGULAR.iter().any(|face| face.family == family)
                    || LATIN_BOLD.iter().any(|face| face.family == family)
                    || family == "Segoe UI",
                "拉丁链的 {family} 既无注入表、也不是系统兜底档"
            );
        }
        for family in HAN_CHAIN {
            assert!(
                HAN_REGULAR.iter().any(|face| face.family == family)
                    || HAN_BOLD.iter().any(|face| face.family == family)
                    || family == "Microsoft YaHei UI",
                "汉字链的 {family} 既无注入表、也不是系统兜底档"
            );
        }
    }

    #[test]
    fn bold_faces_exist_for_both_scripts() {
        // 真粗体要两个脚本都能落地：拉丁粗（裸 Windows 也有 segoeuib.ttf）、汉字粗。
        assert!(LATIN_BOLD.iter().any(|face| face.file == "segoeuib.ttf"));
        assert!(HAN_BOLD.iter().any(|face| face.file == "msyhbd.ttc"));
        // 汉字粗体的候选必须有**单面**文件兜底，不能全靠 TTC 索引猜测。
        assert!(HAN_BOLD.iter().any(|face| face.file.ends_with(".ttf")));
        assert!(BOLD_FAMILY == "AriaBold");
    }

    #[test]
    fn inject_budget_stays_under_the_memory_headroom() {
        // 稳态余量约 21MB（实测 81MB / 闸门 100MB），单文件预算必须留出余地。
        // 写成 `const` 块：这两条是编译期常量，clippy 的 `assertions_on_constants`
        // 会把运行时 `assert!` 判为死断言（`-D warnings` 下会挂 CI）。
        const _: () = assert!(INJECT_BUDGET <= 18 * 1024 * 1024);
        const _: () = assert!(INJECT_BUDGET >= 10 * 1024 * 1024);
    }

    #[test]
    fn find_in_prefers_the_first_directory_that_has_the_file() {
        let root = std::env::temp_dir().join("ah-fonts-probe");
        let user = root.join("user");
        let system = root.join("system");
        std::fs::create_dir_all(&user).expect("建临时目录");
        std::fs::create_dir_all(&system).expect("建临时目录");
        std::fs::write(user.join("a.ttf"), b"x").expect("写临时字体");
        std::fs::write(system.join("b.ttf"), b"y").expect("写临时字体");
        let dirs = vec![user.clone(), system.clone()];
        assert_eq!(find_in(&dirs, "a.ttf"), Some(user.join("a.ttf")));
        assert_eq!(find_in(&dirs, "b.ttf"), Some(system.join("b.ttf")));
        assert_eq!(find_in(&dirs, "missing.ttf"), None);
        // 第一个目录有同名文件时不被第二个目录抢走（顺序即优先级）。
        std::fs::write(system.join("a.ttf"), b"z").expect("写临时字体");
        assert_eq!(find_in(&dirs, "a.ttf"), Some(user.join("a.ttf")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn summary_never_echoes_the_requested_family() {
        // 这是本文件最重要的一条回归：曾经 `han` 显示成「Segoe UI」，
        // 而 Segoe UI 根本不含汉字——名字来自我们自己的提问，不是系统事实。
        let state = FontState {
            resolved: Some("system:Microsoft YaHei:400:0".to_owned()),
            supply: Supply {
                latin: Some("system:Segoe UI:400:0".to_owned()),
                han: Some("system:Microsoft YaHei:400:0".to_owned()),
                mono: None,
            },
            inject_used: 0,
            ..FontState::default()
        };
        let text = state.supply_summary();
        assert!(text.contains("汉字=system:Microsoft YaHei:400:0"), "{text}");
        // 汉字那一段绝不能是我们请求过的族名。
        assert!(!text.contains("汉字=Segoe UI"), "{text}");
        // 未命中的档位必须显式写 miss，而不是留空。
        assert!(text.contains("等宽=miss"), "{text}");
        assert_eq!(state.han_supplier(), Some("system:Microsoft YaHei:400:0"));
        assert_eq!(state.latin_supplier(), Some("system:Segoe UI:400:0"));
    }

    #[test]
    fn fonts_line_is_parseable_and_reports_bold_honestly() {
        let state = FontState {
            resolved: None,
            supply: Supply {
                latin: Some("system:Segoe UI:400:0".to_owned()),
                han: Some("system:Microsoft YaHei:400:0".to_owned()),
                mono: None,
            },
            injected: vec![InjectRecord {
                role: "拉丁粗",
                family: "Segoe UI".to_owned(),
                file: "segoeuib.ttf".to_owned(),
                bytes: 947_092,
            }],
            inject_skipped: vec!["汉字:Word 装的是 .ttc，按设计跳过".to_owned()],
            inject_used: 947_092,
            bold_ready: true,
        };
        let line = state.fonts_line();
        // 这里断言的是**未脱敏**的原始行（族名里的空格保留）；
        // `--panel-selftest` 打印前还会过一次 `sanitize`（空白→下划线），
        // 所以判据里看到的是 `system:Segoe_UI:400:0`，两者不要混。
        assert!(line.starts_with("latin=system:Segoe UI"), "{line}");
        assert!(line.contains("|han=system:Microsoft YaHei"), "{line}");
        assert!(line.contains("|mono=miss"), "{line}");
        assert!(line.contains("|bold=yes"), "{line}");
        // 注入必须同时给出「哪个族、多少字节」，只报面数读不出真接线。
        assert!(line.contains("|inject=Segoe UI:947092"), "{line}");
        assert!(line.contains("|budget=947092/"), "{line}");
        assert!(line.contains("|inject_skip=汉字:"), "{line}");
        // 无粗体时必须写 no，不许省略这个字段（省略会让判据没法读）。
        assert!(FontState::default().fonts_line().contains("|bold=no"));
        assert!(FontState::default().fonts_line().contains("|inject=miss"));
        assert!(FontState::default()
            .fonts_line()
            .contains("|inject_skip=none"));
    }

    #[test]
    fn empty_supply_says_not_probed() {
        assert!(FontState::default().supply_summary().contains("未探测"));
        assert_eq!(FontState::default().han_supplier(), None);
    }

    #[test]
    fn injected_path_is_only_used_when_the_file_exists() {
        // 注入表的每一项都必须长得像文件名（防止有人把整条路径写进表里，
        // 那样 `font_dirs()` 拼接会永远找不到）。
        for face in LATIN_REGULAR
            .iter()
            .chain(LATIN_BOLD.iter())
            .chain(HAN_REGULAR.iter())
            .chain(HAN_BOLD.iter())
        {
            assert!(
                !face.file.contains('\\') && !face.file.contains('/'),
                "{} 不是纯文件名",
                face.file
            );
            assert!(
                std::path::Path::new(face.file).extension().is_some(),
                "{}",
                face.file
            );
        }
    }
}
