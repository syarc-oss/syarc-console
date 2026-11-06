//! Ebook sub-app: topics (optionally nested), books (epub/pdf), covers,
//! sharing, public links, reading lists and an in-browser EPUB reader.
//! Ported from fastmain. Deviations: PDF->EPUB conversion returns 501,
//! epub metadata embedding and ISBN network enrichment are skipped.

use axum::extract::{Multipart, Path, Query, RawForm, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

use crate::{
    err_json, form_map, fv, human_size, is_super, new_uuid, now_str, page, redirect, root,
    static_dir, RequireUser, SharedState,
};

pub(crate) const EBOOK_EXTENSIONS: [&str; 2] = ["epub", "pdf"];

pub(crate) fn covers_dir() -> std::path::PathBuf {
    static_dir("ebook").join("covers")
}

/// Covers now live next to the book file in the library tree (shared between
/// dev and prod): `<book.epub>.cover.jpg`. The covers_dir is kept only as a
/// legacy fallback for covers migrated from fastmain.
pub(crate) fn sibling_cover_path(book_path: &str) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("{book_path}.cover.jpg"))
}

/// Sentinel stored in ebook_books.cover_filename when the cover is the
/// sibling file next to the book (all newly generated covers).
pub(crate) const COVER_SIBLING: &str = "sibling";


/// Remove a book's cover file: the sibling next to the book if present,
/// otherwise the legacy covers_dir entry.
pub(crate) fn remove_book_cover(file_path: &str, cover_filename: Option<&str>) {
    if !file_path.is_empty() {
        let sp = sibling_cover_path(file_path);
        if sp.is_file() {
            let _ = std::fs::remove_file(sp);
        }
    }
    if let Some(cf) = cover_filename.filter(|c| *c != COVER_SIBLING) {
        let cp = covers_dir().join(cf);
        if cp.is_file() {
            let _ = std::fs::remove_file(cp);
        }
    }
}

fn err404(msg: &str) -> Response {
    err_json(axum::http::StatusCode::NOT_FOUND, msg)
}
fn err403(msg: &str) -> Response {
    err_json(axum::http::StatusCode::FORBIDDEN, msg)
}
fn err400(msg: &str) -> Response {
    err_json(axum::http::StatusCode::BAD_REQUEST, msg)
}

// ---------------------------------------------------------------- helpers

pub(crate) fn sanitize_str(s: &str) -> String {
    s.replace('\u{0}', "").trim().to_string()
}

#[derive(Default, Clone)]
pub(crate) struct EbookMeta {
    pub title: String,
    pub author: String,
    pub isbn: Option<String>,
    pub isbn13: Option<String>,
    pub subjects: Vec<String>,
    pub publisher: Option<String>,
    pub published_date: Option<String>,
    pub description: Option<String>,
    pub language: String,
}

impl EbookMeta {
    fn unknown() -> Self {
        EbookMeta {
            title: "Unknown Title".into(),
            author: "Unknown Author".into(),
            language: "en".into(),
            ..Default::default()
        }
    }
}

pub(crate) fn fallback_metadata_from_filename(path: &str) -> EbookMeta {
    let filename = std::path::Path::new(path)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    let stem = filename
        .strip_suffix(
            std::path::Path::new(&filename)
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy()))
                .as_deref()
                .unwrap_or(""),
        )
        .unwrap_or(&filename)
        .to_string();
    let mut meta = EbookMeta::unknown();
    meta.title = stem.clone();
    if let Some((author, title)) = stem.split_once(" - ") {
        meta.author = author.trim().to_string();
        meta.title = title.trim().to_string();
    } else {
        let lower = stem.to_lowercase();
        if let Some(idx) = lower.find(" by ") {
            let title = stem[..idx].trim().to_string();
            let author = stem[idx + 4..].trim().to_string();
            if !title.is_empty() && !author.is_empty() {
                meta.title = title;
                meta.author = author;
            }
        }
    }
    meta
}

// --------------------------------------------------------------- epub parse

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

fn tag_content(xml: &str, tag: &str) -> Option<String> {
    let re = regex::Regex::new(&format!(r"(?is)<{tag}\b[^>]*>(.*?)</{tag}>")).ok()?;
    let m = re.captures(xml)?;
    Some(xml_unescape(&m[1]).trim().to_string())
}

fn tag_contents(xml: &str, tag: &str) -> Vec<String> {
    let Ok(re) = regex::Regex::new(&format!(r"(?is)<{tag}\b[^>]*>(.*?)</{tag}>")) else {
        return vec![];
    };
    re.captures_iter(xml)
        .map(|c| xml_unescape(&c[1]).trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

pub(crate) struct EpubArchive {
    pub zf: zip::ZipArchive<std::fs::File>,
    pub opf: String,
}

pub(crate) fn open_epub(path: &str) -> Option<EpubArchive> {
    let file = std::fs::File::open(path).ok()?;
    let mut zf = zip::ZipArchive::new(file).ok()?;
    let container = {
        let mut buf = Vec::new();
        let name = zf
            .file_names()
            .find(|n| n.eq_ignore_ascii_case("META-INF/container.xml"))?
            .to_string();
        use std::io::Read;
        zf.by_name(&name).ok()?.read_to_end(&mut buf).ok()?;
        String::from_utf8_lossy(&buf).into_owned()
    };
    let re = regex::Regex::new(r#"(?i)full-path="([^"]+)""#).ok()?;
    let opf = xml_unescape(&re.captures(&container)?[1]);
    Some(EpubArchive { zf, opf })
}

pub(crate) fn read_zip_entry(zf: &mut zip::ZipArchive<std::fs::File>, name: &str) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut f = zf.by_name(name).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// Find zip entry by exact name (case-insensitive) or by suffix match.
pub(crate) fn find_zip_entry(
    zf: &mut zip::ZipArchive<std::fs::File>,
    target: &str,
) -> Option<String> {
    let norm = target.replace('\\', "/").to_lowercase();
    let names: Vec<String> = zf.file_names().map(|n| n.to_string()).collect();
    if let Some(exact) = names
        .iter()
        .find(|n| n.replace('\\', "/").eq_ignore_ascii_case(&norm))
    {
        return Some(exact.clone());
    }
    names
        .into_iter()
        .find(|n| n.replace('\\', "/").to_lowercase().ends_with(&norm))
}

/// Manifest item: (id, href resolved against OPF dir, media_type, raw name)
#[derive(Clone)]
pub(crate) struct ManifestItem {
    pub id: String,
    pub href: String,
    pub media_type: String,
}

pub(crate) fn opf_dir(opf: &str) -> String {
    match opf.rfind('/') {
        Some(i) => opf[..i].to_string(),
        None => String::new(),
    }
}

pub(crate) fn resolve_href(opf: &str, href: &str) -> String {
    let href = xml_unescape(href);
    let base = opf_dir(opf);
    if base.is_empty() || href.starts_with('/') {
        return href.trim_start_matches('/').to_string();
    }
    format!("{base}/{href}")
}

pub(crate) fn manifest_items(opf_xml: &str, opf: &str) -> Vec<ManifestItem> {
    let Ok(re) = regex::Regex::new(r"(?is)<item\b[^>]*/?>") else {
        return vec![];
    };
    let attr = |tag: &str, name: &str| -> String {
        let are =
            regex::Regex::new(&format!(r#"(?i){name}\s*=\s*"([^"]*)""#)).unwrap();
        are.captures(tag)
            .map(|c| c[1].to_string())
            .unwrap_or_default()
    };
    re.captures_iter(opf_xml)
        .map(|c| {
            let tag = c[0].to_string();
            ManifestItem {
                id: attr(&tag, "id"),
                href: resolve_href(opf, &attr(&tag, "href")),
                media_type: attr(&tag, "media-type"),
            }
        })
        .collect()
}

/// Spine order as resolved hrefs (documents only, following media-type).
pub(crate) fn spine_hrefs(opf_xml: &str, items: &[ManifestItem]) -> Vec<String> {
    let Ok(re) = regex::Regex::new(r#"(?is)<itemref\b[^>]*idref\s*=\s*"([^"]+)""#) else {
        return vec![];
    };
    re.captures_iter(opf_xml)
        .map(|c| {
            let idref = c[1].to_string();
            items
                .iter()
                .find(|it| it.id == idref)
                .map(|it| it.href.clone())
                .unwrap_or_default()
        })
        .filter(|h| !h.is_empty())
        .collect()
}

pub(crate) fn read_opf(epub: &mut EpubArchive) -> Option<String> {
    let name = find_zip_entry(&mut epub.zf, &epub.opf.clone())?;
    let bytes = read_zip_entry(&mut epub.zf, &name)?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Returns (opf_xml, manifest, spine hrefs).
pub(crate) fn epub_structure(epub: &mut EpubArchive) -> Option<(String, Vec<ManifestItem>, Vec<String>)> {
    let opf_xml = read_opf(epub)?;
    let items = manifest_items(&opf_xml, &epub.opf);
    let spine = spine_hrefs(&opf_xml, &items);
    Some((opf_xml, items, spine))
}

pub(crate) fn extract_epub_metadata(path: &str) -> EbookMeta {
    let mut meta = EbookMeta::unknown();
    let Some(mut epub) = open_epub(path) else {
        return fallback_metadata_from_filename(path);
    };
    let Some((opf_xml, _, _)) = epub_structure(&mut epub) else {
        return fallback_metadata_from_filename(path);
    };
    if let Some(t) = tag_content(&opf_xml, "dc:title").filter(|t| !t.is_empty()) {
        meta.title = t;
    }
    if let Some(a) = tag_content(&opf_xml, "dc:creator").filter(|a| !a.is_empty()) {
        meta.author = a;
    }
    if let Some(p) = tag_content(&opf_xml, "dc:publisher").filter(|p| !p.is_empty()) {
        meta.publisher = Some(p);
    }
    if let Some(d) = tag_content(&opf_xml, "dc:date").filter(|d| !d.is_empty()) {
        meta.published_date = Some(d);
    }
    if let Some(d) = tag_content(&opf_xml, "dc:description").filter(|d| !d.is_empty()) {
        meta.description = Some(d);
    }
    if let Some(l) = tag_content(&opf_xml, "dc:language").filter(|l| !l.is_empty()) {
        meta.language = l;
    }
    meta.subjects = tag_contents(&opf_xml, "dc:subject");
    for ident in tag_contents(&opf_xml, "dc:identifier") {
        let digits: String = ident.chars().filter(|c| c.is_ascii_digit()).collect();
        if ident.to_lowercase().contains("isbn") || (digits.len() == 10 || digits.len() == 13) {
            if digits.len() == 13 && meta.isbn13.is_none() {
                meta.isbn13 = Some(ident.clone());
            } else if digits.len() == 10 && meta.isbn.is_none() {
                meta.isbn = Some(ident.clone());
            }
        }
    }
    meta
}

pub(crate) fn extract_pdf_metadata(path: &str) -> EbookMeta {
    fallback_metadata_from_filename(path)
}

// ------------------------------------------------------------------ covers

fn cover_ext_for(name: &str, media_type: &str) -> &'static str {
    let n = name.to_lowercase();
    if n.ends_with(".png") || media_type.contains("png") {
        "png"
    } else if n.ends_with(".gif") || media_type.contains("gif") {
        "gif"
    } else {
        "jpg"
    }
}

fn save_cover_image(data: &[u8], path: &std::path::Path, ext: &str) {
    let Ok(img) = image::load_from_memory(data) else {
        let _ = std::fs::write(path, data);
        return;
    };
    let img = if img.height() > 600 {
        let ratio = 600.0 / img.height() as f32;
        img.resize(
            (img.width() as f32 * ratio).round() as u32,
            600,
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        img
    };
    let fmt = match ext {
        "png" => image::ImageFormat::Png,
        "gif" => image::ImageFormat::Gif,
        _ => image::ImageFormat::Jpeg,
    };
    let _ = img.save_with_format(path, fmt);
}

pub(crate) fn extract_epub_cover(path: &str, _book_id: i64) -> Option<String> {
    let mut epub = open_epub(path)?;
    let (opf_xml, items, _) = epub_structure(&mut epub)?;

    let attr_re = |tag: &str, name: &str| -> String {
        regex::Regex::new(&format!(r#"(?i){name}\s*=\s*"([^"]*)""#))
            .unwrap()
            .captures(tag)
            .map(|c| c[1].to_string())
            .unwrap_or_default()
    };

    // 1. OPF <meta name="cover" content="id"> or OPF <cover content="id">
    let mut cover_href: Option<String> = None;
    let meta_re =
        regex::Regex::new(r"(?is)<meta\b[^>]*/?>").unwrap();
    for c in meta_re.captures_iter(&opf_xml) {
        let tag = c[0].to_string();
        let name = attr_re(&tag, "name");
        if name.eq_ignore_ascii_case("cover") {
            let cid = attr_re(&tag, "content");
            if let Some(item) = items.iter().find(|it| it.id == cid) {
                cover_href = Some(item.href.clone());
            }
        }
    }
    if cover_href.is_none() {
        if let Ok(cover_re) = regex::Regex::new(r"(?is)<cover\b[^>]*>") {
            if let Some(c) = cover_re.captures(&opf_xml) {
                let cid = attr_re(&c[0], "content");
                if let Some(item) = items.iter().find(|it| it.id == cid) {
                    cover_href = Some(item.href.clone());
                }
            }
        }
    }

    // 2. If the cover item is an HTML document, pull the first <img src>.
    if let Some(href) = cover_href.clone() {
        if !href.is_empty() && matches!(href.rsplit('.').next().unwrap_or(""), "xhtml" | "html" | "htm" | "xml") {
            if let Some(entry) = find_zip_entry(&mut epub.zf, &href) {
                if let Some(bytes) = read_zip_entry(&mut epub.zf, &entry) {
                    let html = String::from_utf8_lossy(&bytes).into_owned();
                    if let Some(m) =
                        regex::Regex::new(r#"(?is)<img\b[^>]*src\s*=\s*"([^"]+)""#).ok().and_then(|re| re.captures(&html))
                    {
                        let src = xml_unescape(&m[1]);
                        let src_name = src.rsplit('/').next().unwrap_or(&src);
                        if let Some(img_item) = items
                            .iter()
                            .find(|it| {
                                it.media_type.starts_with("image/")
                                                    && it.href.rsplit('/').next().map(|n| n == src_name).unwrap_or(false)
                            })
                        {
                            cover_href = Some(img_item.href.clone());
                        }
                    }
                }
            }
        }
    }

    // 3. Fallback: image entry with "cover" in the name.
    if cover_href.is_none() {
        cover_href = items
            .iter()
            .filter(|it| it.media_type.starts_with("image/"))
            .find(|it| it.href.to_lowercase().contains("cover"))
            .map(|it| it.href.clone());
    }
    // 4. Last resort: first image entry.
    if cover_href.is_none() {
        cover_href = items
            .iter()
            .find(|it| it.media_type.starts_with("image/"))
            .map(|it| it.href.clone());
    }

    let href = cover_href?;
    let entry = find_zip_entry(&mut epub.zf, &href)?;
    let data = read_zip_entry(&mut epub.zf, &entry)?;
    if data.len() < 100 {
        return None;
    }
    let is_jpg = data.starts_with(&[0xFF, 0xD8, 0xFF]);
    let is_png = data.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    let is_gif = data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a");
    if !(is_jpg || is_png || is_gif) {
        return None;
    }
    let ext = cover_ext_for(&entry, "");
    let out = sibling_cover_path(path);
    save_cover_image(&data, &out, ext);
    Some(COVER_SIBLING.to_string())
}

/// PDF cover via pdftoppm (poppler) when available.
pub(crate) fn extract_pdf_cover(path: &str, _book_id: i64) -> Option<String> {
    let prefix = format!("{path}.cover");
    let status = std::process::Command::new("pdftoppm")
        .arg("-jpeg")
        .arg("-f").arg("1")
        .arg("-l").arg("1")
        .arg("-scale-to").arg("600")
        .arg("-singlefile")
        .arg(path)
        .arg(&prefix)
        .output();
    let jpg = std::path::PathBuf::from(format!("{prefix}.jpg"));
    if !matches!(status, Ok(o) if o.status.success()) || !jpg.is_file() {
        let _ = std::fs::remove_file(&jpg);
        return None;
    }
    Some(COVER_SIBLING.to_string())
}

/// Async wrapper: run cover extraction off the async runtime.
pub(crate) async fn extract_cover(path: String, book_id: i64, file_type: String) -> Option<String> {
    tokio::task::spawn_blocking(move || {
        if file_type == "epub" {
            extract_epub_cover(&path, book_id)
        } else {
            extract_pdf_cover(&path, book_id)
        }
    })
    .await
    .ok()
    .flatten()
}

/// Async metadata extraction.
pub(crate) async fn extract_metadata(path: String, file_type: String) -> EbookMeta {
    tokio::task::spawn_blocking(move || {
        if file_type == "epub" {
            extract_epub_metadata(&path)
        } else {
            extract_pdf_metadata(&path)
        }
    })
    .await
    .unwrap_or_else(|_| EbookMeta::unknown())
}

// ----------------------------------------------------------- reader (chapters)

fn html_title(html: &str) -> Option<String> {
    for tag in ["h1", "h2", "h3", "title"] {
        if let Some(t) = tag_content(html, tag) {
            let text = strip_tags(&t);
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    None
}

fn strip_tags(s: &str) -> String {
    regex::Regex::new(r"(?is)<[^>]+>")
        .map(|re| re.replace_all(s, "").into_owned())
        .unwrap_or_else(|_| s.to_string())
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn body_html(html: &str) -> String {
    if let Ok(re) = regex::Regex::new(r"(?is)<body\b[^>]*>(.*?)</body>") {
        if let Some(c) = re.captures(html) {
            return format!("<body>{}</body>", &c[1]);
        }
    }
    html.to_string()
}

/// TOC from the spine: [{id, title, path}]
pub(crate) fn extract_epub_toc(path: &str) -> Vec<Value> {
    let mut epub = match open_epub(path) {
        Some(e) => e,
        None => return vec![],
    };
    let (_, _, spine) = match epub_structure(&mut epub) {
        Some(s) => s,
        None => return vec![],
    };
    spine
        .iter()
        .enumerate()
        .filter_map(|(idx, href)| {
            let entry = find_zip_entry(&mut epub.zf, href)?;
            let bytes = read_zip_entry(&mut epub.zf, &entry)?;
            let html = String::from_utf8_lossy(&bytes).into_owned();
            let title = html_title(&html)
                .unwrap_or_else(|| href.rsplit('/').next().unwrap_or(href).to_string());
            Some(json!({"id": idx, "title": title, "path": href}))
        })
        .collect()
}

/// Chapter data for the reader: {id, title, content, path, has_next, has_prev}
pub(crate) fn get_epub_chapter(path: &str, chapter_id: i64) -> Option<Value> {
    let mut epub = open_epub(path)?;
    let (_, _, spine) = epub_structure(&mut epub)?;
    if chapter_id < 0 || chapter_id as usize >= spine.len() {
        return None;
    }
    let href = spine[chapter_id as usize].clone();
    let entry = find_zip_entry(&mut epub.zf, &href)?;
    let bytes = read_zip_entry(&mut epub.zf, &entry)?;
    let html = String::from_utf8_lossy(&bytes).into_owned();
    let title =
        html_title(&html).unwrap_or_else(|| format!("Chapter {}", chapter_id + 1));
    Some(json!({
        "id": chapter_id,
        "title": title,
        "content": body_html(&html),
        "path": href,
        "has_next": (chapter_id as usize) < spine.len() - 1,
        "has_prev": chapter_id > 0,
    }))
}

// ------------------------------------------------------------------ rows / db

fn topic_row(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<String,_>("id"),
        "user_id": r.get::<i64,_>("user_id"),
        "name": r.try_get::<String,_>("name").unwrap_or_default(),
        "normalized_name": r.try_get::<String,_>("normalized_name").unwrap_or_default(),
        "description": r.try_get::<Option<String>,_>("description").ok().flatten(),
        "is_public": r.get::<i64,_>("is_public") == 1,
        "directory_path": r.try_get::<String,_>("directory_path").unwrap_or_default(),
        "book_count": r.get::<i64,_>("book_count"),
        "parent_id": r.try_get::<Option<String>,_>("parent_id").ok().flatten(),
        "created_at": r.try_get::<Option<String>,_>("created_at").ok().flatten(),
        "updated_at": r.try_get::<Option<String>,_>("updated_at").ok().flatten(),
    })
}

fn book_row(r: &sqlx::sqlite::SqliteRow) -> Value {
    let size = r.try_get::<Option<i64>,_>("file_size").ok().flatten();
    let author_name = r
        .try_get::<Option<String>,_>("author_name")
        .ok()
        .flatten()
        .unwrap_or_else(|| "Unknown".into());
    json!({
        "id": r.get::<i64,_>("id"),
        "title": r.try_get::<String,_>("title").unwrap_or_default(),
        "normalized_title": r.try_get::<String,_>("normalized_title").unwrap_or_default(),
        "isbn": r.try_get::<Option<String>,_>("isbn").ok().flatten(),
        "isbn13": r.try_get::<Option<String>,_>("isbn13").ok().flatten(),
        "author_id": r.get::<i64,_>("author_id"),
        "author": {"id": r.get::<i64,_>("author_id"), "name": author_name},
        "topic_id": r.try_get::<Option<String>,_>("topic_id").ok().flatten(),
        "user_id": r.try_get::<Option<i64>,_>("user_id").ok().flatten(),
        "file_type": r.try_get::<String,_>("file_type").unwrap_or_default(),
        "file_path": r.try_get::<String,_>("file_path").unwrap_or_default(),
        "file_size": size,
        "size_human": size.map(human_size).unwrap_or_default(),
        "cover_filename": r.try_get::<Option<String>,_>("cover_filename").ok().flatten(),
        "publisher": r.try_get::<Option<String>,_>("publisher").ok().flatten(),
        "published_date": r.try_get::<Option<String>,_>("published_date").ok().flatten(),
        "description": r.try_get::<Option<String>,_>("description").ok().flatten(),
        "language": r.try_get::<String,_>("language").unwrap_or_else(|_| "en".into()),
        "is_public": r.get::<i64,_>("is_public") == 1,
        "is_banned": r.get::<i64,_>("is_banned") == 1,
        "created_at": r.try_get::<Option<String>,_>("created_at").ok().flatten(),
    })
}

const BOOK_SELECT: &str = "SELECT b.*, a.name AS author_name FROM ebook_books b \
     LEFT JOIN ebook_authors a ON a.id = b.author_id";

async fn get_topic(db: &syarc_plugin_api::Db, id: &str) -> Option<Value> {
    sqlx::query(
        "SELECT t.*, u.username AS owner_username FROM ebook_topics t \
         JOIN users u ON u.id = t.user_id WHERE t.id = ?",
    )
    .bind(id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .map(|r| {
        let mut t = topic_row(&r);
        t["owner"] =
            json!({"username": r.try_get::<String, _>("owner_username").unwrap_or_default()});
        t
    })
}

async fn get_book(db: &syarc_plugin_api::Db, id: i64) -> Option<Value> {
    sqlx::query(&format!("{BOOK_SELECT} WHERE b.id = ?"))
        .bind(id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|r| book_row(&r))
}

async fn fetch_books(db: &syarc_plugin_api::Db, where_sql: &str, args: &[&str]) -> Vec<Value> {
    // Callers may append "LIMIT ? OFFSET ?" to where_sql; splice ORDER BY in
    // front of it so the SQL stays valid.
    let tail = if let Some(pos) = where_sql.to_uppercase().find("LIMIT") {
        format!("ORDER BY b.title {}", &where_sql[pos..])
    } else {
        "ORDER BY b.title".to_string()
    };
    let head = if let Some(pos) = where_sql.to_uppercase().find("LIMIT") {
        &where_sql[..pos]
    } else {
        where_sql
    };
    let sql = format!("{BOOK_SELECT} {head} {tail}");
    let mut q = sqlx::query(&sql);
    for a in args {
        q = q.bind(a);
    }
    match q.fetch_all(db).await {
        Ok(rows) => rows.iter().map(book_row).collect(),
        Err(e) => {
            eprintln!("[library] fetch_books error: {e} (sql: {sql})");
            Vec::new()
        }
    }
}

async fn count_scalar(db: &syarc_plugin_api::Db, sql: &str, arg: Option<&str>) -> i64 {
    let mut q = sqlx::query(sql);
    if let Some(a) = arg {
        q = q.bind(a);
    }
    q.fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|r| r.get::<i64,_>(0))
        .unwrap_or(0)
}

pub(crate) async fn get_or_create_author(db: &syarc_plugin_api::Db, name: &str) -> i64 {
    let name = sanitize_str(name);
    let name = if name.is_empty() { "Unknown Author".to_string() } else { name };
    let norm = name.to_lowercase().trim().to_string();
    if let Some(r) = sqlx::query("SELECT id FROM ebook_authors WHERE normalized_name = ?")
        .bind(&norm)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
    {
        return r.get::<i64,_>("id");
    }
    let _ = sqlx::query("INSERT INTO ebook_authors (name, normalized_name) VALUES (?, ?)")
        .bind(&name)
        .bind(&norm)
        .execute(db)
        .await;
    count_scalar(db, "SELECT id FROM ebook_authors WHERE normalized_name = ?", Some(&norm)).await
}

async fn get_or_create_category(db: &syarc_plugin_api::Db, name: &str) -> Option<i64> {
    let name = sanitize_str(name);
    if name.is_empty() {
        return None;
    }
    let norm = name.to_lowercase().trim().to_string();
    if let Some(r) = sqlx::query("SELECT id FROM ebook_categories WHERE normalized_name = ?")
        .bind(&norm)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
    {
        return Some(r.get::<i64,_>("id"));
    }
    let _ = sqlx::query("INSERT OR IGNORE INTO ebook_categories (name, normalized_name) VALUES (?, ?)")
        .bind(&name)
        .bind(&norm)
        .execute(db)
        .await;
    sqlx::query("SELECT id FROM ebook_categories WHERE normalized_name = ?")
        .bind(&norm)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|r| r.get::<i64,_>("id"))
}

pub(crate) async fn refresh_topic_count(db: &syarc_plugin_api::Db, topic_id: &str) {
    let count = count_scalar(
        db,
        "SELECT COUNT(*) FROM ebook_books WHERE topic_id = ?",
        Some(topic_id),
    ).await;
    let _ = sqlx::query("UPDATE ebook_topics SET book_count = ?, updated_at = ? WHERE id = ?")
        .bind(count)
        .bind(now_str())
        .bind(topic_id)
        .execute(db)
        .await;
}

/// Insert a book row; returns the new id.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn insert_book(
    db: &syarc_plugin_api::Db,
    meta: &EbookMeta,
    author_id: i64,
    file_type: &str,
    file_path: &str,
    topic_id: Option<&str>,
    user_id: Option<i64>,
    is_public: bool,
) -> i64 {
    let file_size = std::fs::metadata(file_path).ok().map(|m| m.len() as i64);
    let now = now_str();
    let _ = sqlx::query(
        "INSERT INTO ebook_books (title, normalized_title, isbn, isbn13, author_id, topic_id, \
         user_id, file_type, file_path, file_size, publisher, published_date, description, \
         language, is_public, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&meta.title)
    .bind(meta.title.to_lowercase().trim())
    .bind(&meta.isbn)
    .bind(&meta.isbn13)
    .bind(author_id)
    .bind(topic_id)
    .bind(user_id)
    .bind(file_type)
    .bind(file_path)
    .bind(file_size)
    .bind(&meta.publisher)
    .bind(&meta.published_date)
    .bind(&meta.description)
    .bind(if meta.language.is_empty() { "en".to_string() } else { meta.language.clone() })
    .bind(is_public as i64)
    .bind(&now)
    .bind(&now)
    .execute(db)
    .await;
    count_scalar(db, "SELECT id FROM ebook_books WHERE file_path = ?", Some(file_path)).await
}

/// Bind epub subject categories for a book.
pub(crate) async fn bind_categories(db: &syarc_plugin_api::Db, book_id: i64, subjects: &[String]) {
    let mut added = std::collections::HashSet::new();
    for subject in subjects {
        let Some(cat_id) = get_or_create_category(db, subject).await else {
            continue;
        };
        if added.insert(cat_id) {
            let _ = sqlx::query(
                "INSERT OR IGNORE INTO ebook_book_categories (book_id, category_id) VALUES (?, ?)",
            )
            .bind(book_id)
            .bind(cat_id)
            .execute(db)
            .await;
        }
    }
}

// --------------------------------------------------------------- access

pub(crate) fn public_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(root("ebook")).join("public")
}

fn topic_dir_for(username: &str, name: &str) -> String {
    std::path::Path::new(&root("ebook"))
        .join(username)
        .join(name)
        .to_string_lossy()
        .into_owned()
}

async fn accessible_topic_ids(db: &syarc_plugin_api::Db, user_id: i64) -> Vec<String> {
    let own: Vec<String> = sqlx::query("SELECT id FROM ebook_topics WHERE user_id = ?")
        .bind(user_id)
        .fetch_all(db)
        .await
        .map(|rows| rows.iter().map(|r| r.get::<String,_>("id")).collect())
        .unwrap_or_default();
    let shared: Vec<String> =
        sqlx::query("SELECT topic_id FROM ebook_topic_shares WHERE shared_with_user_id = ?")
            .bind(user_id)
            .fetch_all(db)
            .await
            .map(|rows| rows.iter().map(|r| r.get::<String,_>("topic_id")).collect())
            .unwrap_or_default();
    let public: Vec<String> = sqlx::query("SELECT id FROM ebook_topics WHERE is_public = 1")
        .fetch_all(db)
        .await
        .map(|rows| rows.iter().map(|r| r.get::<String,_>("id")).collect())
        .unwrap_or_default();
    let mut ids: Vec<String> = own;
    ids.extend(shared);
    ids.extend(public);
    ids.sort();
    ids.dedup();
    ids
}

/// SQL fragment restricting books to topics the user can see (non-super).
fn visibility_where(ids: &[String]) -> String {
    if ids.is_empty() {
        " WHERE (b.is_public = 1 AND b.is_banned = 0)".to_string()
    } else {
        let quoted: Vec<String> = ids
            .iter()
            .map(|id| format!("'{}'", id.replace('\'', "")))
            .collect();
        format!(
            " WHERE (b.topic_id IN ({}) OR (b.is_public = 1 AND b.is_banned = 0))",
            quoted.join(",")
        )
    }
}

async fn user_has_topic_access(db: &syarc_plugin_api::Db, topic_id: &str, user_id: i64) -> bool {
    let Ok(Some(r)) = sqlx::query("SELECT user_id, is_public FROM ebook_topics WHERE id = ?")
        .bind(topic_id)
        .fetch_optional(db)
        .await
    else {
        return false;
    };
    if r.get::<i64,_>("user_id") == user_id || r.get::<i64,_>("is_public") == 1 {
        return true;
    }
    sqlx::query("SELECT 1 FROM ebook_topic_shares WHERE topic_id = ? AND shared_with_user_id = ?")
        .bind(topic_id)
        .bind(user_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .is_some()
}

async fn user_is_topic_owner(db: &syarc_plugin_api::Db, topic_id: &str, user_id: i64) -> bool {
    sqlx::query("SELECT 1 FROM ebook_topics WHERE id = ? AND user_id = ?")
        .bind(topic_id)
        .bind(user_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .is_some()
}

async fn make_topic_public(db: &syarc_plugin_api::Db, topic_id: &str) {
    let _ = sqlx::query("UPDATE ebook_topics SET is_public = 1, updated_at = ? WHERE id = ?")
        .bind(now_str())
        .bind(topic_id)
        .execute(db)
        .await;
    if let Some(r) = sqlx::query(
        "SELECT u.username, t.name FROM ebook_topics t \
         JOIN users u ON u.id = t.user_id WHERE t.id = ?",
    )
    .bind(topic_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    {
        let link = public_dir().join(format!(
            "{}_{}",
            r.try_get::<String,_>("username").unwrap_or_default(),
            r.try_get::<String,_>("name").unwrap_or_default()
        ));
        if link.is_symlink() {
            let _ = std::fs::remove_file(&link);
        }
    }
    let _ = sqlx::query(
        "UPDATE ebook_books SET is_public = 1 WHERE topic_id = ? AND is_banned = 0",
    )
    .bind(topic_id)
    .execute(db)
    .await;
}

async fn make_topic_private(db: &syarc_plugin_api::Db, topic_id: &str) {
    let _ = sqlx::query("UPDATE ebook_topics SET is_public = 0, updated_at = ? WHERE id = ?")
        .bind(now_str())
        .bind(topic_id)
        .execute(db)
        .await;
    if let Some(r) = sqlx::query(
        "SELECT u.username, t.name FROM ebook_topics t JOIN users u ON u.id = t.user_id WHERE t.id = ?",
    )
    .bind(topic_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    {
        let link = public_dir().join(format!(
            "{}_{}",
            r.try_get::<String,_>("username").unwrap_or_default(),
            r.try_get::<String,_>("name").unwrap_or_default()
        ));
        if link.is_symlink() {
            let _ = std::fs::remove_file(&link);
        }
    }
    let _ = sqlx::query("UPDATE ebook_books SET is_public = 0 WHERE topic_id = ?")
        .bind(topic_id)
        .execute(db)
        .await;
}

async fn reading_lists_for_dropdown(db: &syarc_plugin_api::Db, user_id: i64) -> Vec<Value> {
    sqlx::query(
        "SELECT name, MAX(created_at) AS latest FROM ebook_reading_lists \
         WHERE user_id = ? GROUP BY name ORDER BY latest DESC",
    )
    .bind(user_id)
    .fetch_all(db)
    .await
    .map(|rows| {
        rows.iter()
            .map(|r| {
                json!({"name": r.get::<String,_>("name"), "created_at": r.try_get::<Option<String>,_>("latest").ok().flatten()})
            })
            .collect()
    })
    .unwrap_or_default()
}

// ------------------------------------------------------------------ router

pub fn router() -> Router<SharedState> {
    Router::new()
        .route("/library/ebook", get(index))
        .route("/library/ebook/", get(index))
        .route("/library/ebook/browse/users", get(browse_users))
        .route("/library/ebook/browse/users/{user_id}", get(browse_user_topics))
        .route("/library/ebook/browse/topics", get(browse_topics))
        .route("/library/ebook/browse/authors", get(browse_authors))
        .route("/library/ebook/browse/books", get(browse_books))
        .route("/library/ebook/browse/categories", get(browse_categories))
        .route("/library/ebook/topic/new", get(new_topic_page).post(create_topic))
        .route("/library/ebook/topic/{topic_id}", get(view_topic))
        .route("/library/ebook/topic/{topic_id}/edit", get(edit_topic_page).post(update_topic))
        .route("/library/ebook/topic/{topic_id}/delete", post(delete_topic))
        .route("/library/ebook/topic/{topic_id}/share", post(share_topic))
        .route("/library/ebook/topic/{topic_id}/revoke/{share_id}", post(revoke_topic_share))
        .route("/library/ebook/topic/{topic_id}/import", get(import_page))
        .route("/library/ebook/topic/{topic_id}/import-path", post(import_from_path))
        .route("/library/ebook/topic/{topic_id}/rescan", post(rescan_topic))
        .route("/library/ebook/topic/{topic_id}/toggle-visibility", post(toggle_topic_visibility))
        .route("/library/ebook/book/{book_id}", get(view_book))
        .route("/library/ebook/book/{book_id}/edit", get(edit_book_page))
        .route("/library/ebook/book/edit", post(update_book))
        .route("/library/ebook/book/{book_id}/move", post(move_book))
        .route("/library/ebook/book/{book_id}/delete", post(delete_book))
        .route("/library/ebook/book/{book_id}/make-public", post(user_make_book_public))
        .route("/library/ebook/book/{book_id}/remove-public", post(user_remove_book_public))
        .route("/library/ebook/book/{book_id}/toggle-visibility", post(toggle_book_visibility))
        .route("/library/ebook/book/{book_id}/convert-to-epub", post(convert_pdf_to_epub))
        .route("/library/ebook/download/{book_id}", get(download_book))
        .route("/library/ebook/read/{book_id}", get(read_book))
        .route("/library/ebook/read/{book_id}/image/{*img_path}", get(get_epub_image))
        .route("/library/ebook/search", get(search))
        .route("/library/ebook/reading-lists", get(list_reading_lists))
        .route("/library/ebook/reading-lists/create", post(create_reading_list))
        .route("/library/ebook/reading-lists/{reading_list_name}", get(view_reading_list))
        .route("/library/ebook/reading-lists/{reading_list_name}/add", post(add_to_reading_list))
        .route("/library/ebook/reading-lists/{reading_list_name}/delete", post(delete_reading_list))
        .route("/library/ebook/reading-lists/{reading_list_name}/item/{item_id}/delete", post(delete_reading_list_item))
        .route("/library/ebook/reading-lists/{reading_list_name}/analyze", get(analyze_reading_list))
        .route("/library/ebook/reading-lists/{reading_list_name}/export", get(export_reading_list))
        .route("/library/ebook/author/{author_id}", get(view_author))
        .route("/library/ebook/add", post(add_book_quick))
        .route("/library/ebook/cover/{book_id}", get(cover_by_book))
        .route("/library/ebook/admin/clean", post(admin_clean_rescan))
        .route("/library/ebook/admin/rebuild-covers", post(admin_rebuild_covers))
        .route("/library/ebook/scan", get(trigger_scan))
        .route("/library/ebook/scan/my-library", post(scan_my_library))
        .route("/library/ebook/scan/force", get(force_scan))
        .route("/library/ebook/admin", get(admin_page))
        .route("/library/ebook/admin/assign-orphan-topics", post(admin_assign_orphan_topics))
        .route("/library/ebook/admin/book/{book_id}/ban", post(ban_book))
        .route("/library/ebook/admin/book/{book_id}/unban", post(unban_book))
        .route("/library/ebook/admin/book/{book_id}/remove-public", post(admin_remove_public_book))
        .route("/library/ebook/admin/topic/{topic_id}/remove-public", post(admin_remove_public_topic))
        .route("/library/ebook/admin/topic/{topic_id}/toggle-visibility", post(admin_toggle_topic_visibility))
        .route("/library/ebook/admin/topic/{topic_id}/rescan", post(admin_rescan_topic))
        .route("/library/ebook/admin/topics/purge-empty", post(admin_purge_empty_topics))
        .route("/library/ebook/admin/topics/all-public", post(admin_all_topics_public))
        .route("/library/ebook/admin/topics/all-private", post(admin_all_topics_private))
        .route("/library/ebook_static/covers/{file}", get(static_cover))
}

#[derive(Deserialize)]
struct PageParams {
    #[serde(default = "one")]
    page: i64,
    #[serde(default)]
    per_page: Option<i64>,
    #[serde(default)]
    letter: Option<String>,
    #[serde(default)]
    from_letter: Option<String>,
}

fn one() -> i64 {
    1
}

fn page_params(p: &PageParams, default_per: i64, min: i64, max: i64) -> (i64, i64, i64, i64) {
    let page = p.page.max(1);
    let per_page = p.per_page.unwrap_or(default_per).clamp(min, max);
    let offset = (page - 1) * per_page;
    (page, per_page, offset, (page) * per_page - per_page)
}

// ------------------------------------------------------------------ index

async fn index(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    let db = &state.db;
    let full_access = is_super(&user);
    let write_access = is_super(&user);

    let own_topics: Vec<Value> = if write_access {
        sqlx::query(
            "SELECT * FROM ebook_topics WHERE user_id = ? AND parent_id IS NULL ORDER BY name",
        )
        .bind(user.id)
        .fetch_all(db)
        .await
        .map(|rows| rows.iter().map(topic_row).collect())
        .unwrap_or_default()
    } else {
        vec![]
    };

    let shared_topics: Vec<Value> = if write_access {
        sqlx::query(
            "SELECT t.*, u.username AS owner_username FROM ebook_topic_shares s \
             JOIN ebook_topics t ON t.id = s.topic_id \
             JOIN users u ON u.id = t.user_id \
             WHERE s.shared_with_user_id = ? ORDER BY t.name",
        )
        .bind(user.id)
        .fetch_all(db)
        .await
        .map(|rows| {
            rows.iter()
                .map(|r| {
                    let mut t = topic_row(r);
                    t["owner_username"] = json!(
                        r.try_get::<String,_>("owner_username").unwrap_or_default()
                    );
                    t
                })
                .collect()
        })
        .unwrap_or_default()
    } else {
        vec![]
    };

    let total_books = count_scalar(
        db,
        "SELECT COUNT(*) FROM ebook_books b JOIN ebook_topics t ON b.topic_id = t.id WHERE t.user_id = ?",
        Some(&user.id.to_string()),
    ).await;
    let public_topic_count = count_scalar(db, "SELECT COUNT(*) FROM ebook_topics WHERE is_public = 1", None).await;
    let public_books_count = count_scalar(
        db,
        "SELECT COUNT(*) FROM ebook_books b JOIN ebook_topics t ON b.topic_id = t.id \
         WHERE t.is_public = 1 AND b.is_public = 1 AND b.is_banned = 0",
        None,
    ).await;
    let legacy_book_count = count_scalar(db, "SELECT COUNT(*) FROM ebook_books WHERE topic_id IS NULL", None).await;

    let vis = if is_super(&user) {
        String::new()
    } else {
        visibility_where(&accessible_topic_ids(db, user.id).await)
    };
    let all_books_count = count_scalar(db, &format!("SELECT COUNT(*) FROM ebook_books b{vis}"), None).await;
    let total_authors = count_scalar(
        db,
        &format!("SELECT COUNT(DISTINCT b.author_id) FROM ebook_books b{vis}"),
        None,
    ).await;
    let total_categories = count_scalar(
        db,
        &format!(
            "SELECT COUNT(DISTINCT c.category_id) FROM ebook_book_categories c \
             JOIN ebook_books b ON b.id = c.book_id{vis}"
        ),
        None,
    ).await;

    page(
        &state,
        "ebook/index.html",
        json!({
            "user": crate::core::user_json(&user),
            "own_topics": own_topics,
            "shared_topics": shared_topics,
            "total_books": total_books,
            "public_topic_count": public_topic_count,
            "public_books_count": public_books_count,
            "legacy_book_count": legacy_book_count,
            "total_authors": total_authors,
            "total_categories": total_categories,
            "all_books_count": all_books_count,
            "full_access": full_access,
            "write_access": write_access,
        }),
    )
}

// ------------------------------------------------------------------ browse

async fn browse_users(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    let db = &state.db;
    let sql = if is_super(&user) {
        "SELECT t.user_id, u.username, COUNT(*) AS topic_count FROM ebook_topics t \
         JOIN users u ON u.id = t.user_id GROUP BY t.user_id, u.username ORDER BY u.username"
    } else {
        "SELECT t.user_id, u.username, COUNT(*) AS topic_count FROM ebook_topics t \
         JOIN users u ON u.id = t.user_id WHERE t.is_public = 1 \
         GROUP BY t.user_id, u.username ORDER BY u.username"
    };
    let users: Vec<Value> = sqlx::query(sql)
        .fetch_all(db)
        .await
        .map(|rows| {
            rows.iter()
                .map(|r| {
                    json!({
                        "user_id": r.get::<i64,_>("user_id"),
                        "username": r.try_get::<String,_>("username").unwrap_or_default(),
                        "topic_count": r.get::<i64,_>("topic_count"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    page(
        &state,
        "ebook/browse_users.html",
        json!({"user": crate::core::user_json(&user), "users_with_topics": users}),
    )
}

async fn browse_user_topics(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(user_id): Path<i64>,
) -> Response {
    let db = &state.db;
    let Some(target) = sqlx::query("SELECT id, username FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
    else {
        return err404("User not found");
    };
    let target_username = target.get::<String,_>("username");

    let topics: Vec<Value> = if user_id == user.id && is_super(&user) {
        sqlx::query("SELECT * FROM ebook_topics WHERE user_id = ? AND parent_id IS NULL ORDER BY name")
            .bind(user_id)
            .fetch_all(db)
            .await
            .map(|rows| rows.iter().map(topic_row).collect())
            .unwrap_or_default()
    } else {
        sqlx::query(
            "SELECT * FROM ebook_topics WHERE user_id = ? AND parent_id IS NULL AND \
             (is_public = 1 OR id IN \
               (SELECT topic_id FROM ebook_topic_shares WHERE shared_with_user_id = ?)) \
             ORDER BY name",
        )
        .bind(user_id)
        .bind(user.id)
        .fetch_all(db)
        .await
        .map(|rows| rows.iter().map(topic_row).collect())
        .unwrap_or_default()
    };

    let mut topic_book_counts = serde_json::Map::new();
    for t in &topics {
        let id = t["id"].as_str().unwrap_or("").to_string();
        let count = count_scalar(
            db,
            "SELECT COUNT(*) FROM ebook_books WHERE topic_id = ?",
            Some(&id),
        ).await;
        topic_book_counts.insert(id, json!(count));
    }

    page(
        &state,
        "ebook/browse_user_topics.html",
        json!({
            "user": crate::core::user_json(&user),
            "target_user": {"id": user_id, "username": target_username},
            "topics": topics,
            "topic_book_counts": Value::Object(topic_book_counts),
        }),
    )
}

async fn browse_topics(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(p): Query<PageParams>,
) -> Response {
    let db = &state.db;
    let (page_no, per_page, offset, _) = page_params(&p, 24, 12, 120);
    let ids = accessible_topic_ids(db, user.id).await;
    let quoted: Vec<String> = ids.iter().map(|i| format!("'{}'", i.replace('\'', ""))).collect();
    let in_list = if quoted.is_empty() { "''".to_string() } else { quoted.join(",") };

    let total = count_scalar(
        db,
        &format!("SELECT COUNT(*) FROM ebook_topics WHERE id IN ({in_list})"),
        None,
    ).await;
    let total_pages = ((total) + per_page - 1) / per_page;
    let mut topics: Vec<Value> = Vec::new();
    {
    let raw_rows = sqlx::query(&format!(
        "SELECT t.*, u.username AS owner_username FROM ebook_topics t \
         JOIN users u ON u.id = t.user_id \
         WHERE t.id IN ({in_list}) AND t.parent_id IS NULL \
         ORDER BY t.name LIMIT ? OFFSET ?"
    ))
    .bind(per_page)
    .bind(offset)
    .fetch_all(db)
    .await
    .unwrap_or_default();
    for r in &raw_rows {
        let t = topic_row(r);
        let id = t["id"].as_str().unwrap_or("").to_string();
        let books = count_scalar(
            db,
            "SELECT COUNT(*) FROM ebook_books WHERE topic_id = ?",
            Some(&id),
        ).await;
        let subs = count_scalar(
            db,
            "SELECT COUNT(*) FROM ebook_topics WHERE parent_id = ?",
            Some(&id),
        ).await;
        topics.push(json!({
            "topic": t,
            "owner_username": r.try_get::<String,_>("owner_username").unwrap_or_default(),
            "visible_book_count": books,
            "subtopic_count": subs,
        }));
    }
    }

    page(
        &state,
        "ebook/browse_topics.html",
        json!({
            "user": crate::core::user_json(&user),
            "topics": topics,
            "current_page": page_no,
            "total_pages": total_pages,
            "total": total,
            "per_page": per_page,
            "full_access": is_super(&user),
        }),
    )
}

async fn browse_authors(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(p): Query<PageParams>,
) -> Response {
    let db = &state.db;
    let (page_no, per_page, offset, _) = page_params(&p, 50, 50, 200);
    let letter = p
        .letter
        .clone()
        .filter(|l| l.chars().count() == 1 && l.chars().all(|c| c.is_alphabetic()))
        .map(|l| l.to_uppercase());

    let like = letter.clone().map(|l| format!("{l}%"));
    let total = count_scalar(
        db,
        "SELECT COUNT(*) FROM ebook_authors WHERE (? IS NULL OR name LIKE ?)",
        like.as_deref(),
    ).await;
    let authors: Vec<Value> = sqlx::query(
        "SELECT * FROM ebook_authors WHERE (? IS NULL OR name LIKE ?) ORDER BY name LIMIT ? OFFSET ?",
    )
    .bind(like.as_deref())
    .bind(like.as_deref())
    .bind(per_page)
    .bind(offset)
    .fetch_all(db)
    .await
    .map(|rows| {
        rows.iter()
            .map(|r| {
                json!({
                    "id": r.get::<i64,_>("id"),
                    "name": r.try_get::<String,_>("name").unwrap_or_default(),
                    "normalized_name": r.try_get::<String,_>("normalized_name").unwrap_or_default(),
                })
            })
            .collect()
    })
    .unwrap_or_default();

    let vis = if is_super(&user) {
        String::new()
    } else {
        visibility_where(&accessible_topic_ids(db, user.id).await)
            .replacen(" WHERE ", " AND ", 1)
    };
    let mut counts = serde_json::Map::new();
    for a in &authors {
        let id = a["id"].as_i64().unwrap_or(0);
        let count = count_scalar(
            db,
            &format!("SELECT COUNT(*) FROM ebook_books b{vis} AND b.author_id = ?"),
            Some(&id.to_string()),
        ).await;
        counts.insert(id.to_string(), json!(count));
    }

    page(
        &state,
        "ebook/browse_authors.html",
        json!({
            "user": crate::core::user_json(&user),
            "authors": authors,
            "author_book_counts": Value::Object(counts),
            "current_letter": letter,
            "current_page": page_no,
            "total_pages": ((total) + per_page - 1) / per_page,
            "total": total,
            "per_page": per_page,
        }),
    )
}

#[derive(Deserialize)]
struct BrowseBooksParams {
    #[serde(default = "one")]
    page: i64,
    #[serde(default)]
    per_page: Option<i64>,
    #[serde(default)]
    letter: Option<String>,
    #[serde(default)]
    author_id: Option<i64>,
    #[serde(default)]
    category_id: Option<i64>,
    #[serde(default)]
    file_type: Option<String>,
}

async fn browse_books(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(p): Query<BrowseBooksParams>,
) -> Response {
    let db = &state.db;
    let (page_no, per_page, offset) = (
        p.page.max(1),
        p.per_page.unwrap_or(24).clamp(24, 120),
        (p.page.max(1) - 1) * p.per_page.unwrap_or(24).clamp(24, 120),
    );
    let letter = p
        .letter
        .clone()
        .filter(|l| l.chars().count() == 1 && l.chars().all(|c| c.is_alphabetic()))
        .map(|l| l.to_uppercase());

    let vis = if is_super(&user) {
        String::new()
    } else {
        visibility_where(&accessible_topic_ids(db, user.id).await)
    };
    let mut cond = if vis.is_empty() {
        " WHERE 1=1".to_string()
    } else {
        vis.clone()
    };
    if let Some(ft) = &p.file_type {
        cond.push_str(&format!(" AND b.file_type = '{}'", ft.replace('\'', "")));
    }
    if let Some(l) = &letter {
        cond.push_str(&format!(" AND b.title LIKE '{}%'", l.replace('\'', "")));
    }
    if let Some(a) = p.author_id {
        cond.push_str(&format!(" AND b.author_id = {a}"));
    }
    if let Some(c) = p.category_id {
        cond.push_str(&format!(
            " AND b.id IN (SELECT book_id FROM ebook_book_categories WHERE category_id = {c})"
        ));
    }

    let total = count_scalar(
        db,
        &format!("SELECT COUNT(*) FROM ebook_books b{cond}"),
        None,
    ).await;
    let books = fetch_books(
        db,
        &format!("{cond} LIMIT ? OFFSET ?"),
        &[&per_page.to_string(), &offset.to_string()],
    )
    .await;

    let selected_author = match p.author_id {
        Some(aid) => sqlx::query("SELECT * FROM ebook_authors WHERE id = ?")
            .bind(aid)
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
            .map(|r| {
                json!({"id": r.get::<i64,_>("id"), "name": r.try_get::<String,_>("name").unwrap_or_default()})
            }),
        None => None,
    };
    let selected_category = match p.category_id {
        Some(cid) => sqlx::query("SELECT * FROM ebook_categories WHERE id = ?")
            .bind(cid)
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
            .map(|r| {
                json!({"id": r.get::<i64,_>("id"), "name": r.try_get::<String,_>("name").unwrap_or_default()})
            }),
        None => None,
    };
    let reading_lists = reading_lists_for_dropdown(db, user.id).await;

    page(
        &state,
        "ebook/browse_books.html",
        json!({
            "user": crate::core::user_json(&user),
            "books": books,
            "current_letter": letter,
            "current_page": page_no,
            "total_pages": ((total) + per_page - 1) / per_page,
            "total": total,
            "selected_author": selected_author,
            "selected_category": selected_category,
            "author_id": p.author_id,
            "category_id": p.category_id,
            "file_type": p.file_type,
            "per_page": per_page,
            "reading_lists": reading_lists,
        }),
    )
}

async fn browse_categories(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(p): Query<PageParams>,
) -> Response {
    let db = &state.db;
    let (page_no, per_page, offset, _) = page_params(&p, 50, 50, 200);
    let total = count_scalar(db, "SELECT COUNT(*) FROM ebook_categories", None).await;
    let categories: Vec<Value> = sqlx::query(
        "SELECT * FROM ebook_categories ORDER BY name LIMIT ? OFFSET ?",
    )
    .bind(per_page)
    .bind(offset)
    .fetch_all(db)
    .await
    .map(|rows| {
        rows.iter()
            .map(|r| {
                json!({
                    "id": r.get::<i64,_>("id"),
                    "name": r.try_get::<String,_>("name").unwrap_or_default(),
                    "normalized_name": r.try_get::<String,_>("normalized_name").unwrap_or_default(),
                })
            })
            .collect()
    })
    .unwrap_or_default();

    let mut counts = serde_json::Map::new();
    for c in &categories {
        let id = c["id"].as_i64().unwrap_or(0);
        let count = count_scalar(
            db,
            "SELECT COUNT(*) FROM ebook_book_categories WHERE category_id = ?",
            Some(&id.to_string()),
        ).await;
        counts.insert(id.to_string(), json!(count));
    }

    page(
        &state,
        "ebook/browse_categories.html",
        json!({
            "user": crate::core::user_json(&user),
            "categories": categories,
            "category_book_counts": Value::Object(counts),
            "current_page": page_no,
            "total_pages": ((total) + per_page - 1) / per_page,
            "total": total,
            "per_page": per_page,
        }),
    )
}

// ------------------------------------------------------------------ topics

async fn new_topic_page(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let own_topics = own_topics_list(&state.db, user.id).await;
    page(
        &state,
        "ebook/topic_form.html",
        json!({
            "user": crate::core::user_json(&user),
            "topic": Value::Null,
            "mode": "new",
            "own_topics": own_topics,
        }),
    )
}

async fn own_topics_list(db: &syarc_plugin_api::Db, user_id: i64) -> Vec<Value> {
    sqlx::query("SELECT * FROM ebook_topics WHERE user_id = ? ORDER BY name")
        .bind(user_id)
        .fetch_all(db)
        .await
        .map(|rows| rows.iter().map(topic_row).collect())
        .unwrap_or_default()
}

async fn create_topic(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    RawForm(form): RawForm,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let form = form_map(&form);
    let Some(name) = fv(&form, "name") else {
        return redirect("/library/ebook/topic/new");
    };
    let norm = name.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
    if sqlx::query("SELECT 1 FROM ebook_topics WHERE user_id = ? AND normalized_name = ?")
        .bind(user.id)
        .bind(&norm)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .is_some()
    {
        return redirect("/library/ebook/topic/new");
    }

    let parent_id = fv(&form, "parent_id").filter(|p| p != "None");
    let topic_dir = match &parent_id {
        Some(pid) => match get_topic(db, pid).await {
            Some(t) => std::path::Path::new(t["directory_path"].as_str().unwrap_or(""))
                .join(&name)
                .to_string_lossy()
                .into_owned(),
            None => return redirect("/library/ebook/topic/new"),
        },
        None => topic_dir_for(&user.username, &name),
    };
    let _ = std::fs::create_dir_all(&topic_dir);

    let is_public = form.get("is_public").map(|v| v == "on").unwrap_or(false);
    let id = new_uuid();
    let _ = sqlx::query(
        "INSERT INTO ebook_topics (id, user_id, name, normalized_name, description, is_public, \
         directory_path, parent_id, book_count, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?)",
    )
    .bind(&id)
    .bind(user.id)
    .bind(&name)
    .bind(&norm)
    .bind(fv(&form, "description"))
    .bind(is_public as i64)
    .bind(&topic_dir)
    .bind(parent_id.as_deref())
    .bind(now_str())
    .bind(now_str())
    .execute(db)
    .await;

    if is_public {
        make_topic_public(db, &id).await;
    }
    redirect(&format!("/library/ebook/topic/{id}"))
}

async fn view_topic(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(topic_id): Path<String>,
    Query(p): Query<PageParams>,
) -> Response {
    let db = &state.db;
    let Some(topic) = get_topic(db, &topic_id).await else {
        return err404("Topic not found");
    };
    let full_access = is_super(&user);
    let is_owner = topic["user_id"].as_i64() == Some(user.id) && full_access;
    if !is_owner
        && !user_has_topic_access(db, &topic_id, user.id).await
        && !topic["is_public"].as_bool().unwrap_or(false)
    {
        return err403("Access denied");
    }

    let (page_no, per_page, offset, _) = page_params(&p, 24, 12, 120);
    let letter = p
        .letter
        .clone()
        .filter(|l| l.chars().count() == 1 && l.chars().all(|c| c.is_alphabetic()))
        .map(|l| l.to_uppercase());

    let is_public = topic["is_public"].as_bool().unwrap_or(false);
    let mut cond = format!(" WHERE b.topic_id = '{}'", topic_id.replace('\'', ""));
    if !is_owner {
        if is_public {
            cond.push_str(" AND b.is_banned = 0");
        } else {
            cond.push_str(" AND b.is_public = 1 AND b.is_banned = 0");
        }
    }
    if let Some(l) = &letter {
        cond.push_str(&format!(" AND b.title LIKE '{}%'", l.replace('\'', "")));
    }
    let total = count_scalar(db, &format!("SELECT COUNT(*) FROM ebook_books b{cond}"), None).await;
    let books = fetch_books(db, &format!("{cond} LIMIT ? OFFSET ?"), &[&per_page.to_string(), &offset.to_string()]).await;

    let subtopics: Vec<Value> = sqlx::query(
        "SELECT * FROM ebook_topics WHERE parent_id = ? ORDER BY name",
    )
    .bind(&topic_id)
    .fetch_all(db)
    .await
    .map(|rows| rows.iter().map(topic_row).collect())
    .unwrap_or_default();
    let mut subtopic_book_counts = serde_json::Map::new();
    for st in &subtopics {
        let sid = st["id"].as_str().unwrap_or("").to_string();
        let count = count_scalar(
            db,
            "SELECT COUNT(*) FROM ebook_books WHERE topic_id = ?",
            Some(&sid),
        ).await;
        subtopic_book_counts.insert(sid, json!(count));
    }

    let shared = crate::shared_users(db, "ebook_topic_shares", "topic_id", &topic_id).await;
    let reading_lists = reading_lists_for_dropdown(db, user.id).await;
    let parent_name = match topic["parent_id"].as_str() {
        Some(pid) => sqlx::query("SELECT name FROM ebook_topics WHERE id = ?")
            .bind(pid)
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
            .and_then(|r| r.try_get::<String,_>("name").ok()),
        None => None,
    };

    page(
        &state,
        "ebook/topic.html",
        json!({
            "user": crate::core::user_json(&user),
            "topic": topic,
            "books": books,
            "subtopics": subtopics,
            "subtopic_book_counts": Value::Object(subtopic_book_counts),
            "is_owner": is_owner,
            "shared_users": shared,
            "current_page": page_no,
            "total_pages": ((total) + per_page - 1) / per_page,
            "total": total,
            "per_page": per_page,
            "current_letter": letter,
            "full_access": full_access,
            "reading_lists": reading_lists,
            "parent_name": parent_name,
        }),
    )
}

async fn edit_topic_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(topic_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let Some(topic) = sqlx::query("SELECT * FROM ebook_topics WHERE id = ? AND user_id = ?")
        .bind(&topic_id)
        .bind(user.id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|r| topic_row(&r))
    else {
        return err404("Topic not found");
    };
    let shared = crate::shared_users(db, "ebook_topic_shares", "topic_id", &topic_id).await;
    let all_users = crate::all_users(db, user.id).await;
    let mut own = own_topics_list(db, user.id).await;
    own.retain(|t| t["id"].as_str() != Some(topic_id.as_str()));

    page(
        &state,
        "ebook/topic_form.html",
        json!({
            "user": crate::core::user_json(&user),
            "topic": topic,
            "mode": "edit",
            "shared_users": shared,
            "all_users": all_users,
            "own_topics": own,
        }),
    )
}

async fn update_topic(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(topic_id): Path<String>,
    RawForm(form): RawForm,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let form = form_map(&form);
    let Some(name) = fv(&form, "name") else {
        return redirect(&format!("/library/ebook/topic/{topic_id}/edit"));
    };
    let Some(topic) = get_topic(db, &topic_id).await.filter(|t| t["user_id"].as_i64() == Some(user.id)) else {
        return redirect("/library/ebook/");
    };
    let norm = name.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
    let was_public = topic["is_public"].as_bool().unwrap_or(false);
    let is_public = form.get("is_public").map(|v| v == "on").unwrap_or(false);

    let _ = sqlx::query(
        "UPDATE ebook_topics SET name = ?, normalized_name = ?, description = ?, is_public = ?, \
         updated_at = ? WHERE id = ?",
    )
    .bind(&name)
    .bind(&norm)
    .bind(fv(&form, "description"))
    .bind(is_public as i64)
    .bind(now_str())
    .bind(&topic_id)
    .execute(db)
    .await;

    if is_public && !was_public {
        make_topic_public(db, &topic_id).await;
    } else if !is_public && was_public {
        make_topic_private(db, &topic_id).await;
    }
    redirect(&format!("/library/ebook/topic/{topic_id}"))
}

async fn delete_topic(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(topic_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let Some(topic) = get_topic(db, &topic_id).await.filter(|t| t["user_id"].as_i64() == Some(user.id)) else {
        return redirect("/library/ebook/");
    };
    let book_count = count_scalar(
        db,
        "SELECT COUNT(*) FROM ebook_books WHERE topic_id = ?",
        Some(&topic_id),
    ).await;
    let child_count = count_scalar(
        db,
        "SELECT COUNT(*) FROM ebook_topics WHERE parent_id = ?",
        Some(&topic_id),
    ).await;
    if book_count > 0 || child_count > 0 {
        return redirect(&format!("/library/ebook/topic/{topic_id}"));
    }
    if topic["is_public"].as_bool().unwrap_or(false) {
        make_topic_private(db, &topic_id).await;
    }
    let topic_dir = topic["directory_path"].as_str().unwrap_or("").to_string();
    let _ = sqlx::query("DELETE FROM ebook_topic_shares WHERE topic_id = ?")
        .bind(&topic_id)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM ebook_topics WHERE id = ?")
        .bind(&topic_id)
        .execute(db)
        .await;
    if !topic_dir.is_empty() && std::path::Path::new(&topic_dir).is_dir() {
        let _ = std::fs::remove_dir_all(&topic_dir);
    }
    redirect("/library/ebook/")
}

async fn share_topic(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(topic_id): Path<String>,
    RawForm(form): RawForm,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    if !user_is_topic_owner(db, &topic_id, user.id).await {
        return redirect("/library/ebook/");
    }
    let form = form_map(&form);
    let Some(share_with) = fv(&form, "share_with_user_id") else {
        return redirect(&format!("/library/ebook/topic/{topic_id}"));
    };
    let Ok(target) = share_with.parse::<i64>() else {
        return redirect(&format!("/library/ebook/topic/{topic_id}"));
    };
    if target == user.id {
        return redirect(&format!("/library/ebook/topic/{topic_id}"));
    }
    let _ = sqlx::query(
        "INSERT OR IGNORE INTO ebook_topic_shares (topic_id, shared_with_user_id, created_at) \
         VALUES (?, ?, ?)",
    )
    .bind(&topic_id)
    .bind(target)
    .bind(now_str())
    .execute(db)
    .await;
    redirect(&format!("/library/ebook/topic/{topic_id}/edit"))
}

async fn revoke_topic_share(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((topic_id, share_id)): Path<(String, i64)>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    if !user_is_topic_owner(db, &topic_id, user.id).await {
        return redirect("/library/ebook/");
    }
    let _ = sqlx::query("DELETE FROM ebook_topic_shares WHERE id = ? AND topic_id = ?")
        .bind(share_id)
        .bind(&topic_id)
        .execute(db)
        .await;
    redirect(&format!("/library/ebook/topic/{topic_id}/edit"))
}

async fn import_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(topic_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let Some(topic) = get_topic(&state.db, &topic_id)
        .await
        .filter(|t| t["user_id"].as_i64() == Some(user.id))
    else {
        return err404("Topic not found");
    };
    page(
        &state,
        "ebook/import.html",
        json!({"user": crate::core::user_json(&user), "topic": topic}),
    )
}

// ------------------------------------------------------------------ add / import / rescan

/// Walk a directory (or take a single file) collecting epub/pdf paths.
pub(crate) fn collect_ebook_items(path: &std::path::Path) -> Vec<String> {
    let mut items = Vec::new();
    if path.is_file() {
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if EBOOK_EXTENSIONS.contains(&ext.as_str()) {
            items.push(path.to_string_lossy().into_owned());
        }
    } else if path.is_dir() {
        collect_dir_ebooks(path, &mut items);
    }
    items
}

fn collect_dir_ebooks(dir: &std::path::Path, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut names: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    names.sort();
    for path in names {
        // Skip hidden files/dirs (.epub, .DS_Store, ...)
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with('.'))
            .unwrap_or(true)
        {
            continue;
        }
        if path.is_dir() {
            collect_dir_ebooks(&path, out);
        } else {
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if EBOOK_EXTENSIONS.contains(&ext.as_str()) {
                out.push(path.to_string_lossy().into_owned());
            }
        }
    }
}

async fn add_book_quick(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    RawForm(form): RawForm,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let form = form_map(&form);
    let Some(file_path) = fv(&form, "file_path") else {
        return err400("File or directory path is required");
    };
    let path = std::path::PathBuf::from(&file_path);
    if !path.exists() {
        return err400(&format!("Path not found: {file_path}"));
    }
    let items = collect_ebook_items(&path);
    if items.is_empty() {
        return err400("No .epub or .pdf files found");
    }

    let mut imported = 0i64;
    let mut skipped = 0i64;
    let mut results: Vec<Value> = Vec::new();
    let mut topic_cache: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for item in items {
        if count_scalar(
            db,
            "SELECT COUNT(*) FROM ebook_books WHERE file_path = ?",
            Some(&item),
        ).await > 0
        {
            skipped += 1;
            continue;
        }
        let file_type = if item.to_lowercase().ends_with(".epub") { "epub" } else { "pdf" };
        let mut meta = extract_metadata(item.clone(), file_type.to_string()).await;
        if meta.title.is_empty() {
            meta = fallback_metadata_from_filename(&item);
        }
        let author_name = sanitize_str(&meta.author);
        let author_id = get_or_create_author(db, &author_name).await;

        let book_dir = std::path::Path::new(&item)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let topic_id = match topic_cache.get(&book_dir) {
            Some(t) => Some(t.clone()),
            None => {
                let existing = sqlx::query(
                    "SELECT id, is_public FROM ebook_topics WHERE user_id = ? AND directory_path = ?",
                )
                .bind(user.id)
                .bind(&book_dir)
                .fetch_optional(db)
                .await
                .ok()
                .flatten();
                let tid = match existing {
                    Some(r) => r.get::<String,_>("id"),
                    None => {
                        let topic_name = std::path::Path::new(&book_dir)
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let tid = new_uuid();
                        let _ = sqlx::query(
                            "INSERT INTO ebook_topics (id, user_id, name, normalized_name, \
                             directory_path, is_public, book_count, created_at, updated_at) \
                             VALUES (?, ?, ?, ?, ?, 0, 0, ?, ?)",
                        )
                        .bind(&tid)
                        .bind(user.id)
                        .bind(&topic_name)
                        .bind(topic_name.to_lowercase().trim())
                        .bind(&book_dir)
                        .bind(now_str())
                        .bind(now_str())
                        .execute(db)
                        .await;
                        tid
                    }
                };
                topic_cache.insert(book_dir.clone(), tid.clone());
                Some(tid)
            }
        };
        let topic_public = sqlx::query("SELECT is_public FROM ebook_topics WHERE id = ?")
            .bind(topic_id.as_deref().unwrap_or(""))
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
            .map(|r| r.get::<i64,_>("is_public") == 1)
            .unwrap_or(false);

        let book_id = insert_book(
            db,
            &meta,
            author_id,
            file_type,
            &item,
            topic_id.as_deref(),
            Some(user.id),
            topic_public,
        )
        .await;
        if let Some(cover) = extract_cover(item.clone(), book_id, file_type.to_string()).await {
            let _ = sqlx::query("UPDATE ebook_books SET cover_filename = ? WHERE id = ?")
                .bind(&cover)
                .bind(book_id)
                .execute(db)
                .await;
        }
        bind_categories(db, book_id, &meta.subjects).await;
        imported += 1;
        results.push(json!({"title": meta.title, "author": author_name}));
    }

    for tid in topic_cache.values() {
        refresh_topic_count(db, tid).await;
    }

    let mut msg = format!("Added {imported} book(s)");
    if skipped > 0 {
        msg.push_str(&format!(", {skipped} already in database"));
    }
    axum::Json(json!({"success": true, "message": msg, "details": results})).into_response()
}

async fn import_from_path(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(topic_id): Path<String>,
    RawForm(form): RawForm,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let Some(topic) = get_topic(db, &topic_id)
        .await
        .filter(|t| t["user_id"].as_i64() == Some(user.id))
    else {
        return err404("Topic not found");
    };
    let form = form_map(&form);
    let Some(import_path) = fv(&form, "import_path") else {
        return redirect(&format!("/library/ebook/topic/{topic_id}/import"));
    };
    let make_public = form.get("make_public").map(|v| v == "on").unwrap_or(false);
    let src = std::path::PathBuf::from(&import_path);
    if !src.exists() {
        return redirect(&format!("/library/ebook/topic/{topic_id}/import"));
    }

    let topic_dir = topic["directory_path"].as_str().unwrap_or("").to_string();
    let is_inside = src.starts_with(&topic_dir);

    let mut items: Vec<String> = Vec::new();
    if src.is_file() {
        items = collect_ebook_items(&src);
    } else {
        // top-level files plus one level of subdirectories
        let Ok(entries) = std::fs::read_dir(&src) else {
            return redirect(&format!("/library/ebook/topic/{topic_id}/import"));
        };
        let mut paths: Vec<std::path::PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
        paths.sort();
        for p in paths {
            if p.is_file() {
                let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
                if EBOOK_EXTENSIONS.contains(&ext.as_str()) {
                    items.push(p.to_string_lossy().into_owned());
                }
            } else if p.is_dir() {
                if let Ok(subs) = std::fs::read_dir(&p) {
                    let mut files: Vec<std::path::PathBuf> =
                        subs.filter_map(|e| e.ok().map(|e| e.path())).collect();
                    files.sort();
                    for f in files {
                        let ext = f.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
                        if EBOOK_EXTENSIONS.contains(&ext.as_str()) {
                            items.push(f.to_string_lossy().into_owned());
                        }
                    }
                }
            }
        }
    }

    let mut imported_count = 0;
    let mut skipped = 0;
    let topic_public = topic["is_public"].as_bool().unwrap_or(false);

    for s in items {
        if is_inside
            && count_scalar(
                db,
                "SELECT COUNT(*) FROM ebook_books WHERE file_path = ? AND topic_id = ?",
                Some(&s),
            ).await > 0
        {
            skipped += 1;
            continue;
        }
        let dest = if is_inside {
            s.clone()
        } else {
            let base = std::path::Path::new(&s)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let mut dest = std::path::Path::new(&topic_dir).join(&base);
            if dest.exists() {
                let stem = std::path::Path::new(&base)
                    .file_stem()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let ext = std::path::Path::new(&base)
                    .extension()
                    .map(|e| format!(".{}", e.to_string_lossy()))
                    .unwrap_or_default();
                dest = std::path::Path::new(&topic_dir)
                    .join(format!("{}_{}{}", stem, &new_uuid()[..8], ext));
            }
            if std::fs::copy(&s, &dest).is_err() {
                continue;
            }
            dest.to_string_lossy().into_owned()
        };

        let file_type = if dest.to_lowercase().ends_with(".epub") { "epub" } else { "pdf" };
        let mut meta = extract_metadata(dest.clone(), file_type.to_string()).await;
        if meta.title.is_empty() {
            meta = fallback_metadata_from_filename(&dest);
        }
        let author_id = get_or_create_author(db, &meta.author).await;
        let is_public = make_public || topic_public;
        let book_id = insert_book(
            db,
            &meta,
            author_id,
            file_type,
            &dest,
            Some(&topic_id),
            Some(user.id),
            is_public,
        )
        .await;
        if let Some(cover) = extract_cover(dest.clone(), book_id, file_type.to_string()).await {
            let _ = sqlx::query("UPDATE ebook_books SET cover_filename = ? WHERE id = ?")
                .bind(&cover)
                .bind(book_id)
                .execute(db)
                .await;
        }
        imported_count += 1;
    }

    if imported_count > 0 {
        refresh_topic_count(db, &topic_id).await;
        if topic_public {
            make_topic_public(db, &topic_id).await;
        }
    }
    let mut msg = format!("Imported {imported_count} books.");
    if skipped > 0 {
        msg.push_str(&format!(" Skipped {skipped} already in database."));
    }
    redirect(&format!("/library/ebook/topic/{topic_id}"))
}

/// Shared topic rescan used by the owner and admin routes.
async fn run_topic_rescan(
    db: &syarc_plugin_api::Db,
    topic_id: &str,
    scan_user_id: i64,
) -> Response {
    let Some(topic) = get_topic(db, topic_id).await else {
        return err404("Topic not found");
    };
    let topic_dir = topic["directory_path"].as_str().unwrap_or("").to_string();
    if !std::path::Path::new(&topic_dir).is_dir() {
        return axum::Json(json!({"message": "Topic directory does not exist on disk."}))
            .into_response();
    }

    // Match against ALL known book paths, not just this topic's: the same
    // directory can be referenced by nested topics, and re-adding would
    // create duplicate rows for one file.
    let existing: Vec<(i64, String, Option<String>)> = sqlx::query(
        "SELECT id, file_path, cover_filename FROM ebook_books",
    )
    .fetch_all(db)
    .await
    .map(|rows| {
        rows.iter()
            .map(|r| {
                (
                    r.get::<i64,_>("id"),
                    r.try_get::<String,_>("file_path").unwrap_or_default(),
                    r.try_get::<Option<String>,_>("cover_filename").ok().flatten(),
                )
            })
            .collect()
    })
    .unwrap_or_default();
    let known: std::collections::HashMap<String, i64> =
        existing.iter().map(|(id, p, _)| (p.clone(), *id)).collect();

    let disk_items = collect_ebook_items(std::path::Path::new(&topic_dir));
    let topic_public = topic["is_public"].as_bool().unwrap_or(false);

    let mut added = 0i64;
    let mut cover_count = 0i64;
    for item in disk_items {
        if known.contains_key(&item) {
            continue;
        }
        let file_type = if item.to_lowercase().ends_with(".epub") { "epub" } else { "pdf" };
        let mut meta = extract_metadata(item.clone(), file_type.to_string()).await;
        if meta.title.is_empty() {
            meta = fallback_metadata_from_filename(&item);
        }
        let author_id = get_or_create_author(db, &meta.author).await;
        let book_id = insert_book(
            db,
            &meta,
            author_id,
            file_type,
            &item,
            Some(topic_id),
            Some(scan_user_id),
            topic_public,
        )
        .await;
        if let Some(cover) = extract_cover(item.clone(), book_id, file_type.to_string()).await {
            let _ = sqlx::query("UPDATE ebook_books SET cover_filename = ? WHERE id = ?")
                .bind(&cover)
                .bind(book_id)
                .execute(db)
                .await;
            cover_count += 1;
        }
        added += 1;
    }

    let mut removed = 0i64;
    for (_, path, cover) in &existing {
        if !std::path::Path::new(path).exists() {
            remove_book_cover(path, cover.as_deref());
            let _ = sqlx::query("DELETE FROM ebook_reading_list_items WHERE item_type = 'book' AND item_id = ?")
                .bind(known.get(path).copied().unwrap_or(0))
                .execute(db)
                .await;
            let _ = sqlx::query("DELETE FROM ebook_books WHERE file_path = ?")
                .bind(path)
                .execute(db)
                .await;
            removed += 1;
        }
    }

    refresh_topic_count(db, topic_id).await;
    if topic_public && added > 0 {
        make_topic_public(db, topic_id).await;
    }

    let book_count = count_scalar(
        db,
        "SELECT COUNT(*) FROM ebook_books WHERE topic_id = ?",
        Some(topic_id),
    ).await;
    let mut parts: Vec<String> = Vec::new();
    if added > 0 {
        parts.push(format!("Added {added} new"));
    }
    if cover_count > 0 {
        parts.push(format!("generated {cover_count} covers"));
    }
    if removed > 0 {
        parts.push(format!("removed {removed} missing"));
    }
    if parts.is_empty() {
        return axum::Json(json!({"message": "Topic is already in sync. No changes needed."}))
            .into_response();
    }
    axum::Json(json!({
        "message": format!("Rescan complete: {}. Topic now has {book_count} books.", parts.join(", "))
    }))
    .into_response()
}

async fn rescan_topic(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(topic_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    if !user_is_topic_owner(db, &topic_id, user.id).await {
        return err404("Topic not found");
    }
    run_topic_rescan(db, &topic_id, user.id).await
}

// ------------------------------------------------------------------ books

async fn view_book(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(book_id): Path<i64>,
    Query(p): Query<FromLetterParams>,
) -> Response {
    let db = &state.db;
    let Some(book) = get_book(db, book_id).await else {
        return err404("Book not found");
    };
    let full_access = is_super(&user);

    let topic = match book["topic_id"].as_str() {
        Some(tid) => get_topic(db, tid).await,
        None => None,
    };
    let mut is_owner = false;
    if let Some(t) = &topic {
        is_owner = t["user_id"].as_i64() == Some(user.id) && full_access;
        if !is_owner
            && !is_super(&user)
            && !user_has_topic_access(db, t["id"].as_str().unwrap_or(""), user.id).await
            && !(book["is_public"].as_bool().unwrap_or(false)
                && !book["is_banned"].as_bool().unwrap_or(false))
        {
            return err403("Access denied");
        }
    } else if !book["is_public"].as_bool().unwrap_or(false)
        && book["user_id"].as_i64() != Some(user.id)
        && !is_super(&user)
    {
        return err403("Access denied");
    } else if book["user_id"].as_i64() == Some(user.id) && full_access {
        is_owner = true;
    }

    let own_topics = if is_owner || is_super(&user) {
        own_topics_list(db, user.id).await
    } else {
        vec![]
    };
    let reading_lists = reading_lists_for_dropdown(db, user.id).await;
    let parent_name = match topic.as_ref().and_then(|t| t["parent_id"].as_str()) {
        Some(pid) => sqlx::query("SELECT name FROM ebook_topics WHERE id = ?")
            .bind(pid)
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
            .and_then(|r| r.try_get::<String,_>("name").ok()),
        None => None,
    };

    page(
        &state,
        "ebook/book.html",
        json!({
            "user": crate::core::user_json(&user),
            "book": book,
            "topic": topic,
            "is_owner": is_owner,
            "full_access": full_access,
            "own_topics": own_topics,
            "reading_lists": reading_lists,
            "parent_name": parent_name,
            "from_letter": p.from_letter,
        }),
    )
}

#[derive(Deserialize)]
struct FromLetterParams {
    #[serde(default)]
    from_letter: Option<String>,
}

async fn edit_book_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(book_id): Path<i64>,
    Query(p): Query<FromLetterParams>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let Some(book) = get_book(db, book_id).await else {
        return err404("Book not found");
    };
    if let Some(tid) = book["topic_id"].as_str() {
        if !user_is_topic_owner(db, tid, user.id).await && book["user_id"].as_i64() != Some(user.id) {
            return err403("Only the topic owner or book owner can edit books");
        }
    }
    let topic = match book["topic_id"].as_str() {
        Some(tid) => get_topic(db, tid).await,
        None => None,
    };
    let parent_name = match topic.as_ref().and_then(|t| t["parent_id"].as_str()) {
        Some(pid) => sqlx::query("SELECT name FROM ebook_topics WHERE id = ?")
            .bind(pid)
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
            .and_then(|r| r.try_get::<String,_>("name").ok()),
        None => None,
    };
    let all_topics = own_topics_list(db, user.id).await;
    page(
        &state,
        "ebook/edit_book.html",
        json!({
            "user": crate::core::user_json(&user),
            "book": book,
            "topic": topic,
            "parent_name": parent_name,
            "all_topics": all_topics,
            "from_letter": p.from_letter,
        }),
    )
}

async fn update_book(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    mut multipart: Multipart,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;

    let mut book_id: Option<i64> = None;
    let mut new_title: Option<String> = None;
    let mut new_author: Option<String> = None;
    let mut new_topic_id: Option<String> = None;
    let mut cover_data: Option<String> = None;
    let mut cover_file: Option<(String, Vec<u8>)> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "cover" => {
                let filename = field.file_name().unwrap_or("").to_string();
                if !filename.is_empty() {
                    if let Ok(bytes) = field.bytes().await {
                        cover_file = Some((filename, bytes.to_vec()));
                    }
                }
            }
            _ => {
                if let Ok(text) = field.text().await {
                    match name.as_str() {
                        "book_id" => book_id = text.trim().parse().ok(),
                        "title" => new_title = Some(text),
                        "author" => new_author = Some(text),
                        "topic_id" => new_topic_id = Some(text.trim().to_string()).filter(|t| !t.is_empty()),
                        "cover_data" => cover_data = Some(text),
                        _ => {}
                    }
                }
            }
        }
    }

    let Some(book_id) = book_id else {
        return err400("Missing required fields");
    };
    let Some(title) = new_title.clone().filter(|t| !t.trim().is_empty()) else {
        return err400("Missing required fields");
    };
    let Some(book) = get_book(db, book_id).await else {
        return err404("Book not found");
    };
    if let Some(tid) = book["topic_id"].as_str() {
        if !user_is_topic_owner(db, tid, user.id).await && book["user_id"].as_i64() != Some(user.id) {
            return err403("Only the topic owner or book owner can edit books");
        }
    }

    let norm = title.trim().to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
    let _ = sqlx::query(
        "UPDATE ebook_books SET title = ?, normalized_title = ?, updated_at = ? WHERE id = ?",
    )
    .bind(title.trim())
    .bind(&norm)
    .bind(now_str())
    .bind(book_id)
    .execute(db)
    .await;

    if let Some(author) = new_author.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
        let new_author_id = get_or_create_author(db, author).await;
        let old_author_id = book["author_id"].as_i64().unwrap_or(0);
        let _ = sqlx::query("UPDATE ebook_books SET author_id = ? WHERE id = ?")
            .bind(new_author_id)
            .bind(book_id)
            .execute(db)
            .await;
        if old_author_id != new_author_id {
            let remaining = count_scalar(
                db,
                "SELECT COUNT(*) FROM ebook_books WHERE author_id = ?",
                Some(&old_author_id.to_string()),
            ).await;
            if remaining == 0 {
                let _ = sqlx::query("DELETE FROM ebook_authors WHERE id = ?")
                    .bind(old_author_id)
                    .execute(db)
                    .await;
            }
        }
    }

    if let Some(ntid) = new_topic_id.clone().filter(|t| !t.is_empty() && book["topic_id"].as_str() != Some(t.as_str())) {
        let Some(target) = get_topic(db, &ntid)
            .await
            .filter(|t| t["user_id"].as_i64() == Some(user.id))
        else {
            return err404("Target topic not found or not owned by you");
        };
        let old_file = book["file_path"].as_str().unwrap_or("").to_string();
        let filename = std::path::Path::new(&old_file)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let tdir = target["directory_path"].as_str().unwrap_or("");
        let mut dest = std::path::Path::new(tdir).join(&filename);
        if dest.exists() {
            let stem = std::path::Path::new(&filename)
                .file_stem()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let ext = std::path::Path::new(&filename)
                .extension()
                .map(|e| format!(".{}", e.to_string_lossy()))
                .unwrap_or_default();
            dest = std::path::Path::new(tdir).join(format!("{}_{}{}", stem, &new_uuid()[..8], ext));
        }
        let old_topic_id = book["topic_id"].as_str().map(|s| s.to_string());
        if std::path::Path::new(&old_file).exists() && old_file != dest.to_string_lossy()
            && std::fs::rename(&old_file, &dest).is_ok() {
                let _ = sqlx::query("UPDATE ebook_books SET file_path = ? WHERE id = ?")
                    .bind(dest.to_string_lossy().into_owned())
                    .bind(book_id)
                    .execute(db)
                    .await;
            }
        let target_public = target["is_public"].as_bool().unwrap_or(false);
        let new_public = target_public;
        let _ = sqlx::query(
            "UPDATE ebook_books SET topic_id = ?, user_id = ?, is_public = ? WHERE id = ?",
        )
        .bind(&ntid)
        .bind(user.id)
        .bind(new_public as i64)
        .bind(book_id)
        .execute(db)
        .await;
        if let Some(old) = old_topic_id {
            refresh_topic_count(db, &old).await;
        }
        refresh_topic_count(db, &ntid).await;
    }

    let book_file = book["file_path"].as_str().unwrap_or("").to_string();
    let out_path = sibling_cover_path(&book_file);
    let mut new_cover: Option<String> = None;
    if let Some(data) = cover_data.filter(|d| d.starts_with("data:image")) {
        let Some((header, b64)) = data.split_once(',') else {
            return err400("Invalid image format from clipboard");
        };
        let mut ext = header
            .split('/')
            .nth(1)
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .to_string();
        if ext == "jpeg" {
            ext = "jpg".into();
        }
        if !matches!(ext.as_str(), "jpg" | "png" | "gif" | "webp") {
            return err400("Invalid image format from clipboard");
        }
        if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(b64) {
            let _ = std::fs::write(&out_path, bytes);
            new_cover = Some(COVER_SIBLING.to_string());
        }
    } else if let Some((filename, bytes)) = cover_file {
        let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();
        if !matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "gif" | "webp") {
            return err400("Invalid image format");
        }
        let _ = std::fs::write(&out_path, bytes);
        new_cover = Some(COVER_SIBLING.to_string());
    }
    if let Some(filename) = new_cover {
        remove_book_cover(
            &book_file,
            book["cover_filename"].as_str().filter(|c| *c != COVER_SIBLING),
        );
        let _ = sqlx::query("UPDATE ebook_books SET cover_filename = ? WHERE id = ?")
            .bind(&filename)
            .bind(book_id)
            .execute(db)
            .await;
    }

    axum::Json(json!({"message": "Book metadata updated successfully"})).into_response()
}

async fn move_book(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(book_id): Path<i64>,
    RawForm(form): RawForm,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let form = form_map(&form);
    let Some(target_id) = fv(&form, "target_topic_id") else {
        return err400("Target topic is required");
    };
    let Some(book) = get_book(db, book_id).await else {
        return err404("Book not found");
    };
    if let Some(tid) = book["topic_id"].as_str() {
        if !user_is_topic_owner(db, tid, user.id).await && book["user_id"].as_i64() != Some(user.id) {
            return err403("Only the topic owner or book owner can move books");
        }
    }
    let Some(target) = get_topic(db, &target_id)
        .await
        .filter(|t| t["user_id"].as_i64() == Some(user.id))
    else {
        return err404("Target topic not found or not owned by you");
    };
    if book["topic_id"].as_str() == Some(target_id.as_str()) {
        return err400("Book is already in this topic");
    }

    let old_file = book["file_path"].as_str().unwrap_or("").to_string();
    let filename = std::path::Path::new(&old_file)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tdir = target["directory_path"].as_str().unwrap_or("");
    let mut dest = std::path::Path::new(tdir).join(&filename);
    if dest.exists() {
        let stem = std::path::Path::new(&filename)
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let ext = std::path::Path::new(&filename)
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        dest = std::path::Path::new(tdir).join(format!("{}_{}{}", stem, &new_uuid()[..8], ext));
    }
    let final_path = if std::path::Path::new(&old_file).exists() {
        if std::fs::rename(&old_file, &dest).is_ok() {
            dest.to_string_lossy().into_owned()
        } else {
            old_file.clone()
        }
    } else {
        old_file.clone()
    };

    let old_topic_id = book["topic_id"].as_str().map(|s| s.to_string());
    let target_public = target["is_public"].as_bool().unwrap_or(false);
    let _ = sqlx::query(
        "UPDATE ebook_books SET topic_id = ?, user_id = ?, file_path = ?, is_public = ?, updated_at = ? WHERE id = ?",
    )
    .bind(&target_id)
    .bind(user.id)
    .bind(&final_path)
    .bind(target_public as i64)
    .bind(now_str())
    .bind(book_id)
    .execute(db)
    .await;
    if let Some(old) = old_topic_id {
        refresh_topic_count(db, &old).await;
    }
    refresh_topic_count(db, &target_id).await;

    axum::Json(json!({
        "message": format!("Moved '{}' to '{}'", book["title"].as_str().unwrap_or(""), target["name"].as_str().unwrap_or("")),
        "redirect": format!("/library/ebook/book/{book_id}"),
    }))
    .into_response()
}

async fn delete_book(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(book_id): Path<i64>,
    RawForm(form): RawForm,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let Some(book) = get_book(db, book_id).await else {
        return err404("Book not found");
    };
    let is_owner = match book["topic_id"].as_str() {
        Some(tid) => user_is_topic_owner(db, tid, user.id).await,
        None => book["user_id"].as_i64() == Some(user.id),
    };
    if !is_owner && !is_super(&user) {
        return err403("Access denied");
    }
    let form = form_map(&form);
    let delete_from_disk = form
        .get("delete_from_disk")
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let title = book["title"].as_str().unwrap_or("").to_string();
    let cover = book["cover_filename"].as_str().map(|s| s.to_string());
    let file_path = book["file_path"].as_str().unwrap_or("").to_string();
    let topic_id = book["topic_id"].as_str().map(|s| s.to_string());

    let _ = sqlx::query("DELETE FROM ebook_reading_list_items WHERE item_type = 'book' AND item_id = ?")
        .bind(book_id)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM ebook_book_categories WHERE book_id = ?")
        .bind(book_id)
        .execute(db)
        .await;
    let _ = sqlx::query("DELETE FROM ebook_books WHERE id = ?")
        .bind(book_id)
        .execute(db)
        .await;

    remove_book_cover(&file_path, cover.as_deref());
    let mut disk_msg = String::new();
    if delete_from_disk && !file_path.is_empty() && std::path::Path::new(&file_path).exists() {
        match std::fs::remove_file(&file_path) {
            Ok(_) => disk_msg = " and file removed from disk".into(),
            Err(e) => disk_msg = format!(" but could not delete from disk: {e}"),
        }
    }
    if let Some(tid) = topic_id {
        refresh_topic_count(db, &tid).await;
    }
    axum::Json(json!({"message": format!("Book '{title}' deleted from database{disk_msg}")}))
        .into_response()
}

async fn user_make_book_public(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(book_id): Path<i64>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let Some(book) = get_book(db, book_id).await else {
        return err404("Book not found");
    };
    if let Some(tid) = book["topic_id"].as_str() {
        if !user_is_topic_owner(db, tid, user.id).await {
            return err403("Only the topic owner can do this");
        }
    }
    if book["is_banned"].as_bool().unwrap_or(false) {
        return err400("This book has been banned from public view");
    }
    let _ = sqlx::query("UPDATE ebook_books SET is_public = 1, updated_at = ? WHERE id = ?")
        .bind(now_str())
        .bind(book_id)
        .execute(db)
        .await;
    if let Some(tid) = book["topic_id"].as_str() {
        let topic = get_topic(db, tid).await;
        if let Some(t) = topic {
            if !t["is_public"].as_bool().unwrap_or(false) {
                make_topic_public(db, tid).await;
            }
        }
    }
    redirect(&format!("/library/ebook/book/{book_id}"))
}

async fn user_remove_book_public(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(book_id): Path<i64>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let Some(book) = get_book(db, book_id).await else {
        return err404("Book not found");
    };
    if let Some(tid) = book["topic_id"].as_str() {
        if !user_is_topic_owner(db, tid, user.id).await {
            return err403("Only the topic owner can do this");
        }
    }
    let _ = sqlx::query("UPDATE ebook_books SET is_public = 0, updated_at = ? WHERE id = ?")
        .bind(now_str())
        .bind(book_id)
        .execute(db)
        .await;
    redirect(&format!("/library/ebook/book/{book_id}"))
}

async fn toggle_topic_visibility(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(topic_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let Some(topic) = get_topic(db, &topic_id).await else {
        return err404("Topic not found");
    };
    if topic["user_id"].as_i64() != Some(user.id) {
        return err403("Only the topic owner can do this");
    }
    let is_public = if topic["is_public"].as_bool().unwrap_or(false) {
        make_topic_private(db, &topic_id).await;
        false
    } else {
        make_topic_public(db, &topic_id).await;
        true
    };
    axum::Json(json!({"is_public": is_public})).into_response()
}

async fn toggle_book_visibility(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(book_id): Path<i64>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let Some(book) = get_book(db, book_id).await else {
        return err404("Book not found");
    };
    if let Some(tid) = book["topic_id"].as_str() {
        if !user_is_topic_owner(db, tid, user.id).await {
            return err403("Only the topic owner can do this");
        }
    } else if book["user_id"].as_i64() != Some(user.id) {
        return err403("Access denied");
    }
    let new_public = if book["is_public"].as_bool().unwrap_or(false) {
        false
    } else {
        if book["is_banned"].as_bool().unwrap_or(false) {
            return err400("This book has been banned from public view");
        }
        true
    };
    let _ = sqlx::query("UPDATE ebook_books SET is_public = ?, updated_at = ? WHERE id = ?")
        .bind(new_public as i64)
        .bind(now_str())
        .bind(book_id)
        .execute(db)
        .await;
    axum::Json(json!({"is_public": new_public})).into_response()
}

// ------------------------------------------------------------------ download / read / convert

#[derive(Deserialize)]
struct DownloadParams {
    #[serde(default)]
    inline: Option<String>,
}

async fn download_book(
    State(state): State<SharedState>,
    RequireUser(_user): RequireUser,
    Path(book_id): Path<i64>,
    Query(p): Query<DownloadParams>,
) -> Response {
    let db = &state.db;
    let Some(book) = get_book(db, book_id).await else {
        return err404("Book not found");
    };
    let file_path = book["file_path"].as_str().unwrap_or("").to_string();
    if !std::path::Path::new(&file_path).is_file() {
        return err404("File not found on disk");
    }
    let media_type = if book["file_type"].as_str() == Some("epub") {
        "application/epub+zip"
    } else {
        "application/pdf"
    };
    let inline = p
        .inline
        .as_deref()
        .map(|v| v == "true" || v == "1" || v == "True")
        .unwrap_or(false);
    let filename = std::path::Path::new(&file_path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match tokio::fs::read(&file_path).await {
        Ok(bytes) => {
            let disposition = if inline && book["file_type"].as_str() != Some("epub") {
                format!("inline; filename=\"{filename}\"")
            } else {
                format!("attachment; filename=\"{filename}\"")
            };
            (
                [
                    (axum::http::header::CONTENT_TYPE, media_type.to_string()),
                    (
                        axum::http::header::CONTENT_DISPOSITION,
                        disposition,
                    ),
                ],
                bytes,
            )
                .into_response()
        }
        Err(_) => err404("File not found on disk"),
    }
}

async fn convert_pdf_to_epub(
    State(_state): State<SharedState>,
    RequireUser(_user): RequireUser,
    Path(_book_id): Path<i64>,
) -> Response {
    err_json(
        axum::http::StatusCode::NOT_IMPLEMENTED,
        "PDF to EPUB conversion is not supported in this build",
    )
}

#[derive(Deserialize)]
struct ReadParams {
    #[serde(default)]
    chapter: i64,
}

async fn read_book(
    State(state): State<SharedState>,
    RequireUser(_user): RequireUser,
    Path(book_id): Path<i64>,
    Query(p): Query<ReadParams>,
) -> Response {
    let db = &state.db;
    let Some(book) = get_book(db, book_id).await else {
        return err404("Book not found");
    };
    let file_path = book["file_path"].as_str().unwrap_or("").to_string();
    if !std::path::Path::new(&file_path).is_file() {
        return err404("File not found on disk");
    }
    if book["file_type"].as_str() == Some("pdf") {
        return redirect(&format!("/library/ebook/download/{book_id}?inline=true"));
    }
    let toc = extract_epub_toc(&file_path);
    let chapter = get_epub_chapter(&file_path, p.chapter.max(0));
    if chapter.is_none() {
        return err404("Chapter not found");
    }
    page(
        &state,
        "ebook/reader.html",
        json!({
            "user": Value::Null,
            "book": book,
            "chapter": chapter,
            "toc": toc,
            "current_chapter": p.chapter,
        }),
    )
}

async fn get_epub_image(
    State(state): State<SharedState>,
    RequireUser(_user): RequireUser,
    Path((book_id, img_path)): Path<(i64, String)>,
) -> Response {
    let db = &state.db;
    let Some(book) = get_book(db, book_id).await else {
        return err404("Book not found");
    };
    let file_path = book["file_path"].as_str().unwrap_or("").to_string();
    if !std::path::Path::new(&file_path).is_file() {
        return err404("EPUB file not found on disk");
    }
    let target = img_path.clone();
    let bytes = tokio::task::spawn_blocking(move || {
        let mut epub = open_epub(&file_path)?;
        let entry = find_zip_entry(&mut epub.zf, &target)?;
        read_zip_entry(&mut epub.zf, &entry)
    })
    .await
    .ok()
    .flatten();
    match bytes {
        Some(data) => {
            let ctype = if img_path.to_lowercase().ends_with(".png") {
                "image/png"
            } else if img_path.to_lowercase().ends_with(".gif") {
                "image/gif"
            } else if img_path.to_lowercase().ends_with(".svg") {
                "image/svg+xml"
            } else {
                "image/jpeg"
            };
            ([(axum::http::header::CONTENT_TYPE, ctype)], data).into_response()
        }
        None => err404(&format!("Image '{img_path}' not found in EPUB.")),
    }
}

// ------------------------------------------------------------------ search

#[derive(Deserialize)]
struct SearchParams {
    #[serde(default)]
    q: Option<String>,
}

async fn search(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(p): Query<SearchParams>,
) -> Response {
    let db = &state.db;
    let Some(q) = p.q.filter(|q| !q.is_empty()) else {
        return axum::Json(json!({"authors": [], "books": [], "topics": []})).into_response();
    };
    let query = q.to_lowercase();
    let like = format!("%{}%", query.replace('%', ""));

    let authors: Vec<Value> = sqlx::query(
        "SELECT * FROM ebook_authors WHERE normalized_name LIKE ?",
    )
    .bind(&like)
    .fetch_all(db)
    .await
    .map(|rows| {
        rows.iter()
            .map(|r| json!({"id": r.get::<i64,_>("id"), "name": r.try_get::<String,_>("name").unwrap_or_default()}))
            .collect()
    })
    .unwrap_or_default();

    let vis = if is_super(&user) {
        " WHERE 1=1".to_string()
    } else {
        visibility_where(&accessible_topic_ids(db, user.id).await)
    };
    let books = fetch_books(
        db,
        &format!("{vis} AND (b.normalized_title LIKE ? OR b.author_id IN \
                  (SELECT id FROM ebook_authors WHERE normalized_name LIKE ?))"),
        &[&like, &like],
    )
    .await;

    let topics: Vec<Value> = sqlx::query("SELECT id, name FROM ebook_topics WHERE normalized_name LIKE ?")
        .bind(&like)
        .fetch_all(db)
        .await
        .map(|rows| {
            rows.iter()
                .map(|r| {
                    json!({
                        "id": r.get::<String,_>("id"),
                        "name": r.try_get::<String,_>("name").unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    axum::Json(json!({
        "authors": authors,
        "books": books.iter().map(|b| json!({
            "id": b["id"],
            "title": b["title"],
            "author_name": b["author"]["name"],
            "author_id": b["author_id"],
            "cover": b["cover_filename"],
        })).collect::<Vec<_>>(),
        "topics": topics,
    }))
    .into_response()
}

// ------------------------------------------------------------------ reading lists

async fn list_reading_lists(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    let db = &state.db;
    let lists: Vec<Value> = sqlx::query(
        "SELECT name, user_id, MAX(created_at) AS created_at FROM ebook_reading_lists \
         WHERE user_id = ? GROUP BY name ORDER BY created_at DESC",
    )
    .bind(user.id)
    .fetch_all(db)
    .await
    .map(|rows| {
        rows.iter()
            .map(|r| {
                json!({
                    "name": r.get::<String,_>("name"),
                    "user_id": r.get::<i64,_>("user_id"),
                    "created_at": r.try_get::<Option<String>,_>("created_at").ok().flatten(),
                })
            })
            .collect()
    })
    .unwrap_or_default();
    page(
        &state,
        "ebook/reading_lists.html",
        json!({"user": crate::core::user_json(&user), "reading_lists": lists}),
    )
}

#[derive(Deserialize)]
struct ReadingListParams {
    #[serde(default)]
    file_type: Option<String>,
}

async fn view_reading_list(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(reading_list_name): Path<String>,
    Query(p): Query<ReadingListParams>,
) -> Response {
    let db = &state.db;
    let exists = sqlx::query("SELECT 1 FROM ebook_reading_lists WHERE name = ? AND user_id = ?")
        .bind(&reading_list_name)
        .bind(user.id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if exists.is_none() {
        return err404("Reading list not found");
    }
    let rows: Vec<(i64, i64)> = sqlx::query(
        "SELECT id, item_id FROM ebook_reading_list_items \
         WHERE reading_list_name = ? AND user_id = ? ORDER BY id ASC",
    )
    .bind(&reading_list_name)
    .bind(user.id)
    .fetch_all(db)
    .await
    .map(|rows| rows.iter().map(|r| (r.get::<i64,_>("id"), r.get::<i64,_>("item_id"))).collect())
    .unwrap_or_default();

    let mut items: Vec<Value> = Vec::new();
    for (item_row_id, book_id) in rows {
        let Some(book) = get_book(db, book_id).await else {
            continue;
        };
        if let Some(ft) = &p.file_type {
            if book["file_type"].as_str() != Some(ft.as_str()) {
                continue;
            }
        }
        items.push(json!({
            "type": "book",
            "id": item_row_id,
            "item_id": book_id,
            "title": book["title"],
            "author": book["author"]["name"],
            "cover": book["cover_filename"],
            "file_type": book["file_type"],
        }));
    }
    page(
        &state,
        "ebook/reading_list_details.html",
        json!({
            "user": crate::core::user_json(&user),
            "reading_list_name": reading_list_name,
            "items": items,
            "file_type": p.file_type,
        }),
    )
}

async fn create_reading_list(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    RawForm(form): RawForm,
) -> Response {
    let db = &state.db;
    let form = form_map(&form);
    let Some(name) = fv(&form, "name") else {
        return err400("Reading list name is required");
    };
    let item_type = fv(&form, "type").unwrap_or_else(|| "book".into());
    let item_id = fv(&form, "item_id").and_then(|v| v.parse::<i64>().ok());

    if sqlx::query("SELECT 1 FROM ebook_reading_lists WHERE name = ? AND user_id = ?")
        .bind(&name)
        .bind(user.id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .is_some()
    {
        return err400("Reading list with this name already exists");
    }
    let _ = sqlx::query(
        "INSERT INTO ebook_reading_lists (name, user_id, created_at) VALUES (?, ?, ?)",
    )
    .bind(&name)
    .bind(user.id)
    .bind(now_str())
    .execute(db)
    .await;
    if let Some(iid) = item_id {
        let _ = sqlx::query(
            "INSERT INTO ebook_reading_list_items (reading_list_name, user_id, item_type, item_id) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(&name)
        .bind(user.id)
        .bind(&item_type)
        .bind(iid)
        .execute(db)
        .await;
    }
    axum::Json(json!({
        "success": true,
        "message": format!("Reading list '{name}' created"),
        "reading_list_name": name,
    }))
    .into_response()
}

async fn add_to_reading_list(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(reading_list_name): Path<String>,
    RawForm(form): RawForm,
) -> Response {
    let db = &state.db;
    let form = form_map(&form);
    let item_type = fv(&form, "type").unwrap_or_else(|| "book".into());
    let item_id = fv(&form, "item_id").and_then(|v| v.parse::<i64>().ok());

    let exists = sqlx::query("SELECT 1 FROM ebook_reading_lists WHERE name = ? AND user_id = ?")
        .bind(&reading_list_name)
        .bind(user.id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if exists.is_none() {
        return err404("Reading list not found");
    }
    let already = sqlx::query(
        "SELECT 1 FROM ebook_reading_list_items WHERE reading_list_name = ? AND user_id = ? \
         AND item_type = ? AND item_id = ?",
    )
    .bind(&reading_list_name)
    .bind(user.id)
    .bind(&item_type)
    .bind(item_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten();
    if already.is_some() {
        return axum::Json(json!({
            "success": true,
            "message": format!("Already in reading list '{reading_list_name}'"),
            "reading_list_name": reading_list_name,
        }))
        .into_response();
    }
    let _ = sqlx::query(
        "INSERT INTO ebook_reading_list_items (reading_list_name, user_id, item_type, item_id) \
         VALUES (?, ?, ?, ?)",
    )
    .bind(&reading_list_name)
    .bind(user.id)
    .bind(&item_type)
    .bind(item_id)
    .execute(db)
    .await;
    axum::Json(json!({
        "success": true,
        "message": format!("Added to reading list '{reading_list_name}'"),
        "reading_list_name": reading_list_name,
    }))
    .into_response()
}

async fn delete_reading_list(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(reading_list_name): Path<String>,
) -> Response {
    let db = &state.db;
    let _ = sqlx::query(
        "DELETE FROM ebook_reading_list_items WHERE reading_list_name = ? AND user_id = ?",
    )
    .bind(&reading_list_name)
    .bind(user.id)
    .execute(db)
    .await;
    let _ = sqlx::query("DELETE FROM ebook_reading_lists WHERE name = ? AND user_id = ?")
        .bind(&reading_list_name)
        .bind(user.id)
        .execute(db)
        .await;
    redirect("/library/ebook/reading-lists")
}

async fn delete_reading_list_item(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((reading_list_name, item_id)): Path<(String, i64)>,
) -> Response {
    let db = &state.db;
    let _ = sqlx::query(
        "DELETE FROM ebook_reading_list_items WHERE id = ? AND reading_list_name = ? AND user_id = ?",
    )
    .bind(item_id)
    .bind(&reading_list_name)
    .bind(user.id)
    .execute(db)
    .await;
    redirect(&format!("/library/ebook/reading-lists/{reading_list_name}"))
}

async fn analyze_reading_list(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(reading_list_name): Path<String>,
) -> Response {
    let db = &state.db;
    let exists = sqlx::query("SELECT 1 FROM ebook_reading_lists WHERE name = ? AND user_id = ?")
        .bind(&reading_list_name)
        .bind(user.id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if exists.is_none() {
        return err404("Reading list not found");
    }
    let book_ids: Vec<i64> = sqlx::query(
        "SELECT item_id FROM ebook_reading_list_items \
         WHERE reading_list_name = ? AND user_id = ? AND item_type = 'book'",
    )
    .bind(&reading_list_name)
    .bind(user.id)
    .fetch_all(db)
    .await
    .map(|rows| rows.iter().map(|r| r.get::<i64,_>("item_id")).collect())
    .unwrap_or_default();
    if book_ids.is_empty() {
        return err400("No books in reading list");
    }
    let total_books = book_ids.len() as i64;
    let total_size_bytes: i64 = sqlx::query(
        "SELECT COALESCE(SUM(file_size), 0) FROM ebook_books WHERE id IN \
         (SELECT item_id FROM ebook_reading_list_items WHERE reading_list_name = ? AND user_id = ? AND item_type = 'book')",
    )
    .bind(&reading_list_name)
    .bind(user.id)
    .fetch_one(db)
    .await
    .map(|r| r.get::<i64,_>(0))
    .unwrap_or(0);
    axum::Json(json!({
        "total_books": total_books,
        "total_files": total_books * 2,
        "total_size_mb": total_size_bytes as f64 / (1024.0 * 1024.0),
    }))
    .into_response()
}

async fn export_reading_list(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(reading_list_name): Path<String>,
) -> Response {
    let db = &state.db;
    let exists = sqlx::query("SELECT 1 FROM ebook_reading_lists WHERE name = ? AND user_id = ?")
        .bind(&reading_list_name)
        .bind(user.id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten();
    if exists.is_none() {
        return err404("Reading list not found");
    }
    let rows: Vec<(i64, String, String, String)> = sqlx::query(
        "SELECT i.id, a.name, b.title, b.file_path \
         FROM ebook_reading_list_items i \
         JOIN ebook_books b ON b.id = i.item_id \
         JOIN ebook_authors a ON a.id = b.author_id \
         WHERE i.reading_list_name = ? AND i.user_id = ? AND i.item_type = 'book' \
         ORDER BY i.id ASC",
    )
    .bind(&reading_list_name)
    .bind(user.id)
    .fetch_all(db)
    .await
    .map(|rows| {
        rows.iter()
            .map(|r| {
                (
                    r.get::<i64,_>("id"),
                    r.try_get::<String,_>("name").unwrap_or_default(),
                    r.try_get::<String,_>("title").unwrap_or_default(),
                    r.try_get::<String,_>("file_path").unwrap_or_default(),
                )
            })
            .collect()
    })
    .unwrap_or_default();
    if rows.is_empty() {
        return err400("No books found in reading list");
    }

    let list_name = reading_list_name.clone();
    let bytes = tokio::task::spawn_blocking(move || build_reading_list_tgz(&list_name, &rows))
        .await
        .ok()
        .flatten();
    match bytes {
        Some(data) => (
            [
                (axum::http::header::CONTENT_TYPE, "application/gzip".to_string()),
                (
                    axum::http::header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{reading_list_name}.tgz\""),
                ),
            ],
            data,
        )
            .into_response(),
        None => err400("Failed to build archive"),
    }
}

fn build_reading_list_tgz(list_name: &str, rows: &[(i64, String, String, String)]) -> Option<Vec<u8>> {
    let enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(enc);
    for (_, author, title, file_path) in rows {
        let path = std::path::Path::new(file_path);
        if !path.is_file() {
            continue;
        }
        let safe_author = author.replace(['/', '\\'], "_");
        let safe_title = title.replace(['/', '\\'], "_");
        let filename = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let entry_path = format!("{list_name}/{} - {}/{}", safe_author, safe_title, filename);
        let data = std::fs::read(path).ok()?;
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append_data(&mut header, &entry_path, data.as_slice()).ok()?;
    }
    builder.into_inner().ok()?.finish().ok()
}

// ------------------------------------------------------------------ author / scan / admin

async fn view_author(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(author_id): Path<i64>,
    Query(p): Query<PageParams>,
) -> Response {
    let db = &state.db;
    let Some(author) = sqlx::query("SELECT * FROM ebook_authors WHERE id = ?")
        .bind(author_id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
    else {
        return err404("Author not found");
    };
    let (page_no, per_page, offset, _) = page_params(&p, 24, 12, 120);

    let vis = if is_super(&user) {
        String::new()
    } else {
        visibility_where(&accessible_topic_ids(db, user.id).await)
    };
    let cond = format!("{vis} AND b.author_id = {author_id}");
    // vis is "" for admins; make sure the filter lands in a WHERE clause and
    // is not silently absorbed into the JOIN's ON condition.
    let cond = if cond.starts_with(" AND") {
        format!(" WHERE 1=1{cond}")
    } else {
        cond
    };
    let total = count_scalar(db, &format!("SELECT COUNT(*) FROM ebook_books b{cond}"), None).await;
    let books = fetch_books(
        db,
        &format!("{cond} LIMIT ? OFFSET ?"),
        &[&per_page.to_string(), &offset.to_string()],
    )
    .await;
    let reading_lists = reading_lists_for_dropdown(db, user.id).await;

    page(
        &state,
        "ebook/author.html",
        json!({
            "user": crate::core::user_json(&user),
            "author": {
                "id": author.get::<i64,_>("id"),
                "name": author.try_get::<String,_>("name").unwrap_or_default(),
            },
            "books": books,
            "total": total,
            "current_page": page_no,
            "total_pages": ((total) + per_page - 1) / per_page,
            "use_pagination": total > per_page,
            "reading_lists": reading_lists,
            "from_letter": p.from_letter,
        }),
    )
}

async fn scan_my_library(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let user_dir = std::path::Path::new(&root("ebook")).join(&user.username);
    if !user_dir.is_dir() {
        return err404(&format!(
            "Your library directory does not exist: {}",
            user_dir.to_string_lossy()
        ));
    }
    let db = state.db.clone();
    let path = user_dir.to_string_lossy().into_owned();
    let msg_path = path.clone();
    tokio::spawn(async move {
        crate::scanner::scan_ebooks(&db, Some(path), false).await;
    });
    axum::Json(json!({"message": format!("Scanning your library: {msg_path}")})).into_response()
}

async fn trigger_scan(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = state.db.clone();
    tokio::spawn(async move {
        crate::scanner::scan_ebooks(&db, None, false).await;
    });
    axum::Json(json!({"message": "Library scan started in background."})).into_response()
}

async fn force_scan(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = state.db.clone();
    tokio::spawn(async move {
        crate::scanner::scan_ebooks(&db, None, true).await;
    });
    axum::Json(json!({"message": "Force scan started"})).into_response()
}

async fn admin_page(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;

    let mut public_topics: Vec<Value> = Vec::new();
    {
    let raw_rows = sqlx::query(
        "SELECT t.*, u.username AS owner_username FROM ebook_topics t \
         JOIN users u ON u.id = t.user_id WHERE t.is_public = 1 ORDER BY t.name"
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();
    for r in &raw_rows {
        let t = topic_row(r);
        let id = t["id"].as_str().unwrap_or("").to_string();
        let count = count_scalar(
            db,
            "SELECT COUNT(*) FROM ebook_books WHERE topic_id = ?",
            Some(&id),
        ).await;
        public_topics.push(json!({
            "topic": t,
            "owner_username": r.try_get::<String,_>("owner_username").unwrap_or_default(),
            "book_count": count,
        }));
    }
    }

    let banned_books: Vec<Value> = sqlx::query(&format!(
        "{BOOK_SELECT} LEFT JOIN ebook_topics t ON t.id = b.topic_id \
         WHERE b.is_banned = 1 ORDER BY b.created_at DESC"
    ))
    .fetch_all(db)
    .await
    .map(|rows| {
        rows.iter()
            .map(|r| {
                json!({
                    "book": book_row(r),
                    "topic": Value::Null,
                })
            })
            .collect()
    })
    .unwrap_or_default();

    let mut all_topics: Vec<Value> = Vec::new();
    {
    let raw_rows = sqlx::query(
        "SELECT t.*, u.username AS owner_username FROM ebook_topics t \
         JOIN users u ON u.id = t.user_id ORDER BY t.name"
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();
    for r in &raw_rows {
        let t = topic_row(r);
        let id = t["id"].as_str().unwrap_or("").to_string();
        let count = count_scalar(
            db,
            "SELECT COUNT(*) FROM ebook_books WHERE topic_id = ?",
            Some(&id),
        ).await;
        all_topics.push(json!({
            "topic": t,
            "owner_username": r.try_get::<String,_>("owner_username").unwrap_or_default(),
            "book_count": count,
        }));
    }
    }

    page(
        &state,
        "ebook/admin.html",
        json!({
            "user": crate::core::user_json(&user),
            "public_topics": public_topics,
            "banned_books": banned_books,
            "all_topics": all_topics,
        }),
    )
}

async fn admin_purge_empty_topics(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let _ = sqlx::query(
        "DELETE FROM ebook_topic_shares WHERE topic_id IN (\n         SELECT id FROM ebook_topics WHERE id NOT IN\n         (SELECT DISTINCT topic_id FROM ebook_books WHERE topic_id IS NOT NULL))",
    )
    .execute(db)
    .await;
    let _ = sqlx::query(
        "DELETE FROM ebook_topics WHERE id NOT IN\n         (SELECT DISTINCT topic_id FROM ebook_books WHERE topic_id IS NOT NULL)",
    )
    .execute(db)
    .await;
    axum::response::Redirect::to("/library/ebook/admin").into_response()
}

async fn admin_set_all_topics_visibility(
    state: &SharedState,
    user: &syarc_plugin_api::User,
    public: bool,
) -> Response {
    if !is_super(user) {
        return err403("admin access required");
    }
    let _ = sqlx::query("UPDATE ebook_topics SET is_public = ?")
        .bind(public)
        .execute(&state.db)
        .await;
    axum::response::Redirect::to("/library/ebook/admin").into_response()
}

async fn admin_all_topics_public(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
) -> Response {
    admin_set_all_topics_visibility(&state, &user, true).await
}

async fn admin_all_topics_private(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
) -> Response {
    admin_set_all_topics_visibility(&state, &user, false).await
}

async fn admin_assign_orphan_topics(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let orphans: Vec<(i64, String, Option<i64>)> = sqlx::query(
        "SELECT id, file_path, user_id FROM ebook_books WHERE topic_id IS NULL",
    )
    .fetch_all(db)
    .await
    .map(|rows| {
        rows.iter()
            .map(|r| {
                (
                    r.get::<i64,_>("id"),
                    r.try_get::<String,_>("file_path").unwrap_or_default(),
                    r.try_get::<Option<i64>,_>("user_id").ok().flatten(),
                )
            })
            .collect()
    })
    .unwrap_or_default();
    if orphans.is_empty() {
        return axum::Json(json!({
            "success": true,
            "message": "No orphan books found.",
            "assigned": 0,
        }))
        .into_response();
    }

    let mut assigned = 0;
    for (book_id, file_path, owner_id) in orphans {
        let Some(owner) = owner_id else { continue };
        let Some(parent) = std::path::Path::new(&file_path).parent() else {
            continue;
        };
        if !parent.is_dir() {
            continue;
        }
        let book_dir = parent.to_string_lossy().into_owned();
        let topic_name = parent
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        let topic_id = match sqlx::query(
            "SELECT id FROM ebook_topics WHERE user_id = ? AND directory_path = ?",
        )
        .bind(owner)
        .bind(&book_dir)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        {
            Some(r) => r.get::<String,_>("id"),
            None => {
                let norm = topic_name.to_lowercase().trim().to_string();
                match sqlx::query(
                    "SELECT id FROM ebook_topics WHERE user_id = ? AND normalized_name = ?",
                )
                .bind(owner)
                .bind(&norm)
                .fetch_optional(db)
                .await
                .ok()
                .flatten()
                {
                    Some(r) => {
                        let id = r.get::<String,_>("id");
                        let _ = sqlx::query(
                            "UPDATE ebook_topics SET directory_path = ? WHERE id = ?",
                        )
                        .bind(&book_dir)
                        .bind(&id)
                        .execute(db)
                        .await;
                        id
                    }
                    None => {
                        let id = new_uuid();
                        let _ = sqlx::query(
                            "INSERT INTO ebook_topics (id, user_id, name, normalized_name, \
                             directory_path, is_public, book_count, created_at, updated_at) \
                             VALUES (?, ?, ?, ?, ?, 0, 0, ?, ?)",
                        )
                        .bind(&id)
                        .bind(owner)
                        .bind(&topic_name)
                        .bind(&norm)
                        .bind(&book_dir)
                        .bind(now_str())
                        .bind(now_str())
                        .execute(db)
                        .await;
                        id
                    }
                }
            }
        };
        let _ = sqlx::query("UPDATE ebook_books SET topic_id = ? WHERE id = ?")
            .bind(&topic_id)
            .bind(book_id)
            .execute(db)
            .await;
        refresh_topic_count(db, &topic_id).await;
        assigned += 1;
    }

    axum::Json(json!({
        "success": true,
        "message": format!("Assigned {assigned} orphan book(s) to topics based on their directory."),
        "assigned": assigned,
    }))
    .into_response()
}

async fn ban_book(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(book_id): Path<i64>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let _ = sqlx::query(
        "UPDATE ebook_books SET is_banned = 1, is_public = 0, updated_at = ? WHERE id = ?",
    )
    .bind(now_str())
    .bind(book_id)
    .execute(&state.db)
    .await;
    axum::Json(json!({"message": "Book banned and removed from public view."})).into_response()
}

async fn unban_book(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(book_id): Path<i64>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let _ = sqlx::query("UPDATE ebook_books SET is_banned = 0, updated_at = ? WHERE id = ?")
        .bind(now_str())
        .bind(book_id)
        .execute(&state.db)
        .await;
    axum::Json(json!({"message": "Book unbanned."})).into_response()
}

async fn admin_remove_public_book(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(book_id): Path<i64>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let _ = sqlx::query("UPDATE ebook_books SET is_public = 0, updated_at = ? WHERE id = ?")
        .bind(now_str())
        .bind(book_id)
        .execute(&state.db)
        .await;
    axum::Json(json!({"message": "Book removed from public view."})).into_response()
}

async fn admin_remove_public_topic(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(topic_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let Some(topic) = get_topic(&state.db, &topic_id).await else {
        return err404("Topic not found");
    };
    make_topic_private(&state.db, &topic_id).await;
    let name = topic["name"].as_str().unwrap_or("").to_string();
    axum::Json(json!({"message": format!("Topic '{name}' removed from public.")})).into_response()
}

async fn admin_toggle_topic_visibility(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(topic_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let Some(topic) = get_topic(db, &topic_id).await else {
        return err404("Topic not found");
    };
    let is_public = if topic["is_public"].as_bool().unwrap_or(false) {
        make_topic_private(db, &topic_id).await;
        false
    } else {
        make_topic_public(db, &topic_id).await;
        true
    };
    axum::Json(json!({
        "message": format!("Topic is now {}.", if is_public { "public" } else { "private" }),
        "is_public": is_public,
    }))
    .into_response()
}

async fn admin_rescan_topic(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(topic_id): Path<String>,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let owner = get_topic(db, &topic_id)
        .await
        .and_then(|t| t["user_id"].as_i64());
    let Some(owner) = owner else {
        return err404("Topic not found");
    };
    run_topic_rescan(db, &topic_id, owner).await
}

// ------------------------------------------------------------------ static covers

/// Serve a book's cover from the sibling file next to the book, falling back
/// to the legacy covers directory for fastmain-migrated covers.
async fn cover_by_book(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(book_id): Path<i64>,
) -> Response {
    let db = &state.db;
    let Ok(Some(r)) = sqlx::query(
        "SELECT file_path, cover_filename, is_public, topic_id FROM ebook_books WHERE id = ?",
    )
    .bind(book_id)
    .fetch_optional(db)
    .await
    else {
        return err404("Book not found");
    };
    let file_path = r.try_get::<String,_>("file_path").unwrap_or_default();
    let cover_filename = r.try_get::<Option<String>,_>("cover_filename").ok().flatten();
    let is_public = r.try_get::<i64,_>("is_public").unwrap_or(0) == 1;
    let topic_id = r.try_get::<Option<String>,_>("topic_id").ok().flatten();
    if !is_super(&user) && !is_public {
        let ok = match topic_id.as_deref() {
            Some(t) => user_has_topic_access(db, t, user.id).await,
            None => false,
        };
        if !ok {
            return err403("Access denied");
        }
    }
    let mut path = sibling_cover_path(&file_path);
    if !path.is_file() {
        if let Some(cf) = cover_filename.as_deref().filter(|c| *c != COVER_SIBLING) {
            let legacy = covers_dir().join(cf);
            if legacy.is_file() {
                path = legacy;
            }
        }
    }
    if !path.is_file() {
        return err404("no cover");
    }
    match tokio::fs::read(&path).await {
        Ok(bytes) => serve_image(bytes, &path).await,
        Err(_) => err404("not found"),
    }
}

async fn serve_image(bytes: Vec<u8>, path: &std::path::Path) -> Response {
    let ctype = match path.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "image/jpeg",
    };
    ([(axum::http::header::CONTENT_TYPE, ctype)], bytes).into_response()
}

/// Admin: remove books whose files are gone, then rescan every topic
/// directory (port of fastmain's "clean database and rescan library").
async fn admin_clean_rescan(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let before = count_scalar(db, "SELECT COUNT(*) FROM ebook_books", None).await;

    // Books with no topic whose file vanished.
    let orphans: Vec<(i64, String, Option<String>)> = sqlx::query(
        "SELECT id, file_path, cover_filename FROM ebook_books WHERE topic_id IS NULL",
    )
    .fetch_all(db)
    .await
    .map(|rows| {
        rows.iter()
            .map(|r| {
                (
                    r.get::<i64,_>("id"),
                    r.try_get::<String,_>("file_path").unwrap_or_default(),
                    r.try_get::<Option<String>,_>("cover_filename").ok().flatten(),
                )
            })
            .collect()
    })
    .unwrap_or_default();
    let mut removed_orphans = 0i64;
    for (id, path, cover) in &orphans {
        if !path.is_empty() && !std::path::Path::new(path).exists() {
            remove_book_cover(path, cover.as_deref());
            let _ = sqlx::query("DELETE FROM ebook_reading_list_items WHERE item_type = 'book' AND item_id = ?")
                .bind(id).execute(db).await;
            let _ = sqlx::query("DELETE FROM ebook_books WHERE id = ?").bind(id).execute(db).await;
            removed_orphans += 1;
        }
    }

    // Rescan every topic that has a directory (run_topic_rescan also prunes
    // books with missing files inside the topic).
    let topics: Vec<String> = sqlx::query(
        "SELECT id FROM ebook_topics WHERE directory_path IS NOT NULL AND directory_path != ''",
    )
    .fetch_all(db)
    .await
    .map(|rows| rows.iter().map(|r| r.get::<String,_>("id")).collect())
    .unwrap_or_default();
    let mut rescanned = 0i64;
    for t in &topics {
        let _ = run_topic_rescan(db, t, user.id).await;
        rescanned += 1;
    }
    let after = count_scalar(db, "SELECT COUNT(*) FROM ebook_books", None).await;
    axum::Json(json!({
        "message": format!(
            "Cleaned {removed_orphans} orphaned book(s), rescanned {rescanned} topic(s). Books: {before} -> {after}"
        )
    }))
    .into_response()
}

/// Admin: (re)generate cover art next to each book file. Existing legacy
/// covers from fastmain are copied into the library tree; books without any
/// cover get one extracted from the epub/pdf.
async fn admin_rebuild_covers(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
) -> Response {
    if !is_super(&user) {
        return err403("admin access required");
    }
    let db = &state.db;
    let books: Vec<(i64, String, Option<String>)> = sqlx::query(
        "SELECT id, file_path, cover_filename FROM ebook_books",
    )
    .fetch_all(db)
    .await
    .map(|rows| {
        rows.iter()
            .map(|r| {
                (
                    r.get::<i64,_>("id"),
                    r.try_get::<String,_>("file_path").unwrap_or_default(),
                    r.try_get::<Option<String>,_>("cover_filename").ok().flatten(),
                )
            })
            .collect()
    })
    .unwrap_or_default();
    let mut copied = 0i64;
    let mut extracted = 0i64;
    let mut missing = 0i64;
    for (id, path, cover) in &books {
        if path.is_empty() || !std::path::Path::new(path).exists() {
            missing += 1;
            continue;
        }
        let sibling = sibling_cover_path(path);
        if sibling.is_file() {
            if cover.as_deref() != Some(COVER_SIBLING) {
                let _ = sqlx::query("UPDATE ebook_books SET cover_filename = ? WHERE id = ?")
                    .bind(COVER_SIBLING).bind(id).execute(db).await;
            }
            continue;
        }
        if let Some(cf) = cover.as_deref().filter(|c| *c != COVER_SIBLING && !c.is_empty()) {
            let legacy = covers_dir().join(cf);
            if legacy.is_file() && std::fs::copy(&legacy, &sibling).is_ok() {
                let _ = sqlx::query("UPDATE ebook_books SET cover_filename = ? WHERE id = ?")
                    .bind(COVER_SIBLING).bind(id).execute(db).await;
                copied += 1;
                continue;
            }
        }
        let file_type = if path.to_lowercase().ends_with(".pdf") { "pdf" } else { "epub" };
        if extract_cover(path.clone(), *id, file_type.to_string()).await.is_some() {
            let _ = sqlx::query("UPDATE ebook_books SET cover_filename = ? WHERE id = ?")
                .bind(COVER_SIBLING).bind(id).execute(db).await;
            extracted += 1;
        }
    }
    axum::Json(json!({
        "message": format!(
            "Copied {copied} existing cover(s) into the library tree, extracted {extracted} new cover(s); {missing} book file(s) missing on disk"
        )
    }))
    .into_response()
}

async fn static_cover(Path(file): Path<String>) -> Response {
    if file.contains("..") || file.contains('/') {
        return err404("not found");
    }
    let path = covers_dir().join(&file);
    if !path.is_file() {
        return err404("not found");
    }
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let ctype = match file.rsplit('.').next().unwrap_or("") {
                "png" => "image/png",
                "gif" => "image/gif",
                "webp" => "image/webp",
                _ => "image/jpeg",
            };
            ([(axum::http::header::CONTENT_TYPE, ctype)], bytes).into_response()
        }
        Err(_) => err404("not found"),
    }
}
