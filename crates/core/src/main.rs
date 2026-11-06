mod auth;
mod registry;
mod routes;

use std::net::SocketAddr;

use axum::routing::{get, post};
use axum::Router;
use tower_sessions::{Expiry, MemoryStore, SessionManagerLayer};
use time::Duration;

use syarc_plugin_api::{AppState, SharedState};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "sqlite://syarc-console.db?mode=rwc".to_string());
    let connect_opts: sqlx::sqlite::SqliteConnectOptions = db_url
        .parse()
        .map_err(|e| format!("invalid DATABASE_URL: {e}"))?;
    let connect_opts = connect_opts
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .busy_timeout(std::time::Duration::from_secs(5));
    let db = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(8)
        .connect_with(connect_opts)
        .await?;
    sqlx::migrate!("./migrations").run(&db).await?;

    let pecuniary = pecuniary_pool(&db).await?;

    let templates = syarc_plugin_api::Templates::load()?;

    let plugins = registry::all_apps();
    for plugin in &plugins {
        if let Err(e) = plugin.on_startup(&db).await {
            eprintln!("plugin {} startup failed: {e}", plugin.manifest().name);
        }
    }
    registry::seed_apps(&db, &plugins).await?;
    registry::reconcile_apps(&db, &plugins).await?;
    registry::seed_default_permissions(&db, &plugins).await?;
    auth::ensure_first_admin_prompt(&db).await;

    let session_store = MemoryStore::default();
    let session_layer = SessionManagerLayer::new(session_store)
        .with_secure(false)
        .with_expiry(Expiry::OnInactivity(Duration::hours(12)));

    let state: SharedState = std::sync::Arc::new(AppState {
        db: db.clone(),
        pecuniary,
        templates,
    });

    let mut app = Router::new()
        .route("/", get(routes::dashboard))
        .route("/login", get(auth::login_form).post(auth::login))
        .route("/register", get(auth::register_form).post(auth::register))
        .route("/logout", post(auth::logout))
        .route("/about", get(routes::about))
        .nest_service("/static", tower_http::services::ServeDir::new("static"));

    for plugin in &plugins {
        let app_id = plugin.manifest().id;
        let st = state.clone();
        let gated = plugin.router(state.clone()).route_layer(
            axum::middleware::from_fn(move |req: axum::extract::Request, next: axum::middleware::Next| {
                let st = st.clone();
                async move { app_permission_gate(st, app_id, req, next).await }
            }),
        );
        app = app.merge(gated);
    }

    let app = app
        .layer(session_layer)
        .layer(axum::middleware::from_fn(no_store_html))
        .with_state(state);

    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8090);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("syarc-console listening on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Authenticated pages are rendered per-user from live data, and inline page
/// scripts change between releases; without explicit cache directives Safari
/// and other browsers may serve a stale cached document (observed causing a
/// stale pagination footer on the PMS listings page). Static assets under
/// /static are exempt.
async fn no_store_html(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let is_static = req.uri().path().starts_with("/static/");
    let mut res = next.run(req).await;
    if !is_static {
        res.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-store, must-revalidate"),
        );
    }
    res
}

/// Tables owned by the pecuniary app. They live in the encrypted DB; the
/// `REFERENCES users(id)` clauses are dropped because users stay in the main
/// database (SQLite cannot enforce cross-database foreign keys).
const PECUNIARY_TABLES: &[&str] = &[
    "checkbook_registers",
    "checkbook_shares",
    "checkbook",
    "ledger_books",
    "ledger_shares",
    "ledger_entries",
    "pms_portfolios",
    "pms_shares",
    "pms_transactions",
    "pms_current_values",
    "stock_portfolios",
    "stock_shares",
    "stock_transactions",
];

async fn pecuniary_pool(main_db: &sqlx::SqlitePool) -> Result<sqlx::SqlitePool, Box<dyn std::error::Error>> {
    let path = std::env::var("PECUNIARY_DB").unwrap_or_else(|_| "pecuniary.db".to_string());
    let key = std::env::var("PECUNIARY_DB_KEY").unwrap_or_default();
    if key.is_empty() {
        eprintln!("warning: PECUNIARY_DB_KEY not set; pecuniary database will be UNENCRYPTED at {path}");
    }
    let mut opts = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .busy_timeout(std::time::Duration::from_secs(5));
    if !key.is_empty() {
        // SQLCipher: set before any other access. sqlx writes pragma values
        // verbatim, so the key must be quoted as a SQL string literal itself
        // (keys can contain '/', '+', '=').
        opts = opts.pragma("key", format!("'{}'", key.replace('\'', "''")));
    }
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(opts)
        .await?;

    sqlx::raw_sql(include_str!("pecuniary_schema.sql"))
        .execute(&pool)
        .await?;

    migrate_legacy_pecuniary(main_db, &pool).await?;
    Ok(pool)
}

/// One-time move: if the pecuniary tables still exist in the main (plain)
/// database and the encrypted copies are empty, copy every row across and
/// drop the legacy tables. Runs at startup; idempotent.
async fn migrate_legacy_pecuniary(
    main_db: &sqlx::SqlitePool,
    pecuniary: &sqlx::SqlitePool,
) -> Result<(), Box<dyn std::error::Error>> {
    let existing: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type='table' AND name IN \
         ('checkbook_registers','checkbook_shares','checkbook','ledger_books','ledger_shares',\
         'ledger_entries','pms_portfolios','pms_shares','pms_transactions','pms_current_values',\
         'stock_portfolios','stock_shares','stock_transactions')",
    )
    .fetch_all(main_db)
    .await?;
    if existing.is_empty() {
        return Ok(());
    }
    let main_path = std::env::var("DATABASE_URL").unwrap_or_default();
    let main_path = main_path.trim_start_matches("sqlite://");
    let main_path = main_path.split('?').next().unwrap_or("");

    let mut conn = pecuniary.acquire().await?;
    sqlx::query("PRAGMA foreign_keys = OFF").execute(&mut *conn).await?;
    // KEY '' attaches the legacy plaintext DB without applying the cipher key
    sqlx::query(&format!("ATTACH DATABASE '{main_path}' AS legacy KEY ''")).execute(&mut *conn).await?;
    for table in PECUNIARY_TABLES {
        if !existing.iter().any(|t| t == table) {
            continue;
        }
        let moved = sqlx::query(&format!(
            "INSERT INTO main.{table} SELECT * FROM legacy.{table} \
             WHERE (SELECT COUNT(*) FROM main.{table}) = 0"
        ))
        .execute(&mut *conn)
        .await?
        .rows_affected();
        println!("pecuniary: moved {moved} rows from legacy {table}");
    }
    sqlx::query("DETACH DATABASE legacy").execute(&mut *conn).await?;
    drop(conn);

    for table in PECUNIARY_TABLES {
        sqlx::query(&format!("DROP TABLE IF EXISTS {table}"))
            .execute(main_db)
            .await?;
    }
    println!("pecuniary: dropped {} legacy tables from the main database", existing.len());
    Ok(())
}

/// logged-in user needs a user_app_permissions row for the app. Requests
/// without a session pass through so plugin endpoints that do their own auth
/// (e.g. the mesh broker WebSocket) keep working.
async fn app_permission_gate(
    state: SharedState,
    app_id: &str,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::extract::FromRequestParts;
    use axum::response::IntoResponse;
    let (mut parts, body) = req.into_parts();
    let session = tower_sessions::Session::from_request_parts(&mut parts, &())
        .await
        .ok();
    if let Some(session) = session {
        if let Ok(Some(user_id)) = session.get::<i64>(syarc_plugin_api::USER_ID_KEY).await {
            let allowed = match sqlx::query_as::<_, (String, i64)>(
                "SELECT role, is_active FROM users WHERE id = ?",
            )
            .bind(user_id)
            .fetch_optional(&state.db)
            .await
            {
                Ok(Some((role, 1))) => {
                    registry::user_can_access_app(&state.db, user_id, &role, app_id).await
                }
                _ => false,
            };
            if !allowed {
                return (
                    axum::http::StatusCode::FORBIDDEN,
                    axum::response::Html(
                        "<h1>403 Forbidden</h1><p>You do not have access to this app. \
                         Ask an administrator to grant it.</p>\
                         <p><a href=\"/\">Back to dashboard</a></p>",
                    ),
                )
                    .into_response();
            }
        }
    }
    let req = axum::extract::Request::from_parts(parts, body);
    next.run(req).await
}
