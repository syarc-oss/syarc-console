//! Calendar app: month overview aggregating tasks, chores, cooking menu,
//! foraging outings and weather; task manager with crew invites; recurring
//! chores; central invites panel (task crew + conferencing invitations).
//! Ported from the fastmain tasks + weather apps.

mod weather;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use chrono::{Datelike, Duration, NaiveDate, Weekday};
use serde_json::{json, Value};
use sqlx::Row;
use std::collections::HashMap as Map;

use syarc_plugin_api::{RequireUser, SharedState, User};

pub const TASK_STATUSES: [&str; 5] = ["draft", "pending", "in_progress", "completed", "cancelled"];
pub const FREQUENCIES: [(&str, &str); 7] = [
    ("daily", "Daily"),
    ("weekly", "Weekly"),
    ("biweekly", "Every 2 Weeks"),
    ("semimonthly", "Semi-Monthly (1st & 15th)"),
    ("monthly", "Monthly"),
    ("quarterly", "Quarterly"),
    ("yearly", "Yearly"),
];
pub const URGENCY_LABELS: [(u8, &str); 5] = [
    (1, "1 - lowest"),
    (2, "2"),
    (3, "3 - normal"),
    (4, "4"),
    (5, "5 - highest"),
];

pub const EVENT_CATEGORIES: [(&str, &str); 2] = [
    ("appointment", "Appointment"),
    ("meeting", "Meeting"),
];

pub struct CalendarPlugin;

impl CalendarPlugin {
    pub const MANIFEST: syarc_plugin_api::AppManifest =
        syarc_plugin_api::AppManifest::new("calendar", "Calendar", "/calendar")
            .icon("bi-calendar-week");
}

#[async_trait::async_trait]
impl syarc_plugin_api::AppPlugin for CalendarPlugin {
    fn manifest(&self) -> &'static syarc_plugin_api::AppManifest {
        &Self::MANIFEST
    }

    /// Grant calendar to existing active human accounts (idempotent) so the
    /// aggregated views work for everyone; new registrations do not get it
    /// by default.
    async fn on_startup(&self, db: &syarc_plugin_api::Db) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT OR IGNORE INTO user_app_permissions (user_id, app_id) \
             SELECT id, 'calendar' FROM users WHERE is_active = 1 AND username NOT LIKE 'mesh-%'",
        )
        .execute(db)
        .await?;
        Ok(())
    }

    fn router(&self, state: SharedState) -> Router<SharedState> {
        Router::new()
            .route("/calendar", get(month_view))
            .route("/calendar/", get(month_view))
            .route("/calendar/day/{date}", get(day_view))
            .route("/calendar/tasks", get(tasks_list))
            .route("/calendar/tasks/new", get(task_new_page).post(task_create))
            .route("/calendar/tasks/{task_id}/edit", get(task_edit_page).post(task_save))
            .route("/calendar/tasks/{task_id}/status", post(task_set_status))
            .route("/calendar/tasks/{task_id}/delete", post(task_delete))
            .route("/calendar/tasks/{task_id}/crew/add", post(task_crew_add))
            .route("/calendar/tasks/{task_id}/crew/{crew_id}/remove", post(task_crew_remove))
            .route("/calendar/tasks/{task_id}/crew/{crew_id}/respond", post(task_crew_respond))
            .route("/calendar/chores", get(chores_list))
            .route("/calendar/chores/new", get(chore_new_page).post(chore_create))
            .route("/calendar/chores/{chore_id}/edit", get(chore_edit_page).post(chore_save))
            .route("/calendar/chores/{chore_id}/complete", post(chore_complete))
            .route("/calendar/chores/{chore_id}/toggle", post(chore_toggle))
            .route("/calendar/chores/{chore_id}/delete", post(chore_delete))
            .route("/calendar/invites", get(invites_panel))
            .route("/calendar/invites/task/{crew_id}/{action}", post(invite_task_respond))
            .route("/calendar/invites/conferencing/{invitation_id}/{action}", post(invite_conferencing_respond))
            .route("/calendar/events", get(events_list))
            .route("/calendar/events/new", get(event_new_page).post(event_create))
            .route("/calendar/events/{event_id}/edit", get(event_edit_page).post(event_save))
            .route("/calendar/events/{event_id}/delete", post(event_delete))
            .route("/calendar/events/{event_id}/invitees/add", post(event_invitee_add))
            .route("/calendar/events/{event_id}/invitees/{invitee_id}/remove", post(event_invitee_remove))
            .route("/calendar/events/{event_id}/invitees/{invitee_id}/respond", post(event_invitee_respond))
            .route("/calendar/weather", get(weather_page))
            .route("/calendar/weather/location", post(weather_location))
            .with_state(state)
    }
}

// ------------------------------------------------------------------ helpers

fn page(state: &SharedState, template: &str, ctx: Value) -> Response {
    Html(state.templates.render(template, ctx)).into_response()
}

fn base_ctx(user: &User) -> Value {
    json!({
        "username": user.username,
        "is_admin": user.role == "admin",
    })
}

/// Merge base context keys into a page context object.
fn ctx_with_user(mut v: Value, user: &User) -> Value {
    if let (Value::Object(dst), Value::Object(src)) = (&mut v, base_ctx(user)) {
        for (k, val) in src {
            dst.insert(k, val);
        }
    }
    v
}

fn now_str() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn today_str() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

fn form_map(raw: &[u8]) -> Map<String, String> {
    form_urlencoded::parse(raw)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn fv(form: &Map<String, String>, key: &str) -> Option<String> {
    form.get(key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn err404(msg: &str) -> Response {
    (axum::http::StatusCode::NOT_FOUND, msg.to_string()).into_response()
}

fn err403(msg: &str) -> Response {
    (axum::http::StatusCode::FORBIDDEN, msg.to_string()).into_response()
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (y, m) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    (NaiveDate::from_ymd_opt(y, m, 1).unwrap() - NaiveDate::from_ymd_opt(year, month, 1).unwrap()).num_days() as u32
}

fn month_name(month: u32) -> &'static str {
    ["January", "February", "March", "April", "May", "June", "July",
     "August", "September", "October", "November", "December"][(month as usize - 1).min(11)]
}

async fn active_users(db: &syarc_plugin_api::Db) -> Vec<(i64, String)> {
    let rows = sqlx::query(
        "SELECT id, username FROM users WHERE is_active = 1 AND username NOT LIKE 'mesh-%' ORDER BY username",
    )
    .fetch_all(db)
    .await
    .unwrap_or_default();
    rows.iter().map(|r| (r.get::<i64, _>("id"), r.get::<String, _>("username"))).collect()
}

/// Location used for forecasts: settings table override, else Seattle default.
async fn forecast_location(db: &syarc_plugin_api::Db) -> (f64, f64) {
    let lat = sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key = 'wx_latitude'")
        .fetch_optional(db).await.ok().flatten()
        .and_then(|v| v.parse().ok());
    let lon = sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key = 'wx_longitude'")
        .fetch_optional(db).await.ok().flatten()
        .and_then(|v| v.parse().ok());
    (lat.unwrap_or(47.6062), lon.unwrap_or(-122.3321))
}

/// Port of fastmain compute_next_due_date: the next occurrence strictly after
/// `anchor` for a recurring chore.
pub fn compute_next_due_date(
    frequency: &str,
    day_of_week: Option<i64>,
    day_of_month: Option<i64>,
    anchor: NaiveDate,
) -> NaiveDate {
    match frequency {
        "weekly" => {
            let target = day_of_week.unwrap_or(anchor.weekday().num_days_from_monday() as i64) as u32;
            let today_wd = anchor.weekday().num_days_from_monday();
            let mut days_ahead = (target + 7 - today_wd) % 7;
            if days_ahead == 0 {
                days_ahead = 7;
            }
            anchor + Duration::days(days_ahead as i64)
        }
        "biweekly" => anchor + Duration::days(14),
        "semimonthly" => {
            if anchor.day() < 15 {
                NaiveDate::from_ymd_opt(anchor.year(), anchor.month(), 15).unwrap()
            } else {
                let (y, m) = if anchor.month() == 12 { (anchor.year() + 1, 1) } else { (anchor.year(), anchor.month() + 1) };
                NaiveDate::from_ymd_opt(y, m, 1).unwrap()
            }
        }
        "monthly" => {
            let target = day_of_month.unwrap_or(anchor.day() as i64).clamp(1, 31) as u32;
            let (y, m) = if anchor.month() == 12 { (anchor.year() + 1, 1) } else { (anchor.year(), anchor.month() + 1) };
            NaiveDate::from_ymd_opt(y, m, target.min(days_in_month(y, m))).unwrap()
        }
        "quarterly" => {
            let target = day_of_month.unwrap_or(anchor.day() as i64).clamp(1, 28) as u32;
            let mut q_month = anchor.month() as i32 + 3;
            let mut q_year = anchor.year();
            if q_month > 12 {
                q_month -= 12;
                q_year += 1;
            }
            NaiveDate::from_ymd_opt(q_year, q_month as u32, target).unwrap()
        }
        "yearly" => {
            let d = anchor.day().min(28);
            NaiveDate::from_ymd_opt(anchor.year() + 1, anchor.month(), d).unwrap()
        }
        _ => anchor + Duration::days(1), // daily + fallback
    }
}

#[allow(dead_code)]
fn weekday_index(d: Weekday) -> i64 {
    d.num_days_from_monday() as i64
}

// ------------------------------------------------------------- month / day

async fn month_view(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(params): Query<Map<String, String>>,
) -> Response {
    let db = &state.db;
    let today = chrono::Local::now().date_naive();
    let year: i32 = params.get("year").and_then(|v| v.parse().ok()).unwrap_or(today.year());
    let month: u32 = params
        .get("month")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(today.month())
        .clamp(1, 12);

    let num_days = days_in_month(year, month);
    let first = NaiveDate::from_ymd_opt(year, month, 1).unwrap_or(today);
    let first_weekday = first.weekday().num_days_from_monday() as usize;

    let start = format!("{year:04}-{month:02}-01");
    let end = format!("{year:04}-{month:02}-{num_days:02}");

    // Tasks owned or crewed by the user with a due date in the month.
    let task_rows = sqlx::query(
        "SELECT DISTINCT t.id, t.description, t.due_date, t.status, t.urgency, t.owner_id \
         FROM tasks t LEFT JOIN task_crew c ON c.task_id = t.id AND c.user_id = ? \
         WHERE (t.owner_id = ? OR c.user_id = ?) AND t.due_date IS NOT NULL \
           AND t.due_date >= ? AND t.due_date <= ? AND t.status NOT IN ('completed','cancelled') \
         ORDER BY t.due_date",
    )
    .bind(user.id).bind(user.id).bind(user.id).bind(&start).bind(&end)
    .fetch_all(db).await.unwrap_or_default();

    // Chores the user owns that come due in the month (or before it ends).
    let chore_rows = sqlx::query(
        "SELECT id, description, next_due_date, frequency \
         FROM chores WHERE owner_id = ? AND is_active = 1 \
           AND next_due_date <= ? ORDER BY next_due_date",
    )
    .bind(user.id).bind(&end)
    .fetch_all(db).await.unwrap_or_default();

    // Cooking menu entries (public recipes or own), with assigned cook.
    let meal_rows = sqlx::query(
        "SELECT m.id, m.recipe_id, m.menu_date, m.meal_type, r.title, m.cook_user_id, cu.username AS cook \
         FROM cooking_menu_entries m \
         JOIN cooking_recipes r ON r.id = m.recipe_id \
         LEFT JOIN users cu ON cu.id = m.cook_user_id \
         WHERE (r.is_public = 1 OR r.user_id = ?) AND m.menu_date >= ? AND m.menu_date <= ? \
         ORDER BY m.menu_date, m.meal_type",
    )
    .bind(user.id).bind(&start).bind(&end)
    .fetch_all(db).await.unwrap_or_default();

    // Foraging outings owned or shared with the user.
    let forage_rows = sqlx::query(
        "SELECT DISTINCT o.id, o.outing_date, o.outing_type, o.location \
         FROM foraging_outings o \
         LEFT JOIN foraging_shares s ON s.outing_id = o.id AND s.shared_with_user_id = ? \
         WHERE (o.user_id = ? OR s.shared_with_user_id = ?) \
           AND o.outing_date >= ? AND o.outing_date <= ? \
         ORDER BY o.outing_date",
    )
    .bind(user.id).bind(user.id).bind(user.id).bind(&start).bind(&end)
    .fetch_all(db).await.unwrap_or_default();

    // Appointments/meetings owned or invited-to.
    let event_rows = sqlx::query(
        "SELECT DISTINCT e.* FROM calendar_events e \
         LEFT JOIN calendar_event_invitees i ON i.event_id = e.id AND i.user_id = ? \
         WHERE (e.owner_id = ? OR i.user_id = ?) AND e.event_date >= ? AND e.event_date <= ? \
         ORDER BY e.event_date, e.event_time",
    )
    .bind(user.id).bind(user.id).bind(user.id).bind(&start).bind(&end)
    .fetch_all(db).await.unwrap_or_default();

    // Forecast per day (best-effort; degrades silently offline).
    let (lat, lon) = forecast_location(db).await;
    let (periods, _stale, _err) = weather::get_forecast(lat, lon).await;
    let mut wx: Map<String, Value> = Map::new();
    for p in &periods {
        if !p.get("is_day").and_then(|v| v.as_bool()).unwrap_or(false) {
            continue;
        }
        if let Some(day) = p.get("start_time").and_then(|v| v.as_str()).and_then(|s| s.get(0..10)) {
            wx.entry(day.to_string()).or_insert_with(|| p.clone());
        }
    }

    let mut cells: Map<String, Vec<Value>> = Map::new();
    for r in &task_rows {
        if let Some(d) = r.try_get::<Option<String>, _>("due_date").ok().flatten() {
            let short: String = r.get::<String, _>("description").chars().take(30).collect();
            cells.entry(d).or_default().push(json!({
                "kind": "task", "id": r.get::<String, _>("id"), "label": short,
                "status": r.get::<String, _>("status"), "urgency": r.get::<i64, _>("urgency"),
            }));
        }
    }
    for r in &chore_rows {
        // A recurring chore can appear on every matching due date this month.
        let next: NaiveDate = match NaiveDate::parse_from_str(&r.get::<String, _>("next_due_date"), "%Y-%m-%d") {
            Ok(d) => d,
            Err(_) => continue,
        };
        let end_d = NaiveDate::parse_from_str(&end, "%Y-%m-%d").unwrap_or(today);
        let freq: String = r.get("frequency");
        let dow = r.try_get::<Option<i64>, _>("day_of_week").ok().flatten();
        let dom = r.try_get::<Option<i64>, _>("day_of_month").ok().flatten();
        let mut d = next;
        let mut guard = 0;
        while d <= end_d && guard < 62 {
            if d >= first {
                let desc: String = r.get::<String, _>("description");
                let short: String = desc.chars().take(28).collect();
                cells.entry(d.format("%Y-%m-%d").to_string()).or_default().push(json!({
                    "kind": "chore", "id": r.get::<String, _>("id"), "label": short,
                }));
            }
            d = compute_next_due_date(&freq, dow, dom, d);
            guard += 1;
        }
    }
    for r in &meal_rows {
        let d: String = r.get("menu_date");
        let title: String = r.get("title");
        let short: String = title.chars().take(26).collect();
        cells.entry(d).or_default().push(json!({
            "kind": "meal", "id": r.get::<String, _>("id"),
            "recipe_id": r.get::<String, _>("recipe_id"),
            "meal_type": r.get::<String, _>("meal_type"),
            "label": short,
            "cook": r.try_get::<Option<String>, _>("cook").ok().flatten(),
        }));
    }
    for r in &forage_rows {
        let d: String = r.get("outing_date");
        let kind: String = r.get("outing_type");
        let loc: Option<String> = r.try_get("location").ok().flatten();
        cells.entry(d).or_default().push(json!({
            "kind": "foraging", "id": r.get::<String, _>("id"), "label": kind, "location": loc,
        }));
    }
    for r in &event_rows {
        let d: String = r.get("event_date");
        let title: String = r.get("title");
        let short: String = title.chars().take(28).collect();
        cells.entry(d).or_default().push(json!({
            "kind": r.get::<String, _>("category"), "id": r.get::<String, _>("id"),
            "label": short, "time": r.try_get::<Option<String>, _>("event_time").ok().flatten(),
        }));
    }

    let today_s = today.format("%Y-%m-%d").to_string();
    let mut weeks: Vec<Vec<Option<Value>>> = Vec::new();
    let mut week: Vec<Option<Value>> = vec![None; first_weekday];
    for day in 1..=num_days {
        let ds = format!("{year:04}-{month:02}-{day:02}");
        week.push(Some(json!({
            "day": day,
            "date": ds,
            "is_today": ds == today_s,
            "entries": cells.get(&ds).cloned().unwrap_or_default(),
            "wx": wx.get(&ds),
        })));
        if week.len() == 7 {
            weeks.push(std::mem::take(&mut week));
        }
    }
    if !week.is_empty() {
        while week.len() < 7 {
            week.push(None);
        }
        weeks.push(week);
    }

    let (prev_year, prev_month) = if month == 1 { (year - 1, 12) } else { (year, month - 1) };
    let (next_year, next_month) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };

    // Pending invite count for the nav badge.
    let crew_pending = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM task_crew WHERE user_id = ? AND status = 'pending'",
    ).bind(user.id).fetch_one(db).await.unwrap_or(0);
    let conf_pending = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM conferencing_invitations WHERE invitee_id = ? AND status = 'pending'",
    ).bind(user.id).fetch_one(db).await.unwrap_or(0);
    let ev_pending = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM calendar_event_invitees WHERE user_id = ? AND status = 'pending'",
    ).bind(user.id).fetch_one(db).await.unwrap_or(0);

    let out = json!({
        "year": year, "month": month, "month_name": month_name(month),
        "prev_year": prev_year, "prev_month": prev_month, "prev_month_name": month_name(prev_month),
        "next_year": next_year, "next_month": next_month, "next_month_name": month_name(next_month),
        "weeks": weeks,
        "invite_count": crew_pending + conf_pending + ev_pending,
    });
    page(&state, "calendar_month.html", ctx_with_user(out, &user))
}

async fn day_view(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(date): Path<String>,
) -> Response {
    let db = &state.db;
    let Ok(d) = NaiveDate::parse_from_str(&date, "%Y-%m-%d") else {
        return err404("bad date");
    };
    let ds = d.format("%Y-%m-%d").to_string();

    let tasks = sqlx::query(
        "SELECT DISTINCT t.id, t.description, t.status, t.urgency, t.due_date, u.username AS owner \
         FROM tasks t JOIN users u ON u.id = t.owner_id \
         LEFT JOIN task_crew c ON c.task_id = t.id AND c.user_id = ? \
         WHERE (t.owner_id = ? OR c.user_id = ?) AND t.due_date = ? AND t.status NOT IN ('completed','cancelled') \
         ORDER BY t.urgency DESC",
    ).bind(user.id).bind(user.id).bind(user.id).bind(&ds)
    .fetch_all(db).await.unwrap_or_default();

    let chores = sqlx::query(
        "SELECT id, description, next_due_date, frequency FROM chores \
         WHERE owner_id = ? AND is_active = 1 AND next_due_date <= ? ORDER BY next_due_date",
    ).bind(user.id).bind(&ds).fetch_all(db).await.unwrap_or_default();

    let meals = sqlx::query(
        "SELECT m.id, m.recipe_id, m.meal_type, r.title, cu.username AS cook \
         FROM cooking_menu_entries m JOIN cooking_recipes r ON r.id = m.recipe_id \
         LEFT JOIN users cu ON cu.id = m.cook_user_id \
         WHERE (r.is_public = 1 OR r.user_id = ?) AND m.menu_date = ? ORDER BY m.meal_type",
    ).bind(user.id).bind(&ds).fetch_all(db).await.unwrap_or_default();

    let foraging = sqlx::query(
        "SELECT DISTINCT o.id, o.outing_type, o.location, o.status \
         FROM foraging_outings o \
         LEFT JOIN foraging_shares s ON s.outing_id = o.id AND s.shared_with_user_id = ? \
         WHERE (o.user_id = ? OR s.shared_with_user_id = ?) AND o.outing_date = ?",
    ).bind(user.id).bind(user.id).bind(user.id).bind(&ds)
    .fetch_all(db).await.unwrap_or_default();

    let (lat, lon) = forecast_location(db).await;
    let (periods, _stale, _err) = weather::get_forecast(lat, lon).await;
    let day_periods: Vec<Value> = periods
        .iter()
        .filter(|p| p.get("start_time").and_then(|v| v.as_str()).map(|s| s.starts_with(&ds)).unwrap_or(false))
        .cloned()
        .collect();

    let prev = (d - Duration::days(1)).format("%Y-%m-%d").to_string();
    let next = (d + Duration::days(1)).format("%Y-%m-%d").to_string();

    let task_json: Vec<Value> = tasks.iter().map(|r| json!({
        "id": r.get::<String, _>("id"), "description": r.get::<String, _>("description"),
        "status": r.get::<String, _>("status"), "urgency": r.get::<i64, _>("urgency"),
        "owner": r.get::<String, _>("owner"),
    })).collect();
    let chore_json: Vec<Value> = chores.iter().map(|r| json!({
        "id": r.get::<String, _>("id"), "description": r.get::<String, _>("description"),
        "next_due_date": r.get::<String, _>("next_due_date"), "frequency": r.get::<String, _>("frequency"),
    })).collect();
    let meal_json: Vec<Value> = meals.iter().map(|r| json!({
        "id": r.get::<String, _>("id"), "recipe_id": r.get::<String, _>("recipe_id"),
        "meal_type": r.get::<String, _>("meal_type"), "title": r.get::<String, _>("title"),
        "cook": r.try_get::<Option<String>, _>("cook").ok().flatten(),
    })).collect();
    let forage_json: Vec<Value> = foraging.iter().map(|r| json!({
        "id": r.get::<String, _>("id"), "outing_type": r.get::<String, _>("outing_type"),
        "location": r.try_get::<Option<String>, _>("location").ok().flatten(),
        "status": r.get::<String, _>("status"),
    })).collect();

    let events = sqlx::query(
        "SELECT DISTINCT e.*, u.username AS owner FROM calendar_events e \
         JOIN users u ON u.id = e.owner_id \
         LEFT JOIN calendar_event_invitees i ON i.event_id = e.id AND i.user_id = ? \
         WHERE (e.owner_id = ? OR i.user_id = ?) AND e.event_date = ? ORDER BY e.event_time",
    ).bind(user.id).bind(user.id).bind(user.id).bind(&ds)
    .fetch_all(db).await.unwrap_or_default();
    let event_json: Vec<Value> = events.iter().map(|r| json!({
        "id": r.get::<String, _>("id"), "title": r.get::<String, _>("title"),
        "category": r.get::<String, _>("category"),
        "event_time": r.try_get::<Option<String>, _>("event_time").ok().flatten(),
        "duration_mins": r.try_get::<Option<i64>, _>("duration_mins").ok().flatten(),
        "location": r.try_get::<Option<String>, _>("location").ok().flatten(),
        "owner": r.get::<String, _>("owner"),
    })).collect();

    let out = json!({
        "date": ds, "pretty": d.format("%A, %B %-d, %Y").to_string(),
        "prev": prev, "next": next, "is_today": ds == today_str(),
        "tasks": task_json, "chores": chore_json, "meals": meal_json,
        "foraging": forage_json, "periods": day_periods, "events": event_json,
    });
    page(&state, "calendar_day.html", ctx_with_user(out, &user))
}

// ------------------------------------------------------------------- tasks

async fn task_row(db: &syarc_plugin_api::Db, task_id: &str) -> Option<sqlx::sqlite::SqliteRow> {
    sqlx::query("SELECT * FROM tasks WHERE id = ?").bind(task_id).fetch_optional(db).await.ok().flatten()
}

fn can_view_task(row: &sqlx::sqlite::SqliteRow, user: &User, crew: &[Value]) -> bool {
    row.get::<i64, _>("owner_id") == user.id || crew.iter().any(|c| c.get("user_id") == Some(&json!(user.id)))
}

async fn task_crew_json(db: &syarc_plugin_api::Db, task_id: &str) -> Vec<Value> {
    let rows = sqlx::query(
        "SELECT c.id, c.user_id, c.status, u.username FROM task_crew c \
         JOIN users u ON u.id = c.user_id WHERE c.task_id = ? ORDER BY c.assigned_at",
    ).bind(task_id).fetch_all(db).await.unwrap_or_default();
    rows.iter().map(|r| json!({
        "id": r.get::<String, _>("id"), "user_id": r.get::<i64, _>("user_id"),
        "status": r.get::<String, _>("status"), "username": r.get::<String, _>("username"),
    })).collect()
}

fn task_json(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<String, _>("id"),
        "description": r.get::<String, _>("description"),
        "urgency": r.get::<i64, _>("urgency"),
        "status": r.get::<String, _>("status"),
        "due_date": r.try_get::<Option<String>, _>("due_date").ok().flatten(),
        "estimated_hours": r.try_get::<Option<f64>, _>("estimated_hours").ok().flatten(),
        "estimated_cost": r.try_get::<Option<f64>, _>("estimated_cost").ok().flatten(),
        "supplies_needed": r.try_get::<Option<String>, _>("supplies_needed").ok().flatten(),
        "location": r.try_get::<Option<String>, _>("location").ok().flatten(),
        "notes": r.try_get::<Option<String>, _>("notes").ok().flatten(),
        "completed_at": r.try_get::<Option<String>, _>("completed_at").ok().flatten(),
        "owner_id": r.get::<i64, _>("owner_id"),
        "created_at": r.get::<String, _>("created_at"),
    })
}

async fn tasks_list(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(params): Query<Map<String, String>>,
) -> Response {
    let db = &state.db;
    let status_filter = params.get("status").filter(|s| TASK_STATUSES.contains(&s.as_str()));
    let rows = sqlx::query(
        "SELECT DISTINCT t.* FROM tasks t \
         LEFT JOIN task_crew c ON c.task_id = t.id AND c.user_id = ? \
         WHERE (t.owner_id = ? OR c.user_id = ?) ORDER BY \
           CASE t.status WHEN 'in_progress' THEN 0 WHEN 'pending' THEN 1 WHEN 'draft' THEN 2 ELSE 3 END, \
           t.due_date IS NULL, t.due_date, t.urgency DESC",
    ).bind(user.id).bind(user.id).bind(user.id)
    .fetch_all(db).await.unwrap_or_default();

    let mut list = Vec::new();
    for r in &rows {
        let mut v = task_json(r);
        let crew = task_crew_json(db, r.get::<String, _>("id").as_str()).await;
        v["crew"] = json!(crew);
        v["is_owner"] = json!(r.get::<i64, _>("owner_id") == user.id);
        list.push(v);
    }
    if let Some(s) = status_filter {
        list.retain(|t| t.get("status").and_then(|v| v.as_str()) == Some(s.as_str()));
    }

    let out = json!({
        "tasks": list,
        "statuses": TASK_STATUSES, "active_status": status_filter,
    });
    page(&state, "calendar_tasks.html", ctx_with_user(out, &user))
}

async fn task_form_ctx(
    state: &SharedState,
    user: &User,
    existing: Option<Value>,
    error: Option<&str>,
) -> Response {
    let task_id = existing.as_ref().and_then(|t| t.get("id")).and_then(|v| v.as_str()).map(String::from);
    let crew = match &task_id {
        Some(id) => task_crew_json(&state.db, id).await,
        None => vec![],
    };
    let users = active_users(&state.db).await;
    let user_opts: Vec<Value> = users.iter()
        .filter(|(id, _)| Some(*id) != Some(user.id))
        .map(|(id, name)| json!({"id": id, "username": name}))
        .collect();
    let out = json!({
        "task": existing, "crew": crew, "users": user_opts,
        "urgencies": URGENCY_LABELS, "error": error,
    });
    page(state, "calendar_task_form.html", ctx_with_user(out, user))
}

async fn task_new_page(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    task_form_ctx(&state, &user, None, None).await
}

async fn task_edit_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(task_id): Path<String>,
) -> Response {
    let Some(row) = task_row(&state.db, &task_id).await else {
        return err404("task not found");
    };
    let crew = task_crew_json(&state.db, &task_id).await;
    if !can_view_task(&row, &user, &crew) {
        return err403("not your task");
    }
    task_form_ctx(&state, &user, Some(task_json(&row)), None).await
}

async fn task_create(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    body: Bytes,
) -> Response {
    task_save(State(state), RequireUser(user), Path(String::new()), body).await
}

async fn task_save(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(task_id): Path<String>,
    body: Bytes,
) -> Response {
    let db = &state.db;
    let form = form_map(&body);
    let Some(description) = fv(&form, "description") else {
        return task_form_ctx(&state, &user, None, Some("Description is required.")).await;
    };
    let urgency = form.get("urgency").and_then(|v| v.parse::<i64>().ok()).unwrap_or(3).clamp(1, 5);
    let status = form.get("status").filter(|s| TASK_STATUSES.contains(&s.as_str())).cloned().unwrap_or_else(|| "draft".into());
    let due_date = fv(&form, "due_date").filter(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").is_ok());
    let now = now_str();

    let existing = if task_id.is_empty() { None } else { task_row(db, &task_id).await };
    if let Some(row) = &existing {
        let crew = task_crew_json(db, &row.get::<String, _>("id")).await;
        if !can_view_task(row, &user, &crew) {
            return err403("not your task");
        }
    }

    let completed_at = if status == "completed" {
        existing.as_ref()
            .and_then(|r| r.try_get::<Option<String>, _>("completed_at").ok().flatten())
            .or_else(|| Some(now.clone()))
    } else {
        None
    };

    let id = match &existing {
        Some(row) => {
            let id = row.get::<String, _>("id");
            let _ = sqlx::query(
                "UPDATE tasks SET description = ?, urgency = ?, status = ?, estimated_hours = ?, \
                 supplies_needed = ?, estimated_cost = ?, due_date = ?, completed_at = ?, \
                 location = ?, notes = ?, updated_at = ? WHERE id = ?",
            )
            .bind(&description).bind(urgency).bind(&status)
            .bind(form.get("estimated_hours").and_then(|v| v.parse::<f64>().ok()))
            .bind(fv(&form, "supplies_needed"))
            .bind(form.get("estimated_cost").and_then(|v| v.parse::<f64>().ok()))
            .bind(&due_date).bind(&completed_at)
            .bind(fv(&form, "location")).bind(fv(&form, "notes"))
            .bind(&now).bind(&id)
            .execute(db).await;
            id
        }
        None => {
            let id = new_id();
            let _ = sqlx::query(
                "INSERT INTO tasks (id, owner_id, description, urgency, status, estimated_hours, \
                 supplies_needed, estimated_cost, due_date, completed_at, location, notes, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&id).bind(user.id).bind(&description).bind(urgency).bind(&status)
            .bind(form.get("estimated_hours").and_then(|v| v.parse::<f64>().ok()))
            .bind(fv(&form, "supplies_needed"))
            .bind(form.get("estimated_cost").and_then(|v| v.parse::<f64>().ok()))
            .bind(&due_date).bind(&completed_at)
            .bind(fv(&form, "location")).bind(fv(&form, "notes"))
            .bind(&now).bind(&now)
            .execute(db).await;
            id
        }
    };
    Redirect::to(&format!("/calendar/tasks/{id}/edit")).into_response()
}

async fn task_set_status(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(task_id): Path<String>,
    body: Bytes,
) -> Response {
    let db = &state.db;
    let form = form_map(&body);
    let Some(status) = form.get("status").filter(|s| TASK_STATUSES.contains(&s.as_str())) else {
        return err404("bad status");
    };
    let Some(row) = task_row(db, &task_id).await else {
        return err404("task not found");
    };
    let crew = task_crew_json(db, &task_id).await;
    if !can_view_task(&row, &user, &crew) {
        return err403("not your task");
    }
    let completed_at = if status == "completed" { Some(now_str()) } else { None };
    let _ = sqlx::query("UPDATE tasks SET status = ?, completed_at = ?, updated_at = ? WHERE id = ?")
        .bind(status).bind(&completed_at).bind(now_str()).bind(&task_id)
        .execute(db).await;
    Redirect::to(&redirect_back(&form)).into_response()
}

fn redirect_back(form: &Map<String, String>) -> String {
    form.get("back").filter(|v| v.starts_with('/')).cloned().unwrap_or_else(|| "/calendar/tasks".into())
}

async fn task_delete(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(task_id): Path<String>,
    body: Bytes,
) -> Response {
    let db = &state.db;
    let form = form_map(&body);
    let res = sqlx::query("DELETE FROM tasks WHERE id = ? AND owner_id = ?")
        .bind(&task_id).bind(user.id).execute(db).await;
    match res {
        Ok(r) if r.rows_affected() > 0 => Redirect::to(&redirect_back(&form)).into_response(),
        _ => err403("only the owner can delete a task"),
    }
}

async fn task_crew_add(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(task_id): Path<String>,
    body: Bytes,
) -> Response {
    let db = &state.db;
    let form = form_map(&body);
    let Some(row) = task_row(db, &task_id).await else {
        return err404("task not found");
    };
    if row.get::<i64, _>("owner_id") != user.id {
        return err403("only the owner can invite crew");
    }
    let Ok(uid) = form.get("user_id").and_then(|v| v.parse::<i64>().ok()).ok_or(()) else {
        return err404("pick a user");
    };
    if uid == user.id {
        return Redirect::to(&format!("/calendar/tasks/{task_id}/edit")).into_response();
    }
    let _ = sqlx::query(
        "INSERT INTO task_crew (id, task_id, user_id, status, assigned_at) \
         SELECT ?, ?, ?, 'pending', ? \
         WHERE NOT EXISTS (SELECT 1 FROM task_crew WHERE task_id = ? AND user_id = ?)",
    )
    .bind(new_id()).bind(&task_id).bind(uid).bind(now_str()).bind(&task_id).bind(uid)
    .execute(db).await;
    Redirect::to(&format!("/calendar/tasks/{task_id}/edit")).into_response()
}

async fn task_crew_remove(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((task_id, crew_id)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    let owner = task_row(db, &task_id).await.map(|r| r.get::<i64, _>("owner_id"));
    if owner != Some(user.id) {
        return err403("only the owner can remove crew");
    }
    let _ = sqlx::query("DELETE FROM task_crew WHERE id = ? AND task_id = ?")
        .bind(&crew_id).bind(&task_id).execute(db).await;
    Redirect::to(&format!("/calendar/tasks/{task_id}/edit")).into_response()
}

async fn task_crew_respond(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((task_id, crew_id)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let db = &state.db;
    let form = form_map(&body);
    let action = form.get("action").map(String::as_str);
    let status = match action {
        Some("confirm") => "confirmed",
        Some("decline") => "declined",
        _ => return err404("bad action"),
    };
    let _ = sqlx::query(
        "UPDATE task_crew SET status = ?, responded_at = ? WHERE id = ? AND task_id = ? AND user_id = ?",
    ).bind(status).bind(now_str()).bind(&crew_id).bind(&task_id).bind(user.id)
    .execute(db).await;
    Redirect::to(&redirect_back(&form)).into_response()
}

// ------------------------------------------------------------------ chores

fn chore_json(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<String, _>("id"),
        "description": r.get::<String, _>("description"),
        "frequency": r.get::<String, _>("frequency"),
        "day_of_week": r.try_get::<Option<i64>, _>("day_of_week").ok().flatten(),
        "day_of_month": r.try_get::<Option<i64>, _>("day_of_month").ok().flatten(),
        "time_estimate": r.try_get::<Option<f64>, _>("time_estimate").ok().flatten(),
        "location": r.try_get::<Option<String>, _>("location").ok().flatten(),
        "supplies_needed": r.try_get::<Option<String>, _>("supplies_needed").ok().flatten(),
        "notes": r.try_get::<Option<String>, _>("notes").ok().flatten(),
        "is_active": r.get::<i64, _>("is_active") != 0,
        "last_completed_at": r.try_get::<Option<String>, _>("last_completed_at").ok().flatten(),
        "next_due_date": r.get::<String, _>("next_due_date"),
    })
}

async fn chores_list(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    let rows = sqlx::query(
        "SELECT * FROM chores WHERE owner_id = ? ORDER BY is_active DESC, next_due_date",
    ).bind(user.id).fetch_all(&state.db).await.unwrap_or_default();
    let list: Vec<Value> = rows.iter().map(chore_json).collect();
    let out = json!({
        "chores": list,
        "frequencies": FREQUENCIES,
    });
    page(&state, "calendar_chores.html", ctx_with_user(out, &user))
}

async fn chore_form_ctx(state: &SharedState, user: &User, existing: Option<Value>, error: Option<&str>) -> Response {
    let out = json!({
        "chore": existing, "frequencies": FREQUENCIES, "error": error,
    });
    page(state, "calendar_chore_form.html", ctx_with_user(out, user))
}

async fn chore_new_page(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    chore_form_ctx(&state, &user, None, None).await
}

async fn chore_edit_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(chore_id): Path<String>,
) -> Response {
    let row = sqlx::query("SELECT * FROM chores WHERE id = ? AND owner_id = ?")
        .bind(&chore_id).bind(user.id).fetch_optional(&state.db).await.ok().flatten();
    match row {
        Some(r) => chore_form_ctx(&state, &user, Some(chore_json(&r)), None).await,
        None => err404("chore not found"),
    }
}

async fn chore_create(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    body: Bytes,
) -> Response {
    chore_save(State(state), RequireUser(user), Path(String::new()), body).await
}

async fn chore_save(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(chore_id): Path<String>,
    body: Bytes,
) -> Response {
    let db = &state.db;
    let form = form_map(&body);
    let Some(description) = fv(&form, "description") else {
        return chore_form_ctx(&state, &user, None, Some("Description is required.")).await;
    };
    let Some(frequency) = form.get("frequency").filter(|f| FREQUENCIES.iter().any(|(k, _)| k == f)).cloned() else {
        return chore_form_ctx(&state, &user, None, Some("Pick a frequency.")).await;
    };
    let day_of_week = form.get("day_of_week").and_then(|v| v.parse::<i64>().ok()).filter(|v| (0..7).contains(v));
    let day_of_month = form.get("day_of_month").and_then(|v| v.parse::<i64>().ok()).filter(|v| (1..=31).contains(v));
    let now = now_str();

    let existing = if chore_id.is_empty() {
        None
    } else {
        sqlx::query("SELECT * FROM chores WHERE id = ? AND owner_id = ?")
            .bind(&chore_id).bind(user.id).fetch_optional(db).await.ok().flatten()
    };

    let _id = match &existing {
        Some(row) => {
            let id = row.get::<String, _>("id");
            let next = fv(&form, "next_due_date")
                .and_then(|d| NaiveDate::parse_from_str(&d, "%Y-%m-%d").ok())
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| row.get::<String, _>("next_due_date"));
            let _ = sqlx::query(
                "UPDATE chores SET description = ?, frequency = ?, day_of_week = ?, day_of_month = ?, \
                 time_estimate = ?, location = ?, supplies_needed = ?, notes = ?, next_due_date = ?, updated_at = ? \
                 WHERE id = ?",
            )
            .bind(&description).bind(&frequency).bind(day_of_week).bind(day_of_month)
            .bind(form.get("time_estimate").and_then(|v| v.parse::<f64>().ok()))
            .bind(fv(&form, "location")).bind(fv(&form, "supplies_needed")).bind(fv(&form, "notes"))
            .bind(&next).bind(&now).bind(&id)
            .execute(db).await;
            id
        }
        None => {
            let id = new_id();
            let next = fv(&form, "next_due_date")
                .and_then(|d| NaiveDate::parse_from_str(&d, "%Y-%m-%d").ok())
                .unwrap_or_else(|| chrono::Local::now().date_naive());
            let next = compute_next_due_date(&frequency, day_of_week, day_of_month, next)
                .format("%Y-%m-%d").to_string();
            let _ = sqlx::query(
                "INSERT INTO chores (id, owner_id, description, frequency, day_of_week, day_of_month, \
                 time_estimate, location, supplies_needed, notes, is_active, next_due_date, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?)",
            )
            .bind(&id).bind(user.id).bind(&description).bind(&frequency)
            .bind(day_of_week).bind(day_of_month)
            .bind(form.get("time_estimate").and_then(|v| v.parse::<f64>().ok()))
            .bind(fv(&form, "location")).bind(fv(&form, "supplies_needed")).bind(fv(&form, "notes"))
            .bind(&next).bind(&now).bind(&now)
            .execute(db).await;
            id
        }
    };
    Redirect::to("/calendar/chores").into_response()
}

async fn chore_complete(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(chore_id): Path<String>,
    body: Bytes,
) -> Response {
    let db = &state.db;
    let form = form_map(&body);
    let row = sqlx::query("SELECT * FROM chores WHERE id = ? AND owner_id = ?")
        .bind(&chore_id).bind(user.id).fetch_optional(db).await.ok().flatten();
    let Some(row) = row else { return err404("chore not found") };
    let now = now_str();
    let today = chrono::Local::now().date_naive();
    let next = compute_next_due_date(
        &row.get::<String, _>("frequency"),
        row.try_get::<Option<i64>, _>("day_of_week").ok().flatten(),
        row.try_get::<Option<i64>, _>("day_of_month").ok().flatten(),
        today,
    ).format("%Y-%m-%d").to_string();
    let _ = sqlx::query(
        "INSERT INTO chore_completions (id, chore_id, completed_by, completed_at, notes) VALUES (?, ?, ?, ?, ?)",
    ).bind(new_id()).bind(&chore_id).bind(user.id).bind(&now).bind(fv(&form, "notes"))
    .execute(db).await;
    let _ = sqlx::query(
        "UPDATE chores SET last_completed_at = ?, next_due_date = ?, updated_at = ? WHERE id = ?",
    ).bind(&now).bind(&next).bind(&now).bind(&chore_id).execute(db).await;
    Redirect::to(&redirect_back(&form)).into_response()
}

async fn chore_toggle(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(chore_id): Path<String>,
    body: Bytes,
) -> Response {
    let form = form_map(&body);
    let _ = sqlx::query("UPDATE chores SET is_active = 1 - is_active, updated_at = ? WHERE id = ? AND owner_id = ?")
        .bind(now_str()).bind(&chore_id).bind(user.id)
        .execute(&state.db).await;
    Redirect::to(&redirect_back(&form)).into_response()
}

async fn chore_delete(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(chore_id): Path<String>,
    body: Bytes,
) -> Response {
    let form = form_map(&body);
    let _ = sqlx::query("DELETE FROM chores WHERE id = ? AND owner_id = ?")
        .bind(&chore_id).bind(user.id).execute(&state.db).await;
    Redirect::to(&redirect_back(&form)).into_response()
}

// ----------------------------------------------------------------- invites

async fn invites_panel(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    let db = &state.db;

    let crew_rows = sqlx::query(
        "SELECT c.id, c.task_id, c.status, c.assigned_at, t.description, t.due_date, \
                t.urgency, t.location, u.username AS owner \
         FROM task_crew c JOIN tasks t ON t.id = c.task_id JOIN users u ON u.id = t.owner_id \
         WHERE c.user_id = ? AND t.status NOT IN ('completed','cancelled') ORDER BY c.assigned_at DESC",
    ).bind(user.id).fetch_all(db).await.unwrap_or_default();
    let crew: Vec<Value> = crew_rows.iter().map(|r| json!({
        "id": r.get::<String, _>("id"), "task_id": r.get::<String, _>("task_id"),
        "status": r.get::<String, _>("status"),
        "assigned_at": r.get::<String, _>("assigned_at"),
        "description": r.get::<String, _>("description"),
        "due_date": r.try_get::<Option<String>, _>("due_date").ok().flatten(),
        "urgency": r.get::<i64, _>("urgency"),
        "location": r.try_get::<Option<String>, _>("location").ok().flatten(),
        "owner": r.get::<String, _>("owner"),
    })).collect();

    // Conferencing invitations for rooms that are still open.
    let conf_rows = sqlx::query(
        "SELECT i.id, i.call_id, i.status, i.created_at, i.expires_at, u.username AS inviter, \
                r.media_type, r.status AS call_status \
         FROM conferencing_invitations i \
         JOIN conferencing_calls r ON r.id = i.call_id \
         JOIN users u ON u.id = r.caller_id \
         WHERE i.invitee_id = ? ORDER BY i.created_at DESC LIMIT 50",
    ).bind(user.id).fetch_all(db).await.unwrap_or_default();
    let conf: Vec<Value> = conf_rows.iter().map(|r| json!({
        "id": r.get::<String, _>("id"), "call_id": r.get::<String, _>("call_id"),
        "status": r.get::<String, _>("status"),
        "created_at": r.get::<String, _>("created_at"),
        "expires_at": r.get::<String, _>("expires_at"),
        "inviter": r.get::<String, _>("inviter"),
        "media_type": r.get::<String, _>("media_type"),
        "call_status": r.get::<String, _>("call_status"),
    })).collect();

    // Foraging outings shared with the user (share = soft invite).
    let forage_rows = sqlx::query(
        "SELECT s.outing_id, o.outing_date, o.outing_type, o.location, o.status, u.username AS owner \
         FROM foraging_shares s JOIN foraging_outings o ON o.id = s.outing_id \
         JOIN users u ON u.id = o.user_id \
         WHERE s.shared_with_user_id = ? AND o.outing_date >= date('now') ORDER BY o.outing_date",
    ).bind(user.id).fetch_all(db).await.unwrap_or_default();
    let forage: Vec<Value> = forage_rows.iter().map(|r| json!({
        "outing_id": r.get::<String, _>("outing_id"),
        "outing_date": r.get::<String, _>("outing_date"),
        "outing_type": r.get::<String, _>("outing_type"),
        "location": r.try_get::<Option<String>, _>("location").ok().flatten(),
        "status": r.get::<String, _>("status"),
        "owner": r.get::<String, _>("owner"),
    })).collect();

    // Appointment/meeting invitations for the user.
    let event_rows = sqlx::query(
        "SELECT i.id, i.status, i.invited_at, e.id AS event_id, e.title, e.category, \
                e.event_date, e.event_time, e.duration_mins, e.location, u.username AS owner \
         FROM calendar_event_invitees i \
         JOIN calendar_events e ON e.id = i.event_id \
         JOIN users u ON u.id = e.owner_id \
         WHERE i.user_id = ? ORDER BY e.event_date, e.event_time LIMIT 50",
    ).bind(user.id).fetch_all(db).await.unwrap_or_default();
    let event_inv: Vec<Value> = event_rows.iter().map(|r| json!({
        "id": r.get::<String, _>("id"), "event_id": r.get::<String, _>("event_id"),
        "status": r.get::<String, _>("status"), "invited_at": r.get::<String, _>("invited_at"),
        "title": r.get::<String, _>("title"), "category": r.get::<String, _>("category"),
        "event_date": r.get::<String, _>("event_date"),
        "event_time": r.try_get::<Option<String>, _>("event_time").ok().flatten(),
        "duration_mins": r.try_get::<Option<i64>, _>("duration_mins").ok().flatten(),
        "location": r.try_get::<Option<String>, _>("location").ok().flatten(),
        "owner": r.get::<String, _>("owner"),
    })).collect();

    // Days where the user (or nobody) is assigned to cook.
    let cook_rows = sqlx::query(
        "SELECT m.menu_date, r.title, m.meal_type, cu.username AS cook \
         FROM cooking_menu_entries m JOIN cooking_recipes r ON r.id = m.recipe_id \
         LEFT JOIN users cu ON cu.id = m.cook_user_id \
         WHERE (r.is_public = 1 OR r.user_id = ?) \
           AND m.menu_date >= date('now') AND m.menu_date <= date('now', '+7 day') \
           AND (m.cook_user_id IS NULL OR m.cook_user_id = ?) \
         ORDER BY m.menu_date, m.meal_type",
    ).bind(user.id).bind(user.id).fetch_all(db).await.unwrap_or_default();
    let cooking: Vec<Value> = cook_rows.iter().map(|r| json!({
        "menu_date": r.get::<String, _>("menu_date"),
        "title": r.get::<String, _>("title"),
        "meal_type": r.get::<String, _>("meal_type"),
        "cook": r.try_get::<Option<String>, _>("cook").ok().flatten(),
    })).collect();

    let out = json!({
        "crew": crew, "conferencing": conf, "event_invites": event_inv,
        "foraging": forage, "cooking": cooking,
    });
    page(&state, "calendar_invites.html", ctx_with_user(out, &user))
}

async fn invite_task_respond(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((crew_id, action)): Path<(String, String)>,
) -> Response {
    let status = match action.as_str() {
        "accept" => "confirmed",
        "decline" => "declined",
        _ => return err404("bad action"),
    };
    let _ = sqlx::query(
        "UPDATE task_crew SET status = ?, responded_at = ? WHERE id = ? AND user_id = ? AND status = 'pending'",
    ).bind(status).bind(now_str()).bind(&crew_id).bind(user.id)
    .execute(&state.db).await;
    Redirect::to("/calendar/invites").into_response()
}

async fn invite_conferencing_respond(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((invitation_id, action)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    let status = match action.as_str() {
        "accept" => "accepted",
        "decline" => "rejected",
        _ => return err404("bad action"),
    };
    let inv = sqlx::query_scalar::<_, String>(
        "SELECT status FROM conferencing_invitations WHERE id = ? AND invitee_id = ?",
    ).bind(&invitation_id).bind(user.id).fetch_optional(db).await.ok().flatten();
    let Some(cur) = inv else { return err404("invitation not found") };
    if cur != "pending" {
        return Redirect::to("/calendar/invites").into_response();
    }
    let call_id = sqlx::query_scalar::<_, String>(
        "SELECT call_id FROM conferencing_invitations WHERE id = ?",
    ).bind(&invitation_id).fetch_one(db).await.ok();
    let _ = sqlx::query(
        "UPDATE conferencing_invitations SET status = ?, responded_at = ? WHERE id = ?",
    ).bind(status).bind(now_str()).bind(&invitation_id).execute(db).await;
    if status == "accepted" {
        if let Some(call_id) = call_id {
            let _ = sqlx::query(
                "INSERT INTO conferencing_call_participants (call_id, user_id, joined_at) \
                 SELECT ?, ?, ? WHERE NOT EXISTS \
                   (SELECT 1 FROM conferencing_call_participants WHERE call_id = ? AND user_id = ?)",
            ).bind(&call_id).bind(user.id).bind(now_str()).bind(&call_id).bind(user.id)
            .execute(db).await;
            return Redirect::to("/conferencing/").into_response();
        }
    }
    Redirect::to("/calendar/invites").into_response()
}

// ------------------------------------------------------------------ events

fn event_json(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<String, _>("id"),
        "title": r.get::<String, _>("title"),
        "category": r.get::<String, _>("category"),
        "event_date": r.get::<String, _>("event_date"),
        "event_time": r.try_get::<Option<String>, _>("event_time").ok().flatten(),
        "duration_mins": r.try_get::<Option<i64>, _>("duration_mins").ok().flatten(),
        "location": r.try_get::<Option<String>, _>("location").ok().flatten(),
        "notes": r.try_get::<Option<String>, _>("notes").ok().flatten(),
        "owner_id": r.get::<i64, _>("owner_id"),
    })
}

async fn event_row(db: &syarc_plugin_api::Db, event_id: &str) -> Option<sqlx::sqlite::SqliteRow> {
    sqlx::query("SELECT * FROM calendar_events WHERE id = ?").bind(event_id).fetch_optional(db).await.ok().flatten()
}

async fn event_invitees_json(db: &syarc_plugin_api::Db, event_id: &str) -> Vec<Value> {
    let rows = sqlx::query(
        "SELECT i.id, i.user_id, i.status, u.username FROM calendar_event_invitees i \
         JOIN users u ON u.id = i.user_id WHERE i.event_id = ? ORDER BY i.invited_at",
    ).bind(event_id).fetch_all(db).await.unwrap_or_default();
    rows.iter().map(|r| json!({
        "id": r.get::<String, _>("id"), "user_id": r.get::<i64, _>("user_id"),
        "status": r.get::<String, _>("status"), "username": r.get::<String, _>("username"),
    })).collect()
}

async fn events_list(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(params): Query<Map<String, String>>,
) -> Response {
    let db = &state.db;
    let cat = params.get("category").filter(|c| EVENT_CATEGORIES.iter().any(|(k, _)| k == c)).cloned();
    let rows = sqlx::query(
        "SELECT DISTINCT e.* FROM calendar_events e \
         LEFT JOIN calendar_event_invitees i ON i.event_id = e.id AND i.user_id = ? \
         WHERE (e.owner_id = ? OR i.user_id = ?) AND e.event_date >= date('now') \
         ORDER BY e.event_date, e.event_time LIMIT 200",
    ).bind(user.id).bind(user.id).bind(user.id)
    .fetch_all(db).await.unwrap_or_default();
    let mut list = Vec::new();
    for r in &rows {
        if let Some(c) = &cat {
            if r.get::<String, _>("category") != *c {
                continue;
            }
        }
        let mut v = event_json(r);
        v["invitees"] = json!(event_invitees_json(db, r.get::<String, _>("id").as_str()).await);
        v["is_owner"] = json!(r.get::<i64, _>("owner_id") == user.id);
        list.push(v);
    }
    page(&state, "calendar_events.html", json!({
        "events": list, "categories": EVENT_CATEGORIES, "active_category": cat,
    }))
}

async fn event_form_ctx(
    state: &SharedState,
    user: &User,
    existing: Option<Value>,
    error: Option<&str>,
) -> Response {
    let event_id = existing.as_ref().and_then(|e| e.get("id")).and_then(|v| v.as_str()).map(String::from);
    let invitees = match &event_id {
        Some(id) => event_invitees_json(&state.db, id).await,
        None => vec![],
    };
    let users = active_users(&state.db).await;
    let user_opts: Vec<Value> = users.iter()
        .filter(|(id, _)| Some(*id) != Some(user.id))
        .map(|(id, name)| json!({"id": id, "username": name}))
        .collect();
    page(state, "calendar_event_form.html", json!({
        "event": existing, "invitees": invitees, "users": user_opts,
        "categories": EVENT_CATEGORIES, "error": error,
    }))
}

async fn event_new_page(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    event_form_ctx(&state, &user, None, None).await
}

async fn event_create(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    body: Bytes,
) -> Response {
    event_save(State(state), RequireUser(user), Path(String::new()), body).await
}

async fn event_edit_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(event_id): Path<String>,
) -> Response {
    let Some(row) = event_row(&state.db, &event_id).await else {
        return err404("event not found");
    };
    if row.get::<i64, _>("owner_id") != user.id && user.role != "admin" {
        return err403("not your event");
    }
    event_form_ctx(&state, &user, Some(event_json(&row)), None).await
}

async fn event_save(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(event_id): Path<String>,
    body: Bytes,
) -> Response {
    let db = &state.db;
    let form = form_map(&body);
    let Some(title) = fv(&form, "title") else {
        return event_form_ctx(&state, &user, None, Some("Title is required.")).await;
    };
    let Some(category) = form.get("category").filter(|c| EVENT_CATEGORIES.iter().any(|(k, _)| k == c)).cloned() else {
        return event_form_ctx(&state, &user, None, Some("Pick a category.")).await;
    };
    let Some(event_date) = fv(&form, "event_date").filter(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").is_ok()) else {
        return event_form_ctx(&state, &user, None, Some("A valid date is required.")).await;
    };
    let event_time = fv(&form, "event_time").filter(|t| NaiveDate::parse_from_str(&format!("2000-01-01 {t}"), "%Y-%m-%d %H:%M").is_ok());
    let duration_mins = form.get("duration_mins").and_then(|v| v.parse::<i64>().ok()).filter(|v| *v > 0);
    let now = now_str();

    let existing = if event_id.is_empty() { None } else { event_row(db, &event_id).await };
    if let Some(row) = &existing {
        if row.get::<i64, _>("owner_id") != user.id && user.role != "admin" {
            return err403("not your event");
        }
    }

    let id = match &existing {
        Some(row) => {
            let id = row.get::<String, _>("id");
            let _ = sqlx::query(
                "UPDATE calendar_events SET title = ?, category = ?, event_date = ?, event_time = ?, \
                 duration_mins = ?, location = ?, notes = ?, updated_at = ? WHERE id = ?",
            )
            .bind(&title).bind(&category).bind(&event_date).bind(&event_time)
            .bind(duration_mins).bind(fv(&form, "location")).bind(fv(&form, "notes"))
            .bind(&now).bind(&id)
            .execute(db).await;
            id
        }
        None => {
            let id = new_id();
            let _ = sqlx::query(
                "INSERT INTO calendar_events (id, owner_id, category, title, event_date, event_time, \
                 duration_mins, location, notes, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&id).bind(user.id).bind(&category).bind(&title).bind(&event_date)
            .bind(&event_time).bind(duration_mins)
            .bind(fv(&form, "location")).bind(fv(&form, "notes")).bind(&now).bind(&now)
            .execute(db).await;
            id
        }
    };
    Redirect::to(&format!("/calendar/events/{id}/edit")).into_response()
}

async fn event_delete(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(event_id): Path<String>,
    body: Bytes,
) -> Response {
    let form = form_map(&body);
    let res = sqlx::query("DELETE FROM calendar_events WHERE id = ? AND owner_id = ?")
        .bind(&event_id).bind(user.id).execute(&state.db).await;
    match res {
        Ok(r) if r.rows_affected() > 0 => Redirect::to(&redirect_back(&form)).into_response(),
        _ => err403("only the owner can delete an event"),
    }
}

async fn event_invitee_add(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(event_id): Path<String>,
    body: Bytes,
) -> Response {
    let db = &state.db;
    let form = form_map(&body);
    let Some(row) = event_row(db, &event_id).await else {
        return err404("event not found");
    };
    if row.get::<i64, _>("owner_id") != user.id && user.role != "admin" {
        return err403("only the owner can invite people");
    }
    let Some(uid) = form.get("user_id").and_then(|v| v.parse::<i64>().ok()) else {
        return err404("pick a user");
    };
    if uid == user.id {
        return Redirect::to(&format!("/calendar/events/{event_id}/edit")).into_response();
    }
    let _ = sqlx::query(
        "INSERT INTO calendar_event_invitees (id, event_id, user_id, status, invited_at) \
         SELECT ?, ?, ?, 'pending', ? \
         WHERE NOT EXISTS (SELECT 1 FROM calendar_event_invitees WHERE event_id = ? AND user_id = ?)",
    )
    .bind(new_id()).bind(&event_id).bind(uid).bind(now_str()).bind(&event_id).bind(uid)
    .execute(db).await;
    Redirect::to(&format!("/calendar/events/{event_id}/edit")).into_response()
}

async fn event_invitee_remove(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((event_id, invitee_id)): Path<(String, String)>,
) -> Response {
    let db = &state.db;
    let owner = event_row(db, &event_id).await.map(|r| r.get::<i64, _>("owner_id"));
    if owner != Some(user.id) && user.role != "admin" {
        return err403("only the owner can remove invitees");
    }
    let _ = sqlx::query("DELETE FROM calendar_event_invitees WHERE id = ? AND event_id = ?")
        .bind(&invitee_id).bind(&event_id).execute(db).await;
    Redirect::to(&format!("/calendar/events/{event_id}/edit")).into_response()
}

async fn event_invitee_respond(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path((event_id, invitee_id)): Path<(String, String)>,
    body: Bytes,
) -> Response {
    let db = &state.db;
    let form = form_map(&body);
    let status = match form.get("action").map(String::as_str) {
        Some("accept") => "confirmed",
        Some("decline") => "declined",
        _ => return err404("bad action"),
    };
    let _ = sqlx::query(
        "UPDATE calendar_event_invitees SET status = ?, responded_at = ? \
         WHERE id = ? AND event_id = ? AND user_id = ? AND status = 'pending'",
    ).bind(status).bind(now_str()).bind(&invitee_id).bind(&event_id).bind(user.id)
    .execute(db).await;
    Redirect::to(&redirect_back(&form)).into_response()
}

// ----------------------------------------------------------------- weather

async fn weather_page(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    let db = &state.db;
    let (lat, lon) = forecast_location(db).await;
    let (periods, stale, f_err) = weather::get_forecast(lat, lon).await;
    let (current, c_stale, c_err) = weather::get_current(lat, lon).await;
    let (alerts, a_err) = weather::get_alerts(lat, lon).await;
    let error = f_err.or(c_err).or(a_err);
    let out = json!({
        "lat": lat, "lon": lon,
        "periods": periods, "forecast_stale": stale,
        "current": current, "current_stale": c_stale,
        "alerts": alerts, "error": error,
        "is_admin": user.role == "admin",
    });
    page(&state, "calendar_weather.html", ctx_with_user(out, &user))
}

async fn weather_location(
    State(state): State<SharedState>,
    syarc_plugin_api::RequireAdmin(_admin): syarc_plugin_api::RequireAdmin,
    body: Bytes,
) -> Response {
    let form = form_map(&body);
    let (Some(lat), Some(lon)) = (
        form.get("latitude").and_then(|v| v.parse::<f64>().ok()),
        form.get("longitude").and_then(|v| v.parse::<f64>().ok()),
    ) else {
        return err404("bad coordinates");
    };
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lon) {
        return err404("coordinates out of range");
    }
    for (key, value) in [("wx_latitude", format!("{lat}")), ("wx_longitude", format!("{lon}"))] {
        let _ = sqlx::query(
            "INSERT INTO settings (key, value) VALUES (?, ?) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        ).bind(key).bind(&value).execute(&state.db).await;
    }
    Redirect::to("/calendar/weather").into_response()
}
