//! Music directory scanner: walks ROOT/Author/Album/track.mp3 (3 deep),
//! extracting tags and embedded art. Ported from the Python scanner.

use sqlx::Row;

use crate::music::{create_track, get_or_create_album, read_audio_meta};

pub async fn scan_music(db: &syarc_plugin_api::Db, root_path: Option<String>, force: bool) {
    let root = root_path.unwrap_or_else(|| crate::root("music"));

    if force {
        let _ = sqlx::query("DELETE FROM music_playlist_items").execute(db).await;
        let _ = sqlx::query("DELETE FROM music_playlists").execute(db).await;
        let _ = sqlx::query(
            "DELETE FROM music_tracks WHERE album_id IN (SELECT id FROM music_albums WHERE media_type = 'music')",
        )
        .execute(db)
        .await;
        let _ = sqlx::query("DELETE FROM music_albums WHERE media_type = 'music'").execute(db).await;
        let _ = sqlx::query(
            "DELETE FROM music_authors WHERE id NOT IN (SELECT author_id FROM music_albums)",
        )
        .execute(db)
        .await;
    }

    // resolve scan user from root basename (per-user library directories)
    let base = root.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string();
    let mut scan_user: Option<i64> = None;
    if !base.is_empty() && base != crate::root("music").trim_end_matches('/').rsplit('/').next().unwrap_or("") {
        if let Ok(Some(r)) = sqlx::query("SELECT id FROM users WHERE username = ?")
            .bind(&base)
            .fetch_optional(db)
            .await
        {
            scan_user = r.try_get::<i64, _>("id").ok();
        }
    }

    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut mp3s: Vec<String> = Vec::new();
        let mut subdirs: Vec<String> = Vec::new();
        for e in rd.flatten() {
            let p = e.path();
            let name = p.to_string_lossy().to_lowercase();
            if p.is_dir() {
                subdirs.push(p.to_string_lossy().to_string());
            } else if name.ends_with(".mp3") {
                mp3s.push(p.to_string_lossy().to_string());
            }
        }
        if mp3s.is_empty() {
            subdirs.sort();
            stack.extend(subdirs);
            continue;
        }

        // depth relative to scan root determines author/album naming
        let rel = dir.strip_prefix(&root).unwrap_or(&dir).trim_matches('/');
        let depth = if rel.is_empty() { 0 } else { rel.split('/').count() };
        let parts: Vec<&str> = dir.trim_end_matches('/').split('/').collect();
        let (author_dir, display_album) = match depth {
            2 => (parts[parts.len() - 2].to_string(), parts[parts.len() - 1].to_string()),
            3 => (
                parts[parts.len() - 3].to_string(),
                format!("{} ({})", parts[parts.len() - 2], parts[parts.len() - 1]),
            ),
            _ => {
                continue; // malformed depth
            }
        };

        mp3s.sort();
        let multi_disc = mp3s.iter().take(5).any(|f| {
            let m = read_audio_meta(f, false);
            m.disc.unwrap_or(0) > 1
        });

        let author_id = get_or_create_author(db, &author_dir).await;
        let album_id = get_or_create_album(
            db,
            author_id,
            &display_album,
            &dir,
            &mp3s[0],
            scan_user,
            true,
        )
        .await;
        for mp3 in &mp3s {
            let meta = read_audio_meta(mp3, multi_disc);
            create_track(db, album_id, mp3, &meta, scan_user, true).await;
        }
    }
    println!("[library] music scan complete");
}

async fn get_or_create_author(db: &syarc_plugin_api::Db, name: &str) -> i64 {
    let name = if name.trim().is_empty() { "Unknown Artist" } else { name.trim() };
    let norm = name.to_lowercase();
    if let Some(r) = sqlx::query("SELECT id FROM music_authors WHERE normalized_name = ?")
        .bind(&norm)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
    {
        return r.get(0);
    }
    sqlx::query("INSERT INTO music_authors (name, normalized_name) VALUES (?,?)")
        .bind(name)
        .bind(&norm)
        .execute(db)
        .await
        .map(|r| r.last_insert_rowid())
        .unwrap_or(0)
}

// ---------------------------------------------------------------- audiobook

async fn ab_get_or_create_author(db: &syarc_plugin_api::Db, name: &str) -> i64 {
    let name = if name.trim().is_empty() { "Unknown Author" } else { name.trim() };
    let norm = name.to_lowercase();
    if let Some(r) = sqlx::query("SELECT id FROM audiobook_authors WHERE normalized_name = ?")
        .bind(&norm)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
    {
        return r.get(0);
    }
    sqlx::query("INSERT INTO audiobook_authors (name, normalized_name) VALUES (?,?)")
        .bind(name)
        .bind(&norm)
        .execute(db)
        .await
        .map(|r| r.last_insert_rowid())
        .unwrap_or(0)
}

fn ab_art_dir() -> std::path::PathBuf {
    crate::static_dir("audiobook").join("art")
}

fn ab_extract_cover(audio_path: &str, book_id: i64, directory_path: &str) -> Option<String> {
    let (data, _is_png) = crate::music::embedded_art(audio_path)?;
    // Art belongs next to the files: <book dir>/cover.jpg, with the
    // central art dir only as a fallback for unwritable trees.
    if !directory_path.is_empty() {
        let p = std::path::Path::new(directory_path).join("cover.jpg");
        if std::fs::write(&p, &data).is_ok() {
            return Some(crate::audiobook::ART_SIBLING.to_string());
        }
    }
    let filename = format!("book_{book_id}.jpg");
    let dir = ab_art_dir();
    let _ = std::fs::create_dir_all(&dir);
    std::fs::write(dir.join(&filename), &data).ok()?;
    Some(filename)
}

/// Discover topics from ROOT/username/topic directories and return a
/// directory_path -> (topic_id, user_id) lookup.
async fn discover_topics(db: &syarc_plugin_api::Db, root: &str) -> Vec<(String, String, i64)> {
    if !std::path::Path::new(root).is_dir() {
        return vec![];
    }
    // Prune topics discovered from disk whose directory has been removed or
    // moved (e.g. a topic folder dragged under another topic).
    let stale: Vec<String> = sqlx::query("SELECT id, directory_path FROM audiobook_topics")
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|r| {
            let id = r.get::<String, _>("id");
            let dp = r.try_get::<Option<String>, _>("directory_path").ok().flatten()?;
            if dp.is_empty() || !dp.starts_with(&format!("{root}/")) {
                return None;
            }
            if std::path::Path::new(&dp).is_dir() {
                None
            } else {
                Some(id)
            }
        })
        .collect();
    for tid in &stale {
        let _ = sqlx::query("DELETE FROM audiobook_topic_shares WHERE topic_id = ?")
            .bind(tid)
            .execute(db)
            .await;
        let _ = sqlx::query("UPDATE audiobook_books SET topic_id = NULL WHERE topic_id = ?")
            .bind(tid)
            .execute(db)
            .await;
        let _ = sqlx::query("DELETE FROM audiobook_topics WHERE id = ?")
            .bind(tid)
            .execute(db)
            .await;
    }
    let users: Vec<(i64, String)> = sqlx::query("SELECT id, username FROM users")
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| (r.get::<i64,_>("id"), r.try_get::<String,_>("username").unwrap_or_default()))
        .collect();
    let existing: Vec<String> = sqlx::query("SELECT directory_path FROM audiobook_topics")
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| r.get::<String,_>("directory_path"))
        .collect();

    let mut usernames: Vec<String> = std::fs::read_dir(root)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().is_dir())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .collect()
        })
        .unwrap_or_default();
    usernames.sort();

    for uname in usernames {
        let Some((uid, _)) = users.iter().find(|(_, u)| u == &uname) else {
            continue;
        };
        let user_dir = format!("{root}/{uname}");
        let mut topics: Vec<String> = std::fs::read_dir(&user_dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.path().is_dir())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .collect()
            })
            .unwrap_or_default();
        topics.sort();
        for tname in topics {
            let topic_dir = format!("{user_dir}/{tname}");
            if existing.contains(&topic_dir) {
                continue;
            }
            let norm = tname.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
            let already = sqlx::query(
                "SELECT 1 FROM audiobook_topics WHERE user_id = ? AND normalized_name = ?",
            )
            .bind(uid)
            .bind(&norm)
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
            .is_some();
            if already {
                continue;
            }
            let _ = sqlx::query(
                "INSERT INTO audiobook_topics (id, user_id, name, normalized_name, directory_path, book_count, created_at, updated_at) \
                 VALUES (?,?,?,?,?,0,?,?)",
            )
            .bind(crate::new_uuid())
            .bind(uid)
            .bind(&tname)
            .bind(&norm)
            .bind(&topic_dir)
            .bind(crate::now_str())
            .bind(crate::now_str())
            .execute(db)
            .await;
        }
    }

    sqlx::query("SELECT id, directory_path, user_id FROM audiobook_topics")
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| (r.get::<String,_>("id"), r.get::<String,_>("directory_path"), r.get::<i64,_>("user_id")))
        .collect()
}

fn find_topic_for_path<'a>(path: &str, lookup: &'a [(String, String, i64)]) -> Option<&'a (String, String, i64)> {
    lookup
        .iter()
        .find(|(_, dir, _)| path == dir || path.starts_with(&format!("{dir}/")))
}


fn ab_series_clean(name: &str) -> String {
    let t = name.trim();
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 && digits <= 2 {
        let after: String = t.chars().skip(digits).collect();
        return after.trim_start().trim_start_matches(['-', '.', '_']).trim().to_string();
    }
    t.to_string()
}

fn ab_series_dir(name: &str) -> bool {
    let t = name.trim();
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 || digits > 2 {
        return false;
    }
    let after: String = t.chars().skip(digits).collect();
    let after = after.trim_start();
    after.starts_with('-') || after.starts_with('.') || after.starts_with('_')
}

/// Derive (author, title) for a directory of audio files.
/// Layout: ROOT/[user]/topic/Author/Book (or files directly in Author).
/// Depth 3 (files directly in an author/series dir): audio-tag artist wins
/// (e.g. m4b "Frank Herbert (audio)") so series dirs don't become authors.
/// Depth >=4 (book dir inside an author/series dir): the parent dir is the
/// author unless it looks like a series dir ("01 - Dune Saga"), in which
/// case the tag artist (or the grandparent dir) is used.
fn ab_author_title(root: &str, dir: &str, tag_artist: Option<&str>) -> Option<(String, String)> {
    let clean = |s: &str| s.trim().trim_end_matches(" (audio)").trim().to_string();
    let rel = dir.strip_prefix(root).unwrap_or(dir).trim_matches('/');
    let depth = if rel.is_empty() { 0 } else { rel.split('/').count() };
    let parts: Vec<&str> = dir.trim_end_matches('/').split('/').collect();
    let (dir_author, title) = match depth {
        2 => (parts[parts.len() - 2].to_string(), parts[parts.len() - 1].to_string()),
        3 => (parts[parts.len() - 1].to_string(), parts[parts.len() - 1].to_string()),
        d if d >= 4 => {
            let parent = parts[parts.len() - 2];
            let author = if ab_series_dir(parent) {
                tag_artist
                    .map(|a| clean(a))
                    .filter(|a| !a.is_empty())
                    .unwrap_or_else(|| ab_series_clean(parent))
            } else {
                clean(parent)
            };
            (author, parts[parts.len() - 1].to_string())
        }
        _ => return None,
    };
    let author = match tag_artist.map(|a| clean(a)).filter(|a| !a.is_empty()) {
        Some(a) if depth < 4 => a,
        _ => clean(&dir_author),
    };
    if author.is_empty() || title.trim().is_empty() {
        return None;
    }
    Some((author, title.trim().to_string()))
}

async fn ab_get_or_create_book(
    db: &syarc_plugin_api::Db,
    author_id: i64,
    folder_name: &str,
    folder_path: &str,
    sample_file: &str,
    topic: Option<&(String, String, i64)>,
) -> i64 {
    if let Some(r) = sqlx::query(
        "SELECT id, topic_id, directory_path FROM audiobook_books WHERE author_id = ? AND title = ?",
    )
    .bind(author_id)
    .bind(folder_name)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    {
        let id: i64 = r.get("id");
        let stored_topic: Option<String> = r.try_get::<Option<String>, _>("topic_id").ok().flatten();
        if let Some(t) = topic {
            if stored_topic.as_deref() != Some(t.0.as_str()) {
                let _ = sqlx::query(
                    "UPDATE audiobook_books SET topic_id = ?, user_id = ?, directory_path = ? WHERE id = ?",
                )
                .bind(&t.0)
                .bind(t.2)
                .bind(folder_path)
                .bind(id)
                .execute(db)
                .await;
            }
        }
        return id;
    }
    let (topic_id, uid) = match topic {
        Some(t) => (Some(t.0.clone()), Some(t.2)),
        None => (None, None),
    };
    let Ok(res) = sqlx::query(
        "INSERT INTO audiobook_books (title, normalized_title, author_id, directory_path, topic_id, user_id, updated_at) \
         VALUES (?,?,?,?,?,?,?)",
    )
    .bind(folder_name)
    .bind(folder_name.to_lowercase().trim())
    .bind(author_id)
    .bind(folder_path)
    .bind(topic_id)
    .bind(uid)
    .bind(crate::now_str())
    .execute(db)
    .await
    else {
        return 0;
    };
    let id = res.last_insert_rowid();
    if let Some(art) = ab_extract_cover(sample_file, id, folder_path) {
        let _ = sqlx::query("UPDATE audiobook_books SET image_filename = ? WHERE id = ?")
            .bind(&art)
            .bind(id)
            .execute(db)
            .await;
    }
    id
}

pub async fn scan_audiobooks(db: &syarc_plugin_api::Db, root_path: Option<String>, force: bool) {
    scan_audiobooks_inner(db, root_path, None, force).await;
}

/// Incremental scan limited to `scope` (an absolute directory under the
/// audiobook root), for the quick-add path. Depth/topic resolution still
/// uses the full root so layout rules are unchanged.
pub async fn scan_audiobooks_scope(db: &syarc_plugin_api::Db, scope: &str) {
    scan_audiobooks_inner(db, None, Some(scope.to_string()), false).await;
}

async fn scan_audiobooks_inner(
    db: &syarc_plugin_api::Db,
    root_path: Option<String>,
    scope: Option<String>,
    force: bool,
) {
    let root = root_path.unwrap_or_else(|| crate::root("audiobook"));
    let start = scope.unwrap_or_else(|| root.clone());

    if force {
        let _ = sqlx::query("DELETE FROM audiobook_playlist_items").execute(db).await;
        let _ = sqlx::query("DELETE FROM audiobook_playlists").execute(db).await;
        let _ = sqlx::query("DELETE FROM audiobook_chapters").execute(db).await;
        let _ = sqlx::query("DELETE FROM audiobook_books").execute(db).await;
        let _ = sqlx::query(
            "DELETE FROM audiobook_authors WHERE id NOT IN (SELECT author_id FROM audiobook_books)",
        )
        .execute(db)
        .await;
        let dir = ab_art_dir();
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with("book_") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }

    let topic_lookup = discover_topics(db, &root).await;

    // link orphaned books to topics
    let orphans: Vec<(i64, Option<String>)> = sqlx::query(
        "SELECT id, directory_path FROM audiobook_books WHERE topic_id IS NULL",
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| (r.get::<i64,_>("id"), r.try_get::<Option<String>,_>("directory_path").ok().flatten()))
    .collect();
    for (book_id, dirpath) in &orphans {
        if let Some(dp) = dirpath {
            if let Some(t) = find_topic_for_path(dp, &topic_lookup) {
                let _ = sqlx::query("UPDATE audiobook_books SET topic_id = ?, user_id = ? WHERE id = ?")
                    .bind(&t.0)
                    .bind(t.2)
                    .bind(book_id)
                    .execute(db)
                    .await;
            }
        }
    }

    // MP3 pass: ROOT/[user/]topic/Author/Book (depth 2 or 3 from scan root)
    let mut stack = vec![start.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut mp3s: Vec<String> = Vec::new();
        let mut subdirs: Vec<String> = Vec::new();
        for e in rd.flatten() {
            let p = e.path();
            let name = p.to_string_lossy().to_lowercase();
            if p.is_dir() {
                subdirs.push(p.to_string_lossy().to_string());
            } else if name.ends_with(".mp3") {
                mp3s.push(p.to_string_lossy().to_string());
            }
        }
        if mp3s.is_empty() {
            subdirs.sort();
            stack.extend(subdirs);
            continue;
        }
        let sample_meta = crate::music::read_audio_meta(&mp3s[0], false);
        let Some((author_name, display_book)) =
            ab_author_title(&root, &dir, sample_meta.artist.as_deref())
        else {
            continue;
        };

        mp3s.sort();
        let multi_disc = mp3s.iter().take(5).any(|f| {
            crate::music::read_audio_meta(f, false).disc.unwrap_or(0) > 1
        });

        // Loose MP3s directly in an author dir (depth 3) are usually
        // separate books: group them by album tag, one book per album.
        let rel = dir.strip_prefix(root.as_str()).unwrap_or(dir.as_str()).trim_matches('/');
        let depth = if rel.is_empty() { 0 } else { rel.split('/').count() };
        let mut groups: Vec<(String, Vec<String>)> = Vec::new();
        if depth == 3 {
            for mp3 in &mp3s {
                let meta = crate::music::read_audio_meta(mp3, false);
                let key = meta
                    .album
                    .as_deref()
                    .map(str::trim)
                    .filter(|a| !a.is_empty())
                    .unwrap_or(&display_book)
                    .to_string();
                match groups.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, v)) => v.push(mp3.clone()),
                    None => groups.push((key, vec![mp3.clone()])),
                }
            }
        } else {
            groups.push((display_book.clone(), mp3s.clone()));
        }

        let author_id = ab_get_or_create_author(db, &author_name).await;
        let topic = find_topic_for_path(&dir, &topic_lookup);
        for (group_title, group_files) in &groups {
            let book_id = ab_get_or_create_book(db, author_id, group_title, &dir, &group_files[0], topic).await;
            for mp3 in group_files {
                let exists = sqlx::query("SELECT 1 FROM audiobook_chapters WHERE file_path = ?")
                    .bind(mp3)
                    .fetch_optional(db)
                    .await
                    .ok()
                    .flatten()
                    .is_some();
                if exists {
                    continue;
                }
                let meta = crate::music::read_audio_meta(mp3, multi_disc);
                let _ = sqlx::query(
                    "INSERT INTO audiobook_chapters (title, file_path, track_number, size, book_id) VALUES (?,?,?,?,?)",
                )
                .bind(&meta.title)
                .bind(mp3)
                .bind(&meta.track_num)
                .bind(meta.size_mb)
                .bind(book_id)
                .execute(db)
                .await;
            }
        }
    }

    // M4B/M4A pass: one chapter per file
    let mut stack = vec![start.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut m4bs: Vec<String> = Vec::new();
        let mut subdirs: Vec<String> = Vec::new();
        for e in rd.flatten() {
            let p = e.path();
            let name = p.to_string_lossy().to_lowercase();
            if p.is_dir() {
                subdirs.push(p.to_string_lossy().to_string());
            } else if name.ends_with(".m4b") || name.ends_with(".m4a") {
                m4bs.push(p.to_string_lossy().to_string());
            }
        }
        subdirs.sort();
        stack.extend(subdirs);
        if m4bs.is_empty() {
            continue;
        }
        m4bs.sort();
        for m4b in &m4bs {
            let exists = sqlx::query("SELECT 1 FROM audiobook_chapters WHERE file_path = ?")
                .bind(m4b)
                .fetch_optional(db)
                .await
                .ok()
                .flatten()
                .is_some();
            if exists {
                continue;
            }
            let meta = crate::music::read_audio_meta(m4b, false);
            let stem = m4b.rsplit('/').next().unwrap_or("");
            let book_title = if meta.title.is_empty() || meta.title == stem {
                stem.rsplit_once('.').map(|(s, _)| s.to_string()).unwrap_or_default()
            } else {
                meta.title.clone()
            };
            let Some((author_name, _)) =
                ab_author_title(&root, &dir, meta.artist.as_deref())
            else {
                continue;
            };
            let author_id = ab_get_or_create_author(db, &author_name).await;
            let topic = find_topic_for_path(&dir, &topic_lookup);
            let book_id = ab_get_or_create_book(db, author_id, &book_title, &dir, m4b, topic).await;
            let _ = sqlx::query(
                "INSERT INTO audiobook_chapters (title, file_path, track_number, size, book_id) VALUES (?,?,?,?,?)",
            )
            .bind(&book_title)
            .bind(m4b)
            .bind("1")
            .bind(meta.size_mb)
            .bind(book_id)
            .execute(db)
            .await;
        }
    }

    // refresh topic book counts
    let _ = sqlx::query(
        "UPDATE audiobook_topics SET book_count = (SELECT COUNT(*) FROM audiobook_books b WHERE b.topic_id = audiobook_topics.id)",
    )
    .execute(db)
    .await;
    println!("[library] audiobook scan complete");
}

pub async fn clean_audiobook_database(db: &syarc_plugin_api::Db) {
    let books: Vec<(i64, Option<String>)> = sqlx::query(
        "SELECT id, image_filename FROM audiobook_books",
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| (r.get::<i64,_>("id"), r.try_get::<Option<String>,_>("image_filename").ok().flatten()))
    .collect();

    let chapters: Vec<(i64, i64, String)> = sqlx::query(
        "SELECT c.id, c.book_id, c.file_path FROM audiobook_chapters c",
    )
    .fetch_all(db)
    .await
    .unwrap_or_default()
    .iter()
    .map(|r| (r.get::<i64,_>("id"), r.get::<i64,_>("book_id"), r.get::<String,_>("file_path")))
    .collect();

    let mut bad_books: Vec<i64> = Vec::new();
    for (book_id, _, _) in &chapters {
        // handled below
        let _ = book_id;
    }
    let mut missing: Vec<i64> = Vec::new();
    for (cid, bid, path) in &chapters {
        if !std::path::Path::new(path).exists() {
            missing.push(*cid);
            if !bad_books.contains(bid) {
                bad_books.push(*bid);
            }
        }
    }
    for cid in &missing {
        let _ = sqlx::query("DELETE FROM audiobook_chapters WHERE id = ?")
            .bind(cid)
            .execute(db)
            .await;
    }
    for bid in &bad_books {
        if let Some(art) = books.iter().find(|(b, _)| b == bid).and_then(|(_, a)| a.clone()) {
            let _ = std::fs::remove_file(ab_art_dir().join(&art));
        }
        let _ = sqlx::query("DELETE FROM audiobook_chapters WHERE book_id = ?").bind(bid).execute(db).await;
        let _ = sqlx::query("DELETE FROM audiobook_books WHERE id = ?").bind(bid).execute(db).await;
    }
    let _ = sqlx::query(
        "DELETE FROM audiobook_authors WHERE id NOT IN (SELECT DISTINCT author_id FROM audiobook_books)",
    )
    .execute(db)
    .await;
    let _ = sqlx::query(
        "UPDATE audiobook_topics SET book_count = (SELECT COUNT(*) FROM audiobook_books b WHERE b.topic_id = audiobook_topics.id)",
    )
    .execute(db)
    .await;
    println!("[library] audiobook database clean complete");
}

pub async fn clean_music_database(db: &syarc_plugin_api::Db) {
    let tracks: Vec<(i64, String)> = sqlx::query("SELECT id, file_path FROM music_tracks")
        .fetch_all(db)
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|r| Some((r.get(0), r.try_get(1).ok()?)))
        .collect();
    for (id, path) in tracks {
        if !std::path::Path::new(&path).exists() {
            let _ = sqlx::query("DELETE FROM music_tracks WHERE id = ?").bind(id).execute(db).await;
        }
    }
    let _ = sqlx::query(
        "DELETE FROM music_albums WHERE id NOT IN (SELECT DISTINCT album_id FROM music_tracks)",
    )
    .execute(db)
    .await;
    let _ = sqlx::query(
        "DELETE FROM music_authors WHERE id NOT IN (SELECT DISTINCT author_id FROM music_albums)",
    )
    .execute(db)
    .await;
    println!("[library] music database clean complete");
}

// ------------------------------------------------------------------- movie

use crate::{new_uuid, now_str, root, static_dir};

const MOVIE_VIDEO_EXTS: [&str; 7] = ["mov", "mp4", "m4v", "avi", "mkv", "webm", "vob"];

pub(crate) fn is_video_ext(name: &str) -> bool {
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    MOVIE_VIDEO_EXTS.contains(&ext.as_str())
}

pub(crate) fn movie_ext(name: &str) -> String {
    name.rsplit('.').next().unwrap_or("").to_lowercase()
}

pub(crate) fn determine_movie_title(entry_name: &str) -> String {
    let stem = std::path::Path::new(entry_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(entry_name);
    let re_year = regex::Regex::new(r"[(\[]?\d{4}[)\]]?").unwrap();
    let re_sep = regex::Regex::new(r"[_.]+").unwrap();
    let name = re_sep.replace_all(&re_year.replace_all(stem, ""), " ").trim().to_string();
    if name.is_empty() {
        stem.to_string()
    } else {
        name
    }
}

pub(crate) fn extract_year_from_name(name: &str) -> Option<i64> {
    let re_br = regex::Regex::new(r"[\[(](\d{4})[\])]").unwrap();
    if let Some(m) = re_br.captures(name) {
        if let Ok(y) = m[1].parse::<i64>() {
            return Some(y);
        }
    }
    let re_sep = regex::Regex::new(r"[s.\-_](\d{4})[s.\-_]").unwrap();
    if let Some(m) = re_sep.captures(name) {
        if let Ok(y) = m[1].parse::<i64>() {
            if (1900..=2100).contains(&y) {
                return Some(y);
            }
        }
    }
    None
}

/// If `file_path` is a directory (DVD rip etc.), resolve to the largest
/// playable video file inside it.
pub(crate) fn resolve_video_file(file_path: &str) -> Option<String> {
    let p = std::path::Path::new(file_path);
    if p.is_file() {
        return Some(file_path.to_string());
    }
    if !p.is_dir() {
        return None;
    }
    let ts_dir = p.join("VIDEO_TS");
    if ts_dir.is_dir() {
        if let Ok(entries) = std::fs::read_dir(&ts_dir) {
            let mut names: Vec<String> = entries
                .filter_map(|e| e.ok().and_then(|e| e.file_name().to_str().map(|s| s.to_string())))
                .collect();
            names.sort();
            for f in names {
                let upper = f.to_uppercase();
                if upper.ends_with(".VOB") && upper.starts_with("VTS_") && !upper.contains("_0.VOB") {
                    return Some(ts_dir.join(f).to_string_lossy().into_owned());
                }
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(p) {
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok().and_then(|e| e.file_name().to_str().map(|s| s.to_string())))
            .collect();
        names.sort();
        for f in names {
            if is_video_ext(&f) {
                return Some(p.join(f).to_string_lossy().into_owned());
            }
        }
    }
    None
}

pub(crate) fn determine_movie_file_type(file_path: &str) -> String {
    let p = std::path::Path::new(file_path);
    if p.is_dir() {
        if p.join("VIDEO_TS").is_dir() {
            return "dvd".into();
        }
        if let Ok(entries) = std::fs::read_dir(p) {
            let mut names: Vec<String> = entries
                .filter_map(|e| e.ok().and_then(|e| e.file_name().to_str().map(|s| s.to_string())))
                .collect();
            names.sort();
            for f in names {
                let ext = movie_ext(&f);
                if MOVIE_VIDEO_EXTS.contains(&ext.as_str()) {
                    return ext;
                }
            }
        }
    } else {
        let ext = file_path.rsplit('.').next().unwrap_or("").to_lowercase();
        if MOVIE_VIDEO_EXTS.contains(&ext.as_str()) {
            return ext;
        }
    }
    "mp4".into()
}

pub(crate) fn sidecar_poster_path(video_path: &str) -> String {
    let p = std::path::Path::new(video_path);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or(video_path);
    match p.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => {
            dir.join(format!("{stem}.jpg")).to_string_lossy().into_owned()
        }
        _ => format!("{stem}.jpg"),
    }
}

pub(crate) fn movie_posters_dir() -> std::path::PathBuf {
    let d = static_dir("movie").join("posters");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn ffmpeg_extract(resolved: &str, skip_seconds: u32, tmp_path: String) -> bool {
    std::process::Command::new("ffmpeg")
        .args([
            "-y", "-nostdin", "-ss", &skip_seconds.to_string(),
            "-i", resolved,
            "-vframes", "1", "-q:v", "2",
            "-vf", "scale=400:-1",
            &tmp_path,
        ])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Extract a poster frame for a movie. The poster lives next to the video
/// file (`<stem>.jpg`); the return value is the movies.poster_filename
/// value: "sibling" normally, or a legacy posters-dir filename when the
/// library tree is unwritable.
pub(crate) const POSTER_SIBLING: &str = "sibling";

pub(crate) async fn extract_first_frame(
    video_path: &str,
    movie_id: &str,
    skip_seconds: Option<u32>,
) -> Option<String> {
    let resolved = resolve_video_file(video_path)?;
    if !std::path::Path::new(&resolved).is_file() {
        println!("[library/movie] poster: video file not found: {resolved}");
        return None;
    }

    let sidecar = sidecar_poster_path(&resolved);
    let sp = std::path::Path::new(&sidecar);
    if sp.is_file() && sp.metadata().map(|m| m.len() > 500).unwrap_or(false) {
        return Some(POSTER_SIBLING.to_string());
    }

    let skip = skip_seconds.unwrap_or(3);
    let tmp_path = format!("{sidecar}.tmp.jpg");
    let resolved_clone = resolved.clone();
    let tmp_clone = tmp_path.clone();
    let ok = tokio::task::spawn_blocking(move || ffmpeg_extract(&resolved_clone, skip, tmp_clone))
        .await
        .ok()
        .unwrap_or(false);
    if !ok {
        let _ = std::fs::remove_file(&tmp_path);
        println!("[library/movie] poster: ffmpeg failed for {resolved}");
        return None;
    }
    let tp = std::path::Path::new(&tmp_path);
    if !tp.is_file() || tp.metadata().map(|m| m.len() <= 500).unwrap_or(true) {
        let _ = std::fs::remove_file(&tmp_path);
        return None;
    }
    if std::fs::rename(&tmp_path, &sidecar).is_ok() {
        return Some(POSTER_SIBLING.to_string());
    }
    // Library tree unwritable: fall back to the legacy posters dir.
    let save_path = movie_posters_dir().join(format!("poster_{movie_id}.jpg"));
    let _ = std::fs::rename(&tmp_path, &save_path);
    Some(format!("poster_{movie_id}.jpg"))
}

pub(crate) fn dir_size(path: &str) -> Option<i64> {
    let p = std::path::Path::new(path);
    if p.is_dir() {
        let mut total: i64 = 0;
        fn walk(dir: &std::path::Path, total: &mut i64) {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for e in entries.flatten() {
                    let ep = e.path();
                    if ep.is_file() {
                        if let Ok(md) = ep.metadata() {
                            *total += md.len() as i64;
                        }
                    } else if ep.is_dir() {
                        walk(&ep, total);
                    }
                }
            }
        }
        walk(p, &mut total);
        Some(total)
    } else {
        p.metadata().ok().map(|m| m.len() as i64)
    }
}

async fn user_id_by_name(db: &syarc_plugin_api::Db, username: &str) -> Option<i64> {
    sqlx::query("SELECT id FROM users WHERE username = ?")
        .bind(username)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|r| r.get::<i64, _>("id"))
}

async fn get_or_create_movie_category(
    db: &syarc_plugin_api::Db,
    user_id: i64,
    name: &str,
    dir_path: &str,
    parent_id: Option<&str>,
) -> String {
    let norm = name.trim().to_lowercase();
    let cond = match parent_id {
        Some(_) => "parent_id = ?3",
        None => "parent_id IS NULL",
    };
    let sql = format!(
        "SELECT id FROM movie_category WHERE user_id = ?1 AND normalized_name = ?2 AND {cond}"
    );
    let q = sqlx::query(&sql).bind(user_id).bind(&norm);
    let q = match parent_id {
        Some(pid) => q.bind(pid),
        None => q,
    };
    if let Ok(Some(r)) = q.fetch_optional(db).await {
        return r.get::<String, _>("id");
    }
    let id = new_uuid();
    let _ = sqlx::query(
        "INSERT INTO movie_category (id, user_id, name, normalized_name, is_public, directory_path, parent_id, created_at, updated_at) \
         VALUES (?, ?, ?, ?, 0, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(user_id)
    .bind(name)
    .bind(&norm)
    .bind(dir_path)
    .bind(parent_id)
    .bind(now_str())
    .bind(now_str())
    .execute(db)
    .await;
    id
}

fn dir_has_video(p: &std::path::Path, deep: bool) -> bool {
    if p.join("VIDEO_TS").is_dir() {
        return true;
    }
    if let Ok(entries) = std::fs::read_dir(p) {
        for e in entries.flatten() {
            let ep = e.path();
            if ep.is_file() && is_video_ext(&e.file_name().to_string_lossy()) {
                return true;
            }
            if deep && ep.is_dir() && ep.join("VIDEO_TS").is_dir() {
                return true;
            }
        }
    }
    false
}

fn create_movie_subcategories<'a>(
    db: &'a syarc_plugin_api::Db,
    user_id: i64,
    dir_path: &'a str,
    parent_id: &'a str,
) -> std::pin::Pin<std::boxed::Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
    let entries = match std::fs::read_dir(dir_path) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok().and_then(|e| e.file_name().to_str().map(|s| s.to_string())))
        .collect();
    names.sort();
    for entry in names {
        if entry.starts_with('.') || entry.to_uppercase() == "VIDEO_TS" {
            continue;
        }
        let entry_path = std::path::Path::new(dir_path).join(&entry);
        if !entry_path.is_dir() || dir_has_video(&entry_path, true) {
            continue;
        }
        let sub_id = get_or_create_movie_category(
            db,
            user_id,
            &entry,
            &entry_path.to_string_lossy(),
            Some(parent_id),
        )
        .await;
        create_movie_subcategories(db, user_id, &entry_path.to_string_lossy(), &sub_id).await;
    }
    })
}

fn discover_movie_items(dir_path: &str) -> Vec<String> {
    let mut items = Vec::new();
    let entries = match std::fs::read_dir(dir_path) {
        Ok(e) => e,
        Err(_) => return items,
    };
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok().and_then(|e| e.file_name().to_str().map(|s| s.to_string())))
        .collect();
    names.sort();
    for entry_name in names {
        if entry_name.starts_with("._") {
            continue;
        }
        let entry_path = std::path::Path::new(dir_path).join(&entry_name);
        if (entry_path.is_file() && is_video_ext(&entry_name))
            || (entry_path.is_dir() && dir_has_video(&entry_path, false))
        {
            items.push(entry_path.to_string_lossy().into_owned());
        }
    }
    items
}

async fn insert_movie(
    db: &syarc_plugin_api::Db,
    category_id: &str,
    entry_path: &str,
    is_public: bool,
) -> String {
    let entry_name = std::path::Path::new(entry_path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let title = determine_movie_title(&entry_name);
    let id = new_uuid();
    let _ = sqlx::query(
        "INSERT INTO movies (id, category_id, title, normalized_title, file_type, file_path, file_size, year, is_public, is_banned, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?)",
    )
    .bind(&id)
    .bind(category_id)
    .bind(&title)
    .bind(title.to_lowercase().trim())
    .bind(determine_movie_file_type(entry_path))
    .bind(entry_path)
    .bind(dir_size(entry_path))
    .bind(extract_year_from_name(&entry_name))
    .bind(is_public as i64)
    .bind(now_str())
    .bind(now_str())
    .execute(db)
    .await;
    id
}

pub async fn scan_movies(
    db: &syarc_plugin_api::Db,
    root_path: Option<String>,
    scan_user: Option<String>,
    force: bool,
) {
    let root_path = root_path.unwrap_or_else(|| root("movie"));
    println!(
        "[library/movie] scan started from {root_path} (user={})",
        scan_user.as_deref().unwrap_or("all")
    );
    if !std::path::Path::new(&root_path).is_dir() {
        println!("[library/movie] root does not exist: {root_path}");
        return;
    }

    if force {
        let posters = movie_posters_dir();
        if let Ok(entries) = std::fs::read_dir(&posters) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if matches!(
                    name.rsplit('.').next().unwrap_or(""),
                    "jpg" | "jpeg" | "png" | "gif" | "webp"
                ) {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        let _ = sqlx::query("DELETE FROM movies").execute(db).await;
        let _ = sqlx::query("DELETE FROM movie_category_shares").execute(db).await;
        let _ = sqlx::query("DELETE FROM movie_category").execute(db).await;
    }

    let mut processed: i64 = 0;
    let entries = match std::fs::read_dir(&root_path) {
        Ok(e) => e,
        Err(_) => return,
    };
    let mut usernames: Vec<String> = entries
        .filter_map(|e| e.ok().and_then(|e| e.file_name().to_str().map(|s| s.to_string())))
        .collect();
    usernames.sort();

    for username in usernames {
        if let Some(su) = &scan_user {
            if &username != su {
                continue;
            }
        }
        if username == "public" {
            continue;
        }
        let user_path = std::path::Path::new(&root_path).join(&username);
        if !user_path.is_dir() {
            continue;
        }
        let Some(db_user_id) = user_id_by_name(db, &username).await else {
            println!("[library/movie] user '{username}' not found, skipping");
            continue;
        };

        let cats = match std::fs::read_dir(&user_path) {
            Ok(e) => e,
            Err(_) => continue,
        };
        let mut cat_names: Vec<String> = cats
            .filter_map(|e| e.ok().and_then(|e| e.file_name().to_str().map(|s| s.to_string())))
            .collect();
        cat_names.sort();

        for category_name in cat_names {
            let category_path = user_path.join(&category_name);
            if !category_path.is_dir() {
                continue;
            }
            let category_path_s = category_path.to_string_lossy().into_owned();
            let category_id = get_or_create_movie_category(
                db,
                db_user_id,
                &category_name,
                &category_path_s,
                None,
            )
            .await;
            create_movie_subcategories(db, db_user_id, &category_path_s, &category_id).await;

            let is_public = sqlx::query("SELECT is_public FROM movie_category WHERE id = ?")
                .bind(&category_id)
                .fetch_one(db)
                .await
                .map(|r| r.get::<i64, _>("is_public") == 1)
                .unwrap_or(false);

            for entry_path in discover_movie_items(&category_path_s) {
                let exists = sqlx::query("SELECT 1 FROM movies WHERE file_path = ?")
                    .bind(&entry_path)
                    .fetch_optional(db)
                    .await
                    .ok()
                    .flatten()
                    .is_some();
                if exists && !force {
                    continue;
                }
                let movie_id = insert_movie(db, &category_id, &entry_path, is_public).await;
                if let Some(poster) = extract_first_frame(&entry_path, &movie_id, None).await {
                    let _ = sqlx::query("UPDATE movies SET poster_filename = ? WHERE id = ?")
                        .bind(poster)
                        .bind(&movie_id)
                        .execute(db)
                        .await;
                }
                processed += 1;
            }

            let count = sqlx::query("SELECT COUNT(*) AS c FROM movies WHERE category_id = ?")
                .bind(&category_id)
                .fetch_one(db)
                .await
                .map(|r| r.get::<i64, _>("c"))
                .unwrap_or(0);
            let _ = sqlx::query("UPDATE movie_category SET movie_count = ? WHERE id = ?")
                .bind(count)
                .bind(&category_id)
                .execute(db)
                .await;
        }
    }
    println!("[library/movie] scan complete. Processed {processed} movies.");
}

// ==================================================================== ebook

use crate::ebook::{
    bind_categories, covers_dir, extract_cover, extract_metadata, insert_book,
    refresh_topic_count, EbookMeta, EBOOK_EXTENSIONS,
};

async fn truncate_ebook_tables(db: &syarc_plugin_api::Db) {
    let covers = covers_dir();
    if let Ok(entries) = std::fs::read_dir(&covers) {
        for e in entries.filter_map(|e| e.ok()) {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".jpg")
                || name.ends_with(".jpeg")
                || name.ends_with(".png")
                || name.ends_with(".gif")
                || name.ends_with(".webp")
            {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    for sql in [
        "DELETE FROM ebook_book_categories",
        "DELETE FROM ebook_reading_list_items WHERE item_type = 'book'",
        "DELETE FROM ebook_books",
        "DELETE FROM ebook_topic_shares",
        "DELETE FROM ebook_topics",
        "DELETE FROM ebook_authors",
    ] {
        let _ = sqlx::query(sql).execute(db).await;
    }
    let _ = sqlx::query("DELETE FROM sqlite_sequence WHERE name IN ('ebook_books','ebook_authors')")
        .execute(db)
        .await;
}

struct TopicRow {
    id: String,
    user_id: i64,
    directory_path: String,
    is_public: bool,
}

/// Recursively create sub-topics mirroring the directory tree (boxed recursion).
fn create_subtopics_recursive<'a>(
    db: &'a syarc_plugin_api::Db,
    user_id: i64,
    dir_path: std::path::PathBuf,
    parent_id: String,
    existing_by_path: &'a mut std::collections::HashMap<String, String>,
    existing_names: &'a mut std::collections::HashSet<(i64, String)>,
    created: &'a mut i64,
) -> std::pin::Pin<std::boxed::Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
    Box::pin(async move {
        let Ok(entries) = std::fs::read_dir(&dir_path) else {
            return;
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok().and_then(|e| e.file_name().to_str().map(|s| s.to_string())))
            .collect();
        names.sort();
        for entry in names {
            if entry.starts_with('.') {
                continue;
            }
            let entry_path = dir_path.join(&entry);
            if !entry_path.is_dir() {
                continue;
            }
            let norm_path = entry_path.to_string_lossy().into_owned();
            let norm_name = entry.to_lowercase().trim().to_string();

            let sub_parent = if let Some(id) = existing_by_path.get(&norm_path) {
                id.clone()
            } else {
                if existing_names.contains(&(user_id, norm_name.clone())) {
                    continue;
                }
                let id = crate::new_uuid();
                let _ = sqlx::query(
                    "INSERT INTO ebook_topics (id, user_id, name, normalized_name, directory_path, \
                     is_public, parent_id, book_count, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, ?, 0, ?, 0, ?, ?)",
                )
                .bind(&id)
                .bind(user_id)
                .bind(&entry)
                .bind(&norm_name)
                .bind(&norm_path)
                .bind(&parent_id)
                .bind(crate::now_str())
                .bind(crate::now_str())
                .execute(db)
                .await;
                existing_by_path.insert(norm_path.clone(), id.clone());
                existing_names.insert((user_id, norm_name));
                *created += 1;
                id
            };
            create_subtopics_recursive(
                db,
                user_id,
                entry_path,
                sub_parent,
                existing_by_path,
                existing_names,
                created,
            )
            .await;
        }
    })
}

pub async fn scan_ebooks(db: &syarc_plugin_api::Db, root_path: Option<String>, force: bool) {
    let root_path = root_path.unwrap_or_else(|| crate::root("ebook"));
    println!("[library/ebook] scan starting at {root_path}");
    let _ = std::fs::create_dir_all(covers_dir());

    if force {
        truncate_ebook_tables(db).await;
    }

    // Map usernames to user ids.
    let users: Vec<(i64, String)> = sqlx::query("SELECT id, username FROM users")
        .fetch_all(db)
        .await
        .map(|rows| {
            rows.iter()
                .map(|r| (r.get::<i64,_>("id"), r.try_get::<String,_>("username").unwrap_or_default()))
                .collect()
        })
        .unwrap_or_default();
    let username_to_id: std::collections::HashMap<String, i64> =
        users.iter().map(|(id, un)| (un.clone(), *id)).collect();

    let norm_root = std::fs::canonicalize(&root_path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| root_path.clone());

    // Which user directories to scan.
    let mut user_dirs: Vec<(i64, String)> = Vec::new();
    if let Some((uid, uname)) = users.iter().find(|(_, u)| norm_root.ends_with(u)) {
        user_dirs.push((*uid, root_path.clone()));
        let _ = uname;
    } else if std::path::Path::new(&root_path).is_dir() {
        let Ok(entries) = std::fs::read_dir(&root_path) else {
            println!("[library/ebook] cannot read {root_path}");
            return;
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok().and_then(|e| e.file_name().to_str().map(|s| s.to_string())))
            .collect();
        names.sort();
        for name in names {
            if matches!(name.as_str(), "public" | ".thumbnails" | ".covers") || name.starts_with('.') {
                continue;
            }
            let user_path = std::path::Path::new(&root_path).join(&name);
            if !user_path.is_dir() {
                continue;
            }
            if let Some(uid) = username_to_id.get(&name) {
                user_dirs.push((*uid, user_path.to_string_lossy().into_owned()));
            }
        }
    }

    // Load existing topics.
    async fn load_topics(db: &syarc_plugin_api::Db) -> Vec<TopicRow> {
        sqlx::query(
            "SELECT id, user_id, directory_path, normalized_name, is_public FROM ebook_topics",
        )
        .fetch_all(db)
        .await
        .map(|rows| {
            rows.iter()
                .map(|r| TopicRow {
                    id: r.get::<String,_>("id"),
                    user_id: r.get::<i64,_>("user_id"),
                    directory_path: r.try_get::<String,_>("directory_path").unwrap_or_default(),
                    is_public: r.get::<i64,_>("is_public") == 1,
                })
                .collect()
        })
        .unwrap_or_default()
    }

    let mut existing_by_path: std::collections::HashMap<String, String> = Default::default();
    let mut existing_names: std::collections::HashSet<(i64, String)> = Default::default();
    for t in load_topics(db).await {
        existing_by_path.insert(t.directory_path.clone(), t.id.clone());
    }
    existing_names.clear();
    if let Ok(rows) = sqlx::query(
        "SELECT user_id, normalized_name FROM ebook_topics",
    )
    .fetch_all(db)
    .await
    {
        for r in rows {
            existing_names.insert((
                r.get::<i64,_>("user_id"),
                r.try_get::<String,_>("normalized_name").unwrap_or_default(),
            ));
        }
    }

    // Auto-create topics mirroring directories.
    let mut created: i64 = 0;
    for (db_user_id, user_path) in &user_dirs {
        let Ok(entries) = std::fs::read_dir(user_path) else {
            continue;
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok().and_then(|e| e.file_name().to_str().map(|s| s.to_string())))
            .collect();
        names.sort();
        for entry in names {
            if entry.starts_with('.') {
                continue;
            }
            let entry_path = std::path::Path::new(user_path).join(&entry);
            if !entry_path.is_dir() {
                continue;
            }
            let norm_path = entry_path.to_string_lossy().into_owned();
            let norm_name = entry.to_lowercase().trim().to_string();
            let parent_id = if let Some(id) = existing_by_path.get(&norm_path) {
                id.clone()
            } else {
                if existing_names.contains(&(*db_user_id, norm_name.clone())) {
                    continue;
                }
                let id = crate::new_uuid();
                let _ = sqlx::query(
                    "INSERT INTO ebook_topics (id, user_id, name, normalized_name, directory_path, \
                     is_public, book_count, created_at, updated_at) VALUES (?, ?, ?, ?, ?, 0, 0, ?, ?)",
                )
                .bind(&id)
                .bind(*db_user_id)
                .bind(&entry)
                .bind(&norm_name)
                .bind(&norm_path)
                .bind(crate::now_str())
                .bind(crate::now_str())
                .execute(db)
                .await;
                existing_by_path.insert(norm_path.clone(), id.clone());
                existing_names.insert((*db_user_id, norm_name));
                created += 1;
                id
            };
            create_subtopics_recursive(
                db,
                *db_user_id,
                entry_path,
                parent_id,
                &mut existing_by_path,
                &mut existing_names,
                &mut created,
            )
            .await;
        }
    }
    if created > 0 {
        println!("[library/ebook] auto-created {created} topics from directory structure.");
    }

    // Reload topics sorted by longest directory_path first.
    let mut topic_dirs: Vec<TopicRow> = load_topics(db).await;
    topic_dirs.sort_by_key(|a| std::cmp::Reverse(a.directory_path.len()));
    let resolve_topic = |filepath: &str| -> Option<&TopicRow> {
        let norm = std::path::Path::new(filepath);
        topic_dirs.iter().find(|t| {
            let tp = std::path::Path::new(&t.directory_path);
            norm.starts_with(tp) && norm.parent() != Some(std::path::Path::new(""))
        })
    };

    // Walk the tree.
    let mut stack = vec![std::path::PathBuf::from(&root_path)];
    let mut processed: i64 = 0;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut paths: Vec<std::path::PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
        paths.sort();
        for path in paths {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if path.is_dir() {
                if !name.starts_with('.') {
                    stack.push(path);
                }
                continue;
            }
            if name.starts_with("._") || name.starts_with('.') {
                continue;
            }
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if !EBOOK_EXTENSIONS.contains(&ext.as_str()) {
                continue;
            }
            let full_path = path.to_string_lossy().into_owned();

            let exists = sqlx::query("SELECT 1 FROM ebook_books WHERE file_path = ?")
                .bind(&full_path)
                .fetch_optional(db)
                .await
                .ok()
                .flatten()
                .is_some();
            if exists && !force {
                continue;
            }

            let file_type = if ext == "epub" { "epub" } else { "pdf" };
            let meta: EbookMeta = extract_metadata(full_path.clone(), file_type.to_string()).await;
            if meta.title.is_empty() {
                continue;
            }
            let author_id = crate::ebook::get_or_create_author(db, &meta.author).await;

            // Dedupe by (normalized_title, author) with a different path.
            let norm_title = meta.title.to_lowercase().trim().to_string();
            let dup: Option<String> = sqlx::query(
                "SELECT file_path FROM ebook_books WHERE normalized_title = ? AND author_id = ? LIMIT 1",
            )
            .bind(&norm_title)
            .bind(author_id)
            .fetch_optional(db)
            .await
            .ok()
            .flatten()
            .and_then(|r| r.try_get::<String,_>("file_path").ok());
            if let Some(dp) = dup {
                if dp != full_path && !force {
                    continue;
                }
            }

            let resolved = resolve_topic(&full_path);
            let book_id = insert_book(
                db,
                &meta,
                author_id,
                file_type,
                &full_path,
                resolved.map(|t| t.id.as_str()),
                resolved.map(|t| t.user_id),
                resolved.map(|t| t.is_public).unwrap_or(false),
            )
            .await;

            if let Some(cover) = extract_cover(full_path.clone(), book_id, file_type.to_string()).await {
                let _ = sqlx::query("UPDATE ebook_books SET cover_filename = ? WHERE id = ?")
                    .bind(&cover)
                    .bind(book_id)
                    .execute(db)
                    .await;
            }
            if !meta.subjects.is_empty() {
                bind_categories(db, book_id, &meta.subjects).await;
            }
            processed += 1;
        }
    }

    // Refresh book counts.
    if let Ok(rows) = sqlx::query("SELECT DISTINCT topic_id FROM ebook_books WHERE topic_id IS NOT NULL")
        .fetch_all(db)
        .await
    {
        for r in rows {
            if let Ok(tid) = r.try_get::<String,_>("topic_id") {
                refresh_topic_count(db, &tid).await;
            }
        }
    }
    println!("[library/ebook] scan complete. Processed {processed} books.");
}
