use eframe::egui;
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use std::path::PathBuf;
use std::sync::mpsc::Receiver;

use crate::core::mermaid::preprocess_mermaid_for_egui;
use crate::core::toc::{self, TocEntry};

pub fn run(file_path: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let canonical_file = std::fs::canonicalize(&file_path)
        .unwrap_or_else(|_| {
            std::env::current_dir()
                .map(|cwd| cwd.join(&file_path))
                .unwrap_or_else(|_| file_path.clone())
        });
    let base_dir = canonical_file.parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    let raw_markdown = std::fs::read_to_string(&file_path)
        .unwrap_or_else(|e| format!("# Error\nCould not read `{}`: {}", file_path.display(), e));

    let toc_entries = toc::extract_toc(&raw_markdown);
    let markdown = preprocess_mermaid_for_egui(&raw_markdown);
    let markdown = resolve_local_image_paths(&markdown, &base_dir);
    let (has_preamble, sections) = split_by_headings(&markdown);

    let watcher_rx = crate::core::watcher::watch_file(&file_path)?;

    let (icon_rgba, icon_w, icon_h) = crate::core::icon::load_icon_rgba();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 900.0])
            .with_title(format!("mdr - {}", file_path.display()))
            .with_icon(egui::IconData {
                rgba: icon_rgba,
                width: icon_w,
                height: icon_h,
            }),
        ..Default::default()
    };

    let section_parts: Vec<Vec<SectionPart>> =
        sections.iter().map(|s| split_section_by_mermaid(s)).collect();

    let file_path_clone = file_path.clone();
    eframe::run_native(
        "mdr",
        options,
        Box::new(move |_cc| {
            Ok(Box::new(MdrApp {
                markdown,
                sections,
                section_parts,
                has_preamble,
                caches: Vec::new(),
                file_path: file_path_clone,
                base_dir,
                watcher_rx,
                toc_entries,
                scroll_to_section: None,
                search_active: false,
                search_query: String::new(),
                search_section_matches: Vec::new(),
                current_match: 0,
                texture_cache: std::collections::HashMap::new(),
                popup_texture: None,
            }))
        }),
    )
    .map_err(|e| e.to_string().into())
}

/// Split markdown into sections at heading boundaries.
/// Returns (has_preamble, sections) where has_preamble is true if there's
/// content before the first heading (which means headings start at index 1).
fn split_by_headings(markdown: &str) -> (bool, Vec<String>) {
    let mut sections = Vec::new();
    let mut current = String::new();

    for line in markdown.lines() {
        if line.starts_with('#') && !line.starts_with("#!") {
            let trimmed = line.trim_start_matches('#');
            if trimmed.starts_with(' ') && !current.is_empty() {
                sections.push(current);
                current = String::new();
            }
        }
        current.push_str(line);
        current.push('\n');
    }
    if !current.is_empty() {
        sections.push(current);
    }

    // Check if section 0 starts with a heading or is preamble text
    let has_preamble = sections.first()
        .map(|s| {
            let first_line = s.lines().next().unwrap_or("");
            let trimmed = first_line.trim_start_matches('#');
            !(first_line.starts_with('#') && trimmed.starts_with(' '))
        })
        .unwrap_or(false);

    (has_preamble, sections)
}

enum SectionPart {
    Markdown(String),
    MermaidImage { base64_data: String },
}

/// Split a section's markdown text by mermaid diagram images.
/// Mermaid images produced by `preprocess_mermaid_for_egui` have the form:
/// `![mermaid diagram](data:image/png;base64,...)`
/// Each such image becomes a `MermaidImage` part; surrounding text becomes `Markdown` parts.
fn split_section_by_mermaid(section: &str) -> Vec<SectionPart> {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"!\[mermaid diagram\]\(data:image/png;base64,([A-Za-z0-9+/=]+)\)",
        )
        .unwrap()
    });

    let mut parts = Vec::new();
    let mut last_end = 0;

    for cap in re.captures_iter(section) {
        let whole = cap.get(0).unwrap();
        let before = &section[last_end..whole.start()];
        if !before.trim().is_empty() {
            parts.push(SectionPart::Markdown(before.to_string()));
        }
        parts.push(SectionPart::MermaidImage {
            base64_data: cap[1].to_string(),
        });
        last_end = whole.end();
    }

    let after = &section[last_end..];
    if !after.trim().is_empty() {
        parts.push(SectionPart::Markdown(after.to_string()));
    }

    if parts.is_empty() {
        parts.push(SectionPart::Markdown(section.to_string()));
    }

    parts
}

/// Decode a base64-encoded PNG and load (or retrieve from cache) an egui texture.
fn load_or_cache_texture(
    ctx: &egui::Context,
    base64_data: &str,
    cache: &mut std::collections::HashMap<u64, egui::TextureHandle>,
) -> egui::TextureHandle {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    base64_data.len().hash(&mut hasher);
    // Hash first and last 256 bytes for speed (full data is huge)
    base64_data[..base64_data.len().min(256)].hash(&mut hasher);
    if base64_data.len() > 256 {
        base64_data[base64_data.len() - 256..].hash(&mut hasher);
    }
    let hash = hasher.finish();

    cache
        .entry(hash)
        .or_insert_with(|| {
            use base64::Engine;
            let png_bytes = base64::engine::general_purpose::STANDARD
                .decode(base64_data)
                .unwrap_or_default();
            let img = image::load_from_memory(&png_bytes)
                .unwrap_or_else(|_| image::DynamicImage::new_rgba8(1, 1));
            let rgba = img.to_rgba8();
            let (w, h) = rgba.dimensions();
            let color_image = egui::ColorImage::from_rgba_unmultiplied(
                [w as usize, h as usize],
                &rgba,
            );
            ctx.load_texture(
                format!("mermaid_{}", hash),
                color_image,
                egui::TextureOptions::LINEAR,
            )
        })
        .clone()
}

struct MdrApp {
    markdown: String,
    sections: Vec<String>,
    section_parts: Vec<Vec<SectionPart>>,
    has_preamble: bool,
    caches: Vec<CommonMarkCache>,
    file_path: PathBuf,
    base_dir: PathBuf,
    watcher_rx: Receiver<()>,
    toc_entries: Vec<TocEntry>,
    scroll_to_section: Option<usize>,
    search_active: bool,
    search_query: String,
    search_section_matches: Vec<usize>,
    current_match: usize,
    texture_cache: std::collections::HashMap<u64, egui::TextureHandle>,
    popup_texture: Option<egui::TextureHandle>,
}

impl eframe::App for MdrApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Check for file changes
        if self.watcher_rx.try_recv().is_ok() {
            while self.watcher_rx.try_recv().is_ok() {}
            if let Ok(content) = std::fs::read_to_string(&self.file_path) {
                self.toc_entries = toc::extract_toc(&content);
                self.markdown = preprocess_mermaid_for_egui(&content);
                self.markdown = resolve_local_image_paths(&self.markdown, &self.base_dir);
                let (has_preamble, sections) = split_by_headings(&self.markdown);
                self.has_preamble = has_preamble;
                self.section_parts = sections.iter().map(|s| split_section_by_mermaid(s)).collect();
                self.sections = sections;
                self.caches.clear();
                self.texture_cache.clear();
            }
        }

        // Ensure we have enough caches (one per Markdown part across all sections)
        let total_md_parts: usize = self.section_parts.iter()
            .flat_map(|parts| parts.iter())
            .filter(|p| matches!(p, SectionPart::Markdown(_)))
            .count();
        while self.caches.len() < total_md_parts {
            self.caches.push(CommonMarkCache::default());
        }

        // Handle Ctrl+F for search
        if ctx.input(|i| i.key_pressed(egui::Key::F) && i.modifiers.ctrl) {
            self.search_active = !self.search_active;
            if !self.search_active {
                self.search_query.clear();
                self.search_section_matches.clear();
            }
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            if self.popup_texture.is_some() {
                self.popup_texture = None;
            } else if self.search_active {
                self.search_active = false;
                self.search_query.clear();
                self.search_section_matches.clear();
            }
        }

        // Search bar panel
        if self.search_active {
            egui::TopBottomPanel::top("search_bar").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Search:");
                    let response = ui.text_edit_singleline(&mut self.search_query);
                    if response.changed() {
                        // Update matches
                        self.search_section_matches.clear();
                        self.current_match = 0;
                        if !self.search_query.is_empty() {
                            let query_lower = self.search_query.to_lowercase();
                            for (i, section) in self.sections.iter().enumerate() {
                                if section.to_lowercase().contains(&query_lower) {
                                    self.search_section_matches.push(i);
                                }
                            }
                            if !self.search_section_matches.is_empty() {
                                self.scroll_to_section = Some(self.search_section_matches[0]);
                            }
                        }
                    }
                    // Request focus on first show
                    if response.gained_focus() || ctx.input(|i| i.key_pressed(egui::Key::F) && i.modifiers.ctrl) {
                        response.request_focus();
                    }

                    let match_text = if self.search_section_matches.is_empty() {
                        if self.search_query.is_empty() { "".to_string() }
                        else { "No matches".to_string() }
                    } else {
                        format!("{}/{}", self.current_match + 1, self.search_section_matches.len())
                    };
                    ui.label(&match_text);

                    if ui.button("\u{25B2}").clicked() || (ui.input(|i| i.key_pressed(egui::Key::Enter) && i.modifiers.shift) && self.search_active) {
                        if !self.search_section_matches.is_empty() {
                            self.current_match = if self.current_match == 0 {
                                self.search_section_matches.len() - 1
                            } else {
                                self.current_match - 1
                            };
                            self.scroll_to_section = Some(self.search_section_matches[self.current_match]);
                        }
                    }
                    if ui.button("\u{25BC}").clicked() || (ui.input(|i| i.key_pressed(egui::Key::Enter) && !i.modifiers.shift) && self.search_active) {
                        if !self.search_section_matches.is_empty() {
                            self.current_match = (self.current_match + 1) % self.search_section_matches.len();
                            self.scroll_to_section = Some(self.search_section_matches[self.current_match]);
                        }
                    }
                    if ui.button("\u{2715}").clicked() {
                        self.search_active = false;
                        self.search_query.clear();
                        self.search_section_matches.clear();
                    }
                });
            });
        }

        // TOC sidebar
        let has_preamble = self.has_preamble;
        let scroll_target = &mut self.scroll_to_section;

        egui::SidePanel::left("toc_panel")
            .default_width(220.0)
            .show(ctx, |ui| {
                ui.heading("Table of Contents");
                ui.separator();
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for (i, entry) in self.toc_entries.iter().enumerate() {
                        let indent = ((entry.level as f32 - 1.0) * 12.0).max(0.0);
                        ui.horizontal(|ui| {
                            ui.add_space(indent);
                            let text = match entry.level {
                                1 => egui::RichText::new(&entry.text).strong(),
                                2 => egui::RichText::new(&entry.text).strong().size(13.0),
                                3 => egui::RichText::new(&entry.text).size(13.0),
                                _ => egui::RichText::new(&entry.text).size(12.0).weak(),
                            };
                            if ui.link(text).clicked() {
                                // Map TOC index to section index
                                let section_idx = if has_preamble { i + 1 } else { i };
                                *scroll_target = Some(section_idx);
                            }
                        });
                    }
                });
            });

        // Main content - render each section with scroll anchors
        let scroll_to = self.scroll_to_section.take();

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                let mut cache_idx = 0;
                for (i, parts) in self.section_parts.iter().enumerate() {
                    // Place an invisible anchor widget before the section
                    let response = ui.allocate_response(
                        egui::vec2(0.0, 0.0),
                        egui::Sense::hover(),
                    );

                    // If this is the target section, scroll to the anchor
                    if scroll_to == Some(i) {
                        response.scroll_to_me(Some(egui::Align::TOP));
                    }

                    for (j, part) in parts.iter().enumerate() {
                        match part {
                            SectionPart::Markdown(ref md) => {
                                let anchor_id = ui.id().with(format!("section_{}_{}", i, j));
                                ui.push_id(anchor_id, |ui| {
                                    CommonMarkViewer::new()
                                        .show(ui, &mut self.caches[cache_idx], md);
                                });
                                cache_idx += 1;
                            }
                            SectionPart::MermaidImage { ref base64_data } => {
                                let texture = load_or_cache_texture(
                                    ui.ctx(),
                                    base64_data,
                                    &mut self.texture_cache,
                                );
                                let texture_size = texture.size_vec2();
                                let max_width = ui.available_width();
                                let scale = (max_width / texture_size.x).min(1.0);
                                let display_size = texture_size * scale;

                                let (rect, response) = ui.allocate_exact_size(
                                    display_size,
                                    egui::Sense::click(),
                                );
                                if ui.is_rect_visible(rect) {
                                    let uv = egui::Rect::from_min_max(
                                        egui::pos2(0.0, 0.0),
                                        egui::pos2(1.0, 1.0),
                                    );
                                    ui.painter().image(
                                        texture.id(),
                                        rect,
                                        uv,
                                        egui::Color32::WHITE,
                                    );
                                }
                                if response.hovered() {
                                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                }
                                if response.clicked() {
                                    self.popup_texture = Some(texture);
                                }
                            }
                        }
                    }
                }
            });
        });

        // Mermaid diagram popup window
        let mut close_popup = false;
        if let Some(ref texture) = self.popup_texture {
            let mut open = true;
            egui::Window::new("Mermaid Diagram")
                .open(&mut open)
                .resizable(true)
                .collapsible(false)
                .default_size([800.0, 600.0])
                .vscroll(true)
                .hscroll(true)
                .show(ctx, |ui| {
                    let size = texture.size_vec2();
                    let uv = egui::Rect::from_min_max(
                        egui::pos2(0.0, 0.0),
                        egui::pos2(1.0, 1.0),
                    );
                    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                    if ui.is_rect_visible(rect) {
                        ui.painter().image(
                            texture.id(),
                            rect,
                            uv,
                            egui::Color32::WHITE,
                        );
                    }
                });
            if !open {
                close_popup = true;
            }
        }
        if close_popup {
            self.popup_texture = None;
        }

        ctx.request_repaint_after(std::time::Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- split_by_headings tests ---

    #[test]
    fn split_by_headings_single_heading() {
        let md = "# Title\nSome content\n";
        let (has_preamble, sections) = split_by_headings(md);
        assert!(!has_preamble);
        assert_eq!(sections.len(), 1);
        assert!(sections[0].contains("# Title"));
        assert!(sections[0].contains("Some content"));
    }

    #[test]
    fn split_by_headings_multiple_headings() {
        let md = "# First\nContent 1\n## Second\nContent 2\n### Third\nContent 3\n";
        let (has_preamble, sections) = split_by_headings(md);
        assert!(!has_preamble);
        assert_eq!(sections.len(), 3);
        assert!(sections[0].contains("# First"));
        assert!(sections[1].contains("## Second"));
        assert!(sections[2].contains("### Third"));
    }

    #[test]
    fn split_by_headings_with_preamble() {
        let md = "Some introductory text.\n\n# First Heading\nContent here.\n";
        let (has_preamble, sections) = split_by_headings(md);
        assert!(has_preamble);
        assert_eq!(sections.len(), 2);
        assert!(sections[0].contains("Some introductory text."));
        assert!(sections[1].contains("# First Heading"));
    }

    #[test]
    fn split_by_headings_no_headings() {
        let md = "Just some text.\nNo headings here.\n";
        let (has_preamble, sections) = split_by_headings(md);
        assert!(has_preamble);
        assert_eq!(sections.len(), 1);
        assert!(sections[0].contains("Just some text."));
    }

    #[test]
    fn split_by_headings_empty_input() {
        let (has_preamble, sections) = split_by_headings("");
        assert!(!has_preamble);
        assert!(sections.is_empty());
    }

    #[test]
    fn split_by_headings_hash_in_code_block_not_split() {
        // Lines starting with # inside code are not headings if they lack
        // the space after the # sequence. But the function checks for trimmed.starts_with(' ')
        // so `# comment` inside code would still split. This tests that non-heading # lines
        // (like shebang #!) are ignored.
        let md = "# Title\n#!/bin/bash\necho hello\n";
        let (has_preamble, sections) = split_by_headings(md);
        assert!(!has_preamble);
        // The shebang line starts with #! which is filtered by !line.starts_with("#!")
        assert_eq!(sections.len(), 1);
        assert!(sections[0].contains("#!/bin/bash"));
    }

    #[test]
    fn split_by_headings_shebang_as_first_line() {
        let md = "#!/bin/bash\n# Title\nContent\n";
        let (has_preamble, sections) = split_by_headings(md);
        // First line is #!/bin/bash which is not a heading -> preamble
        assert!(has_preamble);
        assert_eq!(sections.len(), 2);
    }

    #[test]
    fn split_by_headings_consecutive_headings() {
        let md = "# H1\n## H2\n## H3\n";
        let (has_preamble, sections) = split_by_headings(md);
        assert!(!has_preamble);
        assert_eq!(sections.len(), 3);
    }

    #[test]
    fn split_by_headings_heading_without_space_not_treated_as_heading() {
        // "#notaheading" should not be treated as a heading (no space after #)
        let md = "# Real Heading\n#notaheading\ntext\n";
        let (has_preamble, sections) = split_by_headings(md);
        assert!(!has_preamble);
        // #notaheading lacks space after #, so it doesn't split
        assert_eq!(sections.len(), 1);
        assert!(sections[0].contains("#notaheading"));
    }

    #[test]
    fn split_by_headings_preserves_content_within_sections() {
        let md = "# Title\nLine 1\nLine 2\n\n## Next\nLine 3\n";
        let (_, sections) = split_by_headings(md);
        assert!(sections[0].contains("Line 1"));
        assert!(sections[0].contains("Line 2"));
        assert!(sections[1].contains("Line 3"));
    }

    // --- split_section_by_mermaid tests ---

    #[test]
    fn split_no_mermaid_returns_single_markdown() {
        let section = "# Title\nSome text\n";
        let parts = split_section_by_mermaid(section);
        assert_eq!(parts.len(), 1);
        assert!(matches!(&parts[0], SectionPart::Markdown(s) if s.contains("Title")));
    }

    #[test]
    fn split_mermaid_in_middle() {
        let section = "Before text\n![mermaid diagram](data:image/png;base64,AAAA)\nAfter text\n";
        let parts = split_section_by_mermaid(section);
        assert_eq!(parts.len(), 3);
        assert!(matches!(&parts[0], SectionPart::Markdown(s) if s.contains("Before")));
        assert!(matches!(&parts[1], SectionPart::MermaidImage { base64_data } if base64_data == "AAAA"));
        assert!(matches!(&parts[2], SectionPart::Markdown(s) if s.contains("After")));
    }

    #[test]
    fn split_mermaid_only() {
        let section = "![mermaid diagram](data:image/png;base64,BBBB)\n";
        let parts = split_section_by_mermaid(section);
        assert_eq!(parts.len(), 1);
        assert!(matches!(&parts[0], SectionPart::MermaidImage { base64_data } if base64_data == "BBBB"));
    }

    #[test]
    fn split_multiple_mermaids() {
        let section = "Text1\n![mermaid diagram](data:image/png;base64,AA)\nText2\n![mermaid diagram](data:image/png;base64,BB)\nText3\n";
        let parts = split_section_by_mermaid(section);
        assert_eq!(parts.len(), 5);
        assert!(matches!(&parts[0], SectionPart::Markdown(_)));
        assert!(matches!(&parts[1], SectionPart::MermaidImage { base64_data } if base64_data == "AA"));
        assert!(matches!(&parts[2], SectionPart::Markdown(_)));
        assert!(matches!(&parts[3], SectionPart::MermaidImage { base64_data } if base64_data == "BB"));
        assert!(matches!(&parts[4], SectionPart::Markdown(_)));
    }

    #[test]
    fn split_non_mermaid_image_not_split() {
        let section = "![screenshot](data:image/png;base64,CCCC)\n";
        let parts = split_section_by_mermaid(section);
        assert_eq!(parts.len(), 1);
        assert!(matches!(&parts[0], SectionPart::Markdown(_)));
    }

    #[test]
    fn split_empty_section() {
        let parts = split_section_by_mermaid("");
        assert_eq!(parts.len(), 1);
        assert!(matches!(&parts[0], SectionPart::Markdown(_)));
    }
}

/// Resolve relative image paths in markdown to inline data URIs.
/// We use data URIs for ALL images (not file:// URLs) because:
/// - file:// URLs break when paths contain spaces
/// - Data URIs are self-contained and always work
/// SVG files are rasterized to PNG first to avoid egui_commonmark parsing issues.
fn resolve_local_image_paths(markdown: &str, base_dir: &std::path::Path) -> String {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"!\[([^\]]*)\]\(([^)]+)\)").unwrap());
    re.replace_all(markdown, |caps: &regex::Captures| {
        let alt = &caps[1];
        let src = &caps[2];
        // Skip URLs and data URIs
        if src.starts_with("http://") || src.starts_with("https://")
            || src.starts_with("data:") || src.starts_with("file://")
        {
            return caps[0].to_string();
        }
        let abs_path = base_dir.join(src);
        // Path traversal protection: ensure resolved path is within base_dir
        if let (Ok(canonical), Ok(canonical_base)) = (abs_path.canonicalize(), base_dir.canonicalize()) {
            if !canonical.starts_with(&canonical_base) {
                return caps[0].to_string();
            }
        }
        if abs_path.exists() {
            // SVG files: rasterize to PNG data URI to avoid parsing failures
            let is_svg = abs_path.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("svg"))
                .unwrap_or(false);
            if is_svg {
                // Try rasterizing SVG to PNG (handles complex SVGs better)
                if let Ok(data_uri) = rasterize_svg_to_png_data_uri(&abs_path) {
                    return format!("![{}]({})", alt, data_uri);
                }
                // Fallback: embed SVG directly as data URI for egui_commonmark's SVG feature
                if let Ok(data_uri) = file_to_data_uri(&abs_path) {
                    return format!("![{}]({})", alt, data_uri);
                }
                // SVG completely failed — skip it
                return caps[0].to_string();
            }
            // All non-SVG images: embed as base64 data URI
            if let Ok(data_uri) = file_to_data_uri(&abs_path) {
                return format!("![{}]({})", alt, data_uri);
            }
            caps[0].to_string()
        } else {
            caps[0].to_string()
        }
    })
    .to_string()
}

/// Convert a local file to a base64 data URI string.
fn file_to_data_uri(path: &std::path::Path) -> Result<String, Box<dyn std::error::Error>> {
    use base64::Engine;
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let mime = match ext.to_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    };
    let data = std::fs::read(path)?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
    Ok(format!("data:{};base64,{}", mime, b64))
}

/// Rasterize an SVG file to PNG and return as a base64 data URI.
/// Caps dimensions at 8192px to avoid GPU texture overflow.
fn rasterize_svg_to_png_data_uri(path: &std::path::Path) -> Result<String, Box<dyn std::error::Error>> {
    use base64::Engine;
    use std::sync::{Arc, OnceLock};

    const MAX_DIM: f32 = 8192.0;

    let svg_data = std::fs::read_to_string(path)?;

    // Reject files that aren't actually SVG (e.g. HTML pages saved with .svg extension)
    let trimmed = svg_data.trim_start();
    if !trimmed.starts_with('<') || trimmed.starts_with("<!DOCTYPE html") || trimmed.starts_with("<html") {
        if !trimmed.contains("<svg") {
            return Err("File is not a valid SVG (possibly an HTML page)".into());
        }
    }

    static FONTDB: OnceLock<Arc<usvg::fontdb::Database>> = OnceLock::new();
    let fontdb = FONTDB.get_or_init(|| {
        let mut db = usvg::fontdb::Database::new();
        db.load_system_fonts();
        Arc::new(db)
    });

    let mut options = usvg::Options::default();
    options.fontdb = Arc::clone(fontdb);
    let tree = usvg::Tree::from_str(&svg_data, &options)?;
    let size = tree.size();
    let svg_w = size.width();
    let svg_h = size.height();

    if svg_w <= 0.0 || svg_h <= 0.0 {
        return Err("SVG has zero dimensions".into());
    }

    // Scale 2x for retina, but cap at MAX_DIM
    let ideal_scale = 2.0_f32;
    let max_scale_w = MAX_DIM / svg_w;
    let max_scale_h = MAX_DIM / svg_h;
    let scale = ideal_scale.min(max_scale_w).min(max_scale_h);

    let width = (svg_w * scale) as u32;
    let height = (svg_h * scale) as u32;

    if width == 0 || height == 0 {
        return Err("SVG too small after scaling".into());
    }

    let mut pixmap = tiny_skia::Pixmap::new(width, height)
        .ok_or("Failed to create pixmap")?;
    let transform = tiny_skia::Transform::from_scale(scale, scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    let png_data = pixmap.encode_png()?;
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png_data);
    Ok(format!("data:image/png;base64,{}", b64))
}
