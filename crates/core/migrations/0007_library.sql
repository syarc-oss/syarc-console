-- Library app (music, audiobook, ebooks, movies, photos)
-- UUID PKs are kept as TEXT since they are embedded in URLs and file references.

CREATE TABLE IF NOT EXISTS library_instances (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    library_type TEXT NOT NULL CHECK (library_type IN ('audio','digital')),
    visibility TEXT NOT NULL DEFAULT 'public' CHECK (visibility IN ('public','private')),
    owner_id INTEGER REFERENCES users(id),
    root_path TEXT NOT NULL,
    static_dir TEXT,
    is_active INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS library_access (
    id TEXT PRIMARY KEY,
    library_id TEXT NOT NULL REFERENCES library_instances(id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_library_access_user ON library_access(user_id);

-- music
CREATE TABLE IF NOT EXISTS music_authors (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    normalized_name TEXT NOT NULL,
    library_id TEXT REFERENCES library_instances(id)
);
CREATE INDEX IF NOT EXISTS idx_music_authors_name ON music_authors(name);

CREATE TABLE IF NOT EXISTS music_albums (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    author_id INTEGER NOT NULL REFERENCES music_authors(id),
    media_type TEXT NOT NULL DEFAULT 'music',
    image_filename TEXT,
    updated_at TEXT,
    library_id TEXT REFERENCES library_instances(id),
    user_id INTEGER REFERENCES users(id),
    is_public INTEGER NOT NULL DEFAULT 0,
    is_banned INTEGER NOT NULL DEFAULT 0,
    directory_path TEXT
);
CREATE INDEX IF NOT EXISTS idx_music_albums_author ON music_albums(author_id);

CREATE TABLE IF NOT EXISTS music_album_shares (
    id INTEGER PRIMARY KEY,
    album_id INTEGER NOT NULL REFERENCES music_albums(id),
    shared_with_user_id INTEGER NOT NULL REFERENCES users(id),
    created_at TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_music_album_share ON music_album_shares(album_id, shared_with_user_id);

CREATE TABLE IF NOT EXISTS music_tracks (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    file_path TEXT NOT NULL,
    track_number TEXT,
    size REAL,
    album_id INTEGER NOT NULL REFERENCES music_albums(id),
    user_id INTEGER REFERENCES users(id),
    is_public INTEGER NOT NULL DEFAULT 0,
    file_size REAL
);
CREATE INDEX IF NOT EXISTS idx_music_tracks_album ON music_tracks(album_id);

CREATE TABLE IF NOT EXISTS music_playlists (
    name TEXT NOT NULL,
    user_id INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (name, user_id)
);

CREATE TABLE IF NOT EXISTS music_playlist_items (
    id INTEGER PRIMARY KEY,
    playlist_name TEXT NOT NULL,
    user_id INTEGER NOT NULL,
    item_type TEXT NOT NULL,
    item_id INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_music_playlist_item ON music_playlist_items(user_id, playlist_name, item_type, item_id);

-- audiobook
CREATE TABLE IF NOT EXISTS audiobook_authors (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    normalized_name TEXT NOT NULL,
    library_id TEXT REFERENCES library_instances(id)
);

CREATE TABLE IF NOT EXISTS audiobook_topics (
    id TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id),
    name TEXT NOT NULL,
    normalized_name TEXT NOT NULL,
    description TEXT,
    is_public INTEGER NOT NULL DEFAULT 0,
    directory_path TEXT NOT NULL,
    book_count INTEGER NOT NULL DEFAULT 0,
    parent_id TEXT REFERENCES audiobook_topics(id),
    created_at TEXT,
    updated_at TEXT
);

CREATE TABLE IF NOT EXISTS audiobook_topic_shares (
    id INTEGER PRIMARY KEY,
    topic_id TEXT NOT NULL REFERENCES audiobook_topics(id),
    shared_with_user_id INTEGER NOT NULL REFERENCES users(id),
    created_at TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_audiobook_topic_share ON audiobook_topic_shares(topic_id, shared_with_user_id);

CREATE TABLE IF NOT EXISTS audiobook_books (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    normalized_title TEXT NOT NULL DEFAULT '',
    author_id INTEGER NOT NULL REFERENCES audiobook_authors(id),
    image_filename TEXT,
    updated_at TEXT,
    library_id TEXT REFERENCES library_instances(id),
    topic_id TEXT REFERENCES audiobook_topics(id),
    user_id INTEGER REFERENCES users(id),
    is_public INTEGER NOT NULL DEFAULT 0,
    is_banned INTEGER NOT NULL DEFAULT 0,
    description TEXT,
    directory_path TEXT
);

CREATE TABLE IF NOT EXISTS audiobook_chapters (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    file_path TEXT NOT NULL,
    track_number TEXT,
    size REAL,
    book_id INTEGER NOT NULL REFERENCES audiobook_books(id)
);
CREATE INDEX IF NOT EXISTS idx_audiobook_chapters_book ON audiobook_chapters(book_id);

CREATE TABLE IF NOT EXISTS audiobook_playlists (
    name TEXT NOT NULL,
    user_id INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (name, user_id)
);

CREATE TABLE IF NOT EXISTS audiobook_playlist_items (
    id INTEGER PRIMARY KEY,
    playlist_name TEXT NOT NULL,
    user_id INTEGER NOT NULL,
    item_type TEXT NOT NULL,
    item_id INTEGER NOT NULL
);

-- ebook
CREATE TABLE IF NOT EXISTS ebook_authors (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    normalized_name TEXT NOT NULL,
    library_id TEXT REFERENCES library_instances(id)
);

CREATE TABLE IF NOT EXISTS ebook_categories (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    normalized_name TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS ebook_topics (
    id TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id),
    name TEXT NOT NULL,
    normalized_name TEXT NOT NULL,
    description TEXT,
    is_public INTEGER NOT NULL DEFAULT 0,
    directory_path TEXT NOT NULL,
    book_count INTEGER NOT NULL DEFAULT 0,
    parent_id TEXT REFERENCES ebook_topics(id),
    created_at TEXT,
    updated_at TEXT
);

CREATE TABLE IF NOT EXISTS ebook_topic_shares (
    id INTEGER PRIMARY KEY,
    topic_id TEXT NOT NULL REFERENCES ebook_topics(id),
    shared_with_user_id INTEGER NOT NULL REFERENCES users(id),
    created_at TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_ebook_topic_share ON ebook_topic_shares(topic_id, shared_with_user_id);

CREATE TABLE IF NOT EXISTS ebook_books (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    normalized_title TEXT NOT NULL,
    isbn TEXT,
    isbn13 TEXT,
    author_id INTEGER NOT NULL REFERENCES ebook_authors(id),
    topic_id TEXT REFERENCES ebook_topics(id),
    user_id INTEGER REFERENCES users(id),
    file_type TEXT NOT NULL DEFAULT 'epub',
    file_path TEXT NOT NULL,
    file_size INTEGER,
    cover_filename TEXT,
    publisher TEXT,
    published_date TEXT,
    description TEXT,
    language TEXT DEFAULT 'en',
    page_count INTEGER,
    metadata_fetched INTEGER NOT NULL DEFAULT 0,
    is_public INTEGER NOT NULL DEFAULT 0,
    is_banned INTEGER NOT NULL DEFAULT 0,
    library_id TEXT REFERENCES library_instances(id),
    created_at TEXT,
    updated_at TEXT
);

CREATE TABLE IF NOT EXISTS ebook_book_categories (
    book_id INTEGER NOT NULL REFERENCES ebook_books(id),
    category_id INTEGER NOT NULL REFERENCES ebook_categories(id),
    PRIMARY KEY (book_id, category_id)
);

CREATE TABLE IF NOT EXISTS ebook_reading_lists (
    name TEXT NOT NULL,
    user_id INTEGER NOT NULL,
    created_at TEXT,
    PRIMARY KEY (name, user_id)
);

CREATE TABLE IF NOT EXISTS ebook_reading_list_items (
    id INTEGER PRIMARY KEY,
    reading_list_name TEXT NOT NULL,
    user_id INTEGER NOT NULL,
    item_type TEXT NOT NULL,
    item_id INTEGER NOT NULL
);

-- movie
CREATE TABLE IF NOT EXISTS movie_category (
    id TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id),
    name TEXT NOT NULL,
    normalized_name TEXT NOT NULL,
    description TEXT,
    is_public INTEGER NOT NULL DEFAULT 0,
    directory_path TEXT NOT NULL,
    movie_count INTEGER NOT NULL DEFAULT 0,
    parent_id TEXT REFERENCES movie_category(id),
    created_at TEXT,
    updated_at TEXT
);

CREATE TABLE IF NOT EXISTS movie_category_shares (
    id INTEGER PRIMARY KEY,
    category_id TEXT NOT NULL REFERENCES movie_category(id),
    shared_with_user_id INTEGER NOT NULL REFERENCES users(id),
    created_at TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_movie_category_share ON movie_category_shares(category_id, shared_with_user_id);

CREATE TABLE IF NOT EXISTS movies (
    id TEXT PRIMARY KEY,
    category_id TEXT NOT NULL REFERENCES movie_category(id),
    title TEXT NOT NULL,
    normalized_title TEXT NOT NULL,
    description TEXT,
    year INTEGER,
    file_type TEXT NOT NULL DEFAULT 'mp4',
    file_path TEXT NOT NULL,
    file_size INTEGER,
    poster_filename TEXT,
    is_public INTEGER NOT NULL DEFAULT 0,
    is_banned INTEGER NOT NULL DEFAULT 0,
    created_at TEXT,
    updated_at TEXT
);

-- photo
CREATE TABLE IF NOT EXISTS photo_albums (
    id TEXT PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id),
    name TEXT NOT NULL,
    normalized_name TEXT NOT NULL,
    description TEXT,
    is_public INTEGER NOT NULL DEFAULT 0,
    directory_path TEXT NOT NULL,
    cover_photo_id TEXT,
    photo_count INTEGER NOT NULL DEFAULT 0,
    parent_id TEXT REFERENCES photo_albums(id),
    created_at TEXT,
    updated_at TEXT
);

CREATE TABLE IF NOT EXISTS photo_album_shares (
    id INTEGER PRIMARY KEY,
    album_id TEXT NOT NULL REFERENCES photo_albums(id),
    shared_with_user_id INTEGER NOT NULL REFERENCES users(id),
    created_at TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_photo_album_share ON photo_album_shares(album_id, shared_with_user_id);

CREATE TABLE IF NOT EXISTS photos (
    id TEXT PRIMARY KEY,
    album_id TEXT NOT NULL REFERENCES photo_albums(id),
    filename TEXT NOT NULL,
    file_path TEXT NOT NULL,
    file_size INTEGER NOT NULL DEFAULT 0,
    file_type TEXT NOT NULL DEFAULT 'jpg',
    width INTEGER,
    height INTEGER,
    is_public INTEGER NOT NULL DEFAULT 0,
    is_banned INTEGER NOT NULL DEFAULT 0,
    caption TEXT,
    annotation TEXT,
    taken_at TEXT,
    thumbnail_filename TEXT,
    created_at TEXT,
    updated_at TEXT
);
