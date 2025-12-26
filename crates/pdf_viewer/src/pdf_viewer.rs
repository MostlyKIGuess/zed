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
    page_number: usize,
    scale_factor: f32,
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
    page_content: Option<PdfPageContent>,
    secondary_page_content: Option<PdfPageContent>,
    load_task: Option<Task<()>>,
    zoom_level: f32,
    page_cache: HashMap<usize, PdfPageContent>,
    scroll_offset: f32,
    view_height: f32,
    view_mode: ViewMode,
    is_loading: bool,
    loading_progress: f32,
    show_settings_menu: bool,
    text_selection: TextSelection,
}

const SCROLL_AMOUNT: f32 = 50.0;
const PAGE_SCROLL_AMOUNT: f32 = 400.0;
const MIN_ZOOM: f32 = 0.2;
const MAX_ZOOM: f32 = 5.0;

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
            page_content: None,
            secondary_page_content: None,
            load_task: None,
            zoom_level: 1.0,
            page_cache: HashMap::new(),
            scroll_offset: 0.0,
            view_height: 600.0,
            view_mode: ViewMode::default(),
            is_loading: true,
            loading_progress: 0.0,
            show_settings_menu: false,
            text_selection: TextSelection::default(),
        };

        viewer.load_current_page(cx);
        viewer
    }

    fn load_current_page(&mut self, cx: &mut Context<Self>) {
        let scale_factor = self.zoom_level;
        self.is_loading = true;
        self.loading_progress = 0.0;
        cx.notify();

        if let Some(cached) = self.page_cache.get(&self.current_page) {
            if (cached.scale_factor - scale_factor).abs() < 0.01 {
                self.page_content = Some(cached.clone());
                self.is_loading = false;

                if self.view_mode == ViewMode::DualPage {
                    self.load_secondary_page(cx);
                } else {
                    self.secondary_page_content = None;
                    cx.notify();
                }
                return;
            }
        }

        let pdf_item = self.pdf_item.clone();
        let page_number = self.current_page;
        let view_mode = self.view_mode;

        self.load_task = Some(cx.spawn(async move |this, cx| {
            let bytes = cx.update(|cx| {
                let item = pdf_item.read(cx);
                item.metadata.as_ref().map(|m| m.bytes.clone())
            });

            let bytes = match bytes {
                Ok(Some(bytes)) => bytes,
                _ => return,
            };

            let _ = this.update(cx, |this, cx| {
                this.loading_progress = 0.3;
                cx.notify();
            });

            let page_image = cx
                .background_spawn({
                    let bytes = bytes.clone();
                    async move { render_pdf_page_to_raw(&bytes, page_number, scale_factor) }
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                this.loading_progress = 0.8;
                cx.notify();
            });

            let _ = this.update(cx, |this, cx| {
                match page_image {
                    Ok(rendered) => {
                        let render_image = Arc::new(RenderImage::new(rendered.data));
                        let content = PdfPageContent {
                            image: render_image,
                            page_number,
                            scale_factor,
                        };
                        this.page_cache.insert(page_number, content.clone());
                        this.page_content = Some(content);
                        this.is_loading = false;
                        this.loading_progress = 1.0;

                        if view_mode == ViewMode::DualPage {
                            this.load_secondary_page(cx);
                        } else {
                            this.secondary_page_content = None;
                        }
                    }
                    Err(e) => {
                        log::error!("Failed to render PDF page: {e:?}");
                        this.is_loading = false;
                    }
                }
                cx.notify();
            });
        }));
    }

    fn load_secondary_page(&mut self, cx: &mut Context<Self>) {
        let page_count = self
            .pdf_item
            .read(cx)
            .metadata
            .as_ref()
            .map(|m| m.page_count)
            .unwrap_or(0);

        let secondary_page = self.current_page + 1;
        if secondary_page >= page_count {
            self.secondary_page_content = None;
            cx.notify();
            return;
        }

        let scale_factor = self.zoom_level;

        if let Some(cached) = self.page_cache.get(&secondary_page) {
            if (cached.scale_factor - scale_factor).abs() < 0.01 {
                self.secondary_page_content = Some(cached.clone());
                cx.notify();
                return;
            }
        }

        let pdf_item = self.pdf_item.clone();

        cx.spawn(async move |this, cx| {
            let bytes = cx.update(|cx| {
                let item = pdf_item.read(cx);
                item.metadata.as_ref().map(|m| m.bytes.clone())
            });

            let bytes = match bytes {
                Ok(Some(bytes)) => bytes,
                _ => return,
            };

            let page_image = cx
                .background_spawn({
                    let bytes = bytes.clone();
                    async move { render_pdf_page_to_raw(&bytes, secondary_page, scale_factor) }
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                if let Ok(rendered) = page_image {
                    let render_image = Arc::new(RenderImage::new(rendered.data));
                    let content = PdfPageContent {
                        image: render_image,
                        page_number: secondary_page,
                        scale_factor,
                    };
                    this.page_cache.insert(secondary_page, content.clone());
                    this.secondary_page_content = Some(content);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn zoom_in(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.zoom_level = (self.zoom_level * 1.2).min(MAX_ZOOM);
        self.page_cache.clear();
        self.load_current_page(cx);
    }

    fn zoom_out(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.zoom_level = (self.zoom_level / 1.2).max(MIN_ZOOM);
        self.page_cache.clear();
        self.load_current_page(cx);
    }

    fn zoom_reset(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.zoom_level = 1.0;
        self.page_cache.clear();
        self.load_current_page(cx);
    }

    fn next_page(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(metadata) = self.pdf_item.read(cx).metadata.as_ref() {
            let increment = if self.view_mode == ViewMode::DualPage {
                2
            } else {
                1
            };
            if self.current_page + increment < metadata.page_count {
                self.current_page += increment;
                self.scroll_offset = 0.0;
                self.load_current_page(cx);
            }
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
            self.load_current_page(cx);
        } else if self.current_page > 0 {
            self.current_page = 0;
            self.scroll_offset = 0.0;
            self.load_current_page(cx);
        }
    }

    fn scroll_down(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.scroll_offset -= SCROLL_AMOUNT;
        cx.notify();
    }

    fn scroll_up(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.scroll_offset += SCROLL_AMOUNT;
        if self.scroll_offset > 0.0 {
            self.scroll_offset = 0.0;
        }
        cx.notify();
    }

    fn page_down(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.scroll_offset -= PAGE_SCROLL_AMOUNT;
        cx.notify();

        if let Some(content) = &self.page_content {
            let image_height = content.image.size(0).height.0 as f32;
            if -self.scroll_offset > image_height {
                self.next_page(window, cx);
            }
        }
    }

    fn page_up(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.scroll_offset += PAGE_SCROLL_AMOUNT;
        if self.scroll_offset > 0.0 {
            if self.current_page > 0 {
                self.previous_page(window, cx);
                if let Some(content) = &self.page_content {
                    let image_height = content.image.size(0).height.0 as f32;
                    self.scroll_offset = -(image_height - self.view_height).max(0.0);
                }
            } else {
                self.scroll_offset = 0.0;
            }
        }
        cx.notify();
    }

    fn go_to_first_page(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.current_page != 0 {
            self.current_page = 0;
            self.scroll_offset = 0.0;
            self.load_current_page(cx);
        }
    }

    fn go_to_last_page(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(metadata) = self.pdf_item.read(cx).metadata.as_ref() {
            let last_page = metadata.page_count.saturating_sub(1);
            if self.current_page != last_page {
                self.current_page = last_page;
                self.scroll_offset = 0.0;
                self.load_current_page(cx);
            }
        }
    }

    fn set_view_mode(&mut self, mode: ViewMode, cx: &mut Context<Self>) {
        if self.view_mode != mode {
            self.view_mode = mode;
            self.scroll_offset = 0.0;
            self.load_current_page(cx);
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
        let progress = self.loading_progress;

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
                    .child("Loading PDF..."),
            )
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

    fn render_page_content(&self, content: PdfPageContent, _cx: &mut Context<Self>) -> AnyElement {
        let has_selection = self.text_selection.start_position.is_some()
            && self.text_selection.end_position.is_some()
            && !self.text_selection.is_selecting;

        let selection_overlay = if has_selection {
            if let (Some(start), Some(end)) = (
                self.text_selection.start_position,
                self.text_selection.end_position,
            ) {
                let min_x = start.x.min(end.x);
                let min_y = start.y.min(end.y);
                let width = (start.x - end.x).abs();
                let height = (start.y - end.y).abs();

                Some(
                    div()
                        .absolute()
                        .left(min_x)
                        .top(min_y)
                        .w(width)
                        .h(height)
                        .bg(gpui::rgba(0x3b82f640))
                        .border_1()
                        .border_color(gpui::rgba(0x3b82f6ff)),
                )
            } else {
                None
            }
        } else {
            None
        };

        div()
            .relative()
            .child(
                img(content.image)
                    .object_fit(ObjectFit::Contain)
                    .max_w_full(),
            )
            .children(selection_overlay)
            .into_any_element()
    }

    fn render_dual_page_content(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut children: Vec<AnyElement> = Vec::new();

        if let Some(primary) = self.page_content.clone() {
            children.push(self.render_page_content(primary, cx));
        }
        if let Some(secondary) = self.secondary_page_content.clone() {
            children.push(self.render_page_content(secondary, cx));
        }

        div()
            .flex()
            .flex_row()
            .items_start()
            .justify_center()
            .gap_4()
            .mt(px(self.scroll_offset))
            .children(children)
            .into_any_element()
    }

    fn render_single_page_content(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut children: Vec<AnyElement> = Vec::new();

        if let Some(content) = self.page_content.clone() {
            children.push(self.render_page_content(content, cx));
        }

        div()
            .flex()
            .flex_col()
            .items_center()
            .gap_2()
            .mt(px(self.scroll_offset))
            .children(children)
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
            page_content: self.page_content.clone(),
            secondary_page_content: self.secondary_page_content.clone(),
            load_task: None,
            zoom_level: self.zoom_level,
            page_cache: self.page_cache.clone(),
            scroll_offset: self.scroll_offset,
            view_height: self.view_height,
            view_mode: self.view_mode,
            is_loading: false,
            loading_progress: 1.0,
            show_settings_menu: false,
            text_selection: TextSelection::default(),
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
        let metadata = self.pdf_item.read(cx).metadata.as_ref();
        let page_count = metadata.map(|m| m.page_count).unwrap_or(0);
        let file_name = self.pdf_item.read(cx).file.file_name(cx).to_string();

        let has_content = self.page_content.is_some();
        let is_loading = self.is_loading;
        let show_settings = self.show_settings_menu;

        let mut page_info_str = String::new();
        if let Some(ref content) = self.page_content {
            let display_page = if self.view_mode == ViewMode::DualPage {
                format!(
                    "{}-{}",
                    content.page_number + 1,
                    (content.page_number + 2).min(page_count)
                )
            } else {
                format!("{}", content.page_number + 1)
            };
            page_info_str = format!(
                "Page {} of {} | Zoom: {:.0}% | {}",
                display_page,
                page_count,
                self.zoom_level * 100.0,
                self.view_mode.label()
            );
        }

        let content_area = if is_loading && !has_content {
            self.render_loading_indicator(cx).into_any_element()
        } else if has_content {
            match self.view_mode {
                ViewMode::DualPage => self.render_dual_page_content(cx).into_any_element(),
                _ => self.render_single_page_content(cx).into_any_element(),
            }
        } else {
            div()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .h_full()
                .gap_2()
                .child(div().text_lg().child("Failed to load PDF"))
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().colors().text_muted)
                        .child(file_name.clone()),
                )
                .into_any_element()
        };

        div()
            .key_context("PdfViewer")
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().colors().background)
            .on_action(cx.listener(|this, _: &ScrollDown, window, cx| {
                this.scroll_down(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ScrollUp, window, cx| {
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

                        if (new_zoom - this.zoom_level).abs() > 0.01 {
                            this.zoom_level = new_zoom;
                            this.page_cache.retain(|_, cached| {
                                (cached.scale_factor - this.zoom_level).abs() < 0.01
                            });
                            this.load_current_page(cx);
                        }
                    } else {
                        this.scroll_offset += dy;
                        cx.notify();
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _window, cx| {
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
                    .when(is_loading && has_content, |this| {
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
                                .child("Rendering..."),
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
