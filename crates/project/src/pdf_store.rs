use std::{collections::HashMap, path::PathBuf, sync::Arc};

use gpui::{App, AppContext, Context, Entity, EventEmitter, Image, Task, WeakEntity};
use language::{DiskState, File as _};

use crate::{Project, ProjectEntryId, ProjectItem, ProjectPath, worktree_store::WorktreeStore};

#[derive(Debug, Clone)]
pub struct PdfMetadata {
    pub page_count: usize,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub struct PdfId(u64);

impl PdfId {
    pub fn new() -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self(NEXT_ID.fetch_add(1, Ordering::SeqCst))
    }

    pub fn from_proto(id: u64) -> Self {
        Self(id)
    }

    pub fn to_proto(&self) -> u64 {
        self.0
    }
}

#[derive(Debug)]
pub enum PdfItemEvent {
    FileChanged,
}

impl EventEmitter<PdfItemEvent> for PdfItem {}

pub struct PdfItem {
    pub id: PdfId,
    pub file: Arc<worktree::File>,
    pub metadata: Option<PdfMetadata>,
    pub page_images: Vec<Option<Arc<Image>>>,
}

impl PdfItem {
    pub fn project_path(&self, cx: &App) -> ProjectPath {
        ProjectPath {
            worktree_id: self.file.worktree_id(cx),
            path: self.file.path().clone(),
        }
    }

    pub fn abs_path(&self, cx: &App) -> Option<PathBuf> {
        Some(self.file.as_local()?.abs_path(cx))
    }
}

impl ProjectItem for PdfItem {
    fn try_open(
        project: &Entity<Project>,
        path: &ProjectPath,
        cx: &mut App,
    ) -> Option<Task<anyhow::Result<Entity<Self>>>> {
        if is_pdf_file(project, path, cx) {
            Some(cx.spawn({
                let path = path.clone();
                let project = project.clone();
                async move |cx| {
                    project
                        .update(cx, |project, cx| project.open_pdf(path, cx))?
                        .await
                }
            }))
        } else {
            None
        }
    }

    fn entry_id(&self, _: &App) -> Option<ProjectEntryId> {
        self.file.entry_id
    }

    fn project_path(&self, cx: &App) -> Option<ProjectPath> {
        Some(self.project_path(cx))
    }

    fn is_dirty(&self) -> bool {
        false
    }
}

fn is_pdf_file(project: &Entity<Project>, path: &ProjectPath, cx: &mut App) -> bool {
    let ext = util::maybe!({
        let worktree_abs_path = project
            .read(cx)
            .worktree_for_id(path.worktree_id, cx)?
            .read(cx)
            .abs_path();
        path.path
            .extension()
            .or_else(|| worktree_abs_path.extension()?.to_str())
            .map(str::to_lowercase)
    });

    ext.as_deref() == Some("pdf")
}

pub struct PdfStore {
    opened_pdfs: HashMap<PdfId, WeakEntity<PdfItem>>,
    pdfs_by_path: HashMap<ProjectPath, PdfId>,
    worktree_store: Entity<WorktreeStore>,
}

impl PdfStore {
    pub fn new(worktree_store: Entity<WorktreeStore>) -> Self {
        Self {
            opened_pdfs: Default::default(),
            pdfs_by_path: Default::default(),
            worktree_store,
        }
    }

    pub fn open_pdf(
        &mut self,
        project_path: ProjectPath,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<Entity<PdfItem>>> {
        if let Some(pdf_id) = self.pdfs_by_path.get(&project_path) {
            if let Some(pdf) = self.opened_pdfs.get(pdf_id).and_then(|p| p.upgrade()) {
                return Task::ready(Ok(pdf));
            }
        }

        let worktree_id = project_path.worktree_id;
        let path = project_path.path.clone();

        let Some(worktree) = self
            .worktree_store
            .read(cx)
            .worktree_for_id(worktree_id, cx)
        else {
            return Task::ready(Err(anyhow::anyhow!("no such worktree")));
        };

        let project_path_clone = project_path.clone();
        cx.spawn(async move |this, cx| {
            let (entry_id, is_private, mtime) = worktree.update(cx, |worktree, _cx| {
                let entry = worktree.entry_for_path(&path);
                let entry_id = entry.map(|e| e.id);
                let is_private = entry.map(|e| e.is_private).unwrap_or(false);
                let mtime = entry.and_then(|e| e.mtime);
                (entry_id, is_private, mtime)
            })?;

            let disk_state = match mtime {
                Some(mtime) => DiskState::Present { mtime },
                None => DiskState::New,
            };

            let pdf_bytes = worktree
                .update(cx, |worktree, cx| {
                    worktree.load_binary_file(path.as_ref(), cx)
                })?
                .await?
                .content;

            let metadata = load_pdf_metadata(pdf_bytes.clone())?;

            this.update(cx, |this, cx| {
                let pdf_id = PdfId::new();
                let file = Arc::new(worktree::File {
                    worktree: worktree.clone(),
                    path,
                    disk_state,
                    entry_id,
                    is_local: true,
                    is_private,
                });

                let page_count = metadata.page_count;
                let pdf = cx.new(|_cx| PdfItem {
                    id: pdf_id,
                    file,
                    metadata: Some(metadata),
                    page_images: vec![None; page_count],
                });

                this.pdfs_by_path.insert(project_path_clone, pdf_id);
                this.opened_pdfs.insert(pdf_id, pdf.downgrade());
                Ok(pdf)
            })?
        })
    }

    pub fn pdfs(&self) -> impl '_ + Iterator<Item = Entity<PdfItem>> {
        self.opened_pdfs.values().filter_map(|pdf| pdf.upgrade())
    }

    pub fn get(&self, pdf_id: PdfId) -> Option<Entity<PdfItem>> {
        self.opened_pdfs.get(&pdf_id).and_then(|pdf| pdf.upgrade())
    }

    pub fn get_by_path(&self, path: &ProjectPath, cx: &App) -> Option<Entity<PdfItem>> {
        self.pdfs()
            .find(|pdf| &pdf.read(cx).project_path(cx) == path)
    }
}

fn load_pdf_metadata(bytes: Vec<u8>) -> anyhow::Result<PdfMetadata> {
    use lopdf::Document;

    let doc = Document::load_mem(&bytes)?;
    let page_count = doc.get_pages().len();

    Ok(PdfMetadata { page_count, bytes })
}

pub fn render_pdf_page_to_image(
    bytes: &[u8],
    page_number: usize,
    scale_factor: f32,
) -> anyhow::Result<Arc<Image>> {
    use pdfium_render::prelude::*;

    // Try to bind to pdfium:
    // 1. The bundled library in crates/pdf_viewer/lib/
    // 2. Local directory (for development)
    // 3. System library
    let pdfium = Pdfium::new(
        Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path(
            "crates/pdf_viewer/lib/",
        ))
        .or_else(|_| Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path("./")))
        .or_else(|_| Pdfium::bind_to_system_library())
        .map_err(|e| {
            anyhow::anyhow!(
                "Failed to bind to pdfium library: {}. Ensure pdfium binaries are available.",
                e
            )
        })?,
    );

    let document = pdfium.load_pdf_from_byte_slice(bytes, None)?;

    let page = document.pages().get(page_number as u16)?;

    let page_width = page.width().value;
    let page_height = page.height().value;

    let target_width = (page_width * scale_factor) as i32;
    let target_height = (page_height * scale_factor) as i32;

    let render_config = PdfRenderConfig::new()
        .set_target_width(target_width)
        .set_target_height(target_height)
        .set_reverse_byte_order(true);

    let bitmap = page.render_with_config(&render_config)?;

    let dynamic_image = bitmap.as_image();

    let mut buffer = Vec::new();
    dynamic_image
        .write_to(
            &mut std::io::Cursor::new(&mut buffer),
            image::ImageFormat::Png,
        )
        .map_err(|e| anyhow::anyhow!("Failed to write image buffer: {}", e))?;

    Ok(Arc::new(Image::from_bytes(gpui::ImageFormat::Png, buffer)))
}

pub fn extract_page_text(bytes: &[u8], page_number: usize) -> anyhow::Result<String> {
    use lopdf::Document;

    let doc = Document::load_mem(bytes)?;

    let mut page_ids: Vec<_> = doc.get_pages().keys().cloned().collect();
    page_ids.sort();

    if page_number >= page_ids.len() {
        return Ok(format!("[Page {} does not exist]", page_number + 1));
    }

    let page_num = page_number as u32;
    match doc.extract_text(&[page_num]) {
        Ok(text) if !text.trim().is_empty() => Ok(text),
        _ => {
            let all_page_numbers: Vec<u32> = (0..page_ids.len() as u32).collect();
            match doc.extract_text(&all_page_numbers) {
                Ok(all_text) => {
                    if all_text.trim().is_empty() {
                        Ok(format!(
                            "[Page {} of {} - No text content found]\n\nThis page may contain only images or be scanned.",
                            page_number + 1,
                            page_ids.len()
                        ))
                    } else {
                        Ok(format!(
                            "[Showing full document text]\n\nPage {} of {}\n\n{}",
                            page_number + 1,
                            page_ids.len(),
                            all_text
                        ))
                    }
                }
                Err(e) => Ok(format!(
                    "[Could not extract text from page {}]\n\nError: {}",
                    page_number + 1,
                    e
                )),
            }
        }
    }
}
