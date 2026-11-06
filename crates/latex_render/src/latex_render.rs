use ab_glyph::{Font as _, FontRef, OutlineCurve};
use anyhow::Result;
use collections::HashMap;
use futures::StreamExt;
use futures::channel::mpsc;
use gpui::{
    App, BackgroundExecutor, Bounds, Context, Element, ElementId, FontId as GpuiFontId, FontStyle,
    FontWeight, GlobalElementId, GlyphId, Hsla, InspectorElementId, LayoutId, PathBuilder, Pixels,
    Point, Rgba, Style, Task, TextRun, Window, fill, point, px, size,
};
use parking_lot::{Mutex, RwLock};
use ratex_font::FontId;
use ratex_font_loader::outline_cache;
use ratex_types::color::Color as RatexColor;
use ratex_types::display_item::{DisplayItem, DisplayList};
use ratex_types::path_command::PathCommand;
use std::borrow::Cow;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

static UNDERSCORE_ESCAPE: &str = r"\_";

// Glyph outline coordinates come from ab_glyph in font design units. Most KaTeX
// faces report 1000 units per em; fall back to that when the face doesn't advertise.
const DEFAULT_UNITS_PER_EM: f32 = 1000.0;

// Outline points whose mapped pixel distance is below this are considered to
// continue the current sub-path rather than starting a new one. Matches the
// upstream ratex-svg standalone tolerance.
const SUBPATH_JOIN_TOLERANCE: f32 = 0.01;

// Default stroke width for unfilled ratex SVG paths (radicals, large delimiters).
// ratex-svg's standalone export uses 1.5 user units at its default em; ours is
// applied in device-pixel space so we scale with font size.
const STROKE_WIDTH_EM: f32 = 1.5 / 40.0;

struct KatexFace {
    id: FontId,
    filename: &'static str,
    family: &'static str,
    weight: FontWeight,
    style: FontStyle,
}

// The embedded KaTeX faces ratex can emit, with the family/weight/style they
// advertise to font databases. CJK and emoji fallbacks resolve to system fonts
// at runtime and are handled by the outline-tessellation fallback instead.
const KATEX_FACES: &[KatexFace] = &[
    KatexFace {
        id: FontId::AmsRegular,
        filename: "KaTeX_AMS-Regular.ttf",
        family: "KaTeX_AMS",
        weight: FontWeight::NORMAL,
        style: FontStyle::Normal,
    },
    KatexFace {
        id: FontId::CaligraphicRegular,
        filename: "KaTeX_Caligraphic-Regular.ttf",
        family: "KaTeX_Caligraphic",
        weight: FontWeight::NORMAL,
        style: FontStyle::Normal,
    },
    KatexFace {
        id: FontId::FrakturRegular,
        filename: "KaTeX_Fraktur-Regular.ttf",
        family: "KaTeX_Fraktur",
        weight: FontWeight::NORMAL,
        style: FontStyle::Normal,
    },
    KatexFace {
        id: FontId::FrakturBold,
        filename: "KaTeX_Fraktur-Bold.ttf",
        family: "KaTeX_Fraktur",
        weight: FontWeight::BOLD,
        style: FontStyle::Normal,
    },
    KatexFace {
        id: FontId::MainBold,
        filename: "KaTeX_Main-Bold.ttf",
        family: "KaTeX_Main",
        weight: FontWeight::BOLD,
        style: FontStyle::Normal,
    },
    KatexFace {
        id: FontId::MainBoldItalic,
        filename: "KaTeX_Main-BoldItalic.ttf",
        family: "KaTeX_Main",
        weight: FontWeight::BOLD,
        style: FontStyle::Italic,
    },
    KatexFace {
        id: FontId::MainItalic,
        filename: "KaTeX_Main-Italic.ttf",
        family: "KaTeX_Main",
        weight: FontWeight::NORMAL,
        style: FontStyle::Italic,
    },
    KatexFace {
        id: FontId::MainRegular,
        filename: "KaTeX_Main-Regular.ttf",
        family: "KaTeX_Main",
        weight: FontWeight::NORMAL,
        style: FontStyle::Normal,
    },
    KatexFace {
        id: FontId::MathBoldItalic,
        filename: "KaTeX_Math-BoldItalic.ttf",
        family: "KaTeX_Math",
        weight: FontWeight::BOLD,
        style: FontStyle::Italic,
    },
    KatexFace {
        id: FontId::MathItalic,
        filename: "KaTeX_Math-Italic.ttf",
        family: "KaTeX_Math",
        weight: FontWeight::NORMAL,
        style: FontStyle::Italic,
    },
    KatexFace {
        id: FontId::SansSerifBold,
        filename: "KaTeX_SansSerif-Bold.ttf",
        family: "KaTeX_SansSerif",
        weight: FontWeight::BOLD,
        style: FontStyle::Normal,
    },
    KatexFace {
        id: FontId::SansSerifItalic,
        filename: "KaTeX_SansSerif-Italic.ttf",
        family: "KaTeX_SansSerif",
        weight: FontWeight::NORMAL,
        style: FontStyle::Italic,
    },
    KatexFace {
        id: FontId::SansSerifRegular,
        filename: "KaTeX_SansSerif-Regular.ttf",
        family: "KaTeX_SansSerif",
        weight: FontWeight::NORMAL,
        style: FontStyle::Normal,
    },
    KatexFace {
        id: FontId::ScriptRegular,
        filename: "KaTeX_Script-Regular.ttf",
        family: "KaTeX_Script",
        weight: FontWeight::NORMAL,
        style: FontStyle::Normal,
    },
    KatexFace {
        id: FontId::Size1Regular,
        filename: "KaTeX_Size1-Regular.ttf",
        family: "KaTeX_Size1",
        weight: FontWeight::NORMAL,
        style: FontStyle::Normal,
    },
    KatexFace {
        id: FontId::Size2Regular,
        filename: "KaTeX_Size2-Regular.ttf",
        family: "KaTeX_Size2",
        weight: FontWeight::NORMAL,
        style: FontStyle::Normal,
    },
    KatexFace {
        id: FontId::Size3Regular,
        filename: "KaTeX_Size3-Regular.ttf",
        family: "KaTeX_Size3",
        weight: FontWeight::NORMAL,
        style: FontStyle::Normal,
    },
    KatexFace {
        id: FontId::Size4Regular,
        filename: "KaTeX_Size4-Regular.ttf",
        family: "KaTeX_Size4",
        weight: FontWeight::NORMAL,
        style: FontStyle::Normal,
    },
    KatexFace {
        id: FontId::TypewriterRegular,
        filename: "KaTeX_Typewriter-Regular.ttf",
        family: "KaTeX_Typewriter",
        weight: FontWeight::NORMAL,
        style: FontStyle::Normal,
    },
];

/// Logical-em dimensions of a rendered formula. Multiply by the target font size to get pixel dimensions.
#[derive(Clone, Copy, Debug)]
pub struct RenderedFormula {
    pub width: f64,
    pub height: f64,
    pub depth: f64,
}

/// Parsed and laid-out formula. Cheap to clone (display list lives behind `Arc`), repaints just walk the stored display list.
#[derive(Clone)]
pub struct PreparedFormula {
    pub display_list: Arc<DisplayList>,
    pub metrics: RenderedFormula,
}

#[derive(Clone)]
enum CacheState {
    /// Queued for a background worker.
    Pending,
    /// A background worker is parsing and laying out the formula.
    Running,
    Ready(Result<PreparedFormula, Arc<String>>),
}

impl CacheState {
    fn is_ready(&self) -> bool {
        matches!(self, Self::Ready(_))
    }
}

struct CacheEntry {
    state: CacheState,
    last_used: u64,
    /// The render pass that last requested this entry.
    generation: u64,
}

// The limit is soft. A layout pass requests every formula of its document, and
// entries requested by the current pass are never evicted, so a document with
// more formulas than this keeps its whole working set. Entries left behind by
// edits (one per prefix typed) or removed content are evicted when a pass ends.
const MAX_CACHE_ENTRIES: usize = 512;
const CACHE_EVICTION_BATCH: usize = MAX_CACHE_ENTRIES / 4;
// Parsing and layout is CPU-bound; one math-heavy document must not occupy
// every background thread.
const MAX_CONCURRENT_JOBS: usize = 4;

struct FormulaJob {
    source: String,
    display: bool,
}

#[derive(Default)]
struct FormulaCache {
    inline: HashMap<String, CacheEntry>,
    display: HashMap<String, CacheEntry>,
    clock: u64,
    generation: u64,
    open_passes: usize,
    queue: VecDeque<FormulaJob>,
    workers: usize,
}

impl FormulaCache {
    fn entries(&mut self, display: bool) -> &mut HashMap<String, CacheEntry> {
        if display {
            &mut self.display
        } else {
            &mut self.inline
        }
    }

    fn len(&self) -> usize {
        self.inline.len() + self.display.len()
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    fn begin_pass(&mut self) {
        if self.open_passes == 0 {
            self.generation += 1;
        }
        self.open_passes += 1;
    }

    fn end_pass(&mut self) {
        self.open_passes = self.open_passes.saturating_sub(1);
        if self.open_passes == 0 {
            self.evict_stale();
        }
    }

    /// Looks the formula up, queueing a job for it on a miss.
    fn request(&mut self, source: &str, display: bool) -> Option<Result<PreparedFormula, String>> {
        let clock = self.tick();
        let generation = self.generation;
        if let Some(entry) = self.entries(display).get_mut(source) {
            entry.last_used = clock;
            entry.generation = generation;
            return match &entry.state {
                CacheState::Pending | CacheState::Running => None,
                CacheState::Ready(result) => {
                    Some(result.clone().map_err(|error| error.as_ref().clone()))
                }
            };
        }
        self.entries(display).insert(
            source.to_owned(),
            CacheEntry {
                state: CacheState::Pending,
                last_used: clock,
                generation,
            },
        );
        self.queue.push_back(FormulaJob {
            source: source.to_owned(),
            display,
        });
        None
    }

    /// Evicts least recently used entries that the current pass did not request,
    /// leaving some headroom so the next few edits don't evict again.
    fn evict_stale(&mut self) {
        if self.len() <= MAX_CACHE_ENTRIES {
            return;
        }
        let excess = self.len() - (MAX_CACHE_ENTRIES - CACHE_EVICTION_BATCH);
        let generation = self.generation;
        let mut stale: Vec<(u64, bool, String)> = self
            .inline
            .iter()
            .map(|(source, entry)| (source, entry, false))
            .chain(
                self.display
                    .iter()
                    .map(|(source, entry)| (source, entry, true)),
            )
            .filter(|(_, entry, _)| entry.generation < generation)
            .map(|(source, entry, display)| (entry.last_used, display, source.clone()))
            .collect();
        if stale.len() > excess {
            stale.select_nth_unstable_by_key(excess, |(last_used, _, _)| *last_used);
            stale.truncate(excess);
        }
        for (_, display, source) in stale {
            self.entries(display).remove(&source);
        }
    }

    /// Reserves workers for queued jobs, up to the concurrency limit.
    fn claim_workers(&mut self) -> usize {
        let count = MAX_CONCURRENT_JOBS
            .saturating_sub(self.workers)
            .min(self.queue.len());
        self.workers += count;
        count
    }

    /// Hands a worker its next job, skipping jobs whose formula was evicted or
    /// already finished. Releases the worker when the queue is empty.
    fn next_job(&mut self) -> Option<FormulaJob> {
        while let Some(job) = self.queue.pop_front() {
            if let Some(entry) = self.entries(job.display).get_mut(&job.source)
                && matches!(entry.state, CacheState::Pending)
            {
                entry.state = CacheState::Running;
                return Some(job);
            }
        }
        self.workers = self.workers.saturating_sub(1);
        None
    }

    /// Stores a finished layout. Returns false when the formula is no longer
    /// cached, in which case the result is dropped.
    fn complete(&mut self, job: &FormulaJob, state: CacheState) -> bool {
        match self.entries(job.display).get_mut(&job.source) {
            Some(entry) if !entry.state.is_ready() => {
                entry.state = state;
                true
            }
            _ => false,
        }
    }
}

/// Coalesces completion notifications: while one is waiting to be delivered,
/// further completions don't send another.
struct CompletionNotifier {
    scheduled: Arc<AtomicBool>,
    sender: mpsc::UnboundedSender<()>,
}

impl CompletionNotifier {
    fn formula_completed(&self) {
        if !self.scheduled.swap(true, Ordering::AcqRel) && self.sender.unbounded_send(()).is_err() {
            log::debug!("latex_render: skipping notify for dropped view");
        }
    }
}

/// Counters for observing renderer work, used by regression tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LatexRendererStats {
    /// Formulas currently held by the cache, pending or ready.
    pub cached_formulas: usize,
    /// Cached formulas whose parse and layout has not finished.
    pub pending_formulas: usize,
    /// Parse and layout jobs started since the renderer was created.
    pub jobs_started: u64,
    /// View notifications delivered since the renderer was created.
    pub notifications: u64,
}

#[derive(Default)]
struct RendererCounters {
    jobs_started: AtomicU64,
    notifications: AtomicU64,
}

/// Marks one layout of a whole document. Every formula requested while the
/// pass is open belongs to the document's working set and is protected from
/// eviction; stale entries are evicted when the guard drops.
#[must_use = "the pass ends when the guard is dropped"]
pub struct LatexRenderPass {
    cache: Arc<Mutex<FormulaCache>>,
}

impl Drop for LatexRenderPass {
    fn drop(&mut self) {
        self.cache.lock().end_pass();
    }
}

pub struct LatexRenderer {
    cache: Arc<Mutex<FormulaCache>>,
    executor: BackgroundExecutor,
    counters: Arc<RendererCounters>,
    notifier: Option<Arc<CompletionNotifier>>,
    _notify_task: Option<Task<()>>,
}

impl LatexRenderer {
    /// Build a renderer wired to re-render the owning view whenever formulas
    /// finish parsing in the background. Spawns and owns the foreground
    /// bridge task internally, so callers don't have to thread an mpsc channel through their setup code.
    pub fn for_view<T: 'static>(executor: BackgroundExecutor, cx: &mut Context<T>) -> Self {
        let counters = Arc::new(RendererCounters::default());
        let scheduled = Arc::new(AtomicBool::new(false));
        let (sender, mut receiver) = mpsc::unbounded::<()>();
        let notify_task = cx.spawn({
            let counters = counters.clone();
            let scheduled = scheduled.clone();
            async move |this, cx| {
                while receiver.next().await.is_some() {
                    // Cleared before notifying: a formula finishing after this
                    // point schedules another notification, so the last
                    // completion of a batch is never dropped.
                    scheduled.store(false, Ordering::Release);
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                    counters.notifications.fetch_add(1, Ordering::Relaxed);
                }
            }
        });
        Self {
            cache: Arc::new(Mutex::new(FormulaCache::default())),
            executor,
            counters,
            notifier: Some(Arc::new(CompletionNotifier { scheduled, sender })),
            _notify_task: Some(notify_task),
        }
    }

    /// Opens a layout pass over a whole document. Hold the returned guard until
    /// every formula of the document has been requested.
    pub fn begin_render_pass(&self) -> LatexRenderPass {
        self.cache.lock().begin_pass();
        LatexRenderPass {
            cache: self.cache.clone(),
        }
    }

    pub fn stats(&self) -> LatexRendererStats {
        let cache = self.cache.lock();
        let pending_formulas = cache
            .inline
            .values()
            .chain(cache.display.values())
            .filter(|entry| !entry.state.is_ready())
            .count();
        LatexRendererStats {
            cached_formulas: cache.len(),
            pending_formulas,
            jobs_started: self.counters.jobs_started.load(Ordering::Relaxed),
            notifications: self.counters.notifications.load(Ordering::Relaxed),
        }
    }

    /// Returns `None` while parsing/layout is still in progress. Returns
    /// `Some(Ok(...))` with the cached display list once ready, or `Some(Err(...))`
    /// if parsing failed.
    ///
    /// Colorless: laying out in black lets one entry serve every theme.
    pub fn render(&self, source: &str, display: bool) -> Option<Result<PreparedFormula, String>> {
        let source = sanitize_source(source);
        let (result, workers) = {
            let mut cache = self.cache.lock();
            // A request outside any pass is a pass of its own, so callers that
            // never open passes still get a bounded cache.
            let standalone = cache.open_passes == 0;
            if standalone {
                cache.begin_pass();
            }
            let result = cache.request(&source, display);
            if standalone {
                cache.end_pass();
            }
            let workers = if result.is_none() {
                cache.claim_workers()
            } else {
                0
            };
            (result, workers)
        };
        for _ in 0..workers {
            self.spawn_worker();
        }
        result
    }

    fn spawn_worker(&self) {
        let cache = self.cache.clone();
        let counters = self.counters.clone();
        let notifier = self.notifier.clone();
        self.executor
            .spawn(async move {
                loop {
                    let job = cache.lock().next_job();
                    let Some(job) = job else {
                        break;
                    };
                    counters.jobs_started.fetch_add(1, Ordering::Relaxed);
                    let state = match parse_and_layout(&job.source, job.display) {
                        Ok(prepared) => CacheState::Ready(Ok(prepared)),
                        Err(error) => CacheState::Ready(Err(Arc::new(error.to_string()))),
                    };
                    let stored = cache.lock().complete(&job, state);
                    if stored && let Some(notifier) = &notifier {
                        notifier.formula_completed();
                    }
                }
            })
            .detach();
    }
}

/// Prettier rewrites `_` inside math as `\_`, which breaks subscripts.
fn sanitize_source(source: &str) -> Cow<'_, str> {
    let trimmed = source.trim();
    if trimmed.contains(UNDERSCORE_ESCAPE) {
        Cow::Owned(trimmed.replace(UNDERSCORE_ESCAPE, "_"))
    } else {
        Cow::Borrowed(trimmed)
    }
}

fn parse_and_layout(source: &str, display: bool) -> Result<PreparedFormula> {
    use ratex_layout::{LayoutOptions, layout, to_display_list};
    use ratex_parser::parse;
    use ratex_types::math_style::MathStyle;

    let ast = parse(source).map_err(|error| anyhow::anyhow!("LaTeX parse error: {error}"))?;
    let opts = LayoutOptions::default()
        .with_style(if display {
            MathStyle::Display
        } else {
            MathStyle::Text
        })
        .with_color(RatexColor::BLACK);
    let lbox = layout(&ast, &opts);
    let dl = to_display_list(&lbox);
    let metrics = RenderedFormula {
        width: dl.width,
        height: dl.height,
        depth: dl.depth,
    };
    Ok(PreparedFormula {
        display_list: Arc::new(dl),
        metrics,
    })
}

/// Where a formula sits relative to surrounding content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MathPlacement {
    /// Math flows with text: the element occupies a line-height-tall slot and the formula baseline is pinned to the surrounding text's baseline.
    Inline,
    /// Math owns its own block: a tight box (formula width by height plus depth) meant for a centered display container.
    Display,
}

/// Build a GPUI element that paints `formula` natively. The formula's em size
/// is the inherited text style's font size times `font_scale`, and it draws in
/// the inherited text color (for any item the formula didn't recolor).
///
/// All metrics are resolved from `window.text_style()` during this element's
/// own layout and paint, not captured at build time. Sibling text elements
/// resolve their fonts and line heights the same way, so inline placement
/// stays exact under enclosing div text-style refinements (paragraph
/// line-height overrides, heading font sizes) and at any zoom level.
///
/// For [`MathPlacement::Inline`], descenders paint past the slot bottom by
/// design (no overflow clipping). GPUI does not feed text-glyph baselines into
/// taffy, so `items_baseline` on a flex container treats every item's baseline
/// as its bottom edge. Making this element the same height as the host text
/// line and offsetting the painted formula inside is what lines the baselines up.
pub fn latex_formula_element(
    formula: PreparedFormula,
    placement: MathPlacement,
    font_scale: f32,
) -> LatexFormulaElement {
    LatexFormulaElement {
        formula,
        placement,
        font_scale,
    }
}

pub struct LatexFormulaElement {
    formula: PreparedFormula,
    placement: MathPlacement,
    font_scale: f32,
}

/// Public only as this element's `Element::RequestLayoutState`.
pub struct ResolvedFormulaLayout {
    em: f32,
    size: gpui::Size<Pixels>,
    /// Top of the painted formula relative to the element's top.
    formula_top: f32,
}

impl LatexFormulaElement {
    fn resolve_layout(&self, window: &Window) -> ResolvedFormulaLayout {
        let text_style = window.text_style();
        let rem_size = window.rem_size();
        let text_font_size = text_style.font_size.to_pixels(rem_size);
        let em = f32::from(text_font_size) * self.font_scale;
        let width = self.formula.metrics.width as f32 * em;
        let height = self.formula.metrics.height as f32 * em;
        let depth = self.formula.metrics.depth as f32 * em;
        match self.placement {
            MathPlacement::Inline => {
                let line_height = text_style.line_height_in_pixels(rem_size);
                // Match the surrounding text's painted baseline exactly: shape
                // a probe line through the same pipeline text elements use, and
                // reproduce text painting's `(line_height - ascent - descent) /
                // 2 + ascent` placement. Font-header metrics can differ
                // slightly from the shaped line's ascent/descent, which reads
                // as the formula sitting a hair below the text baseline.
                let probe = window.text_system().shape_line(
                    "x".into(),
                    text_font_size,
                    &[TextRun {
                        len: 1,
                        font: text_style.font(),
                        color: text_style.color,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }],
                    None,
                );
                let text_baseline =
                    (line_height - probe.ascent - probe.descent) / 2. + probe.ascent;
                ResolvedFormulaLayout {
                    em,
                    size: size(px(width), line_height),
                    formula_top: f32::from(text_baseline) - height,
                }
            }
            MathPlacement::Display => ResolvedFormulaLayout {
                em,
                size: size(px(width), px(height + depth)),
                formula_top: 0.0,
            },
        }
    }
}

impl Element for LatexFormulaElement {
    type RequestLayoutState = ResolvedFormulaLayout;
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ResolvedFormulaLayout) {
        let mut style = Style::default();
        let resolved = self.resolve_layout(window);
        style.size.width = resolved.size.width.into();
        style.size.height = resolved.size.height.into();
        (window.request_layout(style, [], cx), resolved)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut ResolvedFormulaLayout,
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        resolved: &mut ResolvedFormulaLayout,
        _prepaint: &mut (),
        window: &mut Window,
        _cx: &mut App,
    ) {
        let color = window.text_style().color;
        let origin = point(bounds.origin.x, bounds.origin.y + px(resolved.formula_top));
        paint_display_list(
            window,
            origin,
            resolved.em,
            color,
            &self.formula.display_list,
        );
    }
}

fn paint_display_list(
    window: &mut Window,
    origin: Point<Pixels>,
    em: f32,
    default_color: Hsla,
    display_list: &DisplayList,
) {
    let ox = f32::from(origin.x);
    let oy = f32::from(origin.y);
    for item in &display_list.items {
        match item {
            DisplayItem::GlyphPath {
                x,
                y,
                scale,
                font,
                char_code,
                color,
            } => {
                let color = ratex_to_hsla(*color, default_color);
                paint_glyph(
                    window,
                    ox,
                    oy,
                    em,
                    *x as f32,
                    *y as f32,
                    *scale as f32,
                    font,
                    *char_code,
                    color,
                );
            }
            DisplayItem::Line {
                x,
                y,
                width,
                thickness,
                color,
                dashed,
            } => {
                let color = ratex_to_hsla(*color, default_color);
                let thickness_px = (*thickness as f32) * em;
                let x_px = ox + (*x as f32) * em;
                let center_y_px = oy + (*y as f32) * em;
                let width_px_val = (*width as f32) * em;
                if *dashed {
                    paint_dashed_line(window, x_px, center_y_px, width_px_val, thickness_px, color);
                } else {
                    // Snap rules (fraction bars, overlines) to device pixels like
                    // GPUI underlines, so a thin bar lands on one pixel row
                    // instead of smearing in half-tones across two.
                    let scale_factor = window.scale_factor();
                    let thickness_snapped =
                        (thickness_px * scale_factor).round().max(1.0) / scale_factor;
                    let top_px =
                        ((center_y_px - thickness_px / 2.0) * scale_factor).round() / scale_factor;
                    let bounds = Bounds {
                        origin: point(px(x_px), px(top_px)),
                        size: size(px(width_px_val), px(thickness_snapped)),
                    };
                    window.paint_quad(fill(bounds, color));
                }
            }
            DisplayItem::Rect {
                x,
                y,
                width,
                height,
                color,
            } => {
                let color = ratex_to_hsla(*color, default_color);
                let bounds = Bounds {
                    origin: point(px(ox + (*x as f32) * em), px(oy + (*y as f32) * em)),
                    size: size(px((*width as f32) * em), px((*height as f32) * em)),
                };
                window.paint_quad(fill(bounds, color));
            }
            DisplayItem::Path {
                x,
                y,
                commands,
                fill: filled,
                color,
            } => {
                let color = ratex_to_hsla(*color, default_color);
                paint_em_path(
                    window,
                    ox + (*x as f32) * em,
                    oy + (*y as f32) * em,
                    em,
                    commands,
                    *filled,
                    color,
                );
            }
        }
    }
}

fn ratex_to_hsla(color: RatexColor, default: Hsla) -> Hsla {
    // Layout stamps black on anything the source did not recolor itself.
    if color == RatexColor::BLACK {
        return default;
    }
    Rgba {
        r: color.r,
        g: color.g,
        b: color.b,
        a: color.a,
    }
    .into()
}

fn paint_em_path(
    window: &mut Window,
    origin_x_px: f32,
    origin_y_px: f32,
    em: f32,
    commands: &[PathCommand],
    filled: bool,
    color: Hsla,
) {
    // ratex emits one DisplayItem::Path with multiple subpaths separated by `MoveTo`.
    // Lyon's fill tessellator behaves more reliably when each subpath is built as its
    // own Path, matching ratex-svg's approach of emitting one `<path>` per subpath.
    let mut subpath_start: Option<usize> = None;
    for (i, cmd) in commands.iter().enumerate() {
        if matches!(cmd, PathCommand::MoveTo { .. }) {
            if let Some(start) = subpath_start
                && start < i
            {
                paint_subpath(
                    window,
                    origin_x_px,
                    origin_y_px,
                    em,
                    &commands[start..i],
                    filled,
                    color,
                );
            }
            subpath_start = Some(i);
        }
    }
    if let Some(start) = subpath_start
        && start < commands.len()
    {
        paint_subpath(
            window,
            origin_x_px,
            origin_y_px,
            em,
            &commands[start..],
            filled,
            color,
        );
    }
}

// Filled subpaths at most this many device pixels across one axis are painted
// as snapped quads instead of tessellated paths. Extensible delimiter bars
// (`\|`, `\left|`) are ~0.04em wide, under a device pixel at body sizes, and
// antialias into near-invisible smears when they straddle pixel boundaries,
// making the same delimiter visible on one side of a formula and not the other.
const HAIRLINE_MAX_DEVICE_PX: f32 = 2.0;
// A subpath only counts as a hairline strip when it is this many times longer
// than it is thick, so small square-ish path pieces still tessellate normally.
const HAIRLINE_MIN_ASPECT: f32 = 4.0;

fn paint_subpath(
    window: &mut Window,
    origin_x_px: f32,
    origin_y_px: f32,
    em: f32,
    commands: &[PathCommand],
    filled: bool,
    color: Hsla,
) {
    if filled
        && let Some(bounds) = hairline_strip_bounds(window, origin_x_px, origin_y_px, em, commands)
    {
        window.paint_quad(fill(bounds, color));
        return;
    }
    let stroke_width = px(STROKE_WIDTH_EM * em);
    let mut builder = if filled {
        PathBuilder::fill()
    } else {
        PathBuilder::stroke(stroke_width)
    };
    let map = |x: f64, y: f64| {
        point(
            px(origin_x_px + x as f32 * em),
            px(origin_y_px + y as f32 * em),
        )
    };
    let mut started = false;
    for cmd in commands {
        match *cmd {
            PathCommand::MoveTo { x, y } => {
                builder.move_to(map(x, y));
                started = true;
            }
            PathCommand::LineTo { x, y } => {
                builder.line_to(map(x, y));
            }
            PathCommand::QuadTo { x1, y1, x, y } => {
                builder.curve_to(map(x, y), map(x1, y1));
            }
            PathCommand::CubicTo {
                x1,
                y1,
                x2,
                y2,
                x,
                y,
            } => {
                builder.cubic_bezier_to(map(x, y), map(x1, y1), map(x2, y2));
            }
            PathCommand::Close => {
                builder.close();
            }
        }
    }
    if started && filled {
        // Auto-close any subpath that ratex left open: fill tessellation requires
        // a closed loop, otherwise lyon stitches a straight edge across the gap.
        builder.close();
    }
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

/// If `commands` outline a thin vertical or horizontal strip, return its
/// device-pixel-snapped bounds (thin axis at least one device pixel) so the
/// caller can draw a quad instead of tessellating. Rounded strip ends are
/// flattened into the quad; at hairline sizes the difference is invisible.
fn hairline_strip_bounds(
    window: &Window,
    origin_x_px: f32,
    origin_y_px: f32,
    em: f32,
    commands: &[PathCommand],
) -> Option<Bounds<Pixels>> {
    let mut min = (f32::MAX, f32::MAX);
    let mut max = (f32::MIN, f32::MIN);
    let mut extend = |x: f64, y: f64| {
        let (x, y) = (x as f32, y as f32);
        min = (min.0.min(x), min.1.min(y));
        max = (max.0.max(x), max.1.max(y));
    };
    for cmd in commands {
        match *cmd {
            PathCommand::MoveTo { x, y } | PathCommand::LineTo { x, y } => extend(x, y),
            PathCommand::QuadTo { x1, y1, x, y } => {
                extend(x1, y1);
                extend(x, y);
            }
            PathCommand::CubicTo {
                x1,
                y1,
                x2,
                y2,
                x,
                y,
            } => {
                extend(x1, y1);
                extend(x2, y2);
                extend(x, y);
            }
            PathCommand::Close => {}
        }
    }
    if min.0 > max.0 || min.1 > max.1 {
        return None;
    }

    let width_px = (max.0 - min.0) * em;
    let height_px = (max.1 - min.1) * em;
    let scale_factor = window.scale_factor();
    let snap = |value: f32| (value * scale_factor).round() / scale_factor;
    let snap_thickness = |value: f32| (value * scale_factor).round().max(1.0) / scale_factor;

    if width_px * scale_factor <= HAIRLINE_MAX_DEVICE_PX
        && height_px >= HAIRLINE_MIN_ASPECT * width_px
    {
        return Some(Bounds {
            origin: point(
                px(snap(origin_x_px + min.0 * em)),
                px(origin_y_px + min.1 * em),
            ),
            size: size(px(snap_thickness(width_px)), px(height_px)),
        });
    }
    if height_px * scale_factor <= HAIRLINE_MAX_DEVICE_PX
        && width_px >= HAIRLINE_MIN_ASPECT * height_px
    {
        return Some(Bounds {
            origin: point(
                px(origin_x_px + min.0 * em),
                px(snap(origin_y_px + min.1 * em)),
            ),
            size: size(px(width_px), px(snap_thickness(height_px))),
        });
    }
    None
}

fn paint_dashed_line(
    window: &mut Window,
    x: f32,
    center_y: f32,
    width: f32,
    thickness: f32,
    color: Hsla,
) {
    let dash_unit = px(thickness * 4.0);
    let mut builder = PathBuilder::stroke(px(thickness)).dash_array(&[dash_unit, dash_unit]);
    builder.move_to(point(px(x), px(center_y)));
    builder.line_to(point(px(x + width), px(center_y)));
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

fn paint_glyph(
    window: &mut Window,
    origin_x_px: f32,
    origin_y_px: f32,
    em: f32,
    x_em: f32,
    y_em: f32,
    glyph_scale: f32,
    font_str: &str,
    char_code: u32,
    color: Hsla,
) {
    let Some(font_id) = FontId::parse(font_str) else {
        return;
    };
    let Some(ch) = char::from_u32(char_code) else {
        return;
    };
    let pen_x = origin_x_px + x_em * em;
    let pen_y = origin_y_px + y_em * em;

    // Prefer GPUI's glyph atlas: the platform text system rasterizes with the
    // same hinting and subpixel treatment as editor text, so math glyphs stay
    // sharp at every zoom level. Outline tessellation below remains as the
    // fallback for system CJK/emoji faces and registration failures.
    if let Some((gpui_font_id, glyph_id)) = atlas_glyph(window, font_id, ch) {
        match window.paint_glyph(
            point(px(pen_x), px(pen_y)),
            gpui_font_id,
            glyph_id,
            px(glyph_scale * em),
            color,
        ) {
            Ok(()) => return,
            Err(error) => {
                log::warn!("latex_render: atlas glyph paint failed for {ch:?}: {error}")
            }
        }
    }

    with_font(font_id, |font_ref| {
        let glyph_id = font_ref.glyph_id(ch);
        let Some(curves) = outline_cache::get_or_compute_outline(font_id, font_ref, glyph_id)
        else {
            return;
        };
        if curves.is_empty() {
            return;
        }
        let upem = font_ref.units_per_em().unwrap_or(DEFAULT_UNITS_PER_EM);
        let unit_scale = glyph_scale * em / upem;
        paint_glyph_curves(window, pen_x, pen_y, unit_scale, &curves, color);
    });
}

fn paint_glyph_curves(
    window: &mut Window,
    pen_x: f32,
    pen_y: f32,
    unit_scale: f32,
    curves: &[OutlineCurve],
    color: Hsla,
) {
    let mut builder = PathBuilder::fill();
    let mut last_end: Option<(f32, f32)> = None;
    // ab_glyph outlines are y-up with baseline at zero; flip y to GPUI's y-down.
    let map = |p: ab_glyph::Point| (pen_x + p.x * unit_scale, pen_y - p.y * unit_scale);

    for curve in curves {
        let (start, end) = match curve {
            OutlineCurve::Line(p0, p1) => (map(*p0), map(*p1)),
            OutlineCurve::Quad(p0, _, p2) => (map(*p0), map(*p2)),
            OutlineCurve::Cubic(p0, _, _, p3) => (map(*p0), map(*p3)),
        };
        let need_move = last_end.is_none_or(|(lx, ly)| {
            (lx - start.0).abs() > SUBPATH_JOIN_TOLERANCE
                || (ly - start.1).abs() > SUBPATH_JOIN_TOLERANCE
        });
        if need_move {
            if last_end.is_some() {
                builder.close();
            }
            builder.move_to(point(px(start.0), px(start.1)));
        }
        match curve {
            OutlineCurve::Line(_, p1) => {
                let pt = map(*p1);
                builder.line_to(point(px(pt.0), px(pt.1)));
            }
            OutlineCurve::Quad(_, p1, p2) => {
                let ctrl = map(*p1);
                let pt = map(*p2);
                builder.curve_to(point(px(pt.0), px(pt.1)), point(px(ctrl.0), px(ctrl.1)));
            }
            OutlineCurve::Cubic(_, p1, p2, p3) => {
                let c1 = map(*p1);
                let c2 = map(*p2);
                let pt = map(*p3);
                builder.cubic_bezier_to(
                    point(px(pt.0), px(pt.1)),
                    point(px(c1.0), px(c1.1)),
                    point(px(c2.0), px(c2.1)),
                );
            }
        }
        last_end = Some(end);
    }
    if last_end.is_some() {
        builder.close();
    }
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

type AtlasGlyphs = RwLock<HashMap<(FontId, char), Option<(GpuiFontId, GlyphId)>>>;

fn atlas_glyphs() -> &'static AtlasGlyphs {
    static ATLAS_GLYPHS: OnceLock<AtlasGlyphs> = OnceLock::new();
    ATLAS_GLYPHS.get_or_init(|| RwLock::new(HashMap::default()))
}

/// Resolve a ratex font + character to a GPUI font id and glyph id suitable
/// for `Window::paint_glyph`. Glyph indices are font-internal, and GPUI loads
/// the same TTF bytes we query through ab_glyph, so the cmap lookup is valid
/// for both.
fn atlas_glyph(window: &Window, font_id: FontId, ch: char) -> Option<(GpuiFontId, GlyphId)> {
    if let Some(resolved) = atlas_glyphs().read().get(&(font_id, ch)) {
        return *resolved;
    }
    let resolved = (|| {
        let gpui_font_id = resolve_katex_font(window, font_id)?;
        let glyph_index = with_font(font_id, |font_ref| font_ref.glyph_id(ch).0)?;
        if glyph_index == 0 {
            return None;
        }
        Some((gpui_font_id, GlyphId(glyph_index as u32)))
    })();
    atlas_glyphs().write().insert((font_id, ch), resolved);
    resolved
}

fn resolved_fonts() -> &'static RwLock<HashMap<FontId, Option<GpuiFontId>>> {
    static RESOLVED: OnceLock<RwLock<HashMap<FontId, Option<GpuiFontId>>>> = OnceLock::new();
    RESOLVED.get_or_init(|| RwLock::new(HashMap::default()))
}

fn resolve_katex_font(window: &Window, font_id: FontId) -> Option<GpuiFontId> {
    if let Some(resolved) = resolved_fonts().read().get(&font_id) {
        return *resolved;
    }
    let resolved = (|| {
        let face = KATEX_FACES.iter().find(|face| face.id == font_id)?;
        if !register_katex_fonts(window) {
            return None;
        }
        let mut font = gpui::font(face.family);
        font.weight = face.weight;
        font.style = face.style;
        let text_system = window.text_system();
        let candidate = text_system.resolve_font(&font);
        // `resolve_font` silently substitutes the default font stack when the
        // family is missing; painting with another face's glyph indices would
        // draw garbage, so verify the id maps back to the family we asked for.
        let family_matches = text_system
            .get_font_for_id(candidate)
            .is_some_and(|resolved| resolved.family == face.family);
        family_matches.then_some(candidate)
    })();
    resolved_fonts().write().insert(font_id, resolved);
    resolved
}

fn register_katex_fonts(window: &Window) -> bool {
    static REGISTERED: OnceLock<bool> = OnceLock::new();
    *REGISTERED.get_or_init(|| {
        let fonts: Vec<Cow<'static, [u8]>> = KATEX_FACES
            .iter()
            .filter_map(|face| {
                let bytes = ratex_katex_fonts::ttf_bytes(face.filename)?;
                Some(patched_font_bytes(face, bytes))
            })
            .collect();
        if fonts.len() != KATEX_FACES.len() {
            log::error!("latex_render: embedded KaTeX font files are missing");
            return false;
        }
        match window.text_system().add_fonts(fonts) {
            Ok(()) => true,
            Err(error) => {
                log::error!("latex_render: failed to register KaTeX fonts: {error}");
                false
            }
        }
    })
}

/// KaTeX TTFs ship with no style bits set: `fsSelection` is REGULAR and
/// `head.macStyle` is zero on every face, italic ones included. Font databases
/// therefore can't tell Main-Regular from Main-Italic (both "Normal, 400") and
/// family matching degrades to registration order; rasterizing one face with
/// glyph indices computed from another draws shifted glyphs (`=` becomes `@`).
/// Patch each face's real style into the bytes before registering.
fn patched_font_bytes(face: &KatexFace, bytes: Cow<'static, [u8]>) -> Cow<'static, [u8]> {
    let italic = face.style == FontStyle::Italic;
    let bold = face.weight == FontWeight::BOLD;
    if !italic && !bold {
        return bytes;
    }
    match apply_style_bits(&bytes, italic, bold) {
        Some(patched) => Cow::Owned(patched),
        None => {
            log::warn!(
                "latex_render: could not patch style bits for {}",
                face.filename
            );
            bytes
        }
    }
}

fn apply_style_bits(bytes: &[u8], italic: bool, bold: bool) -> Option<Vec<u8>> {
    let mut data = bytes.to_vec();
    let num_tables = u16::from_be_bytes([*data.get(4)?, *data.get(5)?]) as usize;
    let mut fs_selection_at = None;
    let mut mac_style_at = None;
    for i in 0..num_tables {
        let record = 12 + i * 16;
        let tag = data.get(record..record + 4)?;
        let table_offset =
            u32::from_be_bytes(data.get(record + 8..record + 12)?.try_into().ok()?) as usize;
        match tag {
            // OS/2 `fsSelection` lives at byte 62 of the table in every version.
            b"OS/2" => fs_selection_at = Some(table_offset + 62),
            // `macStyle` lives at byte 44 of `head`.
            b"head" => mac_style_at = Some(table_offset + 44),
            _ => {}
        }
    }

    fn patch_u16(data: &mut [u8], at: usize, set: u16, clear: u16) -> Option<()> {
        let raw: [u8; 2] = data.get(at..at + 2)?.try_into().ok()?;
        let value = (u16::from_be_bytes(raw) & !clear) | set;
        data.get_mut(at..at + 2)?
            .copy_from_slice(&value.to_be_bytes());
        Some(())
    }

    // fsSelection: bit 0 = ITALIC, bit 5 = BOLD, bit 6 = REGULAR (exclusive
    // with the other two, so clear it).
    let fs_bits = (italic as u16) | ((bold as u16) << 5);
    patch_u16(&mut data, fs_selection_at?, fs_bits, 1 << 6)?;
    // macStyle: bit 0 = bold, bit 1 = italic.
    let mac_bits = (bold as u16) | ((italic as u16) << 1);
    patch_u16(&mut data, mac_style_at?, mac_bits, 0)?;
    Some(data)
}

struct FontEntry {
    font_ref: FontRef<'static>,
}

fn cache() -> &'static RwLock<HashMap<FontId, FontEntry>> {
    static FONT_CACHE: OnceLock<RwLock<HashMap<FontId, FontEntry>>> = OnceLock::new();
    FONT_CACHE.get_or_init(|| RwLock::new(HashMap::default()))
}

fn with_font<R>(font_id: FontId, f: impl FnOnce(&FontRef<'_>) -> R) -> Option<R> {
    {
        let r = cache().read();
        if let Some(entry) = r.get(&font_id) {
            return Some(f(&entry.font_ref));
        }
    }
    let bytes = load_font_bytes(font_id)?;
    // Box::leak is intentional: KaTeX fonts are loaded once per process and
    // used for the program's lifetime.
    let static_bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    let font_ref = FontRef::try_from_slice(static_bytes).ok()?;
    {
        let mut w = cache().write();
        w.entry(font_id).or_insert(FontEntry { font_ref });
    }
    let r = cache().read();
    let entry = r.get(&font_id)?;
    Some(f(&entry.font_ref))
}

fn load_font_bytes(font_id: FontId) -> Option<Vec<u8>> {
    // ratex-font-loader doesn't expose a "load this font_id" helper; we synthesize
    // a one-item DisplayList that uses the target font and let it do the routing
    // (embedded KaTeX fonts vs system CJK/emoji).
    let probe = DisplayItem::GlyphPath {
        x: 0.0,
        y: 0.0,
        scale: 1.0,
        font: font_id.as_str().to_string(),
        char_code: 0x41,
        color: RatexColor::BLACK,
    };
    let set = ratex_font_loader::load_fonts_for_items("", std::slice::from_ref(&probe)).ok()?;
    set.get(&font_id).map(|bytes| bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext as _;

    #[test]
    fn sanitize_reverses_prettier_underscore_escape() {
        assert_eq!(
            sanitize_source(r"x\_{1,2} = \frac{-b}{2a} \tag{1}"),
            r"x_{1,2} = \frac{-b}{2a} \tag{1}"
        );
    }

    #[test]
    fn sanitize_borrows_when_nothing_to_rewrite() {
        // The per-frame lookup must not allocate a key.
        assert!(matches!(
            sanitize_source("  x^2 + y^2 = z^2  "),
            Cow::Borrowed("x^2 + y^2 = z^2")
        ));
    }

    fn failed_state() -> CacheState {
        CacheState::Ready(Err(Arc::new("unused".to_string())))
    }

    /// Requests `sources` in one pass, then finishes every queued job.
    fn request_pass(cache: &mut FormulaCache, sources: &[String]) {
        cache.begin_pass();
        for source in sources {
            cache.request(source, false);
        }
        cache.end_pass();
        while let Some(job) = cache.next_job() {
            cache.complete(&job, failed_state());
        }
    }

    fn numbered(range: std::ops::Range<usize>) -> Vec<String> {
        range.map(|index| format!("x^{{{index}}}")).collect()
    }

    #[test]
    fn cache_keeps_every_entry_requested_by_the_current_pass() {
        let mut cache = FormulaCache::default();
        let sources = numbered(0..MAX_CACHE_ENTRIES + 100);
        request_pass(&mut cache, &sources);
        request_pass(&mut cache, &sources);
        assert_eq!(cache.len(), sources.len());
    }

    #[test]
    fn cache_evicts_stale_entries_oldest_first() {
        let mut cache = FormulaCache::default();
        request_pass(&mut cache, &numbered(0..MAX_CACHE_ENTRIES + 1));
        let newest = format!("x^{{{MAX_CACHE_ENTRIES}}}");
        request_pass(&mut cache, std::slice::from_ref(&newest));

        assert!(cache.len() <= MAX_CACHE_ENTRIES - CACHE_EVICTION_BATCH);
        assert!(!cache.inline.contains_key("x^{0}"), "oldest entry survived");
        assert!(
            cache.inline.contains_key(&newest),
            "requested entry evicted"
        );
        assert!(
            cache
                .inline
                .contains_key(&format!("x^{{{}}}", MAX_CACHE_ENTRIES - 1)),
            "recently used entry evicted"
        );
    }

    #[test]
    fn queued_jobs_for_evicted_formulas_are_skipped() {
        let mut cache = FormulaCache::default();
        cache.begin_pass();
        cache.request("abandoned", false);
        cache.end_pass();
        // The document moves on before the job runs, and the stale entry is evicted.
        cache.begin_pass();
        for source in numbered(0..MAX_CACHE_ENTRIES + 1) {
            cache.request(&source, false);
        }
        cache.end_pass();
        assert!(!cache.inline.contains_key("abandoned"));

        let mut started = Vec::new();
        while let Some(job) = cache.next_job() {
            started.push(job.source.clone());
            cache.complete(&job, failed_state());
        }
        assert!(!started.iter().any(|source| source == "abandoned"));
        assert_eq!(started.len(), MAX_CACHE_ENTRIES + 1);
        assert_eq!(cache.workers, 0);
    }

    #[test]
    fn cache_separates_inline_and_display_layouts() {
        let mut cache = FormulaCache::default();
        cache.request(r"\sum_{i=0}^n i", false);
        assert!(cache.entries(false).contains_key(r"\sum_{i=0}^n i"));
        assert!(!cache.entries(true).contains_key(r"\sum_{i=0}^n i"));
    }

    struct TestHost;

    fn test_renderer(cx: &mut gpui::TestAppContext) -> (gpui::Entity<TestHost>, LatexRenderer) {
        let host = cx.new(|_| TestHost);
        let executor = cx.executor();
        let renderer = host.update(cx, |_, cx| LatexRenderer::for_view(executor, cx));
        (host, renderer)
    }

    fn synthetic_formula(index: usize) -> String {
        format!("r_{{{index}}} = {index} + 1")
    }

    /// Requests every formula inside one pass and returns how many were not ready.
    fn render_pass(renderer: &LatexRenderer, formulas: &[String], display: bool) -> usize {
        let _pass = renderer.begin_render_pass();
        formulas
            .iter()
            .filter(|formula| renderer.render(formula, display).is_none())
            .count()
    }

    #[gpui::test]
    fn working_set_larger_than_the_cache_survives_its_own_pass(cx: &mut gpui::TestAppContext) {
        let (_host, renderer) = test_renderer(cx);
        let formulas: Vec<String> = (0..MAX_CACHE_ENTRIES + 100)
            .map(synthetic_formula)
            .collect();

        // In the app, background parses finish while the first pass is still
        // walking the document, so early formulas are ready before late ones
        // are inserted.
        {
            let _pass = renderer.begin_render_pass();
            for (index, formula) in formulas.iter().enumerate() {
                renderer.render(formula, false);
                if index % 50 == 49 {
                    cx.run_until_parked();
                }
            }
        }
        cx.run_until_parked();
        assert_eq!(render_pass(&renderer, &formulas, false), 0);

        let settled = renderer.stats();
        assert_eq!(settled.pending_formulas, 0);
        assert_eq!(
            settled.jobs_started,
            formulas.len() as u64,
            "every formula should be parsed exactly once"
        );
        assert!(settled.cached_formulas >= formulas.len());

        assert_eq!(render_pass(&renderer, &formulas, false), 0);
        cx.run_until_parked();
        assert_eq!(
            renderer.stats().jobs_started,
            settled.jobs_started,
            "an unchanged pass must not start new jobs"
        );
    }

    #[gpui::test]
    fn stale_entries_are_pruned_back_to_the_soft_limit(cx: &mut gpui::TestAppContext) {
        let (_host, renderer) = test_renderer(cx);
        let formulas: Vec<String> = (0..MAX_CACHE_ENTRIES + 100)
            .map(synthetic_formula)
            .collect();
        render_pass(&renderer, &formulas, false);
        cx.run_until_parked();
        assert!(renderer.stats().cached_formulas > MAX_CACHE_ENTRIES);

        // The document shrinks to a few formulas it already had.
        let remaining = &formulas[..10];
        assert_eq!(render_pass(&renderer, remaining, false), 0);
        let stats = renderer.stats();
        assert!(
            stats.cached_formulas <= MAX_CACHE_ENTRIES,
            "cache should return to its soft limit, holds {}",
            stats.cached_formulas
        );
        assert_eq!(render_pass(&renderer, remaining, false), 0);
        assert_eq!(renderer.stats().jobs_started, formulas.len() as u64);
    }

    #[gpui::test]
    fn pending_entries_are_deduplicated(cx: &mut gpui::TestAppContext) {
        let (_host, renderer) = test_renderer(cx);
        let formula = vec![synthetic_formula(7); 3];
        assert_eq!(render_pass(&renderer, &formula, false), 3);
        cx.run_until_parked();
        assert_eq!(renderer.stats().jobs_started, 1);
        assert_eq!(render_pass(&renderer, &formula, false), 0);
    }

    #[gpui::test]
    fn inline_and_display_layouts_are_cached_separately(cx: &mut gpui::TestAppContext) {
        let (_host, renderer) = test_renderer(cx);
        let formula = vec![r"\sum_{i=0}^n i".to_string()];
        render_pass(&renderer, &formula, false);
        render_pass(&renderer, &formula, true);
        cx.run_until_parked();
        let stats = renderer.stats();
        assert_eq!(stats.jobs_started, 2);
        assert_eq!(stats.cached_formulas, 2);
        let inline = renderer.render(&formula[0], false).and_then(Result::ok);
        let display = renderer.render(&formula[0], true).and_then(Result::ok);
        let (Some(inline), Some(display)) = (inline, display) else {
            panic!("both layouts should be ready");
        };
        assert_ne!(inline.metrics.height, display.metrics.height);
    }

    #[gpui::test]
    fn completed_batch_is_coalesced_into_few_notifications(cx: &mut gpui::TestAppContext) {
        let (_host, renderer) = test_renderer(cx);
        let formulas: Vec<String> = (0..200).map(synthetic_formula).collect();
        render_pass(&renderer, &formulas, false);
        cx.run_until_parked();
        let stats = renderer.stats();
        assert_eq!(stats.pending_formulas, 0);
        assert!(
            (1..=MAX_CONCURRENT_JOBS as u64).contains(&stats.notifications),
            "expected a coalesced notification, got {} for {} formulas",
            stats.notifications,
            formulas.len()
        );
    }

    #[gpui::test]
    fn completion_after_a_delivered_notification_notifies_again(cx: &mut gpui::TestAppContext) {
        let (_host, renderer) = test_renderer(cx);
        let first: Vec<String> = (0..20).map(synthetic_formula).collect();
        render_pass(&renderer, &first, false);
        cx.run_until_parked();
        let delivered = renderer.stats().notifications;
        assert!(delivered >= 1);

        render_pass(&renderer, &[synthetic_formula(1000)], false);
        cx.run_until_parked();
        assert_eq!(renderer.stats().notifications, delivered + 1);
        assert_eq!(renderer.stats().pending_formulas, 0);
    }

    #[gpui::test]
    fn dropping_the_view_stops_notifications(cx: &mut gpui::TestAppContext) {
        let (host, renderer) = test_renderer(cx);
        let formulas: Vec<String> = (0..20).map(synthetic_formula).collect();
        render_pass(&renderer, &formulas, false);
        drop(host);
        cx.run_until_parked();
        let stats = renderer.stats();
        assert_eq!(stats.notifications, 0);
        assert_eq!(stats.pending_formulas, 0);
    }

    #[test]
    fn parse_and_layout_simple() {
        let result = parse_and_layout("x^2", true);
        assert!(result.is_ok(), "{:?}", result.err());
    }

    #[test]
    fn parse_and_layout_fraction() {
        let result = parse_and_layout(r"\frac{a}{b}", true);
        assert!(result.is_ok(), "{:?}", result.err());
    }

    #[test]
    fn parse_and_layout_chemistry_preserved() {
        let result = parse_and_layout(r"\ce{H2SO4 + 2NaOH -> Na2SO4 + 2H2O}", true);
        assert!(result.is_ok(), "{:?}", result.err());
    }

    #[test]
    fn parse_and_layout_tag_preserved() {
        let result = parse_and_layout(r"\tag{1} x_1 + x_2 = y", true);
        assert!(result.is_ok(), "{:?}", result.err());
    }

    #[test]
    fn style_bit_patch_keeps_font_parseable_and_glyphs_stable() {
        let bytes = ratex_katex_fonts::ttf_bytes("KaTeX_Main-Italic.ttf")
            .expect("embedded Main-Italic available");
        let patched = apply_style_bits(&bytes, true, false).expect("patch applies");
        assert_ne!(bytes.as_ref(), patched.as_slice());

        let original = FontRef::try_from_slice(&bytes).expect("original parses");
        let patched_font = FontRef::try_from_slice(&patched).expect("patched parses");
        for ch in ['=', 'x', '4', 'a'] {
            assert_eq!(
                original.glyph_id(ch),
                patched_font.glyph_id(ch),
                "glyph id changed for {ch:?}"
            );
        }
    }

    #[test]
    fn style_bit_patch_disambiguates_faces_for_font_databases() {
        // KaTeX faces all ship with REGULAR style bits; fontdb (via ttf-parser)
        // must see the patched style or family matching stays order-dependent
        // and glyphs render shifted (`=` as `@`).
        for face in KATEX_FACES {
            let bytes = ratex_katex_fonts::ttf_bytes(face.filename).expect("face embedded");
            let registered = patched_font_bytes(face, bytes);
            let parsed = ttf_parser::Face::parse(&registered, 0).expect("face parses");
            assert_eq!(
                parsed.is_italic(),
                face.style == FontStyle::Italic,
                "italic bit wrong for {}",
                face.filename
            );
            assert_eq!(
                parsed.is_bold(),
                face.weight == FontWeight::BOLD,
                "bold bit wrong for {}",
                face.filename
            );
        }
    }

    #[test]
    fn norm_delimiter_bars_detected_as_hairline_strips() {
        // `\left\| ... \right\|` extensible bars are ~0.04em-wide filled path
        // subpaths. They must be classified as hairline strips so they paint as
        // snapped quads; tessellated they antialias into near-invisibility at
        // body sizes, depending on where the formula lands on the pixel grid.
        let result = parse_and_layout(r"\left\| \frac{\partial f}{\partial x} \right\|_2", true)
            .expect("layout");
        let bar_paths = result
            .display_list
            .items
            .iter()
            .filter(|item| matches!(item, DisplayItem::Path { fill: true, .. }))
            .count();
        assert_eq!(bar_paths, 2, "expected one path per delimiter");
    }

    #[test]
    fn loads_katex_main_regular() {
        let bytes = load_font_bytes(FontId::MainRegular);
        assert!(
            bytes.is_some(),
            "embed-fonts feature should make Main-Regular available"
        );
    }
}
