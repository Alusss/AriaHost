//! 汉字字形图集容量探针（task-106 / UI-8）。
//!
//! ## 为什么要有这个入口
//!
//! `fonts.rs` 里原有的 `#[ignore]` 单测只能在**开发机** `cargo test` 跑；用户机器上的产物
//! 跑不了。而「图集够不够」的答案取决于**这台机器的字体链**（思源 / 雅黑 / 黑体的字形宽度
//! 与覆盖率都不同）与 **pixels_per_point**，所以必须在真机产物上能量。
//!
//! 本入口**不开窗、不要 wgpu**：`Context::run_ui` 负责排版，`Context::tessellate` 触发
//! 光栅化。因此 CI（ubuntu）与无显卡机器都能跑，结果只与字体与图集有关。
//!
//! ## 口径（写死，换机器才会变）
//!
//! - 字号取自产品真实阶梯（[`product_ladder`] → `theme::text_styles()` 的 Heading / AriaSubtitle
//!   / Body / Small 四档 = 20/16/14/12）。**不许再硬编码档位**：两版历史探针分别硬编码
//!   20/14/12 与 14/16/20，集合互不相同，排出的临界点都不对应产品负载（task-107）。
//!   旧文档写「4 档语义字号」并把 12pt 也算了进去，与实际排版不一致——**本探针只认自己
//!   打印的 `sizes=`**，不再引用那张旧表。
//! - `glyphs=` 是**本次提交排版的 (字, 字号) 唯一对数**（标识 `glyphs_src=requested`）。
//!   epaint 不公开「已光栅化字形数」（`TextureAtlas` 没有该计数），所以这里给的是
//!   「送进排版的不同字形数」——它与实际进图集的字形数同量级，差额只会来自 `missing`
//!   （字体链里没有这个字形）。不把两者混为一谈。
//! - `missing=` 是**真 API** 逐字查出来的：`Context::fonts_mut(|f| f.has_glyph(&FontId, c))`
//!   （`epaint-0.37.0/src/text/fonts.rs:363`）。**miss 与 overflow 是两件不同的事**：
//!   miss = 字体链没覆盖 → 换字体 / 加链；overflow = 图集放不下 → 砍档 / 子集化 / 提上限。
//!   混在一起报会把处置方向带错，所以分两列。
//! - `overflow=` 是**两路信号取或**（`overflow_src=epaint_log_warn+atlas_reset`）：
//!   ① epaint 的告警 `texture_atlas.rs:246`（`TextureAtlas::overflowed` 是私有字段，
//!   公开 API 拿不到）；② **图集高度回落**（`d_h<0`）——放不下时 epaint 把光标重置到
//!   `height/3` 继续画，`grow_height` 又被 `max_texture_side` 顶住，于是高度变小、
//!   先前的字形被覆盖。
//!   为什么必须有 ②：本机实测 `overflow=0` 而 `d_h=-1024` 同时出现——`env_logger`
//!   默认过滤器把 WARN 吃掉了，**只看日志会得出「没有溢出」的错结论**。两路取或之后，
//!   日志器被占也还有 ② 兜底；只有两路都拿不到时才会误报，日志器装不上时打
//!   `overflow_src=atlas_reset`（此时溢出仍可知）。
//! - `atlas=` / `fill=` 都是**累计值**：正常情况下图集只增不减；**`d_h<0` 就是上面那条
//!   重置信号**，不是「图集缩容」。
//! - 字形位图尺寸 = 字号 × `pixels_per_point`，图集开销约 ×ppp²。无显示器环境下 ppp 恒为
//!   1.0，输出行带 `ppp=`，真机 125% / 150% 缩放时要按这个换算。
//!
//! ## 三个实现约束（改代码前先读，否则数字会假）
//!
//! 1. **光栅化发生在下一帧**：本帧排的字形在 `tessellate` 里才 allocate 进图集，所以必须
//!    「排版 → tessellate → 再读尺寸」，同帧读只会读到旧值。
//! 2. **裁剪会漏计**：落在裁剪矩形之外的字形不会被 tessellate，也就不会进图集。所以整段
//!    文本必须换行排进画布内（[`CANVAS_W`] × [`CANVAS_H`]），**不能**裹 `ScrollArea`。
//! 3. **探针模式下不画任何 UI 外壳**：外壳自己的字形会污染计数。

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use ariahost_common::{EXIT_ERR, EXIT_OK};

use crate::logbuf::LogBuffer;

/// GB2312 一级字库的字数（[`crate::atlas_probe_data::GB2312_LEVEL1`] 的长度）。
pub const MAX_ATLAS_CHARS: usize = 3755;

/// `theme::text_styles()` 里「屏内小标题」那一档的族名（`TextStyle::Name`）。
const SUBTITLE_STYLE: &str = "AriaSubtitle";

/// `--profile`：探针排哪些档、每档排多少字。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    /// 每档都排全量（默认，压测上界；与 task-106 的行为一致）
    Worst,
    /// 按产品文案结构折算：Body 全量 / Small 15% / Subtitle 3% / Heading 1.5%
    Real,
    /// 只排 Body 一档全量（单档基线）
    Single,
}

impl Profile {
    /// 解析 `--profile` 取值；认不出返回 `None`（**不猜**：拼错时宁可报用法错）。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "worst" => Some(Self::Worst),
            "real" => Some(Self::Real),
            "single" => Some(Self::Single),
            _ => None,
        }
    }

    /// 输出行里的机器可读名（英文，便于 grep）。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Worst => "worst",
            Self::Real => "real",
            Self::Single => "single",
        }
    }
}

/// 产品阶梯里的四个语义档。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ladder {
    Heading,
    Subtitle,
    Body,
    Small,
}

impl Ladder {
    /// 输出与日志里的英文档名。
    pub const fn name(self) -> &'static str {
        match self {
            Self::Heading => "Heading",
            Self::Subtitle => "Subtitle",
            Self::Body => "Body",
            Self::Small => "Small",
        }
    }

    /// `--profile real` 下该档取全量的**千分比**。
    ///
    /// 依据（不是拍脑袋）：Heading/Subtitle 只用于页面标题与屏内小标题，是**固定文案**
    /// （几十字量级）；Body 用于任务列表与正文，是**用户数据**（可达数千汉字）；
    /// Small 用于状态/次要信息（数百字量级）。所以 real 里 Body 取全量。
    pub const fn share_permille(self) -> u32 {
        match self {
            Self::Heading => 15,
            Self::Subtitle => 30,
            Self::Body => 1000,
            Self::Small => 150,
        }
    }
}

/// 阶梯上的一档：语义档 + 实际字号（点）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LadderStep {
    pub ladder: Ladder,
    pub pt: f32,
}

/// 产品真实阶梯：从 `theme::text_styles()` 取四档，按字号升序去重。
///
/// **为什么从 theme 取而不是写死**：写死一份「探针档位」等于用探针自己的假设去测产品。
/// 两版历史探针（旧 20/14/12、新 14/16/20）都因此测错了负载（task-107）。
///
/// **等宽档（`Monospace` 13pt / `AriaNumber` 16pt 等宽）为什么不排**：产品里等宽档只用于
/// RPC 端点、路径、JSON 与数字，这些是 ASCII，汉字不会落到这一档。这是**排除理由，不是
/// 覆盖结论**：将来若某处把中文放进等宽档，本探针测不到它（已记 known-limitations）。
pub fn product_ladder() -> Vec<LadderStep> {
    let mut steps: Vec<LadderStep> = Vec::new();
    for (style, font) in crate::theme::text_styles() {
        let ladder = match &style {
            egui::TextStyle::Heading => Ladder::Heading,
            egui::TextStyle::Body => Ladder::Body,
            egui::TextStyle::Small => Ladder::Small,
            egui::TextStyle::Name(name) if name.as_ref() == SUBTITLE_STYLE => Ladder::Subtitle,
            _ => continue,
        };
        if !steps.iter().any(|step| step.pt == font.size) {
            steps.push(LadderStep {
                ladder,
                pt: font.size,
            });
        }
    }
    steps.sort_by(|a, b| a.pt.partial_cmp(&b.pt).unwrap_or(std::cmp::Ordering::Equal));
    steps
}

/// 一次 step 的排版计划：`(字号, 该档实际字数)`。
///
/// 每档字数必须落到输出行里，否则 `real` 的结论无法复核（task-107 B）。
pub fn plan(profile: Profile, ladder: &[LadderStep], count: usize) -> Vec<(f32, usize)> {
    let mut out = Vec::new();
    for step in ladder {
        if profile == Profile::Single && step.ladder != Ladder::Body {
            continue;
        }
        let chars = match profile {
            Profile::Real => {
                let scaled = count.saturating_mul(step.ladder.share_permille() as usize) / 1000;
                scaled.max(1).min(count)
            }
            Profile::Worst | Profile::Single => count,
        };
        out.push((step.pt, chars));
    }
    out
}

/// 默认阶梯：从 500 字一路加到整个一级字库。
pub const DEFAULT_CHARS: [usize; 8] = [500, 1000, 1500, 2000, 2500, 3000, 3500, 3755];

/// 画布逻辑宽（点）。3500 汉字 @20pt 约 70000px 宽，换行后仍要落在这块画布里。
const CANVAS_W: f32 = 2400.0;
/// 画布逻辑高（点）。
const CANVAS_H: f32 = 1600.0;

/// 捕获到的 epaint 图集溢出告警次数。
static OVERFLOW_HITS: AtomicU64 = AtomicU64::new(0);

/// 只认 epaint 那条固定告警的日志器。
struct OverflowWatch;

static WATCH: OverflowWatch = OverflowWatch;

impl log::Log for OverflowWatch {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::Level::Warn
    }

    fn log(&self, record: &log::Record<'_>) {
        if record.level() != log::Level::Warn || !record.target().starts_with("epaint") {
            return;
        }
        // epaint 在 `texture_atlas.rs:246` 用一条固定字面量报溢出；别把别的 warn 算进来。
        if format!("{}", record.args()).contains("atlas overflowed") {
            OVERFLOW_HITS.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn flush(&self) {}
}

/// 装日志器；返回 `false` 表示本进程的 `log` 门面已被别人占用（此时溢出不可知）。
fn install_overflow_watch() -> bool {
    let owned = log::set_logger(&WATCH).is_ok();
    log::set_max_level(log::LevelFilter::Warn);
    owned
}

/// 一帧之后能读到的事实。
struct FrameFacts {
    /// 图集纹理的实际尺寸（物理像素）。
    atlas: (usize, usize),
    /// 图集填充率（0..=1）。
    fill: f32,
    /// 本次送进排版的不同字形数（= 不同 (字, 字号) 对数）。
    glyphs: usize,
}

/// 排一段文本 → 光栅化 → 读回图集事实。
fn paint_and_rasterize(ctx: &egui::Context, text: &str, size: f32) -> FrameFacts {
    let canvas = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(CANVAS_W, CANVAS_H));
    let input = egui::RawInput {
        screen_rect: Some(canvas),
        ..Default::default()
    };
    let glyphs = text.chars().collect::<BTreeSet<_>>().len();
    let full = ctx.run_ui(input, |ui| {
        // 只在画布内画：裁剪矩形之外的字形不会被 tessellate，也就不会进图集。
        let painter = ui.painter().with_clip_rect(canvas);
        let galley = painter.layout(
            text.to_owned(),
            egui::FontId::proportional(size),
            egui::Color32::WHITE,
            CANVAS_W - 24.0,
        );
        painter.galley(egui::pos2(12.0, 12.0), galley, egui::Color32::WHITE);
    });
    // 这里没有渲染后端，图集纹理增量没人上传；epaint 的 drop 检查会直接 panic
    // （`epaint-0.37.0/src/textures.rs:337`：`Dropped TexturesDelta with 1 unapplied deltas`），
    // 所以必须显式清掉。清的是「待上传的增量」，不影响图集自身的尺寸与填充率。
    let mut textures = full.textures_delta;
    textures.clear();
    drop(textures);
    // 这一行才是真正光栅化的地方：`tessellate` 才会把每个字形 allocate 进图集。
    let primitives = ctx.tessellate(full.shapes, ctx.pixels_per_point());
    drop(primitives);
    let (atlas, fill) = ctx.fonts(|fonts| (fonts.font_image_size(), fonts.font_atlas_fill_ratio()));
    FrameFacts {
        atlas: (atlas[0], atlas[1]),
        fill,
        glyphs,
    }
}

/// 逐字查「字体链里到底有没有这个字形」，返回前缀累计缺失数（长度 = chars.len() + 1）。
///
/// 这是真 API，不是推断：`FontsView::has_glyph` 会走完整条链（注入的 Inter、系统 provider、
/// 逐字形回退）。3755×3 次调用只在一开始做一遍，之后每档 O(1) 取前缀值。
fn missing_prefix(ctx: &egui::Context, chars: &[char], size: f32) -> Vec<usize> {
    let font_id = egui::FontId::proportional(size);
    let mut prefix = Vec::with_capacity(chars.len() + 1);
    prefix.push(0usize);
    let mut missing = 0usize;
    for &ch in chars {
        if !ctx.fonts_mut(|fonts| fonts.has_glyph(&font_id, ch)) {
            missing += 1;
        }
        prefix.push(missing);
    }
    prefix
}

/// 把可能带空格的值压成单 token（机器可读行不能有空格）。
fn flat(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join("_")
}

/// `--atlas-probe [--chars N[,N...]]` 的实现；返回进程退出码。
pub fn run(chars_arg: Option<&[usize]>, profile: Profile) -> i32 {
    let log = Arc::new(LogBuffer::new());

    let mut steps: Vec<usize> = chars_arg
        .map(<[usize]>::to_vec)
        .unwrap_or_else(|| DEFAULT_CHARS.to_vec());
    steps.retain(|count| *count > 0);
    steps.sort_unstable();
    steps.dedup();
    if steps.is_empty() {
        eprintln!("ATLAS_PROBE failed reason=no_steps");
        return EXIT_ERR;
    }

    // 日志器只是第二路信号；装不装得上都不影响 `d_h<0` 那一路。
    let overflow_src = if install_overflow_watch() {
        "epaint_log_warn+atlas_reset"
    } else {
        "atlas_reset"
    };

    let ladder = product_ladder();
    // 阶梯为空只可能是 theme 被改成四档全丢——那是设计基线被人动过，直接报出来。
    let Some(&first) = ladder.first() else {
        println!("ATLAS_PROBE ok=0 reason=empty_ladder（theme::text_styles 里没有四个语义档）");
        return EXIT_ERR;
    };

    let ctx = egui::Context::default();
    let font = crate::fonts::install(&ctx, &log);
    // 暖机：`Context::fonts_mut` 在第一次 pass 之前会 panic，所以先空跑一帧。
    let warm = paint_and_rasterize(&ctx, "", first.pt);
    let ppp = ctx.pixels_per_point();

    let pool: Vec<char> = crate::atlas_probe_data::GB2312_LEVEL1.chars().collect();
    let steps: Vec<usize> = steps.iter().map(|count| (*count).min(pool.len())).collect();
    let steps_text = steps
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let sizes_text = ladder
        .iter()
        .map(|step| format!("{:.0}", step.pt))
        .collect::<Vec<_>>()
        .join(",");
    let ladder_text = ladder
        .iter()
        .map(|step| format!("{}={:.0}", step.ladder.name(), step.pt))
        .collect::<Vec<_>>()
        .join(",");

    println!(
        "ATLAS_PROBE_BEGIN profile={} sizes={sizes_text} ladder={ladder_text} steps={steps_text} \
         cjk_pool={} ppp={ppp:.2} glyphs_src=requested miss_src=epaint_has_glyph \
         overflow_src={overflow_src} fonts={}",
        profile.as_str(),
        pool.len(),
        flat(&font.fonts_line())
    );

    // 每档各存一份前缀缺失表；`real` 模式下各档字数不同，按字数取下标。
    let missing_by_size: Vec<(f32, Vec<usize>)> = ladder
        .iter()
        .map(|step| (step.pt, missing_prefix(&ctx, &pool, step.pt)))
        .collect();

    let mut last_height = warm.atlas.1;
    let mut atlas_max = (warm.atlas.0, warm.atlas.1);
    let mut overflow_total = 0i64;
    let mut missing_max = 0usize;
    let mut glyphs_max = 0usize;
    let mut step_no = 0usize;
    let mut stopped = "none";

    'outer: for &count in &steps {
        for &(size, chars) in &plan(profile, &ladder, count) {
            step_no += 1;
            let text: String = pool[..chars].iter().collect();
            let before = OVERFLOW_HITS.load(Ordering::Relaxed);
            let facts = paint_and_rasterize(&ctx, &text, size);
            let hits = OVERFLOW_HITS.load(Ordering::Relaxed) as i64 - before as i64;
            let missing = missing_by_size
                .iter()
                .find(|entry| entry.0 == size)
                .and_then(|(_, prefix)| prefix.get(chars))
                .copied()
                .unwrap_or(0);
            missing_max = missing_max.max(missing);
            glyphs_max = glyphs_max.max(facts.glyphs);
            let delta_height = facts.atlas.1 as i64 - last_height as i64;
            last_height = facts.atlas.1;
            atlas_max = (
                atlas_max.0.max(facts.atlas.0),
                atlas_max.1.max(facts.atlas.1),
            );
            // 图集放不下时 epaint 会重置光标（高度回落），先前的字形被覆盖。这条路比
            // 日志可靠：日志器被别的库占了就静默。两路取或，任一命中都算溢出。
            let reset = delta_height < 0;
            let overflow_here = hits > 0 || reset;
            if overflow_here {
                overflow_total += 1;
            }
            println!(
                "ATLAS step={step_no} profile={} chars={chars} size={size:.0} ppp={ppp:.2} \
                 atlas={}x{} fill={:.3} d_h={delta_height:+} glyphs={} missing={missing} \
                 overflow={} reset={}",
                profile.as_str(),
                facts.atlas.0,
                facts.atlas.1,
                facts.fill,
                facts.glyphs,
                i32::from(hits > 0),
                i32::from(reset)
            );
            if overflow_here {
                // 已经溢出：再往上加档只会覆盖旧字形，数字没有意义，停在这里。
                stopped = "overflow";
                break 'outer;
            }
        }
    }

    println!(
        "ATLAS_PROBE ok=1 profile={} steps={step_no} sizes={sizes_text} chars_max={} atlas_max={}x{} \
         overflow_total={overflow_total} overflow_src={overflow_src} missing_max={missing_max} \
         glyphs_max={glyphs_max} ppp={ppp:.2} stopped={stopped}",
        profile.as_str(),
        steps.last().copied().unwrap_or(0),
        atlas_max.0,
        atlas_max.1
    );

    EXIT_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    // 直接调 `WATCH.log(...)` 需要把 trait 带进作用域（方法来自 `log::Log`）。
    use log::Log as _;

    #[test]
    fn atlas_probe_data_is_gb2312_level1() {
        let text = crate::atlas_probe_data::GB2312_LEVEL1;
        let chars = text.chars().collect::<Vec<_>>();
        assert_eq!(chars.len(), MAX_ATLAS_CHARS, "一级字库必须是 3755 字");
        assert_eq!(&chars[..3], &['啊', '阿', '埃'], "首三字必须是啊/阿/埃");
        let unique = chars.iter().collect::<BTreeSet<_>>();
        assert_eq!(unique.len(), MAX_ATLAS_CHARS, "一级字库不得有重复字");
        assert!(chars
            .iter()
            .all(|ch| ('\u{4E00}'..='\u{9FFF}').contains(ch)));
    }

    #[test]
    fn default_ladder_grows_and_stays_in_pool() {
        assert!(DEFAULT_CHARS.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(DEFAULT_CHARS[DEFAULT_CHARS.len() - 1], MAX_ATLAS_CHARS);
    }

    #[test]
    fn ladder_is_the_product_ladder() {
        // 防漂移：四个语义档的字号必须正好是 12/14/16/20（升序）。
        // theme 的阶梯值若被改，这里先红——探针档位不许自己漂。
        let ladder = product_ladder();
        let pts: Vec<f32> = ladder.iter().map(|step| step.pt).collect();
        assert_eq!(pts, [12.0, 14.0, 16.0, 20.0], "四个语义档");
        let names: Vec<&str> = ladder.iter().map(|step| step.ladder.name()).collect();
        assert_eq!(names, ["Small", "Body", "Subtitle", "Heading"]);
        assert_eq!(Ladder::Body.share_permille(), 1000, "real 里 Body 取全量");
        assert_eq!(Ladder::Small.share_permille(), 150);
        assert_eq!(Ladder::Subtitle.share_permille(), 30);
        assert_eq!(Ladder::Heading.share_permille(), 15);
    }

    #[test]
    fn real_plan_follows_the_declared_shares() {
        let plan = plan(Profile::Real, &product_ladder(), 3500);
        assert_eq!(plan.len(), 4);
        assert_eq!(plan[0], (12.0, 525)); // 15%
        assert_eq!(plan[1], (14.0, 3500)); // 全量
        assert_eq!(plan[2], (16.0, 105)); // 3%
        assert_eq!(plan[3], (20.0, 52)); // 1.5%
    }

    #[test]
    fn single_plan_only_keeps_body() {
        let plan = plan(Profile::Single, &product_ladder(), 1000);
        assert_eq!(plan, vec![(14.0, 1000)]);
    }

    #[test]
    fn worst_plan_keeps_every_ladder_step_full() {
        let plan = plan(Profile::Worst, &product_ladder(), 1000);
        assert_eq!(
            plan,
            vec![(12.0, 1000), (14.0, 1000), (16.0, 1000), (20.0, 1000)]
        );
    }

    #[test]
    fn profile_parses_only_known_values() {
        assert_eq!(Profile::parse("worst"), Some(Profile::Worst));
        assert_eq!(Profile::parse("real"), Some(Profile::Real));
        assert_eq!(Profile::parse("single"), Some(Profile::Single));
        assert_eq!(Profile::parse("REAL"), None);
        assert_eq!(Profile::parse(""), None);
    }

    #[test]
    fn flat_joins_whitespace() {
        assert_eq!(
            flat("latin=system:Segoe UI:0:0"),
            "latin=system:Segoe_UI:0:0"
        );
    }

    #[test]
    fn watch_only_counts_the_epaint_overflow_line() {
        // 直接调日志器实现，不装全局 logger（装了就撤不掉，会污染其它测试）。
        OVERFLOW_HITS.store(0, Ordering::Relaxed);
        WATCH.log(
            &log::Record::builder()
                .args(format_args!("epaint texture atlas overflowed!"))
                .level(log::Level::Warn)
                .target("epaint::text::glyph_atlas")
                .build(),
        );
        assert_eq!(OVERFLOW_HITS.load(Ordering::Relaxed), 1);
        WATCH.log(
            &log::Record::builder()
                .args(format_args!("something else"))
                .level(log::Level::Warn)
                .target("epaint::whatever")
                .build(),
        );
        assert_eq!(OVERFLOW_HITS.load(Ordering::Relaxed), 1);
        OVERFLOW_HITS.store(0, Ordering::Relaxed);
    }
}
