use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use editor::{EditorSettings, items::entry_git_aware_label_color};
use file_icons::FileIcons;
use gpui::FontWeight;
use gpui::{
    AnyElement, App, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ObjectFit, ParentElement, Pixels, Point, Render, RenderImage, StatefulInteractiveElement,
    Styled, Task, WeakEntity, Window, actions, div, img, prelude::*, px,
};

use language::File as _;
use project::{PdfItem, Project, ProjectPath, pdf_store::render_pdf_page_to_raw};
use settings::Settings;

use theme::{Theme, ThemeSettings};
use ui::prelude::*;
use util::paths::PathExt;
use workspace::{
    ItemId, ItemSettings, Pane, ToolbarItemLocation, Workspace, WorkspaceId, delete_unloaded_items,
    invalid_item_view::InvalidItemView,
    item::{BreadcrumbText, Item, ProjectItem, SerializableItem, TabContentParams},
};

use crate::persistence::PDF_VIEWER_DB;

#[derive(Clone)]
struct PdfPageContent {
    image: Arc<RenderImage>,
    #[allow(dead_code)]
    page_number: usize,
    #[allow(dead_code)]
    scale_factor: f32,
    height: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ViewMode {
    #[default]
    SinglePage,
    DualPage,
    ContinuousScroll,
}

impl ViewMode {
    fn label(&self) -> &'static str {
        match self {
            ViewMode::SinglePage => "Single Page",
            ViewMode::DualPage => "Dual Page",
            ViewMode::ContinuousScroll => "Continuous",
        }
    }
}

#[derive(Clone, Debug, Default)]
struct TextSelection {
    is_selecting: bool,
    start_position: Option<Point<Pixels>>,
    end_position: Option<Point<Pixels>>,
    selected_text: Option<String>,
}

actions!(
    pdf_viewer,
    [
        ScrollDown,
        ScrollUp,
        PageDown,
        PageUp,
        GoToFirstPage,
        GoToLastPage,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        NextPage,
        PreviousPage,
        ToggleViewMode,
        CopySelectedText,
        SelectAll,
    ]
);

pub struct PdfViewer {
    pdf_item: Entity<PdfItem>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    current_page: usize,
    zoom_level: f32,
    page_cache: HashMap<usize, PdfPageContent>,
    scroll_offset: f32,
    view_height: f32,
    view_mode: ViewMode,
    is_loading: bool,
    loading_pages: HashMap<usize, u64>,
    show_settings_menu: bool,
    text_selection: TextSelection,
    visible_pages: Vec<usize>,
    render_generation: u64,
}

const SCROLL_AMOUNT: f32 = 80.0;
const PAGE_SCROLL_AMOUNT: f32 = 500.0;
const MIN_ZOOM: f32 = 0.2;
const MAX_ZOOM: f32 = 5.0;
const PAGE_GAP: f32 = 20.0;
const ESTIMATED_PAGE_HEIGHT: f32 = 800.0;

impl PdfViewer {
    pub fn new(
        pdf_item: Entity<PdfItem>,
        project: Entity<Project>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.subscribe(&pdf_item, Self::on_pdf_event).detach();

        let mut viewer = Self {
            pdf_item,
            project,
            focus_handle: cx.focus_handle(),
            current_page: 0,
            zoom_level: 1.0,
            page_cache: HashMap::new(),
            scroll_offset: 0.0,
            view_height: 600.0,
            view_mode: ViewMode::default(),
            is_loading: true,
            loading_pages: HashMap::new(),
            show_settings_menu: false,
            text_selection: TextSelection::default(),
            visible_pages: vec![0],
            render_generation: 0,
        };

        // log::info!(
        //     "PdfViewer::new - initial load, generation: {}",
        //     viewer.render_generation
        // );
        viewer.load_visible_pages(cx);
        viewer
    }

    fn page_count(&self, cx: &Context<Self>) -> usize {
        self.pdf_item
            .read(cx)
            .metadata
            .as_ref()
            .map(|m| m.page_count)
            .unwrap_or(0)
    }

    fn get_page_height(&self, page: usize) -> f32 {
        if let Some(content) = self.page_cache.get(&page) {
            content.height
        } else {
            ESTIMATED_PAGE_HEIGHT * self.zoom_level
        }
    }

    fn calculate_visible_pages(&self, cx: &Context<Self>) -> Vec<usize> {
        let page_count = self.page_count(cx);
        if page_count == 0 {
            return vec![];
        }

        match self.view_mode {
            ViewMode::SinglePage => vec![self.current_page],
            ViewMode::DualPage => {
                let mut pages = vec![self.current_page];
                if self.current_page + 1 < page_count {
                    pages.push(self.current_page + 1);
                }
                pages
            }
            ViewMode::ContinuousScroll => {
                let mut pages = Vec::new();
                let mut y_offset = 0.0;
                let scroll = -self.scroll_offset;
                let view_top = scroll;
                let view_bottom = scroll + self.view_height + 200.0;

                for page in 0..page_count {
                    let page_height = self.get_page_height(page);
                    let page_top = y_offset;
                    let page_bottom = y_offset + page_height;

                    if page_bottom >= view_top && page_top <= view_bottom {
                        pages.push(page);
                    }

                    y_offset += page_height + PAGE_GAP;

                    if page_top > view_bottom {
                        break;
                    }
                }

                if pages.is_empty() && page_count > 0 {
                    pages.push(0);
                }

                pages
            }
        }
    }

    fn load_visible_pages(&mut self, cx: &mut Context<Self>) {
        let old_visible = self.visible_pages.clone();
        let new_visible = self.calculate_visible_pages(cx);

        let cache_has_all_visible =
            !new_visible.is_empty() && new_visible.iter().all(|p| self.page_cache.contains_key(p));

        if new_visible == old_visible && cache_has_all_visible {
            return;
        }

        self.visible_pages = new_visible.clone();

        // log::info!(
        //     "load_visible_pages: visible={:?}, cached={}, loading={}, generation={}",
        //     self.visible_pages,
        //     self.page_cache.len(),
        //     self.loading_pages.len(),
        //     self.render_generation
        // );

        let pages_to_load: Vec<usize> = self
            .visible_pages
            .iter()
            .filter(|&&page| {
                !self.page_cache.contains_key(&page) && !self.loading_pages.contains_key(&page)
            })
            .copied()
            .collect();

        // if !pages_to_load.is_empty() {
        //     // log::info!("Pages to load: {:?}", pages_to_load);
        // }

        // notify iff something changed
        let something_changed = !pages_to_load.is_empty() || new_visible != old_visible;

        for page in pages_to_load {
            self.load_page(page, cx);
        }

        self.preload_adjacent_pages(cx);
        self.update_loading_state();

        if something_changed {
            cx.notify();
        }
    }

    fn load_page(&mut self, page_number: usize, cx: &mut Context<Self>) {
        if self.loading_pages.contains_key(&page_number) {
            return;
        }

        let page_count = self.page_count(cx);
        if page_number >= page_count {
            return;
        }

        let generation = self.render_generation;
        // log::info!(
        //     "Starting to load page {} (generation {})",
        //     page_number,
        //     generation
        // );
        self.loading_pages.insert(page_number, generation);
        self.is_loading = true;

        let pdf_item = self.pdf_item.clone();
        let scale_factor = self.zoom_level;

        cx.spawn(async move |this, cx| {
            let bytes_result = cx.update(|cx| {
                let item = pdf_item.read(cx);
                item.metadata.as_ref().map(|m| m.bytes.clone())
            });

            let bytes = match bytes_result {
                Ok(Some(bytes)) => bytes,
                Ok(None) => {
                    // log::warn!("No PDF metadata available for page {}", page_number);
                    if let Err(e) = this.update(cx, |this, cx| {
                        if this.loading_pages.get(&page_number) == Some(&generation) {
                            this.loading_pages.remove(&page_number);
                        }
                        this.update_loading_state();
                        cx.notify();
                    }) {
                        // log::error!("Failed to update state after missing metadata: {e:?}");
                    }
                    return;
                }
                Err(e) => {
                    // log::error!("Failed to get PDF bytes for page {}: {e:?}", page_number);
                    if let Err(e) = this.update(cx, |this, cx| {
                        if this.loading_pages.get(&page_number) == Some(&generation) {
                            this.loading_pages.remove(&page_number);
                        }
                        this.update_loading_state();
                        cx.notify();
                    }) {
                        // log::error!("Failed to update state after error: {e:?}");
                    }
                    return;
                }
            };

            let render_result = cx
                .background_spawn({
                    let bytes = bytes.clone();
                    async move { render_pdf_page_to_raw(&bytes, page_number, scale_factor) }
                })
                .await;

            if let Err(e) = this.update(cx, |this, cx| {
                let current_generation = this.render_generation;

                if generation != current_generation {
                    // log::info!(
                    //     "Discarding stale render for page {} (generation {} vs current {})",
                    //     page_number,
                    //     generation,
                    //     current_generation
                    // );
                    if this.loading_pages.get(&page_number) == Some(&generation) {
                        this.loading_pages.remove(&page_number);
                    }
                    this.update_loading_state();
                    cx.notify();
                    return;
                }

                this.loading_pages.remove(&page_number);

                match render_result {
                    Ok(rendered) => {
                        // log::info!(
                        //     "Successfully rendered page {} ({}x{}, generation {})",
                        //     page_number,
                        //     rendered.width,
                        //     rendered.height,
                        //     generation
                        // );
                        let height = rendered.height as f32;
                        let render_image = Arc::new(RenderImage::new(rendered.data));
                        let content = PdfPageContent {
                            image: render_image,
                            page_number,
                            scale_factor,
                            height,
                        };
                        this.page_cache.insert(page_number, content);
                    }
                    Err(e) => {
                        // log::error!("Failed to render PDF page {}: {e:?}", page_number);
                    }
                }

                this.update_loading_state();
                cx.notify();
            }) {
                // log::error!(
                //     "Failed to update view after rendering page {}: {e:?}",
                //     page_number
                // );
            }
        })
        .detach();
    }

    fn update_loading_state(&mut self) {
        self.is_loading = !self.loading_pages.is_empty()
            || self
                .visible_pages
                .iter()
                .any(|p| !self.page_cache.contains_key(p));
    }

    fn preload_adjacent_pages(&mut self, cx: &mut Context<Self>) {
        let page_count = self.page_count(cx);
        if page_count == 0 {
            return;
        }

        let mut pages_to_preload: Vec<usize> = Vec::new();

        match self.view_mode {
            ViewMode::SinglePage => {
                if self.current_page > 0 {
                    pages_to_preload.push(self.current_page - 1);
                }
                if self.current_page + 1 < page_count {
                    pages_to_preload.push(self.current_page + 1);
                }
            }
            ViewMode::DualPage => {
                if self.current_page >= 2 {
                    pages_to_preload.push(self.current_page - 2);
                    pages_to_preload.push(self.current_page - 1);
                } else if self.current_page >= 1 {
                    pages_to_preload.push(self.current_page - 1);
                }
                if self.current_page + 2 < page_count {
                    pages_to_preload.push(self.current_page + 2);
                }
                if self.current_page + 3 < page_count {
                    pages_to_preload.push(self.current_page + 3);
                }
            }
            ViewMode::ContinuousScroll => {
                if let Some(&first) = self.visible_pages.first() {
                    if first > 0 {
                        pages_to_preload.push(first - 1);
                    }
                    if first > 1 {
                        pages_to_preload.push(first - 2);
                    }
                }
                if let Some(&last) = self.visible_pages.last() {
                    if last + 1 < page_count {
                        pages_to_preload.push(last + 1);
                    }
                    if last + 2 < page_count {
                        pages_to_preload.push(last + 2);
                    }
                }
            }
        }

        for page in pages_to_preload {
            if !self.page_cache.contains_key(&page) && !self.loading_pages.contains_key(&page) {
                self.load_page(page, cx);
            }
        }
    }

    fn clear_cache_and_reload(&mut self, cx: &mut Context<Self>) {
        self.render_generation = self.render_generation.wrapping_add(1);
        // log::info!("Clearing cache, new generation: {}", self.render_generation);
        self.page_cache.clear();
        self.loading_pages.clear();
        self.visible_pages.clear();
        self.is_loading = true;
        self.load_visible_pages(cx);
    }

    fn zoom_in(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        // log::info!("zoom_in called, current zoom: {}", self.zoom_level);
        self.zoom_level = (self.zoom_level * 1.2).min(MAX_ZOOM);
        self.clear_cache_and_reload(cx);
    }

    fn zoom_out(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        // log::info!("zoom_out called, current zoom: {}", self.zoom_level);
        self.zoom_level = (self.zoom_level / 1.2).max(MIN_ZOOM);
        self.clear_cache_and_reload(cx);
    }

    fn zoom_reset(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.zoom_level = 1.0;
        self.clear_cache_and_reload(cx);
    }

    fn next_page(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let page_count = self.page_count(cx);
        if page_count == 0 {
            return;
        }

        let increment = if self.view_mode == ViewMode::DualPage {
            2
        } else {
            1
        };

        if self.current_page + increment < page_count {
            self.current_page += increment;
            self.scroll_offset = 0.0;
            self.load_visible_pages(cx);
        }
    }

    fn previous_page(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let decrement = if self.view_mode == ViewMode::DualPage {
            2
        } else {
            1
        };

        if self.current_page >= decrement {
            self.current_page -= decrement;
            self.scroll_offset = 0.0;
            self.load_visible_pages(cx);
        } else if self.current_page > 0 {
            self.current_page = 0;
            self.scroll_offset = 0.0;
            self.load_visible_pages(cx);
        }
    }

    fn scroll_down(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        // log::info!("scroll_down called, scroll_offset: {}", self.scroll_offset);
        self.scroll_offset -= SCROLL_AMOUNT;
        self.clamp_scroll(cx);
        // Notify for immediate visual update
        cx.notify();
        // Only check for new pages after scrolling significantly
        if self.scroll_offset < -500.0 {
            self.load_visible_pages(cx);
        }
    }

    fn scroll_up(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        // log::info!("scroll_up called, scroll_offset: {}", self.scroll_offset);
        self.scroll_offset += SCROLL_AMOUNT;
        if self.scroll_offset > 0.0 {
            self.scroll_offset = 0.0;
        }
        self.clamp_scroll(cx);
        // Notify for immediate visual update
        cx.notify();
        // Don't trigger load_pages on scroll_up since we're going backwards
    }

    fn clamp_scroll(&mut self, cx: &Context<Self>) {
        match self.view_mode {
            ViewMode::SinglePage | ViewMode::DualPage => {
                let page_height = self.get_page_height(self.current_page);
                let min_scroll = -(page_height - self.view_height).max(0.0);
                self.scroll_offset = self.scroll_offset.clamp(min_scroll, 0.0);
            }
            ViewMode::ContinuousScroll => {
                let total_height = self.calculate_total_height(cx);
                let min_scroll = -(total_height - self.view_height).max(0.0);
                self.scroll_offset = self.scroll_offset.clamp(min_scroll, 0.0);
            }
        }
    }

    fn calculate_total_height(&self, cx: &Context<Self>) -> f32 {
        let page_count = self.page_count(cx);
        let mut total = 0.0;
        for page in 0..page_count {
            total += self.get_page_height(page) + PAGE_GAP;
        }
        total - PAGE_GAP
    }

    fn page_down(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.view_mode == ViewMode::ContinuousScroll {
            self.scroll_offset -= PAGE_SCROLL_AMOUNT;
            self.clamp_scroll(cx);
            self.load_visible_pages(cx);
        } else {
            self.next_page(window, cx);
        }
    }

    fn page_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.view_mode == ViewMode::ContinuousScroll {
            self.scroll_offset += PAGE_SCROLL_AMOUNT;
            if self.scroll_offset > 0.0 {
                self.scroll_offset = 0.0;
            }
            self.load_visible_pages(cx);
        } else {
            self.previous_page(window, cx);
        }
    }

    fn go_to_first_page(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.current_page != 0 || self.scroll_offset != 0.0 {
            self.current_page = 0;
            self.scroll_offset = 0.0;
            self.load_visible_pages(cx);
        }
    }

    fn go_to_last_page(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let page_count = self.page_count(cx);
        if page_count == 0 {
            return;
        }

        if self.view_mode == ViewMode::ContinuousScroll {
            // In continuous mode, scroll to the end of the document
            let total_height = self.calculate_total_height(cx);
            self.scroll_offset = -(total_height - self.view_height).max(0.0);
            self.load_visible_pages(cx);
        } else {
            // In single/dual page mode, jump to the last page
            let last_page = page_count.saturating_sub(1);
            if self.current_page != last_page {
                self.current_page = last_page;
                self.scroll_offset = 0.0;
                self.load_visible_pages(cx);
            }
        }
        cx.notify();
    }

    fn set_view_mode(&mut self, mode: ViewMode, cx: &mut Context<Self>) {
        if self.view_mode != mode {
            self.view_mode = mode;
            self.scroll_offset = 0.0;
            self.loading_pages.clear();
            self.load_visible_pages(cx);
        }
    }

    fn toggle_settings_menu(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.show_settings_menu = !self.show_settings_menu;
        cx.notify();
    }

    fn start_text_selection(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        self.text_selection = TextSelection {
            is_selecting: true,
            start_position: Some(position),
            end_position: Some(position),
            selected_text: None,
        };
        cx.notify();
    }

    fn update_text_selection(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if self.text_selection.is_selecting {
            self.text_selection.end_position = Some(position);
            cx.notify();
        }
    }

    fn end_text_selection(&mut self, cx: &mut Context<Self>) {
        if self.text_selection.is_selecting {
            self.text_selection.is_selecting = false;

            if let (Some(start), Some(end)) = (
                self.text_selection.start_position,
                self.text_selection.end_position,
            ) {
                let dx: f32 = (end.x - start.x).into();
                let dy: f32 = (end.y - start.y).into();
                let distance = (dx.powi(2) + dy.powi(2)).sqrt();
                if distance > 5.0 {
                    self.extract_selected_text(cx);
                } else {
                    self.text_selection = TextSelection::default();
                }
            }
            cx.notify();
        }
    }

    fn extract_selected_text(&mut self, cx: &mut Context<Self>) {
        let bytes = self
            .pdf_item
            .read(cx)
            .metadata
            .as_ref()
            .map(|m| m.bytes.clone());

        if let Some(bytes) = bytes {
            let page_number = self.current_page;
            cx.spawn(async move |this, cx| {
                let text = cx
                    .background_spawn(async move {
                        project::pdf_store::extract_page_text(&bytes, page_number).ok()
                    })
                    .await;

                let _ = this.update(cx, |this, cx| {
                    if let Some(text) = text {
                        this.text_selection.selected_text = Some(text);
                    }
                    cx.notify();
                });
            })
            .detach();
        }
    }

    fn copy_selected_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = &self.text_selection.selected_text {
            cx.write_to_clipboard(gpui::ClipboardItem::new_string(text.clone()));
        }
    }

    fn on_pdf_event(&mut self, _: Entity<PdfItem>, event: &PdfItemEvent, cx: &mut Context<Self>) {
        match event {
            PdfItemEvent::FileChanged => {
                cx.emit(PdfViewerEvent::TitleChanged);
                cx.notify();
            }
        }
    }

    fn render_loading_indicator(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let loading_count = self.loading_pages.len();
        let visible_count = self.visible_pages.len();
        let cached_count = self
            .visible_pages
            .iter()
            .filter(|p| self.page_cache.contains_key(p))
            .count();

        let progress = if visible_count > 0 {
            cached_count as f32 / visible_count as f32
        } else {
            0.0
        };

        let seems_stuck = loading_count == 0 && cached_count < visible_count;

        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .h_full()
            .gap_4()
            .child(
                div()
                    .w(px(200.0))
                    .h(px(4.0))
                    .rounded_full()
                    .bg(cx.theme().colors().border)
                    .child(
                        div()
                            .h_full()
                            .rounded_full()
                            .bg(cx.theme().colors().text_accent)
                            .w(px(200.0 * progress)),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().colors().text_muted)
                    .child(if seems_stuck {
                        format!("Loading stalled ({}/{} pages)", cached_count, visible_count)
                    } else if loading_count > 0 {
                        format!("Loading pages... ({} in progress)", loading_count)
                    } else {
                        "Preparing...".to_string()
                    }),
            )
            .when(seems_stuck, |this| {
                this.child(
                    Button::new("retry-all", "Retry Loading").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.clear_cache_and_reload(cx);
                        },
                    )),
                )
            })
    }

    fn render_settings_menu(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current_mode = self.view_mode;

        div()
            .absolute()
            .right(px(8.0))
            .top(px(44.0))
            .w(px(180.0))
            .bg(cx.theme().colors().elevated_surface_background)
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_md()
            .shadow_md()
            .p_1()
            .flex()
            .flex_col()
            .gap_0p5()
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors().text_muted)
                    .px_2()
                    .py_1()
                    .child("View Mode"),
            )
            .child(self.render_menu_item(
                "Single Page",
                current_mode == ViewMode::SinglePage,
                ViewMode::SinglePage,
                cx,
            ))
            .child(self.render_menu_item(
                "Dual Page",
                current_mode == ViewMode::DualPage,
                ViewMode::DualPage,
                cx,
            ))
            .child(self.render_menu_item(
                "Continuous",
                current_mode == ViewMode::ContinuousScroll,
                ViewMode::ContinuousScroll,
                cx,
            ))
    }

    fn render_menu_item(
        &self,
        label: &'static str,
        is_selected: bool,
        mode: ViewMode,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected_bg = cx.theme().colors().ghost_element_selected;
        let hover_bg = cx.theme().colors().ghost_element_hover;
        let accent = cx.theme().colors().text_accent;

        div()
            .id(label)
            .px_2()
            .py_1()
            .rounded_sm()
            .cursor_pointer()
            .when(is_selected, |this| this.bg(selected_bg))
            .hover(|this| this.bg(hover_bg))
            .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                this.set_view_mode(mode, cx);
                this.show_settings_menu = false;
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_sm().child(label))
                    .when(is_selected, |this| {
                        this.child(div().text_sm().text_color(accent).child("*"))
                    }),
            )
    }

    fn render_page(&self, page_number: usize, cx: &mut Context<Self>) -> AnyElement {
        if let Some(content) = self.page_cache.get(&page_number) {
            div()
                .flex()
                .flex_col()
                .items_center()
                .child(
                    img(content.image.clone())
                        .object_fit(ObjectFit::Contain)
                        .max_w_full(),
                )
                .into_any_element()
        } else {
            let is_loading = self.loading_pages.contains_key(&page_number);
            let bg_color = cx.theme().colors().surface_background;
            let text_color = cx.theme().colors().text_muted;
            let border_color = cx.theme().colors().border;

            div()
                .id(("page-placeholder", page_number))
                .w(px(600.0 * self.zoom_level))
                .h(px(ESTIMATED_PAGE_HEIGHT * self.zoom_level))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .bg(bg_color)
                .border_1()
                .border_color(border_color)
                .rounded_md()
                .child(div().text_sm().text_color(text_color).child(if is_loading {
                    format!("Loading page {}...", page_number + 1)
                } else {
                    format!("Page {} not loaded", page_number + 1)
                }))
                .when(!is_loading, |this| {
                    this.child(
                        Button::new(("retry", page_number), "Retry").on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.load_page(page_number, cx);
                            },
                        )),
                    )
                })
                .into_any_element()
        }
    }

    fn render_single_page_content(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap_2()
            .mt(px(self.scroll_offset))
            .child(self.render_page(self.current_page, cx))
            .into_any_element()
    }

    fn render_dual_page_content(&self, cx: &mut Context<Self>) -> AnyElement {
        let page_count = self.page_count(cx);

        div()
            .flex()
            .flex_row()
            .items_start()
            .justify_center()
            .gap_4()
            .mt(px(self.scroll_offset))
            .child(self.render_page(self.current_page, cx))
            .when(self.current_page + 1 < page_count, |this| {
                this.child(self.render_page(self.current_page + 1, cx))
            })
            .into_any_element()
    }

    fn render_continuous_content(&self, cx: &mut Context<Self>) -> AnyElement {
        let page_count = self.page_count(cx);
        let scroll = -self.scroll_offset;
        let view_top = scroll;
        let view_bottom = scroll + self.view_height + 400.0;

        let mut elements: Vec<AnyElement> = Vec::new();
        let mut y_offset = 0.0;

        for page in 0..page_count {
            let page_height = self.get_page_height(page);
            let page_top = y_offset;
            let page_bottom = y_offset + page_height;

            if page_bottom >= view_top - 200.0 && page_top <= view_bottom {
                elements.push(
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .w_full()
                        .pb(px(PAGE_GAP))
                        .child(self.render_page(page, cx))
                        .into_any_element(),
                );
            } else if page_top > view_bottom {
                break;
            } else {
                elements.push(
                    div()
                        .h(px(page_height + PAGE_GAP))
                        .w_full()
                        .into_any_element(),
                );
            }

            y_offset += page_height + PAGE_GAP;
        }

        div()
            .flex()
            .flex_col()
            .items_center()
            .w_full()
            .mt(px(self.scroll_offset))
            .children(elements)
            .into_any_element()
    }
}

pub enum PdfItemEvent {
    FileChanged,
}

impl EventEmitter<PdfItemEvent> for PdfItem {}

pub enum PdfViewerEvent {
    TitleChanged,
}

impl EventEmitter<PdfViewerEvent> for PdfViewer {}

impl Item for PdfViewer {
    type Event = PdfViewerEvent;

    fn to_item_events(event: &Self::Event, mut f: impl FnMut(workspace::item::ItemEvent)) {
        match event {
            PdfViewerEvent::TitleChanged => {
                f(workspace::item::ItemEvent::UpdateTab);
                f(workspace::item::ItemEvent::UpdateBreadcrumbs);
            }
        }
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        f(self.pdf_item.entity_id(), self.pdf_item.read(cx))
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        let abs_path = self.pdf_item.read(cx).abs_path(cx)?;
        let file_path = abs_path.compact().to_string_lossy().into_owned();
        Some(file_path.into())
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        let project_path = self.pdf_item.read(cx).project_path(cx);

        let label_color = if ItemSettings::get_global(cx).git_status {
            let git_status = self
                .project
                .read(cx)
                .project_path_git_status(&project_path, cx)
                .map(|status| status.summary())
                .unwrap_or_default();

            self.project
                .read(cx)
                .entry_for_path(&project_path, cx)
                .map(|entry| {
                    entry_git_aware_label_color(git_status, entry.is_ignored, params.selected)
                })
                .unwrap_or_else(|| params.text_color())
        } else {
            params.text_color()
        };

        Label::new(self.tab_content_text(params.detail.unwrap_or_default(), cx))
            .single_line()
            .color(label_color)
            .when(params.preview, |this| this.italic())
            .into_any_element()
    }

    fn tab_content_text(&self, _: usize, cx: &App) -> SharedString {
        self.pdf_item.read(cx).file.file_name(cx).to_string().into()
    }

    fn tab_icon(&self, _: &Window, cx: &App) -> Option<Icon> {
        let path = self.pdf_item.read(cx).abs_path(cx)?;
        ItemSettings::get_global(cx)
            .file_icons
            .then(|| FileIcons::get_icon(&path, cx))
            .flatten()
            .map(Icon::from_path)
    }

    fn breadcrumb_location(&self, cx: &App) -> ToolbarItemLocation {
        let show_breadcrumb = EditorSettings::get_global(cx).toolbar.breadcrumbs;
        if show_breadcrumb {
            ToolbarItemLocation::PrimaryLeft
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    fn breadcrumbs(&self, _theme: &Theme, cx: &App) -> Option<Vec<BreadcrumbText>> {
        let text = breadcrumbs_text_for_pdf(self.project.read(cx), self.pdf_item.read(cx), cx);
        let settings = ThemeSettings::get_global(cx);

        Some(vec![BreadcrumbText {
            text,
            highlights: None,
            font: Some(settings.buffer_font.clone()),
        }])
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<WorkspaceId>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        Task::ready(Some(cx.new(|cx| Self {
            pdf_item: self.pdf_item.clone(),
            project: self.project.clone(),
            focus_handle: cx.focus_handle(),
            current_page: self.current_page,
            zoom_level: self.zoom_level,
            page_cache: self.page_cache.clone(),
            scroll_offset: self.scroll_offset,
            view_height: self.view_height,
            view_mode: self.view_mode,
            is_loading: false,
            loading_pages: HashMap::new(),
            show_settings_menu: false,
            text_selection: TextSelection::default(),
            visible_pages: self.visible_pages.clone(),
            render_generation: self.render_generation,
        })))
    }

    fn has_deleted_file(&self, cx: &App) -> bool {
        self.pdf_item.read(cx).file.disk_state().is_deleted()
    }

    fn buffer_kind(&self, _: &App) -> workspace::item::ItemBufferKind {
        workspace::item::ItemBufferKind::Singleton
    }
}

fn breadcrumbs_text_for_pdf(project: &Project, pdf: &PdfItem, cx: &App) -> String {
    let mut path = pdf.file.path().clone();
    if project.visible_worktrees(cx).count() > 1
        && let Some(worktree) = project.worktree_for_id(pdf.project_path(cx).worktree_id, cx)
    {
        path = worktree.read(cx).root_name().join(&path);
    }

    path.display(project.path_style(cx)).to_string()
}

impl SerializableItem for PdfViewer {
    fn serialized_item_kind() -> &'static str {
        "PdfViewer"
    }

    fn deserialize(
        project: Entity<Project>,
        _workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        window.spawn(cx, async move |cx| {
            let pdf_path = PDF_VIEWER_DB
                .get_pdf_path(item_id, workspace_id)?
                .context("No pdf path found")?;

            let (worktree, relative_path) = project
                .update(cx, |project, cx| {
                    project.find_or_create_worktree(pdf_path.clone(), false, cx)
                })?
                .await
                .context("Path not found")?;
            let worktree_id = worktree.update(cx, |worktree, _cx| worktree.id())?;

            let project_path = ProjectPath {
                worktree_id,
                path: relative_path,
            };

            let pdf_item = project
                .update(cx, |project, cx| project.open_pdf(project_path, cx))?
                .await?;

            cx.update(|window, cx| Ok(cx.new(|cx| PdfViewer::new(pdf_item, project, window, cx))))?
        })
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<()>> {
        delete_unloaded_items(alive_items, workspace_id, "pdf_viewers", &PDF_VIEWER_DB, cx)
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _closing: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Task<anyhow::Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let pdf_path = self.pdf_item.read(cx).abs_path(cx)?;

        Some(cx.background_spawn({
            async move {
                log::debug!("Saving pdf at path {pdf_path:?}");
                PDF_VIEWER_DB
                    .save_pdf_path(item_id, workspace_id, pdf_path)
                    .await
            }
        }))
    }

    fn should_serialize(&self, _event: &Self::Event) -> bool {
        false
    }
}

impl EventEmitter<()> for PdfViewer {}
impl Focusable for PdfViewer {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PdfViewer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let page_count = self.page_count(cx);
        let file_name = self.pdf_item.read(cx).file.file_name(cx).to_string();

        let has_cached_content = !self.page_cache.is_empty();
        let is_loading = self.is_loading;
        let show_settings = self.show_settings_menu;

        let page_info_str = match self.view_mode {
            ViewMode::DualPage => {
                let end_page = (self.current_page + 2).min(page_count);
                format!(
                    "Page {}-{} of {} | Zoom: {:.0}% | {}",
                    self.current_page + 1,
                    end_page,
                    page_count,
                    self.zoom_level * 100.0,
                    self.view_mode.label()
                )
            }
            ViewMode::ContinuousScroll => {
                let first_visible = self.visible_pages.first().copied().unwrap_or(0);
                format!(
                    "Page {} of {} | Zoom: {:.0}% | {}",
                    first_visible + 1,
                    page_count,
                    self.zoom_level * 100.0,
                    self.view_mode.label()
                )
            }
            ViewMode::SinglePage => {
                format!(
                    "Page {} of {} | Zoom: {:.0}% | {}",
                    self.current_page + 1,
                    page_count,
                    self.zoom_level * 100.0,
                    self.view_mode.label()
                )
            }
        };

        let content_area = if is_loading && !has_cached_content {
            self.render_loading_indicator(cx).into_any_element()
        } else {
            match self.view_mode {
                ViewMode::SinglePage => self.render_single_page_content(cx),
                ViewMode::DualPage => self.render_dual_page_content(cx),
                ViewMode::ContinuousScroll => self.render_continuous_content(cx),
            }
        };

        div()
            .key_context("PdfViewer")
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().colors().background)
            .on_action(cx.listener(|this, _: &ScrollDown, window, cx| {
                // log::info!("ScrollDown action received");
                this.scroll_down(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ScrollUp, window, cx| {
                log::info!("ScrollUp action received");
                this.scroll_up(window, cx);
            }))
            .on_action(cx.listener(|this, _: &PageDown, window, cx| {
                this.page_down(window, cx);
            }))
            .on_action(cx.listener(|this, _: &PageUp, window, cx| {
                this.page_up(window, cx);
            }))
            .on_action(cx.listener(|this, _: &GoToFirstPage, window, cx| {
                this.go_to_first_page(window, cx);
            }))
            .on_action(cx.listener(|this, _: &GoToLastPage, window, cx| {
                this.go_to_last_page(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ZoomIn, window, cx| {
                this.zoom_in(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ZoomOut, window, cx| {
                this.zoom_out(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ZoomReset, window, cx| {
                this.zoom_reset(window, cx);
            }))
            .on_action(cx.listener(|this, _: &NextPage, window, cx| {
                this.next_page(window, cx);
            }))
            .on_action(cx.listener(|this, _: &PreviousPage, window, cx| {
                this.previous_page(window, cx);
            }))
            .on_action(cx.listener(|this, _: &CopySelectedText, window, cx| {
                this.copy_selected_text(window, cx);
            }))
            .on_scroll_wheel(
                cx.listener(|this, event: &gpui::ScrollWheelEvent, _window, cx| {
                    let (_, _, dy) = match event.delta {
                        gpui::ScrollDelta::Pixels(p) => ("pixels", f32::from(p.x), f32::from(p.y)),
                        gpui::ScrollDelta::Lines(l) => ("lines", l.x * 20.0, l.y * 20.0),
                    };

                    if event.control || event.platform {
                        let zoom_delta = dy * 0.003;
                        let new_zoom = (this.zoom_level - zoom_delta).clamp(MIN_ZOOM, MAX_ZOOM);

                        if (new_zoom - this.zoom_level).abs() > 0.05 {
                            log::info!(
                                "Scroll-zoom triggered: {} -> {} (delta {})",
                                this.zoom_level,
                                new_zoom,
                                zoom_delta
                            );
                            this.zoom_level = new_zoom;
                            this.clear_cache_and_reload(cx);
                        }
                    } else {
                        let old_offset = this.scroll_offset;
                        this.scroll_offset += dy;
                        if this.scroll_offset > 0.0 {
                            this.scroll_offset = 0.0;
                        }
                        this.clamp_scroll(cx);

                        // Always notify for smooth scrolling
                        cx.notify();

                        // Only check for new pages if we scrolled significantly
                        if (this.scroll_offset - old_offset).abs() > 50.0 {
                            this.load_visible_pages(cx);
                        }
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.focus_handle.focus(window, cx);
                    this.start_text_selection(event.position, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                if this.text_selection.is_selecting {
                    this.update_text_selection(event.position, cx);
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _event: &MouseUpEvent, _window, cx| {
                    this.end_text_selection(cx);
                }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .p_2()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(file_name),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().colors().text_muted)
                                    .child(page_info_str),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(Button::new("zoom-out", "-").on_click(cx.listener(
                                |this, _event, window, cx| {
                                    this.zoom_out(window, cx);
                                },
                            )))
                            .child(Button::new("zoom-reset", "Reset").on_click(cx.listener(
                                |this, _event, window, cx| {
                                    this.zoom_reset(window, cx);
                                },
                            )))
                            .child(Button::new("zoom-in", "+").on_click(cx.listener(
                                |this, _event, window, cx| {
                                    this.zoom_in(window, cx);
                                },
                            )))
                            .child(div().h_6().w_px().bg(cx.theme().colors().border))
                            .child(
                                Button::new("previous-page", "<")
                                    .disabled(self.current_page == 0)
                                    .on_click(cx.listener(|this, _event, window, cx| {
                                        this.previous_page(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("next-page", ">")
                                    .disabled(
                                        page_count == 0 || self.current_page + 1 >= page_count,
                                    )
                                    .on_click(cx.listener(|this, _event, window, cx| {
                                        this.next_page(window, cx);
                                    })),
                            )
                            .child(div().h_6().w_px().bg(cx.theme().colors().border))
                            .child(Button::new("settings", "...").on_click(cx.listener(
                                |this, _event, window, cx| {
                                    this.toggle_settings_menu(window, cx);
                                },
                            ))),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .relative()
                    .child(content_area)
                    .when(is_loading && has_cached_content, |this| {
                        this.child(
                            div()
                                .absolute()
                                .top_2()
                                .right_2()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .bg(cx.theme().colors().elevated_surface_background)
                                .border_1()
                                .border_color(cx.theme().colors().border)
                                .text_xs()
                                .text_color(cx.theme().colors().text_muted)
                                .child(format!("Loading {} pages...", self.loading_pages.len())),
                        )
                    })
                    .when(show_settings, |this| {
                        this.child(self.render_settings_menu(cx))
                    }),
            )
    }
}

impl ProjectItem for PdfViewer {
    type Item = PdfItem;

    fn for_project_item(
        project: Entity<Project>,
        _: Option<&Pane>,
        item: Entity<Self::Item>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self
    where
        Self: Sized,
    {
        Self::new(item, project, window, cx)
    }

    fn for_broken_project_item(
        abs_path: &Path,
        is_local: bool,
        e: &anyhow::Error,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<InvalidItemView>
    where
        Self: Sized,
    {
        Some(InvalidItemView::new(abs_path, is_local, e, window, cx))
    }
}

pub fn init(cx: &mut App) {
    workspace::register_project_item::<PdfViewer>(cx);
    workspace::register_serializable_item::<PdfViewer>(cx);
}

mod persistence {
    use std::path::PathBuf;

    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use workspace::{ItemId, WorkspaceDb, WorkspaceId};

    pub struct PdfViewerDb(ThreadSafeConnection);

    impl Domain for PdfViewerDb {
        const NAME: &str = stringify!(PdfViewerDb);

        const MIGRATIONS: &[&str] = &[sql!(
                CREATE TABLE pdf_viewers (
                    workspace_id INTEGER,
                    item_id INTEGER UNIQUE,

                    pdf_path BLOB,

                    PRIMARY KEY(workspace_id, item_id),
                    FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                    ON DELETE CASCADE
                ) STRICT;
        )];
    }

    db::static_connection!(PDF_VIEWER_DB, PdfViewerDb, [WorkspaceDb]);

    impl PdfViewerDb {
        query! {
            pub async fn save_pdf_path(
                item_id: ItemId,
                workspace_id: WorkspaceId,
                pdf_path: PathBuf
            ) -> Result<()> {
                INSERT OR REPLACE INTO pdf_viewers(item_id, workspace_id, pdf_path)
                VALUES (?, ?, ?)
            }
        }

        query! {
            pub fn get_pdf_path(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<PathBuf>> {
                SELECT pdf_path
                FROM pdf_viewers
                WHERE item_id = ? AND workspace_id = ?
            }
        }
    }
}
