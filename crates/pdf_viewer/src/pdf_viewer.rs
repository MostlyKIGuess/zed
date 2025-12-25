use std::path::Path;
use std::sync::Arc;

use anyhow::Context as _;
use editor::{EditorSettings, items::entry_git_aware_label_color};
use file_icons::FileIcons;
use gpui::{
    AnyElement, App, Context, Entity, EventEmitter, FocusHandle, Focusable,
    Image, InteractiveElement, IntoElement, ObjectFit, ParentElement, Render, Styled,
    Task, WeakEntity, Window, div, img, prelude::*,
};
use gpui::FontWeight;
use language::File as _;
use project::{PdfItem, Project, ProjectPath, pdf_store::render_pdf_page_to_image};
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
    image: Arc<Image>,
    page_number: usize,
}

pub struct PdfViewer {
    pdf_item: Entity<PdfItem>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    current_page: usize,
    page_content: Option<PdfPageContent>,
    load_task: Option<Task<()>>,
}

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
            load_task: None,
        };

        viewer.load_current_page(cx);
        viewer
    }

    fn load_current_page(&mut self, cx: &mut Context<Self>) {
        let pdf_item = self.pdf_item.clone();
        let page_number = self.current_page;

        self.load_task = Some(cx.spawn(async move |this, cx| {
            let result = cx.update(|cx| {
                let item = pdf_item.read(cx);
                item.metadata.clone()
            });

            if let Ok(Some(metadata)) = result {
                let page_image = Self::render_page_image(&metadata.bytes, page_number).await;

                let _ = this.update(cx, |this, cx| {
                    match page_image {
                        Ok(image) => {
                            this.page_content = Some(PdfPageContent { image, page_number });
                        }
                        Err(e) => {
                            log::error!("Failed to render PDF page: {e:?}");
                        }
                    }
                    cx.notify();
                });
            }
        }));
    }

    async fn render_page_image(bytes: &[u8], page_number: usize) -> anyhow::Result<Arc<Image>> {
        let bytes = bytes.to_vec();
        render_pdf_page_to_image(&bytes, page_number, 2.0)
    }

    fn next_page(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(metadata) = self.pdf_item.read(cx).metadata.as_ref() {
            if self.current_page + 1 < metadata.page_count {
                self.current_page += 1;
                self.load_current_page(cx);
            }
        }
    }

    fn previous_page(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if self.current_page > 0 {
            self.current_page -= 1;
            self.load_current_page(cx);
        }
    }

    fn on_pdf_event(
        &mut self,
        _: Entity<PdfItem>,
        event: &PdfItemEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            PdfItemEvent::FileChanged => {
                cx.emit(PdfViewerEvent::TitleChanged);
                cx.notify();
            }
        }
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
        self.pdf_item
            .read(cx)
            .file
            .file_name(cx)
            .to_string()
            .into()
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
            load_task: None,
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

            cx.update(
                |window, cx| Ok(cx.new(|cx| PdfViewer::new(pdf_item, project, window, cx))),
            )?
        })
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<()>> {
        delete_unloaded_items(
            alive_items,
            workspace_id,
            "pdf_viewers",
            &PDF_VIEWER_DB,
            cx,
        )
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
        let content = self.page_content.clone();

        let mut page_info_str = String::new();
        if let Some(ref content) = self.page_content {
            page_info_str = format!(
                "Page {} of {}",
                content.page_number + 1,
                page_count
            );
        }

        let content_area = if has_content {
            if let Some(content) = content {
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_2()
                    .child(
                        img(content.image)
                            .object_fit(ObjectFit::Contain)
                            .max_w_full()
                    )
                    .into_any_element()
            } else {
                div().into_any_element()
            }
        } else {
            div()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .h_full()
                .gap_2()
                .child(
                    div()
                        .text_lg()
                        .child("Loading PDF..."),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().colors().text_muted)
                        .child(file_name.clone()),
                )
                .into_any_element()
        };

        div()
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().colors().background)
            .child(
                // Toolbar
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
                            .child(
                                Button::new("previous-page", "<")
                                    .disabled(self.current_page == 0)
                                    .on_click(cx.listener(|this, _event, window, cx| {
                                        this.previous_page(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("next-page", ">")
                                    .disabled(page_count == 0 || self.current_page + 1 >= page_count)
                                    .on_click(cx.listener(|this, _event, window, cx| {
                                        this.next_page(window, cx);
                                    })),
                            ),
                    ),
            )
            .child(
                // Content area
                div()
                    .flex_1()
                    .p_4()
                    .child(content_area),
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
