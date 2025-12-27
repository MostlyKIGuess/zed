use std::{collections::HashMap, path::PathBuf, sync::Arc};

use gpui::{App, AppContext, Context, Entity, EventEmitter, Task, WeakEntity};
use image::{Frame, RgbaImage};
use language::{DiskState, File as _};
use smallvec::SmallVec;

use crate::{Project, ProjectEntryId, ProjectItem, ProjectPath, worktree_store::WorktreeStore};

#[derive(Debug, Clone)]
pub struct PdfMetadata {
    pub page_count: usize,
    pub bytes: Arc<Vec<u8>>,
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

            let metadata = load_pdf_metadata(pdf_bytes)?;

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

                let pdf = cx.new(|_cx| PdfItem {
                    id: pdf_id,
                    file,
                    metadata: Some(metadata),
                });

                this.pdfs_by_path.insert(project_path, pdf_id);
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

    Ok(PdfMetadata {
        page_count,
        bytes: Arc::new(bytes),
    })
}

pub struct RenderedPage {
    pub data: SmallVec<[Frame; 1]>,
    pub width: u32,
    pub height: u32,
}

thread_local! {
    static PDFIUM_BINDINGS: std::cell::RefCell<Option<pdfium_render::prelude::Pdfium>> = const { std::cell::RefCell::new(None) };
}

/// Initialize Pdfium on the calling thread.
/// this should be called from the main thread before any background rendering
/// to ensure the Pdfium instance is ready and cached.
pub fn initialize_pdfium() -> anyhow::Result<()> {
    with_pdfium(|_| Ok(()))
}

fn with_pdfium<T, F>(f: F) -> anyhow::Result<T>
where
    F: FnOnce(&pdfium_render::prelude::Pdfium) -> anyhow::Result<T>,
{
    use pdfium_render::prelude::*;

    PDFIUM_BINDINGS
        .try_with(|cell| {
            // if cell.borrow().is_some() {
            //     log::info!("[PDF] with_pdfium: Using cached instance");
            // } else {
            //     log::info!("[PDF] with_pdfium: Initializing new instance");
            // }

            let mut borrow = cell.borrow_mut();
            if borrow.is_none() {
                // log::info!("[PDF] with_pdfium: Binding to library...");
                let bindings = Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path(
                "crates/pdf_viewer/lib/",
            ))
            .or_else(|_| {
                // log::info!("[PDF] with_pdfium: Trying ./ path");
                Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path("./"))
            })
            .or_else(|_| {
                // log::info!("[PDF] with_pdfium: Trying system library");
                Pdfium::bind_to_system_library()
            })
            .map_err(|e| {
                // log::error!("[PDF] with_pdfium: Failed to bind - {}", e);
                anyhow::anyhow!(
                    "Failed to bind to pdfium library: {}. Ensure pdfium binaries are available.",
                    e
                )
            })?;

                // log::info!("[PDF] with_pdfium: Creating Pdfium instance...");
                *borrow = Some(Pdfium::new(bindings));
                // log::info!("[PDF] with_pdfium: Initialization complete");
            }
            let pdfium = borrow.as_ref().expect("pdfium was just initialized");
            f(pdfium)
        })
        .map_err(|e| anyhow::anyhow!("Failed to access thread-local Pdfium: {}", e))?
}

pub fn render_pdf_page_to_raw(
    bytes: &[u8],
    page_number: usize,
    scale_factor: f32,
) -> anyhow::Result<RenderedPage> {
    use pdfium_render::prelude::*;

    with_pdfium(|pdfium| {
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
        let rgba_image = dynamic_image.to_rgba8();

        let width = rgba_image.width();
        let height = rgba_image.height();

        let mut buffer = RgbaImage::from_raw(width, height, rgba_image.into_raw())
            .ok_or_else(|| anyhow::anyhow!("Failed to create RGBA image buffer"))?;

        for pixel in buffer.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }

        let frame = Frame::new(buffer);

        Ok(RenderedPage {
            data: SmallVec::from_elem(frame, 1),
            width,
            height,
        })
    })
}

/// Batch render multiple pages from a single document load.
/// This is more efficient than calling render_pdf_page_to_raw multiple times
/// because the PDF document is loaded only once.
pub fn render_pdf_pages_batch(
    bytes: &[u8],
    page_numbers: &[usize],
    scale_factor: f32,
) -> anyhow::Result<std::collections::HashMap<usize, RenderedPage>> {
    use pdfium_render::prelude::*;
    use std::collections::HashMap;

    // log::info!(
    //     "[PDF] render_pdf_pages_batch: START - pages={:?}, scale={}",
    //     page_numbers,
    //     scale_factor
    // );

    let result = with_pdfium(|pdfium| {
        let document = pdfium.load_pdf_from_byte_slice(bytes, None)?;

        let pages = document.pages();

        let mut results = HashMap::new();

        for &page_number in page_numbers {
            let page = pages.get(page_number as u16)?;

            let page_width = page.width().value;
            let page_height = page.height().value;
            // log::info!(
            //     "[PDF] render_pdf_pages_batch: Page {} size: {}x{}",
            //     page_number,
            //     page_width,
            //     page_height
            // );

            let target_width = (page_width * scale_factor) as i32;
            let target_height = (page_height * scale_factor) as i32;
            // log::info!(
            //     "[PDF] render_pdf_pages_batch: Page {} target: {}x{}",
            //     page_number,
            //     target_width,
            //     target_height
            // );

            let render_config = PdfRenderConfig::new()
                .set_target_width(target_width)
                .set_target_height(target_height)
                .set_reverse_byte_order(true);

            let bitmap = page.render_with_config(&render_config)?;

            let dynamic_image = bitmap.as_image();

            let rgba_image = dynamic_image.to_rgba8();

            let width = rgba_image.width();
            let height = rgba_image.height();
            // log::info!(
            //     "[PDF] render_pdf_pages_batch: Page {} RGBA dimensions: {}x{}",
            //     page_number,
            //     width,
            //     height
            // );

            let mut buffer = RgbaImage::from_raw(width, height, rgba_image.into_raw())
                .ok_or_else(|| anyhow::anyhow!("Failed to create RGBA image buffer"))?;
            // log::info!(
            //     "[PDF] render_pdf_pages_batch: Created buffer for page {}",
            //     page_number
            // );

            for pixel in buffer.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }

            let frame = Frame::new(buffer);

            results.insert(
                page_number,
                RenderedPage {
                    data: SmallVec::from_elem(frame, 1),
                    width,
                    height,
                },
            );
            // log::info!(
            //     "[PDF] render_pdf_pages_batch: Finished page {}",
            //     page_number
            // );
        }

        // log::info!(
        //     "[PDF] render_pdf_pages_batch: Successfully rendered {} pages",
        //     results.len()
        // );
        Ok(results)
    });

    // match &result {
    //     Ok(_) => log::info!("[PDF] render_pdf_pages_batch: COMPLETED OK"),
    //     Err(e) => log::error!("[PDF] render_pdf_pages_batch: FAILED - {}", e),
    // }

    result
}

pub fn render_pdf_page_to_image(
    bytes: &[u8],
    page_number: usize,
    scale_factor: f32,
) -> anyhow::Result<Arc<gpui::Image>> {
    use pdfium_render::prelude::*;

    with_pdfium(|pdfium| {
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

        Ok(Arc::new(gpui::Image::from_bytes(
            gpui::ImageFormat::Png,
            buffer,
        )))
    })
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
